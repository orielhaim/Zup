//! Shared fixtures for transaction tests.

#![allow(dead_code)]

use zup_core::{RelativePath, ResourceKey, SelectedScope, ServiceId, Sha256Digest, hash_reader};
use zup_exec::{
    ExecutionPlan, ExecutionSummary, FileOperation, FileOperationKind, FilePrecondition,
    ShortcutOperation, ShortcutOperationKind,
};
use zup_platform::TargetPath;
use zup_transaction::{TransactionPlan, compile_transaction};

pub fn tpath(s: &str) -> TargetPath {
    TargetPath::new(std::path::PathBuf::from(s)).unwrap()
}

pub fn digest(b: &[u8]) -> Sha256Digest {
    hash_reader(b).unwrap().1
}

/// Two file creates and one shortcut create.
pub fn sample_execution() -> ExecutionPlan {
    ExecutionPlan {
        selected_components: vec![],
        uninstall: false,
        removals: vec![],
        files: vec![
            FileOperation {
                key: ResourceKey::File {
                    destination: r"C:\PF\Acme\a.exe".into(),
                },
                kind: FileOperationKind::Create,
                destination: tpath(r"C:\PF\Acme\a.exe"),
                source_relative: RelativePath::new("a.exe").unwrap(),
                precondition: FilePrecondition::Absent,
                expected_sha256: digest(b"a"),
                expected_size: 1,
                conflict: None,
            },
            FileOperation {
                key: ResourceKey::File {
                    destination: r"C:\PF\Acme\b.dll".into(),
                },
                kind: FileOperationKind::Create,
                destination: tpath(r"C:\PF\Acme\b.dll"),
                source_relative: RelativePath::new("b.dll").unwrap(),
                precondition: FilePrecondition::Absent,
                expected_sha256: digest(b"b"),
                expected_size: 1,
                conflict: None,
            },
        ],
        shortcuts: vec![ShortcutOperation {
            key: ResourceKey::Shortcut {
                location: zup_core::ShortcutLocation::StartMenu,
                name: "Acme".into(),
            },
            kind: ShortcutOperationKind::Create,
            link_path: tpath(r"C:\Programs\Acme.lnk"),
            target: tpath(r"C:\PF\Acme\a.exe"),
            arguments: vec![],
            working_directory: None,
            previous: zup_exec::ObservedShortcutState::Absent,
            conflict: None,
        }],
        path_entries: vec![],
        services: vec![],
        protocols: vec![],
        file_types: vec![],
        uninstall_entries: vec![],
        summary: ExecutionSummary {
            files_create: 2,
            shortcuts_create: 1,
            requires_elevation: true,
            ..Default::default()
        },
    }
}

/// Linear A → B → C file creates for rollback-order tests.
pub fn chain_execution() -> ExecutionPlan {
    let mut plan = sample_execution();
    plan.files.push(FileOperation {
        key: ResourceKey::File {
            destination: r"C:\PF\Acme\c.txt".into(),
        },
        kind: FileOperationKind::Create,
        destination: tpath(r"C:\PF\Acme\c.txt"),
        source_relative: RelativePath::new("c.txt").unwrap(),
        precondition: FilePrecondition::Absent,
        expected_sha256: digest(b"c"),
        expected_size: 1,
        conflict: None,
    });
    plan.shortcuts.clear();
    plan
}

pub fn sample_plan() -> TransactionPlan {
    compile_transaction(&sample_execution()).unwrap()
}

pub fn sample_app_id() -> zup_core::AppId {
    zup_core::AppId::new("com.acme.acme").unwrap()
}

pub fn sample_version() -> semver::Version {
    "1.4.0".parse().unwrap()
}

// Silence unused import in some tests.
#[allow(unused)]
fn _unused(_: SelectedScope, _: ServiceId) {}
