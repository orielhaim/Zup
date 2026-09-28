//! Per-operation authorization is carried exactly, in both directions.
//!
//! A transaction is authorized by the privileges its operations declare. No
//! scope is consulted, so a per-user install that owns a host-wide service
//! still reports that it needs system authority, and a host-wide install can
//! still run an operation that needs none.

mod common;

use zup_core::{BackendResourceId, Privilege, RelativePath, ResourceKey};
use zup_transaction::FileRemovalKind;
use zup_transaction::{
    BackendOperation, FileDelta, FilePrecondition, FileWork, NodeKind, TransactionInput,
    compile_transaction,
};

use common::{digest, target, tpath};

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

/// Authority is read off the operations, never off a scope: a per-user install
/// that owns a host-wide file still asks for system.
#[test]
fn privileges_are_carried_per_operation_and_set_the_authorization_answer() {
    let mut input = TransactionInput::new(target());
    input.files = vec![
        file_work("per-user.exe", Privilege::User),
        file_work("host-wide.exe", Privilege::System),
    ];
    input.backend_operations = vec![backend_apply("agent", Privilege::System)];

    let plan = compile_transaction(&input).unwrap();
    assert!(plan.requires_authorization());

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
        Privilege::User,
        "the first file keeps user authority"
    );
    assert_eq!(
        by_key[1].1,
        Privilege::System,
        "the second file keeps system authority"
    );
    assert_eq!(
        plan.nodes
            .iter()
            .filter(|node| node.meta.privilege == Some(Privilege::System))
            .count(),
        3,
        "one file mutation pair plus the backend op"
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
}

/// Two plans that differ only in authority are different plans: a fingerprint
/// that ignored privilege would let one install satisfy the other's journal.
#[test]
fn node_privilege_is_part_of_the_plan_fingerprint() {
    let mut input = TransactionInput::new(target());
    input.files = vec![file_work("a.exe", Privilege::User)];
    let user_plan = compile_transaction(&input).unwrap();

    input.files = vec![file_work("a.exe", Privilege::System)];
    let system_plan = compile_transaction(&input).unwrap();

    assert_ne!(user_plan.fingerprint(), system_plan.fingerprint());
}
