//! Full v1 delta planning tests with a fake machine.

use std::path::PathBuf;

use zup_core::{
    Privilege, ProtocolScheme, RelativePath, ResourceKey, SelectedScope, ServiceId, ServiceStart,
    Sha256Digest, ShortcutLocation, hash_reader,
};
use zup_exec::{
    Conflict, FileOperationKind, FileTypeOperationKind, MachineSnapshot, ObservedExtensionState,
    ObservedFile, ObservedFileState, ObservedFileType, ObservedPathEntry, ObservedProgIdState,
    ObservedProtocol, ObservedProtocolState, ObservedService, ObservedServiceState,
    ObservedShortcut, ObservedShortcutState, PathEntryState, PathOperationKind,
    ProtocolOperationKind, ServiceOperationKind, ShortcutOperationKind, plan_execution,
};
use zup_exec::{InstallLedger, OwnedResource, ProtocolState, ServiceState, ShortcutState};
use zup_platform::{CommandSpec, TargetPath};

fn digest(bytes: &[u8]) -> Sha256Digest {
    hash_reader(bytes).unwrap().1
}

fn tpath(s: &str) -> TargetPath {
    TargetPath::new(PathBuf::from(s)).unwrap()
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
            privilege: Privilege::Machine,
        }],
        shortcuts: vec![zup_platform::TargetShortcut {
            key: ResourceKey::Shortcut {
                location: ShortcutLocation::StartMenu,
                name: "Acme".into(),
            },
            location: ShortcutLocation::StartMenu,
            name: zup_core::NonEmptyString::new("Acme").unwrap(),
            link_path: tpath(r"C:\ProgramData\Microsoft\Windows\Start Menu\Programs\Acme.lnk"),
            target: tpath(r"C:\PF\Acme\Acme.exe"),
            arguments: vec![],
            working_directory: Some(tpath(r"C:\PF\Acme")),
            privilege: Privilege::Machine,
        }],
        path_entries: vec![zup_platform::TargetPathEntry {
            key: ResourceKey::PathEntry {
                value: r"C:\PF\Acme\bin".into(),
            },
            value: tpath(r"C:\PF\Acme\bin"),
            scope: SelectedScope::Machine,
            privilege: Privilege::Machine,
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
            privilege: Privilege::Machine,
        }],
        protocols: vec![zup_platform::TargetProtocol {
            key: ResourceKey::Protocol {
                scheme: ProtocolScheme::new("acme").unwrap(),
            },
            scheme: ProtocolScheme::new("acme").unwrap(),
            command: cmd(r"C:\PF\Acme\Acme.exe", &["--url", "%1"]),
            scope: SelectedScope::Machine,
            privilege: Privilege::Machine,
        }],
        file_types: vec![zup_platform::TargetFileType {
            key: ResourceKey::FileType {
                id: zup_core::FileTypeId::new("Acme.Document").unwrap(),
            },
            extension: zup_core::FileExtension::new(".acme").unwrap(),
            id: zup_core::FileTypeId::new("Acme.Document").unwrap(),
            description: Some("Acme Document".into()),
            command: cmd(r"C:\PF\Acme\Acme.exe", &[]),
            scope: SelectedScope::Machine,
            privilege: Privilege::Machine,
        }],
        summary: zup_platform::TargetPlanSummary {
            file_count: 1,
            install_bytes: 4,
            resource_count: 5,
            requires_elevation: true,
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

fn exact_file_type() -> ObservedFileType {
    ObservedFileType {
        key: ResourceKey::FileType {
            id: zup_core::FileTypeId::new("Acme.Document").unwrap(),
        },
        extension: ".acme".into(),
        id: "Acme.Document".into(),
        scope: SelectedScope::Machine,
        id_state: ObservedProgIdState::Registration {
            description: Some("Acme Document".into()),
            command: cmd(r"C:\PF\Acme\Acme.exe", &[]),
        },
        extension_state: ObservedExtensionState::Mapped {
            prog_id: "Acme.Document".into(),
        },
    }
}

fn snapshot_happy() -> MachineSnapshot {
    let target = sample_target();
    MachineSnapshot {
        files: vec![ObservedFile {
            key: target.files[0].key.clone(),
            path: target.files[0].destination.clone(),
            state: ObservedFileState::File {
                size: 4,
                sha256: digest(b"data"),
            },
        }],
        shortcuts: vec![ObservedShortcut {
            key: target.shortcuts[0].key.clone(),
            link_path: target.shortcuts[0].link_path.clone(),
            state: ObservedShortcutState::Absent,
        }],
        path_entries: vec![ObservedPathEntry {
            key: target.path_entries[0].key.clone(),
            desired: target.path_entries[0].value.clone(),
            scope: SelectedScope::Machine,
            state: PathEntryState::Absent,
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
        file_types: vec![exact_file_type()],
    }
}

#[test]
fn shortcut_and_service_ownership_decisions() {
    let mut target = sample_target();
    let mut snapshot = snapshot_happy();
    let original_shortcut = ShortcutState::Link {
        target: target.shortcuts[0].target.clone(),
        arguments: vec!["--old".into()],
        working_directory: target.shortcuts[0].working_directory.clone(),
    };
    let original_service = ServiceState::Registration {
        display_name: "Old Agent".into(),
        command: target.services[0].command.clone(),
        start: ServiceStart::Manual,
    };
    snapshot.shortcuts[0].state = ObservedShortcutState::Shortcut {
        target: target.shortcuts[0].target.clone(),
        arguments: vec!["--old".into()],
        working_directory: target.shortcuts[0].working_directory.clone(),
    };
    snapshot.services[0].state = ObservedServiceState::Service {
        display_name: "Old Agent".into(),
        command: target.services[0].command.clone(),
        start: ServiceStart::Manual,
        runtime_state: None,
    };
    let foreign = plan_execution(&target, &snapshot, None).unwrap();
    assert_eq!(foreign.shortcuts[0].kind, ShortcutOperationKind::Conflict);
    assert_eq!(foreign.services[0].kind, ServiceOperationKind::Conflict);

    let mut ledger = InstallLedger::new(target.app.id.clone(), SelectedScope::Machine);
    ledger.resources.insert(
        target.shortcuts[0].key.clone(),
        OwnedResource::Shortcut {
            link_path: target.shortcuts[0].link_path.clone(),
            previous: ShortcutState::Absent,
            installed: original_shortcut,
        },
    );
    ledger.resources.insert(
        target.services[0].key.clone(),
        OwnedResource::Service {
            name: target.services[0].name.to_string(),
            previous: ServiceState::Absent,
            installed: original_service,
        },
    );
    let owned = plan_execution(&target, &snapshot, Some(&ledger)).unwrap();
    assert_eq!(owned.shortcuts[0].kind, ShortcutOperationKind::UpdateOwned);
    assert_eq!(owned.services[0].kind, ServiceOperationKind::UpdateOwned);
    snapshot.shortcuts[0].state = ObservedShortcutState::Absent;
    snapshot.services[0].state = ObservedServiceState::Absent;
    let drift = plan_execution(&target, &snapshot, Some(&ledger)).unwrap();
    assert_eq!(drift.shortcuts[0].kind, ShortcutOperationKind::Drift);
    assert_eq!(drift.services[0].kind, ServiceOperationKind::Drift);

    target.shortcuts[0].arguments = vec!["--old".into()];
    target.services[0].display_name = Some(zup_core::NonEmptyString::new("Old Agent").unwrap());
    target.services[0].start = ServiceStart::Manual;
    snapshot.shortcuts[0].state = ObservedShortcutState::Shortcut {
        target: target.shortcuts[0].target.clone(),
        arguments: vec!["--old".into()],
        working_directory: target.shortcuts[0].working_directory.clone(),
    };
    snapshot.services[0].state = ObservedServiceState::Service {
        display_name: "Old Agent".into(),
        command: target.services[0].command.clone(),
        start: ServiceStart::Manual,
        runtime_state: None,
    };
    let exact = plan_execution(&target, &snapshot, Some(&ledger)).unwrap();
    assert_eq!(exact.shortcuts[0].kind, ShortcutOperationKind::NoOp);
    assert_eq!(exact.services[0].kind, ServiceOperationKind::NoOp);
}

#[test]
fn e2e_fake_machine_mixed_deltas() {
    let target = sample_target();
    let snapshot = snapshot_happy();
    let plan = plan_execution(&target, &snapshot, None).expect("plan");

    assert_eq!(plan.files[0].kind, FileOperationKind::NoOp);
    assert_eq!(plan.path_entries[0].kind, PathOperationKind::Add);
    assert_eq!(plan.shortcuts[0].kind, ShortcutOperationKind::Create);
    assert_eq!(plan.services[0].kind, ServiceOperationKind::NoOp);
    assert_eq!(plan.protocols[0].kind, ProtocolOperationKind::Create);
    assert_eq!(plan.file_types[0].kind, FileTypeOperationKind::NoOp);

    assert_eq!(plan.summary.files_unchanged, 1);
    assert_eq!(plan.summary.path_entries_add, 1);
    assert_eq!(plan.summary.shortcuts_create, 1);
    assert_eq!(plan.summary.services_unchanged, 1);
    assert_eq!(plan.summary.protocols_create, 1);
    assert_eq!(plan.summary.file_types_unchanged, 1);
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
    snapshot.shortcuts[0].state = ObservedShortcutState::Shortcut {
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
    assert_eq!(plan.shortcuts[0].kind, ShortcutOperationKind::Conflict);
    assert!(matches!(
        plan.shortcuts[0].conflict,
        Some(Conflict::ShortcutAlreadyOwnedByDifferentTarget { .. })
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

    let mut ledger = InstallLedger::new(target.app.id.clone(), SelectedScope::Machine);
    ledger.resources.insert(
        target.files[0].key.clone(),
        OwnedResource::File {
            destination: target.files[0].destination.clone(),
            source_relative: target.files[0].source_relative.clone(),
            sha256: digest(b"old!"),
            size: 4,
            created_directories: vec![],
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
fn file_type_parts_are_planned_independently() {
    let target = sample_target();
    let mut snapshot = snapshot_happy();

    // ProgID exact, extension missing.
    snapshot.file_types[0].extension_state = ObservedExtensionState::Absent;
    let plan = plan_execution(&target, &snapshot, None).unwrap();
    assert_eq!(plan.file_types[0].kind, FileTypeOperationKind::Create);
    assert_eq!(plan.file_types[0].prog_id_kind, FileTypeOperationKind::NoOp);
    assert_eq!(
        plan.file_types[0].extension_kind,
        FileTypeOperationKind::Create
    );

    // Extension exact, ProgID missing.
    snapshot.file_types[0].id_state = ObservedProgIdState::Absent;
    snapshot.file_types[0].extension_state = ObservedExtensionState::Mapped {
        prog_id: "Acme.Document".into(),
    };
    let plan = plan_execution(&target, &snapshot, None).unwrap();
    assert_eq!(plan.file_types[0].kind, FileTypeOperationKind::Create);
    assert_eq!(
        plan.file_types[0].prog_id_kind,
        FileTypeOperationKind::Create
    );
    assert_eq!(
        plan.file_types[0].extension_kind,
        FileTypeOperationKind::NoOp
    );

    // Both absent → Create
    snapshot.file_types[0].id_state = ObservedProgIdState::Absent;
    snapshot.file_types[0].extension_state = ObservedExtensionState::Absent;
    let plan = plan_execution(&target, &snapshot, None).unwrap();
    assert_eq!(plan.file_types[0].kind, FileTypeOperationKind::Create);
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

    let mut ledger = InstallLedger::new(target.app.id.clone(), target.scope);
    ledger.resources.insert(
        target.protocols[0].key.clone(),
        OwnedResource::Protocol {
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
    let mut ledger = InstallLedger::new(target.app.id.clone(), target.scope);
    ledger.resources.insert(
        target.path_entries[0].key.clone(),
        OwnedResource::PathEntry {
            value: target.path_entries[0].value.clone(),
            value_type: "expand_sz".into(),
        },
    );
    let plan = plan_execution(&target, &snapshot, Some(&ledger)).unwrap();
    assert_eq!(plan.path_entries[0].kind, PathOperationKind::Drift);
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
    let snapshot = MachineSnapshot {
        files: snapshot_happy().files,
        ..Default::default()
    };
    let err = plan_execution(&target, &snapshot, None).unwrap_err();
    assert!(matches!(
        err,
        zup_exec::ExecutionPlanError::MissingSnapshotObservation { .. }
    ));
}
