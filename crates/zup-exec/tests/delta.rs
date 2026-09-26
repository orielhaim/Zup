//! Full v1 delta planning tests with a fake machine.

use zup_core::{
    LauncherLocation, Privilege, ProtocolScheme, RelativePath, ResourceKey, SelectedScope,
    ServiceId, ServiceStart, Sha256Digest, TargetTriple, hash_reader,
};
use zup_exec::{
    Conflict, FileAssociationOperationKind, FileOperationKind, HostSnapshot, LauncherOperationKind,
    ObservedExtensionState, ObservedFile, ObservedFileAssociation, ObservedFileAssociationState,
    ObservedFileState, ObservedLauncher, ObservedLauncherState, ObservedPathEntry,
    ObservedProtocol, ObservedProtocolState, ObservedService, ObservedServiceState,
    PathOperationKind, ProtocolOperationKind, SearchPath, ServiceOperationKind, plan_execution,
};
use zup_exec::{InstallLedger, LauncherState, OwnedResource, ProtocolState, ServiceState};
use zup_platform::{CommandSpec, TargetPath};

fn digest(bytes: &[u8]) -> Sha256Digest {
    hash_reader(bytes).unwrap().1
}

fn tpath(s: &str) -> TargetPath {
    TargetPath::new(TargetTriple::parse("x86_64-pc-windows-msvc").unwrap(), s).unwrap()
}

fn cmd(path: &str, args: &[&str]) -> CommandSpec {
    CommandSpec::new(tpath(path), args.iter().map(|s| (*s).to_owned()).collect())
}

fn sample_target() -> zup_platform::TargetPlan {
    zup_platform::TargetPlan {
        app: zup_core::App {
            id: zup_core::AppId::new("com.acme.acme").unwrap(),
            name: zup_core::NonEmptyString::new("Acme").unwrap(),
            version: "1.4.0".parse().unwrap(),
            publisher: None,
            main: None,
            description: None,
        },
        target: TargetTriple::parse("x86_64-pc-windows-msvc").unwrap(),
        scope: SelectedScope::Machine,
        install_directory: tpath(r"C:\PF\Acme"),
        selected_components: vec![],
        prerequisites: vec![],
        files: vec![zup_platform::TargetFile {
            key: ResourceKey::File {
                destination: r"C:\PF\Acme\Acme.exe".into(),
            },
            source_relative: RelativePath::new("Acme.exe").unwrap(),
            destination: tpath(r"C:\PF\Acme\Acme.exe"),
            size: 4,
            sha256: digest(b"data"),
            privilege: Privilege::System,
        }],
        launchers: vec![zup_platform::TargetLauncher {
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
        path_entries: vec![zup_platform::TargetPathEntry {
            key: ResourceKey::PathEntry {
                value: r"C:\PF\Acme\bin".into(),
            },
            value: tpath(r"C:\PF\Acme\bin"),
            scope: SelectedScope::Machine,
            privilege: Privilege::System,
        }],
        services: vec![zup_platform::TargetService {
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
        protocols: vec![zup_platform::TargetProtocol {
            key: ResourceKey::Protocol {
                scheme: ProtocolScheme::new("acme").unwrap(),
            },
            scheme: ProtocolScheme::new("acme").unwrap(),
            command: cmd(r"C:\PF\Acme\Acme.exe", &["--url", "%1"]),
            scope: SelectedScope::Machine,
            privilege: Privilege::System,
        }],
        file_associations: vec![zup_platform::TargetFileAssociation {
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
        summary: zup_platform::TargetPlanSummary {
            file_count: 1,
            install_bytes: 4,
            resource_count: 5,
            requires_authorization: true,
            selected_component_count: 2,
            prerequisite_count: 0,
            download_bytes: 0,
        },
    }
}

fn exact_service() -> ObservedServiceState {
    ObservedServiceState::Service {
        display_name: "Acme Agent".into(),
        command: cmd(r"C:\PF\Acme\acme-agent.exe", &[]),
        start: ServiceStart::Automatic,
        runtime_state: None,
    }
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
        display_name: "Old Agent".into(),
        command: target.services[0].command.clone(),
        start: ServiceStart::Manual,
        runtime_state: None,
    };
    let foreign = plan_execution(&target, &snapshot, None).unwrap();
    assert_eq!(foreign.launchers[0].kind, LauncherOperationKind::Conflict);
    assert_eq!(foreign.services[0].kind, ServiceOperationKind::Conflict);

    let mut ledger = InstallLedger::new(
        target.app.id.clone(),
        target.target.clone(),
        SelectedScope::Machine,
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
    assert_eq!(owned.launchers[0].kind, LauncherOperationKind::UpdateOwned);
    assert_eq!(owned.services[0].kind, ServiceOperationKind::UpdateOwned);
    snapshot.launchers[0].state = ObservedLauncherState::Absent;
    snapshot.services[0].state = ObservedServiceState::Absent;
    let drift = plan_execution(&target, &snapshot, Some(&ledger)).unwrap();
    assert_eq!(drift.launchers[0].kind, LauncherOperationKind::Drift);
    assert_eq!(drift.services[0].kind, ServiceOperationKind::Drift);

    target.launchers[0].arguments = vec!["--old".into()];
    target.services[0].display_name = Some(zup_core::NonEmptyString::new("Old Agent").unwrap());
    target.services[0].start = ServiceStart::Manual;
    snapshot.launchers[0].state = ObservedLauncherState::Launcher {
        target: target.launchers[0].target.clone(),
        arguments: vec!["--old".into()],
        working_directory: target.launchers[0].working_directory.clone(),
    };
    snapshot.services[0].state = ObservedServiceState::Service {
        display_name: "Old Agent".into(),
        command: target.services[0].command.clone(),
        start: ServiceStart::Manual,
        runtime_state: None,
    };
    let exact = plan_execution(&target, &snapshot, Some(&ledger)).unwrap();
    assert_eq!(exact.launchers[0].kind, LauncherOperationKind::NoOp);
    assert_eq!(exact.services[0].kind, ServiceOperationKind::NoOp);
}

#[test]
fn e2e_fake_machine_mixed_deltas() {
    let target = sample_target();
    let snapshot = snapshot_happy();
    let plan = plan_execution(&target, &snapshot, None).expect("plan");

    assert_eq!(plan.files[0].kind, FileOperationKind::NoOp);
    assert_eq!(plan.path_entries[0].kind, PathOperationKind::Add);
    assert_eq!(plan.launchers[0].kind, LauncherOperationKind::Create);
    assert_eq!(plan.services[0].kind, ServiceOperationKind::NoOp);
    assert_eq!(plan.protocols[0].kind, ProtocolOperationKind::Create);
    assert_eq!(
        plan.file_associations[0].kind,
        FileAssociationOperationKind::NoOp
    );

    assert_eq!(plan.summary.files_unchanged, 1);
    assert_eq!(plan.summary.path_entries_add, 1);
    assert_eq!(plan.summary.launchers_create, 1);
    assert_eq!(plan.summary.services_unchanged, 1);
    assert_eq!(plan.summary.protocols_create, 1);
    assert_eq!(plan.summary.file_associations_unchanged, 1);
}

#[test]
fn e2e_foreign_resources_conflict() {
    let target = sample_target();
    let mut snapshot = snapshot_happy();

    snapshot.protocols[0].state = ObservedProtocolState::Registration {
        command: cmd(r"C:\Other\handler.exe", &[]),
        url_protocol_marker: true,
    };
    snapshot.services[0].state = ObservedServiceState::Service {
        display_name: "Something Else".into(),
        command: cmd(r"C:\Other\svc.exe", &[]),
        start: ServiceStart::Manual,
        runtime_state: None,
    };
    snapshot.launchers[0].state = ObservedLauncherState::Launcher {
        target: tpath(r"C:\Other\app.exe"),
        arguments: vec![],
        working_directory: None,
    };

    let plan = plan_execution(&target, &snapshot, None).unwrap();
    assert_eq!(plan.protocols[0].kind, ProtocolOperationKind::Conflict);
    assert!(matches!(
        plan.protocols[0].conflict,
        Some(Conflict::ProtocolAlreadyRegistered { .. })
    ));
    assert_eq!(plan.services[0].kind, ServiceOperationKind::Conflict);
    assert!(matches!(
        plan.services[0].conflict,
        Some(Conflict::ServiceAlreadyExistsWithDifferentConfiguration { .. })
    ));
    assert_eq!(plan.launchers[0].kind, LauncherOperationKind::Conflict);
    assert!(matches!(
        plan.launchers[0].conflict,
        Some(Conflict::LauncherAlreadyOwnedByDifferentTarget { .. })
    ));
}

#[test]
fn files_replace_non_file_conflict() {
    let target = sample_target();
    let mut snapshot = snapshot_happy();

    snapshot.files[0].state = ObservedFileState::File {
        size: 4,
        sha256: digest(b"old!"),
    };
    let plan = plan_execution(&target, &snapshot, None).unwrap();
    assert_eq!(plan.files[0].kind, FileOperationKind::Conflict);

    let mut ledger = InstallLedger::new(
        target.app.id.clone(),
        target.target.clone(),
        SelectedScope::Machine,
    );
    ledger.resources.insert(
        target.files[0].key.clone(),
        OwnedResource::File {
            destination: target.files[0].destination.clone(),
            source_relative: target.files[0].source_relative.clone(),
            sha256: digest(b"old!"),
            size: 4,
            created_directories: vec![],
            privilege: Privilege::System,
        },
    );
    let plan = plan_execution(&target, &snapshot, Some(&ledger)).unwrap();
    assert_eq!(plan.files[0].kind, FileOperationKind::Replace);

    snapshot.files[0].state = ObservedFileState::File {
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
        FileAssociationOperationKind::Create
    );
    assert_eq!(
        plan.file_associations[0].association_kind,
        FileAssociationOperationKind::NoOp
    );
    assert_eq!(
        plan.file_associations[0].extension_kind,
        FileAssociationOperationKind::Create
    );

    // Extension exact, association missing.
    snapshot.file_associations[0].association_state = ObservedFileAssociationState::Absent;
    snapshot.file_associations[0].extension_state = ObservedExtensionState::Mapped {
        association_id: "Acme.Document".into(),
    };
    let plan = plan_execution(&target, &snapshot, None).unwrap();
    assert_eq!(
        plan.file_associations[0].kind,
        FileAssociationOperationKind::Create
    );
    assert_eq!(
        plan.file_associations[0].association_kind,
        FileAssociationOperationKind::Create
    );
    assert_eq!(
        plan.file_associations[0].extension_kind,
        FileAssociationOperationKind::NoOp
    );

    // Both absent → Create
    snapshot.file_associations[0].association_state = ObservedFileAssociationState::Absent;
    snapshot.file_associations[0].extension_state = ObservedExtensionState::Absent;
    let plan = plan_execution(&target, &snapshot, None).unwrap();
    assert_eq!(
        plan.file_associations[0].kind,
        FileAssociationOperationKind::Create
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
    assert_eq!(foreign.protocols[0].kind, ProtocolOperationKind::Conflict);

    let mut ledger = InstallLedger::new(target.app.id.clone(), target.target.clone(), target.scope);
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
        ProtocolOperationKind::UpdateOwned
    );

    snapshot.protocols[0].state = ObservedProtocolState::Registration {
        command: target.protocols[0].command.clone(),
        url_protocol_marker: true,
    };
    let drift = plan_execution(&target, &snapshot, Some(&ledger)).unwrap();
    assert_eq!(drift.protocols[0].kind, ProtocolOperationKind::Drift);
}

#[test]
fn removed_owned_path_is_drift() {
    let target = sample_target();
    let snapshot = snapshot_happy();
    let mut ledger = InstallLedger::new(target.app.id.clone(), target.target.clone(), target.scope);
    ledger.resources.insert(
        target.path_entries[0].key.clone(),
        OwnedResource::PathEntry {
            value: target.path_entries[0].value.clone(),
            value_type: "expand_sz".into(),
            privilege: Privilege::System,
        },
    );
    let plan = plan_execution(&target, &snapshot, Some(&ledger)).unwrap();
    assert_eq!(plan.path_entries[0].kind, PathOperationKind::Drift);
}

#[test]
fn ledger_target_mismatch_is_rejected() {
    let target = sample_target();
    let snapshot = snapshot_happy();
    let ledger = InstallLedger::new(
        target.app.id.clone(),
        TargetTriple::parse("arm64-pc-windows-msvc").unwrap(),
        target.scope,
    );
    let error = plan_execution(&target, &snapshot, Some(&ledger)).unwrap_err();
    assert!(matches!(
        error,
        zup_exec::ExecutionPlanError::LedgerMismatch
    ));
}

#[test]
fn determinism() {
    let target = sample_target();
    let snapshot = snapshot_happy();
    let a = plan_execution(&target, &snapshot, None).unwrap();
    let b = plan_execution(&target, &snapshot, None).unwrap();
    assert_eq!(a, b);
    assert_eq!(
        serde_json::to_string(&a).unwrap(),
        serde_json::to_string(&b).unwrap()
    );
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
        zup_exec::ExecutionPlanError::MissingSnapshotObservation { .. }
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
    assert_eq!(plan.summary.path_entries_present, 1);
    assert_eq!(plan.summary.path_entries_add, 0);

    // An empty search path is the same decision as one that lacks the entry.
    snapshot.path_entries[0].search_path = SearchPath::new(vec![tpath(r"C:\Windows")]);
    let plan = plan_execution(&target, &snapshot, None).unwrap();
    assert_eq!(plan.path_entries[0].kind, PathOperationKind::Add);
    assert!(!plan.path_entries[0].present);
    assert_eq!(plan.summary.path_entries_add, 1);
}

#[test]
fn search_path_membership_uses_target_path_identity() {
    let target = sample_target();
    let mut snapshot = snapshot_happy();
    // Separators and case are the adapter's normalization, not a string compare.
    snapshot.path_entries[0].search_path = SearchPath::new(vec![tpath(r"c:/pf/acme/bin/")]);
    assert!(
        snapshot.path_entries[0]
            .search_path
            .contains(&target.path_entries[0].value)
    );
    let plan = plan_execution(&target, &snapshot, None).unwrap();
    assert_eq!(plan.path_entries[0].kind, PathOperationKind::Present);

    // A different directory under the same prefix is not a member.
    snapshot.path_entries[0].search_path = SearchPath::new(vec![tpath(r"C:\PF\Acme")]);
    assert!(
        !snapshot.path_entries[0]
            .search_path
            .contains(&target.path_entries[0].value)
    );
}

#[test]
fn search_path_ownership_decisions_are_deterministic() {
    let target = sample_target();
    let mut snapshot = snapshot_happy();
    let mut ledger = InstallLedger::new(target.app.id.clone(), target.target.clone(), target.scope);
    ledger.resources.insert(
        target.path_entries[0].key.clone(),
        OwnedResource::PathEntry {
            value: target.path_entries[0].value.clone(),
            value_type: "expand_sz".into(),
            privilege: Privilege::System,
        },
    );
    // A reworded but equivalent entry is still owned and still present.
    snapshot.path_entries[0].search_path = SearchPath::new(vec![tpath(r"c:/pf/acme/bin")]);
    let first = plan_execution(&target, &snapshot, Some(&ledger)).unwrap();
    let second = plan_execution(&target, &snapshot, Some(&ledger)).unwrap();
    assert_eq!(first.path_entries[0].kind, PathOperationKind::Present);
    assert_eq!(first, second);

    // Removing it is drift, not an add, because the ledger owns it.
    snapshot.path_entries[0].search_path = SearchPath::default();
    let plan = plan_execution(&target, &snapshot, Some(&ledger)).unwrap();
    assert_eq!(plan.path_entries[0].kind, PathOperationKind::Drift);
    assert!(plan.path_entries[0].previously_owned);
    assert!(plan.path_entries[0].conflict.is_some());
    assert_eq!(plan.summary.path_entries_conflict, 1);
}

#[test]
fn operation_privilege_is_carried_verbatim_from_the_target_plan() {
    let mut target = sample_target();
    // A per-user scope that still owns a host-wide service, and a host-wide
    // scope that owns a per-user launcher. Both must survive planning as-is.
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
    target.summary.requires_authorization = true;

    let mut snapshot = snapshot_happy();
    snapshot.path_entries[0].scope = SelectedScope::User;
    snapshot.protocols[0].scope = SelectedScope::User;
    snapshot.file_associations[0].scope = SelectedScope::User;

    let plan = plan_execution(&target, &snapshot, None).unwrap();
    assert_eq!(plan.files[0].privilege, Privilege::User);
    assert_eq!(plan.launchers[0].privilege, Privilege::User);
    assert_eq!(plan.path_entries[0].privilege, Privilege::User);
    assert_eq!(plan.path_entries[0].scope, SelectedScope::User);
    assert_eq!(plan.protocols[0].privilege, Privilege::User);
    assert_eq!(plan.file_associations[0].privilege, Privilege::User);
    // The service is the only host-wide operation, and it is still System.
    assert_eq!(plan.services[0].privilege, Privilege::System);
    assert!(plan.summary.requires_authorization);
}

#[test]
fn serialized_field_names_name_authorization_not_elevation() {
    let plan = plan_execution(&sample_target(), &snapshot_happy(), None).unwrap();
    let json = serde_json::to_string(&plan).unwrap();
    assert!(
        !json.contains("elevation"),
        "execution plan leaked elevation: {json}"
    );
    assert!(json.contains("requires_authorization"));

    let summary = serde_json::to_value(zup_platform::TargetPlanSummary {
        file_count: 0,
        install_bytes: 0,
        resource_count: 0,
        requires_authorization: true,
        selected_component_count: 0,
        prerequisite_count: 0,
        download_bytes: 0,
    })
    .unwrap();
    assert_eq!(summary["requires_authorization"], serde_json::json!(true));
    assert!(summary.get("requires_elevation").is_none());
}

#[test]
fn privilege_serializes_as_authorization_words_only() {
    assert_eq!(serde_json::to_string(&Privilege::User).unwrap(), "\"user\"");
    assert_eq!(
        serde_json::to_string(&Privilege::System).unwrap(),
        "\"system\""
    );
    // No alias: the old word is not accepted on the way back in.
    assert!(serde_json::from_str::<Privilege>("\"machine\"").is_err());
    assert_eq!(
        serde_json::from_str::<Privilege>("\"system\"").unwrap(),
        Privilege::System
    );
}
