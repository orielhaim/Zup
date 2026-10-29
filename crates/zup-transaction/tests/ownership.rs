mod common;

use std::collections::BTreeSet;

use common::fake::FakeExecutor;
use common::{digest, file, removal_input, sample_app_id, sample_version, target};
use rstest::rstest;
use zup_core::{Privilege, RelativePath, SelectedScope};
use zup_exec::{InstallLedger, OwnedResource};
use zup_transaction::{
    FileDelta, FilePrecondition, FilesystemTransactionStore, OwnershipError, ReconciliationState,
    TransactionCoordinator, TransactionInput, TransactionStore, compile_transaction,
    reconcile_committed, validate_ownership,
};

fn ledger() -> InstallLedger {
    let mut ledger = InstallLedger::new(sample_app_id(), target(), SelectedScope::User);
    ledger.committed_transaction = "generation".into();
    ledger
}

fn owned_file(work: &zup_transaction::FileWork) -> OwnedResource {
    OwnedResource::File {
        destination: work.destination.clone(),
        source_relative: work.source_relative.clone(),
        sha256: digest(b"old"),
        size: 3,
        created_directories: Vec::new(),
        privilege: Privilege::User,
    }
}

#[rstest]
#[case(FileDelta::Create)]
#[case(FileDelta::Replace)]
#[case(FileDelta::RestoreOwned)]
#[case(FileDelta::RepairOwned)]
fn file_changes_are_bound_to_committed_ownership(#[case] delta: FileDelta) {
    let mut ledger = ledger();
    let mut work = file("a", b"old");
    work.delta = delta;
    work.precondition = match delta {
        FileDelta::Create | FileDelta::RestoreOwned => FilePrecondition::Absent,
        FileDelta::Replace | FileDelta::RepairOwned => FilePrecondition::Exact {
            size: 3,
            sha256: digest(b"old"),
        },
        _ => unreachable!("only file mutations are parameterized"),
    };
    if delta != FileDelta::Create {
        ledger.resources.insert(work.key.clone(), owned_file(&work));
    }
    let mut input = TransactionInput::new(target());
    input.observed_transaction = Some(ledger.committed_transaction.clone());
    input.files.push(work.clone());
    let plan = compile_transaction(&input).unwrap();
    validate_ownership(&plan, Some(&ledger), &BTreeSet::new()).unwrap();
    if delta == FileDelta::Create {
        ledger.resources.insert(work.key.clone(), owned_file(&work));
    } else if let OwnedResource::File { sha256, .. } = ledger.resources.get_mut(&work.key).unwrap()
    {
        *sha256 = digest(b"different owner bytes");
    }
    assert!(matches!(
        validate_ownership(&plan, Some(&ledger), &BTreeSet::new()),
        Err(OwnershipError::File(_))
    ));
}

#[test]
fn removal_cannot_forget_or_retarget_committed_resources() {
    let mut ledger = ledger();
    let mut input = removal_input();
    input.uninstall = true;
    input.observed_transaction = Some(ledger.committed_transaction.clone());
    let removal = &input.removals[0];
    ledger.resources.insert(
        removal.key.clone(),
        OwnedResource::File {
            destination: removal.destination.clone(),
            source_relative: RelativePath::new("old.exe").unwrap(),
            sha256: removal.sha256,
            size: removal.size,
            created_directories: Vec::new(),
            privilege: Privilege::User,
        },
    );
    let retired = input.retired_keys.iter().cloned().collect();
    let plan = compile_transaction(&input).unwrap();
    validate_ownership(&plan, Some(&ledger), &retired).unwrap();
    assert!(matches!(
        validate_ownership(&plan, Some(&ledger), &BTreeSet::new()),
        Err(OwnershipError::Retired)
    ));
    let extra = file("extra", b"old");
    ledger
        .resources
        .insert(extra.key.clone(), owned_file(&extra));
    assert!(matches!(
        validate_ownership(&plan, Some(&ledger), &retired),
        Err(OwnershipError::Retired)
    ));
    ledger.resources.remove(&extra.key);
    ledger.committed_transaction = "new-generation".into();
    assert!(matches!(
        validate_ownership(&plan, Some(&ledger), &retired),
        Err(OwnershipError::Generation)
    ));
    ledger.committed_transaction = "generation".into();
    if let OwnedResource::File { size, .. } = ledger.resources.get_mut(&removal.key).unwrap() {
        *size += 1;
    }
    assert!(matches!(
        validate_ownership(&plan, Some(&ledger), &retired),
        Err(OwnershipError::File(_))
    ));
}

#[test]
fn reconciliation_publishes_the_durable_body_and_retries_failed_publication() {
    let root = tempfile::tempdir().unwrap();
    let store = FilesystemTransactionStore::private_at(root.path());
    let mut input = TransactionInput::new(target());
    input.files.push(file("published", b"bytes"));
    let plan = compile_transaction(&input).unwrap();
    let settled = TransactionCoordinator::new(store.clone())
        .begin(sample_app_id(), SelectedScope::User, sample_version(), plan)
        .unwrap()
        .execute(&mut FakeExecutor::new())
        .unwrap();
    let mut supplied = settled.record.clone();
    supplied.plan.nodes.clear();
    supplied.nodes.clear();
    assert_ne!(supplied.plan, settled.record.plan);
    assert_ne!(supplied.nodes, settled.record.nodes);
    let result: Result<(), _> = reconcile_committed(&store, &supplied, root.path(), |durable| {
        assert_eq!(durable.plan, settled.record.plan);
        assert_eq!(durable.nodes, settled.record.nodes);
        Err("publication interrupted".into())
    });
    assert!(result.is_err());
    assert!(matches!(
        store.load(&supplied.transaction_id).unwrap().reconciliation,
        ReconciliationState::Pending { failure: Some(_) }
    ));
    for _ in 0..2 {
        reconcile_committed(&store, &supplied, root.path(), |durable| {
            assert_eq!(durable.plan, settled.record.plan);
            Ok(())
        })
        .unwrap();
        assert_eq!(
            store.load(&supplied.transaction_id).unwrap().reconciliation,
            ReconciliationState::Complete
        );
    }
    supplied.app_id = zup_core::AppId::new("com.example.foreign").unwrap();
    assert!(
        reconcile_committed(&store, &supplied, root.path(), |_| -> Result<(), String> {
            panic!("a foreign identity cannot reach publication")
        })
        .is_err()
    );
}
