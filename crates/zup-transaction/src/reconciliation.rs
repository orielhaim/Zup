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
