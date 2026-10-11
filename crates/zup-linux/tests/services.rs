#![cfg(target_os = "linux")]
#![cfg(feature = "test-support")]

use std::collections::BTreeMap;
use std::path::PathBuf;

use zup_bundle::DirectoryPayloadSource;
use zup_core::{AppId, Privilege, ResourceKey, SelectedScope, ServiceStart, TargetTriple};
use zup_exec::{HostSnapshot, InstallLedger, LifecycleAction, OwnedResource};
use zup_linux::test_support::SharedFakeSystemd;
use zup_linux::{
    DesiredService, LinuxFileExecutor, LinuxLedgerStore, MachineRoots, ServiceCompilation,
    ServiceSupport, SystemdManager as _, SystemdRoots, compile_machine_execution_plan,
    snapshot_services, snapshot_target,
};
use zup_platform::{CommandSpec, TargetFile, TargetPath, TargetPlan, TargetPlanSummary};
use zup_transaction::{
    FilesystemTransactionStore, NodeKind, NodeState, TransactionCoordinator, TransactionOutcome,
    TransactionPhase, TransactionStore, compile_transaction,
};

const APP_ID: &str = "com.example.tool";
const SERVICE_ID: &str = "tool";
const PAYLOAD_BYTES: &[u8] = b"fake-elf-bytes";

fn target() -> TargetTriple {
    TargetTriple::parse("x86_64-unknown-linux-gnu").expect("a target")
}

fn app(version: &str) -> zup_core::App {
    zup_core::App {
        id: AppId::new(APP_ID).expect("an id"),
        name: zup_core::NonEmptyString::new("Tool").expect("a name"),
        version: semver::Version::parse(version).expect("a version"),
        publisher: None,
        main: None,
        description: None,
    }
}

struct Fixture {
    _base: tempfile::TempDir,
    roots: MachineRoots,
    systemd: SystemdRoots,
    state: PathBuf,
    payload: PathBuf,
    manager: SharedFakeSystemd,
    uid: u32,
}

impl Fixture {
    fn new() -> Self {
        let base = tempfile::tempdir().expect("an isolated base");
        let roots = MachineRoots::new(
            base.path().join("opt"),
            base.path().join("var/lib/zup"),
            base.path().join("var/opt"),
        );
        let systemd = SystemdRoots::new(base.path().join("units"));
        std::fs::create_dir_all(&systemd.unit_dir).expect("a unit tree");
        let payload = base.path().join("payload");
        std::fs::create_dir_all(&payload).expect("a payload tree");
        std::fs::write(payload.join("tool"), PAYLOAD_BYTES).expect("a service binary");
        Self {
            state: roots.state.clone(),
            _base: base,
            roots,
            systemd,
            payload,
            manager: SharedFakeSystemd::default(),
            uid: rustix::process::getuid().as_raw(),
        }
    }

    fn binary(&self) -> TargetPath {
        TargetPath::new(
            target(),
            self.roots
                .programs
                .join("acme")
                .join("tool")
                .to_string_lossy(),
        )
        .expect("a path")
    }

    fn plan(&self, version: &str, start: ServiceStart, arguments: Vec<String>) -> TargetPlan {
        let binary = self.binary();
        let destination = binary.clone();
        let (size, sha256) = zup_core::hash_reader(PAYLOAD_BYTES).expect("bytes hash");
        let service = zup_platform::TargetService {
            key: ResourceKey::Service {
                id: zup_core::ServiceId::new(SERVICE_ID).expect("an id"),
            },
            id: zup_core::ServiceId::new(SERVICE_ID).expect("an id"),
            name: zup_core::NonEmptyString::new("Tool").expect("a name"),
            display_name: None,
            command: CommandSpec::new(binary, arguments),
            start,
            privilege: Privilege::System,
        };
        TargetPlan {
            app: app(version),
            target: target(),
            scope: SelectedScope::Machine,
            install_directory: TargetPath::new(
                target(),
                self.roots.programs.join("acme").to_string_lossy(),
            )
            .expect("a path"),
            selected_components: Vec::new(),
            prerequisites: Vec::new(),
            files: vec![TargetFile {
                key: ResourceKey::File {
                    destination: destination.to_string(),
                },
                source_relative: zup_core::RelativePath::new("tool").expect("a path"),
                destination,
                size,
                sha256,
                privilege: Privilege::System,
                executable: true,
            }],
            launchers: Vec::new(),
            path_entries: Vec::new(),
            services: vec![service],
            protocols: Vec::new(),
            file_associations: Vec::new(),
            summary: TargetPlanSummary {
                file_count: 1,
                install_bytes: size,
                resource_count: 1,
                requires_authorization: true,
                selected_component_count: 0,
                prerequisite_count: 0,
                download_bytes: 0,
            },
            preset: None,
        }
    }

    fn unit(&self, plan: &TargetPlan) -> String {
        DesiredService::derive(&plan.services[0])
            .expect("a service derives")
            .unit
    }

    fn seed(&self, unit: &str) {
        let fragment = self
            .systemd
            .unit_dir
            .join(unit)
            .to_string_lossy()
            .into_owned();
        self.manager.borrow_mut().seed_fragment(unit, &fragment);
    }

    fn snapshot(&self, plan: &TargetPlan) -> HostSnapshot {
        let mut snapshot = snapshot_target(plan);
        snapshot.services =
            snapshot_services(plan, &mut self.manager.clone(), &self.systemd).expect("a snapshot");
        snapshot
    }

    fn owned_matches(
        &self,
        ledger: Option<&InstallLedger>,
        snapshot: &HostSnapshot,
    ) -> BTreeMap<ResourceKey, bool> {
        let mut matches = BTreeMap::new();
        let Some(ledger) = ledger else {
            return matches;
        };
        for (key, owned) in &ledger.resources {
            let found = match owned {
                OwnedResource::File {
                    destination,
                    sha256,
                    size,
                    ..
                } => {
                    let host = PathBuf::from(destination.to_string());
                    std::fs::read(&host)
                        .map(|bytes| {
                            zup_core::hash_reader(bytes.as_slice()).is_ok_and(
                                |(found_size, found)| found_size == *size && found == *sha256,
                            )
                        })
                        .unwrap_or(false)
                }
                OwnedResource::Service { installed, .. } => {
                    snapshot.services.iter().any(|observed| {
                        observed.key == *key
                            && match (&observed.state, installed) {
                                (
                                    zup_exec::ObservedServiceState::Service {
                                        display_name,
                                        command,
                                        start,
                                        ..
                                    },
                                    zup_exec::ServiceState::Registration {
                                        display_name: wanted_display,
                                        command: wanted_command,
                                        start: wanted_start,
                                    },
                                ) => {
                                    display_name == wanted_display
                                        && command == wanted_command
                                        && start == wanted_start
                                }
                                _ => false,
                            }
                    })
                }
                _ => false,
            };
            matches.insert(key.clone(), found);
        }
        matches
    }

    fn ledgers(&self) -> LinuxLedgerStore {
        LinuxLedgerStore::new(&self.state)
    }

    fn load_ledger(&self) -> Option<InstallLedger> {
        self.ledgers()
            .load(&AppId::new(APP_ID).expect("an id"), SelectedScope::Machine)
            .expect("a ledger reads")
    }

    fn run(
        &mut self,
        plan: &TargetPlan,
        action: LifecycleAction,
        force: bool,
    ) -> Result<(TransactionOutcome, Option<InstallLedger>), String> {
        let ledgers = self.ledgers();
        let ledger = self.load_ledger();
        let snapshot = self.snapshot(plan);
        let owned_matches = self.owned_matches(ledger.as_ref(), &snapshot);
        let execution = zup_exec::plan_lifecycle(
            action,
            (action != LifecycleAction::Uninstall).then_some(plan),
            Some(&snapshot),
            ledger.as_ref(),
            &owned_matches,
        )
        .map_err(|error| format!("lifecycle: {error}"))?;
        let mut manager = self.manager.clone();
        let input = compile_machine_execution_plan(
            &execution,
            plan,
            ServiceCompilation {
                roots: &self.roots,
                systemd: &self.systemd,
                manager: &mut manager,
                force_services: force,
            },
        )
        .map_err(|error| format!("input: {error}"))?;
        let plan_compiled =
            compile_transaction(&input).map_err(|error| format!("transaction: {error}"))?;
        ledgers
            .validate_plan(
                &plan.app.id,
                SelectedScope::Machine,
                &plan.app.version,
                &plan_compiled,
            )
            .map_err(|error| format!("ledger validation: {error}"))?;
        let store = FilesystemTransactionStore::new(&self.state);
        let coordinator = TransactionCoordinator::new(store);
        let record = coordinator
            .begin(
                plan.app.id.clone(),
                SelectedScope::Machine,
                plan.app.version.clone(),
                plan_compiled,
            )
            .map_err(|error| format!("begin: {error}"))?;
        let mut executor = LinuxFileExecutor::new()
            .with_payload(DirectoryPayloadSource::new(&self.payload))
            .for_machine()
            .with_services(ServiceSupport::isolated(
                self.roots.clone(),
                self.systemd.clone(),
                self.manager.clone(),
                self.uid,
            ));
        executor
            .register_plan(&record.plan)
            .map_err(|error| format!("register: {error}"))?;
        let (record, outcome) = coordinator
            .execute(record, &mut executor)
            .map_err(|error| format!("execute: {error}"))?;
        match outcome {
            TransactionOutcome::Committed => {
                let ledger = ledgers
                    .publish_committed(&record, SelectedScope::Machine)
                    .map_err(|error| format!("publish: {error}"))?;
                Ok((outcome, Some(ledger)))
            }
            TransactionOutcome::RolledBack | TransactionOutcome::RecoveryRequired => {
                Ok((outcome, self.load_ledger()))
            }
        }
    }

    fn commit(
        &mut self,
        plan: &TargetPlan,
        action: LifecycleAction,
        force: bool,
    ) -> Option<InstallLedger> {
        let (outcome, ledger) = self
            .run(plan, action, force)
            .expect("planning accepts the transition");
        assert_eq!(outcome, TransactionOutcome::Committed);
        ledger
    }

    fn begin(&mut self, plan: &TargetPlan) -> zup_transaction::TransactionRecord {
        let snapshot = self.snapshot(plan);
        let owned_matches = self.owned_matches(None, &snapshot);
        let execution = zup_exec::plan_lifecycle(
            LifecycleAction::Install,
            Some(plan),
            Some(&snapshot),
            None,
            &owned_matches,
        )
        .expect("lifecycle plans");
        let mut manager = self.manager.clone();
        let input = compile_machine_execution_plan(
            &execution,
            plan,
            ServiceCompilation {
                roots: &self.roots,
                systemd: &self.systemd,
                manager: &mut manager,
                force_services: false,
            },
        )
        .expect("an input compiles");
        let compiled = compile_transaction(&input).expect("a transaction compiles");
        self.ledgers()
            .validate_plan(
                &plan.app.id,
                SelectedScope::Machine,
                &plan.app.version,
                &compiled,
            )
            .expect("a plan validates");
        let store = FilesystemTransactionStore::new(&self.state);
        TransactionCoordinator::new(store)
            .begin(
                plan.app.id.clone(),
                SelectedScope::Machine,
                plan.app.version.clone(),
                compiled,
            )
            .expect("a journal begins")
    }

    fn begin_uninstall(&mut self, plan: &TargetPlan) -> zup_transaction::TransactionRecord {
        let ledger = self.load_ledger().expect("an installation exists");
        let snapshot = self.snapshot(plan);
        let owned_matches = self.owned_matches(Some(&ledger), &snapshot);
        let execution = zup_exec::plan_lifecycle(
            LifecycleAction::Uninstall,
            None,
            Some(&snapshot),
            Some(&ledger),
            &owned_matches,
        )
        .expect("lifecycle plans");
        let mut manager = self.manager.clone();
        let input = compile_machine_execution_plan(
            &execution,
            plan,
            ServiceCompilation {
                roots: &self.roots,
                systemd: &self.systemd,
                manager: &mut manager,
                force_services: false,
            },
        )
        .expect("an input compiles");
        let compiled = compile_transaction(&input).expect("a transaction compiles");
        self.ledgers()
            .validate_plan(
                &plan.app.id,
                SelectedScope::Machine,
                &plan.app.version,
                &compiled,
            )
            .expect("a plan validates");
        let store = FilesystemTransactionStore::new(&self.state);
        TransactionCoordinator::new(store)
            .begin(
                plan.app.id.clone(),
                SelectedScope::Machine,
                plan.app.version.clone(),
                compiled,
            )
            .expect("a journal begins")
    }

    fn recover(
        &self,
        record: zup_transaction::TransactionRecord,
    ) -> (zup_transaction::TransactionRecord, TransactionOutcome) {
        let store = FilesystemTransactionStore::new(&self.state);
        let mut executor = LinuxFileExecutor::new()
            .with_payload(DirectoryPayloadSource::new(&self.payload))
            .for_machine()
            .with_services(ServiceSupport::isolated(
                self.roots.clone(),
                self.systemd.clone(),
                self.manager.clone(),
                self.uid,
            ));
        executor
            .register_plan(&record.plan)
            .expect("a plan registers");
        zup_transaction::recover(record, &store, &mut executor).expect("recovery settles")
    }

    fn crash_mid_apply(
        &self,
        record: zup_transaction::TransactionRecord,
    ) -> zup_transaction::TransactionRecord {
        let ids: Vec<_> = record
            .plan
            .nodes
            .iter()
            .filter(|node| {
                matches!(
                    &node.kind,
                    NodeKind::BackendOperation { .. } | NodeKind::BackendRemoval { .. }
                )
            })
            .map(|node| node.id.clone())
            .collect();
        assert!(!ids.is_empty(), "the plan holds service work");
        let store = FilesystemTransactionStore::new(&self.state);
        store
            .update(&record.transaction_id, &mut |journaled| {
                journaled.phase = TransactionPhase::Applying;
                for id in &ids {
                    journaled.nodes.insert(id.clone(), NodeState::Running);
                }
                Ok(())
            })
            .expect("the journal advances")
    }

    fn publish(&self, record: &zup_transaction::TransactionRecord) {
        self.ledgers()
            .publish_committed(record, SelectedScope::Machine)
            .expect("a commit publishes");
    }

    fn policy(&self, unit: &str) -> String {
        self.manager
            .borrow_mut()
            .unit_file_state(unit)
            .expect("a state reads")
    }

    fn source(&self, unit: &str) -> Option<Vec<u8>> {
        std::fs::read(self.systemd.unit_dir.join(unit)).ok()
    }
}

#[test]
fn install_registers_boot_policy_without_starting() {
    let mut fixture = Fixture::new();
    let plan = fixture.plan("1.0.0", ServiceStart::Automatic, vec!["--serve".into()]);
    let unit = fixture.unit(&plan);
    fixture.seed(&unit);
    let reloads_before = fixture.manager.borrow().reloads;
    let ledger = fixture.commit(&plan, LifecycleAction::Install, false);
    assert_eq!(fixture.policy(&unit), "enabled");
    assert!(fixture.manager.borrow().reloads > reloads_before);

    let expected = DesiredService::derive(&plan.services[0]).expect("a service derives");
    assert_eq!(
        fixture.source(&unit).expect("a source exists"),
        expected.bytes
    );
    use std::os::unix::fs::PermissionsExt as _;
    let mode = std::fs::metadata(fixture.systemd.unit_dir.join(&unit))
        .expect("a source stats")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o644);

    assert_eq!(
        std::fs::read(fixture.roots.programs.join("acme").join("tool")).expect("a binary reads"),
        PAYLOAD_BYTES
    );
    let ledger = ledger.expect("a ledger publishes");
    let owned = ledger
        .resources
        .get(&ResourceKey::Service {
            id: zup_core::ServiceId::new(SERVICE_ID).expect("an id"),
        })
        .expect("the service is owned");
    assert!(
        matches!(
            owned,
            OwnedResource::Service {
                installed: zup_exec::ServiceState::Registration {
                    start: ServiceStart::Automatic,
                    ..
                },
                ..
            }
        ),
        "the ledger owns the portable registration, not systemd state"
    );
}

#[test]
fn start_policy_transitions_reconcile() {
    let mut fixture = Fixture::new();
    let expected = [
        ("1.0.0", ServiceStart::Automatic, "enabled"),
        ("2.0.0", ServiceStart::Manual, "disabled"),
        ("3.0.0", ServiceStart::Disabled, "masked"),
        ("4.0.0", ServiceStart::Automatic, "enabled"),
    ];
    let mut ledger = None;
    for (version, start, policy) in expected {
        let plan = fixture.plan(version, start, vec!["--serve".into()]);
        let unit = fixture.unit(&plan);
        fixture.seed(&unit);
        let action = if ledger.is_none() {
            LifecycleAction::Install
        } else {
            LifecycleAction::Upgrade
        };
        ledger = fixture.commit(&plan, action, false);
        assert_eq!(fixture.policy(&unit), policy, "{version} reconciles");
        if start == ServiceStart::Disabled {
            assert!(
                fixture.source(&unit).is_some(),
                "a mask keeps the canonical source intact"
            );
        }
    }
    assert!(ledger.is_some());
}

#[test]
fn source_updates_land_transactionally() {
    let mut fixture = Fixture::new();
    let v1 = fixture.plan("1.0.0", ServiceStart::Automatic, vec!["--serve".into()]);
    let unit = fixture.unit(&v1);
    fixture.seed(&unit);
    fixture.commit(&v1, LifecycleAction::Install, false);
    let before = fixture.source(&unit).expect("a source exists");
    let v2 = fixture.plan(
        "2.0.0",
        ServiceStart::Automatic,
        vec!["--serve".into(), "--port=8080".into()],
    );
    assert_eq!(
        fixture.unit(&v2),
        unit,
        "identity is stable across versions"
    );
    let reloads = fixture.manager.borrow().reloads;
    fixture.commit(&v2, LifecycleAction::Upgrade, false);
    let after = fixture.source(&unit).expect("a source exists");
    assert_ne!(before, after, "the source updates");
    assert!(
        fixture.manager.borrow().reloads > reloads,
        "the manager reloads onto the new fragment"
    );
}

#[test]
fn repair_restores_the_source() {
    let mut fixture = Fixture::new();
    let plan = fixture.plan("1.0.0", ServiceStart::Automatic, vec!["--serve".into()]);
    let unit = fixture.unit(&plan);
    fixture.seed(&unit);
    fixture.commit(&plan, LifecycleAction::Install, false);
    let healthy = fixture.source(&unit).expect("a source exists");

    std::fs::remove_file(fixture.systemd.unit_dir.join(&unit)).expect("a deletion");
    eprintln!("STEP repair-after-delete");
    fixture.commit(&plan, LifecycleAction::Repair { force_files: false }, false);
    assert_eq!(fixture.source(&unit).expect("a source restores"), healthy);

    std::fs::write(fixture.systemd.unit_dir.join(&unit), b"[Unit]\n# damaged\n").expect("damage");
    eprintln!("STEP repair-damaged-no-force");
    assert!(
        fixture
            .run(&plan, LifecycleAction::Repair { force_files: false }, false)
            .is_err(),
        "repair without force refuses damaged owned content"
    );
    assert!(
        fixture
            .source(&unit)
            .expect("damage stays")
            .starts_with(b"[Unit]\n#"),
        "nothing was overwritten without force"
    );
    fixture.commit(&plan, LifecycleAction::Repair { force_files: true }, true);
    assert_eq!(fixture.source(&unit).expect("force restores"), healthy);
}

#[test]
fn repair_restores_external_policy_drift() {
    let mut fixture = Fixture::new();
    let plan = fixture.plan("1.0.0", ServiceStart::Automatic, vec!["--serve".into()]);
    let unit = fixture.unit(&plan);
    fixture.seed(&unit);
    fixture.commit(&plan, LifecycleAction::Install, false);

    fixture
        .manager
        .borrow_mut()
        .disable(&unit)
        .expect("an admin edit");
    assert_eq!(fixture.policy(&unit), "disabled");
    assert!(
        fixture
            .run(&plan, LifecycleAction::Repair { force_files: false }, false)
            .is_err(),
        "drift refuses without force"
    );
    fixture.commit(&plan, LifecycleAction::Repair { force_files: true }, true);
    assert_eq!(fixture.policy(&unit), "enabled");

    let reloads = fixture.manager.borrow().reloads;
    fixture.commit(&plan, LifecycleAction::Repair { force_files: false }, false);
    assert_eq!(
        fixture.manager.borrow().reloads,
        reloads,
        "no work, no reload"
    );
}

#[test]
fn uninstall_retires_only_what_zup_owns() {
    let mut fixture = Fixture::new();
    let plan = fixture.plan("1.0.0", ServiceStart::Automatic, vec!["--serve".into()]);
    let unit = fixture.unit(&plan);
    fixture.seed(&unit);
    fixture.commit(&plan, LifecycleAction::Install, false);

    let neighbor = fixture.systemd.unit_dir.join("unrelated.service");
    std::fs::write(&neighbor, b"[Unit]\n").expect("a neighbor");
    fixture.commit(&plan, LifecycleAction::Uninstall, false);
    assert!(fixture.source(&unit).is_none(), "the source retires");
    assert_eq!(
        fixture.policy(&unit),
        "disabled",
        "the mask/enablement retires"
    );
    assert!(fixture.load_ledger().is_none(), "the ledger retires");
    assert_eq!(
        std::fs::read(&neighbor)
            .expect("a neighbor survives")
            .as_slice(),
        b"[Unit]\n"
    );
    assert!(
        fixture.systemd.unit_dir.is_dir(),
        "the shared unit directory itself is never deleted"
    );
}

#[test]
fn a_planted_source_symlink_refuses() {
    let mut fixture = Fixture::new();
    let plan = fixture.plan("1.0.0", ServiceStart::Automatic, vec!["--serve".into()]);
    let unit = fixture.unit(&plan);
    fixture.seed(&unit);
    let elsewhere = fixture._base.path().join("elsewhere.service");
    std::fs::write(&elsewhere, b"[Unit]\n").expect("a target");
    std::os::unix::fs::symlink(&elsewhere, fixture.systemd.unit_dir.join(&unit))
        .expect("a planted link");
    let result = fixture.run(&plan, LifecycleAction::Install, false);
    assert!(
        result.is_err(),
        "a symlink source never installs: {result:?}"
    );
    assert_eq!(
        std::fs::read(&elsewhere)
            .expect("the target survives")
            .as_slice(),
        b"[Unit]\n"
    );
    assert_eq!(fixture.policy(&unit), "disabled", "no policy was applied");
}

#[test]
fn unrelated_admin_enablement_is_preserved() {
    let mut fixture = Fixture::new();
    let v1 = fixture.plan("1.0.0", ServiceStart::Automatic, vec!["--serve".into()]);
    let unit = fixture.unit(&v1);
    fixture.seed(&unit);
    fixture.commit(&v1, LifecycleAction::Install, false);

    fixture.manager.borrow_mut().extra_links.insert(
        unit.clone(),
        vec!["/etc/systemd/system/graphical.target.wants/unit".to_owned()],
    );
    let v2 = fixture.plan(
        "2.0.0",
        ServiceStart::Manual,
        vec!["--serve".into(), "--port=8080".into()],
    );
    let result = fixture.run(&v2, LifecycleAction::Upgrade, false);
    let outcome = result.expect("the transition reaches a stable outcome").0;
    assert!(
        !matches!(outcome, TransactionOutcome::Committed),
        "a broad disable never deletes unrelated state"
    );
    assert_eq!(fixture.policy(&unit), "enabled", "nothing was removed");

    let v1_bytes = DesiredService::derive(&v1.services[0]).expect("a service derives");
    assert_eq!(
        fixture.source(&unit).expect("a source remains"),
        v1_bytes.bytes
    );
    assert!(
        fixture.run(&v2, LifecycleAction::Upgrade, false).is_ok(),
        "the compensated world plans cleanly"
    );
    fixture.manager.borrow_mut().extra_links.clear();
    fixture.commit(&v2, LifecycleAction::Upgrade, false);
    assert_eq!(fixture.policy(&unit), "disabled");
}

#[test]
fn a_lost_reply_reconciles_to_installed() {
    let mut fixture = Fixture::new();
    let plan = fixture.plan("1.0.0", ServiceStart::Automatic, vec!["--serve".into()]);
    let unit = fixture.unit(&plan);
    fixture.seed(&unit);

    let record = fixture.begin(&plan);
    let record = fixture.crash_mid_apply(record);
    let expected = DesiredService::derive(&plan.services[0]).expect("a service derives");
    std::fs::write(fixture.systemd.unit_dir.join(&unit), &expected.bytes).expect("a source");
    fixture
        .manager
        .borrow_mut()
        .enable(&unit)
        .expect("policy applied");
    let (record, outcome) = fixture.recover(record);
    assert_eq!(outcome, TransactionOutcome::Committed);
    fixture.publish(&record);
    assert_eq!(fixture.policy(&unit), "enabled");
    assert!(fixture.load_ledger().is_some());
}

#[test]
fn a_reload_failure_resumes() {
    let mut fixture = Fixture::new();
    let plan = fixture.plan("1.0.0", ServiceStart::Automatic, vec!["--serve".into()]);
    let unit = fixture.unit(&plan);
    fixture.seed(&unit);

    let record = fixture.begin(&plan);
    let record = fixture.crash_mid_apply(record);
    let expected = DesiredService::derive(&plan.services[0]).expect("a service derives");
    std::fs::write(fixture.systemd.unit_dir.join(&unit), &expected.bytes).expect("a source");
    assert_eq!(fixture.policy(&unit), "disabled");
    let (record, outcome) = fixture.recover(record);
    assert_eq!(outcome, TransactionOutcome::Committed, "recovery resumes");
    fixture.publish(&record);
    assert_eq!(fixture.policy(&unit), "enabled");
    assert!(fixture.load_ledger().is_some());
}

#[test]
fn a_mask_before_crash_reconciles() {
    let mut fixture = Fixture::new();
    let plan = fixture.plan("1.0.0", ServiceStart::Disabled, vec!["--serve".into()]);
    let unit = fixture.unit(&plan);
    fixture.seed(&unit);
    let record = fixture.begin(&plan);
    let record = fixture.crash_mid_apply(record);
    let expected = DesiredService::derive(&plan.services[0]).expect("a service derives");
    std::fs::write(fixture.systemd.unit_dir.join(&unit), &expected.bytes).expect("a source");
    fixture
        .manager
        .borrow_mut()
        .mask(&unit)
        .expect("a mask applied");
    let (record, outcome) = fixture.recover(record);
    assert_eq!(outcome, TransactionOutcome::Committed);
    fixture.publish(&record);
    assert_eq!(fixture.policy(&unit), "masked");
    assert!(
        fixture.source(&unit).is_some(),
        "a mask keeps the canonical source intact"
    );
}

#[test]
fn an_interrupted_uninstall_converges() {
    let mut fixture = Fixture::new();
    let plan = fixture.plan("1.0.0", ServiceStart::Automatic, vec!["--serve".into()]);
    let unit = fixture.unit(&plan);
    fixture.seed(&unit);
    fixture.commit(&plan, LifecycleAction::Install, false);

    let record = fixture.begin_uninstall(&plan);
    let record = fixture.crash_mid_apply(record);
    fixture
        .manager
        .borrow_mut()
        .disable(&unit)
        .expect("policy retired");
    let (record, outcome) = fixture.recover(record);
    assert_eq!(outcome, TransactionOutcome::Committed);
    fixture.publish(&record);
    assert!(fixture.source(&unit).is_none());

    fixture.commit(&plan, LifecycleAction::Install, false);
    let record = fixture.begin_uninstall(&plan);
    let record = fixture.crash_mid_apply(record);
    std::fs::remove_file(fixture.systemd.unit_dir.join(&unit)).expect("source gone");
    assert_eq!(fixture.policy(&unit), "enabled", "policy still pending");
    let (record, outcome) = fixture.recover(record);
    assert_eq!(outcome, TransactionOutcome::Committed);
    fixture.publish(&record);
    assert!(fixture.load_ledger().is_none());
}

#[test]
fn a_swapped_binary_refuses_before_registration() {
    let mut fixture = Fixture::new();
    let plan = fixture.plan("1.0.0", ServiceStart::Automatic, vec!["--serve".into()]);
    let unit = fixture.unit(&plan);
    fixture.seed(&unit);

    std::fs::write(fixture.payload.join("tool"), b"attacker-bytes").expect("a swap");
    let result = fixture.run(&plan, LifecycleAction::Install, false);
    let (outcome, ledger) = result.expect("the failure reaches a stable outcome");
    assert!(
        !matches!(outcome, TransactionOutcome::Committed),
        "a swapped binary never registers"
    );
    assert!(ledger.is_none());
    assert!(fixture.source(&unit).is_none(), "no unit was installed");
    assert_eq!(fixture.policy(&unit), "disabled", "no policy was applied");
}

#[test]
fn tampered_unit_bytes_refuse_at_apply() {
    let fixture = Fixture::new();
    let plan = fixture.plan("1.0.0", ServiceStart::Automatic, vec!["--serve".into()]);
    let unit = fixture.unit(&plan);
    fixture.seed(&unit);

    let snapshot = fixture.snapshot(&plan);
    let owned_matches = fixture.owned_matches(None, &snapshot);
    let execution = zup_exec::plan_lifecycle(
        LifecycleAction::Install,
        Some(&plan),
        Some(&snapshot),
        None,
        &owned_matches,
    )
    .expect("lifecycle plans");
    let mut manager = fixture.manager.clone();
    let mut input = compile_machine_execution_plan(
        &execution,
        &plan,
        zup_linux::ServiceCompilation {
            roots: &fixture.roots,
            systemd: &fixture.systemd,
            manager: &mut manager,
            force_services: false,
        },
    )
    .expect("an input compiles");
    for operation in &mut input.backend_operations {
        let mut payload: zup_linux::ServicePayload =
            zup_linux::decode_payload(&operation.payload).expect("a payload decodes");
        if let zup_linux::ServicePayload::Apply { unit_bytes, .. } = &mut payload {
            *unit_bytes = b"[Unit]\nDescription=Forged\n".to_vec();
        }
        operation.payload = zup_linux::encode_payload(&payload).expect("a payload encodes");
    }
    let compiled = compile_transaction(&input).expect("a transaction compiles");
    let store = FilesystemTransactionStore::new(&fixture.state);
    let coordinator = TransactionCoordinator::new(store);
    let record = coordinator
        .begin(
            plan.app.id.clone(),
            SelectedScope::Machine,
            plan.app.version.clone(),
            compiled,
        )
        .expect("a record begins");
    let mut executor = LinuxFileExecutor::new()
        .with_payload(DirectoryPayloadSource::new(&fixture.payload))
        .for_machine()
        .with_services(ServiceSupport::isolated(
            fixture.roots.clone(),
            fixture.systemd.clone(),
            fixture.manager.clone(),
            fixture.uid,
        ));
    executor
        .register_plan(&record.plan)
        .expect("a plan registers");
    let (_, outcome) = coordinator
        .execute(record, &mut executor)
        .expect("execution reaches a stable outcome");
    assert!(
        !matches!(outcome, TransactionOutcome::Committed),
        "a substituted payload never commits"
    );
    assert!(fixture.source(&unit).is_none());
}

#[test]
fn neighboring_unit_files_survive_transitions() {
    let mut fixture = Fixture::new();
    let neighbor = fixture.systemd.unit_dir.join("admin-owned.service");
    std::fs::write(&neighbor, b"[Unit]\n").expect("an admin file");
    let v1 = fixture.plan("1.0.0", ServiceStart::Manual, vec!["--serve".into()]);
    let unit = fixture.unit(&v1);
    fixture.seed(&unit);
    for (version, start) in [
        ("1.0.0", ServiceStart::Manual),
        ("2.0.0", ServiceStart::Disabled),
    ] {
        let plan = fixture.plan(version, start, vec!["--serve".into()]);
        let action = if version == "1.0.0" {
            LifecycleAction::Install
        } else {
            LifecycleAction::Upgrade
        };
        fixture.commit(&plan, action, false);
    }
    fixture.commit(
        &fixture.plan("2.0.0", ServiceStart::Disabled, vec!["--serve".into()]),
        LifecycleAction::Uninstall,
        false,
    );
    assert_eq!(
        std::fs::read(&neighbor)
            .expect("an admin file survives")
            .as_slice(),
        b"[Unit]\n"
    );
    let _ = unit;
}

fn binary_path(fixture: &Fixture) -> PathBuf {
    fixture.roots.programs.join("acme").join("tool")
}

#[test]
fn the_binary_path_is_trusted_machine_content() {
    let fixture = Fixture::new();
    assert_eq!(
        binary_path(&fixture).to_string_lossy(),
        fixture.binary().to_string()
    );
}
