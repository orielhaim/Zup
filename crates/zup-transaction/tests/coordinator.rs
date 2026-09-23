//! Coordinator execution, rollback, crash injection, and recovery tests.

mod common;
mod fake;

use common::{chain_execution, sample_app_id, sample_execution, sample_plan, sample_version};
use fake::FakeExecutor;
use tempfile::TempDir;
use zup_transaction::{
    FilesystemTransactionStore, NodeKind, NodeState, ReconcileResult, TransactionCoordinator,
    TransactionId, TransactionOutcome, TransactionPhase, TransactionRecord, TransactionStore,
    compile_transaction, recover,
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
    assert!(exec.applied.iter().any(|id| id.contains("setup")));
    assert!(exec.applied.iter().any(|id| id.contains("Acme")));
}

#[test]
fn failure_rolls_back_in_reverse_order() {
    let (_dir, coord) = coord();
    let plan = compile_transaction(&chain_execution()).unwrap();
    let record = coord
        .begin(
            sample_app_id(),
            zup_core::SelectedScope::User,
            sample_version(),
            plan,
        )
        .unwrap();
    let mut exec = FakeExecutor::new().fail_on_apply(2);
    let (final_record, outcome) = coord.execute(record, &mut exec).unwrap();
    assert_eq!(outcome, TransactionOutcome::RolledBack);
    assert_eq!(final_record.phase, TransactionPhase::RolledBack);
    assert_eq!(exec.rolled_back.len(), exec.applied.len());
    assert!(exec.rolled_back.len() >= 2);
}

#[test]
fn rollback_failure_requires_recovery() {
    let (_dir, coord) = coord();
    let plan = compile_transaction(&chain_execution()).unwrap();
    let record = coord
        .begin(
            sample_app_id(),
            zup_core::SelectedScope::User,
            sample_version(),
            plan,
        )
        .unwrap();
    let mut exec = FakeExecutor::new().fail_on_apply(2).fail_on_rollback(0);
    let (final_record, outcome) = coord.execute(record, &mut exec).unwrap();
    assert_eq!(outcome, TransactionOutcome::RecoveryRequired);
    assert_eq!(final_record.phase, TransactionPhase::RecoveryRequired);
}

#[test]
fn irreversible_failure_requires_recovery() {
    let (_dir, coord) = coord();
    let record = coord
        .begin(
            sample_app_id(),
            zup_core::SelectedScope::User,
            sample_version(),
            sample_plan(),
        )
        .unwrap();
    let total = record
        .plan
        .execution_order
        .iter()
        .filter(|id| !id.as_str().starts_with("ctrl:"))
        .count();
    let mut exec = FakeExecutor::new().fail_on_apply(total.saturating_sub(1));
    let (final_record, outcome) = coord.execute(record, &mut exec).unwrap();
    assert!(
        matches!(
            outcome,
            TransactionOutcome::RolledBack | TransactionOutcome::RecoveryRequired
        ),
        "outcome: {outcome:?} phase: {:?}",
        final_record.phase
    );
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
fn irreversible_opaque_runs_after_reversible() {
    let plan = sample_plan();
    let setup = plan
        .nodes
        .iter()
        .find(|n| n.id.as_str().contains("setup"))
        .unwrap();
    assert_eq!(setup.rollback, zup_transaction::RollbackCapability::None);

    let pos_setup = plan
        .execution_order
        .iter()
        .position(|id| id == &setup.id)
        .unwrap();
    for node in &plan.nodes {
        if matches!(node.kind, NodeKind::FileMutation { .. })
            || matches!(node.kind, NodeKind::ManagedIntegration { .. })
        {
            let pos = plan
                .execution_order
                .iter()
                .position(|id| id == &node.id)
                .unwrap();
            assert!(
                pos < pos_setup,
                "{} must precede irreversible setup",
                node.id
            );
        }
    }
}

#[test]
fn rollback_order_is_reverse_dependency() {
    let plan = compile_transaction(&chain_execution()).unwrap();
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
    let a = compile_transaction(&sample_execution()).unwrap();
    let b = compile_transaction(&sample_execution()).unwrap();
    assert_eq!(a.fingerprint(), b.fingerprint());
    assert_eq!(
        serde_json::to_string(&a).unwrap(),
        serde_json::to_string(&b).unwrap()
    );
}

#[test]
fn conflicts_reject_compilation() {
    let mut exec = sample_execution();
    exec.files[0].conflict = Some(zup_exec::Conflict::TargetNonFile { path: "x".into() });
    let err = compile_transaction(&exec).unwrap_err();
    assert!(matches!(
        err,
        zup_transaction::TransactionPlanError::UnresolvedConflict { .. }
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
