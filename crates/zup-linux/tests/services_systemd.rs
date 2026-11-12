#![cfg(target_os = "linux")]

use std::collections::BTreeMap;
use std::path::PathBuf;
use zup_core::TargetPath;
use zup_plan::compile_deployment;

use zup_bundle::DirectoryPayloadSource;
use zup_core::CommandSpec;
use zup_core::{AppId, Privilege, ResourceKey, SelectedScope, ServiceStart, Target};
use zup_linux::{
    LinuxFileExecutor, LinuxInstallationStore, MachineRoots, RealSystemd, ServiceCompilation,
    ServiceSupport, SystemdManager as _, SystemdRoots, SystemdUnit, compile_machine_execution_plan,
    probe_systemd, snapshot_services, snapshot_target,
};
use zup_plan::{DesiredFile, DesiredState};
use zup_transaction::{FilesystemTransactionStore, TransactionCoordinator, TransactionOutcome};

struct DiagnosticExecutor<'a>(&'a mut LinuxFileExecutor, Option<String>);

impl zup_transaction::OperationExecutor for DiagnosticExecutor<'_> {
    type Error = String;
    fn prepare(&mut self, node: &zup_plan::DeploymentNode) -> Result<(), String> {
        self.0.prepare(node).map_err(|error| {
            eprintln!("prepare {}: {error}", node.id);
            error.to_string()
        })
    }
    fn apply(
        &mut self,
        node: &zup_plan::DeploymentNode,
        transaction: &zup_transaction::TransactionRecord,
    ) -> Result<zup_transaction::OperationReceipt, String> {
        if matches!(node.kind, zup_plan::NodeKind::FileMutation { .. })
            && let Some(unit) = &self.1
        {
            let mut manager = RealSystemd::connect().unwrap();
            assert!(
                manager
                    .control(
                        unit,
                        zup_plan::ServiceTerminalState::Running,
                        std::time::Duration::from_secs(5),
                        &[]
                    )
                    .is_err(),
                "payload replacement accepted concurrent activation"
            );
            assert_eq!(
                manager.runtime(unit).unwrap().state,
                zup_plan::ServiceRuntimeState::Stopped
            );
        }
        self.0.apply(node, transaction).map_err(|error| {
            eprintln!("apply {}: {error}", node.id);
            error.to_string()
        })
    }
    fn verify(
        &mut self,
        node: &zup_plan::DeploymentNode,
        receipt: &zup_transaction::OperationReceipt,
    ) -> Result<(), String> {
        self.0.verify(node, receipt).map_err(|error| {
            eprintln!("verify {}: {error}", node.id);
            error.to_string()
        })
    }
    fn rollback(
        &mut self,
        node: &zup_plan::DeploymentNode,
        receipt: &zup_transaction::OperationReceipt,
    ) -> Result<(), String> {
        self.0.rollback(node, receipt).map_err(|error| {
            eprintln!("rollback {}: {error}", node.id);
            error.to_string()
        })
    }
    fn reconcile(
        &mut self,
        node: &zup_plan::DeploymentNode,
        receipt: Option<&zup_transaction::OperationReceipt>,
        transaction: &zup_transaction::TransactionRecord,
    ) -> Result<zup_transaction::ReconcileResult, String> {
        self.0
            .reconcile(node, receipt, transaction)
            .map_err(|error| {
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
        let executable = zup_core::TargetPath::new(target, binary).expect("a path");
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
            key: zup_core::ResourceKey::Service {
                id: zup_core::ServiceId::new("verify").expect("an id"),
            },
            id: zup_core::ServiceId::new("verify").expect("an id"),
            name: zup_core::NonEmptyString::new("verify.service").expect("a name"),
            display_name: None,
            command: zup_core::CommandSpec::new(
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

#[rstest::rstest]
#[case::static_account(false)]
#[case::dynamic_account(true)]
fn native_service_state_is_owned_while_stopped_and_available_without_zup(#[case] dynamic: bool) {
    if !privileged_available() {
        return;
    }
    let tag = uuid::Uuid::now_v7().simple().to_string();
    let mut world = RealWorld::stage(&tag);
    std::fs::write(
        world.payload.join("tool"),
        b"#!/bin/sh\nprintf native-state > \"$1/database\"\nexec /usr/bin/sleep 120\n",
    )
    .unwrap();
    let mut plan = world.plan("1.0.0", ServiceStart::Manual);
    let service = &mut plan.services[0];
    if dynamic {
        service.execution = zup_core::ServiceExecution::SystemdDynamicUser;
    }
    let id = service.id.clone();
    let data_id = zup_core::ResourceId::new("database").unwrap();
    let name = zup_plan::service_state_name(&plan.app.id, &id, &data_id);
    let unit = zup_linux::SystemdUnit::derive(service).unwrap().unit;
    service.name = zup_core::NonEmptyString::new(&unit).unwrap();
    service.command.arguments = vec!["${data.database}".into()];
    service.state_directories = vec![name.clone()];
    plan.data.push(zup_core::PersistentDirectory {
        id: data_id.clone(),
        location: zup_core::PersistentLocation::Service { service: id },
        compatibility: Default::default(),
    });
    let plan = plan.bind_fixed().unwrap();
    world.resolve_unit(&plan);
    let state_path = PathBuf::from("/var/lib").join(name.as_str());
    assert!(!state_path.exists());
    assert_eq!(
        world
            .drive(&plan, zup_plan::LifecycleAction::Install)
            .unwrap(),
        TransactionOutcome::Committed
    );
    world.assert_inactive("native state initially stopped");
    assert!(!state_path.exists());
    let installed = world.load_ledger().unwrap();
    assert!(matches!(
        installed.data[&data_id].ownership,
        zup_plan::PersistentOwnership::SystemdState { .. }
    ));
    let mut manager = RealSystemd::connect().unwrap();
    manager
        .control(
            &world.unit,
            zup_plan::ServiceTerminalState::Running,
            std::time::Duration::from_secs(10),
            &[],
        )
        .unwrap();
    let database = state_path.join("database");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !database.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    assert_eq!(std::fs::read(&database).unwrap(), b"native-state");
    manager
        .control(
            &world.unit,
            zup_plan::ServiceTerminalState::Stopped,
            std::time::Duration::from_secs(10),
            &[],
        )
        .unwrap();
    assert_eq!(
        world
            .drive(&plan, zup_plan::LifecycleAction::Uninstall)
            .unwrap(),
        TransactionOutcome::Committed
    );
    assert_eq!(std::fs::read(&database).unwrap(), b"native-state");
    assert_eq!(world.load_ledger().unwrap().data, installed.data);
    let data_root = if dynamic {
        PathBuf::from("/var/lib/private").join(name.as_str())
    } else {
        state_path.clone()
    };
    assert!(data_root.starts_with("/var/lib"));
    assert_eq!(
        data_root.file_name(),
        Some(std::ffi::OsStr::new(name.as_str()))
    );
    std::fs::remove_dir_all(data_root).unwrap();
    if dynamic {
        std::fs::remove_file(state_path).unwrap();
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
    let dangling = PathBuf::from("/etc/systemd/system/multi-user.target.wants").join(&world.unit);
    std::os::unix::fs::symlink(world.source_path(), &dangling).unwrap();
    world.foreign.push(dangling.clone());
    assert!(
        world
            .drive(&plan, zup_plan::LifecycleAction::Install)
            .is_err()
    );
    assert_eq!(std::fs::read_link(&dangling).unwrap(), world.source_path());
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
    std::fs::remove_file(world.source_path()).unwrap();
    RealSystemd::connect().unwrap().reload().unwrap();
    let repair = world.plan("3.0.0", ServiceStart::Disabled);
    assert_eq!(
        world
            .drive(&repair, zup_plan::LifecycleAction::Repair { force: false })
            .unwrap(),
        TransactionOutcome::Committed
    );
    assert!(world.source_path().is_file());
    world.assert_policy("masked");
    world.assert_inactive("after restoring a masked definition");

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
    assert!(
        world
            .load_ledger()
            .is_some_and(|record| !record.is_installed()),
        "the installation is retired"
    );
}

#[rstest::rstest]
#[case::running(true)]
#[case::stopped(false)]
fn real_systemd_boot_policy_updates_preserve_execution(#[case] running: bool) {
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
    if running {
        plan.services[0].lifecycle.install = zup_core::InitialServiceState::Running;
    }
    world.resolve_unit(&plan);
    assert_eq!(
        world
            .drive(&plan, zup_plan::LifecycleAction::Install)
            .unwrap(),
        TransactionOutcome::Committed
    );
    let mut manager = RealSystemd::connect().unwrap();
    let before = manager.runtime(&world.unit).unwrap();
    for (version, policy) in [
        ("2.0.0", ServiceStart::Automatic),
        ("3.0.0", ServiceStart::Manual),
    ] {
        let update = world.plan(version, policy);
        assert_eq!(
            world
                .drive(&update, zup_plan::LifecycleAction::Upgrade)
                .unwrap(),
            TransactionOutcome::Committed
        );
        let after = manager.runtime(&world.unit).unwrap();
        assert_eq!(after.state, before.state);
        assert_eq!(after.execution_generation, before.execution_generation);
        assert_eq!(after.manager_generation, before.manager_generation);
        assert!(!after.pending_job);
    }
    world.uninstall();
}

#[rstest::rstest]
#[case::metadata(zup_plan::ServiceRegistrationChange::DisplayName)]
#[case::timeouts(zup_plan::ServiceRegistrationChange::NativeTimeouts)]
#[case::failure(zup_plan::ServiceRegistrationChange::FailurePolicy)]
fn real_systemd_live_updates_preserve_execution(
    #[case] change: zup_plan::ServiceRegistrationChange,
) {
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
    let mut update = world.plan("2.0.0", ServiceStart::Manual);
    match change {
        zup_plan::ServiceRegistrationChange::DisplayName => {
            update.services[0].display_name =
                Some(zup_core::NonEmptyString::new("Updated service metadata").unwrap())
        }
        zup_plan::ServiceRegistrationChange::NativeTimeouts => {
            update.services[0].lifecycle.timeouts = Some(zup_core::ServiceTimeouts::Systemd {
                start_timeout_seconds: 7,
                stop_grace_seconds: 3,
                abort_timeout_seconds: 2,
                settle_seconds: 3,
            })
        }
        zup_plan::ServiceRegistrationChange::FailurePolicy => {
            update.services[0].failure = zup_core::ServiceFailurePolicy::Systemd {
                condition: zup_core::SystemdRestartCondition::OnFailure,
                delay_seconds: 1,
                start_limit_burst: 2,
                start_limit_interval_seconds: 60,
            }
        }
        _ => unreachable!(),
    }
    assert_eq!(
        world
            .drive(&update, zup_plan::LifecycleAction::Upgrade)
            .unwrap(),
        TransactionOutcome::Committed
    );
    let after = manager.runtime(&world.unit).unwrap();
    assert_eq!(after.state, zup_plan::ServiceRuntimeState::Running);
    assert_eq!(after.execution_generation, before.execution_generation);
    assert_eq!(after.manager_generation, before.manager_generation);
    assert_eq!(after.failure, update.services[0].failure);
    assert_eq!(
        after.native_timeouts,
        Some(
            update.services[0]
                .lifecycle
                .timeouts_for(&update.services[0].execution)
                .native()
        )
    );
    assert!(!after.pending_job);
    world.uninstall();
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
    world.probe_activation = true;
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
            zup_plan::ServiceTerminalState::Stopped,
            std::time::Duration::from_secs(30),
            &[],
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
            zup_plan::ServiceTerminalState::Running,
            std::time::Duration::from_secs(30),
            &[],
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

#[rstest::rstest]
#[case::no_restart(false)]
#[case::bounded_restart(true)]
fn real_systemd_notify_readiness_and_failure_before_ready(#[case] restart: bool) {
    if !privileged_available() {
        return;
    }
    let tag = uuid::Uuid::now_v7().simple().to_string();
    let mut world = RealWorld::stage(&tag);
    let original = b"#!/usr/bin/python3\nimport os, socket, time\na = os.environ['NOTIFY_SOCKET']\ns = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)\ns.connect(('\\0' + a[1:]) if a.startswith('@') else a)\ns.sendall(b'READY=1')\ntime.sleep(300)\n";
    std::fs::write(world.payload.join("tool"), original).unwrap();
    let mut plan = world.plan("1.0.0", ServiceStart::Manual);
    plan.services[0].readiness = zup_core::ServiceReadiness::Notify;
    if restart {
        plan.services[0].failure = zup_core::ServiceFailurePolicy::Systemd {
            condition: zup_core::SystemdRestartCondition::OnFailure,
            delay_seconds: 1,
            start_limit_burst: 3,
            start_limit_interval_seconds: 60,
        };
    }
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
    upgrade.services[0].failure = plan.services[0].failure;
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

#[rstest::rstest]
#[case::graceful(1, 3)]
#[case::escalated(15, 1)]
fn real_systemd_custom_deadlines_survive_removal(
    #[case] shutdown_delay: u32,
    #[case] stop_grace_seconds: u32,
) {
    if !privileged_available() {
        return;
    }
    let tag = uuid::Uuid::now_v7().simple().to_string();
    let mut world = RealWorld::stage(&tag);
    let source = format!(
        "#!/usr/bin/python3\nimport os, signal, socket, time\ndef stop(*args):\n time.sleep({shutdown_delay})\n raise SystemExit(0)\nsignal.signal(signal.SIGTERM, stop)\na = os.environ['NOTIFY_SOCKET']\ns = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)\ns.connect(('\\0' + a[1:]) if a.startswith('@') else a)\ntime.sleep(0.3)\ns.sendall(b'READY=1')\nwhile True: time.sleep(1)\n"
    );
    std::fs::write(world.payload.join("tool"), source).unwrap();
    let mut plan = world.plan("1.0.0", ServiceStart::Manual);
    plan.services[0].readiness = zup_core::ServiceReadiness::Notify;
    plan.services[0].lifecycle = zup_core::ServiceLifecycle {
        install: zup_core::InitialServiceState::Running,
        timeouts: Some(zup_core::ServiceTimeouts::Systemd {
            start_timeout_seconds: 2,
            stop_grace_seconds,
            abort_timeout_seconds: 2,
            settle_seconds: 3,
        }),
        ..Default::default()
    };
    world.resolve_unit(&plan);
    assert_eq!(
        world
            .drive(&plan, zup_plan::LifecycleAction::Install)
            .unwrap(),
        TransactionOutcome::Committed
    );
    let runtime = RealSystemd::connect()
        .unwrap()
        .runtime(&world.unit)
        .unwrap();
    assert_eq!(
        runtime.native_timeouts,
        Some(plan.services[0].lifecycle.timeouts.unwrap().native())
    );
    assert_eq!(runtime.state, zup_plan::ServiceRuntimeState::Running);
    let before = std::time::Instant::now();
    world.uninstall();
    assert!(before.elapsed() >= std::time::Duration::from_millis(800));
    assert!(before.elapsed() < std::time::Duration::from_secs(10));
    assert!(!world.source.exists());
}

#[test]
fn real_systemd_cancellation_during_readiness_restores_previous_runtime() {
    if !privileged_available() {
        return;
    }
    let tag = uuid::Uuid::now_v7().simple().to_string();
    let mut world = RealWorld::stage(&tag);
    let original = b"#!/usr/bin/python3\nimport time\ntime.sleep(300)\n";
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
    let replacement = b"#!/usr/bin/python3\nimport os, socket, time\na = os.environ['NOTIFY_SOCKET']\ns = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)\ns.connect(('\\0' + a[1:]) if a.startswith('@') else a)\ntime.sleep(2)\ns.sendall(b'READY=1')\ntime.sleep(300)\n";
    std::fs::write(world.payload.join("tool"), replacement).unwrap();
    let mut upgrade = world.plan("2.0.0", ServiceStart::Manual);
    upgrade.services[0].readiness = zup_core::ServiceReadiness::Notify;
    upgrade.services[0].lifecycle.timeouts = Some(zup_core::ServiceTimeouts::Systemd {
        start_timeout_seconds: 5,
        stop_grace_seconds: 2,
        abort_timeout_seconds: 1,
        settle_seconds: 2,
    });
    let cancellation = zup_runtime::CancellationHandle::new();
    let cancel = cancellation.clone();
    let unit = world.unit.clone();
    let (started, start) = std::sync::mpsc::channel();
    let observer = std::thread::spawn(move || {
        start
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut manager = RealSystemd::connect().unwrap();
        loop {
            let runtime = manager.runtime(&unit).unwrap();
            if runtime.state == zup_plan::ServiceRuntimeState::StartPending {
                cancel.cancel();
                return runtime.execution_generation;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "replacement never entered readiness wait"
            );
            std::thread::park_timeout(std::time::Duration::from_millis(20));
        }
    });
    assert_eq!(
        world
            .drive_with_cancel(
                &upgrade,
                zup_plan::LifecycleAction::Upgrade,
                &cancellation,
                Some(started)
            )
            .unwrap(),
        TransactionOutcome::RolledBack
    );
    let cancelled_generation = observer.join().unwrap();
    assert_eq!(std::fs::read(world.program.join("svc")).unwrap(), original);
    let restored = RealSystemd::connect()
        .unwrap()
        .runtime(&world.unit)
        .unwrap();
    assert_eq!(restored.state, zup_plan::ServiceRuntimeState::Running);
    assert_ne!(restored.execution_generation, cancelled_generation);
    assert!(!restored.pending_job);
    world.uninstall();
}

struct RealWorld {
    installation: zup_core::InstallationId,
    tag: String,
    program: PathBuf,
    payload: PathBuf,
    state: PathBuf,
    unit: String,
    probe_activation: bool,
    source: PathBuf,
    foreign: Vec<PathBuf>,
    foreign_dirs: Vec<PathBuf>,
    _scratch: tempfile::TempDir,
}

#[test]
fn real_systemd_configuration_only_upgrade_quiesces_a_declared_consumer() {
    if !privileged_available() {
        return;
    }
    let tag = uuid::Uuid::now_v7().simple().to_string();
    let mut world = RealWorld::stage(&tag);
    let configuration = world.program.join("worker.conf");
    let observed = world.program.join("observed");
    let source = format!(
        "#!/usr/bin/python3\nimport os, socket, time\nopen({observed:?}, 'w').write(open({configuration:?}).read())\na = os.environ['NOTIFY_SOCKET']\ns = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)\ns.connect(('\\0' + a[1:]) if a.startswith('@') else a)\ns.sendall(b'READY=1')\ntime.sleep(300)\n"
    );
    std::fs::write(world.payload.join("tool"), source).unwrap();
    let mut previous_generation = None;
    for (version, bytes) in [
        ("1.0.0", b"first".as_slice()),
        ("2.0.0", b"second".as_slice()),
    ] {
        std::fs::write(world.payload.join("worker.conf"), bytes).unwrap();
        let mut plan = world.plan(version, ServiceStart::Manual);
        let destination =
            TargetPath::new(RealWorld::target(), configuration.to_string_lossy()).unwrap();
        plan.services[0].requires_files = vec![destination.clone()];
        plan.services[0].readiness = zup_core::ServiceReadiness::Notify;
        plan.services[0].lifecycle.install = zup_core::InitialServiceState::Running;
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
                replacement: zup_core::FixedReplacement::Atomic,
                path: destination,
            },
            size: bytes.len() as u64,
            sha256: zup_core::hash_bytes(bytes),
            privilege: Privilege::System,
            executable: false,
        });
        world.resolve_unit(&plan);
        assert_eq!(
            world
                .drive(
                    &plan,
                    if previous_generation.is_none() {
                        zup_plan::LifecycleAction::Install
                    } else {
                        zup_plan::LifecycleAction::Upgrade
                    }
                )
                .unwrap(),
            TransactionOutcome::Committed
        );
        assert_eq!(std::fs::read(&observed).unwrap(), bytes);
        let generation = RealSystemd::connect()
            .unwrap()
            .runtime(&world.unit)
            .unwrap()
            .execution_generation;
        assert_ne!(previous_generation.as_ref(), Some(&generation));
        previous_generation = Some(generation);
    }
    world.uninstall();
    assert!(
        observed.exists(),
        "uninstall preserves an undeclared service output"
    );
}

#[test]
fn ready_service_cannot_commit_changed_managed_input() {
    if !privileged_available() {
        return;
    }
    let mut world = RealWorld::stage(&uuid::Uuid::now_v7().simple().to_string());
    let configuration = world.program.join("worker.conf");
    let source = format!(
        "#!/usr/bin/python3\nimport os, socket, time\nopen({configuration:?}, 'w').write('changed during startup')\na = os.environ['NOTIFY_SOCKET']\ns = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)\ns.connect(('\\0' + a[1:]) if a.startswith('@') else a)\ns.sendall(b'READY=1')\ntime.sleep(300)\n"
    );
    std::fs::write(world.payload.join("tool"), source).unwrap();
    let bytes = b"authorized configuration";
    std::fs::write(world.payload.join("worker.conf"), bytes).unwrap();
    let destination =
        TargetPath::new(RealWorld::target(), configuration.to_string_lossy()).unwrap();
    let mut plan = world.plan("1.0.0", ServiceStart::Manual);
    plan.services[0].requires_files.push(destination.clone());
    plan.services[0].readiness = zup_core::ServiceReadiness::Notify;
    plan.services[0].lifecycle.install = zup_core::InitialServiceState::Running;
    plan.files.push(DesiredFile {
        key: ResourceKey::File {
            id: zup_core::FileResourceId::Payload {
                id: zup_core::PayloadId::File {
                    declaration: zup_core::ResourceId::new("fixture-1").unwrap(),
                },
            },
        },
        source_relative: zup_core::RelativePath::new("worker.conf").unwrap(),
        placement: zup_core::PayloadPlacement::Fixed {
            replacement: zup_core::FixedReplacement::Atomic,
            path: destination,
        },
        size: bytes.len() as u64,
        sha256: zup_core::hash_bytes(bytes),
        privilege: Privilege::System,
        executable: false,
    });
    world.resolve_unit(&plan);
    assert_eq!(
        world
            .drive(&plan, zup_plan::LifecycleAction::Install)
            .unwrap(),
        TransactionOutcome::RecoveryRequired
    );
    assert!(
        world.load_ledger().is_none(),
        "changed payload must not publish ownership"
    );
    assert_eq!(
        std::fs::read(&configuration).unwrap(),
        b"changed during startup"
    );
    assert_eq!(
        RealSystemd::connect()
            .unwrap()
            .runtime(&world.unit)
            .unwrap()
            .state,
        zup_plan::ServiceRuntimeState::Stopped
    );
}

#[rstest::rstest]
#[case(false)]
#[case(true)]
fn native_process_scope_remains_populated_after_the_main_process_stops(#[case] masked: bool) {
    if !privileged_available() {
        return;
    }
    struct Unit(String);
    impl Drop for Unit {
        fn drop(&mut self) {
            let _ = std::process::Command::new("systemctl")
                .args(["unmask", &self.0])
                .output();
            let _ = std::process::Command::new("systemctl")
                .args(["kill", "--kill-who=all", "--signal=KILL", &self.0])
                .output();
            let _ = std::process::Command::new("systemctl")
                .args(["stop", &self.0])
                .output();
            let _ = std::process::Command::new("systemctl")
                .args(["reset-failed", &self.0])
                .output();
        }
    }
    let scratch = tempfile::tempdir().unwrap();
    let marker = scratch.path().join("child");
    let unit = Unit(format!(
        "zup-scope-{}.service",
        uuid::Uuid::now_v7().simple()
    ));
    let script = "import os, signal, sys, time\nif os.fork() == 0:\n os.setsid()\n signal.signal(signal.SIGTERM, signal.SIG_IGN)\n open(sys.argv[1], 'w').write(str(os.getpid()))\nwhile True: time.sleep(1)\n";
    let started = std::process::Command::new("systemd-run")
        .args([
            "--unit",
            &unit.0,
            "--property=Type=exec",
            "--property=KillMode=process",
            "--property=NoNewPrivileges=yes",
            "--property=TimeoutStopSec=2",
            "/usr/bin/python3",
            "-c",
            script,
        ])
        .arg(&marker)
        .output()
        .unwrap();
    assert!(
        started.status.success(),
        "{}",
        String::from_utf8_lossy(&started.stderr)
    );
    let mut manager = RealSystemd::connect().unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !marker.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "detached fixture child did not start"
        );
        std::thread::park_timeout(std::time::Duration::from_millis(20));
    }
    assert!(!manager.process_scope_is_empty(&unit.0).unwrap());
    assert!(
        std::process::Command::new("systemctl")
            .args(["stop", &unit.0])
            .status()
            .unwrap()
            .success()
    );
    if masked {
        manager.mask(&unit.0).unwrap();
        manager.reload().unwrap();
    }
    let active = std::process::Command::new("systemctl")
        .args(["show", &unit.0, "--property=ActiveState", "--value"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&active.stdout).trim(), "inactive");
    assert!(
        !manager.process_scope_is_empty(&unit.0).unwrap(),
        "inactive does not prove scope quiescence"
    );
    assert!(
        std::process::Command::new("systemctl")
            .args(["kill", "--kill-who=all", "--signal=KILL", &unit.0])
            .status()
            .unwrap()
            .success()
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !manager.process_scope_is_empty(&unit.0).unwrap() {
        assert!(
            std::time::Instant::now() < deadline,
            "owned fixture scope did not empty"
        );
        std::thread::park_timeout(std::time::Duration::from_millis(20));
    }
}

#[test]
fn native_service_paths_and_arguments_preserve_dollars_and_specifiers() {
    if !privileged_available() {
        return;
    }
    let tag = uuid::Uuid::now_v7().simple().to_string();
    let mut world = RealWorld::stage(&tag);
    let special = world
        .program
        .with_file_name(format!("zup-test-{tag} $literal %u"));
    std::fs::rename(&world.program, &special).unwrap();
    world.program = special;
    std::fs::write(world.payload.join("tool"), b"#!/bin/sh\ndestination=$1\nshift\nprintf '%s\\0' \"$@\" > \"$destination\"\nexec /bin/sleep 120\n").unwrap();
    let mut plan = world.plan("1.0.0", ServiceStart::Manual);
    let marker = world.program.join("arguments");
    let expected = ["$HOME", "$$HOME", "${HOME}", "%u", "a b", ""];
    plan.services[0].command.arguments = std::iter::once(marker.to_string_lossy().into_owned())
        .chain(expected.iter().map(|value| (*value).to_owned()))
        .collect();
    plan.services[0].lifecycle.install = zup_core::InitialServiceState::Running;
    world.resolve_unit(&plan);
    assert_eq!(
        world
            .drive(&plan, zup_plan::LifecycleAction::Install)
            .unwrap(),
        TransactionOutcome::Committed
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !marker.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "argument fixture did not execute"
        );
        std::thread::park_timeout(std::time::Duration::from_millis(20));
    }
    let expected = expected
        .iter()
        .flat_map(|value| value.as_bytes().iter().copied().chain([0]))
        .collect::<Vec<_>>();
    assert_eq!(std::fs::read(&marker).unwrap(), expected);
    assert_eq!(
        world
            .drive(&plan, zup_plan::LifecycleAction::Uninstall)
            .unwrap(),
        TransactionOutcome::Committed
    );
}

#[test]
fn dynamic_identity_is_native_allocated_and_cannot_hide_a_cached_static_account() {
    if !privileged_available() {
        return;
    }
    let tag = uuid::Uuid::now_v7().simple().to_string();
    let mut world = RealWorld::stage(&tag);
    std::fs::write(
        world.payload.join("tool"),
        b"#!/bin/sh\nexec /bin/sleep 120\n",
    )
    .unwrap();
    let mut plan = world.plan("1.0.0", ServiceStart::Automatic);
    plan.services[0].execution = zup_core::ServiceExecution::SystemdDynamicUser;
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
    assert_eq!(
        before.execution,
        Some(zup_core::ServiceExecution::SystemdDynamicUser)
    );
    plan.services[0].start = ServiceStart::Manual;
    assert_eq!(
        world
            .drive(&plan, zup_plan::LifecycleAction::Modify)
            .unwrap(),
        TransactionOutcome::Committed
    );
    assert_eq!(
        manager.runtime(&world.unit).unwrap().execution_generation,
        before.execution_generation
    );

    assert_eq!(
        world
            .drive(&plan, zup_plan::LifecycleAction::Uninstall)
            .unwrap(),
        TransactionOutcome::Committed
    );
    plan.services[0].lifecycle.install = zup_core::InitialServiceState::Stopped;
    assert_eq!(
        world
            .drive(&plan, zup_plan::LifecycleAction::Install)
            .unwrap(),
        TransactionOutcome::Committed
    );
    assert_eq!(
        manager.runtime(&world.unit).unwrap().state,
        zup_plan::ServiceRuntimeState::Stopped
    );

    let path = world.source_path().to_path_buf();
    let original = std::fs::read_to_string(&path).unwrap();
    let foreign = original
        .lines()
        .map(|line| {
            if line.starts_with("User=") {
                "User=root"
            } else if line.starts_with("Group=") {
                "Group=root"
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&path, foreign).unwrap();
    manager.reload().unwrap();
    manager.retain_unit(&world.unit).unwrap();
    let timestamp = std::fs::metadata(&path).unwrap().modified().unwrap();
    std::fs::write(&path, &original).unwrap();
    std::fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(timestamp))
        .unwrap();
    assert!(!manager.load_unit(&world.unit).unwrap().need_daemon_reload);
    let refused = manager.runtime(&world.unit).is_err();
    manager.reload().unwrap();
    assert!(
        refused,
        "a cached static account was accepted as dynamic execution"
    );
    assert_eq!(
        world
            .drive(&plan, zup_plan::LifecycleAction::Uninstall)
            .unwrap(),
        TransactionOutcome::Committed
    );
}

#[rstest::rstest]
#[case::account("User=nobody", "User=root")]
#[case::privileges("NoNewPrivileges=yes", "NoNewPrivileges=no")]
fn restored_configuration_cannot_hide_previous_process_credentials(
    #[case] old: &str,
    #[case] foreign: &str,
) {
    if !privileged_available() {
        return;
    }
    let tag = uuid::Uuid::now_v7().simple().to_string();
    let mut world = RealWorld::stage(&tag);
    std::fs::write(
        world.payload.join("tool"),
        b"#!/bin/sh\nexec /bin/sleep 120\n",
    )
    .unwrap();
    let mut plan = world.plan("1.0.0", ServiceStart::Manual);
    plan.services[0].execution = zup_core::ServiceExecution::SystemdUser {
        user: "nobody".into(),
        group: None,
    };
    world.resolve_unit(&plan);
    assert_eq!(
        world
            .drive(&plan, zup_plan::LifecycleAction::Install)
            .unwrap(),
        TransactionOutcome::Committed
    );
    let path = world.source_path().to_path_buf();
    let original = std::fs::read_to_string(&path).unwrap();
    assert!(original.contains(old));
    std::fs::write(&path, original.replace(old, foreign)).unwrap();
    let mut manager = RealSystemd::connect().unwrap();
    manager.reload().unwrap();
    manager
        .control(
            &world.unit,
            zup_plan::ServiceTerminalState::Running,
            std::time::Duration::from_secs(5),
            &[],
        )
        .unwrap();
    std::fs::write(&path, original).unwrap();
    manager.reload().unwrap();
    let refused = manager.runtime(&world.unit).is_err();
    manager
        .control(
            &world.unit,
            zup_plan::ServiceTerminalState::Stopped,
            std::time::Duration::from_secs(5),
            &[],
        )
        .unwrap();
    assert_eq!(
        world
            .drive(&plan, zup_plan::LifecycleAction::Uninstall)
            .unwrap(),
        TransactionOutcome::Committed
    );
    assert!(
        refused,
        "restored registration concealed the running process credentials"
    );
}

#[rstest::rstest]
#[case("ExecStop=/usr/bin/true")]
#[case("ExecStart=+:/usr/bin/true")]
#[case("ExecStart=:/usr/bin/true")]
#[case("AmbientCapabilities=CAP_DAC_OVERRIDE")]
#[case("KillSignal=SIGKILL")]
#[case("Environment=LD_PRELOAD=/unmanaged.so")]
#[case("RootDirectory=/unmanaged")]
#[case("SuccessExitStatus=42")]
#[case("RuntimeMaxSec=1")]
#[case("StartLimitBurst=1")]
#[case("Requires=unmanaged.service")]
#[case("Wants=unmanaged.service")]
#[case("OnFailure=unmanaged.service")]
#[case("Type=simple")]
#[case("NotifyAccess=all")]
#[case("LoadCredential=unmanaged:/unmanaged")]
#[case("JobTimeoutSec=1")]
fn native_cached_configuration_cannot_hide_behind_restored_source(#[case] directive: &str) {
    if !privileged_available() {
        return;
    }
    let tag = uuid::Uuid::now_v7().simple().to_string();
    let mut world = RealWorld::stage(&tag);
    let plan = world.plan("1.0.0", ServiceStart::Manual);
    world.resolve_unit(&plan);
    world.install(ServiceStart::Manual, "1.0.0");
    let path = world.source_path().to_path_buf();
    let original = std::fs::read(&path).unwrap();
    let mut poisoned = String::from_utf8(original.clone()).unwrap();
    let key = directive.split_once('=').unwrap().0;
    let prefix = format!("{key}=");
    if poisoned.lines().any(|line| line.starts_with(&prefix)) {
        poisoned = poisoned
            .lines()
            .map(|line| {
                if line.starts_with(&prefix) {
                    directive
                } else {
                    line
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
    } else {
        let section = if matches!(key, "Requires" | "Wants" | "OnFailure" | "JobTimeoutSec") {
            "Unit"
        } else {
            "Service"
        };
        poisoned = poisoned.replace(
            &format!("[{section}]"),
            &format!("[{section}]\n{directive}"),
        );
    }
    std::fs::write(&path, poisoned).unwrap();
    let mut manager = RealSystemd::connect().unwrap();
    manager.reload().unwrap();
    manager.retain_unit(&world.unit).unwrap();
    let timestamp = std::fs::metadata(&path).unwrap().modified().unwrap();
    std::fs::write(&path, &original).unwrap();
    std::fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(timestamp))
        .unwrap();
    assert!(
        !manager.load_unit(&world.unit).unwrap().need_daemon_reload,
        "fixture did not retain the poisoned native cache"
    );
    let observed = snapshot_services(&plan, &mut manager, &SystemdRoots::production());
    let refused = observed.is_err()
        || observed
            .unwrap()
            .iter()
            .all(|entry| entry.state.configuration().is_none());
    std::fs::write(&path, original).unwrap();
    manager.reload().unwrap();
    assert!(refused, "cached foreign service configuration was accepted");
}

#[test]
fn production_systemd_authority_binds_the_local_host() {
    const CHILD_MODE: &str = "ZUP_TEST_SYSTEMD_AUTHORITY";
    if let Ok(mode) = std::env::var(CHILD_MODE) {
        let manager = RealSystemd::connect();
        match mode.as_str() {
            "bus_override" => assert!(
                manager.is_ok(),
                "caller bus override changed host authority"
            ),
            "mount_namespace" | "pid_namespace" => {
                assert!(manager.is_err(), "foreign manager namespace was accepted")
            }
            _ => panic!("unknown authority fixture mode"),
        }
        return;
    }
    if !privileged_available() {
        return;
    }
    let executable = std::env::current_exe().unwrap();
    for mode in ["bus_override", "mount_namespace", "pid_namespace"] {
        let mut command = if mode != "bus_override" {
            let mut command = std::process::Command::new("unshare");
            command.arg("--mount");
            if mode == "pid_namespace" {
                command.args(["--pid", "--fork", "--mount-proc"]);
            }
            command.arg(&executable);
            command
        } else {
            std::process::Command::new(&executable)
        };
        let output = command
            .args([
                "--exact",
                "production_systemd_authority_binds_the_local_host",
                "--nocapture",
            ])
            .env(CHILD_MODE, mode)
            .env(
                "DBUS_SYSTEM_BUS_ADDRESS",
                "unix:path=/nonexistent-zup-test-bus",
            )
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{mode}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
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
            installation: zup_core::InstallationId::new_v7(),
            tag: tag.to_owned(),
            program,
            payload,
            state,
            unit: String::new(),
            probe_activation: false,
            source: PathBuf::new(),
            foreign: Vec::new(),
            foreign_dirs: Vec::new(),
            _scratch: scratch,
        }
    }

    fn target() -> Target {
        zup_core::Target::host().expect("a supported Linux host")
    }

    fn plan(&self, version: &str, start: ServiceStart) -> zup_plan::BoundDeployment {
        let binary = TargetPath::new(Self::target(), &*self.program.join("svc").to_string_lossy())
            .expect("a path");
        let destination = binary.clone();
        let bytes = std::fs::read(self.payload.join("tool")).expect("payload bytes");
        let (size, sha256) = zup_core::hash_reader(bytes.as_slice()).expect("bytes hash");
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
            generation_readers: Default::default(),
            data: Vec::new(),
            requirements: (),
            maintenance: None,
            installation: self.installation,
            generation: None,
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
                    id: zup_core::FileResourceId::Payload {
                        id: zup_core::PayloadId::File {
                            declaration: zup_core::ResourceId::new("fixture-2").unwrap(),
                        },
                    },
                },
                source_relative: zup_core::RelativePath::new("tool").expect("a path"),
                placement: zup_core::PayloadPlacement::Fixed {
                    replacement: zup_core::FixedReplacement::Atomic,
                    path: destination,
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

    fn resolve_unit(&mut self, plan: &zup_plan::BoundDeployment) {
        let desired = SystemdUnit::derive(&plan.services[0]).expect("a service derives");
        self.unit = desired.unit;
        self.source = std::path::Path::new("/usr/local/lib/systemd/system").join(&self.unit);
    }

    fn ledgers(&self) -> LinuxInstallationStore {
        LinuxInstallationStore::new(&self.state)
    }

    fn load_ledger(&self) -> Option<zup_plan::InstallationRecord> {
        self.ledgers()
            .load(
                &AppId::new("com.example.zup-test").expect("an id"),
                SelectedScope::Machine,
            )
            .expect("a ledger reads")
    }

    fn snapshot(
        &self,
        plan: &zup_plan::BoundDeployment,
        manager: &mut RealSystemd,
    ) -> zup_plan::HostSnapshot {
        let mut snapshot = snapshot_target(plan);
        snapshot.services =
            snapshot_services(plan, manager, &SystemdRoots::production()).expect("a snapshot");
        snapshot
    }

    fn owned_matches(
        ledger: Option<&zup_plan::InstallationRecord>,
        snapshot: &zup_plan::HostSnapshot,
    ) -> BTreeMap<ResourceKey, bool> {
        let mut matches = BTreeMap::new();
        let Some(ledger) = ledger else {
            return matches;
        };
        for (key, owned) in &ledger.current.resources {
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
                zup_plan::OwnedResource::Service(zup_plan::OwnedService { installed, .. }) => {
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

    fn drive(
        &mut self,
        plan: &zup_plan::BoundDeployment,
        action: zup_plan::LifecycleAction,
    ) -> Result<TransactionOutcome, String> {
        self.drive_with_cancel(plan, action, &zup_runtime::CancellationHandle::new(), None)
    }

    fn drive_with_cancel(
        &mut self,
        plan: &zup_plan::BoundDeployment,
        action: zup_plan::LifecycleAction,
        cancellation: &zup_runtime::CancellationHandle,
        started: Option<std::sync::mpsc::Sender<()>>,
    ) -> Result<TransactionOutcome, String> {
        let mut prepared = plan.clone();
        let bindings = zup_linux::resolve_persistent_bindings(&prepared)?;
        zup_plan::bind_service_arguments(&mut prepared, &bindings)
            .map_err(|error| error.to_string())?;
        for service in &prepared.services {
            let source = SystemdUnit::derive(service).map_err(|error| error.to_string())?;
            let source_relative = format!("service-{}.unit", service.id);
            std::fs::write(self.payload.join(&source_relative), &source.bytes)
                .map_err(|error| error.to_string())?;
            prepared.files.push(DesiredFile {
                key: ResourceKey::File {
                    id: zup_core::FileResourceId::Native {
                        id: zup_core::NativeFileId::ServiceSource {
                            id: service.id.clone(),
                        },
                    },
                },
                source_relative: zup_core::RelativePath::new(source_relative).unwrap(),
                placement: zup_core::PayloadPlacement::Fixed {
                    path: TargetPath::new(
                        Self::target(),
                        format!("/usr/local/lib/systemd/system/{}", source.unit),
                    )
                    .unwrap(),
                    replacement: zup_core::FixedReplacement::Atomic,
                },
                size: source.bytes.len() as u64,
                sha256: zup_core::hash_bytes(&source.bytes),
                privilege: Privilege::System,
                executable: false,
            });
        }
        let plan = &prepared;
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
            (!execution.transition.is_remove()).then_some(plan),
            ServiceCompilation {
                ledger: ledger.as_ref(),
                roots: &MachineRoots::production(),
                systemd: &SystemdRoots::production(),
                manager: &mut manager,
            },
        )
        .map_err(|error| format!("input: {error}"))?;
        let bindings = zup_linux::resolve_persistent_bindings(plan)?;
        input
            .bind_persistent(
                (!execution.transition.is_remove()).then_some(plan),
                ledger.as_ref(),
                None,
                &bindings,
            )
            .map_err(|error| error.to_string())?;
        input.observed_transaction = ledger.as_ref().map(|ledger| ledger.current.transaction);
        let compiled =
            compile_deployment(&input).map_err(|error| format!("transaction: {error}"))?;
        ledgers
            .validate_plan(&plan.app.id, SelectedScope::Machine, &compiled)
            .map_err(|error| format!("ledger validation: {error}"))?;
        let store = FilesystemTransactionStore::private_at(&self.state);
        let coordinator = TransactionCoordinator::new(store);
        let record = coordinator
            .begin(
                plan.app.id.clone(),
                SelectedScope::Machine,
                compiled.requirements.app.version.clone(),
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
        executor.set_state_authority(self.state.clone(), record.app_id.clone(), record.scope);
        executor.set_transaction_id(record.transaction_id);
        let starts: std::collections::BTreeSet<_> = record.plan.nodes.iter().filter(|node| {
            matches!(&node.kind, zup_plan::NodeKind::ServiceControl { operation } if operation.stage == zup_plan::ServiceControlStage::Restore)
        }).map(|node| node.id.to_string()).collect();
        let mut executor = zup_runtime::EventingExecutor::new(
            DiagnosticExecutor(
                &mut executor,
                self.probe_activation.then(|| self.unit.clone()),
            ),
            record.plan.total_work(),
            cancellation,
            move |event| {
                if let zup_runtime::RuntimeEvent::OperationStarted { id } = event
                    && starts.contains(&id)
                    && let Some(started) = &started
                {
                    let _ = started.send(());
                }
            },
            || "cancelled".to_owned(),
        );
        let settled = record
            .execute(&mut executor, &ledgers)
            .map_err(|error| format!("execute: {error}"))?;
        let (record, outcome) = (settled.record, settled.outcome);
        match outcome {
            TransactionOutcome::Assessed(_) => panic!("ordinary recovery cannot assess"),
            TransactionOutcome::Committed => {
                ledgers
                    .reconcile_installation(&record, SelectedScope::Machine)
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
                zup_plan::ServiceTerminalState::Stopped,
                std::time::Duration::from_secs(30),
                &[],
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
