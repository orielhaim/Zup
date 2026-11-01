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
    if record.phase != TransactionPhase::Committed {
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
    #[error("only a committed uninstall may retire transaction history")]
    Retirement,
    #[error("transaction target differs from the installed target")]
    Target,
    #[error("unfinished transaction {0} requires recovery")]
    RecoveryRequired(crate::TransactionId),
}

/// Validate the entire history before allowing publication or cleanup of any record.
pub fn settled_records(
    store: &crate::FilesystemTransactionStore,
    app_id: &zup_core::AppId,
    scope: zup_core::SelectedScope,
    ledger: Option<&zup_plan::InstallLedger>,
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
            TransactionPhase::Committed | TransactionPhase::RolledBack => settled.push(record),
            _ => return Err(ReconciliationError::RecoveryRequired(record.transaction_id)),
        }
    }
    settled.sort_by_key(|record| record.transaction_id.as_uuid());
    let (mut pending, mut ordered): (Vec<_>, Vec<_>) = settled.into_iter().partition(|record| {
        record.phase == TransactionPhase::Committed
            && record.reconciliation != ReconciliationState::Complete
    });
    let mut generation = ledger.map(|ledger| ledger.committed_transaction);
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
                .filter(|(_, record)| {
                    record.plan.observed_transaction == generation
                        || (record.plan.uninstall && generation.is_none())
                })
                .map(|(index, _)| index)
                .collect();
            match candidates.as_slice() {
                [index] => *index,
                _ => return Err(ReconciliationError::Generations),
            }
        };
        let record = pending.remove(index);
        generation = if record.plan.uninstall {
            None
        } else {
            Some(record.transaction_id)
        };
        ordered.push(record);
    }
    Ok(ordered)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LedgerPublication {
    Advance,
    Current,
    Settled,
}

#[derive(Debug, thiserror::Error)]
pub enum PublicationError {
    #[error("only a committed transaction can publish ownership")]
    Uncommitted,
    #[error("committed transaction and ledger identities differ")]
    Identity,
    #[error("committed transaction does not follow the installed generation")]
    Generation,
}

pub fn ledger_publication(
    record: &TransactionRecord,
    previous: Option<&zup_plan::InstallLedger>,
) -> Result<LedgerPublication, PublicationError> {
    if record.phase != TransactionPhase::Committed {
        return Err(PublicationError::Uncommitted);
    }
    if record.reconciliation == ReconciliationState::Complete {
        return Ok(LedgerPublication::Settled);
    }
    if previous.is_some_and(|ledger| {
        ledger.app_id != record.app_id
            || ledger.scope != record.scope
            || ledger.target != record.target
    }) {
        return Err(PublicationError::Identity);
    }
    let generation = previous.map(|ledger| ledger.committed_transaction);
    if generation == Some(record.transaction_id) {
        return Ok(LedgerPublication::Current);
    }
    if generation == record.plan.observed_transaction
        || (record.plan.uninstall && generation.is_none())
    {
        return Ok(LedgerPublication::Advance);
    }
    Err(PublicationError::Generation)
}

pub fn retired_records(
    store: &crate::FilesystemTransactionStore,
    uninstall: &TransactionRecord,
) -> Result<Vec<TransactionRecord>, ReconciliationError> {
    if uninstall.phase != TransactionPhase::Committed || !uninstall.plan.uninstall {
        return Err(ReconciliationError::Retirement);
    }
    let records = settled_records(store, &uninstall.app_id, uninstall.scope, None)?;
    let retired: Vec<_> = records
        .into_iter()
        .filter(|record| record.transaction_id != uninstall.transaction_id)
        .collect();
    if let Some(record) = retired.iter().find(|record| {
        record.phase == TransactionPhase::Committed
            && record.reconciliation != ReconciliationState::Complete
    }) {
        return Err(ReconciliationError::RecoveryRequired(record.transaction_id));
    }
    Ok(retired)
}
