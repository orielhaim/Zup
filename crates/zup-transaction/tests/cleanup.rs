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
