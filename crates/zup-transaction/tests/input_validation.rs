//! Transaction input validation reports the resource that failed.
//!
//! Every error names a `TransactionResource`, so a caller can tell an install
//! directory mismatch from a keyed-resource mismatch without matching on text.

mod common;

use zup_core::{
    BackendResourceId, ComponentId, Privilege, RelativePath, ResourceKey, SelectedScope,
};
use zup_platform::TargetPath;
use zup_transaction::{
    BackendOperation, FileDelta, FilePrecondition, FileRemoval, FileRemovalKind, FileWork,
    TransactionInput, TransactionInputError, TransactionResource,
};

use common::{digest, target, tpath};

/// A target whose paths cannot be mistaken for the Windows ones under test.
fn other_target() -> zup_core::TargetTriple {
    zup_core::TargetTriple::parse("aarch64-unknown-linux-gnu").unwrap()
}

fn file_key(name: &str) -> ResourceKey {
    ResourceKey::File {
        destination: format!(r"C:\PF\Acme\{name}"),
    }
}

fn file_work(key: ResourceKey, source: &str, destination: TargetPath) -> FileWork {
    let contents = b"payload";
    FileWork {
        key,
        source_relative: RelativePath::new(source).unwrap(),
        destination,
        precondition: FilePrecondition::Absent,
        expected_sha256: digest(contents),
        expected_size: contents.len() as u64,
        privilege: Privilege::User,
        delta: FileDelta::Create,
    }
}

fn removal(key: ResourceKey, destination: TargetPath) -> FileRemoval {
    FileRemoval {
        key,
        kind: FileRemovalKind::RemoveOwned,
        scope: SelectedScope::User,
        privilege: Privilege::User,
        destination,
        sha256: digest(b"old"),
        size: 3,
        created_directories: Vec::new(),
    }
}

#[test]
fn install_directory_mismatch_names_the_install_directory() {
    let mut input = TransactionInput::new(target());
    input.install_directory = Some(TargetPath::new(other_target(), "/opt/acme").unwrap());

    let error = input.validate().unwrap_err();

    assert_eq!(
        error,
        TransactionInputError::TargetMismatch {
            resource: TransactionResource::InstallDirectory,
        }
    );
    assert_eq!(
        error.to_string(),
        "transaction resource `install_directory` targets a different target"
    );
}

#[test]
fn file_destination_mismatch_carries_the_resource_key() {
    let key = file_key("a.exe");
    let mut input = TransactionInput::new(target());
    input.files = vec![file_work(
        key.clone(),
        "a.exe",
        TargetPath::new(other_target(), "/opt/acme/a.exe").unwrap(),
    )];

    let error = input.validate().unwrap_err();

    assert_eq!(
        error,
        TransactionInputError::TargetMismatch {
            resource: TransactionResource::key(&key),
        }
    );
    assert!(
        error
            .to_string()
            .contains(r#"File { destination: "C:\\PF\\Acme\\a.exe" }"#),
        "error: {error}"
    );
}

#[test]
fn created_directory_mismatch_names_the_removal_key() {
    let key = file_key("old.exe");
    let mut input = TransactionInput::new(target());
    input.retired_keys.push(key.clone());
    let mut removed = removal(key.clone(), tpath(r"C:\PF\Acme\old.exe"));
    removed.created_directories = vec![TargetPath::new(other_target(), "/opt/acme").unwrap()];
    input.removals = vec![removed];

    let error = input.validate().unwrap_err();

    assert_eq!(
        error,
        TransactionInputError::TargetMismatch {
            resource: TransactionResource::key(&key),
        }
    );
}

#[test]
fn duplicate_component_is_typed_rather_than_stringified() {
    let mut input = TransactionInput::new(target());
    input.selected_components = vec![ComponentId::new("core").unwrap(); 2];

    let error = input.validate().unwrap_err();

    assert_eq!(
        error,
        TransactionInputError::DuplicateComponent {
            component: TransactionResource::Component(ComponentId::new("core").unwrap()),
        }
    );
    assert_eq!(error.to_string(), "duplicate selected component `core`");
}

#[test]
fn duplicate_retired_key_names_the_key() {
    let key = file_key("a.exe");
    let mut input = TransactionInput::new(target());
    input.retired_keys = vec![key.clone(), key.clone()];

    let error = input.validate().unwrap_err();

    assert_eq!(
        error,
        TransactionInputError::DuplicateKey {
            resource: TransactionResource::key(&key),
        }
    );
    assert!(
        error
            .to_string()
            .starts_with("duplicate transaction resource key `"),
        "error: {error}"
    );
}

#[test]
fn unretired_removal_names_the_removal_key() {
    let key = file_key("old.exe");
    let mut input = TransactionInput::new(target());
    input.removals = vec![removal(key.clone(), tpath(r"C:\PF\Acme\old.exe"))];

    let error = input.validate().unwrap_err();

    assert_eq!(
        error,
        TransactionInputError::UnretiredRemoval {
            resource: TransactionResource::key(&key),
        }
    );
    assert!(error.to_string().starts_with("removal `"), "error: {error}");
}

#[test]
fn unknown_dependency_names_both_resources() {
    let id = BackendResourceId::new("fake.agent").unwrap();
    let unknown = file_key("missing.exe");
    let mut input = TransactionInput::new(target());
    input.backend_operations = vec![
        BackendOperation::apply(
            ResourceKey::Backend { id: id.clone() },
            id.clone(),
            Privilege::System,
            b"opaque".to_vec(),
        )
        .with_dependencies(vec![unknown.clone()]),
    ];

    let error = input.validate().unwrap_err();

    assert_eq!(
        error,
        TransactionInputError::UnknownDependency {
            resource: TransactionResource::key(&ResourceKey::Backend { id }),
            dependency: TransactionResource::key(&unknown),
        }
    );
    assert!(
        error
            .to_string()
            .starts_with("backend operation `Backend { id: BackendResourceId("),
        "error: {error}"
    );
}

#[test]
fn duplicate_dependency_names_the_repeated_key() {
    let id = BackendResourceId::new("fake.agent").unwrap();
    let file = file_key("a.exe");
    let mut input = TransactionInput::new(target());
    input.files = vec![file_work(file.clone(), "a.exe", tpath(r"C:\PF\Acme\a.exe"))];
    input.backend_operations = vec![
        BackendOperation::apply(
            ResourceKey::Backend { id },
            BackendResourceId::new("fake.agent").unwrap(),
            Privilege::System,
            b"opaque".to_vec(),
        )
        .with_dependencies(vec![file.clone(), file.clone()]),
    ];

    let error = input.validate().unwrap_err();

    assert_eq!(
        error,
        TransactionInputError::DuplicateDependency {
            resource: TransactionResource::key(&ResourceKey::Backend {
                id: BackendResourceId::new("fake.agent").unwrap()
            }),
            dependency: TransactionResource::key(&file),
        }
    );
}

#[test]
fn backend_identity_mismatch_names_the_declared_key() {
    let declared = file_key("a.exe");
    let mut input = TransactionInput::new(target());
    input.backend_operations = vec![BackendOperation::apply(
        declared.clone(),
        BackendResourceId::new("fake.agent").unwrap(),
        Privilege::System,
        b"opaque".to_vec(),
    )];

    let error = input.validate().unwrap_err();

    assert_eq!(
        error,
        TransactionInputError::BackendIdentityMismatch {
            resource: TransactionResource::key(&declared),
        }
    );
    assert!(
        error
            .to_string()
            .ends_with("does not use its backend resource identity"),
        "error: {error}"
    );
}

#[test]
fn self_dependency_names_the_operation_key() {
    let id = BackendResourceId::new("fake.agent").unwrap();
    let key = ResourceKey::Backend { id: id.clone() };
    let mut input = TransactionInput::new(target());
    input.backend_operations = vec![
        BackendOperation::apply(key.clone(), id, Privilege::System, b"opaque".to_vec())
            .with_dependencies(vec![key.clone()]),
    ];

    let error = input.validate().unwrap_err();

    assert_eq!(
        error,
        TransactionInputError::SelfDependency {
            resource: TransactionResource::key(&key),
        }
    );
    assert!(
        error.to_string().ends_with("depends on itself"),
        "error: {error}"
    );
}

#[test]
fn install_directory_renders_the_same_text_as_the_old_magic_string() {
    assert_eq!(
        TransactionResource::InstallDirectory.to_string(),
        "install_directory"
    );
}
