mod common;

use common::{effects, fake::FakeExecutor, sample_app_id, sample_version};
use zup_core::{Privilege, SelectedScope, TransactionId};
use zup_plan::{
    CleanupCandidate, CleanupOperation, DeploymentNode, DeploymentTransition, FixedCleanup,
    NodeKind, OperationId, OwnedResource, RetentionChange, compile_deployment,
};
use zup_transaction::{
    CleanupCompletion, FilesystemTransactionStore, OperationExecutor, OperationReceipt,
    ReconcileResult, TransactionCoordinator, TransactionOutcome, TransactionPhase,
    TransactionRecord,
};

fn history_plan() -> zup_plan::DeploymentPlan {
    use zup_plan::{HistoryCleanup, HistoryFileTarget, HistoryObject, JournalMember, MetadataFile};
    let mut input = effects();
    let source = TransactionId::new_v7();
    let target = HistoryFileTarget::Collection {
        intent: zup_core::hash_bytes(b"retired collection"),
    };
    let observed = MetadataFile {
        object: common::owned_instance().object,
        size: 1,
        sha256: zup_core::hash_bytes(b"metadata"),
    };
    let history = |object| CleanupOperation {
        privilege: Privilege::User,
        candidate: CleanupCandidate::History(HistoryCleanup {
            installation: input.installation,
            source,
            materializations: Vec::new(),
            object,
        }),
    };
    input.cleanup = vec![
        history(HistoryObject::Journal {
            plan_hash: zup_core::hash_bytes(b"retired plan"),
            object: common::owned_instance().object,
            members: [(JournalMember::Record, observed.clone())].into(),
            sidecars: vec![target.clone()],
        }),
        history(HistoryObject::File { target, observed }),
    ];
    let plan = compile_deployment(&input).unwrap();
    plan.validate().unwrap();
    plan
}

#[test]
fn cleanup_obeys_validated_order_instead_of_declaration_order() {
    let temporary = tempfile::tempdir().unwrap();
    let store = FilesystemTransactionStore::private_at(temporary.path().join("state"));
    let plan = history_plan();
    let declared: Vec<_> = plan
        .nodes
        .iter()
        .filter(|n| n.kind.is_post_commit())
        .map(|n| n.id.to_string())
        .collect();
    let expected = vec![declared[1].clone(), declared[0].clone()];
    let ordered: Vec<_> = plan
        .execution_order
        .iter()
        .filter(|id| declared.contains(&id.to_string()))
        .map(ToString::to_string)
        .collect();
    assert_eq!(ordered, expected);
    let mut executor = FakeExecutor::new();
    let settled = TransactionCoordinator::new(store)
        .begin(sample_app_id(), SelectedScope::User, sample_version(), plan)
        .unwrap()
        .execute(&mut executor, &common::Publisher::default())
        .unwrap();
    assert_eq!(settled.outcome, TransactionOutcome::Committed);
    for verb in ["prepare", "apply", "verify"] {
        let actual: Vec<_> = executor
            .verb(verb)
            .into_iter()
            .filter(|id| declared.contains(id))
            .collect();
        assert_eq!(actual, expected, "{verb}");
    }
}

#[test]
fn independent_cleanup_uses_the_compilers_tie_break() {
    let mut input = effects();
    input.cleanup = history_plan()
        .nodes
        .into_iter()
        .filter_map(|node| match node.kind {
            NodeKind::Cleanup { operation } => Some(*operation),
            _ => None,
        })
        .collect();
    let mut independent = input.cleanup[0].clone();
    let CleanupCandidate::History(history) = &mut independent.candidate else {
        unreachable!()
    };
    history.source = TransactionId::new_v7();
    let zup_plan::HistoryObject::Journal { sidecars, .. } = &mut history.object else {
        unreachable!()
    };
    sidecars.clear();
    input.cleanup.insert(0, independent);
    let plan = compile_deployment(&input).unwrap();
    let ids: Vec<_> = input
        .cleanup
        .iter()
        .map(|operation| operation.id().to_string())
        .collect();
    let expected = vec![ids[0].clone(), ids[2].clone(), ids[1].clone()];
    let temporary = tempfile::tempdir().unwrap();
    let mut executor = OrderedCollector::default();
    let settled = TransactionCoordinator::new(FilesystemTransactionStore::private_at(
        temporary.path().join("state"),
    ))
    .begin(sample_app_id(), SelectedScope::User, sample_version(), plan)
    .unwrap()
    .execute(&mut executor, &common::Publisher::default())
    .unwrap();
    assert_eq!(settled.outcome, TransactionOutcome::Committed);
    for verb in ["prepare", "apply", "verify"] {
        assert_eq!(
            executor
                .calls
                .iter()
                .filter(|(_, call)| *call == verb)
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>(),
            expected
        );
    }
}

#[derive(Clone, Copy)]
enum CleanupCorruption {
    Order,
    Predecessor,
    Deferred,
}

#[rstest::rstest]
#[case(CleanupCorruption::Order)]
#[case(CleanupCorruption::Predecessor)]
#[case(CleanupCorruption::Deferred)]
fn cleanup_rejects_incompatible_plan_or_completion_evidence(#[case] corruption: CleanupCorruption) {
    use zup_transaction::NodeState;
    let temporary = tempfile::tempdir().unwrap();
    let plan = history_plan();
    let cleanup: Vec<_> = plan
        .execution_order
        .iter()
        .filter(|id| id.as_str().starts_with("collect:"))
        .cloned()
        .collect();
    let mut executor = OrderedCollector::default();
    let settled = TransactionCoordinator::new(FilesystemTransactionStore::private_at(
        temporary.path().join("state"),
    ))
    .begin(sample_app_id(), SelectedScope::User, sample_version(), plan)
    .unwrap()
    .execute(&mut executor, &common::Publisher::default())
    .unwrap();
    let mut record = settled.record;
    match corruption {
        CleanupCorruption::Order => {
            let a = record
                .plan
                .execution_order
                .iter()
                .position(|id| id == &cleanup[0])
                .unwrap();
            let b = record
                .plan
                .execution_order
                .iter()
                .position(|id| id == &cleanup[1])
                .unwrap();
            record.plan.execution_order.swap(a, b);
            record.plan_hash = record.plan.fingerprint();
        }
        CleanupCorruption::Predecessor => {
            record.nodes.insert(cleanup[0].clone(), NodeState::Running);
        }
        CleanupCorruption::Deferred => {
            let NodeState::Verified { receipt } = record.nodes.get_mut(&cleanup[0]).unwrap() else {
                unreachable!()
            };
            let OperationReceipt::Cleanup { completion, .. } = receipt.as_mut() else {
                unreachable!()
            };
            *completion = CleanupCompletion::Deferred {
                reason: zup_transaction::CleanupDeferral::ReferencedMetadata,
            };
            assert!(!record.cleanup_complete());
        }
    }
    assert!(record.validate().is_err());
}

#[derive(Clone, Copy, Debug)]
enum CleanupFault {
    Prepare,
    LostReceipt,
    Partial,
    Ambiguous,
    Verify,
}

#[derive(Default)]
struct OrderedCollector {
    ordinary: FakeExecutor,
    calls: Vec<(String, &'static str)>,
    absent: std::collections::BTreeSet<OperationId>,
    fault: Option<(OperationId, CleanupFault)>,
    ambiguous: bool,
    deferred: Option<zup_transaction::CleanupDeferral>,
}

impl OrderedCollector {
    fn call(&mut self, node: &DeploymentNode, verb: &'static str) {
        self.calls.push((node.id.to_string(), verb));
    }

    fn receipt(&self, node: &DeploymentNode) -> OperationReceipt {
        let NodeKind::Cleanup { operation } = &node.kind else {
            unreachable!()
        };
        OperationReceipt::Cleanup {
            intent: operation.fingerprint(),
            completion: self.deferred.map_or(CleanupCompletion::Removed, |reason| {
                CleanupCompletion::Deferred { reason }
            }),
        }
    }
}

impl OperationExecutor for OrderedCollector {
    type Error = String;

    fn prepare(&mut self, node: &DeploymentNode) -> Result<(), String> {
        if !node.kind.is_post_commit() {
            return self.ordinary.prepare(node);
        }
        self.call(node, "prepare");
        if matches!(&self.fault, Some((id, CleanupFault::Prepare)) if *id == node.id) {
            self.fault = None;
            return Err("preparation interrupted".into());
        }
        Ok(())
    }

    fn apply(
        &mut self,
        node: &DeploymentNode,
        record: &TransactionRecord,
    ) -> Result<OperationReceipt, String> {
        if !node.kind.is_post_commit() {
            return self.ordinary.apply(node, record);
        }
        assert_eq!(record.phase, TransactionPhase::Committed);
        assert!(
            !self.absent.contains(&node.id),
            "completed deletion repeated"
        );
        self.call(node, "apply");
        if let Some((id, fault)) = self.fault.clone()
            && id == node.id
            && !matches!(fault, CleanupFault::Verify)
        {
            self.fault = None;
            match fault {
                CleanupFault::LostReceipt => {
                    self.absent.insert(node.id.clone());
                }
                CleanupFault::Ambiguous => self.ambiguous = true,
                CleanupFault::Partial => {}
                _ => unreachable!(),
            }
            return Err("application interrupted".into());
        }
        if self.deferred.is_none() {
            self.absent.insert(node.id.clone());
        }
        Ok(self.receipt(node))
    }

    fn verify(&mut self, node: &DeploymentNode, receipt: &OperationReceipt) -> Result<(), String> {
        if !node.kind.is_post_commit() {
            return self.ordinary.verify(node, receipt);
        }
        self.call(node, "verify");
        if matches!(&self.fault, Some((id, CleanupFault::Verify)) if *id == node.id) {
            self.fault = None;
            return Err("native readback unavailable".into());
        }
        if matches!(
            receipt,
            OperationReceipt::Cleanup {
                completion: CleanupCompletion::Removed | CleanupCompletion::AlreadyAbsent,
                ..
            }
        ) {
            assert!(self.absent.contains(&node.id));
        }
        Ok(())
    }

    fn reconcile(
        &mut self,
        node: &DeploymentNode,
        receipt: Option<&OperationReceipt>,
        record: &TransactionRecord,
    ) -> Result<ReconcileResult, String> {
        if !node.kind.is_post_commit() {
            return self.ordinary.reconcile(node, receipt, record);
        }
        self.call(node, "reconcile");
        Ok(if self.ambiguous {
            ReconcileResult::Ambiguous
        } else if self.absent.contains(&node.id) {
            ReconcileResult::AppliedWithReceipt(self.receipt(node))
        } else {
            ReconcileResult::Incomplete
        })
    }

    fn rollback(&mut self, _: &DeploymentNode, _: &OperationReceipt) -> Result<(), String> {
        panic!("committed cleanup must never compensate");
    }
}

#[rstest::rstest]
#[case(CleanupFault::Prepare)]
#[case(CleanupFault::LostReceipt)]
#[case(CleanupFault::Partial)]
#[case(CleanupFault::Ambiguous)]
#[case(CleanupFault::Verify)]
fn cleanup_recovery_preserves_dependency_and_completed_deletions(
    #[case] fault: CleanupFault,
    #[values(0, 1)] failed_position: usize,
) {
    use zup_transaction::TransactionStore;
    let temporary = tempfile::tempdir().unwrap();
    let store = FilesystemTransactionStore::private_at(temporary.path().join("state"));
    let publisher = common::Publisher::default();
    let plan = history_plan();
    let order: Vec<_> = plan
        .execution_order
        .iter()
        .filter(|id| id.as_str().starts_with("collect:"))
        .cloned()
        .collect();
    let mut executor = OrderedCollector {
        fault: Some((order[failed_position].clone(), fault)),
        ..Default::default()
    };
    let settled = TransactionCoordinator::new(store.clone())
        .begin(sample_app_id(), SelectedScope::User, sample_version(), plan)
        .unwrap()
        .execute(&mut executor, &publisher)
        .unwrap();
    assert_eq!(settled.outcome, TransactionOutcome::RecoveryRequired);
    assert_eq!(settled.record.phase, TransactionPhase::Committed);
    assert!(!settled.record.cleanup_complete());
    if failed_position == 0 {
        assert!(
            !executor
                .calls
                .iter()
                .any(|(id, _)| *id == order[1].to_string())
        );
    }
    let mut record = store.load(&settled.record.transaction_id).unwrap();
    if matches!(fault, CleanupFault::Ambiguous) {
        let before = executor.calls.len();
        let (pending, outcome) =
            zup_transaction::recover(record, &store, &mut executor, &publisher).unwrap();
        assert_eq!(outcome, TransactionOutcome::RecoveryRequired);
        assert_eq!(
            &executor.calls[before..],
            &[(order[failed_position].to_string(), "reconcile")]
        );
        executor.ambiguous = false;
        record = pending;
    }
    let ordinary_calls = executor.ordinary.calls.len();
    let (record, outcome) =
        zup_transaction::recover(record, &store, &mut executor, &publisher).unwrap();
    assert_eq!(
        outcome,
        TransactionOutcome::Committed,
        "{:?}",
        record.recovery_failure
    );
    assert!(record.cleanup_complete());
    assert_eq!(record.phase, TransactionPhase::Committed);
    assert_eq!(executor.ordinary.calls.len(), ordinary_calls);
    assert_eq!(executor.absent.len(), 2);
    let first_verified = executor
        .calls
        .iter()
        .rposition(|(id, verb)| *id == order[0].to_string() && *verb == "verify")
        .unwrap();
    let next_prepared = executor
        .calls
        .iter()
        .position(|(id, verb)| *id == order[1].to_string() && *verb == "prepare")
        .unwrap();
    assert!(first_verified < next_prepared);
}

#[rstest::rstest]
#[case(zup_transaction::CleanupDeferral::UntrackedReaders)]
#[case(zup_transaction::CleanupDeferral::InUse)]
#[case(zup_transaction::CleanupDeferral::FenceUnavailable)]
#[case(zup_transaction::CleanupDeferral::ReferencedMetadata)]
fn deferred_cleanup_keeps_evidence_and_successors_pending(
    #[case] reason: zup_transaction::CleanupDeferral,
) {
    use zup_transaction::{NodeState, TransactionStore};
    let temporary = tempfile::tempdir().unwrap();
    let store = FilesystemTransactionStore::private_at(temporary.path().join("state"));
    let publisher = common::Publisher::default();
    let mut executor = OrderedCollector {
        deferred: Some(reason),
        ..Default::default()
    };
    let settled = TransactionCoordinator::new(store.clone())
        .begin(
            sample_app_id(),
            SelectedScope::User,
            sample_version(),
            history_plan(),
        )
        .unwrap()
        .execute(&mut executor, &publisher)
        .unwrap();
    assert_eq!(settled.outcome, TransactionOutcome::RecoveryRequired);
    assert_eq!(settled.record.phase, TransactionPhase::Committed);
    assert!(!settled.record.cleanup_complete());
    let record = store.load(&settled.record.transaction_id).unwrap();
    let (record, outcome) =
        zup_transaction::recover(record, &store, &mut executor, &publisher).unwrap();
    assert_eq!(outcome, TransactionOutcome::RecoveryRequired);
    assert!(
        record
            .nodes
            .values()
            .any(|state| matches!(state, NodeState::Pending))
    );
    assert!(executor.absent.is_empty());
    executor.deferred = None;
    let (record, outcome) =
        zup_transaction::recover(record, &store, &mut executor, &publisher).unwrap();
    assert_eq!(outcome, TransactionOutcome::Committed);
    assert!(record.cleanup_complete());
    assert_eq!(executor.absent.len(), 2);
}

struct Collector<'a> {
    publisher: &'a common::Publisher,
    ordinary: FakeExecutor,
    applied: bool,
    lose_receipt: bool,
    applications: usize,
}

impl OperationExecutor for Collector<'_> {
    type Error = String;
    fn prepare(&mut self, node: &DeploymentNode) -> Result<(), String> {
        if matches!(node.kind, NodeKind::Cleanup { .. }) {
            Ok(())
        } else {
            self.ordinary.prepare(node)
        }
    }
    fn apply(
        &mut self,
        node: &DeploymentNode,
        transaction: &TransactionRecord,
    ) -> Result<OperationReceipt, String> {
        let NodeKind::Cleanup { operation } = &node.kind else {
            return self.ordinary.apply(node, transaction);
        };
        assert_eq!(transaction.phase, TransactionPhase::Committed);
        assert_eq!(
            self.publisher
                .0
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .commit
                .transaction,
            transaction.transaction_id
        );
        self.applied = true;
        self.applications += 1;
        if self.lose_receipt {
            self.lose_receipt = false;
            return Err("lost cleanup completion".into());
        }
        Ok(OperationReceipt::Cleanup {
            intent: operation.fingerprint(),
            completion: CleanupCompletion::Removed,
        })
    }
    fn verify(&mut self, node: &DeploymentNode, receipt: &OperationReceipt) -> Result<(), String> {
        if let NodeKind::Cleanup { operation } = &node.kind {
            assert!(self.applied);
            assert_eq!(
                receipt,
                &OperationReceipt::Cleanup {
                    intent: operation.fingerprint(),
                    completion: CleanupCompletion::Removed
                }
            );
            Ok(())
        } else {
            self.ordinary.verify(node, receipt)
        }
    }
    fn rollback(
        &mut self,
        node: &DeploymentNode,
        receipt: &OperationReceipt,
    ) -> Result<(), String> {
        assert!(!matches!(node.kind, NodeKind::Cleanup { .. }));
        self.ordinary.rollback(node, receipt)
    }
    fn reconcile(
        &mut self,
        node: &DeploymentNode,
        receipt: Option<&OperationReceipt>,
        transaction: &TransactionRecord,
    ) -> Result<ReconcileResult, String> {
        if let NodeKind::Cleanup { operation } = &node.kind {
            assert!(self.applied);
            Ok(ReconcileResult::AppliedWithReceipt(
                OperationReceipt::Cleanup {
                    intent: operation.fingerprint(),
                    completion: CleanupCompletion::Removed,
                },
            ))
        } else {
            self.ordinary.reconcile(node, receipt, transaction)
        }
    }
}

#[rstest::rstest]
#[case(false)]
#[case(true)]
fn cleanup_runs_after_commit_and_reobserves_missing_completion(#[case] lose_receipt: bool) {
    let temporary = tempfile::tempdir().unwrap();
    let store = FilesystemTransactionStore::private_at(temporary.path().join("state"));
    let publisher = common::Publisher::default();
    for _ in 0..2 {
        let previous = publisher.0.lock().unwrap().clone();
        let mut input = effects();
        input.observed = previous.as_ref().map(|record| record.current.selection());
        input.observed_transaction = previous.as_ref().map(|record| record.commit.transaction);
        let settled = TransactionCoordinator::new(store.clone())
            .begin(
                sample_app_id(),
                SelectedScope::User,
                sample_version(),
                compile_deployment(&input).unwrap(),
            )
            .unwrap()
            .execute(&mut FakeExecutor::new(), &publisher)
            .unwrap();
        assert_eq!(settled.outcome, TransactionOutcome::Committed);
    }
    let mut previous = publisher.0.lock().unwrap().clone().unwrap();
    let old = &mut previous.retained[0];
    let file = common::file("old.bin", b"old");
    old.resources.insert(
        file.key.clone(),
        OwnedResource::File {
            instance: common::owned_instance(),
            replacement: file.replacement,
            destination: file.destination.clone(),
            source_relative: file.source_relative,
            sha256: file.expected_sha256,
            size: file.expected_size,
            executable: false,
            created_directories: Vec::new(),
            privilege: Privilege::User,
        },
    );
    previous.validate().unwrap();
    *publisher.0.lock().unwrap() = Some(previous.clone());
    let mut input = effects();
    input.transition = DeploymentTransition::Maintain;
    input.observed = Some(previous.current.selection());
    input.observed_transaction = Some(previous.commit.transaction);
    input.retention = RetentionChange::keeping_previous(&previous, 0);
    input.cleanup.push(CleanupOperation {
        privilege: Privilege::User,
        candidate: CleanupCandidate::Fixed(FixedCleanup {
            key: file.key,
            path: common::tpath(r"C:\PF\Acme\.old-retired"),
            retained_by: TransactionId::new_v7(),
            operation: OperationId::new("file:old"),
            instance: common::owned_instance(),
            size: file.expected_size,
            sha256: file.expected_sha256,
        }),
    });
    let plan = compile_deployment(&input).unwrap();
    zup_transaction::validate_ownership(
        &plan,
        SelectedScope::User,
        Some(&previous),
        &Default::default(),
        None,
    )
    .unwrap();
    let mut executor = Collector {
        publisher: &publisher,
        ordinary: FakeExecutor::new(),
        applied: false,
        lose_receipt,
        applications: 0,
    };
    let settled = TransactionCoordinator::new(store.clone())
        .begin(sample_app_id(), SelectedScope::User, sample_version(), plan)
        .unwrap()
        .execute(&mut executor, &publisher)
        .unwrap();
    assert!(
        settled.record.failure.is_none(),
        "{:?} {:?}",
        settled.record.failure,
        settled.record.recovery_failure
    );

    let record = if lose_receipt {
        assert_eq!(settled.outcome, TransactionOutcome::RecoveryRequired);
        assert_eq!(settled.record.phase, TransactionPhase::Committed);
        assert!(!settled.record.cleanup_complete());
        let (record, outcome) =
            zup_transaction::recover(settled.record, &store, &mut executor, &publisher).unwrap();
        assert_eq!(outcome, TransactionOutcome::Committed);
        record
    } else {
        assert_eq!(settled.outcome, TransactionOutcome::Committed);
        settled.record
    };
    assert!(record.cleanup_complete());
    assert_eq!(executor.applications, 1);
    assert!(executor.ordinary.rolled_back.is_empty());
    assert_eq!(
        publisher.0.lock().unwrap().as_ref().unwrap().current,
        previous.current
    );
}
