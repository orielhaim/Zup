use std::path::Path;

use crate::{TransactionPhase, TransactionRecord, TransactionStore};

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ReconciliationState {
    Pending { failure: Option<String> },
    Complete,
}

pub fn reconcile_committed<T>(
    store: &dyn TransactionStore,
    record: &TransactionRecord,
    state_root: &Path,
    complete: impl FnOnce(&TransactionRecord) -> Result<T, String>,
) -> Result<T, String> {
    if record.phase != TransactionPhase::Committed || !record.cleanup_complete() {
        return Err("only committed changes can be reconciled".into());
    }
    let durable = store
        .load(&record.transaction_id)
        .map_err(|error| error.to_string())?;
    if durable.app_id != record.app_id
        || durable.scope != record.scope
        || durable.target != record.target
        || durable.app_version != record.app_version
        || durable.release != record.release
        || durable.plan_hash != record.plan_hash
        || durable.phase != TransactionPhase::Committed
    {
        return Err("committed reconciliation identity mismatch".into());
    }
    let result = complete(&durable);
    let error = result.as_ref().err().map(|error| {
        format!(
            "{error}\nTransaction: {}\nApplication: {}\nScope: {}\nTransaction state: {}",
            record.transaction_id,
            record.app_id,
            record.scope,
            state_root.display()
        )
    });
    if durable.reconciliation == ReconciliationState::Complete {
        return result.map_err(|_| error.expect("failed reconciliation has a diagnostic"));
    }
    let updated = store
        .update(&record.transaction_id, &mut |fresh| {
            fresh.reconciliation = match &error {
                None => ReconciliationState::Complete,
                Some(error) => ReconciliationState::Pending {
                    failure: match &fresh.reconciliation {
                        ReconciliationState::Pending {
                            failure: Some(original),
                        } => Some(original.clone()),
                        _ => Some(error.clone()),
                    },
                },
            };
            Ok(())
        })
        .map_err(|persist| {
            format!(
                "{}\nCannot persist reconciliation state: {persist}",
                error.as_deref().unwrap_or("committed work completed")
            )
        })?;
    match (result, updated.reconciliation) {
        (Ok(value), _) => Ok(value),
        (
            Err(latest),
            ReconciliationState::Pending {
                failure: Some(original),
            },
        ) => {
            if original.starts_with(&latest) {
                Err(original)
            } else {
                Err(format!("{original}\nLatest attempt: {latest}"))
            }
        }
        (Err(error), _) => Err(error),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ReconciliationError {
    #[error(transparent)]
    Store(#[from] crate::StoreError),
    #[error("committed history has ambiguous or disconnected generations")]
    Generations,
    #[error("transaction target differs from the installed target")]
    Target,
    #[error("unfinished transaction {0} requires recovery")]
    RecoveryRequired(zup_core::TransactionId),
}

/// Validate the entire history before allowing publication or cleanup of any record.
pub fn settled_records(
    store: &crate::FilesystemTransactionStore,
    app_id: &zup_core::AppId,
    scope: zup_core::SelectedScope,
    ledger: Option<&zup_plan::InstallationRecord>,
) -> Result<Vec<TransactionRecord>, ReconciliationError> {
    let mut settled = Vec::new();
    for id in store.list_ids()? {
        let record = store.load(&id)?;
        if record.app_id != *app_id || record.scope != scope {
            continue;
        }
        if record.phase == TransactionPhase::Committed
            && record.reconciliation != ReconciliationState::Complete
            && ledger.is_some_and(|ledger| ledger.target != record.target)
        {
            return Err(ReconciliationError::Target);
        }
        match record.phase {
            TransactionPhase::Committed if !record.cleanup_complete() => {
                return Err(ReconciliationError::RecoveryRequired(record.transaction_id));
            }
            TransactionPhase::Committed | TransactionPhase::RolledBack => settled.push(record),
            _ => return Err(ReconciliationError::RecoveryRequired(record.transaction_id)),
        }
    }
    settled.sort_by_key(|record| record.transaction_id.as_uuid());
    let (mut pending, mut ordered): (Vec<_>, Vec<_>) = settled.into_iter().partition(|record| {
        record.phase == TransactionPhase::Committed
            && record.reconciliation != ReconciliationState::Complete
    });
    if let Some(installation) = ledger {
        let current = pending
            .iter()
            .chain(&ordered)
            .find(|record| record.transaction_id == installation.commit.transaction)
            .ok_or(ReconciliationError::Generations)?;
        crate::verify_installation_evidence(installation, current)
            .map_err(|_| ReconciliationError::Generations)?;
        for snapshot in installation
            .retained
            .iter()
            .chain(
                installation
                    .retired_deployments
                    .iter()
                    .map(|retired| &retired.snapshot),
            )
            .chain(std::iter::once(&installation.current))
        {
            let record = pending
                .iter()
                .chain(&ordered)
                .find(|record| record.transaction_id == snapshot.transaction)
                .ok_or(ReconciliationError::Generations)?;
            if record.phase != TransactionPhase::Committed
                || record.plan.installation != installation.id
                || record.plan_hash != snapshot.plan_hash
                || record.target != installation.target
            {
                return Err(ReconciliationError::Generations);
            }
            if let Some(index) = pending
                .iter()
                .position(|record| record.transaction_id == snapshot.transaction)
            {
                ordered.push(pending.remove(index));
            }
        }
    }
    if let Some(installation) = ledger {
        for retired in &installation.retired_deployments {
            let retirement = pending
                .iter()
                .chain(&ordered)
                .find(|record| record.transaction_id == retired.retired_by)
                .ok_or(ReconciliationError::Generations)?;
            if retirement.phase != TransactionPhase::Committed || retirement.plan.installation != installation.id
                || !retirement.plan.nodes.iter().any(|node| matches!(&node.kind, zup_plan::NodeKind::Retention { selection }
                    if selection.retire.iter().any(|selected| *selected == zup_plan::RetiredDeploymentSelection::from(&retired.snapshot)))) {
                return Err(ReconciliationError::Generations);
            }
        }
    }
    let mut generation = ledger.map(|ledger| ledger.commit.transaction);
    while !pending.is_empty() {
        let current = pending
            .iter()
            .position(|record| Some(record.transaction_id) == generation);
        let index = if let Some(current) = current {
            current
        } else {
            let candidates: Vec<_> = pending
                .iter()
                .enumerate()
                .filter(|(_, record)| record.plan.observed_transaction == generation)
                .map(|(index, _)| index)
                .collect();
            match candidates.as_slice() {
                [index] => *index,
                _ => return Err(ReconciliationError::Generations),
            }
        };
        let record = pending.remove(index);
        generation = Some(record.transaction_id);
        ordered.push(record);
    }
    Ok(ordered)
}
