#![allow(dead_code)]

use zup_core::{
    BackendResourceId, Privilege, RelativePath, ResourceKey, SelectedScope, Sha256Digest,
    TargetTriple, hash_reader,
};
use zup_platform::TargetPath;
use zup_transaction::{
    BackendOperation, FileDelta, FilePrecondition, FileRemoval, FileRemovalKind, FileWork,
    TransactionInput, TransactionPlan, compile_transaction,
};

pub fn target() -> TargetTriple {
    TargetTriple::parse("x86_64-pc-windows-msvc").unwrap()
}

pub fn tpath(value: &str) -> TargetPath {
    TargetPath::new(target(), value).unwrap()
}

pub fn digest(bytes: &[u8]) -> Sha256Digest {
    hash_reader(bytes).unwrap().1
}

fn file(name: &str, contents: &[u8]) -> FileWork {
    FileWork {
        key: ResourceKey::File {
            destination: format!(r"C:\PF\Acme\{name}"),
        },
        source_relative: RelativePath::new(name).unwrap(),
        destination: tpath(&format!(r"C:\PF\Acme\{name}")),
        precondition: FilePrecondition::Absent,
        expected_sha256: digest(contents),
        expected_size: contents.len() as u64,
        privilege: Privilege::User,
        delta: FileDelta::Create,
    }
}

fn backend() -> BackendOperation {
    let id = BackendResourceId::new("fake.backend").unwrap();
    BackendOperation::apply(
        ResourceKey::Backend { id: id.clone() },
        id,
        Privilege::System,
        b"opaque-backend-payload".to_vec(),
    )
}

pub fn sample_input() -> TransactionInput {
    let mut input = TransactionInput::new(target());
    input.files = vec![file("a.exe", b"a"), file("b.dll", b"b")];
    input.backend_operations = vec![backend()];
    input
}

pub fn chain_input() -> TransactionInput {
    let mut input = sample_input();
    input.files.push(file("c.txt", b"c"));
    input.backend_operations.clear();
    input
}

pub fn removal_input() -> TransactionInput {
    let mut input = TransactionInput::new(target());
    let key = ResourceKey::File {
        destination: r"C:\PF\Acme\old.exe".into(),
    };
    input.retired_keys.push(key.clone());
    input.removals.push(FileRemoval {
        key,
        kind: FileRemovalKind::RemoveOwned,
        scope: SelectedScope::User,
        privilege: Privilege::User,
        destination: tpath(r"C:\PF\Acme\old.exe"),
        sha256: digest(b"old"),
        size: 3,
        created_directories: Vec::new(),
    });
    input
}

pub fn sample_plan() -> TransactionPlan {
    compile_transaction(&sample_input()).unwrap()
}

pub fn sample_app_id() -> zup_core::AppId {
    zup_core::AppId::new("com.acme.acme").unwrap()
}

pub fn sample_version() -> semver::Version {
    "1.4.0".parse().unwrap()
}
