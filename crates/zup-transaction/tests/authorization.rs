//! Per-operation authorization is carried exactly, in both directions.
//!
//! A transaction is authorized by the privileges its operations declare. No
//! scope is consulted, so a per-user install that owns a host-wide service
//! still reports that it needs system authority, and a host-wide install can
//! still run an operation that needs none.

mod common;

use zup_core::{
    BackendResourceId, ComponentId, Privilege, RelativePath, ResourceKey, Sha256Digest,
};
use zup_transaction::FileRemovalKind;
use zup_transaction::{
    BackendOperation, FileDelta, FilePrecondition, FileWork, NodeKind, TransactionInput,
    compile_transaction,
};

use common::{digest, sample_plan, target, tpath};

fn file_work(name: &str, privilege: Privilege) -> FileWork {
    let contents = b"payload";
    FileWork {
        key: ResourceKey::File {
            destination: format!(r"C:\PF\Acme\{name}"),
        },
        source_relative: RelativePath::new(name).unwrap(),
        destination: tpath(&format!(r"C:\PF\Acme\{name}")),
        precondition: FilePrecondition::Absent,
        expected_sha256: digest(contents),
        expected_size: contents.len() as u64,
        privilege,
        delta: FileDelta::Create,
    }
}

fn backend_apply(name: &str, privilege: Privilege) -> BackendOperation {
    let id = BackendResourceId::new(format!("fake.{name}")).unwrap();
    BackendOperation::apply(
        ResourceKey::Backend { id: id.clone() },
        id,
        privilege,
        b"opaque".to_vec(),
    )
}

#[test]
fn user_scope_transaction_reports_system_operations() {
    // No scope is present on a transaction at all; only the operations are.
    let mut input = TransactionInput::new(target());
    input.selected_components = vec![ComponentId::new("core").unwrap()];
    input.files = vec![
        file_work("per-user.exe", Privilege::User),
        file_work("host-wide.exe", Privilege::System),
    ];
    input.backend_operations = vec![backend_apply("agent", Privilege::System)];

    let plan = compile_transaction(&input).unwrap();
    assert!(plan.requires_authorization());

    let system_nodes = plan
        .nodes
        .iter()
        .filter(|node| node.meta.privilege == Some(Privilege::System))
        .count();
    assert_eq!(
        system_nodes, 3,
        "one file mutation pair plus the backend op"
    );

    // The privilege of each operation survives the round trip unchanged.
    let restored: zup_transaction::TransactionPlan =
        serde_json::from_str(&serde_json::to_string(&plan).unwrap()).unwrap();
    assert_eq!(restored, plan);
    assert_eq!(
        restored
            .nodes
            .iter()
            .filter(|node| node.meta.privilege == Some(Privilege::System))
            .count(),
        system_nodes
    );
}

#[test]
fn per_operation_privileges_are_not_inferred_from_each_other() {
    let mut input = TransactionInput::new(target());
    input.files = vec![
        file_work("system.exe", Privilege::System),
        file_work("user.exe", Privilege::User),
    ];
    let plan = compile_transaction(&input).unwrap();

    let by_key: Vec<(&ResourceKey, Privilege)> = plan
        .nodes
        .iter()
        .filter_map(|node| match &node.kind {
            NodeKind::FileMutation { key, .. } => Some((key, node.meta.privilege?)),
            _ => None,
        })
        .collect();
    assert_eq!(by_key.len(), 2);
    assert_eq!(
        by_key[0].1,
        Privilege::System,
        "the first file keeps system authority"
    );
    assert_eq!(
        by_key[1].1,
        Privilege::User,
        "the second file keeps user authority"
    );
}

#[test]
fn removal_authorization_comes_from_the_removal_not_the_scope() {
    let mut input = TransactionInput::new(target());
    let key = ResourceKey::File {
        destination: r"C:\PF\Acme\host-wide.exe".into(),
    };
    input.retired_keys.push(key.clone());
    input.removals.push(zup_transaction::FileRemoval {
        key,
        kind: FileRemovalKind::RemoveOwned,
        scope: zup_core::SelectedScope::User,
        privilege: Privilege::System,
        destination: tpath(r"C:\PF\Acme\host-wide.exe"),
        sha256: digest(b"old"),
        size: 3,
        created_directories: Vec::new(),
    });
    let plan = compile_transaction(&input).unwrap();
    assert!(plan.requires_authorization());
    let removal = plan
        .nodes
        .iter()
        .find(|node| matches!(node.kind, NodeKind::FileRemoval { .. }))
        .expect("removal node");
    assert_eq!(removal.meta.privilege, Some(Privilege::System));
}

#[test]
fn all_user_operations_need_no_system_authorization() {
    let mut input = TransactionInput::new(target());
    input.files = vec![file_work("a.exe", Privilege::User)];
    input.backend_operations = vec![backend_apply("hook", Privilege::User)];
    assert!(
        !compile_transaction(&input)
            .unwrap()
            .requires_authorization()
    );

    // The shared fixture mixes authorities, so the plan must report true.
    assert!(sample_plan().requires_authorization());
}

#[test]
fn serialized_nodes_name_authorization_and_not_elevation() {
    let plan = sample_plan();
    let json = serde_json::to_string(&plan).unwrap();
    assert!(
        !json.contains("elevation"),
        "transaction plan leaked elevation: {json}"
    );
    assert!(json.contains("\"privilege\":\"system\""));
    assert!(json.contains("\"privilege\":\"user\""));
}

#[test]
fn node_privilege_is_part_of_the_plan_fingerprint() {
    let mut input = TransactionInput::new(target());
    input.files = vec![file_work("a.exe", Privilege::User)];
    let user_plan = compile_transaction(&input).unwrap();

    input.files = vec![file_work("a.exe", Privilege::System)];
    let system_plan = compile_transaction(&input).unwrap();

    assert_ne!(user_plan.fingerprint(), system_plan.fingerprint());
    let _: Sha256Digest = user_plan.fingerprint();
}
