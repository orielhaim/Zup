mod common;

use std::collections::BTreeSet;

use common::fake::FakeExecutor;
use common::{digest, file, removal_input, sample_app_id, sample_version, target};
use rstest::rstest;
use zup_core::{Privilege, RelativePath, SelectedScope};
use zup_plan::{FileOperationKind, FilePrecondition};
use zup_plan::{InstallLedger, OwnedResource};
use zup_transaction::{
    FilesystemTransactionStore, OwnershipError, ReconciliationState, TransactionCoordinator,
    TransactionInput, TransactionStore, compile_transaction, reconcile_committed,
    validate_ownership,
};

fn ledger() -> InstallLedger {
    let mut ledger = InstallLedger::new(
        sample_app_id(),
        target(),
        SelectedScope::User,
        zup_core::TransactionId::new_v7(),
    );
    ledger.committed_transaction = "00000000-0000-7000-8000-000000000001".parse().unwrap();
    ledger
}

fn owned_file(work: &zup_plan::FileOperation) -> OwnedResource {
    OwnedResource::File {
        destination: work.destination.clone(),
        source_relative: work.source_relative.clone(),
        sha256: digest(b"old"),
        size: 3,
        created_directories: Vec::new(),
        executable: false,
        privilege: Privilege::User,
    }
}

#[rstest]
#[case(FileOperationKind::Create)]
#[case(FileOperationKind::Replace)]
#[case(FileOperationKind::RestoreOwned)]
#[case(FileOperationKind::RepairOwned)]
fn file_changes_are_bound_to_committed_ownership(#[case] delta: FileOperationKind) {
    let mut ledger = ledger();
    let mut work = file("a", b"old");
    work.kind = delta;
    work.precondition = match delta {
        FileOperationKind::Create | FileOperationKind::RestoreOwned => FilePrecondition::Absent,
        FileOperationKind::Replace | FileOperationKind::RepairOwned => FilePrecondition::Exact {
            size: 3,
            sha256: digest(b"old"),
        },
        _ => unreachable!("only file mutations are parameterized"),
    };
    if delta != FileOperationKind::Create {
        ledger.resources.insert(work.key.clone(), owned_file(&work));
    }
    let mut input = TransactionInput::new(target());
    input.observed_transaction = Some(ledger.committed_transaction);
    input.files.push(work.clone());
    let plan = compile_transaction(&input).unwrap();
    validate_ownership(&plan, SelectedScope::User, Some(&ledger), &BTreeSet::new()).unwrap();
    if delta == FileOperationKind::Create {
        ledger.resources.insert(work.key.clone(), owned_file(&work));
    } else if let OwnedResource::File { sha256, .. } = ledger.resources.get_mut(&work.key).unwrap()
    {
        *sha256 = digest(b"different owner bytes");
    }
    assert!(matches!(
        validate_ownership(&plan, SelectedScope::User, Some(&ledger), &BTreeSet::new()),
        Err(OwnershipError::File(_))
    ));
}

#[test]
fn removal_cannot_forget_or_retarget_committed_resources() {
    let mut ledger = ledger();
    let mut input = removal_input();
    input.uninstall = true;
    input.observed_transaction = Some(ledger.committed_transaction);
    let removal = &input.removals[0];
    ledger.resources.insert(
        removal.key.clone(),
        OwnedResource::File {
            destination: removal.destination.clone(),
            source_relative: RelativePath::new("old.exe").unwrap(),
            sha256: removal.sha256,
            size: removal.size,
            created_directories: Vec::new(),
            executable: false,
            privilege: Privilege::User,
        },
    );
    let retired = input.retired_keys.iter().cloned().collect();
    let plan = compile_transaction(&input).unwrap();
    validate_ownership(&plan, SelectedScope::User, Some(&ledger), &retired).unwrap();
    assert!(matches!(
        validate_ownership(&plan, SelectedScope::User, Some(&ledger), &BTreeSet::new()),
        Err(OwnershipError::Retired)
    ));
    let extra = file("extra", b"old");
    ledger
        .resources
        .insert(extra.key.clone(), owned_file(&extra));
    assert!(matches!(
        validate_ownership(&plan, SelectedScope::User, Some(&ledger), &retired),
        Err(OwnershipError::Retired)
    ));
    ledger.resources.remove(&extra.key);
    ledger.committed_transaction = "00000000-0000-7000-8000-000000000002".parse().unwrap();
    assert!(matches!(
        validate_ownership(&plan, SelectedScope::User, Some(&ledger), &retired),
        Err(OwnershipError::Generation)
    ));
    ledger.committed_transaction = "00000000-0000-7000-8000-000000000001".parse().unwrap();
    if let OwnedResource::File { size, .. } = ledger.resources.get_mut(&removal.key).unwrap() {
        *size += 1;
    }
    assert!(matches!(
        validate_ownership(&plan, SelectedScope::User, Some(&ledger), &retired),
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

#[test]
fn unsettled_history_blocks_reconciliation_before_any_cleanup() {
    let root = tempfile::tempdir().unwrap();
    let store = FilesystemTransactionStore::private_at(root.path());
    let committed = TransactionCoordinator::new(store.clone())
        .begin(
            sample_app_id(),
            SelectedScope::User,
            sample_version(),
            common::sample_plan(),
        )
        .unwrap()
        .execute(&mut FakeExecutor::new())
        .unwrap();
    let pending = TransactionCoordinator::new(store.clone())
        .begin(
            sample_app_id(),
            SelectedScope::User,
            sample_version(),
            common::sample_plan(),
        )
        .unwrap()
        .preserve();
    assert!(matches!(
        zup_transaction::settled_records(&store, &sample_app_id(), SelectedScope::User, None),
        Err(zup_transaction::ReconciliationError::RecoveryRequired(id)) if id == pending.transaction_id
    ));
    assert!(matches!(
        store
            .load(&committed.record.transaction_id)
            .unwrap()
            .reconciliation,
        ReconciliationState::Pending { .. }
    ));
}

#[test]
fn reconciliation_uses_generation_edges_instead_of_clock_order() {
    use zup_transaction::{
        LedgerPublication, NodeState, OperationReceipt, TransactionRecord, ledger_publication,
    };

    let root = tempfile::tempdir().unwrap();
    let store = FilesystemTransactionStore::private_at(root.path());
    let first: zup_core::TransactionId = "ffffffff-ffff-7fff-bfff-ffffffffffff".parse().unwrap();
    let second: zup_core::TransactionId = "00000000-0000-7000-8000-000000000001".parse().unwrap();
    let make_record = |id, observed| {
        let mut input = TransactionInput::new(target());
        input.observed_transaction = observed;
        let mut record = TransactionRecord::new(
            id,
            sample_app_id(),
            SelectedScope::User,
            sample_version(),
            compile_transaction(&input).unwrap(),
        );
        record.phase = zup_transaction::TransactionPhase::Committed;
        for state in record.nodes.values_mut() {
            *state = NodeState::Applied {
                receipt: Box::new(OperationReceipt::Control),
            };
        }
        store.create(&record).unwrap();
        record
    };
    make_record(first, None);
    let mut successor = make_record(second, Some(first));
    let records =
        zup_transaction::settled_records(&store, &sample_app_id(), SelectedScope::User, None)
            .unwrap();
    assert_eq!(
        records
            .iter()
            .map(|record| record.transaction_id)
            .collect::<Vec<_>>(),
        [first, second]
    );
    let mut installed = InstallLedger::new(sample_app_id(), target(), SelectedScope::User, first);
    assert_eq!(
        ledger_publication(&successor, Some(&installed)).unwrap(),
        LedgerPublication::Advance
    );
    installed.committed_transaction = second;
    assert_eq!(
        ledger_publication(&successor, Some(&installed)).unwrap(),
        LedgerPublication::Current
    );
    installed.committed_transaction = zup_core::TransactionId::new_v7();
    assert!(ledger_publication(&successor, Some(&installed)).is_err());
    successor.reconciliation = ReconciliationState::Complete;
    assert_eq!(
        ledger_publication(&successor, Some(&installed)).unwrap(),
        LedgerPublication::Settled
    );

    make_record(zup_core::TransactionId::new_v7(), None);
    assert!(matches!(
        zup_transaction::settled_records(&store, &sample_app_id(), SelectedScope::User, None),
        Err(zup_transaction::ReconciliationError::Generations)
    ));
}

#[test]
fn malformed_ownership_cannot_change_resource_family_or_alias_an_identity() {
    let mut installed = ledger();
    let work = file("owned", b"old");
    installed
        .resources
        .insert(work.key.clone(), owned_file(&work));
    assert!(zup_transaction::validate_ledger_identity(
        &installed,
        &sample_app_id(),
        SelectedScope::User
    ));
    installed.resources.insert(
        work.key.clone(),
        OwnedResource::Service {
            timeouts: zup_core::ServiceTimeouts::Windows {
                start_observation_seconds: 90,
                stop_observation_seconds: 90,
            },
            requires_files: Vec::new(),
            requires_services: Vec::new(),
            name: "foreign".into(),
            privilege: Privilege::System,
            previous: zup_plan::ServiceState::Absent,
            installed: Box::new(zup_plan::ServiceState::Absent),
        },
    );
    assert!(!zup_transaction::validate_ledger_identity(
        &installed,
        &sample_app_id(),
        SelectedScope::User
    ));
    installed
        .resources
        .insert(work.key.clone(), owned_file(&work));
    let zup_core::ResourceKey::File { destination } = &work.key else {
        panic!("file key")
    };
    installed.resources.insert(
        zup_core::ResourceKey::File {
            destination: destination.to_uppercase(),
        },
        owned_file(&work),
    );
    assert!(!zup_transaction::validate_ledger_identity(
        &installed,
        &sample_app_id(),
        SelectedScope::User
    ));
}

#[test]
fn user_state_cannot_grant_system_authority_for_creation_or_cleanup() {
    let mut work = file("protected", b"old");
    work.privilege = Privilege::System;
    let mut input = TransactionInput::new(target());
    input.files.push(work.clone());
    let plan = compile_transaction(&input).unwrap();
    assert!(matches!(
        validate_ownership(&plan, SelectedScope::User, None, &BTreeSet::new()),
        Err(OwnershipError::Authority(_))
    ));

    let mut forged = ledger();
    let mut owned = owned_file(&work);
    if let OwnedResource::File { privilege, .. } = &mut owned {
        *privilege = Privilege::System;
    }
    forged.resources.insert(work.key.clone(), owned);
    assert!(!zup_transaction::validate_ledger_identity(
        &forged,
        &sample_app_id(),
        SelectedScope::User
    ));
    let mut removal = removal_input();
    removal.observed_transaction = Some(forged.committed_transaction);
    removal.removals[0].key = work.key.clone();
    removal.removals[0].destination = work.destination.clone();
    removal.removals[0].sha256 = digest(b"old");
    removal.removals[0].size = 3;
    removal.removals[0].privilege = Privilege::System;
    removal.retired_keys = vec![work.key.clone()];
    let plan = compile_transaction(&removal).unwrap();
    assert!(matches!(
        validate_ownership(
            &plan,
            SelectedScope::User,
            Some(&forged),
            &BTreeSet::from([work.key])
        ),
        Err(OwnershipError::Authority(_))
    ));
}

#[test]
fn recovery_rejects_a_journal_that_changes_its_authority_domain() {
    let mut work = file("protected", b"owned");
    work.privilege = Privilege::System;
    let mut input = TransactionInput::new(target());
    input.files.push(work);
    let mut record = zup_transaction::TransactionRecord::new(
        zup_core::TransactionId::new_v7(),
        sample_app_id(),
        SelectedScope::Machine,
        sample_version(),
        compile_transaction(&input).unwrap(),
    );
    record.validate().unwrap();
    record.scope = SelectedScope::User;
    assert!(matches!(
        record.validate(),
        Err(zup_transaction::CorruptReason::InvalidPlan)
    ));
}
