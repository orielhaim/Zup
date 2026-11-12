//! Coordinator execution, rollback, crash injection, and recovery tests.

mod common;

use common::fake::FakeExecutor;
use common::{chain_input, sample_app_id, sample_input, sample_plan, sample_version};
use rstest::rstest;
use tempfile::TempDir;
use zup_core::TransactionId;
use zup_plan::{DeploymentNode, NodeKind, OperationId, compile_deployment};
use zup_transaction::{
    FilesystemTransactionStore, NodeState, ReconcileResult, RecoveryPhase, TransactionCoordinator,
    TransactionOutcome, TransactionPhase, TransactionRecord, TransactionStore, recover,
};

fn coord() -> (TempDir, TransactionCoordinator<FilesystemTransactionStore>) {
    let dir = TempDir::new().unwrap();
    let store = FilesystemTransactionStore::private_at(dir.path().join("state"));
    (dir, TransactionCoordinator::new(store))
}

fn coord_with_store() -> (TempDir, FilesystemTransactionStore) {
    let dir = TempDir::new().unwrap();
    let store = FilesystemTransactionStore::private_at(dir.path().join("state"));
    (dir, store)
}

fn first_op(record: &TransactionRecord) -> zup_plan::OperationId {
    record
        .plan
        .execution_order
        .iter()
        .find(|id| !id.as_str().starts_with("ctrl:"))
        .cloned()
        .expect("has mutating node")
}

fn barrier(name: &str) -> OperationId {
    OperationId::new(name)
}

/// Applied non-control operation ids, in execution order.
fn mutating_applies(exec: &FakeExecutor) -> Vec<String> {
    exec.applied
        .iter()
        .filter(|id| !id.starts_with("ctrl:"))
        .cloned()
        .collect()
}

/// Applied operation ids whose installed state is verified before commit.
fn verified_applies(exec: &FakeExecutor) -> Vec<String> {
    exec.applied
        .iter()
        .filter(|id| !id.starts_with("ctrl:") && !id.starts_with("op:stage-file:"))
        .cloned()
        .collect()
}

/// Every attempted verification, successful or not.
fn verify_attempts(exec: &FakeExecutor) -> Vec<String> {
    exec.verb("verify")
        .into_iter()
        .filter(|id| !id.starts_with("op:stage-file:"))
        .collect()
}

/// A record caught mid-verification: every work node applied, the begin,
/// preflight, and commit-intent barriers crossed, the verify barrier in flight.
/// The caller bumps the revision and persists it.
fn mid_verification(record: &TransactionRecord) -> (TransactionRecord, Vec<DeploymentNode>) {
    let mut crashed = record.clone();
    let checked = crashed
        .plan
        .nodes
        .iter()
        .filter(|node| {
            !node.kind.is_barrier()
                && !matches!(
                    node.kind,
                    NodeKind::ReserveInstallation | NodeKind::PublishInstallation
                )
        })
        .cloned()
        .collect::<Vec<_>>();
    crashed.phase = TransactionPhase::Applying;
    for barrier in ["ctrl:begin", "ctrl:preflight", "ctrl:commit-intent"] {
        crashed.nodes.insert(
            OperationId::new(barrier),
            NodeState::Applied {
                receipt: Box::new(zup_transaction::OperationReceipt::Control),
            },
        );
    }
    for node in &checked {
        crashed.nodes.insert(
            node.id.clone(),
            NodeState::Applied {
                receipt: Box::new(common::fake::receipt_for(node)),
            },
        );
    }
    crashed
        .nodes
        .insert(barrier("ctrl:verify"), NodeState::Running);
    (crashed, checked)
}

/// A store that lets a competing write win a fixed number of races.
///
/// The competing write is a real one, so the coordinator's in-flight record
/// really is stale when it tries to swap. This is the shape of the failure a
/// second actor on the same transaction produces, and it must cost the
/// coordinator a retry rather than the transaction.
struct RevisionRaceStore {
    inner: FilesystemTransactionStore,
    interloper: OperationId,
    budget: std::sync::Mutex<usize>,
    conflicts: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl RevisionRaceStore {
    fn new(root: &std::path::Path, interloper: OperationId, budget: usize) -> (Self, Conflicts) {
        let conflicts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        (
            Self {
                inner: FilesystemTransactionStore::private_at(root.join("state")),
                interloper,
                budget: std::sync::Mutex::new(budget),
                conflicts: std::sync::Arc::clone(&conflicts),
            },
            conflicts,
        )
    }
}

type Conflicts = std::sync::Arc<std::sync::atomic::AtomicUsize>;

impl TransactionStore for RevisionRaceStore {
    fn reserve_installation(
        &self,
        record: &TransactionRecord,
    ) -> Result<zup_transaction::InstallationReservation, zup_transaction::StoreError> {
        self.inner.reserve_installation(record)
    }
    fn create(&self, record: &TransactionRecord) -> Result<(), zup_transaction::StoreError> {
        self.inner.create(record)
    }

    fn load(&self, id: &TransactionId) -> Result<TransactionRecord, zup_transaction::StoreError> {
        self.inner.load(id)
    }

    fn compare_and_swap(
        &self,
        expected_revision: u64,
        updated: &TransactionRecord,
    ) -> Result<(), zup_transaction::StoreError> {
        let steal = {
            let mut budget = self.budget.lock().unwrap();
            if *budget == 0 {
                false
            } else {
                *budget -= 1;
                true
            }
        };
        if !steal {
            return self.inner.compare_and_swap(expected_revision, updated);
        }
        let id = updated.transaction_id;
        let interloper = self.interloper.clone();
        self.inner.update(&id, &mut |fresh| {
            fresh.nodes.insert(interloper.clone(), NodeState::Running);
            Ok(())
        })?;
        self.conflicts
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Err(zup_transaction::StoreError::RevisionConflict {
            expected: expected_revision,
        })
    }
}

#[test]
fn a_lost_swap_costs_a_retry_not_the_transaction() {
    let (dir, store) = coord_with_store();
    let plan = sample_plan();
    let record = TransactionRecord::new(
        TransactionId::new_v7(),
        sample_app_id(),
        zup_core::SelectedScope::User,
        sample_version(),
        plan,
    );
    let id = record.transaction_id;
    let mutating_nodes = record
        .plan
        .execution_order
        .iter()
        .filter(|op| !op.as_str().starts_with("ctrl:"))
        .count();
    let verifying_nodes = record
        .plan
        .nodes
        .iter()
        .filter(|node| node.kind.requires_verification())
        .count();
    store.create(&record).unwrap();
    let (racer, conflicts) = RevisionRaceStore::new(dir.path(), barrier("ctrl:begin"), 6);
    let coord = TransactionCoordinator::new(racer);

    let mut exec = FakeExecutor::new();
    let (final_record, outcome) = coord
        .execute(record, &mut exec, &common::Publisher::default())
        .unwrap();

    assert_eq!(outcome, TransactionOutcome::Committed);
    assert_eq!(final_record.phase, TransactionPhase::Committed);
    final_record.validate().expect("final state is consistent");
    assert_eq!(
        store.load(&id).unwrap(),
        final_record,
        "what the journal holds is what the coordinator reported"
    );
    assert_eq!(
        conflicts.load(std::sync::atomic::Ordering::SeqCst),
        6,
        "the interloper must have won every race it budgeted"
    );
    // A retry re-journals, it does not re-run: each node's side effect and its
    // verification still happen exactly once.
    assert_eq!(
        mutating_applies(&exec).len(),
        mutating_nodes,
        "each mutation applied once"
    );
    assert_eq!(
        verified_applies(&exec).len(),
        verifying_nodes,
        "each verified mutation checked once"
    );
}

/// Commit is gated on verification: nothing is journaled `Verified` that was not
/// checked, nothing is checked before it was applied, and nothing is checked
/// after the verify barrier that guards the commit.
#[test]
fn commit_requires_every_mutation_verified_before_it() {
    let (_dir, coord) = coord();
    let open = coord
        .begin(
            sample_app_id(),
            zup_core::SelectedScope::User,
            sample_version(),
            sample_plan(),
        )
        .unwrap();
    let record = open.record().clone();
    let verified_nodes = record
        .plan
        .nodes
        .iter()
        .filter(|node| node.kind.requires_verification())
        .map(|node| node.id.clone())
        .collect::<Vec<_>>();
    assert!(!verified_nodes.is_empty());
    let last_verify = verified_nodes.last().expect("has verified node").clone();

    let mut exec = FakeExecutor::new();
    let settled = open
        .execute(&mut exec, &common::Publisher::default())
        .unwrap();
    let (final_record, outcome) = (settled.record, settled.outcome);
    assert_eq!(
        outcome,
        TransactionOutcome::Committed,
        "{:?}; {:?}",
        final_record.failure,
        final_record.recovery_failure
    );
    assert_eq!(final_record.phase, TransactionPhase::Committed);

    for id in &verified_nodes {
        assert!(
            matches!(final_record.nodes.get(id), Some(NodeState::Verified { .. })),
            "{id} must be verified before commit"
        );
        assert!(
            exec.position("apply", id).unwrap() < exec.position("verify", id).unwrap(),
            "{id} is verified after it is applied"
        );
    }
    let crossed_verify = exec.position("apply", &barrier("ctrl:verify")).unwrap();
    assert!(
        exec.position("verify", &last_verify).unwrap() < crossed_verify,
        "all verification precedes the verify barrier: {:?}",
        exec.labels()
    );
    assert!(matches!(
        final_record.publication,
        zup_transaction::PublicationState::Published { .. }
    ));
    for barrier in [
        "ctrl:begin",
        "ctrl:preflight",
        "ctrl:commit-intent",
        "ctrl:verify",
        "ctrl:commit",
    ] {
        assert!(
            matches!(
                final_record.nodes.get(&OperationId::new(barrier)),
                Some(NodeState::Applied { receipt }) if **receipt == zup_transaction::OperationReceipt::Control
            ),
            "{barrier} must be journaled with a payload-free control receipt"
        );
    }
}

#[test]
fn barriers_are_prepared_before_any_side_effect() {
    let (_dir, coord) = coord();
    let open = coord
        .begin(
            sample_app_id(),
            zup_core::SelectedScope::User,
            sample_version(),
            sample_plan(),
        )
        .unwrap();
    let record = open.record().clone();
    let begin = barrier("ctrl:begin");
    let preflight = barrier("ctrl:preflight");
    let commit_intent = barrier("ctrl:commit-intent");
    let mutation = first_op(&record);

    let mut exec = FakeExecutor::new();
    let outcome = open
        .execute(&mut exec, &common::Publisher::default())
        .unwrap()
        .outcome;
    assert_eq!(outcome, TransactionOutcome::Committed);

    for effect in record.plan.nodes.iter().filter(|node| {
        !node.kind.is_barrier()
            && !matches!(
                node.kind,
                NodeKind::ReserveInstallation | NodeKind::PublishInstallation
            )
    }) {
        assert!(
            exec.position("prepare", &effect.id).unwrap()
                < exec.position("apply", &commit_intent).unwrap()
        );
    }
    let prepared_begin = exec.position("prepare", &begin).expect("begin prepared");
    let first_mutation = exec.position("apply", &mutation).expect("mutation applied");
    assert!(
        prepared_begin < first_mutation,
        "begin prepare must precede the first side effect: {:?}",
        exec.labels()
    );
    let prepared_preflight = exec
        .position("prepare", &preflight)
        .expect("preflight prepared");
    let crossed_intent = exec
        .position("apply", &commit_intent)
        .expect("commit intent crossed");
    assert!(
        prepared_preflight < crossed_intent,
        "preflight must be prepared immediately before commit intent: {:?}",
        exec.labels()
    );
    assert!(exec.position("apply", &barrier("ctrl:commit")).is_none());
}

/// Whatever a failure undoes, it undoes in reverse execution order, and what
/// was applied is exactly what gets rolled back.
#[rstest]
#[case::prepare_fails_before_any_work(|exec: FakeExecutor| exec.fail_on_prepare(0), 0, 0)]
#[case::preflight_prepare_fails_after_staging(|exec: FakeExecutor| exec.fail_on_prepare(1), 3, 0)]
#[case::first_file_mutation_fails(|exec: FakeExecutor| exec.fail_on_apply(6), 3, 0)]
#[case::verification_fails(|exec: FakeExecutor| exec.fail_on_verify(0), 4, 1)]
fn failure_rolls_back_applied_work_in_reverse_order(
    #[case] inject: fn(FakeExecutor) -> FakeExecutor,
    #[case] applied: usize,
    #[case] attempted_verifications: usize,
) {
    let (_dir, coord) = coord();
    let open = coord
        .begin(
            sample_app_id(),
            zup_core::SelectedScope::User,
            sample_version(),
            compile_deployment(&chain_input()).unwrap(),
        )
        .unwrap();

    let mut exec = inject(FakeExecutor::new());
    let settled = open
        .execute(&mut exec, &common::Publisher::default())
        .unwrap();
    let (final_record, outcome) = (settled.record, settled.outcome);

    assert_eq!(outcome, TransactionOutcome::RolledBack);
    assert_eq!(final_record.phase, TransactionPhase::RolledBack);
    let mutated = mutating_applies(&exec);
    assert_eq!(mutated.len(), applied, "applied work: {mutated:?}");
    assert_eq!(
        exec.rolled_back,
        mutated.iter().rev().cloned().collect::<Vec<_>>(),
        "every applied mutation is undone, in reverse order"
    );
    assert_eq!(
        verify_attempts(&exec).len(),
        attempted_verifications,
        "verification stops at the failure: {:?}",
        exec.labels()
    );
    assert_eq!(
        exec.verified.len(),
        0,
        "nothing verifies on the way to a rollback"
    );
    for id in &mutated {
        assert!(
            final_record
                .nodes
                .get(&OperationId::new(id))
                .is_some_and(NodeState::is_rolled_back),
            "{id} is undone"
        );
    }
    for uncrossed in ["ctrl:verify", "ctrl:commit"] {
        assert_eq!(
            final_record.nodes.get(&barrier(uncrossed)).cloned(),
            Some(NodeState::Pending),
            "{uncrossed} is past the failure and was never crossed"
        );
    }
}

#[rstest]
#[case::rollback_failure(FakeExecutor::new().fail_on_apply(6).fail_on_rollback(0), RecoveryPhase::RollingBack)]
#[case::incomplete_mutation(FakeExecutor::new().fail_on_apply(6).reconcile_with(ReconcileResult::Incomplete), RecoveryPhase::Applying)]
fn failed_mutation_or_rollback_requires_recovery(
    #[case] mut exec: FakeExecutor,
    #[case] resume: RecoveryPhase,
) {
    let (_dir, coord) = coord();
    let plan = compile_deployment(&chain_input()).unwrap();
    let open = coord
        .begin(
            sample_app_id(),
            zup_core::SelectedScope::User,
            sample_version(),
            plan,
        )
        .unwrap();
    let settled = open
        .execute(&mut exec, &common::Publisher::default())
        .unwrap();
    let (final_record, outcome) = (settled.record, settled.outcome);
    assert_eq!(outcome, TransactionOutcome::RecoveryRequired);
    assert_eq!(
        final_record.phase,
        TransactionPhase::RecoveryRequired { resume }
    );
}

#[rstest]
#[case::still_ambiguous(ReconcileResult::Ambiguous, TransactionOutcome::RecoveryRequired)]
#[case::proved_unapplied(ReconcileResult::NotApplied, TransactionOutcome::RolledBack)]
fn blocked_failed_apply_rechecks_evidence_without_resuming_forward_work(
    #[case] evidence: ReconcileResult,
    #[case] expected: TransactionOutcome,
) {
    let (_dir, store) = coord_with_store();
    let coordinator = TransactionCoordinator::new(store.clone());
    let open = coordinator
        .begin(
            sample_app_id(),
            zup_core::SelectedScope::User,
            sample_version(),
            compile_deployment(&chain_input()).unwrap(),
        )
        .unwrap();
    let mut original = FakeExecutor::new()
        .fail_on_apply(6)
        .reconcile_with(ReconcileResult::Ambiguous);
    let blocked = open
        .execute(&mut original, &common::Publisher::default())
        .unwrap();
    assert_eq!(blocked.outcome, TransactionOutcome::RecoveryRequired);
    let failure = blocked.record.failure.clone();
    let mut retry = FakeExecutor::new().reconcile_with(evidence);
    let (record, outcome) = recover(
        blocked.record,
        &store,
        &mut retry,
        &common::Publisher::default(),
    )
    .unwrap();
    assert_eq!(outcome, expected);
    assert_eq!(record.failure, failure);
    assert!(!retry.reconcile_calls.is_empty());
    assert!(retry.applied.is_empty());
    if expected == TransactionOutcome::RecoveryRequired {
        assert!(retry.rolled_back.is_empty());
    }
    record.validate().unwrap();
}

#[rstest]
#[case::nothing_checked_yet(0)]
#[case::first_node_already_checked(1)]
#[case::all_work_already_checked(3)]
fn recovery_rechecks_payload_without_replaying_completed_work(#[case] resume_from: usize) {
    let (_dir, store) = coord_with_store();
    let record = TransactionRecord::new(
        TransactionId::new_v7(),
        sample_app_id(),
        zup_core::SelectedScope::User,
        sample_version(),
        sample_plan(),
    );
    store.create(&record).unwrap();
    let mut reserved = record.clone();
    common::reserve(&store, &mut reserved);
    let (mut crashed, checked) = mid_verification(&reserved);
    let verified = checked
        .iter()
        .filter(|node| node.kind.requires_verification())
        .cloned()
        .collect::<Vec<_>>();
    assert!(verified.len() >= resume_from);

    for node in verified.iter().take(resume_from) {
        let Some(NodeState::Applied { receipt }) = crashed.nodes.get(&node.id).cloned() else {
            panic!("{} was applied before the crash", node.id);
        };
        crashed
            .nodes
            .insert(node.id.clone(), NodeState::Verified { receipt });
    }
    let revision = crashed.revision;
    crashed.touch();
    store.compare_and_swap(revision, &crashed).unwrap();

    let mut exec = FakeExecutor::new();
    let (final_record, outcome) =
        recover(crashed, &store, &mut exec, &common::Publisher::default()).unwrap();

    assert_eq!(outcome, TransactionOutcome::Committed);
    assert_eq!(final_record.phase, TransactionPhase::Committed);
    for node in &verified {
        assert!(
            matches!(
                final_record.nodes.get(&node.id),
                Some(NodeState::Verified { .. })
            ),
            "{} is verified after recovery",
            node.id
        );
    }
    let expected: std::collections::BTreeSet<_> = verified
        .iter()
        .enumerate()
        .filter(|(index, node)| {
            *index >= resume_from
                || matches!(
                    node.kind,
                    NodeKind::FileMutation { .. } | NodeKind::FileRemoval { .. }
                )
        })
        .map(|(_, node)| node.id.as_str().to_owned())
        .collect();
    assert_eq!(
        exec.verified
            .iter()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>(),
        expected
    );
    assert_eq!(
        exec.applied,
        vec!["ctrl:verify".to_owned()],
        "only the remaining barriers run; no work is repeated"
    );
    assert_eq!(
        final_record.nodes.get(&barrier("ctrl:verify")).cloned(),
        Some(NodeState::Applied {
            receipt: Box::new(zup_transaction::OperationReceipt::Control)
        })
    );
}

#[test]
fn interrupted_rollback_never_rolls_forward() {
    let (_dir, store) = coord_with_store();
    let record = TransactionRecord::new(
        TransactionId::new_v7(),
        sample_app_id(),
        zup_core::SelectedScope::User,
        sample_version(),
        compile_deployment(&chain_input()).unwrap(),
    );
    store.create(&record).unwrap();
    let mut crashed = record.clone();
    common::reserve(&store, &mut crashed);
    let op = first_op(&crashed);
    crashed.phase = TransactionPhase::RollingBack;
    let node = crashed
        .plan
        .nodes
        .iter()
        .find(|node| node.id == op)
        .unwrap();
    let receipt =
        zup_transaction::OperationExecutor::apply(&mut FakeExecutor::new(), node, &crashed)
            .unwrap();
    crashed.nodes.insert(
        op,
        NodeState::RollingBack {
            receipt: Box::new(receipt),
        },
    );
    for barrier in ["ctrl:begin", "ctrl:preflight", "ctrl:commit-intent"] {
        crashed.nodes.insert(
            OperationId::new(barrier),
            NodeState::Applied {
                receipt: Box::new(zup_transaction::OperationReceipt::Control),
            },
        );
    }
    let revision = crashed.revision;
    crashed.touch();
    store.compare_and_swap(revision, &crashed).unwrap();

    let mut exec = FakeExecutor::new().reconcile_with(ReconcileResult::Applied);
    let (final_record, outcome) =
        recover(crashed, &store, &mut exec, &common::Publisher::default()).unwrap();
    assert_eq!(outcome, TransactionOutcome::RecoveryRequired);
    assert_eq!(
        final_record.phase,
        TransactionPhase::RecoveryRequired {
            resume: RecoveryPhase::RollingBack
        }
    );
    assert!(exec.applied.is_empty(), "no further mutation");
    assert!(exec.rolled_back.is_empty());
}

/// A node found `Running` was interrupted mid-apply, so the side effect may or
/// may not have landed. The reconcile answer decides: retry, adopt, or stop and
/// ask a human - and whichever it is, the side effect must not happen twice.
#[rstest]
#[case::not_applied_retries(
    ReconcileResult::NotApplied,
    TransactionOutcome::Committed,
    TransactionPhase::Committed,
    1
)]
#[case::incomplete_resumes(
    ReconcileResult::Incomplete,
    TransactionOutcome::Committed,
    TransactionPhase::Committed,
    1
)]
#[case::applied_skips_the_duplicate(
    ReconcileResult::Applied,
    TransactionOutcome::Committed,
    TransactionPhase::Committed,
    0
)]
#[case::ambiguous_escalates(
    ReconcileResult::Ambiguous,
    TransactionOutcome::RecoveryRequired,
    TransactionPhase::RecoveryRequired { resume: RecoveryPhase::Applying },
    0
)]
fn crash_running_reconciles_without_double_applying(
    #[case] answer: ReconcileResult,
    #[case] expected: TransactionOutcome,
    #[case] phase: TransactionPhase,
    #[case] reapply: usize,
) {
    let (_dir, store) = coord_with_store();
    let record = TransactionRecord::new(
        TransactionId::new_v7(),
        sample_app_id(),
        zup_core::SelectedScope::User,
        sample_version(),
        sample_plan(),
    );
    store.create(&record).unwrap();

    let mut crashed = record.clone();
    common::reserve(&store, &mut crashed);
    let op_id = first_op(&crashed);
    crashed.nodes.insert(op_id.clone(), NodeState::Running);
    crashed.phase = TransactionPhase::Applying;
    crashed.touch();
    store
        .compare_and_swap(crashed.revision - 1, &crashed)
        .unwrap();

    let mut exec = FakeExecutor::new().reconcile_with(answer);
    let (final_record, outcome) =
        recover(crashed, &store, &mut exec, &common::Publisher::default()).unwrap();

    assert_eq!(outcome, expected);
    assert_eq!(final_record.phase, phase);
    assert_eq!(exec.reconcile_calls, vec![op_id.to_string()]);
    assert_eq!(
        exec.applied
            .iter()
            .filter(|id| *id == op_id.as_str())
            .count(),
        reapply,
        "an interrupted node is never applied twice: {:?}",
        exec.labels()
    );
    if reapply > 0 {
        assert!(matches!(
            final_record.nodes.get(&op_id),
            Some(NodeState::Applied { .. })
        ));
    }
}

#[test]
fn rollback_order_is_reverse_dependency() {
    let plan = compile_deployment(&chain_input()).unwrap();
    let mut mutations: Vec<_> = plan
        .nodes
        .iter()
        .filter(|n| matches!(n.kind, NodeKind::FileMutation { .. }))
        .map(|n| n.id.clone())
        .collect();
    mutations.sort_by(|a, b| {
        let pa = plan.execution_order.iter().position(|id| id == a).unwrap();
        let pb = plan.execution_order.iter().position(|id| id == b).unwrap();
        pa.cmp(&pb)
    });
    let rb: Vec<_> = plan
        .rollback_order
        .iter()
        .filter(|id| mutations.contains(id))
        .cloned()
        .collect();
    let mut expect = mutations.clone();
    expect.reverse();
    assert_eq!(rb, expect, "rollback is reverse execution order");
}

#[test]
fn oversized_backend_payload_is_rejected() {
    let mut input = sample_input();
    input.backend_operations[0].payload = vec![0; zup_plan::MAX_BACKEND_PAYLOAD_BYTES + 1];
    let err = compile_deployment(&input).unwrap_err();
    assert!(matches!(
        err,
        zup_plan::DeploymentPlanError::InvalidInput(_)
    ));
}

#[test]
fn releasing_a_prepared_transaction_settles_the_journal() {
    let (dir, coordinator) = coord();
    let plan = compile_deployment(&sample_input()).unwrap();
    let open = coordinator
        .begin(
            sample_app_id(),
            zup_core::SelectedScope::User,
            sample_version(),
            plan,
        )
        .unwrap();
    let mut executor = FakeExecutor::default();
    let settled = open.release(&mut executor).unwrap();
    let outcome = settled.outcome;
    let settled = settled.record;
    assert_eq!(outcome, TransactionOutcome::RolledBack);
    assert_eq!(settled.phase, TransactionPhase::RolledBack);
    assert!(executor.applied.is_empty(), "release applies no work");
    let reloaded = FilesystemTransactionStore::private_at(dir.path().join("state"))
        .load(&settled.transaction_id)
        .unwrap();
    assert_eq!(reloaded.phase, TransactionPhase::RolledBack);
}

#[test]
fn failed_payload_verification_prevents_native_exposure() {
    let (_directory, coordinator) = coord();
    let plan = sample_plan();
    let native = plan
        .nodes
        .iter()
        .find(|node| matches!(node.kind, NodeKind::BackendOperation { .. }))
        .unwrap()
        .id
        .clone();
    let open = coordinator
        .begin(
            sample_app_id(),
            zup_core::SelectedScope::User,
            sample_version(),
            plan,
        )
        .unwrap();
    let mut executor = FakeExecutor::new().fail_on_verify(0);
    let settled = open
        .execute(&mut executor, &common::Publisher::default())
        .unwrap();
    assert_eq!(settled.outcome, TransactionOutcome::RolledBack);
    assert_eq!(executor.position("apply", &native), None);
}

#[test]
fn failed_repair_never_compensates_with_the_modified_file() {
    let (_directory, store) = coord_with_store();
    let coordinator = TransactionCoordinator::new(store.clone());
    let mut input = common::effects();
    let mut file = common::file("app.exe", b"authenticated");
    file.kind = zup_plan::FileOperationKind::RepairOwned;
    file.precondition = zup_plan::FilePrecondition::Exact {
        object: common::owned_instance().object,
        size: 8,
        sha256: common::digest(b"modified"),
    };
    input.files.push(file);
    let plan = compile_deployment(&input).unwrap();
    let repair = plan
        .nodes
        .iter()
        .find(|node| matches!(node.kind, NodeKind::FileMutation { .. }))
        .unwrap()
        .id
        .clone();
    let open = coordinator
        .begin(
            sample_app_id(),
            zup_core::SelectedScope::User,
            sample_version(),
            plan,
        )
        .unwrap();
    let mut executor = FakeExecutor::new().fail_on_verify(0);
    let settled = open
        .execute(&mut executor, &common::Publisher::default())
        .unwrap();
    assert_eq!(settled.outcome, TransactionOutcome::RecoveryRequired);
    assert_eq!(executor.position("rollback", &repair), None);
    assert_eq!(
        settled.record.rollback_failure.as_ref().unwrap().operation,
        Some(repair.clone())
    );
    let mut executor = FakeExecutor::new();
    let (_, outcome) = recover(
        settled.record,
        &store,
        &mut executor,
        &common::Publisher::default(),
    )
    .unwrap();
    assert_eq!(outcome, TransactionOutcome::RecoveryRequired);
    assert_eq!(executor.position("rollback", &repair), None);
}

fn blocked_repair(store: &FilesystemTransactionStore) -> TransactionRecord {
    let mut input = common::effects();
    let mut file = common::file("app.exe", b"authenticated");
    file.kind = zup_plan::FileOperationKind::RepairOwned;
    file.precondition = zup_plan::FilePrecondition::Exact {
        object: common::owned_instance().object,
        size: 8,
        sha256: common::digest(b"modified"),
    };
    input.files.push(file);
    TransactionCoordinator::new(store.clone())
        .begin(
            sample_app_id(),
            zup_core::SelectedScope::User,
            sample_version(),
            compile_deployment(&input).unwrap(),
        )
        .unwrap()
        .execute(
            &mut FakeExecutor::new().fail_on_verify(0),
            &common::Publisher::default(),
        )
        .unwrap()
        .record
}

#[rstest]
#[case::transaction(0)]
#[case::app(1)]
#[case::scope(2)]
#[case::plan(3)]
#[case::revision(4)]
#[case::evidence(5)]
#[case::native_readback(6)]
fn forward_resolution_rejects_unbound_or_unverified_authority(#[case] mismatch: u8) {
    use zup_transaction::{RecoveryRequest, assess_recovery, recover_requested};
    let (_root, store) = coord_with_store();
    let record = blocked_repair(&store);
    let mut executor = FakeExecutor::new();
    let mut decision = assess_recovery(&record, &mut executor)
        .unwrap()
        .complete_forward
        .unwrap();
    match mismatch {
        0 => decision.transaction = TransactionId::new_v7(),
        1 => decision.app = zup_core::AppId::new("com.other.app").unwrap(),
        2 => decision.scope = zup_core::SelectedScope::Machine,
        3 => decision.plan = common::digest(b"wrong plan"),
        4 => decision.revision += 1,
        5 => decision.evidence = common::digest(b"wrong evidence"),
        _ => executor.fail_verify_at = Some(executor.verified.len()),
    }
    assert!(
        recover_requested(
            record.clone(),
            &store,
            &mut executor,
            &common::Publisher::default(),
            &RecoveryRequest::CompleteForward {
                decision: Box::new(decision)
            }
        )
        .is_err()
    );
    assert_eq!(store.load(&record.transaction_id).unwrap(), record);
    assert!(executor.rolled_back.is_empty());
}

#[test]
fn explicit_forward_resolution_publishes_and_remains_replayable() {
    use zup_transaction::{RecoveryRequest, assess_recovery, recover_requested};
    let (_root, store) = coord_with_store();
    let record = blocked_repair(&store);
    assert!(!record.nodes.values().any(NodeState::is_rolled_back));
    let mut executor = FakeExecutor::new();
    let decision = assess_recovery(&record, &mut executor)
        .unwrap()
        .complete_forward
        .unwrap();
    let request = RecoveryRequest::CompleteForward {
        decision: Box::new(decision),
    };
    let publisher = common::Publisher::default();
    let (record, outcome) =
        recover_requested(record, &store, &mut executor, &publisher, &request).unwrap();
    assert_eq!(
        outcome,
        TransactionOutcome::Committed,
        "{:?} {:?}",
        record.recovery_failure,
        record.nodes
    );
    record.validate().unwrap();
    let published = publisher.0.lock().unwrap().clone().unwrap();
    zup_transaction::verify_published_installation(&record, &published).unwrap();
    assert_eq!(published.commit.transaction, record.transaction_id);
    assert!(executor.rolled_back.is_empty());
    let (_, outcome) =
        recover_requested(record, &store, &mut executor, &publisher, &request).unwrap();
    assert_eq!(outcome, TransactionOutcome::Committed);
}

#[test]
fn assessment_does_not_mutate_the_journal_or_grant_authority() {
    use zup_transaction::{RecoveryRequest, recover_requested};
    let (_root, store) = coord_with_store();
    let record = blocked_repair(&store);
    let mut executor = FakeExecutor::new();
    let (preview, outcome) = recover_requested(
        record.clone(),
        &store,
        &mut executor,
        &common::Publisher::default(),
        &RecoveryRequest::Assess {},
    )
    .unwrap();
    let TransactionOutcome::Assessed(assessment) = outcome else {
        panic!("assessment expected");
    };
    assert_eq!(preview, record);
    assert!(assessment.complete_forward.is_some());
    assert_eq!(store.load(&record.transaction_id).unwrap(), record);
    assert!(executor.applied.is_empty());
}

#[rstest]
#[case::intent(0)]
#[case::resume(1)]
#[case::receipt(3)]
#[case::publication_requested(5)]
#[case::publication_received(6)]
#[case::committed(7)]
fn durable_forward_intent_survives_interruption_without_reauthorization(
    #[case] crash_write: usize,
) {
    use zup_transaction::{RecoveryRequest, StoreError};
    struct Interrupt {
        store: FilesystemTransactionStore,
        remaining: std::sync::atomic::AtomicUsize,
    }
    impl TransactionStore for Interrupt {
        fn reserve_installation(
            &self,
            record: &TransactionRecord,
        ) -> Result<zup_transaction::InstallationReservation, StoreError> {
            self.store.reserve_installation(record)
        }
        fn create(&self, record: &TransactionRecord) -> Result<(), StoreError> {
            self.store.create(record)
        }
        fn load(&self, id: &TransactionId) -> Result<TransactionRecord, StoreError> {
            self.store.load(id)
        }
        fn compare_and_swap(
            &self,
            revision: u64,
            record: &TransactionRecord,
        ) -> Result<(), StoreError> {
            self.store.compare_and_swap(revision, record)?;
            if record.resolution.is_some()
                && self
                    .remaining
                    .fetch_sub(1, std::sync::atomic::Ordering::SeqCst)
                    == 0
            {
                panic!("injected interruption after durable write");
            }
            Ok(())
        }
    }
    let (_root, store) = coord_with_store();
    let record = blocked_repair(&store);
    let id = record.transaction_id;
    let decision = zup_transaction::assess_recovery(&record, &mut FakeExecutor::new())
        .unwrap()
        .complete_forward
        .unwrap();
    let publisher = common::Publisher::default();
    let interrupted = Interrupt {
        store: store.clone(),
        remaining: crash_write.into(),
    };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        zup_transaction::recover_requested(
            record,
            &interrupted,
            &mut FakeExecutor::new(),
            &publisher,
            &RecoveryRequest::CompleteForward {
                decision: Box::new(decision),
            },
        )
    }));
    assert!(result.is_err());
    let durable = store.load(&id).unwrap();
    assert!(durable.resolution.is_some());
    let (record, outcome) = recover(durable, &store, &mut FakeExecutor::new(), &publisher).unwrap();
    assert_eq!(
        outcome,
        TransactionOutcome::Committed,
        "{:?}",
        record.recovery_failure
    );
    record.validate().unwrap();
}

#[test]
fn destructive_or_untyped_recovery_requests_are_not_authority() {
    for request in [
        r#"{"action":"discard"}"#,
        r#"{"action":"force_rollback"}"#,
        r#"{"action":"restore_data","backup":"C:\\backup"}"#,
        r#"{"action":"retry","force":true}"#,
    ] {
        assert!(serde_json::from_str::<zup_transaction::RecoveryRequest>(request).is_err());
    }
}

#[test]
fn accepted_forward_intent_reobserves_native_evidence_after_publication_request() {
    use zup_transaction::{InstallationPublisher, RecoveryRequest};
    struct Deferred {
        durable: common::Publisher,
        refuse: std::sync::atomic::AtomicBool,
    }
    impl InstallationPublisher for Deferred {
        type Error = String;
        fn prepare(
            &self,
            record: &TransactionRecord,
        ) -> Result<zup_plan::InstallationRecord, String> {
            self.durable.prepare(record)
        }
        fn publish(
            &self,
            record: &TransactionRecord,
        ) -> Result<zup_plan::InstallationRecord, String> {
            if self.refuse.load(std::sync::atomic::Ordering::SeqCst) {
                Err("publication deferred".into())
            } else {
                self.durable.publish(record)
            }
        }
    }
    let (_root, store) = coord_with_store();
    let record = blocked_repair(&store);
    let decision = zup_transaction::assess_recovery(&record, &mut FakeExecutor::new())
        .unwrap()
        .complete_forward
        .unwrap();
    let publisher = Deferred {
        durable: Default::default(),
        refuse: true.into(),
    };
    let (pending, outcome) = zup_transaction::recover_requested(
        record,
        &store,
        &mut FakeExecutor::new(),
        &publisher,
        &RecoveryRequest::CompleteForward {
            decision: Box::new(decision),
        },
    )
    .unwrap();
    assert_eq!(outcome, TransactionOutcome::RecoveryRequired);
    assert!(matches!(
        pending.publication,
        zup_transaction::PublicationState::Requested { .. }
    ));
    publisher
        .refuse
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let (pending, outcome) = recover(
        pending,
        &store,
        &mut FakeExecutor::new().fail_on_verify(0),
        &publisher,
    )
    .unwrap();
    assert_eq!(outcome, TransactionOutcome::RecoveryRequired);
    assert!(publisher.durable.0.lock().unwrap().is_none());
    let (record, outcome) = recover(pending, &store, &mut FakeExecutor::new(), &publisher).unwrap();
    assert_eq!(
        outcome,
        TransactionOutcome::Committed,
        "{:?}",
        record.recovery_failure
    );
}

#[test]
fn forward_authority_cannot_be_reinterpreted_as_compensation() {
    let (_root, store) = coord_with_store();
    let mut record = blocked_repair(&store);
    record.resolution = zup_transaction::assess_recovery(&record, &mut FakeExecutor::new())
        .unwrap()
        .complete_forward;
    record.touch();
    record.validate().unwrap();
    record.phase = TransactionPhase::RollingBack;
    assert!(record.validate().is_err());
}
