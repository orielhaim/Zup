use zup_core::{
    App, AppId, InstallationId, NonEmptyString, PayloadId, PayloadPlacement, Privilege,
    RelativePath, ResourceId, ResourceKey, SelectedScope, Target, TargetPath, hash_bytes,
};
use zup_plan::{
    DesiredFile, DesiredState, FileOperationKind, HostSnapshot, ObservedFile, ObservedFileState,
    plan_execution,
};

#[test]
fn matching_foreign_fixed_payload_remains_a_conflict() {
    let target = Target::WINDOWS_X64;
    let directory = TargetPath::new(target, r"C:\application").unwrap();
    let path = directory.join("application.exe").unwrap();
    let key = ResourceKey::File {
        id: zup_core::FileResourceId::Payload {
            id: PayloadId::TreeMember {
                declaration: ResourceId::new("application").unwrap(),
                member: RelativePath::new("application.exe").unwrap(),
            },
        },
    };
    let desired = DesiredState {
        generation_readers: Default::default(),
        data: Vec::new(),
        requirements: (),
        maintenance: None,
        installation: InstallationId::new_v7(),
        generation: None,
        app: App {
            id: AppId::new("example.fixed-ownership").unwrap(),
            name: NonEmptyString::new("Fixed application").unwrap(),
            version: "1.0.0".parse().unwrap(),
            publisher: None,
            main: None,
            description: None,
        },
        target,
        scope: SelectedScope::User,
        install_directory: directory,
        selected_components: vec![],
        prerequisites: vec![],
        files: vec![DesiredFile {
            key: key.clone(),
            source_relative: RelativePath::new("application.exe").unwrap(),
            placement: PayloadPlacement::Fixed {
                replacement: zup_core::FixedReplacement::Atomic,
                path: path.clone(),
            },
            size: 4,
            sha256: hash_bytes(b"same"),
            privilege: Privilege::User,
            executable: true,
        }],
        launchers: vec![],
        path_entries: vec![],
        services: vec![],
        protocols: vec![],
        file_associations: vec![],
        preset: None,
    }
    .bind_fixed()
    .expect("fixed deployment requirements");
    let snapshot = HostSnapshot {
        files: vec![ObservedFile {
            key: key.clone(),
            path,
            state: ObservedFileState::File {
                object: zup_core::NativeObjectIdentity::Windows {
                    volume: 1,
                    file: [1; 16],
                },
                size: 4,
                sha256: hash_bytes(b"same"),
                executable: Some(true),
            },
        }],
        ..Default::default()
    };
    let plan = plan_execution(&desired, &snapshot, None).unwrap();
    let decision = plan.files.iter().find(|file| file.key == key).unwrap();
    assert_eq!(decision.kind, FileOperationKind::Conflict);
}
