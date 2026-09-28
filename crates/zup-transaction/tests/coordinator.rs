//! Coordinator execution, rollback, crash injection, and recovery tests.

mod common;
mod fake;

use common::{chain_input, sample_app_id, sample_input, sample_plan, sample_version};
use fake::FakeExecutor;
use rstest::rstest;
use tempfile::TempDir;
use zup_transaction::{
    FilesystemTransactionStore, NodeKind, NodeState, OperationId, ReconcileResult,
    TransactionCoordinator, TransactionId, TransactionNode, TransactionOutcome, TransactionPhase,
    TransactionRecord, TransactionStore, compile_transaction, recover,
};

fn coord() -> (TempDir, TransactionCoordinator<FilesystemTransactionStore>) {
    let dir = TempDir::new().unwrap();
    let store = FilesystemTransactionStore::new(dir.path());
    (dir, TransactionCoordinator::new(store))
}

fn coord_with_store() -> (TempDir, FilesystemTransactionStore) {
    let dir = TempDir::new().unwrap();
    let store = FilesystemTransactionStore::new(dir.path());
    (dir, store)
}

fn first_op(record: &TransactionRecord) -> zup_transaction::OperationId {
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
fn mid_verification(record: &TransactionRecord) -> (TransactionRecord, Vec<TransactionNode>) {
    let mut crashed = record.clone();
    let checked = crashed
        .plan
        .nodes
        .iter()
        .filter(|node| !node.kind.is_barrier())
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
                receipt: Box::new(fake::receipt_for(node)),
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
                inner: FilesystemTransactionStore::new(root),
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
    let (final_record, outcome) = coord.execute(record, &mut exec).unwrap();

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
    let record = coord
        .begin(
            sample_app_id(),
            zup_core::SelectedScope::User,
            sample_version(),
            sample_plan(),
        )
        .unwrap();
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
    let (final_record, outcome) = coord.execute(record, &mut exec).unwrap();
    assert_eq!(outcome, TransactionOutcome::Committed);
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
    assert_eq!(
        exec.verified,
        verified_applies(&exec),
        "every applied mutation is verified exactly once, in execution order"
    );
    let crossed_verify = exec.position("apply", &barrier("ctrl:verify")).unwrap();
    let crossed_commit = exec.position("apply", &barrier("ctrl:commit")).unwrap();
    assert!(
        exec.position("verify", &last_verify).unwrap() < crossed_verify,
        "all verification precedes the verify barrier: {:?}",
        exec.labels()
    );
    assert!(
        crossed_verify < crossed_commit,
        "the commit barrier comes after verification: {:?}",
        exec.labels()
    );
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
    let record = coord
        .begin(
            sample_app_id(),
            zup_core::SelectedScope::User,
            sample_version(),
            sample_plan(),
        )
        .unwrap();
    let begin = barrier("ctrl:begin");
    let preflight = barrier("ctrl:preflight");
    let commit_intent = barrier("ctrl:commit-intent");
    let commit = barrier("ctrl:commit");
    let mutation = first_op(&record);

    let mut exec = FakeExecutor::new();
    let (_, outcome) = coord.execute(record, &mut exec).unwrap();
    assert_eq!(outcome, TransactionOutcome::Committed);

    assert_eq!(
        exec.prepared,
        vec!["ctrl:begin".to_owned(), "ctrl:preflight".to_owned()],
        "only the barriers that guard preparation are prepared"
    );
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
    assert!(
        exec.position("apply", &commit).expect("commit crossed") > first_mutation,
        "commit is the last barrier"
    );
}

/// Whatever a failure undoes, it undoes in reverse execution order, and what
/// was applied is exactly what gets rolled back.
#[rstest]
#[case::prepare_fails_before_any_work(|exec: FakeExecutor| exec.fail_on_prepare(0), 0, 0)]
#[case::preflight_prepare_fails_after_staging(|exec: FakeExecutor| exec.fail_on_prepare(1), 3, 0)]
#[case::first_file_mutation_fails(|exec: FakeExecutor| exec.fail_on_apply(6), 3, 0)]
#[case::verification_fails(|exec: FakeExecutor| exec.fail_on_verify(0), 6, 1)]
fn failure_rolls_back_applied_work_in_reverse_order(
    #[case] inject: fn(FakeExecutor) -> FakeExecutor,
    #[case] applied: usize,
    #[case] attempted_verifications: usize,
) {
    let (_dir, coord) = coord();
    let record = coord
        .begin(
            sample_app_id(),
            zup_core::SelectedScope::User,
            sample_version(),
            compile_transaction(&chain_input()).unwrap(),
        )
        .unwrap();

    let mut exec = inject(FakeExecutor::new());
    let (final_record, outcome) = coord.execute(record, &mut exec).unwrap();

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
        assert_eq!(
            final_record.nodes.get(&OperationId::new(id)).cloned(),
            Some(NodeState::RolledBack),
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

#[test]
fn rollback_failure_requires_recovery() {
    let (_dir, coord) = coord();
    let plan = compile_transaction(&chain_input()).unwrap();
    let record = coord
        .begin(
            sample_app_id(),
            zup_core::SelectedScope::User,
            sample_version(),
            plan,
        )
        .unwrap();
    let mut exec = FakeExecutor::new().fail_on_apply(6).fail_on_rollback(0);
    let (final_record, outcome) = coord.execute(record, &mut exec).unwrap();
    assert_eq!(outcome, TransactionOutcome::RecoveryRequired);
    assert_eq!(final_record.phase, TransactionPhase::RecoveryRequired);
}

/// Verification is journalled per node, so a crash inside the pass resumes at
/// the first node that is not yet `Verified` and never re-checks one that is.
/// `resume_from` is how many nodes the crashed pass had already checked.
#[rstest]
#[case::nothing_checked_yet(0)]
#[case::first_node_already_checked(1)]
fn verification_crash_resumes_where_it_stopped(#[case] resume_from: usize) {
    let (_dir, store) = coord_with_store();
    let record = TransactionRecord::new(
        TransactionId::new_v7(),
        sample_app_id(),
        zup_core::SelectedScope::User,
        sample_version(),
        sample_plan(),
    );
    store.create(&record).unwrap();
    let (mut crashed, checked) = mid_verification(&record);
    let verified = checked
        .iter()
        .filter(|node| node.kind.requires_verification())
        .cloned()
        .collect::<Vec<_>>();
    assert!(verified.len() > resume_from);

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
    let (final_record, outcome) = recover(crashed, &store, &mut exec).unwrap();

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
    assert_eq!(
        exec.verified,
        verified[resume_from..]
            .iter()
            .map(|node| node.id.as_str().to_owned())
            .collect::<Vec<_>>(),
        "an already-verified node is never verified twice: {:?}",
        exec.labels()
    );
    assert_eq!(
        exec.applied,
        vec!["ctrl:verify".to_owned(), "ctrl:commit".to_owned()],
        "only the remaining barriers run; no work is repeated"
    );
    for crossed in ["ctrl:verify", "ctrl:commit"] {
        assert_eq!(
            final_record.nodes.get(&barrier(crossed)).cloned(),
            Some(NodeState::Applied {
                receipt: Box::new(zup_transaction::OperationReceipt::Control)
            })
        );
    }
}

#[test]
fn interrupted_rollback_never_rolls_forward() {
    let (_dir, store) = coord_with_store();
    let record = TransactionRecord::new(
        TransactionId::new_v7(),
        sample_app_id(),
        zup_core::SelectedScope::User,
        sample_version(),
        compile_transaction(&chain_input()).unwrap(),
    );
    store.create(&record).unwrap();
    let mut crashed = record.clone();
    let op = first_op(&crashed);
    crashed.phase = TransactionPhase::RollingBack;
    crashed.nodes.insert(op, NodeState::RollingBack);
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
    let (final_record, outcome) = recover(crashed, &store, &mut exec).unwrap();
    assert_eq!(outcome, TransactionOutcome::RecoveryRequired);
    assert_eq!(final_record.phase, TransactionPhase::RecoveryRequired);
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
#[case::applied_skips_the_duplicate(
    ReconcileResult::Applied,
    TransactionOutcome::Committed,
    TransactionPhase::Committed,
    0
)]
#[case::ambiguous_escalates(
    ReconcileResult::Ambiguous,
    TransactionOutcome::RecoveryRequired,
    TransactionPhase::RecoveryRequired,
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
    let op_id = first_op(&crashed);
    crashed.nodes.insert(op_id.clone(), NodeState::Running);
    crashed.phase = TransactionPhase::Applying;
    crashed.touch();
    store.compare_and_swap(record.revision, &crashed).unwrap();

    let mut exec = FakeExecutor::new().reconcile_with(answer);
    let (final_record, outcome) = recover(crashed, &store, &mut exec).unwrap();

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
    let plan = compile_transaction(&chain_input()).unwrap();
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
fn plan_fingerprint_stable() {
    let a = compile_transaction(&sample_input()).unwrap();
    let b = compile_transaction(&sample_input()).unwrap();
    assert_eq!(a.fingerprint(), b.fingerprint());
}

#[test]
fn fingerprint_changes_with_target_identity() {
    let plan = sample_plan();
    let mut other = plan.clone();
    other.target = zup_core::TargetTriple::parse("x86_64-unknown-linux-gnu").unwrap();
    assert_ne!(plan.fingerprint(), other.fingerprint());
}

#[test]
fn oversized_backend_payload_is_rejected() {
    let mut input = sample_input();
    input.backend_operations[0].payload = vec![0; zup_transaction::MAX_BACKEND_PAYLOAD_BYTES + 1];
    let err = compile_transaction(&input).unwrap_err();
    assert!(matches!(
        err,
        zup_transaction::TransactionPlanError::InvalidInput(_)
    ));
}
