use miette::Diagnostic;
use semver::Version;
use thiserror::Error;
use tracing::{info, warn};
use zup_core::{AppId, SelectedScope};

use crate::executor::{OperationExecutor, OperationReceipt, ReconcileResult};
use crate::fault_injection::{Boundary, boundary};
use crate::record::{NodeState, RecoveryPhase, StoreError, TransactionPhase, TransactionRecord};
use crate::storage::TransactionStore;
use zup_core::TransactionId;
use zup_plan::OperationId;
use zup_plan::{DeploymentNode, DeploymentPlan, NodeKind, Phase};

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

/// Journal owner from `begin` until execute, release, or an explicit preserve.
///
/// Dropping it leaves the journal on disk. It does not roll the transaction back.
#[must_use = "an open transaction must be executed, released, or explicitly preserved for recovery"]
pub struct OpenTransaction<S: TransactionStore> {
    store: S,
    record: TransactionRecord,
}

pub struct SettledTransaction {
    pub record: TransactionRecord,
    pub outcome: TransactionOutcome,
}

impl<S: TransactionStore> TransactionCoordinator<S> {
    pub fn new(store: S) -> Self {
        Self { store }
    }

    pub fn begin(
        self,
        app_id: AppId,
        scope: SelectedScope,
        app_version: Version,
        plan: DeploymentPlan,
    ) -> Result<OpenTransaction<S>, TransactionError> {
        let id = TransactionId::new_v7();
        let record = TransactionRecord::new(id, app_id, scope, app_version, plan);
        self.store.create(&record)?;
        info!(transaction_id = %id, plan_hash = %record.plan_hash, "transaction created");
        Ok(OpenTransaction {
            store: self.store,
            record,
        })
    }
}

impl<S: TransactionStore> std::ops::Deref for OpenTransaction<S> {
    type Target = TransactionRecord;

    fn deref(&self) -> &Self::Target {
        &self.record
    }
}

impl<S: TransactionStore> OpenTransaction<S> {
    pub fn with_release(
        mut self,
        release: Option<zup_core::ReleaseIdentity>,
    ) -> Result<Self, TransactionError> {
        self.record.release = release;
        let revision = self.record.revision;
        self.record.touch();
        self.store.compare_and_swap(revision, &self.record)?;
        Ok(self)
    }

    pub fn record(&self) -> &TransactionRecord {
        &self.record
    }

    /// Keeps the journal for recovery without settling it.
    pub fn preserve(self) -> TransactionRecord {
        self.record
    }

    pub fn release<E: OperationExecutor>(
        self,
        executor: &mut E,
    ) -> Result<SettledTransaction, TransactionError>
    where
        E::Error: std::fmt::Display,
    {
        let (record, outcome) =
            TransactionCoordinator { store: self.store }.release(self.record, executor)?;
        Ok(SettledTransaction { record, outcome })
    }

    pub fn execute<E: OperationExecutor, P: crate::InstallationPublisher>(
        self,
        executor: &mut E,
        publisher: &P,
    ) -> Result<SettledTransaction, TransactionError>
    where
        E::Error: std::fmt::Display,
    {
        let (record, outcome) = TransactionCoordinator { store: self.store }.execute(
            self.record,
            executor,
            publisher,
        )?;
        Ok(SettledTransaction { record, outcome })
    }
}

impl<S: TransactionStore> TransactionCoordinator<S> {
    pub fn release<E: OperationExecutor>(
        &self,
        mut record: TransactionRecord,
        executor: &mut E,
    ) -> Result<(TransactionRecord, TransactionOutcome), TransactionError>
    where
        E::Error: std::fmt::Display,
    {
        if record.phase != TransactionPhase::Prepared {
            return Err(TransactionError::InvalidState(
                "only a prepared transaction can be released before execution".into(),
            ));
        }
        if record
            .nodes
            .values()
            .any(|state| !matches!(state, NodeState::Pending))
        {
            return Err(TransactionError::InvalidState(
                "a prepared transaction has no applied work".into(),
            ));
        }
        let outcome = rollback_after_failure(&self.store, &mut record, executor)?;
        Ok((record, outcome))
    }

    pub fn execute<E: OperationExecutor, P: crate::InstallationPublisher>(
        &self,
        mut record: TransactionRecord,
        executor: &mut E,
        publisher: &P,
    ) -> Result<(TransactionRecord, TransactionOutcome), TransactionError>
    where
        E::Error: std::fmt::Display,
    {
        match drive(&self.store, &mut record, executor) {
            Ok(()) => {
                let outcome = publish_and_collect(&self.store, &mut record, publisher, executor)?;
                Ok((record, outcome))
            }
            Err(error) => {
                if matches!(error, TransactionError::Store(_)) {
                    // record the store never accepted would only compound it.
                    return Err(error);
                }
                warn!(transaction_id = %record.transaction_id, error = %error, "execution failed");
                let operation = match &error {
                    TransactionError::Executor { operation, .. } => {
                        Some(OperationId::new(operation))
                    }
                    _ => None,
                };
                let message: String = error.to_string().chars().take(4096).collect();
                commit(&mut record, &self.store, |record| {
                    record.failure = Some(crate::TransactionFailure {
                        operation: operation.clone(),
                        message: message.clone(),
                    });
                    Ok(())
                })?;
                let outcome = fail(&self.store, &mut record, executor)?;
                Ok((record, outcome))
            }
        }
    }
}

pub fn recover<E: OperationExecutor, S: TransactionStore, P: crate::InstallationPublisher>(
    mut record: TransactionRecord,
    store: &S,
    executor: &mut E,
    publisher: &P,
) -> Result<(TransactionRecord, TransactionOutcome), TransactionError>
where
    E::Error: std::fmt::Display,
{
    record
        .validate()
        .map_err(|error| TransactionError::InvalidState(error.to_string()))?;
    info!(transaction_id = %record.transaction_id, phase = ?record.phase, "recovery");

    if record.phase == TransactionPhase::Committed {
        let outcome = collect_committed(store, &mut record, executor)?;
        return Ok((record, outcome));
    }
    if record.phase == TransactionPhase::RolledBack {
        return Ok((record, TransactionOutcome::RolledBack));
    }

    if let TransactionPhase::RecoveryRequired { resume } = record.phase {
        commit_phase(&mut record, store, resume.into())?;
    }
    if !matches!(record.publication, crate::PublicationState::NotRequested) {
        let outcome = publish_and_collect(store, &mut record, publisher, executor)?;
        return Ok((record, outcome));
    }
    let mut recovery_caches: Vec<_> = record
        .plan
        .nodes
        .iter()
        .filter(|node| {
            let NodeKind::NativeCache { operation } = &node.kind else {
                return false;
            };
            if record
                .nodes
                .get(&node.id)
                .is_some_and(NodeState::is_rolled_back)
            {
                return false;
            }
            !matches!(record.nodes.get(&node.id), Some(NodeState::Pending))
                || operation
                    .source_nodes(&record.plan.nodes)
                    .is_ok_and(|sources| {
                        sources.iter().any(|source| {
                            !matches!(
                                record.nodes.get(&source.id),
                                Some(NodeState::Pending | NodeState::Failed)
                            )
                        })
                    })
        })
        .cloned()
        .collect();
    recovery_caches.sort_by(|left, right| left.id.cmp(&right.id));
    for node in recovery_caches {
        if let Err(error) = executor.prepare(&node) {
            commit(&mut record, store, |fresh| {
                fresh.recovery_failure = Some(crate::TransactionFailure {
                    operation: Some(node.id.clone()),
                    message: error.to_string().chars().take(4096).collect(),
                });
                Ok(())
            })?;
            require_recovery(&mut record, store)?;
            return Ok((record, TransactionOutcome::RecoveryRequired));
        }
    }
    reconcile_running(&mut record, store, executor)?;
    if matches!(record.phase, TransactionPhase::RecoveryRequired { .. }) {
        return Ok((record, TransactionOutcome::RecoveryRequired));
    }

    match record.phase {
        TransactionPhase::Applying if record.failure.is_some() => {
            let outcome = rollback_after_failure(store, &mut record, executor)?;
            Ok((record, outcome))
        }
        TransactionPhase::Applying => match drive(store, &mut record, executor) {
            Ok(()) => {
                let outcome = publish_and_collect(store, &mut record, publisher, executor)?;
                Ok((record, outcome))
            }
            Err(error) => {
                if matches!(error, TransactionError::Store(_)) {
                    return Err(error);
                }
                warn!(transaction_id = %record.transaction_id, error = %error, "recovery failed");
                let operation = match &error {
                    TransactionError::Executor { operation, .. } => {
                        Some(OperationId::new(operation))
                    }
                    _ => None,
                };
                let message: String = error.to_string().chars().take(4096).collect();
                commit(&mut record, store, |record| {
                    record.recovery_failure = Some(crate::TransactionFailure {
                        operation: operation.clone(),
                        message: message.clone(),
                    });
                    Ok(())
                })?;
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

fn publish_installation<S: TransactionStore, P: crate::InstallationPublisher>(
    store: &S,
    record: &mut TransactionRecord,
    publisher: &P,
) -> Result<TransactionOutcome, TransactionError> {
    let publication = OperationId::new(OperationId::COMMIT);
    let prepared = publisher
        .prepare(record)
        .map_err(|error| error.to_string())
        .and_then(|installation| {
            crate::verify_published_installation(record, &installation)
                .map(|record_hash| (installation, record_hash))
        });
    let published = match prepared {
        Ok((expected, record_hash)) => {
            if matches!(record.publication, crate::PublicationState::NotRequested) {
                commit(record, store, |fresh| {
                    if !matches!(fresh.nodes.get(&publication), Some(NodeState::Pending)) {
                        return Err(
                            "installation publication has inconsistent journal intent".into()
                        );
                    }
                    fresh.nodes.insert(publication.clone(), NodeState::Running);
                    fresh.publication = crate::PublicationState::Requested { record_hash };
                    Ok(())
                })?;
            }
            let expected_hash = match record.publication {
                crate::PublicationState::Requested { record_hash }
                | crate::PublicationState::Published { record_hash } => record_hash,
                crate::PublicationState::NotRequested => unreachable!(),
            };
            if record_hash != expected_hash {
                Err("installation projection differs from the durable publication request".into())
            } else {
                boundary(
                    Boundary::InstallationPublicationRequested,
                    &record.transaction_id.to_string(),
                );
                publisher.publish(record)
                    .map_err(|error| error.to_string())
                    .and_then(|installation| {
                        if installation != expected {
                            return Err("published ownership differs from the requested installation record".into());
                        }
                        crate::verify_published_installation(record, &installation)
                    })
            }
        }
        Err(error) => Err(error),
    };
    let record_hash = match published {
        Ok(record_hash) => record_hash,
        Err(message) => {
            commit(record, store, |fresh| {
                fresh.recovery_failure = Some(crate::TransactionFailure {
                    operation: None,
                    message: message.chars().take(4096).collect(),
                });
                Ok(())
            })?;
            require_recovery(record, store)?;
            return Ok(TransactionOutcome::RecoveryRequired);
        }
    };
    boundary(
        Boundary::InstallationPublished,
        &record.transaction_id.to_string(),
    );
    commit(record, store, |fresh| {
        fresh.nodes.insert(
            publication.clone(),
            NodeState::Applied {
                receipt: Box::new(OperationReceipt::Control),
            },
        );
        fresh.publication = crate::PublicationState::Published { record_hash };
        fresh.recovery_failure = None;
        Ok(())
    })?;
    commit_phase(record, store, TransactionPhase::Committed)?;
    Ok(TransactionOutcome::Committed)
}

fn publish_and_collect<S: TransactionStore, P: crate::InstallationPublisher, E: OperationExecutor>(
    store: &S,
    record: &mut TransactionRecord,
    publisher: &P,
    executor: &mut E,
) -> Result<TransactionOutcome, TransactionError>
where
    E::Error: std::fmt::Display,
{
    let outcome = publish_installation(store, record, publisher)?;
    if outcome == TransactionOutcome::Committed {
        collect_committed(store, record, executor)
    } else {
        Ok(outcome)
    }
}

fn collect_committed<S: TransactionStore, E: OperationExecutor>(
    store: &S,
    record: &mut TransactionRecord,
    executor: &mut E,
) -> Result<TransactionOutcome, TransactionError>
where
    E::Error: std::fmt::Display,
{
    if record.phase != TransactionPhase::Committed {
        return Err(TransactionError::InvalidState(
            "cleanup requires a committed installation record".into(),
        ));
    }
    let nodes: Vec<_> = record
        .plan
        .nodes
        .iter()
        .filter(|node| node.kind.is_post_commit())
        .cloned()
        .collect();
    if nodes.is_empty() {
        return Ok(TransactionOutcome::Committed);
    }
    for node in nodes {
        if let Err(error) = collect_node(store, record, executor, &node) {
            if matches!(error, TransactionError::Store(_)) {
                return Err(error);
            }
            commit(record, store, |fresh| {
                fresh.recovery_failure = Some(crate::TransactionFailure {
                    operation: Some(node.id.clone()),
                    message: format!("deployment committed; cleanup requires recovery: {error}")
                        .chars()
                        .take(4096)
                        .collect(),
                });
                Ok(())
            })?;
            return Ok(TransactionOutcome::RecoveryRequired);
        }
    }
    if record.recovery_failure.is_some() {
        commit(record, store, |fresh| {
            fresh.recovery_failure = None;
            Ok(())
        })?;
    }
    Ok(TransactionOutcome::Committed)
}

fn collect_node<S: TransactionStore, E: OperationExecutor>(
    store: &S,
    record: &mut TransactionRecord,
    executor: &mut E,
    node: &DeploymentNode,
) -> Result<(), TransactionError>
where
    E::Error: std::fmt::Display,
{
    let failed = |error: E::Error| TransactionError::Executor {
        operation: node.id.to_string(),
        message: error.to_string(),
    };
    let state = record.nodes.get(&node.id).cloned().ok_or_else(|| {
        TransactionError::InvalidState("cleanup node has no journal state".into())
    })?;
    if matches!(state, NodeState::Verified { .. }) {
        return Ok(());
    }
    if matches!(state, NodeState::Running) {
        match executor.reconcile(node, None, record).map_err(failed)? {
            ReconcileResult::NotApplied | ReconcileResult::Incomplete => {
                commit_node(record, store, &node.id, NodeState::Pending)?
            }
            ReconcileResult::AppliedWithReceipt(receipt) => commit_node(
                record,
                store,
                &node.id,
                NodeState::Applied {
                    receipt: Box::new(receipt),
                },
            )?,
            _ => {
                return Err(TransactionError::Executor {
                    operation: node.id.to_string(),
                    message:
                        "cleanup outcome is uncertain; retained ownership evidence is required"
                            .into(),
                });
            }
        }
    }
    if matches!(record.nodes.get(&node.id), Some(NodeState::Pending)) {
        executor.prepare(node).map_err(failed)?;
        commit_node(record, store, &node.id, NodeState::Running)?;
        let receipt = executor.apply(node, record).map_err(failed)?;
        boundary(Boundary::ApplyReturned, node.id.as_str());
        commit_node(
            record,
            store,
            &node.id,
            NodeState::Applied {
                receipt: Box::new(receipt),
            },
        )?;
    }
    let Some(NodeState::Applied { receipt }) = record.nodes.get(&node.id).cloned() else {
        return Err(TransactionError::InvalidState(
            "cleanup is missing its application receipt".into(),
        ));
    };
    boundary(Boundary::VerifyBefore, node.id.as_str());
    executor.verify(node, &receipt).map_err(failed)?;
    boundary(Boundary::VerifyReturned, node.id.as_str());
    commit_node(record, store, &node.id, NodeState::Verified { receipt })
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
    if matches!(record.phase, TransactionPhase::RecoveryRequired { .. }) {
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
        if node.kind.is_post_commit() || matches!(node.kind, NodeKind::PublishInstallation) {
            continue;
        }
        if matches!(node.kind, NodeKind::ReserveInstallation) {
            reserve_installation(store, record, &node.id)?;
            continue;
        }
        if matches!(node.kind, NodeKind::Retention { .. }) {
            if !matches!(record.nodes.get(op_id), Some(NodeState::Applied { .. })) {
                commit_node(record, store, op_id, NodeState::Running)?;
                commit_node(
                    record,
                    store,
                    op_id,
                    NodeState::Applied {
                        receipt: Box::new(OperationReceipt::Control),
                    },
                )?;
            }
            continue;
        }
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
                verify_applied(store, record, executor, VerificationPass::Commit)?;
            }
            cross_barrier(store, record, executor, &node)?;
        } else if matches!(state, NodeState::Pending) {
            if matches!(&node.kind, NodeKind::ServiceControl { operation } if operation.stage == zup_plan::ServiceControlStage::Restore)
            {
                verify_applied(store, record, executor, VerificationPass::BeforeRestore)?;
            }
            run_mutation(store, record, executor, &node)?;
        } else if let NodeState::Applied { receipt } = state
            && node.kind.requires_verification()
        {
            executor
                .verify(&node, &receipt)
                .map_err(|error| TransactionError::Executor {
                    operation: node.id.to_string(),
                    message: error.to_string(),
                })?;
            commit_node(record, store, &node.id, NodeState::Verified { receipt })?;
        }
    }
    Ok(())
}

fn barrier_prepares(phase: Phase) -> bool {
    matches!(phase, Phase::Begin | Phase::Preflight)
}

fn prepare_native_caches<E: OperationExecutor>(
    plan: &DeploymentPlan,
    executor: &mut E,
) -> Result<(), TransactionError>
where
    E::Error: std::fmt::Display,
{
    let mut caches: Vec<_> = plan
        .nodes
        .iter()
        .filter(|node| matches!(node.kind, NodeKind::NativeCache { .. }))
        .collect();
    caches.sort_by_key(|node| &node.id);
    for node in caches {
        executor
            .prepare(node)
            .map_err(|error| TransactionError::Executor {
                operation: node.id.to_string(),
                message: error.to_string(),
            })?;
    }
    Ok(())
}

fn reserve_installation<S: TransactionStore>(
    store: &S,
    record: &mut TransactionRecord,
    id: &OperationId,
) -> Result<(), TransactionError> {
    if matches!(record.nodes.get(id), Some(NodeState::Pending)) {
        commit_node(record, store, id, NodeState::Running)?;
    }
    let observed = store.reserve_installation(record).and_then(|reservation| {
        if record
            .reservation
            .as_ref()
            .is_some_and(|expected| expected != &reservation)
        {
            return Err(StoreError::Corrupt(crate::CorruptReason::IdentityMismatch));
        }
        Ok(reservation)
    });
    match observed {
        Ok(reservation) => {
            if matches!(record.nodes.get(id), Some(NodeState::Applied { .. })) {
                return Ok(());
            }
            boundary(Boundary::InstallationReserved, id.as_str());
            commit(record, store, |fresh| {
                fresh.reservation = Some(reservation.clone());
                fresh.nodes.insert(
                    id.clone(),
                    NodeState::Applied {
                        receipt: Box::new(OperationReceipt::Control),
                    },
                );
                Ok(())
            })
        }
        Err(error @ StoreError::InstallationConflict { .. }) if record.reservation.is_none() => {
            commit_node(record, store, id, NodeState::Failed)?;
            Err(TransactionError::Executor {
                operation: id.to_string(),
                message: error.to_string(),
            })
        }
        Err(error) => {
            commit(record, store, |fresh| {
                fresh.recovery_failure = Some(crate::TransactionFailure {
                    operation: Some(id.clone()),
                    message: error.to_string().chars().take(4096).collect(),
                });
                Ok(())
            })?;
            require_recovery(record, store)?;
            Err(TransactionError::Executor {
                operation: id.to_string(),
                message: error.to_string(),
            })
        }
    }
}

fn cross_barrier<S: TransactionStore, E: OperationExecutor>(
    store: &S,
    record: &mut TransactionRecord,
    executor: &mut E,
    node: &DeploymentNode,
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
    if node.phase == Phase::Preflight {
        prepare_native_caches(&record.plan, executor)?;
        for effect in &record.plan.nodes {
            if matches!(
                effect.kind,
                NodeKind::Barrier
                    | NodeKind::ReserveInstallation
                    | NodeKind::PublishInstallation
                    | NodeKind::Retention { .. }
                    | NodeKind::NativeCache { .. }
                    | NodeKind::Cleanup { .. }
            ) {
                continue;
            }
            executor
                .prepare(effect)
                .map_err(|error| TransactionError::Executor {
                    operation: effect.id.to_string(),
                    message: error.to_string(),
                })?;
        }
    }
    commit_node(record, store, &node.id, NodeState::Running)?;
    let receipt = match executor.apply(node, record) {
        Ok(receipt) => receipt,
        Err(error) => {
            commit_node(record, store, &node.id, NodeState::Pending)?;
            return Err(TransactionError::Executor {
                operation: node.id.to_string(),
                message: error.to_string(),
            });
        }
    };
    boundary(Boundary::ApplyReturned, node.id.as_str());
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
    node: &DeploymentNode,
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
    node: &DeploymentNode,
) -> Result<(), TransactionError>
where
    E::Error: std::fmt::Display,
{
    if matches!(
        node.kind,
        NodeKind::FileMutation { .. } | NodeKind::FileRemoval { .. }
    ) {
        for dependency in record
            .plan
            .dependencies
            .iter()
            .filter(|edge| edge.to == node.id)
        {
            let predecessor = find_node(&record.plan, &dependency.from)?;
            if !matches!(&predecessor.kind, NodeKind::ServiceControl { operation }
                if operation.stage == zup_plan::ServiceControlStage::Quiesce)
            {
                continue;
            }
            let receipt = record.receipt(&predecessor.id).ok_or_else(|| {
                TransactionError::InvalidState(
                    "fixed placement has no service quiescence receipt".into(),
                )
            })?;
            executor
                .verify(predecessor, receipt)
                .map_err(|error| TransactionError::Executor {
                    operation: node.id.to_string(),
                    message: error.to_string(),
                })?;
        }
    }
    if let NodeKind::ServiceControl { operation } = &node.kind
        && operation.stage == zup_plan::ServiceControlStage::Restore
        && let zup_core::ResourceKey::Service { id } = &operation.key
    {
        for data in &record.plan.nodes {
            if let NodeKind::PersistentDirectory { operation } = &data.kind
                && operation.stage == zup_plan::PersistentDirectoryStage::Access
                && operation
                    .intent
                    .directory
                    .location
                    .services()
                    .any(|service| service == id)
            {
                let receipt =
                    record
                        .receipt(&data.id)
                        .ok_or_else(|| TransactionError::Executor {
                            operation: node.id.to_string(),
                            message: "service data access has no verified receipt".into(),
                        })?;
                executor
                    .verify(data, receipt)
                    .map_err(|error| TransactionError::Executor {
                        operation: node.id.to_string(),
                        message: error.to_string(),
                    })?;
            }
        }
    }
    commit_node(record, store, &node.id, NodeState::Running)?;
    info!(operation = %node.id, "node intent");

    let receipt = match executor.apply(node, record) {
        Ok(receipt) => receipt,
        Err(error) => {
            settle_failed_mutation(store, record, executor, node)?;
            return Err(TransactionError::Executor {
                operation: node.id.to_string(),
                message: error.to_string(),
            });
        }
    };
    boundary(Boundary::ApplyReturned, node.id.as_str());
    commit_node(
        record,
        store,
        &node.id,
        NodeState::Applied {
            receipt: Box::new(receipt),
        },
    )?;
    info!(operation = %node.id, "node applied");
    if node.kind.requires_verification() {
        let Some(NodeState::Applied { receipt }) = record.nodes.get(&node.id) else {
            unreachable!("applied receipt was just persisted");
        };
        let receipt = receipt.clone();
        boundary(Boundary::VerifyBefore, node.id.as_str());
        executor
            .verify(node, &receipt)
            .map_err(|error| TransactionError::Executor {
                operation: node.id.to_string(),
                message: error.to_string(),
            })?;
        boundary(Boundary::VerifyReturned, node.id.as_str());
        commit_node(record, store, &node.id, NodeState::Verified { receipt })?;
    }
    Ok(())
}

fn settle_failed_mutation<S: TransactionStore, E: OperationExecutor>(
    store: &S,
    record: &mut TransactionRecord,
    executor: &mut E,
    node: &DeploymentNode,
) -> Result<(), TransactionError>
where
    E::Error: std::fmt::Display,
{
    let op_id = &node.id;
    match executor.reconcile(node, None, record) {
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
        Err(error) => {
            let message = error.to_string().chars().take(4096).collect::<String>();
            commit(record, store, |record| {
                record.recovery_failure = Some(crate::TransactionFailure {
                    operation: Some(op_id.clone()),
                    message: message.clone(),
                });
                Ok(())
            })?;
            require_recovery(record, store)?;
        }
        Ok(ReconcileResult::Incomplete | ReconcileResult::Ambiguous) => {
            require_recovery(record, store)?;
        }
    }
    Ok(())
}

#[derive(PartialEq, Eq)]
enum VerificationPass {
    BeforeRestore,
    Commit,
}

fn verify_applied<S: TransactionStore, E: OperationExecutor>(
    store: &S,
    record: &mut TransactionRecord,
    executor: &mut E,
    pass: VerificationPass,
) -> Result<(), TransactionError>
where
    E::Error: std::fmt::Display,
{
    let order = record.plan.execution_order.clone();
    for op_id in &order {
        let node = find_node(&record.plan, op_id)?.clone();
        if node.kind.is_post_commit() || !node.kind.requires_verification() {
            continue;
        }
        let (receipt, already_verified) = match record.nodes.get(op_id) {
            Some(NodeState::Applied { receipt }) => (receipt.clone(), false),
            Some(NodeState::Verified { receipt })
                if pass == VerificationPass::Commit
                    && matches!(
                        node.kind,
                        NodeKind::FileMutation { .. }
                            | NodeKind::RetainFile { .. }
                            | NodeKind::FileRemoval { .. }
                    ) =>
            {
                (receipt.clone(), true)
            }
            _ => continue,
        };
        executor
            .verify(&node, &receipt)
            .map_err(|error| TransactionError::Executor {
                operation: op_id.to_string(),
                message: error.to_string(),
            })?;
        if !already_verified {
            commit_node(record, store, op_id, NodeState::Verified { receipt })?;
        }
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
        if matches!(node.kind, NodeKind::ReserveInstallation) {
            if let Err(error) = reserve_installation(store, record, op_id) {
                if matches!(record.phase, TransactionPhase::RecoveryRequired { .. }) {
                    return Ok(());
                }
                return Err(error);
            }
            continue;
        }
        if matches!(node.kind, NodeKind::Barrier | NodeKind::Retention { .. }) {
            commit_node(record, store, op_id, NodeState::Pending)?;
            continue;
        }
        info!(operation = %op_id, "reconciliation");
        let result = match executor.reconcile(&node, None, record) {
            Ok(result) => result,
            Err(error) => {
                let message: String = error.to_string().chars().take(4096).collect();
                commit(record, store, |record| {
                    record.recovery_failure = Some(crate::TransactionFailure {
                        operation: Some(op_id.clone()),
                        message: message.clone(),
                    });
                    Ok(())
                })?;
                ReconcileResult::Ambiguous
            }
        };
        match result {
            ReconcileResult::NotApplied | ReconcileResult::Incomplete => {
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
                let uncertain = matches!(
                    &receipt,
                    OperationReceipt::NativeCache {
                        completion: crate::NativeCacheCompletion::CompensationOnly,
                        ..
                    }
                );
                commit_node(
                    record,
                    store,
                    op_id,
                    NodeState::Applied {
                        receipt: Box::new(receipt),
                    },
                )?;
                if uncertain && record.failure.is_none() {
                    commit(record, store, |fresh| {
                        fresh.failure = Some(crate::TransactionFailure {
                            operation: Some(op_id.clone()),
                            message: "native cache invocation has no completion evidence; compensating against restored sources".into(),
                        });
                        Ok(())
                    })?;
                }
            }
            ReconcileResult::Ambiguous => {
                ambiguous = true;
                if record.recovery_failure.is_none() {
                    commit(record, store, |record| {
                        record.recovery_failure = Some(crate::TransactionFailure {
                            operation: Some(op_id.clone()),
                            message: "native effects are uncertain; the original operation receipt is unavailable".into(),
                        });
                        Ok(())
                    })?;
                }
                warn!(operation = %op_id, "ambiguous reconciliation");
            }
        }
    }

    if ambiguous {
        require_recovery(record, store)?;
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
    if !matches!(record.publication, crate::PublicationState::NotRequested) {
        require_recovery(record, store)?;
        return Ok(TransactionOutcome::RecoveryRequired);
    }
    let exposed = record.plan.nodes.iter().any(|node| {
        matches!(
            node.kind,
            NodeKind::FileMutation { .. }
                | NodeKind::NativeService { .. }
                | NodeKind::BackendOperation { .. }
                | NodeKind::Generation {
                    operation: zup_plan::GenerationOperation::Activate,
                    ..
                }
        ) && !matches!(record.nodes.get(&node.id), Some(NodeState::Pending))
    });
    if exposed && let Some(node) = record.plan.nodes.iter().find(|node| {
        matches!(&node.kind, NodeKind::PersistentDirectory { operation } if operation.intent.previous.as_ref()
            .is_some_and(|previous| !previous.directory.compatibility.accepts(&operation.intent.directory.compatibility)))
    }) {
        let id = node.id.clone();
        commit(record, store, |record| {
            record.rollback_failure = Some(crate::TransactionFailure {
                operation: Some(id.clone()),
                message: "application code may have written a data format the previous deployment cannot read; automatic executable rollback is unsafe".into(),
            });
            Ok(())
        })?;
        require_recovery(record, store)?;
        return Ok(TransactionOutcome::RecoveryRequired);
    }
    commit_phase(record, store, TransactionPhase::RollingBack)?;
    info!(transaction_id = %record.transaction_id, "rollback started");

    let mut rollback_failed = false;
    let rollback_order = record.plan.rollback_order.clone();
    for op_id in &rollback_order {
        let node = find_node(&record.plan, op_id)?.clone();
        let mut state = record
            .nodes
            .get(op_id)
            .cloned()
            .unwrap_or(NodeState::Pending);
        if matches!(state, NodeState::Pending)
            && let NodeKind::NativeCache { operation } = &node.kind
            && operation
                .source_nodes(&record.plan.nodes)
                .map_err(|error| TransactionError::InvalidState(error.to_string()))?
                .iter()
                .any(|source| {
                    record
                        .nodes
                        .get(&source.id)
                        .is_some_and(NodeState::is_rolled_back)
                })
        {
            state = NodeState::RollingBack {
                receipt: Box::new(OperationReceipt::NativeCache {
                    intent: operation.fingerprint(),
                    completion: crate::NativeCacheCompletion::CompensationOnly,
                }),
            };
            commit_node(record, store, op_id, state.clone())?;
        }
        if matches!(state, NodeState::Pending | NodeState::Failed)
            && let NodeKind::NativeService { operation } = &node.kind
            && operation
                .source_node(&record.plan.nodes)
                .map_err(|error| TransactionError::InvalidState(error.to_string()))?
                .is_some_and(|source| {
                    record
                        .nodes
                        .get(&source.id)
                        .is_some_and(NodeState::is_rolled_back)
                })
        {
            state = NodeState::RollingBack {
                receipt: Box::new(OperationReceipt::NativeServiceCompensation {
                    intent: operation.fingerprint(),
                }),
            };
            commit_node(record, store, op_id, state.clone())?;
        }
        if node.kind.is_post_commit() {
            continue;
        }
        if node.kind.is_barrier()
            || matches!(
                node.kind,
                NodeKind::ReserveInstallation | NodeKind::Retention { .. }
            )
        {
            if matches!(state, NodeState::Applied { .. }) {
                commit_node(record, store, op_id, NodeState::RolledBack)?;
            }
            continue;
        }
        let interrupted = matches!(state, NodeState::RollingBack { .. });
        let mut receipt = match state {
            NodeState::Applied { receipt }
            | NodeState::Verified { receipt }
            | NodeState::RollingBack { receipt } => receipt,
            _ => continue,
        };
        if matches!(node.kind, NodeKind::RetainFile { .. }) {
            if !interrupted {
                commit_node(
                    record,
                    store,
                    op_id,
                    NodeState::RollingBack {
                        receipt: receipt.clone(),
                    },
                )?;
            }
            commit_node(record, store, op_id, NodeState::RolledBack)?;
            continue;
        }
        let compensation_refusal = match &node.kind {
            NodeKind::FileMutation { delta: zup_plan::FileOperationKind::RepairOwned, .. } => Some("repair displaced an externally modified file; automatic restoration is unsafe; retained instances require recovery".to_owned()),
            NodeKind::NativeService { operation } => match operation.as_ref() {
                zup_plan::NativeServiceOperation::Configure { service, .. } => service.validate_compensation().err().map(|error| error.to_string()),
                zup_plan::NativeServiceOperation::Remove { .. } => None,
            },
            _ => None,
        };
        if let Some(message) = compensation_refusal {
            commit(record, store, |record| {
                record.rollback_failure = Some(crate::TransactionFailure {
                    operation: Some(op_id.clone()),
                    message: message.clone(),
                });
                Ok(())
            })?;
            require_recovery(record, store)?;
            return Ok(TransactionOutcome::RecoveryRequired);
        }
        if rollback_failed && node.kind.requires_verification() {
            require_recovery(record, store)?;
            return Ok(TransactionOutcome::RecoveryRequired);
        }
        match executor.prepare_compensation(&node, &receipt) {
            Ok(prepared) if prepared != *receipt => {
                *receipt = prepared;
                commit_node(
                    record,
                    store,
                    op_id,
                    NodeState::RollingBack {
                        receipt: receipt.clone(),
                    },
                )?;
            }
            Ok(_) => {}
            Err(error) => {
                commit(record, store, |record| {
                    record.rollback_failure = Some(crate::TransactionFailure {
                        operation: Some(op_id.clone()),
                        message: error.to_string().chars().take(4096).collect(),
                    });
                    Ok(())
                })?;
                require_recovery(record, store)?;
                return Ok(TransactionOutcome::RecoveryRequired);
            }
        }
        if interrupted {
            match executor.reconcile_rollback(&node, &receipt) {
                Ok(crate::RollbackReconcileResult::Restored) => {
                    commit_node(record, store, op_id, NodeState::RolledBack)?;
                    continue;
                }
                Ok(crate::RollbackReconcileResult::Retry) => {}
                result => {
                    let message = match result {
                        Err(error) => error.to_string(),
                        _ => "interrupted compensation cannot be established from native evidence"
                            .into(),
                    };
                    commit(record, store, |record| {
                        record.rollback_failure = Some(crate::TransactionFailure {
                            operation: Some(op_id.clone()),
                            message: message.chars().take(4096).collect(),
                        });
                        Ok(())
                    })?;
                    require_recovery(record, store)?;
                    return Ok(TransactionOutcome::RecoveryRequired);
                }
            }
        } else {
            commit_node(
                record,
                store,
                op_id,
                NodeState::RollingBack {
                    receipt: receipt.clone(),
                },
            )?;
        }

        match executor.rollback(&node, &receipt) {
            Ok(()) => {
                boundary(Boundary::RollbackReturned, op_id.as_str());
                commit_node(record, store, op_id, NodeState::RolledBack)?;
                info!(operation = %op_id, "node rolled back");
            }
            Err(error) => {
                rollback_failed = true;
                let message: String = error.to_string().chars().take(4096).collect();
                commit(record, store, |record| {
                    record.rollback_failure = Some(crate::TransactionFailure {
                        operation: Some(op_id.clone()),
                        message: message.clone(),
                    });
                    Ok(())
                })?;
                warn!(operation = %op_id, error = %error, "rollback failed");
                if node.kind.requires_verification() {
                    require_recovery(record, store)?;
                    return Ok(TransactionOutcome::RecoveryRequired);
                }
            }
        }
    }

    if rollback_failed {
        require_recovery(record, store)?;
        return Ok(TransactionOutcome::RecoveryRequired);
    }

    commit_phase(record, store, TransactionPhase::RolledBack)?;
    Ok(TransactionOutcome::RolledBack)
}

fn require_recovery<S: TransactionStore>(
    record: &mut TransactionRecord,
    store: &S,
) -> Result<(), TransactionError> {
    let resume = match record.phase {
        TransactionPhase::Prepared => RecoveryPhase::Prepared,
        TransactionPhase::Applying => RecoveryPhase::Applying,
        TransactionPhase::RollingBack => RecoveryPhase::RollingBack,
        TransactionPhase::RecoveryRequired { .. } => return Ok(()),
        _ => {
            return Err(TransactionError::InvalidState(
                "a completed transaction cannot require native recovery".into(),
            ));
        }
    };
    commit_phase(record, store, TransactionPhase::RecoveryRequired { resume })
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
    let state = match (&state, record.nodes.get(op_id)) {
        (
            NodeState::RolledBack,
            Some(
                NodeState::Applied { receipt }
                | NodeState::Verified { receipt }
                | NodeState::RollingBack { receipt },
            ),
        ) => NodeState::Compensated {
            receipt: receipt.clone(),
        },
        _ => state,
    };
    let point = match state {
        NodeState::Running => {
            boundary(Boundary::IntentBefore, op_id.as_str());
            Some(Boundary::IntentPersisted)
        }
        NodeState::Applied { .. } => Some(Boundary::ReceiptPersisted),
        NodeState::Verified { .. } => Some(Boundary::VerifiedPersisted),
        NodeState::RollingBack { .. } => Some(Boundary::RollbackIntentPersisted),
        NodeState::RolledBack | NodeState::Compensated { .. } => {
            Some(Boundary::RolledBackPersisted)
        }
        _ => None,
    };
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
    })?;
    if let Some(point) = point {
        boundary(point, op_id.as_str());
    }
    Ok(())
}

fn commit_phase<S: TransactionStore>(
    record: &mut TransactionRecord,
    store: &S,
    next: TransactionPhase,
) -> Result<(), TransactionError> {
    if next == TransactionPhase::Committed {
        boundary(Boundary::CommitBefore, &record.transaction_id.to_string());
    }
    commit(record, store, move |fresh| {
        if fresh.phase != next {
            fresh.phase = fresh
                .phase
                .transition(next)
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    })?;
    if next == TransactionPhase::Committed {
        boundary(Boundary::Committed, &record.transaction_id.to_string());
    }
    Ok(())
}

fn find_node<'a>(
    plan: &'a DeploymentPlan,
    id: &OperationId,
) -> Result<&'a DeploymentNode, TransactionError> {
    plan.nodes
        .iter()
        .find(|n| &n.id == id)
        .ok_or_else(|| TransactionError::InvalidState(format!("unknown node {id}")))
}
