//! Transaction coordinator: durable apply / rollback / recovery.

use jiff::Timestamp;
use miette::Diagnostic;
use semver::Version;
use thiserror::Error;
use tracing::{info, warn};
use zup_core::{AppId, SelectedScope};

use crate::executor::{OperationExecutor, OperationReceipt, ReconcileResult};
use crate::id::{OperationId, TransactionId};
use crate::plan::{NodeKind, TransactionNode, TransactionPlan};
use crate::record::{NodeState, StoreError, TransactionPhase, TransactionRecord};
use crate::store::TransactionStore;

/// Final stable outcome of a transaction attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransactionOutcome {
    Committed,
    RolledBack,
    RecoveryRequired,
}

/// Coordinator / executor failures.
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

/// Synchronous transaction coordinator.
///
/// Contains no platform-specific mutation logic.
pub struct TransactionCoordinator<S: TransactionStore> {
    store: S,
}

impl<S: TransactionStore> TransactionCoordinator<S> {
    pub fn new(store: S) -> Self {
        Self { store }
    }

    /// Create the durable record and return it ready for execution.
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

    /// Execute the prepared transaction to a stable outcome.
    pub fn execute<E: OperationExecutor>(
        &self,
        mut record: TransactionRecord,
        executor: &mut E,
    ) -> Result<(TransactionRecord, TransactionOutcome), TransactionError>
    where
        E::Error: std::fmt::Display,
    {
        // Prepared → Applying is the durable commit-intent boundary.
        record.phase = record
            .phase
            .transition(TransactionPhase::Applying)
            .map_err(|e| TransactionError::InvalidState(e.to_string()))?;
        self.persist(&mut record)?;
        info!(transaction_id = %record.transaction_id, "commit intent");

        match apply_all(&self.store, &mut record, executor) {
            Ok(()) => {
                record.phase = TransactionPhase::Committed;
                self.persist(&mut record)?;
                info!(transaction_id = %record.transaction_id, "commit");
                Ok((record, TransactionOutcome::Committed))
            }
            Err(err) => {
                warn!(transaction_id = %record.transaction_id, error = %err, "apply failed");
                if record.phase == TransactionPhase::RecoveryRequired {
                    return Ok((record, TransactionOutcome::RecoveryRequired));
                }
                let outcome = rollback_after_failure(&self.store, &mut record, executor)?;
                Ok((record, outcome))
            }
        }
    }

    fn persist(&self, record: &mut TransactionRecord) -> Result<(), TransactionError> {
        save(record, &self.store)
    }
}

/// Recover a transaction from durable state after a crash.
///
/// Reconciles every `Running` node before doing anything else. Rolls forward
/// when the transaction had already crossed commit intent (`Applying`).
pub fn recover<E: OperationExecutor, S: TransactionStore>(
    mut record: TransactionRecord,
    store: &S,
    executor: &mut E,
) -> Result<(TransactionRecord, TransactionOutcome), TransactionError>
where
    E::Error: std::fmt::Display,
{
    info!(transaction_id = %record.transaction_id, phase = ?record.phase, "recovery");

    let running: Vec<OperationId> = record
        .nodes
        .iter()
        .filter(|(_, s)| **s == NodeState::Running)
        .map(|(id, _)| id.clone())
        .collect();

    let mut ambiguous = false;
    for op_id in &running {
        let node = find_node(&record.plan, op_id)?;
        if matches!(node.kind, NodeKind::Barrier) {
            continue;
        }
        info!(operation = %op_id, "reconciliation");
        match executor
            .reconcile(node, None)
            .map_err(|e| TransactionError::Executor {
                operation: op_id.to_string(),
                message: e.to_string(),
            })? {
            ReconcileResult::NotApplied => {
                set_node_state(&mut record, op_id, NodeState::Pending)?;
                save(&mut record, store)?;
            }
            ReconcileResult::Applied => {
                set_node_state(
                    &mut record,
                    op_id,
                    NodeState::Applied {
                        receipt: Box::new(OperationReceipt::Control),
                    },
                )?;
                save(&mut record, store)?;
            }
            ReconcileResult::AppliedWithReceipt(receipt) => {
                set_node_state(
                    &mut record,
                    op_id,
                    NodeState::Applied {
                        receipt: Box::new(receipt),
                    },
                )?;
                save(&mut record, store)?;
            }
            ReconcileResult::Ambiguous => {
                ambiguous = true;
                warn!(operation = %op_id, "ambiguous reconciliation");
            }
        }
    }

    if ambiguous {
        record.phase = TransactionPhase::RecoveryRequired;
        save(&mut record, store)?;
        return Ok((record, TransactionOutcome::RecoveryRequired));
    }

    if record.phase == TransactionPhase::Committed {
        return Ok((record, TransactionOutcome::Committed));
    }
    if record.phase == TransactionPhase::RolledBack {
        return Ok((record, TransactionOutcome::RolledBack));
    }
    if record.phase == TransactionPhase::RecoveryRequired {
        return Ok((record, TransactionOutcome::RecoveryRequired));
    }

    let crossed_commit_intent = matches!(
        record.phase,
        TransactionPhase::Applying | TransactionPhase::RollingBack
    );

    if crossed_commit_intent {
        roll_forward(record, store, executor)
    } else {
        // Crash before commit intent — no application state was intentionally
        // mutated. Abandon/clean up.
        record.phase = TransactionPhase::RolledBack;
        save(&mut record, store)?;
        Ok((record, TransactionOutcome::RolledBack))
    }
}

fn apply_all<S: TransactionStore, E: OperationExecutor>(
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
        if matches!(node.kind, NodeKind::Barrier) {
            continue;
        }
        let state = record
            .nodes
            .get(op_id)
            .cloned()
            .unwrap_or(NodeState::Pending);
        if !matches!(state, NodeState::Pending) {
            continue;
        }

        // Durable intent BEFORE side effect.
        set_node_state(record, op_id, NodeState::Running)?;
        save(record, store)?;
        info!(operation = %op_id, "node intent");

        let receipt = match executor.apply(&node) {
            Ok(receipt) => receipt,
            Err(err) => {
                match executor.reconcile(&node, None) {
                    Ok(ReconcileResult::NotApplied) => {
                        set_node_state(record, op_id, NodeState::Failed)?;
                        save(record, store)?;
                    }
                    Ok(ReconcileResult::AppliedWithReceipt(receipt)) => {
                        set_node_state(
                            record,
                            op_id,
                            NodeState::Applied {
                                receipt: Box::new(receipt),
                            },
                        )?;
                        save(record, store)?;
                    }
                    Ok(ReconcileResult::Applied) => {
                        set_node_state(
                            record,
                            op_id,
                            NodeState::Applied {
                                receipt: Box::new(OperationReceipt::Control),
                            },
                        )?;
                        save(record, store)?;
                    }
                    Ok(ReconcileResult::Ambiguous) | Err(_) => {
                        record.phase = TransactionPhase::RecoveryRequired;
                        save(record, store)?;
                    }
                }
                return Err(TransactionError::Executor {
                    operation: op_id.to_string(),
                    message: err.to_string(),
                });
            }
        };

        set_node_state(
            record,
            op_id,
            NodeState::Applied {
                receipt: Box::new(receipt),
            },
        )?;
        save(record, store)?;
        info!(operation = %op_id, "node applied");
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
    record.phase = TransactionPhase::RollingBack;
    save(record, store)?;
    info!("rollback started");

    let mut rollback_failed = false;
    let rollback_order = record.plan.rollback_order.clone();
    for op_id in &rollback_order {
        let node = find_node(&record.plan, op_id)?.clone();
        if matches!(node.kind, NodeKind::Barrier) {
            continue;
        }
        let receipt = match record.nodes.get(op_id) {
            Some(NodeState::Applied { receipt }) => receipt.clone(),
            _ => continue,
        };
        set_node_state(record, op_id, NodeState::RollingBack)?;
        save(record, store)?;

        match executor.rollback(&node, &receipt) {
            Ok(()) => {
                set_node_state(record, op_id, NodeState::RolledBack)?;
                save(record, store)?;
                info!(operation = %op_id, "node rolled back");
            }
            Err(err) => {
                rollback_failed = true;
                let _ = set_node_state(record, op_id, NodeState::Failed);
                let _ = save(record, store);
                warn!(operation = %op_id, error = %err, "rollback failed");
            }
        }
    }

    if rollback_failed {
        record.phase = TransactionPhase::RecoveryRequired;
        save(record, store)?;
        return Ok(TransactionOutcome::RecoveryRequired);
    }

    record.phase = TransactionPhase::RolledBack;
    save(record, store)?;
    Ok(TransactionOutcome::RolledBack)
}

fn roll_forward<S: TransactionStore, E: OperationExecutor>(
    mut record: TransactionRecord,
    store: &S,
    executor: &mut E,
) -> Result<(TransactionRecord, TransactionOutcome), TransactionError>
where
    E::Error: std::fmt::Display,
{
    if record.phase == TransactionPhase::Prepared {
        record.phase = TransactionPhase::Applying;
        save(&mut record, store)?;
    }
    apply_all(store, &mut record, executor)?;
    record.phase = TransactionPhase::Committed;
    save(&mut record, store)?;
    Ok((record, TransactionOutcome::Committed))
}

fn save<S: TransactionStore>(
    record: &mut TransactionRecord,
    store: &S,
) -> Result<(), TransactionError> {
    let expected = record.revision;
    record.updated_at = Timestamp::now();
    record.revision = expected.saturating_add(1);
    store.compare_and_swap(expected, record)?;
    Ok(())
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

fn set_node_state(
    record: &mut TransactionRecord,
    id: &OperationId,
    state: NodeState,
) -> Result<(), TransactionError> {
    let current = record
        .nodes
        .get_mut(id)
        .ok_or_else(|| TransactionError::InvalidState(format!("missing node {id}")))?;
    *current = state;
    Ok(())
}
