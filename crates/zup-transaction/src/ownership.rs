use std::collections::BTreeSet;

use thiserror::Error;
use zup_core::{ResourceKey, SelectedScope};
use zup_plan::{InstallLedger, OwnedResource};

use crate::{NodeKind, OperationId, TransactionPlan};
use zup_plan::{FileOperationKind, FilePrecondition};

#[derive(Debug, Error)]
pub enum OwnershipError {
    #[error("target identity")]
    Target,
    #[error("installation generation")]
    Generation,
    #[error("resource authority exceeds installation scope: {0}")]
    Authority(OperationId),
    #[error("retired resources")]
    Retired,
    #[error("{0}")]
    File(OperationId),
}

/// Checks platform-independent file ownership and retirement.
/// Requires structural validation and native key normalization; backend effects stay caller-owned.
pub fn validate_ownership<'a>(
    plan: &'a TransactionPlan,
    scope: SelectedScope,
    ledger: Option<&InstallLedger>,
    retired_ledger_keys: &BTreeSet<ResourceKey>,
) -> Result<BTreeSet<&'a ResourceKey>, OwnershipError> {
    if ledger.is_some_and(|ledger| ledger.target != plan.target || ledger.scope != scope) {
        return Err(OwnershipError::Target);
    }
    match (
        ledger.map(|ledger| ledger.committed_transaction),
        plan.observed_transaction,
    ) {
        (None, None) => {}
        (Some(current), Some(observed)) if current == observed => {}
        _ => return Err(OwnershipError::Generation),
    }
    let retired: BTreeSet<_> = plan.retired_keys.iter().collect();
    if retired.len() != plan.retired_keys.len()
        || retired_ledger_keys.len() != retired.len()
        || retired_ledger_keys
            .iter()
            .any(|key| ledger.is_none_or(|ledger| !ledger.resources.contains_key(key)))
        || (plan.uninstall
            && ledger.is_some_and(|ledger| retired_ledger_keys.len() != ledger.resources.len()))
    {
        return Err(OwnershipError::Retired);
    }
    for node in &plan.nodes {
        if node
            .meta
            .privilege
            .is_some_and(|privilege| privilege != scope.authorization())
            || node
                .meta
                .removal
                .as_ref()
                .is_some_and(|removal| removal.scope != scope)
        {
            return Err(OwnershipError::Authority(node.id.clone()));
        }
        let valid = match &node.kind {
            NodeKind::FileMutation { key, delta } => {
                let owned = ledger.and_then(|ledger| ledger.resources.get(key));
                node.meta.source_relative.is_some()
                    && match (delta, owned) {
                        (FileOperationKind::Create, None) => {
                            matches!(node.meta.file_precondition, Some(FilePrecondition::Absent))
                        }
                        (
                            FileOperationKind::Replace,
                            Some(OwnedResource::File { sha256, size, .. }),
                        ) => {
                            matches!(node.meta.file_precondition,
                                Some(FilePrecondition::Exact { size: found_size, sha256: found_hash })
                                if found_size == *size && found_hash == *sha256)
                        }
                        (
                            FileOperationKind::RestoreOwned,
                            Some(OwnedResource::File {
                                sha256,
                                size,
                                executable,
                                ..
                            }),
                        ) => {
                            matches!(node.meta.file_precondition, Some(FilePrecondition::Absent))
                                && node.meta.expected_sha256 == Some(*sha256)
                                && node.meta.expected_size == Some(*size)
                                && node.meta.executable == Some(*executable)
                        }
                        (
                            FileOperationKind::RepairOwned,
                            Some(OwnedResource::File {
                                sha256,
                                size,
                                executable,
                                ..
                            }),
                        ) => {
                            matches!(
                                node.meta.file_precondition,
                                Some(FilePrecondition::Exact { .. })
                            ) && node.meta.expected_sha256 == Some(*sha256)
                                && node.meta.expected_size == Some(*size)
                                && node.meta.executable == Some(*executable)
                        }
                        _ => false,
                    }
            }
            NodeKind::FileRemoval { key } => match (
                node.meta.removal.as_ref(),
                ledger.and_then(|ledger| ledger.resources.get(key)),
            ) {
                (
                    Some(removal),
                    Some(OwnedResource::File {
                        destination,
                        sha256,
                        size,
                        ..
                    }),
                ) => {
                    retired.contains(key)
                        && removal.key == *key
                        && removal.destination == *destination
                        && removal.sha256 == *sha256
                        && removal.size == *size
                }
                _ => false,
            },
            NodeKind::ServiceControl { operation } => {
                match ledger.and_then(|ledger| ledger.resources.get(&operation.key)) {
                    Some(OwnedResource::Service {
                        installed,
                        privilege,
                        ..
                    }) => {
                        *privilege == operation.privilege
                            && installed == &operation.owned_configuration
                    }
                    None => {
                        operation.owned_configuration == zup_plan::ServiceState::Absent
                            && operation.stage == crate::ServiceControlStage::Restore
                    }
                    _ => false,
                }
            }
            NodeKind::BackendOperation { .. }
            | NodeKind::BackendRemoval { .. }
            | NodeKind::Barrier
            | NodeKind::StageFile { .. } => true,
        };
        if !valid {
            return Err(OwnershipError::File(node.id.clone()));
        }
    }
    Ok(retired)
}
