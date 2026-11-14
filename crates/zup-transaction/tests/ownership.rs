mod common;

use std::collections::BTreeSet;
use zup_plan::compile_deployment;

use common::fake::FakeExecutor;
use common::{digest, file, removal_input, sample_app_id, sample_version};
use rstest::rstest;
use zup_core::{Privilege, RelativePath, SelectedScope};
use zup_plan::{FileOperationKind, FilePrecondition};
use zup_plan::{InstallationRecord, OwnedResource};
use zup_transaction::{
    FilesystemTransactionStore, OwnershipError, ReconciliationState, TransactionCoordinator,
    TransactionStore, reconcile_committed, validate_ownership,
};

fn ledger() -> InstallationRecord {
    common::installed(SelectedScope::User)
}

fn owned_file(work: &zup_plan::FileOperation) -> OwnedResource {
    OwnedResource::File {
        replacement: zup_core::FixedReplacement::Atomic,
        instance: common::owned_instance(),
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
            object: zup_core::NativeObjectIdentity::Windows {
                volume: 1,
                file: [1; 16],
            },
            size: 3,
            sha256: digest(b"old"),
        },
        _ => unreachable!("only file mutations are parameterized"),
    };
    if delta != FileOperationKind::Create {
        ledger
            .current
            .resources
            .insert(work.key.clone(), owned_file(&work));
    }
    let mut input = common::effects();
    input.observed_transaction = Some(ledger.current.transaction);
    input.observed = Some(ledger.current.selection());
    input.files.push(work.clone());
    let plan = compile_deployment(&input).unwrap();
    validate_ownership(
        &plan,
        SelectedScope::User,
        Some(&ledger),
        &BTreeSet::new(),
        None,
    )
    .unwrap();
    if delta == FileOperationKind::Create {
        ledger
            .current
            .resources
            .insert(work.key.clone(), owned_file(&work));
    } else if let OwnedResource::File { sha256, .. } =
        ledger.current.resources.get_mut(&work.key).unwrap()
    {
        *sha256 = digest(b"different owner bytes");
    }
    assert!(matches!(
        validate_ownership(
            &plan,
            SelectedScope::User,
            Some(&ledger),
            &BTreeSet::new(),
            None
        ),
        Err(OwnershipError::File(_))
    ));
}

#[test]
fn removal_cannot_forget_or_retarget_committed_resources() {
    let mut ledger = ledger();
    let mut input = removal_input();
    input.transition = zup_plan::DeploymentTransition::Remove;
    input.observed_transaction = Some(ledger.current.transaction);
    input.observed = Some(ledger.current.selection());
    let removal = &input.removals[0];
    ledger.current.resources.insert(
        removal.key.clone(),
        OwnedResource::File {
            replacement: zup_core::FixedReplacement::Atomic,
            instance: removal.instance,
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
    let plan = compile_deployment(&input).unwrap();
    validate_ownership(&plan, SelectedScope::User, Some(&ledger), &retired, None).unwrap();
    assert!(matches!(
        validate_ownership(
            &plan,
            SelectedScope::User,
            Some(&ledger),
            &BTreeSet::new(),
            None
        ),
        Err(OwnershipError::Retired)
    ));
    let extra = file("extra", b"old");
    ledger
        .current
        .resources
        .insert(extra.key.clone(), owned_file(&extra));
    assert!(matches!(
        validate_ownership(&plan, SelectedScope::User, Some(&ledger), &retired, None),
        Err(OwnershipError::Retired)
    ));
    ledger.current.resources.remove(&extra.key);
    ledger.commit.transaction = "00000000-0000-7000-8000-000000000002".parse().unwrap();
    assert!(matches!(
        validate_ownership(&plan, SelectedScope::User, Some(&ledger), &retired, None),
        Err(OwnershipError::Generation)
    ));
    ledger.commit.transaction = "00000000-0000-7000-8000-000000000001".parse().unwrap();
    if let OwnedResource::File { size, .. } =
        ledger.current.resources.get_mut(&removal.key).unwrap()
    {
        *size += 1;
    }
    assert!(matches!(
        validate_ownership(&plan, SelectedScope::User, Some(&ledger), &retired, None),
        Err(OwnershipError::File(_))
    ));
}

#[test]
fn reconciliation_publishes_the_durable_body_and_retries_failed_publication() {
    let root = tempfile::tempdir().unwrap();
    let store = FilesystemTransactionStore::private_at(root.path().join("state"));
    let mut input = common::effects();
    input.files.push(file("published", b"bytes"));
    let plan = compile_deployment(&input).unwrap();
    let settled = TransactionCoordinator::new(store.clone())
        .begin(sample_app_id(), SelectedScope::User, sample_version(), plan)
        .unwrap()
        .execute(&mut FakeExecutor::new(), &common::Publisher::default())
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
        let before = store.load(&supplied.transaction_id).unwrap();
        reconcile_committed(&store, &supplied, root.path(), |durable| {
            assert_eq!(durable.plan, settled.record.plan);
            Ok(())
        })
        .unwrap();
        assert_eq!(
            store.load(&supplied.transaction_id).unwrap().reconciliation,
            ReconciliationState::Complete
        );
        if before.reconciliation == ReconciliationState::Complete {
            assert_eq!(store.load(&supplied.transaction_id).unwrap(), before);
        }
    }
    let settled = store.load(&supplied.transaction_id).unwrap();
    assert!(
        reconcile_committed(&store, &supplied, root.path(), |_| -> Result<(), String> {
            Err("later observation failed".into())
        })
        .is_err()
    );
    assert_eq!(store.load(&supplied.transaction_id).unwrap(), settled);
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
    let store = FilesystemTransactionStore::private_at(root.path().join("state"));
    let committed = TransactionCoordinator::new(store.clone())
        .begin(
            sample_app_id(),
            SelectedScope::User,
            sample_version(),
            common::sample_plan(),
        )
        .unwrap()
        .execute(&mut FakeExecutor::new(), &common::Publisher::default())
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
    use zup_transaction::TransactionRecord;

    let root = tempfile::tempdir().unwrap();
    let store = FilesystemTransactionStore::private_at(root.path().join("state"));
    let first: zup_core::TransactionId = "ffffffff-ffff-7fff-bfff-ffffffffffff".parse().unwrap();
    let second: zup_core::TransactionId = "00000000-0000-7000-8000-000000000001".parse().unwrap();
    let publisher = common::Publisher::default();
    let make_record = |id, observed: Option<zup_core::TransactionId>| {
        let mut input = common::effects();
        let disconnected = observed.is_none() && publisher.0.lock().unwrap().is_some();
        input.observed_transaction = observed;
        input.observed = if observed.is_some() {
            publisher
                .0
                .lock()
                .unwrap()
                .as_ref()
                .map(|record| record.current.selection())
        } else {
            None
        };
        if disconnected {
            input.installation = zup_core::InstallationId::new_v7();
            input.install_directory = Some(common::tpath(r"C:\PF\Separate"));
        }
        let record = TransactionRecord::new(
            id,
            sample_app_id(),
            SelectedScope::User,
            sample_version(),
            compile_deployment(&input).unwrap(),
        );
        store.create(&record).unwrap();
        let separate = common::Publisher::default();
        TransactionCoordinator::new(store.clone())
            .execute(
                record,
                &mut FakeExecutor::new(),
                if disconnected { &separate } else { &publisher },
            )
            .unwrap()
            .0
    };
    make_record(first, None);
    make_record(second, Some(first));
    let installed = publisher.0.lock().unwrap().clone().unwrap();
    for installation in [None, Some(&installed)] {
        let records = zup_transaction::settled_records(
            &store,
            &sample_app_id(),
            SelectedScope::User,
            installation,
        )
        .unwrap();
        assert_eq!(
            records
                .iter()
                .map(|record| record.transaction_id)
                .collect::<Vec<_>>(),
            [first, second]
        );
    }
    let mut forged = installed;
    forged.retained[0].plan_hash = digest(b"foreign history");
    assert!(
        zup_transaction::settled_records(
            &store,
            &sample_app_id(),
            SelectedScope::User,
            Some(&forged)
        )
        .is_err()
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
        .current
        .resources
        .insert(work.key.clone(), owned_file(&work));
    assert!(zup_transaction::validate_installation_record(
        &installed,
        &sample_app_id(),
        SelectedScope::User
    ));
    installed.current.resources.insert(
        work.key.clone(),
        OwnedResource::Service(zup_plan::OwnedService {
            registration: zup_core::NativeServiceRegistration::Scm {
                installation: installed.id,
            },
            timeouts: zup_core::ServiceTimeouts::Windows {
                start_observation_seconds: 90,
                stop_observation_seconds: 90,
            },
            requires_files: Vec::new(),
            requires_services: Vec::new(),
            name: "foreign".into(),
            privilege: Privilege::System,
            installed: Box::new(zup_plan::ServiceState::Absent),
        }),
    );
    assert!(!zup_transaction::validate_installation_record(
        &installed,
        &sample_app_id(),
        SelectedScope::User
    ));
    installed
        .current
        .resources
        .insert(work.key.clone(), owned_file(&work));
    installed.current.resources.insert(
        zup_core::ResourceKey::Service {
            id: zup_core::ServiceId::new("foreign").unwrap(),
        },
        owned_file(&work),
    );
    assert!(!zup_transaction::validate_installation_record(
        &installed,
        &sample_app_id(),
        SelectedScope::User
    ));
}

#[test]
fn user_state_cannot_grant_system_authority_for_creation_or_cleanup() {
    let mut work = file("protected", b"old");
    work.privilege = Privilege::System;
    let mut input = common::effects();
    input.files.push(work.clone());
    let plan = compile_deployment(&input).unwrap();
    assert!(matches!(
        validate_ownership(&plan, SelectedScope::User, None, &BTreeSet::new(), None),
        Err(OwnershipError::Authority(_))
    ));

    let mut forged = ledger();
    let mut owned = owned_file(&work);
    if let OwnedResource::File { privilege, .. } = &mut owned {
        *privilege = Privilege::System;
    }
    forged.current.resources.insert(work.key.clone(), owned);
    assert!(!zup_transaction::validate_installation_record(
        &forged,
        &sample_app_id(),
        SelectedScope::User
    ));
    let mut removal = removal_input();
    removal.observed_transaction = Some(forged.current.transaction);
    removal.observed = Some(forged.current.selection());
    removal.removals[0].key = work.key.clone();
    removal.removals[0].destination = work.destination.clone();
    removal.removals[0].sha256 = digest(b"old");
    removal.removals[0].size = 3;
    removal.removals[0].privilege = Privilege::System;
    removal.retired_keys = vec![work.key.clone()];
    let plan = compile_deployment(&removal).unwrap();
    assert!(
        validate_ownership(
            &plan,
            SelectedScope::User,
            Some(&forged),
            &BTreeSet::from([work.key]),
            None
        )
        .is_err()
    );
}

#[test]
fn recovery_rejects_a_journal_that_changes_its_authority_domain() {
    let mut work = file("protected", b"owned");
    work.privilege = Privilege::System;
    let mut input = common::effects();
    input.files.push(work);
    input.requirements.scope = SelectedScope::Machine;
    let mut record = zup_transaction::TransactionRecord::new(
        zup_core::TransactionId::new_v7(),
        sample_app_id(),
        SelectedScope::Machine,
        sample_version(),
        compile_deployment(&input).unwrap(),
    );
    record.validate().unwrap();
    record.scope = SelectedScope::User;
    assert!(matches!(
        record.validate(),
        Err(zup_transaction::CorruptReason::InvalidPlan)
    ));
}
