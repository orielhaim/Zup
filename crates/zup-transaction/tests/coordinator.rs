//! Coordinator execution, rollback, crash injection, and recovery tests.

mod common;
mod fake;

use common::{chain_input, sample_app_id, sample_input, sample_plan, sample_version};
use fake::FakeExecutor;
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

#[test]
fn execute_commits_all_nodes() {
    let (_dir, coord) = coord();
    let record = coord
        .begin(
            sample_app_id(),
            zup_core::SelectedScope::User,
            sample_version(),
            sample_plan(),
        )
        .unwrap();
    let mut exec = FakeExecutor::new();
    let (final_record, outcome) = coord.execute(record, &mut exec).unwrap();
    assert_eq!(outcome, TransactionOutcome::Committed);
    assert_eq!(final_record.phase, TransactionPhase::Committed);
    assert!(exec.applied.iter().any(|id| id.contains("a.exe")));
    assert!(exec.applied.iter().any(|id| id.contains("b.dll")));
    assert!(exec.applied.iter().any(|id| id.contains("fake.backend")));
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

#[test]
fn commit_requires_every_mutation_verified() {
    let (_dir, coord) = coord();
    let record = coord
        .begin(
            sample_app_id(),
            zup_core::SelectedScope::User,
            sample_version(),
            sample_plan(),
        )
        .unwrap();
    let verified = record
        .plan
        .nodes
        .iter()
        .filter(|node| node.kind.requires_verification())
        .map(|node| node.id.clone())
        .collect::<Vec<_>>();
    assert!(!verified.is_empty());
    let mut exec = FakeExecutor::new();
    let (final_record, outcome) = coord.execute(record, &mut exec).unwrap();
    assert_eq!(outcome, TransactionOutcome::Committed);
    for id in &verified {
        assert!(
            matches!(final_record.nodes.get(id), Some(NodeState::Verified { .. })),
            "{id} must be verified before commit"
        );
    }
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

#[test]
fn verification_runs_after_every_mutation_and_before_commit() {
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
    let last_verify = verified_nodes.last().expect("has verified node").clone();

    let mut exec = FakeExecutor::new();
    let (_, outcome) = coord.execute(record, &mut exec).unwrap();
    assert_eq!(outcome, TransactionOutcome::Committed);

    assert_eq!(
        exec.verified,
        verified_applies(&exec),
        "every applied mutation is verified exactly once, in execution order"
    );
    let crossed_verify = exec.position("apply", &barrier("ctrl:verify")).unwrap();
    let crossed_commit = exec.position("apply", &barrier("ctrl:commit")).unwrap();
    let verified_last = exec.position("verify", &last_verify).unwrap();
    assert!(
        verified_last < crossed_verify,
        "all verification precedes the verify barrier: {:?}",
        exec.labels()
    );
    assert!(
        crossed_verify < crossed_commit,
        "the commit barrier comes after verification: {:?}",
        exec.labels()
    );
    for id in &verified_nodes {
        assert!(
            exec.position("apply", id).unwrap() < exec.position("verify", id).unwrap(),
            "{id} is verified after it is applied"
        );
    }
}

#[test]
fn prepare_failure_rolls_back_applied_work() {
    let (_dir, coord) = coord();
    let record = coord
        .begin(
            sample_app_id(),
            zup_core::SelectedScope::User,
            sample_version(),
            sample_plan(),
        )
        .unwrap();
    // The second prepare is the preflight barrier, after staging has landed.
    let mut exec = FakeExecutor::new().fail_on_prepare(1);
    let (final_record, outcome) = coord.execute(record, &mut exec).unwrap();
    assert_eq!(outcome, TransactionOutcome::RolledBack);
    assert_eq!(final_record.phase, TransactionPhase::RolledBack);
    let staged = mutating_applies(&exec);
    assert!(
        !staged.is_empty() && staged.iter().all(|id| id.starts_with("op:stage-file:")),
        "only staging landed before the preflight barrier failed: {:?}",
        exec.labels()
    );
    assert_eq!(
        exec.rolled_back,
        staged.iter().rev().cloned().collect::<Vec<_>>(),
        "staged work is undone in reverse order"
    );
    assert!(exec.verified.is_empty());
    assert_eq!(
        final_record
            .nodes
            .get(&barrier("ctrl:preflight"))
            .cloned()
            .unwrap(),
        NodeState::Pending,
        "a barrier whose prepare failed was never crossed"
    );
}

#[test]
fn prepare_failure_before_any_work_still_rolls_back() {
    let (_dir, coord) = coord();
    let record = coord
        .begin(
            sample_app_id(),
            zup_core::SelectedScope::User,
            sample_version(),
            sample_plan(),
        )
        .unwrap();
    let mut exec = FakeExecutor::new().fail_on_prepare(0);
    let (final_record, outcome) = coord.execute(record, &mut exec).unwrap();
    assert_eq!(outcome, TransactionOutcome::RolledBack);
    assert_eq!(final_record.phase, TransactionPhase::RolledBack);
    assert!(
        exec.applied.is_empty(),
        "no side effect: {:?}",
        exec.labels()
    );
    assert!(exec.rolled_back.is_empty());
    assert_eq!(
        final_record
            .nodes
            .get(&barrier("ctrl:begin"))
            .cloned()
            .unwrap(),
        NodeState::Pending
    );
}

#[test]
fn verify_failure_rolls_back_every_applied_mutation() {
    let (_dir, coord) = coord();
    let record = coord
        .begin(
            sample_app_id(),
            zup_core::SelectedScope::User,
            sample_version(),
            sample_plan(),
        )
        .unwrap();
    let mut exec = FakeExecutor::new().fail_on_verify(0);
    let (final_record, outcome) = coord.execute(record, &mut exec).unwrap();
    assert_eq!(outcome, TransactionOutcome::RolledBack);
    assert_eq!(final_record.phase, TransactionPhase::RolledBack);
    let attempted = verify_attempts(&exec);
    assert_eq!(attempted.len(), 1, "verification stops at the failure");
    assert!(exec.verified.is_empty(), "nothing was verified");
    let applied = mutating_applies(&exec);
    assert_eq!(
        exec.rolled_back,
        applied.iter().rev().cloned().collect::<Vec<_>>(),
        "every applied mutation is undone, including unverified ones"
    );
    for id in &applied {
        assert_eq!(
            final_record.nodes.get(&OperationId::new(id)).cloned(),
            Some(NodeState::RolledBack),
            "{id} is undone"
        );
    }
    assert_eq!(
        final_record
            .nodes
            .get(&barrier("ctrl:verify"))
            .cloned()
            .unwrap(),
        NodeState::Pending,
        "a failed verification never crosses the verify barrier"
    );
    assert_eq!(
        final_record
            .nodes
            .get(&barrier("ctrl:commit"))
            .cloned()
            .unwrap(),
        NodeState::Pending
    );
}

#[test]
fn failure_rolls_back_in_reverse_order() {
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
    let mut exec = FakeExecutor::new().fail_on_apply(6);
    let (final_record, outcome) = coord.execute(record, &mut exec).unwrap();
    assert_eq!(outcome, TransactionOutcome::RolledBack);
    assert_eq!(final_record.phase, TransactionPhase::RolledBack);
    let applied = mutating_applies(&exec);
    assert!(
        applied.iter().all(|id| id.starts_with("op:stage-file:")),
        "the first file mutation failed: {applied:?}"
    );
    assert_eq!(
        exec.rolled_back,
        applied.iter().rev().cloned().collect::<Vec<_>>()
    );
    assert!(applied.len() >= 2);
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

#[test]
fn verification_crash_resumes_without_repeating_completed_checks() {
    let (_dir, store) = coord_with_store();
    let record = TransactionRecord::new(
        TransactionId::new_v7(),
        sample_app_id(),
        zup_core::SelectedScope::User,
        sample_version(),
        sample_plan(),
    );
    store.create(&record).unwrap();

    // A crash in the middle of the verification pass: the first checked node
    // is already verified, the rest are applied, and the verify barrier is
    // in flight.
    let (mut crashed, checked) = mid_verification(&record);
    let verified = checked
        .iter()
        .filter(|node| node.kind.requires_verification())
        .cloned()
        .collect::<Vec<_>>();
    assert!(verified.len() >= 2);
    let first = verified[0].id.clone();
    let Some(NodeState::Applied { receipt }) = crashed.nodes.get(&first).cloned() else {
        panic!("fixture starts applied");
    };
    crashed.nodes.insert(first, NodeState::Verified { receipt });
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
        verified[1..]
            .iter()
            .map(|node| node.id.as_str().to_owned())
            .collect::<Vec<_>>(),
        "the already-verified node is never verified twice: {:?}",
        exec.labels()
    );
    assert_eq!(
        exec.applied,
        vec!["ctrl:verify".to_owned(), "ctrl:commit".to_owned()],
        "only the remaining barriers run"
    );
    assert_eq!(
        final_record
            .nodes
            .get(&barrier("ctrl:verify"))
            .cloned()
            .unwrap(),
        NodeState::Applied {
            receipt: Box::new(zup_transaction::OperationReceipt::Control)
        }
    );
    assert_eq!(
        final_record
            .nodes
            .get(&barrier("ctrl:commit"))
            .cloned()
            .unwrap(),
        NodeState::Applied {
            receipt: Box::new(zup_transaction::OperationReceipt::Control)
        }
    );
}

#[test]
fn verification_crash_before_any_check_re_verifies_everything() {
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
    let revision = crashed.revision;
    crashed.touch();
    store.compare_and_swap(revision, &crashed).unwrap();

    let mut exec = FakeExecutor::new();
    let (final_record, outcome) = recover(crashed, &store, &mut exec).unwrap();
    assert_eq!(outcome, TransactionOutcome::Committed);
    assert_eq!(
        exec.verified,
        verified
            .iter()
            .map(|node| node.id.as_str().to_owned())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        exec.applied,
        vec!["ctrl:verify".to_owned(), "ctrl:commit".to_owned()],
        "no work is repeated"
    );
    assert_eq!(final_record.phase, TransactionPhase::Committed);
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

#[test]
fn crash_running_reconcile_not_applied_retries() {
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

    let mut exec = FakeExecutor::new().reconcile_with(ReconcileResult::NotApplied);
    let (final_record, outcome) = recover(crashed, &store, &mut exec).unwrap();
    assert_eq!(outcome, TransactionOutcome::Committed);
    assert!(exec.reconcile_calls.contains(&op_id.to_string()));
    assert!(matches!(
        final_record.nodes.get(&op_id),
        Some(NodeState::Applied { .. })
    ));
}

#[test]
fn crash_running_reconcile_applied_skips_duplicate() {
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

    let mut exec = FakeExecutor::new().reconcile_with(ReconcileResult::Applied);
    let (final_record, _) = recover(crashed, &store, &mut exec).unwrap();
    assert!(matches!(
        final_record.nodes.get(&op_id),
        Some(NodeState::Applied { .. })
    ));
    let apply_count = exec
        .applied
        .iter()
        .filter(|id| *id == op_id.as_str())
        .count();
    assert_eq!(
        apply_count, 0,
        "must not re-apply a reconciled Applied node"
    );
}

#[test]
fn crash_running_reconcile_ambiguous_needs_recovery() {
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

    let mut exec = FakeExecutor::new().reconcile_with(ReconcileResult::Ambiguous);
    let (final_record, outcome) = recover(crashed, &store, &mut exec).unwrap();
    assert_eq!(outcome, TransactionOutcome::RecoveryRequired);
    assert_eq!(final_record.phase, TransactionPhase::RecoveryRequired);
    assert!(exec.applied.is_empty(), "no further mutations");
}

#[test]
fn crash_before_commit_intent_abandons() {
    let (_dir, store) = coord_with_store();
    let record = TransactionRecord::new(
        TransactionId::new_v7(),
        sample_app_id(),
        zup_core::SelectedScope::User,
        sample_version(),
        sample_plan(),
    );
    store.create(&record).unwrap();
    let mut exec = FakeExecutor::new();
    let (final_record, outcome) = recover(record, &store, &mut exec).unwrap();
    assert_eq!(outcome, TransactionOutcome::RolledBack);
    assert!(exec.applied.is_empty());
    assert_eq!(final_record.phase, TransactionPhase::RolledBack);
}

#[test]
fn crash_after_prepared_persisted() {
    let (_dir, store) = coord_with_store();
    let record = TransactionRecord::new(
        TransactionId::new_v7(),
        sample_app_id(),
        zup_core::SelectedScope::User,
        sample_version(),
        sample_plan(),
    );
    store.create(&record).unwrap();
    // record is Prepared with all nodes Pending — crash here means abandon.
    let mut exec = FakeExecutor::new();
    let (_, outcome) = recover(record, &store, &mut exec).unwrap();
    assert_eq!(outcome, TransactionOutcome::RolledBack);
}

#[test]
fn crash_after_running_persisted_before_apply() {
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
    crashed.nodes.insert(op_id, NodeState::Running);
    crashed.phase = TransactionPhase::Applying;
    crashed.touch();
    store.compare_and_swap(record.revision, &crashed).unwrap();
    // Reconcile says NotApplied → retry → Committed (tested above).
}

#[test]
fn crash_after_apply_before_applied_persisted() {
    // Same durable shape as Running-before-apply: node = Running.
    // Reconcile Applied stands in for "apply landed, receipt lost".
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
    let mut exec = FakeExecutor::new().reconcile_with(ReconcileResult::Applied);
    let (final_record, outcome) = recover(crashed, &store, &mut exec).unwrap();
    assert_eq!(outcome, TransactionOutcome::Committed);
    assert!(matches!(
        final_record.nodes.get(&op_id),
        Some(NodeState::Applied { .. })
    ));
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
    assert_eq!(
        serde_json::to_string(&a).unwrap(),
        serde_json::to_string(&b).unwrap()
    );
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

#[test]
fn transaction_record_roundtrip() {
    let plan = sample_plan();
    let record = TransactionRecord::new(
        TransactionId::new_v7(),
        sample_app_id(),
        zup_core::SelectedScope::User,
        sample_version(),
        plan,
    );
    let json = serde_json::to_string_pretty(&record).unwrap();
    let back: TransactionRecord = serde_json::from_str(&json).unwrap();
    assert_eq!(record, back);
}

#[test]
fn phase_transitions_validated() {
    use TransactionPhase::*;
    assert!(Prepared.transition(Applying).is_ok());
    assert!(Applying.transition(Committed).is_ok());
    assert!(Applying.transition(RollingBack).is_ok());
    assert!(RollingBack.transition(RolledBack).is_ok());
    assert!(Prepared.transition(Committed).is_err());
    assert!(Committed.transition(Applying).is_err());
}

#[test]
fn dot_renders_graph() {
    let plan = sample_plan();
    let dot = plan.to_dot();
    assert!(dot.starts_with("digraph"));
    assert!(dot.contains("->"));
}
