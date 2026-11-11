use std::collections::BTreeSet;
use zup_plan::{DeploymentPlan, NodeKind, OperationId};

use thiserror::Error;
use zup_core::{ResourceKey, SelectedScope};
use zup_plan::{InstallationRecord, OwnedResource};

use zup_plan::{FileOperationKind, FilePrecondition};

#[derive(Debug, Error)]
pub enum OwnershipError {
    #[error("target identity")]
    Target,
    #[error("installation generation")]
    Generation,
    #[error("generation materialization is not authorized: {0}")]
    GenerationNode(OperationId),
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
    plan: &'a DeploymentPlan,
    scope: SelectedScope,
    ledger: Option<&InstallationRecord>,
    retired_owned_keys: &BTreeSet<ResourceKey>,
    claims: Option<&zup_plan::PersistentClaimSelection>,
) -> Result<BTreeSet<&'a ResourceKey>, OwnershipError> {
    if claims.is_some_and(|claims| {
        claims.installation != plan.installation
            || plan
                .install_directory
                .as_ref()
                .is_none_or(|directory| !directory.equivalent(&claims.directory))
    }) {
        return Err(OwnershipError::Generation);
    }
    if ledger.is_some_and(|record| {
        record.validate().is_err()
            || record.id != plan.installation
            || Some(record.current.selection()) != plan.observed
    }) || ledger.is_none() && plan.observed.is_some()
    {
        return Err(OwnershipError::Generation);
    }
    if ledger.is_some_and(|ledger| ledger.target != plan.target || ledger.scope != scope) {
        return Err(OwnershipError::Target);
    }
    if plan.transition.is_maintain()
        && ledger.is_none_or(|record| {
            record.current.requirements != plan.requirements
                || record.current.selected_components != plan.selected_components
                || record.current.preset != plan.preset
                || plan.install_directory.as_ref() != Some(&record.directory)
        })
    {
        return Err(OwnershipError::Generation);
    }
    if claims.into_iter().flat_map(|claims| claims.owners.iter()).any(|(id, owned)| {
        !plan.nodes.iter().any(|node| matches!(&node.kind, NodeKind::PersistentDirectory { operation }
            if &operation.intent.directory.id == id && operation.intent.previous.as_ref() == Some(owned)))
    }) {
        return Err(OwnershipError::Generation);
    }
    match (
        ledger.map(|ledger| ledger.commit.transaction),
        plan.observed_transaction,
    ) {
        (None, None) => {}
        (Some(current), Some(observed)) if current == observed => {}
        _ => return Err(OwnershipError::Generation),
    }
    let retired: BTreeSet<_> = plan.retired_keys.iter().collect();
    if retired.len() != plan.retired_keys.len()
        || retired_owned_keys.len() != retired.len()
        || retired_owned_keys
            .iter()
            .any(|key| ledger.is_none_or(|ledger| !ledger.current.resources.contains_key(key)))
        || (plan.transition.is_remove()
            && ledger
                .is_some_and(|ledger| retired_owned_keys.len() != ledger.current.resources.len()))
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
            NodeKind::Cleanup { operation } => ledger.is_some_and(|record| {
                operation
                    .validate_owned(
                        record,
                        plan.nodes.iter().find_map(|node| match &node.kind {
                            NodeKind::Retention { selection } => Some(selection.as_ref()),
                            _ => None,
                        }),
                    )
                    .is_ok()
            }),
            NodeKind::Retention { selection } => {
                ledger.is_some_and(|record| selection.validate_owned(record).is_ok())
            }
            NodeKind::PersistentDirectory { operation } => {
                operation.validate(plan.target, scope).is_ok()
                    && operation.intent.installation == plan.installation
                    && operation.intent.previous.as_ref()
                        == ledger
                            .and_then(|record| record.data.get(&operation.intent.directory.id))
                            .or_else(|| {
                                claims.and_then(|claims| {
                                    claims.owners.get(&operation.intent.directory.id)
                                })
                            })
            }
            NodeKind::Generation { intent, .. } => {
                let root = ledger
                    .map(|record| &record.directory)
                    .or(plan.install_directory.as_ref())
                    .ok_or_else(|| OwnershipError::GenerationNode(node.id.clone()))?;
                let directory = root
                    .join(format!(
                        "{}/{}",
                        intent.publication.namespace(),
                        intent.materialization.id()
                    ))
                    .map_err(|_| OwnershipError::GenerationNode(node.id.clone()))?;
                let retained = ledger
                    .into_iter()
                    .flat_map(|record| {
                        record
                            .retained
                            .iter()
                            .chain(std::iter::once(&record.current))
                    })
                    .flat_map(|snapshot| {
                        snapshot
                            .materialization
                            .iter()
                            .chain(snapshot.maintenance.iter())
                    })
                    .collect::<Vec<_>>();
                let allowed = match intent.preparation {
                    zup_plan::GenerationPreparation::Materialize => !retained
                        .iter()
                        .any(|previous| previous.id() == intent.materialization.id()),
                    zup_plan::GenerationPreparation::ReuseVerified => retained
                        .iter()
                        .any(|previous| **previous == intent.materialization),
                };
                if !allowed
                    || intent.materialization.installation() != plan.installation
                    || intent.materialization.directory() != &directory
                    || matches!(intent.publication, zup_plan::GenerationPublication::Application { previous, .. } if previous != plan.observed)
                {
                    return Err(OwnershipError::GenerationNode(node.id.clone()));
                }
                true
            }
            NodeKind::RetainFile { key } => {
                match ledger.and_then(|record| record.current.resources.get(key)) {
                    Some(OwnedResource::File {
                        instance,
                        destination,
                        sha256,
                        size,
                        ..
                    }) => {
                        node.meta
                            .destination
                            .as_ref()
                            .is_some_and(|path| path.equivalent(destination))
                            && node.meta.expected_sha256 == Some(*sha256)
                            && node.meta.expected_size == Some(*size)
                            && matches!(node.meta.file_precondition, Some(FilePrecondition::Exact { object, size: expected_size, sha256: expected_hash })
                            if object == instance.object && expected_size == *size && expected_hash == *sha256)
                    }
                    _ => false,
                }
            }
            NodeKind::FileMutation { key, delta } => {
                let owned = ledger.and_then(|ledger| ledger.current.resources.get(key));
                node.meta.source_relative.is_some()
                    && owned.is_none_or(|owned| matches!(owned, OwnedResource::File { destination, .. }
                        if node.meta.destination.as_ref().is_some_and(|planned| planned.equivalent(destination))))
                    && match (delta, owned) {
                        (FileOperationKind::Create, None) => {
                            matches!(node.meta.file_precondition, Some(FilePrecondition::Absent))
                        }
                        (
                            FileOperationKind::Replace,
                            Some(OwnedResource::File { instance, sha256, size, .. }),
                        ) => {
                            matches!(node.meta.file_precondition,
                                Some(FilePrecondition::Exact { object, size: found_size, sha256: found_hash })
                                if object == instance.object && found_size == *size && found_hash == *sha256)
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
                                instance,
                                sha256,
                                size,
                                executable,
                                ..
                            }),
                        ) => {
                            matches!(
                                node.meta.file_precondition,
                                Some(FilePrecondition::Exact { object, .. }) if object == instance.object
                            ) && node.meta.expected_sha256 == Some(*sha256)
                                && node.meta.expected_size == Some(*size)
                                && node.meta.executable == Some(*executable)
                        }
                        _ => false,
                    }
            }
            NodeKind::FileRemoval { key } => match (
                node.meta.removal.as_ref(),
                ledger.and_then(|ledger| ledger.current.resources.get(key)),
            ) {
                (
                    Some(removal),
                    Some(OwnedResource::File {
                        instance,
                        destination,
                        sha256,
                        size,
                        ..
                    }),
                ) => {
                    retired.contains(key)
                        && removal.instance == *instance
                        && removal.key == *key
                        && removal.destination == *destination
                        && removal.sha256 == *sha256
                        && removal.size == *size
                }
                _ => false,
            },
            NodeKind::ServiceControl { operation } => {
                match ledger.and_then(|ledger| ledger.current.resources.get(&operation.key)) {
                    Some(OwnedResource::Service(zup_plan::OwnedService {
                        installed,
                        privilege,
                        ..
                    })) => {
                        *privilege == operation.privilege
                            && installed.as_ref() == &operation.owned_configuration
                    }
                    None => {
                        operation.owned_configuration == zup_plan::ServiceState::Absent
                            && operation.stage == zup_plan::ServiceControlStage::Restore
                    }
                    _ => false,
                }
            }
            NodeKind::NativeService { operation } => {
                let owned =
                    match ledger.and_then(|ledger| ledger.current.resources.get(operation.key())) {
                        Some(OwnedResource::Service(service)) => Some(service),
                        None => None,
                        _ => return Err(OwnershipError::File(node.id.clone())),
                    };
                match operation.as_ref() {
                    zup_plan::NativeServiceOperation::Configure { previous, .. } => {
                        previous.as_deref() == owned
                    }
                    zup_plan::NativeServiceOperation::Remove { service, .. } => {
                        retired.contains(operation.key()) && owned == Some(service.as_ref())
                    }
                }
            }
            NodeKind::BackendOperation { .. }
            | NodeKind::NativeCache { .. }
            | NodeKind::BackendRemoval { .. }
            | NodeKind::Barrier
            | NodeKind::ReserveInstallation
            | NodeKind::PublishInstallation
            | NodeKind::StageFile { .. } => true,
        };
        if !valid {
            return Err(OwnershipError::File(node.id.clone()));
        }
    }
    Ok(retired)
}
