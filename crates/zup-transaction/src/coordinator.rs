use miette::Diagnostic;
use semver::Version;
use thiserror::Error;
use tracing::{info, warn};
use zup_core::{AppId, SelectedScope};

use crate::executor::{OperationExecutor, OperationReceipt, ReconcileResult};
use crate::id::{OperationId, TransactionId};
use crate::plan::{NodeKind, Phase, TransactionNode, TransactionPlan};
use crate::record::{NodeState, StoreError, TransactionPhase, TransactionRecord};
use crate::storage::TransactionStore;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransactionOutcome {
    Committed,
    RolledBack,
    RecoveryRequired,
}

#[derive(Debug, Error, Diagnostic)]
pub enum TransactionError {
    #[error("store error: {0}")]
    #[diagnostic(code(zup_transaction::store))]
    Store(#[from] StoreError),

    #[error("executor failed on `{operation}`: {message}")]
    #[diagnostic(code(zup_transaction::executor))]
    Executor { operation: String, message: String },

    #[error("invalid journal state: {0}")]
    #[diagnostic(code(zup_transaction::invalid_state))]
    InvalidState(String),
}

pub struct TransactionCoordinator<S: TransactionStore> {
    store: S,
}

impl<S: TransactionStore> TransactionCoordinator<S> {
    pub fn new(store: S) -> Self {
        Self { store }
    }

    pub fn begin(
        &self,
        app_id: AppId,
        scope: SelectedScope,
        app_version: Version,
        plan: TransactionPlan,
    ) -> Result<TransactionRecord, TransactionError> {
        let id = TransactionId::new_v7();
        let record = TransactionRecord::new(id, app_id, scope, app_version, plan);
        self.store.create(&record)?;
        info!(transaction_id = %id, plan_hash = %record.plan_hash, "transaction created");
        Ok(record)
    }

    pub fn execute<E: OperationExecutor>(
        &self,
        mut record: TransactionRecord,
        executor: &mut E,
    ) -> Result<(TransactionRecord, TransactionOutcome), TransactionError>
    where
        E::Error: std::fmt::Display,
    {
        match drive(&self.store, &mut record, executor) {
            Ok(()) => {
                commit_phase(&mut record, &self.store, TransactionPhase::Committed)?;
                info!(transaction_id = %record.transaction_id, "commit");
                Ok((record, TransactionOutcome::Committed))
            }
            Err(error) => {
                if matches!(error, TransactionError::Store(_)) {
                    // record the store never accepted would only compound it.
                    return Err(error);
                }
                warn!(transaction_id = %record.transaction_id, error = %error, "execution failed");
                let outcome = fail(&self.store, &mut record, executor)?;
                Ok((record, outcome))
            }
        }
    }
}

pub fn recover<E: OperationExecutor, S: TransactionStore>(
    mut record: TransactionRecord,
    store: &S,
    executor: &mut E,
) -> Result<(TransactionRecord, TransactionOutcome), TransactionError>
where
    E::Error: std::fmt::Display,
{
    record
        .validate()
        .map_err(|error| TransactionError::InvalidState(error.to_string()))?;
    info!(transaction_id = %record.transaction_id, phase = ?record.phase, "recovery");

    if matches!(
        record.phase,
        TransactionPhase::Committed
            | TransactionPhase::RolledBack
            | TransactionPhase::RecoveryRequired
    ) {
        let outcome = settled(record.phase);
        return Ok((record, outcome));
    }

    reconcile_running(&mut record, store, executor)?;
    if record.phase == TransactionPhase::RecoveryRequired {
        return Ok((record, TransactionOutcome::RecoveryRequired));
    }

    match record.phase {
        TransactionPhase::Applying => match drive(store, &mut record, executor) {
            Ok(()) => {
                commit_phase(&mut record, store, TransactionPhase::Committed)?;
                info!(transaction_id = %record.transaction_id, "recovery commit");
                Ok((record, TransactionOutcome::Committed))
            }
            Err(error) => {
                if matches!(error, TransactionError::Store(_)) {
                    return Err(error);
                }
                warn!(transaction_id = %record.transaction_id, error = %error, "recovery failed");
                let outcome = fail(store, &mut record, executor)?;
                Ok((record, outcome))
            }
        },
        // must never be turned into a commit.
        TransactionPhase::Prepared | TransactionPhase::RollingBack => {
            let outcome = rollback_after_failure(store, &mut record, executor)?;
            Ok((record, outcome))
        }
        _ => {
            let outcome = settled(record.phase);
            Ok((record, outcome))
        }
    }
}

fn settled(phase: TransactionPhase) -> TransactionOutcome {
    match phase {
        TransactionPhase::Committed => TransactionOutcome::Committed,
        TransactionPhase::RolledBack => TransactionOutcome::RolledBack,
        _ => TransactionOutcome::RecoveryRequired,
    }
}

fn fail<S: TransactionStore, E: OperationExecutor>(
    store: &S,
    record: &mut TransactionRecord,
    executor: &mut E,
) -> Result<TransactionOutcome, TransactionError>
where
    E::Error: std::fmt::Display,
{
    if record.phase == TransactionPhase::RecoveryRequired {
        return Ok(TransactionOutcome::RecoveryRequired);
    }
    rollback_after_failure(store, record, executor)
}

fn drive<S: TransactionStore, E: OperationExecutor>(
    store: &S,
    record: &mut TransactionRecord,
    executor: &mut E,
) -> Result<(), TransactionError>
where
    E::Error: std::fmt::Display,
{
    let order = record.plan.execution_order.clone();
    for op_id in &order {
        let node = find_node(&record.plan, op_id)?.clone();
        let state = record
            .nodes
            .get(op_id)
            .cloned()
            .unwrap_or(NodeState::Pending);
        if node.kind.is_barrier() {
            if matches!(state, NodeState::Applied { .. }) {
                cross_commit_intent(record, store, &node)?;
                continue;
            }
            if node.phase == Phase::Verify {
                verify_applied(store, record, executor)?;
            }
            cross_barrier(store, record, executor, &node)?;
        } else if matches!(state, NodeState::Pending) {
            run_mutation(store, record, executor, &node)?;
        }
    }
    Ok(())
}

fn barrier_prepares(phase: Phase) -> bool {
    matches!(phase, Phase::Begin | Phase::Preflight)
}

/// can never replay a completed barrier as an untracked side effect.
fn cross_barrier<S: TransactionStore, E: OperationExecutor>(
    store: &S,
    record: &mut TransactionRecord,
    executor: &mut E,
    node: &TransactionNode,
) -> Result<(), TransactionError>
where
    E::Error: std::fmt::Display,
{
    if barrier_prepares(node.phase) {
        executor
            .prepare(node)
            .map_err(|error| TransactionError::Executor {
                operation: node.id.to_string(),
                message: error.to_string(),
            })?;
    }
    commit_node(record, store, &node.id, NodeState::Running)?;
    let receipt = match executor.apply(node) {
        Ok(receipt) => receipt,
        Err(error) => {
            commit_node(record, store, &node.id, NodeState::Pending)?;
            return Err(TransactionError::Executor {
                operation: node.id.to_string(),
                message: error.to_string(),
            });
        }
    };
    commit_node(
        record,
        store,
        &node.id,
        NodeState::Applied {
            receipt: Box::new(receipt),
        },
    )?;
    cross_commit_intent(record, store, node)?;
    info!(operation = %node.id, phase = ?node.phase, "barrier crossed");
    Ok(())
}

fn cross_commit_intent<S: TransactionStore>(
    record: &mut TransactionRecord,
    store: &S,
    node: &TransactionNode,
) -> Result<(), TransactionError> {
    if node.phase != Phase::CommitIntent {
        return Ok(());
    }
    commit_phase(record, store, TransactionPhase::Applying)?;
    info!(operation = %node.id, "commit intent");
    Ok(())
}

fn run_mutation<S: TransactionStore, E: OperationExecutor>(
    store: &S,
    record: &mut TransactionRecord,
    executor: &mut E,
    node: &TransactionNode,
) -> Result<(), TransactionError>
where
    E::Error: std::fmt::Display,
{
    commit_node(record, store, &node.id, NodeState::Running)?;
    info!(operation = %node.id, "node intent");

    let receipt = match executor.apply(node) {
        Ok(receipt) => receipt,
        Err(error) => {
            settle_failed_mutation(store, record, executor, node)?;
            return Err(TransactionError::Executor {
                operation: node.id.to_string(),
                message: error.to_string(),
            });
        }
    };
    commit_node(
        record,
        store,
        &node.id,
        NodeState::Applied {
            receipt: Box::new(receipt),
        },
    )?;
    info!(operation = %node.id, "node applied");
    Ok(())
}

fn settle_failed_mutation<S: TransactionStore, E: OperationExecutor>(
    store: &S,
    record: &mut TransactionRecord,
    executor: &mut E,
    node: &TransactionNode,
) -> Result<(), TransactionError>
where
    E::Error: std::fmt::Display,
{
    let op_id = &node.id;
    match executor.reconcile(node, None) {
        Ok(ReconcileResult::NotApplied) => {
            commit_node(record, store, op_id, NodeState::Failed)?;
        }
        Ok(ReconcileResult::AppliedWithReceipt(receipt)) => {
            commit_node(
                record,
                store,
                op_id,
                NodeState::Applied {
                    receipt: Box::new(receipt),
                },
            )?;
        }
        Ok(ReconcileResult::Applied) => {
            commit_node(
                record,
                store,
                op_id,
                NodeState::Applied {
                    receipt: Box::new(OperationReceipt::Control),
                },
            )?;
        }
        Ok(ReconcileResult::Ambiguous) | Err(_) => {
            commit_phase(record, store, TransactionPhase::RecoveryRequired)?;
        }
    }
    Ok(())
}

/// so recovery never re-runs a completed verification.
fn verify_applied<S: TransactionStore, E: OperationExecutor>(
    store: &S,
    record: &mut TransactionRecord,
    executor: &mut E,
) -> Result<(), TransactionError>
where
    E::Error: std::fmt::Display,
{
    let order = record.plan.execution_order.clone();
    for op_id in &order {
        let node = find_node(&record.plan, op_id)?.clone();
        if !node.kind.requires_verification() {
            continue;
        }
        let receipt = match record.nodes.get(op_id) {
            Some(NodeState::Applied { receipt }) => receipt.clone(),
            _ => continue,
        };
        executor
            .verify(&node, &receipt)
            .map_err(|error| TransactionError::Executor {
                operation: op_id.to_string(),
                message: error.to_string(),
            })?;
        commit_node(record, store, op_id, NodeState::Verified { receipt })?;
        info!(operation = %op_id, "node verified");
    }
    Ok(())
}

fn reconcile_running<S: TransactionStore, E: OperationExecutor>(
    record: &mut TransactionRecord,
    store: &S,
    executor: &mut E,
) -> Result<(), TransactionError>
where
    E::Error: std::fmt::Display,
{
    let running: Vec<OperationId> = record
        .nodes
        .iter()
        .filter(|(_, state)| **state == NodeState::Running)
        .map(|(id, _)| id.clone())
        .collect();

    let mut ambiguous = false;
    for op_id in &running {
        let node = find_node(&record.plan, op_id)?.clone();
        if let NodeKind::Barrier = node.kind {
            commit_node(record, store, op_id, NodeState::Pending)?;
            continue;
        }
        info!(operation = %op_id, "reconciliation");
        match executor
            .reconcile(&node, None)
            .map_err(|error| TransactionError::Executor {
                operation: op_id.to_string(),
                message: error.to_string(),
            })? {
            ReconcileResult::NotApplied => {
                commit_node(record, store, op_id, NodeState::Pending)?;
            }
            ReconcileResult::Applied => {
                commit_node(
                    record,
                    store,
                    op_id,
                    NodeState::Applied {
                        receipt: Box::new(OperationReceipt::Control),
                    },
                )?;
            }
            ReconcileResult::AppliedWithReceipt(receipt) => {
                commit_node(
                    record,
                    store,
                    op_id,
                    NodeState::Applied {
                        receipt: Box::new(receipt),
                    },
                )?;
            }
            ReconcileResult::Ambiguous => {
                ambiguous = true;
                warn!(operation = %op_id, "ambiguous reconciliation");
            }
        }
    }

    if ambiguous {
        commit_phase(record, store, TransactionPhase::RecoveryRequired)?;
    }
    Ok(())
}

fn rollback_after_failure<S: TransactionStore, E: OperationExecutor>(
    store: &S,
    record: &mut TransactionRecord,
    executor: &mut E,
) -> Result<TransactionOutcome, TransactionError>
where
    E::Error: std::fmt::Display,
{
    if record
        .nodes
        .values()
        .any(|state| *state == NodeState::RollingBack)
    {
        warn!(transaction_id = %record.transaction_id, "rollback was interrupted");
        commit_phase(record, store, TransactionPhase::RecoveryRequired)?;
        return Ok(TransactionOutcome::RecoveryRequired);
    }

    commit_phase(record, store, TransactionPhase::RollingBack)?;
    info!(transaction_id = %record.transaction_id, "rollback started");

    let mut rollback_failed = false;
    let rollback_order = record.plan.rollback_order.clone();
    for op_id in &rollback_order {
        let node = find_node(&record.plan, op_id)?.clone();
        let state = record
            .nodes
            .get(op_id)
            .cloned()
            .unwrap_or(NodeState::Pending);
        if node.kind.is_barrier() {
            if matches!(state, NodeState::Applied { .. }) {
                commit_node(record, store, op_id, NodeState::RolledBack)?;
            }
            continue;
        }
        let receipt = match state {
            NodeState::Applied { receipt } | NodeState::Verified { receipt } => receipt,
            _ => continue,
        };
        commit_node(record, store, op_id, NodeState::RollingBack)?;

        match executor.rollback(&node, &receipt) {
            Ok(()) => {
                commit_node(record, store, op_id, NodeState::RolledBack)?;
                info!(operation = %op_id, "node rolled back");
            }
            Err(error) => {
                rollback_failed = true;
                let _ = commit_node(record, store, op_id, NodeState::Failed);
                warn!(operation = %op_id, error = %error, "rollback failed");
            }
        }
    }

    if rollback_failed {
        commit_phase(record, store, TransactionPhase::RecoveryRequired)?;
        return Ok(TransactionOutcome::RecoveryRequired);
    }

    commit_phase(record, store, TransactionPhase::RolledBack)?;
    Ok(TransactionOutcome::RolledBack)
}

fn commit<S: TransactionStore>(
    record: &mut TransactionRecord,
    store: &S,
    mut change: impl FnMut(&mut TransactionRecord) -> Result<(), String>,
) -> Result<(), TransactionError> {
    let id = record.transaction_id;
    let committed = store
        .update(&id, &mut |fresh| {
            change(fresh).map_err(|reason| StoreError::Rejected {
                id: id.to_string(),
                reason,
            })
        })
        .map_err(TransactionError::Store)?;
    *record = committed;
    Ok(())
}

fn commit_node<S: TransactionStore>(
    record: &mut TransactionRecord,
    store: &S,
    op_id: &OperationId,
    state: NodeState,
) -> Result<(), TransactionError> {
    commit(record, store, move |fresh| {
        let Some(current) = fresh.nodes.get(op_id) else {
            return Err(format!("missing node {op_id}"));
        };
        if *current == state {
            return Ok(());
        }
        let next = current
            .clone()
            .transition(state.clone())
            .map_err(|_| format!("node {op_id} cannot enter {state:?} from {current:?}"))?;
        fresh.nodes.insert(op_id.clone(), next);
        Ok(())
    })
}

fn commit_phase<S: TransactionStore>(
    record: &mut TransactionRecord,
    store: &S,
    next: TransactionPhase,
) -> Result<(), TransactionError> {
    commit(record, store, move |fresh| {
        if let Ok(phase) = fresh.phase.transition(next) {
            fresh.phase = phase;
        }
        Ok(())
    })
}

fn find_node<'a>(
    plan: &'a TransactionPlan,
    id: &OperationId,
) -> Result<&'a TransactionNode, TransactionError> {
    plan.nodes
        .iter()
        .find(|n| &n.id == id)
        .ok_or_else(|| TransactionError::InvalidState(format!("unknown node {id}")))
}
