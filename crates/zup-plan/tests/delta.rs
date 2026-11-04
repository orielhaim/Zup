//! Full v1 delta planning tests with a fake machine.

use zup_core::{
    LauncherLocation, Privilege, ProtocolScheme, RelativePath, ResourceKey, SelectedScope,
    ServiceId, ServiceStart, Sha256Digest, Target, hash_reader,
};
use zup_plan::{
    CommandSpec, Conflict, FileOperationKind, HostSnapshot, InstallLedger, IntegrationChangeKind,
    LauncherState, ObservedExtensionState, ObservedFile, ObservedFileAssociation,
    ObservedFileAssociationState, ObservedFileState, ObservedLauncher, ObservedLauncherState,
    ObservedPathEntry, ObservedProtocol, ObservedProtocolState, ObservedService,
    ObservedServiceState, OwnedResource, PathOperationKind, ProtocolState, SearchPath,
    ServiceState, TargetPath, plan_execution,
};

fn digest(bytes: &[u8]) -> Sha256Digest {
    hash_reader(bytes).unwrap().1
}

fn tpath(s: &str) -> TargetPath {
    TargetPath::new(Target::parse("windows-x64").unwrap(), s).unwrap()
}

fn cmd(path: &str, args: &[&str]) -> CommandSpec {
    CommandSpec::new(tpath(path), args.iter().map(|s| (*s).to_owned()).collect())
}

fn sample_target() -> zup_plan::DesiredState<TargetPath, TargetPath> {
    zup_plan::DesiredState {
        app: zup_core::App {
            id: zup_core::AppId::new("com.acme.acme").unwrap(),
            name: zup_core::NonEmptyString::new("Acme").unwrap(),
            version: "1.4.0".parse().unwrap(),
            publisher: None,
            main: None,
            description: None,
        },
        target: Target::parse("windows-x64").unwrap(),
        scope: SelectedScope::Machine,
        install_directory: tpath(r"C:\PF\Acme"),
        selected_components: vec![],
        prerequisites: vec![],
        files: vec![zup_plan::DesiredFile {
            key: ResourceKey::File {
                destination: r"C:\PF\Acme\Acme.exe".into(),
            },
            source_relative: RelativePath::new("Acme.exe").unwrap(),
            destination: tpath(r"C:\PF\Acme\Acme.exe"),
            size: 4,
            sha256: digest(b"data"),
            privilege: Privilege::System,
            executable: false,
        }],
        launchers: vec![zup_plan::DesiredLauncher {
            key: ResourceKey::Launcher {
                location: LauncherLocation::Menu,
                name: "Acme".into(),
            },
            location: LauncherLocation::Menu,
            name: zup_core::NonEmptyString::new("Acme").unwrap(),
            launcher_path: tpath(
                r"C:\ProgramData\Microsoft\Windows\Start Menu\Programs\Acme.launcher",
            ),
            target: tpath(r"C:\PF\Acme\Acme.exe"),
            arguments: vec![],
            working_directory: Some(tpath(r"C:\PF\Acme")),
            privilege: Privilege::System,
        }],
        path_entries: vec![zup_plan::DesiredPathEntry {
            key: ResourceKey::PathEntry {
                value: r"C:\PF\Acme\bin".into(),
            },
            value: tpath(r"C:\PF\Acme\bin"),
            scope: SelectedScope::Machine,
            privilege: Privilege::System,
        }],
        services: vec![zup_plan::DesiredService {
            lifecycle: Default::default(),
            readiness: Default::default(),
            key: ResourceKey::Service {
                id: ServiceId::new("acme-agent").unwrap(),
            },
            id: ServiceId::new("acme-agent").unwrap(),
            name: zup_core::NonEmptyString::new("acme-agent").unwrap(),
            display_name: Some(zup_core::NonEmptyString::new("Acme Agent").unwrap()),
            command: cmd(r"C:\PF\Acme\acme-agent.exe", &[]),
            start: ServiceStart::Automatic,
            privilege: Privilege::System,
        }],
        protocols: vec![zup_plan::DesiredProtocol {
            key: ResourceKey::Protocol {
                scheme: ProtocolScheme::new("acme").unwrap(),
            },
            scheme: ProtocolScheme::new("acme").unwrap(),
            command: cmd(r"C:\PF\Acme\Acme.exe", &["--url", "%1"]),
            scope: SelectedScope::Machine,
            privilege: Privilege::System,
        }],
        file_associations: vec![zup_plan::DesiredFileAssociation {
            key: ResourceKey::FileAssociation {
                id: zup_core::FileAssociationId::new("Acme.Document").unwrap(),
            },
            extension: zup_core::FileExtension::new(".acme").unwrap(),
            id: zup_core::FileAssociationId::new("Acme.Document").unwrap(),
            description: Some("Acme Document".into()),
            command: cmd(r"C:\PF\Acme\Acme.exe", &[]),
            scope: SelectedScope::Machine,
            privilege: Privilege::System,
        }],

        preset: None,
    }
}

fn exact_service() -> ObservedServiceState {
    ObservedServiceState::Service {
        readiness: Default::default(),
        display_name: "Acme Agent".into(),
        command: cmd(r"C:\PF\Acme\acme-agent.exe", &[]),
        start: ServiceStart::Automatic,
        runtime_state: None,
    }
}

#[rstest::rstest]
#[case::startup(false)]
#[case::readiness(true)]
fn execution_cannot_change_the_bound_service_policy(#[case] readiness: bool) {
    let desired = sample_target();
    let mut execution = plan_execution(&desired, &snapshot_happy(), None).unwrap();
    execution.validate_desired(Some(&desired)).unwrap();
    if readiness {
        execution.services[0].readiness = zup_core::ServiceReadiness::Notify;
    } else {
        execution.services[0].lifecycle.install = zup_core::InitialServiceState::Running;
    }
    assert!(execution.validate_desired(Some(&desired)).is_err());
}

fn exact_file_association() -> ObservedFileAssociation {
    ObservedFileAssociation {
        key: ResourceKey::FileAssociation {
            id: zup_core::FileAssociationId::new("Acme.Document").unwrap(),
        },
        extension: ".acme".into(),
        id: "Acme.Document".into(),
        scope: SelectedScope::Machine,
        association_state: ObservedFileAssociationState::Registration {
            description: Some("Acme Document".into()),
            command: cmd(r"C:\PF\Acme\Acme.exe", &[]),
        },
        extension_state: ObservedExtensionState::Mapped {
            association_id: "Acme.Document".into(),
        },
    }
}

fn snapshot_happy() -> HostSnapshot {
    let target = sample_target();
    HostSnapshot {
        files: vec![ObservedFile {
            key: target.files[0].key.clone(),
            path: target.files[0].destination.clone(),
            state: ObservedFileState::File {
                executable: None,
                size: 4,
                sha256: digest(b"data"),
            },
        }],
        launchers: vec![ObservedLauncher {
            key: target.launchers[0].key.clone(),
            launcher_path: target.launchers[0].launcher_path.clone(),
            state: ObservedLauncherState::Absent,
        }],
        path_entries: vec![ObservedPathEntry {
            key: target.path_entries[0].key.clone(),
            desired: target.path_entries[0].value.clone(),
            scope: SelectedScope::Machine,
            search_path: SearchPath::default(),
        }],
        services: vec![ObservedService {
            key: target.services[0].key.clone(),
            id: target.services[0].id.clone(),
            state: exact_service(),
        }],
        protocols: vec![ObservedProtocol {
            key: target.protocols[0].key.clone(),
            scheme: target.protocols[0].scheme.clone(),
            scope: SelectedScope::Machine,
            state: ObservedProtocolState::Absent,
        }],
        file_associations: vec![exact_file_association()],
    }
}

#[test]
fn launcher_and_service_ownership_decisions() {
    let mut target = sample_target();
    let mut snapshot = snapshot_happy();
    let original_launcher = LauncherState::Launcher {
        target: target.launchers[0].target.clone(),
        arguments: vec!["--old".into()],
        working_directory: target.launchers[0].working_directory.clone(),
    };
    let original_service = ServiceState::Registration {
        readiness: Default::default(),
        display_name: "Old Agent".into(),
        command: target.services[0].command.clone(),
        start: ServiceStart::Manual,
    };
    snapshot.launchers[0].state = ObservedLauncherState::Launcher {
        target: target.launchers[0].target.clone(),
        arguments: vec!["--old".into()],
        working_directory: target.launchers[0].working_directory.clone(),
    };
    snapshot.services[0].state = ObservedServiceState::Service {
        readiness: Default::default(),
        display_name: "Old Agent".into(),
        command: target.services[0].command.clone(),
        start: ServiceStart::Manual,
        runtime_state: None,
    };
    let foreign = plan_execution(&target, &snapshot, None).unwrap();
    assert_eq!(foreign.launchers[0].kind, IntegrationChangeKind::Conflict);
    assert_eq!(foreign.services[0].kind, IntegrationChangeKind::Conflict);

    let mut ledger = InstallLedger::new(
        target.app.id.clone(),
        target.target,
        SelectedScope::Machine,
        zup_core::TransactionId::new_v7(),
    );
    ledger.resources.insert(
        target.launchers[0].key.clone(),
        OwnedResource::Launcher {
            launcher_path: target.launchers[0].launcher_path.clone(),
            privilege: Privilege::System,
            previous: LauncherState::Absent,
            installed: original_launcher,
        },
    );
    ledger.resources.insert(
        target.services[0].key.clone(),
        OwnedResource::Service {
            name: target.services[0].name.to_string(),
            privilege: Privilege::System,
            previous: ServiceState::Absent,
            installed: original_service,
        },
    );
    let owned = plan_execution(&target, &snapshot, Some(&ledger)).unwrap();
    assert_eq!(owned.launchers[0].kind, IntegrationChangeKind::UpdateOwned);
    assert_eq!(owned.services[0].kind, IntegrationChangeKind::UpdateOwned);
    snapshot.launchers[0].state = ObservedLauncherState::Absent;
    snapshot.services[0].state = ObservedServiceState::Absent;
    let drift = plan_execution(&target, &snapshot, Some(&ledger)).unwrap();
    assert_eq!(drift.launchers[0].kind, IntegrationChangeKind::Drift);
    assert_eq!(drift.services[0].kind, IntegrationChangeKind::Drift);

    target.launchers[0].arguments = vec!["--old".into()];
    target.services[0].display_name = Some(zup_core::NonEmptyString::new("Old Agent").unwrap());
    target.services[0].start = ServiceStart::Manual;
    snapshot.launchers[0].state = ObservedLauncherState::Launcher {
        target: target.launchers[0].target.clone(),
        arguments: vec!["--old".into()],
        working_directory: target.launchers[0].working_directory.clone(),
    };
    snapshot.services[0].state = ObservedServiceState::Service {
        readiness: Default::default(),
        display_name: "Old Agent".into(),
        command: target.services[0].command.clone(),
        start: ServiceStart::Manual,
        runtime_state: None,
    };
    let exact = plan_execution(&target, &snapshot, Some(&ledger)).unwrap();
    assert_eq!(exact.launchers[0].kind, IntegrationChangeKind::NoOp);
    assert_eq!(exact.services[0].kind, IntegrationChangeKind::NoOp);
}

#[test]
fn e2e_fake_machine_mixed_deltas() {
    let target = sample_target();
    let snapshot = snapshot_happy();
    let plan = plan_execution(&target, &snapshot, None).expect("plan");

    assert_eq!(plan.files[0].kind, FileOperationKind::NoOp);
    assert_eq!(plan.path_entries[0].kind, PathOperationKind::Add);
    assert_eq!(plan.launchers[0].kind, IntegrationChangeKind::Create);
    assert_eq!(plan.services[0].kind, IntegrationChangeKind::NoOp);
    assert_eq!(plan.protocols[0].kind, IntegrationChangeKind::Create);
    assert_eq!(plan.file_associations[0].kind, IntegrationChangeKind::NoOp);

    assert_eq!(plan.files[0].kind, FileOperationKind::NoOp);
    assert_eq!(plan.path_entries[0].kind, PathOperationKind::Add);
    assert_eq!(plan.launchers[0].kind, IntegrationChangeKind::Create);
    assert_eq!(plan.services[0].kind, IntegrationChangeKind::NoOp);
    assert_eq!(plan.protocols[0].kind, IntegrationChangeKind::Create);
    assert_eq!(plan.file_associations[0].kind, IntegrationChangeKind::NoOp);
}

#[test]
fn files_replace_non_file_conflict() {
    let target = sample_target();
    let mut snapshot = snapshot_happy();

    snapshot.files[0].state = ObservedFileState::File {
        executable: None,
        size: 4,
        sha256: digest(b"old!"),
    };
    let plan = plan_execution(&target, &snapshot, None).unwrap();
    assert_eq!(plan.files[0].kind, FileOperationKind::Conflict);

    let mut ledger = InstallLedger::new(
        target.app.id.clone(),
        target.target,
        SelectedScope::Machine,
        zup_core::TransactionId::new_v7(),
    );
    ledger.resources.insert(
        target.files[0].key.clone(),
        OwnedResource::File {
            destination: target.files[0].destination.clone(),
            source_relative: target.files[0].source_relative.clone(),
            sha256: digest(b"old!"),
            size: 4,
            created_directories: vec![],
            executable: false,
            privilege: Privilege::System,
        },
    );
    let plan = plan_execution(&target, &snapshot, Some(&ledger)).unwrap();
    assert_eq!(plan.files[0].kind, FileOperationKind::Replace);

    snapshot.files[0].state = ObservedFileState::File {
        executable: None,
        size: 4,
        sha256: digest(b"edit"),
    };
    let plan = plan_execution(&target, &snapshot, Some(&ledger)).unwrap();
    assert_eq!(plan.files[0].kind, FileOperationKind::Drift);

    snapshot.files[0].state = ObservedFileState::NonFile;
    let plan = plan_execution(&target, &snapshot, None).unwrap();
    assert_eq!(plan.files[0].kind, FileOperationKind::Conflict);
    assert!(matches!(
        plan.files[0].conflict,
        Some(Conflict::TargetNonFile { .. })
    ));
}

#[test]
fn file_association_parts_are_planned_independently() {
    let target = sample_target();
    let mut snapshot = snapshot_happy();

    // Association exact, extension missing.
    snapshot.file_associations[0].extension_state = ObservedExtensionState::Absent;
    let plan = plan_execution(&target, &snapshot, None).unwrap();
    assert_eq!(
        plan.file_associations[0].kind,
        IntegrationChangeKind::Create
    );
    assert_eq!(
        plan.file_associations[0].association_kind,
        IntegrationChangeKind::NoOp
    );
    assert_eq!(
        plan.file_associations[0].extension_kind,
        IntegrationChangeKind::Create
    );

    // Extension exact, association missing.
    snapshot.file_associations[0].association_state = ObservedFileAssociationState::Absent;
    snapshot.file_associations[0].extension_state = ObservedExtensionState::Mapped {
        association_id: "Acme.Document".into(),
    };
    let plan = plan_execution(&target, &snapshot, None).unwrap();
    assert_eq!(
        plan.file_associations[0].kind,
        IntegrationChangeKind::Create
    );
    assert_eq!(
        plan.file_associations[0].association_kind,
        IntegrationChangeKind::Create
    );
    assert_eq!(
        plan.file_associations[0].extension_kind,
        IntegrationChangeKind::NoOp
    );

    // Both absent → Create
    snapshot.file_associations[0].association_state = ObservedFileAssociationState::Absent;
    snapshot.file_associations[0].extension_state = ObservedExtensionState::Absent;
    let plan = plan_execution(&target, &snapshot, None).unwrap();
    assert_eq!(
        plan.file_associations[0].kind,
        IntegrationChangeKind::Create
    );
}

#[test]
fn protocol_upgrade_requires_previous_owned_state() {
    let mut target = sample_target();
    let mut snapshot = snapshot_happy();
    let old = cmd(r"C:\PF\Acme\old.exe", &["%1"]);
    snapshot.protocols[0].state = ObservedProtocolState::Registration {
        command: old.clone(),
        url_protocol_marker: true,
    };
    target.protocols[0].command = cmd(r"C:\PF\Acme\new.exe", &["%1"]);
    let foreign = plan_execution(&target, &snapshot, None).unwrap();
    assert_eq!(foreign.protocols[0].kind, IntegrationChangeKind::Conflict);

    let mut ledger = InstallLedger::new(
        target.app.id.clone(),
        target.target,
        target.scope,
        zup_core::TransactionId::new_v7(),
    );
    ledger.resources.insert(
        target.protocols[0].key.clone(),
        OwnedResource::Protocol {
            privilege: Privilege::System,
            previous: ProtocolState::Absent,
            installed: ProtocolState::Registration { command: old },
        },
    );
    let upgrade = plan_execution(&target, &snapshot, Some(&ledger)).unwrap();
    assert_eq!(
        upgrade.protocols[0].kind,
        IntegrationChangeKind::UpdateOwned
    );

    snapshot.protocols[0].state = ObservedProtocolState::Registration {
        command: target.protocols[0].command.clone(),
        url_protocol_marker: true,
    };
    let drift = plan_execution(&target, &snapshot, Some(&ledger)).unwrap();
    assert_eq!(drift.protocols[0].kind, IntegrationChangeKind::Drift);
}

#[test]
fn ledger_target_mismatch_is_rejected() {
    let target = sample_target();
    let snapshot = snapshot_happy();
    let ledger = InstallLedger::new(
        target.app.id.clone(),
        Target::parse("arm64-pc-windows-msvc").unwrap(),
        target.scope,
        zup_core::TransactionId::new_v7(),
    );
    let error = plan_execution(&target, &snapshot, Some(&ledger)).unwrap_err();
    assert!(matches!(
        error,
        zup_plan::ExecutionPlanError::LedgerMismatch
    ));
}

#[test]
fn missing_observation_rejected() {
    let target = sample_target();
    let snapshot = HostSnapshot {
        files: snapshot_happy().files,
        ..Default::default()
    };
    let err = plan_execution(&target, &snapshot, None).unwrap_err();
    assert!(matches!(
        err,
        zup_plan::ExecutionPlanError::MissingSnapshotObservation { .. }
    ));
}

#[test]
fn user_state_cannot_authorize_system_service_ownership() {
    let mut target = sample_target();
    target.scope = SelectedScope::User;
    target.files[0].privilege = Privilege::User;
    target.launchers[0].privilege = Privilege::User;
    target.path_entries[0].scope = SelectedScope::User;
    target.path_entries[0].privilege = Privilege::User;
    target.protocols[0].scope = SelectedScope::User;
    target.protocols[0].privilege = Privilege::User;
    target.file_associations[0].scope = SelectedScope::User;
    target.file_associations[0].privilege = Privilege::User;
    target.services[0].privilege = Privilege::System;

    let mut snapshot = snapshot_happy();
    snapshot.path_entries[0].scope = SelectedScope::User;
    snapshot.protocols[0].scope = SelectedScope::User;
    snapshot.file_associations[0].scope = SelectedScope::User;

    assert!(matches!(
        plan_execution(&target, &snapshot, None),
        Err(zup_plan::ExecutionPlanError::Contract(
            zup_plan::ContractError::Capability(_)
        ))
    ));
}

#[test]
fn search_path_membership_is_a_set_decision() {
    let target = sample_target();
    let mut snapshot = snapshot_happy();
    // An unrelated value does not stop the desired entry from being a member.
    snapshot.path_entries[0].search_path = SearchPath::new(vec![
        tpath(r"C:\Windows"),
        tpath(r"C:\PF\Acme\bin"),
        tpath(r"D:\tools"),
    ]);
    let plan = plan_execution(&target, &snapshot, None).unwrap();
    assert_eq!(plan.path_entries[0].kind, PathOperationKind::Present);
    assert!(plan.path_entries[0].present);
    assert!(!plan.path_entries[0].previously_owned);
    assert_eq!(plan.path_entries[0].kind, PathOperationKind::Present);

    // An empty search path is the same decision as one that lacks the entry.
    snapshot.path_entries[0].search_path = SearchPath::new(vec![tpath(r"C:\Windows")]);
    let plan = plan_execution(&target, &snapshot, None).unwrap();
    assert_eq!(plan.path_entries[0].kind, PathOperationKind::Add);
    assert!(!plan.path_entries[0].present);
    assert_eq!(plan.path_entries[0].kind, PathOperationKind::Add);
}

/// Membership is target-path identity rather than a string compare. Separators and case
/// are the adapter's normalization, and a parent directory under the same prefix is a
/// different path however it is spelled - a prefix match here would put the machine's
/// install directory on a search path the application never asked for.
#[test]
fn search_path_membership_uses_target_path_identity() {
    let target = sample_target();
    let mut snapshot = snapshot_happy();
    for spelling in [r"c:/pf/acme/bin/", r"C:\PF\ACME\BIN"] {
        snapshot.path_entries[0].search_path = SearchPath::new(vec![tpath(spelling)]);
        assert!(
            snapshot.path_entries[0]
                .search_path
                .contains(&target.path_entries[0].value),
            "{spelling} is the same path"
        );
        let plan = plan_execution(&target, &snapshot, None).unwrap();
        assert_eq!(plan.path_entries[0].kind, PathOperationKind::Present);
    }
    snapshot.path_entries[0].search_path = SearchPath::new(vec![tpath(r"C:\PF\Acme")]);
    assert!(
        !snapshot.path_entries[0]
            .search_path
            .contains(&target.path_entries[0].value),
        "a parent directory is not the entry"
    );
    let plan = plan_execution(&target, &snapshot, None).unwrap();
    assert_eq!(plan.path_entries[0].kind, PathOperationKind::Add);
}

#[test]
fn observation_headers_cannot_claim_another_resource_or_scope() {
    let target = sample_target();
    let mutations: [fn(&mut HostSnapshot); 4] = [
        |snapshot| snapshot.path_entries[0].desired = tpath(r"C:\Other"),
        |snapshot| snapshot.services[0].id = ServiceId::new("foreign").unwrap(),
        |snapshot| snapshot.protocols[0].scope = SelectedScope::User,
        |snapshot| snapshot.file_associations[0].extension = ".foreign".into(),
    ];
    for mutate in mutations {
        let mut snapshot = snapshot_happy();
        mutate(&mut snapshot);
        assert!(matches!(
            plan_execution(&target, &snapshot, None),
            Err(zup_plan::ExecutionPlanError::SnapshotResourceMismatch { .. })
        ));
    }
}
