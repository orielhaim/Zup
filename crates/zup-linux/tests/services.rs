#![cfg(target_os = "linux")]
#![cfg(feature = "test-support")]

use std::collections::BTreeMap;
use std::path::PathBuf;
use zup_core::TargetPath;
use zup_plan::{NodeKind, compile_deployment};

use zup_bundle::DirectoryPayloadSource;
use zup_core::CommandSpec;
use zup_core::{AppId, Privilege, ResourceKey, SelectedScope, ServiceStart, Target};
use zup_linux::test_support::SharedFakeSystemd;
use zup_linux::{
    LinuxFileExecutor, LinuxInstallationStore, MachineRoots, ServiceCompilation, ServiceSupport,
    SystemdManager as _, SystemdRoots, SystemdUnit, compile_machine_execution_plan,
    snapshot_services, snapshot_target,
};
use zup_plan::{
    DesiredFile, DesiredState, HostSnapshot, InstallationRecord, LifecycleAction, OwnedResource,
};

use zup_transaction::{
    FilesystemTransactionStore, NodeState, TransactionCoordinator, TransactionOutcome,
    TransactionPhase, TransactionStore,
};

const APP_ID: &str = "com.example.tool";
const SERVICE_ID: &str = "tool";
const PAYLOAD_BYTES: &[u8] = b"fake-elf-bytes";

#[test]
fn explicit_start_and_upgrade_preserve_runtime_state() {
    let mut fixture = Fixture::new();
    let mut plan = fixture.plan("1.0.0", ServiceStart::Automatic, vec!["--v1".into()]);
    plan.services[0].lifecycle.install = zup_core::InitialServiceState::Running;
    let unit = fixture.unit(&plan);
    fixture.seed(&unit);
    fixture.commit(&plan, LifecycleAction::Install);
    assert_eq!(
        fixture.manager.runtime(&unit).unwrap().state,
        zup_plan::ServiceRuntimeState::Running
    );
    let first = fixture.manager.runtime(&unit).unwrap().execution_generation;
    let upgraded = fixture.plan("2.0.0", ServiceStart::Automatic, vec!["--v2".into()]);
    fixture.commit(&upgraded, LifecycleAction::Upgrade);
    let runtime = fixture.manager.runtime(&unit).unwrap();
    assert_eq!(runtime.state, zup_plan::ServiceRuntimeState::Running);
    assert_ne!(runtime.execution_generation, first);
    fixture
        .manager
        .control(
            &unit,
            zup_plan::ServiceTerminalState::Stopped,
            std::time::Duration::from_secs(1),
            &[],
        )
        .unwrap();
    let stopped_upgrade = fixture.plan("3.0.0", ServiceStart::Automatic, vec!["--v3".into()]);
    fixture.commit(&stopped_upgrade, LifecycleAction::Upgrade);
    assert_eq!(
        fixture.manager.runtime(&unit).unwrap().state,
        zup_plan::ServiceRuntimeState::Stopped
    );
}

#[test]
fn unchanged_repair_does_not_restart_a_running_service() {
    let mut fixture = Fixture::new();
    let mut plan = fixture.plan("1.0.0", ServiceStart::Manual, Vec::new());
    plan.services[0].lifecycle.install = zup_core::InitialServiceState::Running;
    let unit = fixture.unit(&plan);
    fixture.seed(&unit);
    fixture.commit(&plan, LifecycleAction::Install);
    let before = fixture.manager.runtime(&unit).unwrap();
    fixture.commit(&plan, LifecycleAction::Repair { force: false });
    assert_eq!(fixture.manager.runtime(&unit).unwrap(), before);
    fixture.commit(&plan, LifecycleAction::Uninstall);
    assert_eq!(
        fixture.manager.runtime(&unit).unwrap().state,
        zup_plan::ServiceRuntimeState::Stopped
    );
    assert!(fixture.source(&unit).is_none());
}

#[test]
fn concurrent_native_jobs_refuse_upgrade_without_stopping_the_service() {
    let mut fixture = Fixture::new();
    let mut initial = fixture.plan("1.0.0", ServiceStart::Manual, vec!["--v1".into()]);
    initial.services[0].lifecycle.install = zup_core::InitialServiceState::Running;
    let unit = fixture.unit(&initial);
    fixture.seed(&unit);
    fixture.commit(&initial, LifecycleAction::Install);
    let mut runtime = fixture.manager.runtime(&unit).unwrap();
    runtime.pending_job = true;
    fixture
        .manager
        .borrow_mut()
        .runtimes
        .insert(unit.clone(), runtime.clone());
    let source = fixture.source(&unit).unwrap();
    let upgraded = fixture.plan("2.0.0", ServiceStart::Manual, vec!["--v2".into()]);
    assert!(fixture.run(&upgraded, LifecycleAction::Upgrade).is_err());
    assert_eq!(fixture.manager.runtime(&unit).unwrap(), runtime);
    assert_eq!(fixture.source(&unit).unwrap(), source);
}

#[test]
fn declared_file_changes_and_retired_dependencies_control_only_their_consumer() {
    let mut fixture = Fixture::new();
    let initial = fixture.configured_plan("1.0.0", b"first", true);
    let unit = fixture.unit(&initial);
    fixture.seed(&unit);
    fixture.commit(&initial, LifecycleAction::Install);
    let first = fixture.manager.runtime(&unit).unwrap().execution_generation;

    let retired = fixture.configured_plan("2.0.0", b"second", false);
    fixture.commit(&retired, LifecycleAction::Upgrade);
    let second = fixture.manager.runtime(&unit).unwrap().execution_generation;
    assert_ne!(
        second, first,
        "the old running service still consumes the retired declaration"
    );

    let unrelated = fixture.configured_plan("3.0.0", b"third", false);
    fixture.commit(&unrelated, LifecycleAction::Upgrade);
    assert_eq!(
        fixture.manager.runtime(&unit).unwrap().execution_generation,
        second
    );

    let declared = fixture.configured_plan("3.0.0", b"third", true);
    let ledger = fixture.commit(&declared, LifecycleAction::Modify).unwrap();
    assert_eq!(
        fixture.manager.runtime(&unit).unwrap().execution_generation,
        second
    );
    assert!(
        matches!(ledger.current.resources.get(&declared.services[0].key), Some(OwnedResource::Service(zup_plan::OwnedService { requires_files, .. })) if requires_files == &declared.services[0].requires_files)
    );

    let changed = fixture.configured_plan("5.0.0", b"fourth", true);
    fixture.commit(&changed, LifecycleAction::Upgrade);
    assert_ne!(
        fixture.manager.runtime(&unit).unwrap().execution_generation,
        second
    );
}

#[test]
fn lost_stop_reply_requires_recovery_before_payload_changes() {
    let mut fixture = Fixture::new();
    let mut plan = fixture.plan("1.0.0", ServiceStart::Manual, vec!["--old".into()]);
    plan.services[0].lifecycle.install = zup_core::InitialServiceState::Running;
    let unit = fixture.unit(&plan);
    fixture.seed(&unit);
    fixture.commit(&plan, LifecycleAction::Install);
    let before = fixture.source(&unit).unwrap();
    fixture
        .manager
        .borrow_mut()
        .lose_reply
        .insert("control".into(), "lost".into());
    let upgraded = fixture.plan("2.0.0", ServiceStart::Manual, vec!["--new".into()]);
    let (outcome, _) = fixture.run(&upgraded, LifecycleAction::Upgrade).unwrap();
    assert_eq!(outcome, TransactionOutcome::RecoveryRequired);
    assert_eq!(fixture.source(&unit).unwrap(), before);
    assert_eq!(
        fixture.manager.runtime(&unit).unwrap().state,
        zup_plan::ServiceRuntimeState::Stopped
    );
}

fn target() -> Target {
    Target::parse("linux-x64").expect("a target")
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
    installation: zup_core::InstallationId,
    _base: tempfile::TempDir,
    roots: MachineRoots,
    systemd: SystemdRoots,
    state: PathBuf,
    payload: PathBuf,
    manager: SharedFakeSystemd,
    uid: u32,
}

impl Fixture {
    fn configured_plan(
        &self,
        version: &str,
        bytes: &[u8],
        declared: bool,
    ) -> zup_plan::BoundDeployment {
        let mut plan = self.plan(version, ServiceStart::Manual, Vec::new());
        let destination = TargetPath::new(
            target(),
            self.roots
                .programs
                .join("acme/worker.conf")
                .to_string_lossy(),
        )
        .unwrap();
        std::fs::write(self.payload.join("worker.conf"), bytes).unwrap();
        plan.files.push(DesiredFile {
            key: ResourceKey::File {
                id: zup_core::FileResourceId::Payload {
                    id: zup_core::PayloadId::File {
                        declaration: zup_core::ResourceId::new("fixture-0").unwrap(),
                    },
                },
            },
            source_relative: zup_core::RelativePath::new("worker.conf").unwrap(),
            placement: zup_core::PayloadPlacement::Fixed {
                path: destination.clone(),
                replacement: zup_core::FixedReplacement::Atomic,
            },
            size: bytes.len() as u64,
            sha256: zup_core::hash_bytes(bytes),
            privilege: Privilege::System,
            executable: false,
        });
        plan.services[0].requires_files = if declared {
            vec![destination]
        } else {
            Vec::new()
        };
        plan.services[0].lifecycle.install = zup_core::InitialServiceState::Running;
        plan
    }
    fn new() -> Self {
        let base = tempfile::tempdir().expect("an isolated base");
        let roots = MachineRoots::new(
            base.path().join("opt"),
            base.path().join("var/lib/zup"),
            base.path().join("var/opt"),
            base.path().join("environment.d"),
        );
        let systemd = SystemdRoots::new(base.path().join("units"));
        std::fs::create_dir_all(&systemd.unit_dir).expect("a unit tree");
        let payload = base.path().join("payload");
        std::fs::create_dir_all(&payload).expect("a payload tree");
        std::fs::write(payload.join("tool"), PAYLOAD_BYTES).expect("a service binary");
        Self {
            installation: zup_core::InstallationId::new_v7(),
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

    fn plan(
        &self,
        version: &str,
        start: ServiceStart,
        arguments: Vec<String>,
    ) -> zup_plan::BoundDeployment {
        let binary = self.binary();
        let destination = binary.clone();
        let (size, sha256) = zup_core::hash_reader(PAYLOAD_BYTES).expect("bytes hash");
        let service = zup_plan::DesiredService {
            state_directories: Vec::new(),
            failure: Default::default(),
            execution: zup_core::ServiceExecution::SystemdUser {
                user: "root".into(),
                group: None,
            },
            requires_files: Vec::new(),
            requires_services: Vec::new(),
            dependency_names: Vec::new(),
            lifecycle: Default::default(),
            readiness: Default::default(),
            key: ResourceKey::Service {
                id: zup_core::ServiceId::new(SERVICE_ID).expect("an id"),
            },
            id: zup_core::ServiceId::new(SERVICE_ID).expect("an id"),
            name: zup_core::NonEmptyString::new("tool.service").expect("a name"),
            display_name: None,
            command: CommandSpec::new(binary, arguments),
            start,
            privilege: Privilege::System,
        };
        DesiredState {
            generation_readers: Default::default(),
            data: Vec::new(),
            requirements: (),
            maintenance: None,
            installation: self.installation,
            generation: None,
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
            files: vec![DesiredFile {
                key: ResourceKey::File {
                    id: zup_core::FileResourceId::Payload {
                        id: zup_core::PayloadId::File {
                            declaration: zup_core::ResourceId::new("fixture-1").unwrap(),
                        },
                    },
                },
                source_relative: zup_core::RelativePath::new("tool").expect("a path"),
                placement: zup_core::PayloadPlacement::Fixed {
                    path: destination,
                    replacement: zup_core::FixedReplacement::Atomic,
                },
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

            preset: None,
        }
        .bind_fixed()
        .expect("fixed deployment requirements")
    }

    fn unit(&self, plan: &zup_plan::BoundDeployment) -> String {
        SystemdUnit::derive(&plan.services[0])
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

    fn snapshot(&self, plan: &zup_plan::BoundDeployment) -> HostSnapshot {
        let mut snapshot = snapshot_target(plan);
        snapshot.services =
            snapshot_services(plan, &mut self.manager.clone(), &self.systemd).expect("a snapshot");
        snapshot
    }

    fn owned_matches(
        &self,
        ledger: Option<&InstallationRecord>,
        snapshot: &HostSnapshot,
    ) -> BTreeMap<ResourceKey, bool> {
        let mut matches = BTreeMap::new();
        let Some(ledger) = ledger else {
            return matches;
        };
        for (key, owned) in &ledger.current.resources {
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
                OwnedResource::Service(zup_plan::OwnedService { installed, .. }) => {
                    snapshot.services.iter().any(|observed| {
                        observed.key == *key
                            && observed.state.configuration().as_ref() == Some(installed.as_ref())
                    })
                }
                _ => false,
            };
            matches.insert(key.clone(), found);
        }
        matches
    }

    fn ledgers(&self) -> LinuxInstallationStore {
        LinuxInstallationStore::new(&self.state)
    }

    fn load_ledger(&self) -> Option<InstallationRecord> {
        self.ledgers()
            .load(&AppId::new(APP_ID).expect("an id"), SelectedScope::Machine)
            .expect("a ledger reads")
    }

    fn with_service_sources(&self, plan: &zup_plan::BoundDeployment) -> zup_plan::BoundDeployment {
        let mut plan = plan.clone();
        for service in &plan.services {
            let derived = SystemdUnit::derive(service).unwrap();
            let source = format!("service-{}.unit", service.id);
            std::fs::write(self.payload.join(&source), &derived.bytes).unwrap();
            let key = ResourceKey::File {
                id: zup_core::FileResourceId::Native {
                    id: zup_core::NativeFileId::ServiceSource {
                        id: service.id.clone(),
                    },
                },
            };
            plan.files.retain(|file| file.key != key);
            plan.files.push(DesiredFile {
                key,
                source_relative: zup_core::RelativePath::new(source).unwrap(),
                placement: zup_core::PayloadPlacement::Fixed {
                    path: TargetPath::new(
                        target(),
                        self.systemd.unit_dir.join(&derived.unit).to_string_lossy(),
                    )
                    .unwrap(),
                    replacement: zup_core::FixedReplacement::Atomic,
                },
                size: derived.bytes.len() as u64,
                sha256: zup_core::hash_bytes(&derived.bytes),
                privilege: Privilege::System,
                executable: false,
            });
        }
        plan
    }

    fn run(
        &mut self,
        plan: &zup_plan::BoundDeployment,
        action: LifecycleAction,
    ) -> Result<(TransactionOutcome, Option<InstallationRecord>), String> {
        let prepared = self.with_service_sources(plan);
        let plan = &prepared;
        let ledgers = self.ledgers();
        let ledger = self.load_ledger();
        let snapshot = self.snapshot(plan);
        let owned_matches = self.owned_matches(ledger.as_ref(), &snapshot);
        let execution = zup_plan::plan_lifecycle(
            action,
            (action != LifecycleAction::Uninstall).then_some(plan),
            Some(&snapshot),
            ledger.as_ref(),
            &owned_matches,
        )
        .map_err(|error| format!("lifecycle: {error}"))?;
        let mut manager = self.manager.clone();
        let mut input = compile_machine_execution_plan(
            &execution,
            (!execution.transition.is_remove()).then_some(plan),
            ServiceCompilation {
                ledger: ledger.as_ref(),
                roots: &self.roots,
                systemd: &self.systemd,
                manager: &mut manager,
            },
        )
        .map_err(|error| format!("input: {error}"))?;
        input.observed_transaction = ledger.as_ref().map(|ledger| ledger.current.transaction);
        let plan_compiled =
            compile_deployment(&input).map_err(|error| format!("transaction: {error}"))?;
        ledgers
            .validate_plan(&plan.app.id, SelectedScope::Machine, &plan_compiled)
            .map_err(|error| format!("ledger validation: {error}"))?;
        let store = FilesystemTransactionStore::private_at(&self.state);
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
        executor.set_transaction_id(record.transaction_id);
        let settled = record
            .execute(&mut executor, &ledgers)
            .map_err(|error| format!("execute: {error}"))?;
        let (record, outcome) = (settled.record, settled.outcome);
        match outcome {
            TransactionOutcome::Assessed(_) => panic!("ordinary execution cannot assess"),
            TransactionOutcome::Committed => {
                let ledger = ledgers
                    .reconcile_installation(&record, SelectedScope::Machine)
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
        plan: &zup_plan::BoundDeployment,
        action: LifecycleAction,
    ) -> Option<InstallationRecord> {
        let (outcome, ledger) = self
            .run(plan, action)
            .expect("planning accepts the transition");
        assert_eq!(outcome, TransactionOutcome::Committed);
        ledger
    }

    fn begin(&mut self, plan: &zup_plan::BoundDeployment) -> zup_transaction::TransactionRecord {
        let prepared = self.with_service_sources(plan);
        let plan = &prepared;
        let snapshot = self.snapshot(plan);
        let owned_matches = self.owned_matches(None, &snapshot);
        let execution = zup_plan::plan_lifecycle(
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
            (!execution.transition.is_remove()).then_some(plan),
            ServiceCompilation {
                ledger: None,
                roots: &self.roots,
                systemd: &self.systemd,
                manager: &mut manager,
            },
        )
        .expect("an input compiles");
        let compiled = compile_deployment(&input).expect("a transaction compiles");
        self.ledgers()
            .validate_plan(&plan.app.id, SelectedScope::Machine, &compiled)
            .expect("a plan validates");
        let store = FilesystemTransactionStore::private_at(&self.state);
        TransactionCoordinator::new(store)
            .begin(
                plan.app.id.clone(),
                SelectedScope::Machine,
                plan.app.version.clone(),
                compiled,
            )
            .expect("a journal begins")
            .preserve()
    }

    fn begin_uninstall(
        &mut self,
        plan: &zup_plan::BoundDeployment,
    ) -> zup_transaction::TransactionRecord {
        let ledger = self.load_ledger().expect("an installation exists");
        let snapshot = self.snapshot(plan);
        let owned_matches = self.owned_matches(Some(&ledger), &snapshot);
        let execution = zup_plan::plan_lifecycle(
            LifecycleAction::Uninstall,
            None,
            Some(&snapshot),
            Some(&ledger),
            &owned_matches,
        )
        .expect("lifecycle plans");
        let mut manager = self.manager.clone();
        let mut input = compile_machine_execution_plan(
            &execution,
            (!execution.transition.is_remove()).then_some(plan),
            ServiceCompilation {
                ledger: Some(&ledger),
                roots: &self.roots,
                systemd: &self.systemd,
                manager: &mut manager,
            },
        )
        .expect("an input compiles");
        input.observed_transaction = Some(ledger.current.transaction);
        let compiled = compile_deployment(&input).expect("a transaction compiles");
        self.ledgers()
            .validate_plan(&plan.app.id, SelectedScope::Machine, &compiled)
            .expect("a plan validates");
        let store = FilesystemTransactionStore::private_at(&self.state);
        TransactionCoordinator::new(store)
            .begin(
                plan.app.id.clone(),
                SelectedScope::Machine,
                plan.app.version.clone(),
                compiled,
            )
            .expect("a journal begins")
            .preserve()
    }

    fn recover(
        &self,
        record: zup_transaction::TransactionRecord,
    ) -> (zup_transaction::TransactionRecord, TransactionOutcome) {
        let store = FilesystemTransactionStore::private_at(&self.state);
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
        executor.set_transaction_id(record.transaction_id);
        zup_transaction::recover(record, &store, &mut executor, &self.ledgers())
            .expect("recovery settles")
    }

    fn crash_mid_apply(
        &self,
        mut record: zup_transaction::TransactionRecord,
    ) -> zup_transaction::TransactionRecord {
        let ids: Vec<_> = record
            .plan
            .nodes
            .iter()
            .filter(|node| matches!(&node.kind, NodeKind::NativeService { .. }))
            .map(|node| node.id.clone())
            .collect();
        assert!(!ids.is_empty(), "the plan holds service work");
        let mut executor = LinuxFileExecutor::new()
            .with_payload(DirectoryPayloadSource::new(&self.payload))
            .for_machine()
            .with_services(ServiceSupport::isolated(
                self.roots.clone(),
                self.systemd.clone(),
                self.manager.clone(),
                self.uid,
            ));
        executor.register_plan(&record.plan).unwrap();
        executor.set_transaction_id(record.transaction_id);
        let store = FilesystemTransactionStore::private_at(&self.state);
        record = store
            .update(&record.transaction_id, &mut |current| {
                current.phase = TransactionPhase::Applying;
                Ok(())
            })
            .unwrap();
        for id in record.plan.execution_order.clone() {
            let node = record
                .plan
                .nodes
                .iter()
                .find(|node| node.id == id)
                .unwrap()
                .clone();
            record = store
                .update(&record.transaction_id, &mut |current| {
                    current.nodes.insert(id.clone(), NodeState::Running);
                    Ok(())
                })
                .unwrap();
            if ids.contains(&id) {
                return record;
            }
            if matches!(node.kind, NodeKind::ReserveInstallation) {
                let reservation = store.reserve_installation(&record).unwrap();
                record = store
                    .update(&record.transaction_id, &mut |current| {
                        current.reservation = Some(reservation.clone());
                        current.nodes.insert(
                            id.clone(),
                            NodeState::Applied {
                                receipt: Box::new(zup_transaction::OperationReceipt::Control),
                            },
                        );
                        Ok(())
                    })
                    .unwrap();
                continue;
            }
            let receipt = if node.kind.is_barrier() {
                zup_transaction::OperationReceipt::Control
            } else {
                executor.prepare(&node).unwrap();
                let receipt = executor.apply(&node, &record).unwrap();
                executor.verify(&node, &receipt).unwrap();
                receipt
            };
            let receipt = Box::new(receipt);
            record = store
                .update(&record.transaction_id, &mut |current| {
                    current.nodes.insert(
                        id.clone(),
                        if node.kind.requires_verification() {
                            NodeState::Verified {
                                receipt: receipt.clone(),
                            }
                        } else {
                            NodeState::Applied {
                                receipt: receipt.clone(),
                            }
                        },
                    );
                    Ok(())
                })
                .unwrap();
        }
        panic!("service operation was not reached")
    }

    fn publish(&self, record: &zup_transaction::TransactionRecord) {
        self.ledgers()
            .reconcile_installation(record, SelectedScope::Machine)
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
    let ledger = fixture.commit(&plan, LifecycleAction::Install);
    assert_eq!(fixture.policy(&unit), "enabled");
    assert!(fixture.manager.borrow().reloads > reloads_before);

    let expected = SystemdUnit::derive(&plan.services[0]).expect("a service derives");
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
        .current
        .resources
        .get(&ResourceKey::Service {
            id: zup_core::ServiceId::new(SERVICE_ID).expect("an id"),
        })
        .expect("the service is owned");
    assert!(
        matches!(
            owned,
            OwnedResource::Service(zup_plan::OwnedService { installed, .. })
                if matches!(installed.as_ref(), zup_plan::ServiceState::Registration { start: ServiceStart::Automatic, .. })
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
        ledger = fixture.commit(&plan, action);
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
    fixture.commit(&v1, LifecycleAction::Install);
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
    fixture.commit(&v2, LifecycleAction::Upgrade);
    let after = fixture.source(&unit).expect("a source exists");
    assert_ne!(before, after, "the source updates");
    assert!(
        fixture.manager.borrow().reloads > reloads,
        "the manager reloads onto the new fragment"
    );
}

#[rstest::rstest]
#[case(false)]
#[case(true)]
fn repair_restores_the_source(#[case] fail_reload: bool) {
    let mut fixture = Fixture::new();
    let plan = fixture.plan("1.0.0", ServiceStart::Automatic, vec!["--serve".into()]);
    let unit = fixture.unit(&plan);
    fixture.seed(&unit);
    fixture.commit(&plan, LifecycleAction::Install);
    let healthy = fixture.source(&unit).expect("a source exists");

    std::fs::remove_file(fixture.systemd.unit_dir.join(&unit)).expect("a deletion");
    fixture.commit(&plan, LifecycleAction::Repair { force: false });
    assert_eq!(fixture.source(&unit).expect("a source restores"), healthy);

    std::fs::write(fixture.systemd.unit_dir.join(&unit), b"[Unit]\n# damaged\n").expect("damage");
    assert!(
        fixture
            .run(&plan, LifecycleAction::Repair { force: false })
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
    if fail_reload {
        fixture
            .manager
            .borrow_mut()
            .fail_next
            .insert("reload".into(), "injected failure".into());
    }
    let (outcome, _) = fixture
        .run(&plan, LifecycleAction::Repair { force: true })
        .expect("repair settles");
    assert_eq!(
        outcome,
        if fail_reload {
            TransactionOutcome::RecoveryRequired
        } else {
            TransactionOutcome::Committed
        }
    );
    assert_eq!(fixture.source(&unit).expect("force restores"), healthy);
}

#[test]
fn repair_restores_external_policy_drift() {
    let mut fixture = Fixture::new();
    let plan = fixture.plan("1.0.0", ServiceStart::Automatic, vec!["--serve".into()]);
    let unit = fixture.unit(&plan);
    fixture.seed(&unit);
    fixture.commit(&plan, LifecycleAction::Install);

    fixture
        .manager
        .borrow_mut()
        .disable(&unit)
        .expect("an admin edit");
    assert_eq!(fixture.policy(&unit), "disabled");
    assert!(
        fixture
            .run(&plan, LifecycleAction::Repair { force: false })
            .is_err(),
        "drift refuses without force"
    );
    fixture.commit(&plan, LifecycleAction::Repair { force: true });
    assert_eq!(fixture.policy(&unit), "enabled");

    let reloads = fixture.manager.borrow().reloads;
    fixture.commit(&plan, LifecycleAction::Repair { force: false });
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
    fixture.commit(&plan, LifecycleAction::Install);

    let neighbor = fixture.systemd.unit_dir.join("unrelated.service");
    std::fs::write(&neighbor, b"[Unit]\n").expect("a neighbor");
    fixture.commit(&plan, LifecycleAction::Uninstall);
    assert!(fixture.source(&unit).is_none(), "the source retires");
    assert_eq!(
        fixture.policy(&unit),
        "disabled",
        "the mask/enablement retires"
    );
    assert!(
        !fixture.load_ledger().unwrap().is_installed(),
        "the installation retains its ownership tombstone"
    );
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
    let fixture = Fixture::new();
    let plan = fixture.plan("1.0.0", ServiceStart::Automatic, vec!["--serve".into()]);
    let unit = fixture.unit(&plan);
    fixture.seed(&unit);
    let elsewhere = fixture._base.path().join("elsewhere.service");
    std::fs::write(&elsewhere, b"[Unit]\n").expect("a target");
    std::os::unix::fs::symlink(&elsewhere, fixture.systemd.unit_dir.join(&unit))
        .expect("a planted link");
    let result = snapshot_services(&plan, &mut fixture.manager.clone(), &fixture.systemd);
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
    fixture.commit(&v1, LifecycleAction::Install);

    fixture.manager.borrow_mut().extra_links.insert(
        unit.clone(),
        vec!["/etc/systemd/system/graphical.target.wants/unit".to_owned()],
    );
    let v2 = fixture.plan(
        "2.0.0",
        ServiceStart::Manual,
        vec!["--serve".into(), "--port=8080".into()],
    );
    let result = fixture.run(&v2, LifecycleAction::Upgrade);
    let outcome = result.expect("the transition reaches a stable outcome").0;
    assert_eq!(outcome, TransactionOutcome::RecoveryRequired);
    assert_eq!(fixture.policy(&unit), "enabled", "nothing was removed");

    let v1_bytes = SystemdUnit::derive(&v1.services[0]).expect("a service derives");
    assert_eq!(
        fixture.source(&unit).expect("a source remains"),
        v1_bytes.bytes
    );
    assert!(fixture.run(&v2, LifecycleAction::Upgrade).is_err());
    assert_eq!(
        fixture.manager.borrow().extra_links.get(&unit).unwrap(),
        &["/etc/systemd/system/graphical.target.wants/unit"]
    );
}

#[test]
fn a_lost_reply_reconciles_to_installed() {
    let mut fixture = Fixture::new();
    let plan = fixture.plan("1.0.0", ServiceStart::Automatic, vec!["--serve".into()]);
    let unit = fixture.unit(&plan);
    fixture.seed(&unit);

    let record = fixture.begin(&plan);
    let record = fixture.crash_mid_apply(record);
    let expected = SystemdUnit::derive(&plan.services[0]).expect("a service derives");
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
    let expected = SystemdUnit::derive(&plan.services[0]).expect("a service derives");
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
    let expected = SystemdUnit::derive(&plan.services[0]).expect("a service derives");
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

#[rstest::rstest]
#[case(false)]
#[case(true)]
fn an_interrupted_uninstall_converges(#[case] mask_retired: bool) {
    let mut fixture = Fixture::new();
    let plan = fixture.plan("1.0.0", ServiceStart::Automatic, vec!["--serve".into()]);
    let unit = fixture.unit(&plan);
    fixture.seed(&unit);
    fixture.commit(&plan, LifecycleAction::Install);

    let record = fixture.begin_uninstall(&plan);
    let record = fixture.crash_mid_apply(record);
    assert!(
        fixture.source(&unit).is_none(),
        "owned source removal precedes native deregistration"
    );
    assert_eq!(fixture.policy(&unit), "masked");
    if mask_retired {
        fixture
            .manager
            .borrow_mut()
            .unmask(&unit)
            .expect("mask retired after source removal");
    }
    let (record, outcome) = fixture.recover(record);
    assert_eq!(outcome, TransactionOutcome::Committed);
    fixture.publish(&record);
    assert!(!fixture.load_ledger().unwrap().is_installed());
}

#[test]
fn a_swapped_binary_refuses_before_registration() {
    let mut fixture = Fixture::new();
    let plan = fixture.plan("1.0.0", ServiceStart::Automatic, vec!["--serve".into()]);
    let unit = fixture.unit(&plan);
    fixture.seed(&unit);

    std::fs::write(fixture.payload.join("tool"), b"attacker-bytes").expect("a swap");
    let result = fixture.run(&plan, LifecycleAction::Install);
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
    let plan = fixture.with_service_sources(&fixture.plan(
        "1.0.0",
        ServiceStart::Automatic,
        vec!["--serve".into()],
    ));
    let unit = fixture.unit(&plan);
    fixture.seed(&unit);

    let snapshot = fixture.snapshot(&plan);
    let owned_matches = fixture.owned_matches(None, &snapshot);
    let execution = zup_plan::plan_lifecycle(
        LifecycleAction::Install,
        Some(&plan),
        Some(&snapshot),
        None,
        &owned_matches,
    )
    .expect("lifecycle plans");
    let mut manager = fixture.manager.clone();
    let input = compile_machine_execution_plan(
        &execution,
        Some(&plan),
        zup_linux::ServiceCompilation {
            ledger: None,
            roots: &fixture.roots,
            systemd: &fixture.systemd,
            manager: &mut manager,
        },
    )
    .expect("an input compiles");
    let compiled = compile_deployment(&input).expect("a transaction compiles");
    std::fs::write(
        fixture
            .payload
            .join(format!("service-{}.unit", plan.services[0].id)),
        b"[Unit]\nDescription=Forged\n",
    )
    .unwrap();
    let store = FilesystemTransactionStore::private_at(&fixture.state);
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
    executor.set_transaction_id(record.transaction_id);
    let outcome = record
        .execute(&mut executor, &fixture.ledgers())
        .expect("execution reaches a stable outcome")
        .outcome;
    assert!(
        !matches!(outcome, TransactionOutcome::Committed),
        "a substituted payload never commits"
    );
    assert!(fixture.source(&unit).is_none());
}

#[test]
fn serialized_updates_cannot_claim_service_ownership() {
    let mut fixture = Fixture::new();
    let desired = fixture.plan("1.0.0", ServiceStart::Manual, vec![]);
    let unit = fixture.unit(&desired);
    fixture.seed(&unit);
    let record = fixture.begin(&desired);
    let mut plan = record.plan.clone();
    let mut changed = false;
    for node in &mut plan.nodes {
        if let NodeKind::NativeService { operation } = &mut node.kind
            && let zup_plan::NativeServiceOperation::Configure { service, .. } = operation.as_mut()
        {
            service.kind = zup_plan::IntegrationChangeKind::UpdateOwned;
            changed = true;
        }
    }
    assert!(changed);
    assert!(
        fixture
            .ledgers()
            .validate_plan(&desired.app.id, SelectedScope::Machine, &plan,)
            .is_err()
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
        fixture.commit(&plan, action);
    }
    fixture.commit(
        &fixture.plan("2.0.0", ServiceStart::Disabled, vec!["--serve".into()]),
        LifecycleAction::Uninstall,
    );
    assert_eq!(
        std::fs::read(&neighbor)
            .expect("an admin file survives")
            .as_slice(),
        b"[Unit]\n"
    );
    let _ = unit;
}
