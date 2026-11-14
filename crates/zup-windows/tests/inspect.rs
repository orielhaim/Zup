#![cfg(windows)]

use zup_core::{
    App, AppId, InstallationId, NonEmptyString, PayloadId, PayloadPlacement, Privilege,
    RelativePath, ResourceId, ResourceKey, SelectedScope, Target, TargetPath, hash_bytes,
};
use zup_plan::{DesiredFile, DesiredState, ObservedFileState};

#[test]
fn equal_content_replacement_has_a_distinct_native_identity() {
    use std::os::windows::fs::OpenOptionsExt;
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("application.bin");
    std::fs::write(&path, b"payload").unwrap();
    let directory = TargetPath::new(Target::WINDOWS_X64, root.path().to_string_lossy()).unwrap();
    let desired = DesiredState {
        generation_readers: Default::default(),
        data: Vec::new(),
        requirements: (),
        maintenance: None,
        installation: InstallationId::new_v7(),
        generation: None,
        app: App {
            id: AppId::new("example.native-observation").unwrap(),
            name: NonEmptyString::new("Native observation").unwrap(),
            version: "1.0.0".parse().unwrap(),
            publisher: None,
            main: None,
            description: None,
        },
        target: Target::WINDOWS_X64,
        scope: SelectedScope::User,
        install_directory: directory.clone(),
        selected_components: vec![],
        prerequisites: vec![],
        files: vec![DesiredFile {
            key: ResourceKey::File {
                id: zup_core::FileResourceId::Payload {
                    id: PayloadId::TreeMember {
                        declaration: ResourceId::new("application").unwrap(),
                        member: RelativePath::new("application.bin").unwrap(),
                    },
                },
            },
            source_relative: RelativePath::new("application.bin").unwrap(),
            placement: PayloadPlacement::Fixed {
                replacement: zup_core::FixedReplacement::Atomic,
                path: directory.join("application.bin").unwrap(),
            },
            size: 7,
            sha256: hash_bytes(b"payload"),
            privilege: Privilege::User,
            executable: false,
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
    let observe = || {
        let files = zup_windows::inspect_files(&desired).unwrap();
        let ObservedFileState::File {
            object,
            sha256,
            size,
            ..
        } = files[0].state
        else {
            panic!("expected a regular file");
        };
        assert_eq!((size, sha256), (7, hash_bytes(b"payload")));
        object
    };
    let first = observe();
    assert_eq!(first, observe());
    {
        let _writer = std::fs::OpenOptions::new()
            .write(true)
            .share_mode(7)
            .open(&path)
            .unwrap();
        assert!(matches!(
            zup_windows::inspect_files(&desired),
            Err(zup_windows::InspectError::TargetInUse { .. })
        ));
    }
    std::fs::rename(&path, root.path().join("retained.bin")).unwrap();
    std::fs::write(&path, b"payload").unwrap();
    assert_ne!(first, observe());
}
