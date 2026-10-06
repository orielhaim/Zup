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
        executable: false,
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

/// A target mismatch anywhere in the operation set is reported against the
/// resource that actually carries the bad path, not against the operation.
#[test]
fn target_mismatch_carries_the_owning_resource_key() {
    let staged = file_key("a.exe");
    let mut staging_input = TransactionInput::new(target());
    staging_input.files = vec![file_work(
        staged.clone(),
        "a.exe",
        TargetPath::new(other_target(), "/opt/acme/a.exe").unwrap(),
    )];
    assert_eq!(
        staging_input.validate().unwrap_err(),
        TransactionInputError::TargetMismatch {
            resource: TransactionResource::key(&staged),
        }
    );

    let removed = file_key("old.exe");
    let mut removal_input = TransactionInput::new(target());
    removal_input.retired_keys.push(removed.clone());
    let mut file = removal(removed.clone(), tpath(r"C:\PF\Acme\old.exe"));
    file.created_directories = vec![TargetPath::new(other_target(), "/opt/acme").unwrap()];
    removal_input.removals = vec![file];
    assert_eq!(
        removal_input.validate().unwrap_err(),
        TransactionInputError::TargetMismatch {
            resource: TransactionResource::key(&removed),
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

    assert_eq!(
        input.validate().unwrap_err(),
        TransactionInputError::DuplicateKey {
            resource: TransactionResource::key(&key),
        }
    );
}

#[test]
fn unretired_removal_names_the_removal_key() {
    let key = file_key("old.exe");
    let mut input = TransactionInput::new(target());
    input.removals = vec![removal(key.clone(), tpath(r"C:\PF\Acme\old.exe"))];

    assert_eq!(
        input.validate().unwrap_err(),
        TransactionInputError::UnretiredRemoval {
            resource: TransactionResource::key(&key),
        }
    );
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

    assert_eq!(
        input.validate().unwrap_err(),
        TransactionInputError::UnknownDependency {
            resource: TransactionResource::key(&ResourceKey::Backend { id }),
            dependency: TransactionResource::key(&unknown),
        }
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
            ResourceKey::Backend { id: id.clone() },
            id.clone(),
            Privilege::System,
            b"opaque".to_vec(),
        )
        .with_dependencies(vec![file.clone(), file.clone()]),
    ];

    assert_eq!(
        input.validate().unwrap_err(),
        TransactionInputError::DuplicateDependency {
            resource: TransactionResource::key(&ResourceKey::Backend { id }),
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

    assert_eq!(
        input.validate().unwrap_err(),
        TransactionInputError::BackendIdentityMismatch {
            resource: TransactionResource::key(&declared),
        }
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

    assert_eq!(
        input.validate().unwrap_err(),
        TransactionInputError::SelfDependency {
            resource: TransactionResource::key(&key),
        }
    );
}

/// A retired removal is a valid backend dependency: regenerating derived
/// state from the removed world is exactly what a refresh does.
#[test]
fn a_backend_apply_may_follow_a_removal() {
    use zup_transaction::{OperationId, compile_transaction};

    let id = BackendResourceId::new("fake.refresh").unwrap();
    let backend = ResourceKey::Backend { id: id.clone() };
    let retired = file_key("old.exe");
    let mut input = TransactionInput::new(target());
    input.retired_keys = vec![retired.clone()];
    input.removals = vec![removal(retired.clone(), tpath(r"C:\PF\Acme\old.exe"))];
    input.backend_operations = vec![
        BackendOperation::apply(backend.clone(), id, Privilege::User, b"refresh".to_vec())
            .with_dependencies(vec![retired.clone()]),
    ];
    input.validate().expect("a removal is a valid dependency");

    let plan = compile_transaction(&input).expect("the plan compiles");
    let position = |token: &str, key: &ResourceKey| {
        plan.execution_order
            .iter()
            .position(|node| node == &OperationId::resource(token, key))
            .expect("the node is planned")
    };
    assert!(
        position("remove_file", &retired) < position("backend_apply", &backend),
        "the refresh follows the removal it derives from"
    );
}

/// Without an explicit dependency a backend apply is not ordered against
/// removals: the blanket edge that once forced it is gone, so ordering is
/// stated where it is needed rather than inherited.
#[test]
fn a_backend_apply_without_dependencies_is_unordered_against_removals() {
    let id = BackendResourceId::new("fake.refresh").unwrap();
    let retired = file_key("old.exe");
    let mut input = TransactionInput::new(target());
    input.retired_keys = vec![retired.clone()];
    input.removals = vec![removal(retired.clone(), tpath(r"C:\PF\Acme\old.exe"))];
    input.backend_operations = vec![BackendOperation::apply(
        ResourceKey::Backend { id: id.clone() },
        id,
        Privilege::User,
        b"refresh".to_vec(),
    )];
    input.validate().expect("no dependency is still valid");
    zup_transaction::compile_transaction(&input).expect("the plan compiles");
}
