#![cfg(target_os = "linux")]

use std::collections::BTreeMap;
use std::path::PathBuf;

use zup_bundle::DirectoryPayloadSource;
use zup_core::{AppId, Privilege, ResourceKey, SelectedScope, ServiceStart, Target};
use zup_linux::{
    LinuxFileExecutor, LinuxLedgerStore, MachineRoots, RealSystemd, ServiceCompilation,
    ServiceSupport, SystemdManager as _, SystemdRoots, SystemdUnit, compile_machine_execution_plan,
    probe_systemd, snapshot_services, snapshot_target,
};
use zup_plan::{CommandSpec, DesiredFile, DesiredState, TargetPath};
use zup_transaction::{
    FilesystemTransactionStore, TransactionCoordinator, TransactionOutcome, compile_transaction,
};

struct DiagnosticExecutor<'a>(&'a mut LinuxFileExecutor);

impl zup_transaction::OperationExecutor for DiagnosticExecutor<'_> {
    type Error = String;
    fn prepare(&mut self, node: &zup_transaction::TransactionNode) -> Result<(), String> {
        self.0.prepare(node).map_err(|error| {
            eprintln!("prepare {}: {error}", node.id);
            error.to_string()
        })
    }
    fn apply(
        &mut self,
        node: &zup_transaction::TransactionNode,
    ) -> Result<zup_transaction::OperationReceipt, String> {
        self.0.apply(node).map_err(|error| {
            eprintln!("apply {}: {error}", node.id);
            error.to_string()
        })
    }
    fn verify(
        &mut self,
        node: &zup_transaction::TransactionNode,
        receipt: &zup_transaction::OperationReceipt,
    ) -> Result<(), String> {
        self.0.verify(node, receipt).map_err(|error| {
            eprintln!("verify {}: {error}", node.id);
            error.to_string()
        })
    }
    fn rollback(
        &mut self,
        node: &zup_transaction::TransactionNode,
        receipt: &zup_transaction::OperationReceipt,
    ) -> Result<(), String> {
        self.0.rollback(node, receipt).map_err(|error| {
            eprintln!("rollback {}: {error}", node.id);
            error.to_string()
        })
    }
    fn reconcile(
        &mut self,
        node: &zup_transaction::TransactionNode,
        receipt: Option<&zup_transaction::OperationReceipt>,
    ) -> Result<zup_transaction::ReconcileResult, String> {
        self.0.reconcile(node, receipt).map_err(|error| {
            eprintln!("reconcile {}: {error}", node.id);
            error.to_string()
        })
    }
}

fn bus_available() -> bool {
    probe_systemd().is_ok()
}

fn privileged_available() -> bool {
    if std::env::var_os("ZUP_TEST_REAL_SYSTEMD").is_none() {
        return false;
    }
    assert_eq!(
        rustix::process::geteuid().as_raw(),
        0,
        "real service verification requires root"
    );
    assert!(
        bus_available(),
        "real service verification requires a systemd bus"
    );
    true
}

fn analyze_available() -> bool {
    std::process::Command::new("systemd-analyze")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

#[test]
fn rendered_units_verify() {
    if !analyze_available() {
        eprintln!("skipped: no systemd-analyze on this machine");
        return;
    }

    let binary = ["/bin/true", "/usr/bin/true"]
        .into_iter()
        .find(|path| std::fs::metadata(path).is_ok_and(|metadata| metadata.is_file()))
        .expect("a true binary exists on a systemd host");
    for start in [
        ServiceStart::Automatic,
        ServiceStart::Manual,
        ServiceStart::Disabled,
    ] {
        let target = zup_core::Target::host().expect("a supported Linux host");
        let executable = zup_plan::TargetPath::new(target, binary).expect("a path");
        let service = zup_plan::DesiredService {
            lifecycle: Default::default(),
            readiness: Default::default(),
            key: zup_core::ResourceKey::Service {
                id: zup_core::ServiceId::new("verify").expect("an id"),
            },
            id: zup_core::ServiceId::new("verify").expect("an id"),
            name: zup_core::NonEmptyString::new("Verify").expect("a name"),
            display_name: None,
            command: zup_plan::CommandSpec::new(
                executable,
                vec!["--serve".to_owned(), "$HOME".to_owned(), "%u".to_owned()],
            ),
            start,
            privilege: zup_core::Privilege::System,
        };
        let desired = zup_linux::SystemdUnit::derive(&service).expect("a service derives");
        let dir = tempfile::tempdir().expect("a directory");
        let path = dir.path().join(&desired.unit);
        std::fs::write(&path, &desired.bytes).expect("a unit writes");
        let output = std::process::Command::new("systemd-analyze")
            .arg("verify")
            .arg(&path)
            .output()
            .expect("systemd-analyze runs");
        assert!(
            output.status.success(),
            "unit for {start:?} verifies: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn real_systemd_service_lifecycle() {
    if !privileged_available() {
        eprintln!("skipped: needs root, a system bus, and ZUP_TEST_REAL_SYSTEMD=1");
        return;
    }
    let tag = uuid::Uuid::now_v7().simple().to_string();
    let mut world = RealWorld::stage(&tag);
    let plan = world.plan("1.0.0", ServiceStart::Automatic);
    world.resolve_unit(&plan);
    let mask = PathBuf::from("/etc/systemd/system").join(&world.unit);
    std::os::unix::fs::symlink("/dev/null", &mask).expect("a foreign mask");
    world.foreign.push(mask.clone());
    assert!(
        world
            .drive(&plan, zup_plan::LifecycleAction::Install)
            .is_err()
    );
    assert_eq!(
        std::fs::read_link(&mask).expect("the mask survives"),
        PathBuf::from("/dev/null")
    );
    world.remove_foreign_links();
    world.install(ServiceStart::Automatic, "1.0.0");
    world.assert_policy("enabled");
    world.assert_fragment();
    world.assert_inactive("after install");
    world.analyze_verify();

    world.upgrade(ServiceStart::Manual, "2.0.0");
    world.assert_policy("disabled");
    world.assert_unmasked();
    world.assert_owned_link_gone();
    world.assert_inactive("after Manual transition");

    world.upgrade(ServiceStart::Disabled, "3.0.0");
    world.assert_policy("masked");
    assert!(
        world.source_path().is_file(),
        "a mask keeps the canonical source intact"
    );
    world.assert_inactive("while masked");

    world.plant_foreign_links();
    let refused = world.try_upgrade(ServiceStart::Automatic, "4.0.0");
    assert!(
        refused.is_err(),
        "foreign integration refuses the transition"
    );
    world.assert_foreign_links_intact();
    assert_eq!(
        world.policy(),
        "masked",
        "a refused transition changes nothing"
    );
    world.remove_foreign_links();
    world.upgrade(ServiceStart::Automatic, "4.0.0");
    world.assert_policy("enabled");
    world.assert_inactive("after re-enable");

    world.uninstall();
    assert!(
        world.source_path().symlink_metadata().is_err(),
        "the source retires"
    );
    assert!(world.load_ledger().is_none(), "the ledger retires");
}

#[test]
fn real_systemd_running_upgrade_and_uninstall() {
    if !privileged_available() {
        return;
    }
    let tag = uuid::Uuid::now_v7().simple().to_string();
    let mut world = RealWorld::stage(&tag);
    std::fs::write(
        world.payload.join("tool"),
        b"#!/bin/sh\nexec /bin/sleep 300\n",
    )
    .unwrap();
    let mut plan = world.plan("1.0.0", ServiceStart::Manual);
    plan.services[0].lifecycle.install = zup_core::InitialServiceState::Running;
    world.resolve_unit(&plan);
    assert_eq!(
        world
            .drive(&plan, zup_plan::LifecycleAction::Install)
            .unwrap(),
        TransactionOutcome::Committed
    );
    let mut manager = RealSystemd::connect().unwrap();
    let before = manager.runtime(&world.unit).unwrap();
    assert_eq!(before.state, zup_plan::ServiceRuntimeState::Running);
    std::fs::write(
        world.payload.join("tool"),
        b"#!/bin/sh\nexec /bin/sleep 301\n",
    )
    .unwrap();
    let upgrade = world.plan("2.0.0", ServiceStart::Manual);
    assert_eq!(
        world
            .drive(&upgrade, zup_plan::LifecycleAction::Upgrade)
            .unwrap(),
        TransactionOutcome::Committed
    );
    let after = manager.runtime(&world.unit).unwrap();
    assert_eq!(after.state, zup_plan::ServiceRuntimeState::Running);
    assert_ne!(after.execution_generation, before.execution_generation);
    assert_eq!(
        world
            .drive(&upgrade, zup_plan::LifecycleAction::Repair { force: false })
            .unwrap(),
        TransactionOutcome::Committed
    );
    assert_eq!(
        manager.runtime(&world.unit).unwrap().execution_generation,
        after.execution_generation
    );
    manager
        .control(
            &world.unit,
            zup_transaction::ServiceTerminalState::Stopped,
            std::time::Duration::from_secs(30),
        )
        .unwrap();
    std::fs::write(
        world.payload.join("tool"),
        b"#!/bin/sh\nexec /bin/sleep 302\n",
    )
    .unwrap();
    let upgrade = world.plan("3.0.0", ServiceStart::Manual);
    assert_eq!(
        world
            .drive(&upgrade, zup_plan::LifecycleAction::Upgrade)
            .unwrap(),
        TransactionOutcome::Committed
    );
    assert_eq!(
        manager.runtime(&world.unit).unwrap().state,
        zup_plan::ServiceRuntimeState::Stopped
    );
    manager
        .control(
            &world.unit,
            zup_transaction::ServiceTerminalState::Running,
            std::time::Duration::from_secs(30),
        )
        .unwrap();
    assert_eq!(
        world
            .drive(&upgrade, zup_plan::LifecycleAction::Uninstall)
            .unwrap(),
        TransactionOutcome::Committed
    );
    assert!(!world.source_path().exists());
}

#[test]
fn real_systemd_failed_exec_restores_previous_payload_and_runtime() {
    if !privileged_available() {
        return;
    }
    let tag = uuid::Uuid::now_v7().simple().to_string();
    let mut world = RealWorld::stage(&tag);
    let original = b"#!/bin/sh\nexec /bin/sleep 300\n";
    std::fs::write(world.payload.join("tool"), original).unwrap();
    let mut plan = world.plan("1.0.0", ServiceStart::Manual);
    plan.services[0].lifecycle.install = zup_core::InitialServiceState::Running;
    world.resolve_unit(&plan);
    assert_eq!(
        world
            .drive(&plan, zup_plan::LifecycleAction::Install)
            .unwrap(),
        TransactionOutcome::Committed
    );
    std::fs::write(
        world.payload.join("tool"),
        b"#!/zup-test-missing-interpreter\n",
    )
    .unwrap();
    let upgrade = world.plan("2.0.0", ServiceStart::Manual);
    assert_eq!(
        world
            .drive(&upgrade, zup_plan::LifecycleAction::Upgrade)
            .unwrap(),
        TransactionOutcome::RolledBack
    );
    assert_eq!(std::fs::read(world.program.join("svc")).unwrap(), original);
    assert_eq!(
        RealSystemd::connect()
            .unwrap()
            .runtime(&world.unit)
            .unwrap()
            .state,
        zup_plan::ServiceRuntimeState::Running
    );
    world.uninstall();
}

#[test]
fn real_systemd_notify_readiness_and_failure_before_ready() {
    if !privileged_available() {
        return;
    }
    let tag = uuid::Uuid::now_v7().simple().to_string();
    let mut world = RealWorld::stage(&tag);
    let original = b"#!/usr/bin/python3\nimport os, socket, time\na = os.environ['NOTIFY_SOCKET']\ns = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)\ns.connect(('\\0' + a[1:]) if a.startswith('@') else a)\ns.sendall(b'READY=1')\ntime.sleep(300)\n";
    std::fs::write(world.payload.join("tool"), original).unwrap();
    let mut plan = world.plan("1.0.0", ServiceStart::Manual);
    plan.services[0].readiness = zup_core::ServiceReadiness::Notify;
    plan.services[0].lifecycle.install = zup_core::InitialServiceState::Running;
    world.resolve_unit(&plan);
    assert_eq!(
        world
            .drive(&plan, zup_plan::LifecycleAction::Install)
            .unwrap(),
        TransactionOutcome::Committed
    );
    let before = RealSystemd::connect()
        .unwrap()
        .runtime(&world.unit)
        .unwrap()
        .execution_generation;
    assert_eq!(
        world
            .drive(&plan, zup_plan::LifecycleAction::Repair { force: false })
            .unwrap(),
        TransactionOutcome::Committed
    );
    assert_eq!(
        RealSystemd::connect()
            .unwrap()
            .runtime(&world.unit)
            .unwrap()
            .execution_generation,
        before
    );
    std::fs::write(
        world.payload.join("tool"),
        b"#!/usr/bin/python3\nraise SystemExit(7)\n",
    )
    .unwrap();
    let mut upgrade = world.plan("2.0.0", ServiceStart::Manual);
    upgrade.services[0].readiness = zup_core::ServiceReadiness::Notify;
    assert_eq!(
        world
            .drive(&upgrade, zup_plan::LifecycleAction::Upgrade)
            .unwrap(),
        TransactionOutcome::RolledBack
    );
    assert_eq!(std::fs::read(world.program.join("svc")).unwrap(), original);
    assert_eq!(
        RealSystemd::connect()
            .unwrap()
            .runtime(&world.unit)
            .unwrap()
            .state,
        zup_plan::ServiceRuntimeState::Running
    );
    world.uninstall();
}

struct RealWorld {
    tag: String,
    program: PathBuf,
    payload: PathBuf,
    state: PathBuf,
    unit: String,
    source: PathBuf,
    foreign: Vec<PathBuf>,
    foreign_dirs: Vec<PathBuf>,
    _scratch: tempfile::TempDir,
}

impl RealWorld {
    fn stage(tag: &str) -> Self {
        let program = PathBuf::from(format!("/opt/zup-test-{tag}"));
        std::fs::create_dir_all(&program).expect("a program tree");
        let scratch = tempfile::tempdir().expect("a scratch tree");
        let payload = scratch.path().join("payload");
        std::fs::create_dir_all(&payload).expect("a payload tree");
        std::fs::write(payload.join("tool"), b"#!/bin/sh\nexit 0\n").expect("payload bytes");
        let state = scratch.path().join("state");
        std::fs::create_dir_all(&state).expect("a state tree");
        Self {
            tag: tag.to_owned(),
            program,
            payload,
            state,
            unit: String::new(),
            source: PathBuf::new(),
            foreign: Vec::new(),
            foreign_dirs: Vec::new(),
            _scratch: scratch,
        }
    }

    fn target() -> Target {
        zup_core::Target::host().expect("a supported Linux host")
    }

    fn plan(
        &self,
        version: &str,
        start: ServiceStart,
    ) -> DesiredState<zup_plan::TargetPath, zup_plan::TargetPath> {
        let binary = TargetPath::new(Self::target(), &*self.program.join("svc").to_string_lossy())
            .expect("a path");
        let destination = binary.clone();
        let bytes = std::fs::read(self.payload.join("tool")).expect("payload bytes");
        let (size, sha256) = zup_core::hash_reader(bytes.as_slice()).expect("bytes hash");
        let service = zup_plan::DesiredService {
            lifecycle: Default::default(),
            readiness: Default::default(),
            key: ResourceKey::Service {
                id: zup_core::ServiceId::new(format!("zup-test-{}", self.tag)).expect("an id"),
            },
            id: zup_core::ServiceId::new(format!("zup-test-{}", self.tag)).expect("an id"),
            name: zup_core::NonEmptyString::new("Zup Test").expect("a name"),
            display_name: None,
            command: CommandSpec::new(binary, vec!["--serve".into()]),
            start,
            privilege: Privilege::System,
        };
        DesiredState {
            app: zup_core::App {
                id: AppId::new("com.example.zup-test").expect("an id"),
                name: zup_core::NonEmptyString::new("Zup Test").expect("a name"),
                version: semver::Version::parse(version).expect("a version"),
                publisher: None,
                main: None,
                description: None,
            },
            target: Self::target(),
            scope: SelectedScope::Machine,
            install_directory: TargetPath::new(Self::target(), &*self.program.to_string_lossy())
                .expect("a path"),
            selected_components: Vec::new(),
            prerequisites: Vec::new(),
            files: vec![DesiredFile {
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

            preset: None,
        }
    }

    fn resolve_unit(&mut self, plan: &DesiredState<zup_plan::TargetPath, zup_plan::TargetPath>) {
        let desired = SystemdUnit::derive(&plan.services[0]).expect("a service derives");
        self.unit = desired.unit;
        self.source = std::path::Path::new("/usr/local/lib/systemd/system").join(&self.unit);
    }

    fn ledgers(&self) -> LinuxLedgerStore {
        LinuxLedgerStore::new(&self.state)
    }

    fn load_ledger(&self) -> Option<zup_plan::InstallLedger> {
        self.ledgers()
            .load(
                &AppId::new("com.example.zup-test").expect("an id"),
                SelectedScope::Machine,
            )
            .expect("a ledger reads")
    }

    fn snapshot(
        &self,
        plan: &DesiredState<zup_plan::TargetPath, zup_plan::TargetPath>,
        manager: &mut RealSystemd,
    ) -> zup_plan::HostSnapshot {
        let mut snapshot = snapshot_target(plan);
        snapshot.services =
            snapshot_services(plan, manager, &SystemdRoots::production()).expect("a snapshot");
        snapshot
    }

    fn owned_matches(
        ledger: Option<&zup_plan::InstallLedger>,
        snapshot: &zup_plan::HostSnapshot,
    ) -> BTreeMap<ResourceKey, bool> {
        let mut matches = BTreeMap::new();
        let Some(ledger) = ledger else {
            return matches;
        };
        for (key, owned) in &ledger.resources {
            let found = match owned {
                zup_plan::OwnedResource::File {
                    destination,
                    sha256,
                    size,
                    ..
                } => std::fs::read(destination.to_string())
                    .map(|bytes| {
                        zup_core::hash_reader(bytes.as_slice()).is_ok_and(|(found_size, found)| {
                            found_size == *size && found == *sha256
                        })
                    })
                    .unwrap_or(false),
                zup_plan::OwnedResource::Service { installed, .. } => {
                    snapshot.services.iter().any(|observed| {
                        observed.key == *key
                            && match (&observed.state, installed) {
                                (
                                    zup_plan::ObservedServiceState::Service {
                                        display_name,
                                        command,
                                        start,
                                        readiness,
                                        ..
                                    },
                                    zup_plan::ServiceState::Registration {
                                        display_name: wanted_display,
                                        command: wanted_command,
                                        start: wanted_start,
                                        readiness: wanted_readiness,
                                    },
                                ) => {
                                    display_name == wanted_display
                                        && command == wanted_command
                                        && start == wanted_start
                                        && readiness == wanted_readiness
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

    fn drive(
        &mut self,
        plan: &DesiredState<zup_plan::TargetPath, zup_plan::TargetPath>,
        action: zup_plan::LifecycleAction,
    ) -> Result<TransactionOutcome, String> {
        let ledgers = self.ledgers();
        let ledger = self.load_ledger();
        let mut manager = RealSystemd::connect().map_err(|error| format!("connect: {error}"))?;
        let snapshot = self.snapshot(plan, &mut manager);
        let owned_matches = Self::owned_matches(ledger.as_ref(), &snapshot);
        let execution = zup_plan::plan_lifecycle(
            action,
            (action != zup_plan::LifecycleAction::Uninstall).then_some(plan),
            Some(&snapshot),
            ledger.as_ref(),
            &owned_matches,
        )
        .map_err(|error| format!("lifecycle: {error}"))?;
        let mut input = compile_machine_execution_plan(
            &execution,
            (!execution.uninstall).then_some(plan),
            ServiceCompilation {
                ledger: ledger.as_ref(),
                roots: &MachineRoots::production(),
                systemd: &SystemdRoots::production(),
                manager: &mut manager,
            },
        )
        .map_err(|error| format!("input: {error}"))?;
        input.observed_transaction = ledger.as_ref().map(|ledger| ledger.committed_transaction);
        let compiled =
            compile_transaction(&input).map_err(|error| format!("transaction: {error}"))?;
        ledgers
            .validate_plan(
                &plan.app.id,
                SelectedScope::Machine,
                &plan.app.version,
                &compiled,
            )
            .map_err(|error| format!("ledger validation: {error}"))?;
        let store = FilesystemTransactionStore::private_at(&self.state);
        let coordinator = TransactionCoordinator::new(store);
        let record = coordinator
            .begin(
                plan.app.id.clone(),
                SelectedScope::Machine,
                plan.app.version.clone(),
                compiled,
            )
            .map_err(|error| format!("begin: {error}"))?;
        let mut executor = LinuxFileExecutor::new()
            .with_payload(DirectoryPayloadSource::new(&self.payload))
            .for_machine()
            .with_services(
                ServiceSupport::production().map_err(|error| format!("support: {error}"))?,
            );
        executor
            .register_plan(&record.plan)
            .map_err(|error| format!("register: {error}"))?;
        executor.set_transaction_id(record.transaction_id);
        let settled = record
            .execute(&mut DiagnosticExecutor(&mut executor))
            .map_err(|error| format!("execute: {error}"))?;
        let (record, outcome) = (settled.record, settled.outcome);
        match outcome {
            TransactionOutcome::Committed => {
                ledgers
                    .publish_committed(&record, SelectedScope::Machine)
                    .map_err(|error| format!("publish: {error}"))?;
                Ok(outcome)
            }
            TransactionOutcome::RolledBack | TransactionOutcome::RecoveryRequired => Ok(outcome),
        }
    }

    fn install(&mut self, start: ServiceStart, version: &str) {
        let plan = self.plan(version, start);
        self.resolve_unit(&plan);
        let outcome = self
            .drive(&plan, zup_plan::LifecycleAction::Install)
            .expect("install plans and executes");
        assert_eq!(outcome, TransactionOutcome::Committed);
    }

    fn upgrade(&mut self, start: ServiceStart, version: &str) {
        let plan = self.plan(version, start);
        self.resolve_unit(&plan);
        let outcome = self
            .drive(&plan, zup_plan::LifecycleAction::Upgrade)
            .unwrap_or_else(|error| panic!("upgrade {version} ({start:?}): {error}"));
        assert_eq!(outcome, TransactionOutcome::Committed);
    }

    fn try_upgrade(
        &mut self,
        start: ServiceStart,
        version: &str,
    ) -> Result<TransactionOutcome, String> {
        let plan = self.plan(version, start);
        self.resolve_unit(&plan);
        self.drive(&plan, zup_plan::LifecycleAction::Upgrade)
    }

    fn uninstall(&mut self) {
        let plan = self.plan("4.0.0", ServiceStart::Automatic);
        let outcome = self
            .drive(&plan, zup_plan::LifecycleAction::Uninstall)
            .expect("uninstall plans and executes");
        assert_eq!(outcome, TransactionOutcome::Committed);
    }

    fn policy(&self) -> String {
        let mut manager = RealSystemd::connect().expect("the manager connects");
        manager.unit_file_state(&self.unit).expect("a state reads")
    }

    fn source_path(&self) -> &std::path::Path {
        &self.source
    }

    fn assert_policy(&self, expected: &str) {
        assert_eq!(
            self.policy(),
            expected,
            "persistent policy reaches {expected}"
        );
    }

    fn assert_fragment(&self) {
        let mut manager = RealSystemd::connect().expect("the manager connects");
        let info = manager.load_unit(&self.unit).expect("the unit loads");
        assert_eq!(
            info.fragment_path,
            self.source.to_string_lossy(),
            "systemd resolves the canonical source"
        );
    }

    fn assert_unmasked(&self) {
        assert!(
            std::fs::symlink_metadata(format!("/etc/systemd/system/{}", self.unit)).is_err(),
            "no mask link remains"
        );
    }

    fn assert_owned_link_gone(&self) {
        assert!(
            std::fs::symlink_metadata(format!(
                "/etc/systemd/system/multi-user.target.wants/{}",
                self.unit
            ))
            .is_err(),
            "the owned enablement link is gone"
        );
    }

    fn assert_inactive(&self, when: &str) {
        let output = std::process::Command::new("systemctl")
            .args(["is-active", &self.unit])
            .output()
            .expect("systemctl runs");
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "inactive",
            "the service is never started {when}"
        );
    }

    fn analyze_verify(&self) {
        if !analyze_available() {
            eprintln!("skipped analyze oracle: no systemd-analyze");
            return;
        }
        let output = std::process::Command::new("systemd-analyze")
            .arg("verify")
            .arg(&self.source)
            .output()
            .expect("systemd-analyze runs");
        assert!(
            output.status.success(),
            "installed source verifies: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn plant_foreign_links(&mut self) {
        let canonical = self.source.to_string_lossy().into_owned();
        let requires = PathBuf::from("/etc/systemd/system/zup-test-alias.target.requires");
        std::fs::create_dir_all(&requires).expect("a requires tree");
        let requires_link = requires.join(&self.unit);
        std::os::unix::fs::symlink(&canonical, &requires_link).expect("a requires link");
        let alias = PathBuf::from(format!(
            "/etc/systemd/system/zup-test-{}-alias.service",
            self.tag
        ));
        std::os::unix::fs::symlink(&canonical, &alias).expect("an alias");
        let run_wants = PathBuf::from("/run/systemd/system/multi-user.target.wants");
        std::fs::create_dir_all(&run_wants).expect("a runtime wants tree");
        let run_link = run_wants.join(&self.unit);
        std::os::unix::fs::symlink(&canonical, &run_link).expect("a runtime link");
        self.foreign = vec![requires_link, alias, run_link];
        self.foreign_dirs = vec![requires, run_wants];
    }

    fn assert_foreign_links_intact(&self) {
        for link in &self.foreign {
            assert!(
                std::fs::symlink_metadata(link)
                    .is_ok_and(|metadata| metadata.file_type().is_symlink()),
                "foreign link preserved: {}",
                link.display()
            );
        }
    }

    fn remove_foreign_links(&mut self) {
        for link in self.foreign.drain(..) {
            std::fs::remove_file(&link).ok();
        }
        for dir in self.foreign_dirs.drain(..) {
            std::fs::remove_dir(&dir).ok();
        }
    }

    fn cleanup_once(&mut self) {
        if !self.unit.is_empty()
            && self.source.exists()
            && let Ok(mut manager) = RealSystemd::connect()
        {
            let _ = manager.control(
                &self.unit,
                zup_transaction::ServiceTerminalState::Stopped,
                std::time::Duration::from_secs(30),
            );
        }
        if !self.unit.is_empty() {
            if let Ok(mut manager) = RealSystemd::connect() {
                let _ = manager.unmask(&self.unit);
                let _ = manager.remove_owned_enablement(&self.unit, &self.source.to_string_lossy());
            }
            for link in self.foreign.drain(..) {
                std::fs::remove_file(&link).ok();
            }
            for dir in self.foreign_dirs.drain(..) {
                std::fs::remove_dir(&dir).ok();
            }
            std::fs::remove_file(&self.source).ok();
            if let Ok(mut manager) = RealSystemd::connect() {
                let _ = manager.reload();
            }
            if let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                let _ = runtime.block_on(async {
                    tokio::time::timeout(std::time::Duration::from_secs(30), async {
                        let connection = zbus_systemd::zbus::Connection::system().await?;
                        let manager =
                            zbus_systemd::systemd1::ManagerProxy::new(&connection).await?;
                        manager.reset_failed_unit(self.unit.clone()).await
                    })
                    .await
                });
            }
        }
        std::fs::remove_dir_all(&self.program).ok();
    }
}

impl Drop for RealWorld {
    fn drop(&mut self) {
        self.cleanup_once();
    }
}
