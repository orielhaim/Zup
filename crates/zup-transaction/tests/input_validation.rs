use zup_plan::RemovalKind;
use zup_plan::{
    BackendOperation, DeploymentEffectsError, DeploymentResource, FileRemoval, OperationId,
    compile_deployment,
};
mod common;

use zup_core::TargetPath;
use zup_core::{
    BackendResourceId, ComponentId, Privilege, RelativePath, ResourceKey, SelectedScope,
};
use zup_plan::{FileOperation, FileOperationKind, FilePrecondition};

use common::{digest, tpath};

fn other_target() -> zup_core::Target {
    zup_core::Target::parse("linux-arm64").unwrap()
}

fn file_key(name: &str) -> ResourceKey {
    ResourceKey::File {
        id: zup_core::FileResourceId::Payload {
            id: zup_core::PayloadId::TreeMember {
                declaration: zup_core::ResourceId::new("fixture").unwrap(),
                member: zup_core::RelativePath::new(name).unwrap(),
            },
        },
    }
}

fn file_work(key: ResourceKey, source: &str, destination: TargetPath) -> FileOperation {
    let contents = b"payload";
    FileOperation {
        replacement: zup_core::FixedReplacement::Atomic,
        key,
        source_relative: RelativePath::new(source).unwrap(),
        destination,
        precondition: FilePrecondition::Absent,
        expected_sha256: digest(contents),
        expected_size: contents.len() as u64,
        privilege: Privilege::User,
        kind: FileOperationKind::Create,
        executable: false,
        conflict: None,
    }
}

fn removal(key: ResourceKey, destination: TargetPath) -> FileRemoval {
    FileRemoval {
        instance: common::owned_instance(),
        key,
        kind: RemovalKind::RemoveOwned,
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
    let mut input = common::effects();
    input.install_directory = Some(TargetPath::new(other_target(), "/opt/acme").unwrap());

    let error = input.validate().unwrap_err();

    assert_eq!(
        error,
        DeploymentEffectsError::TargetMismatch {
            resource: DeploymentResource::InstallDirectory,
        }
    );
    assert_eq!(
        error.to_string(),
        "transaction resource `install_directory` targets a different target"
    );
}

#[test]
fn target_mismatch_carries_the_owning_resource_key() {
    let staged = file_key("a.exe");
    let mut staging_input = common::effects();
    staging_input.files = vec![file_work(
        staged.clone(),
        "a.exe",
        TargetPath::new(other_target(), "/opt/acme/a.exe").unwrap(),
    )];
    assert_eq!(
        staging_input.validate().unwrap_err(),
        DeploymentEffectsError::TargetMismatch {
            resource: DeploymentResource::key(&staged),
        }
    );

    let removed = file_key("old.exe");
    let mut removal_input = common::effects();
    removal_input.retired_keys.push(removed.clone());
    let mut file = removal(removed.clone(), tpath(r"C:\PF\Acme\old.exe"));
    file.created_directories = vec![TargetPath::new(other_target(), "/opt/acme").unwrap()];
    removal_input.removals = vec![file];
    assert_eq!(
        removal_input.validate().unwrap_err(),
        DeploymentEffectsError::TargetMismatch {
            resource: DeploymentResource::key(&removed),
        }
    );
}

#[test]
fn duplicate_component_is_typed_rather_than_stringified() {
    let mut input = common::effects();
    input.selected_components = vec![ComponentId::new("core").unwrap(); 2];

    let error = input.validate().unwrap_err();

    assert_eq!(
        error,
        DeploymentEffectsError::DuplicateComponent {
            component: DeploymentResource::Component(ComponentId::new("core").unwrap()),
        }
    );
    assert_eq!(error.to_string(), "duplicate selected component `core`");
}

#[test]
fn duplicate_retired_key_names_the_key() {
    let key = file_key("a.exe");
    let mut input = common::effects();
    input.retired_keys = vec![key.clone(), key.clone()];

    assert_eq!(
        input.validate().unwrap_err(),
        DeploymentEffectsError::DuplicateKey {
            resource: DeploymentResource::key(&key),
        }
    );
}

#[test]
fn unretired_removal_names_the_removal_key() {
    let key = file_key("old.exe");
    let mut input = common::effects();
    input.removals = vec![removal(key.clone(), tpath(r"C:\PF\Acme\old.exe"))];

    assert_eq!(
        input.validate().unwrap_err(),
        DeploymentEffectsError::UnretiredRemoval {
            resource: DeploymentResource::key(&key),
        }
    );
}

#[test]
fn unknown_dependency_names_both_resources() {
    let id = BackendResourceId::new("fake.agent").unwrap();
    let unknown = file_key("missing.exe");
    let mut input = common::effects();
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
        DeploymentEffectsError::UnknownDependency {
            resource: DeploymentResource::key(&ResourceKey::Backend { id }),
            dependency: DeploymentResource::key(&unknown),
        }
    );
}

#[test]
fn duplicate_dependency_names_the_repeated_key() {
    let id = BackendResourceId::new("fake.agent").unwrap();
    let file = file_key("a.exe");
    let mut input = common::effects();
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
        DeploymentEffectsError::DuplicateDependency {
            resource: DeploymentResource::key(&ResourceKey::Backend { id }),
            dependency: DeploymentResource::key(&file),
        }
    );
}

#[test]
fn backend_identity_mismatch_names_the_declared_key() {
    let declared = file_key("a.exe");
    let mut input = common::effects();
    input.backend_operations = vec![BackendOperation::apply(
        declared.clone(),
        BackendResourceId::new("fake.agent").unwrap(),
        Privilege::System,
        b"opaque".to_vec(),
    )];

    assert_eq!(
        input.validate().unwrap_err(),
        DeploymentEffectsError::BackendIdentityMismatch {
            resource: DeploymentResource::key(&declared),
        }
    );
}

#[test]
fn self_dependency_names_the_operation_key() {
    let id = BackendResourceId::new("fake.agent").unwrap();
    let key = ResourceKey::Backend { id: id.clone() };
    let mut input = common::effects();
    input.backend_operations = vec![
        BackendOperation::apply(key.clone(), id, Privilege::System, b"opaque".to_vec())
            .with_dependencies(vec![key.clone()]),
    ];

    assert_eq!(
        input.validate().unwrap_err(),
        DeploymentEffectsError::SelfDependency {
            resource: DeploymentResource::key(&key),
        }
    );
}

#[test]
fn a_backend_apply_may_follow_a_removal() {
    let id = BackendResourceId::new("fake.refresh").unwrap();
    let backend = ResourceKey::Backend { id: id.clone() };
    let retired = file_key("old.exe");
    let mut input = common::effects();
    input.retired_keys = vec![retired.clone()];
    input.removals = vec![removal(retired.clone(), tpath(r"C:\PF\Acme\old.exe"))];
    input.backend_operations = vec![
        BackendOperation::apply(backend.clone(), id, Privilege::User, b"refresh".to_vec())
            .with_dependencies(vec![retired.clone()]),
    ];
    input.validate().expect("a removal is a valid dependency");

    let plan = compile_deployment(&input).expect("the plan compiles");
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

#[test]
fn a_backend_apply_without_dependencies_is_unordered_against_removals() {
    let id = BackendResourceId::new("fake.refresh").unwrap();
    let retired = file_key("old.exe");
    let mut input = common::effects();
    input.retired_keys = vec![retired.clone()];
    input.removals = vec![removal(retired.clone(), tpath(r"C:\PF\Acme\old.exe"))];
    input.backend_operations = vec![BackendOperation::apply(
        ResourceKey::Backend { id: id.clone() },
        id,
        Privilege::User,
        b"refresh".to_vec(),
    )];
    input.validate().expect("no dependency is still valid");
    zup_plan::compile_deployment(&input).expect("the plan compiles");
}
