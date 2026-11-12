use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};
use zup_plan::{DeploymentNode, DeploymentPlan, NodeKind};

use zup_core::{AppId, ResourceKey, SelectedScope};
use zup_plan::{InstallationRecord, OwnedResource};

use crate::OwnershipError;

pub fn ledger_path(root: &Path, app_id: &AppId, scope: SelectedScope) -> PathBuf {
    root.join("installations").join(format!(
        "{}-{}.json",
        zup_core::hash_bytes(app_id.as_str().as_bytes()).to_hex(),
        crate::scope_name(scope)
    ))
}

pub fn validate_installation_record(
    ledger: &InstallationRecord,
    app_id: &AppId,
    scope: SelectedScope,
) -> bool {
    if ledger.validate().is_err() {
        return false;
    }
    let graph = ledger
        .current
        .resources
        .iter()
        .filter_map(|(key, owned)| match owned {
            OwnedResource::Service(zup_plan::OwnedService {
                requires_services, ..
            }) => Some((
                key.clone(),
                zup_core::service_dependency_keys(requires_services, ledger.target),
            )),
            _ => None,
        })
        .collect();
    if zup_core::validate_service_graph(&graph).is_err() {
        return false;
    }
    let mut identities = BTreeSet::new();
    ledger.app_id == *app_id
        && ledger.scope == scope
        && ledger.directory.target() == &ledger.target
        && ledger.current.resources.iter().all(|(key, owned)| {
            if owned.privilege() != scope.authorization() || !identities.insert(key.canonical_for(&ledger.target)) { return false; }
            match owned {
            zup_plan::OwnedResource::File {
                destination,
                created_directories,
                ..
            } => {
                let identity_matches = match key {
                    ResourceKey::File { id: zup_core::FileResourceId::Payload { .. } } | ResourceKey::File { id: zup_core::FileResourceId::Plugin { .. } } => true,
                    ResourceKey::File { id: zup_core::FileResourceId::Native { .. } } => ledger.target.operating_system() == zup_core::TargetOperatingSystem::Linux,
                    _ => false,
                };
                destination.target() == &ledger.target && identity_matches
                    && created_directories.iter().all(|directory| {
                        directory.target() == &ledger.target
                            && directory.parent().is_some()
                            && directory != destination
                            && destination.starts_with(directory)
                    })
            }
            OwnedResource::Launcher { launcher_path, previous, installed, .. } =>
                matches!(key, ResourceKey::Launcher { .. }) && launcher_path.target() == &ledger.target
                && [previous, installed].iter().all(|state| match state {
                    zup_plan::LauncherState::Absent => true,
                    zup_plan::LauncherState::Launcher { target, working_directory, .. } =>
                        target.target() == &ledger.target && working_directory.as_ref().is_none_or(|path| path.target() == &ledger.target),
                }),
            OwnedResource::PathEntry { value, .. } => matches!(key, ResourceKey::PathEntry { .. }) && value.target() == &ledger.target,
            OwnedResource::Service(zup_plan::OwnedService { registration, name, installed, requires_files, requires_services, privilege, timeouts }) =>
                matches!(key, ResourceKey::Service { .. }) && !name.is_empty()
                  && registration.belongs_to(ledger.id, ledger.target)
                  && requires_services.len() <= 256
                  && requires_services.iter().collect::<BTreeSet<_>>().len() == requires_services.len()
                  && requires_services.iter().all(|id| matches!(ledger.current.resources.get(&ResourceKey::Service { id: id.clone() }.canonical_for(&ledger.target)), Some(OwnedResource::Service(zup_plan::OwnedService { privilege: dependency_privilege, .. })) if dependency_privilege == privilege))
                  && requires_files.len() <= 256
                  && requires_files.iter().collect::<BTreeSet<_>>().len() == requires_files.len()
                  && requires_files.iter().all(|path| path.target() == &ledger.target
                      && ledger.owns_file(path, *privilege))
                && matches!(installed.as_ref(), zup_plan::ServiceState::Registration { command, execution, native_timeouts, .. }
                    if timeouts.validate_for(execution).is_ok() && timeouts.native() == *native_timeouts
                    && command.executable.target() == &ledger.target && execution.validate_for(ledger.target).is_ok() && native_timeouts.validate_for(execution).is_ok()),
            OwnedResource::Protocol { previous, installed, .. } =>
                matches!(key, ResourceKey::Protocol { .. }) && [previous, installed].iter().all(|state| match state {
                    zup_plan::ProtocolState::Absent => true,
                    zup_plan::ProtocolState::Registration { command } => command.executable.target() == &ledger.target,
                }),
            OwnedResource::FileAssociation { previous, installed, .. } =>
                matches!(key, ResourceKey::FileAssociation { .. }) && [previous, installed].iter().all(|state| match state {
                    zup_plan::FileAssociationState::Absent => true,
                    zup_plan::FileAssociationState::Registration { command, .. } => command.executable.target() == &ledger.target,
                }),
            OwnedResource::Extension { .. } => matches!(key, ResourceKey::FileAssociationExtension { .. }),
            OwnedResource::Backend { id, payload, .. } => matches!(key, ResourceKey::Backend { id: identity } if identity == id)
                && payload.len() <= zup_plan::MAX_BACKEND_PAYLOAD_BYTES,
        }})
}

/// Native backends translate their operation keys into the resource identity stored in the ledger.
pub fn retired_owned_keys<E>(
    plan: &DeploymentPlan,
    mut backend_key: impl FnMut(&DeploymentNode) -> Result<ResourceKey, E>,
) -> Result<BTreeSet<ResourceKey>, E>
where
    E: From<OwnershipError>,
{
    let mut keys = BTreeSet::new();
    for key in &plan.retired_keys {
        let ledger_key = match plan.nodes.iter().find(|node| matches!(
            &node.kind,
            NodeKind::BackendOperation { key: node_key, .. } | NodeKind::BackendRemoval { key: node_key } if node_key == key
        )) {
            Some(node) => backend_key(node)?,
            None => key.clone(),
        };
        if !keys.insert(ledger_key) {
            return Err(OwnershipError::Retired.into());
        }
    }
    Ok(keys)
}

pub fn valid_file_key(key: &ResourceKey, target: &zup_core::Target) -> bool {
    match key {
        ResourceKey::File {
            id: zup_core::FileResourceId::Payload { .. },
        }
        | ResourceKey::File {
            id: zup_core::FileResourceId::Plugin { .. },
        } => true,
        ResourceKey::File {
            id: zup_core::FileResourceId::Native { .. },
        } => target.operating_system() == zup_core::TargetOperatingSystem::Linux,
        _ => false,
    }
}

#[derive(Debug, thiserror::Error)]
#[error("verified operation receipts do not match the deployment intent")]
pub struct OwnershipProjectionError;

pub fn prepare_installation_record(
    record: &crate::TransactionRecord,
    previous: Option<&InstallationRecord>,
) -> Result<InstallationRecord, OwnershipProjectionError> {
    record.validate().map_err(|_| OwnershipProjectionError)?;
    let plan_hash = record.plan.fingerprint();
    let directory = record
        .plan
        .install_directory
        .clone()
        .ok_or(OwnershipProjectionError)?;
    let revision = match record.plan.observed {
        Some(observed) if record.plan.transition.is_maintain() => observed.revision,
        Some(observed) => observed
            .revision
            .next()
            .map_err(|_| OwnershipProjectionError)?,
        None => zup_core::ActivationRevision::FIRST,
    };
    if (record.failure.is_some() && record.resolution.is_none())
        || record.plan.nodes.iter().any(|node| {
            !node.kind.is_post_commit()
                && node.kind.requires_verification()
                && !matches!(
                    record.nodes.get(&node.id),
                    Some(crate::NodeState::Verified { .. })
                )
        })
    {
        return Err(OwnershipProjectionError);
    }
    if let Some(previous) = previous {
        previous.validate().map_err(|_| OwnershipProjectionError)?;
        if previous.id != record.plan.installation
            || previous.app_id != record.app_id
            || previous.scope != record.scope
            || previous.target != record.target
            || previous.directory != directory
        {
            return Err(OwnershipProjectionError);
        }
        if previous.commit.transaction == record.transaction_id {
            if previous.commit.plan_hash != plan_hash || previous.current.revision != revision {
                return Err(OwnershipProjectionError);
            }
            return Ok(previous.clone());
        }
        if Some(previous.current.selection()) != record.plan.observed
            || Some(previous.commit.transaction) != record.plan.observed_transaction
        {
            return Err(OwnershipProjectionError);
        }
    } else if record.plan.observed.is_some()
        || record.plan.observed_transaction.is_some()
        || !record.plan.transition.is_deploy()
    {
        return Err(OwnershipProjectionError);
    }
    if record.plan.transition.is_maintain() {
        let previous = previous.ok_or(OwnershipProjectionError)?;
        if previous.current.requirements != record.plan.requirements
            || previous.current.version != record.app_version
            || previous.current.release != record.release
            || previous.current.selected_components != record.plan.selected_components
            || previous.current.preset != record.plan.preset
        {
            return Err(OwnershipProjectionError);
        }
    }
    let mut retained = previous
        .map(|previous| previous.retained.clone())
        .unwrap_or_default();
    if let Some(previous) = previous
        && !record.plan.transition.is_maintain()
        && previous.state == zup_plan::InstallationState::Installed
    {
        retained.push(previous.current.clone());
    }
    let materialization = record.plan.nodes.iter().find_map(|node| match &node.kind {
        NodeKind::Generation {
            operation: zup_plan::GenerationOperation::Activate,
            intent,
        } => Some(intent.materialization.clone()),
        _ => None,
    });
    let maintenance = record.plan.nodes.iter().find_map(|node| match &node.kind {
        NodeKind::Generation { intent, .. }
            if intent.publication == zup_plan::GenerationPublication::Maintenance =>
        {
            Some(intent.materialization.clone())
        }
        _ => None,
    });
    let mut retired_deployments = previous
        .map(|record| record.retired_deployments.clone())
        .unwrap_or_default();
    if let Some(selection) = record.plan.nodes.iter().find_map(|node| match &node.kind {
        NodeKind::Retention { selection } => Some(selection),
        _ => None,
    }) {
        let previous = previous.ok_or(OwnershipProjectionError)?;
        selection
            .validate_owned(previous)
            .map_err(|_| OwnershipProjectionError)?;
        retired_deployments.retain(|retired| {
            !selection.forget.iter().any(|selected| {
                *selected == zup_plan::RetiredDeploymentSelection::from(&retired.snapshot)
            })
        });
        retained.retain(|snapshot| {
            if selection
                .retire
                .iter()
                .any(|selected| selected.revision == snapshot.revision)
            {
                retired_deployments.push(zup_plan::RetiredDeployment {
                    snapshot: snapshot.clone(),
                    retired_by: record.transaction_id,
                });
                false
            } else {
                true
            }
        });
        retired_deployments.sort_by_key(|retired| retired.snapshot.revision);
    }
    let current = if record.plan.transition.is_maintain() {
        previous.ok_or(OwnershipProjectionError)?.current.clone()
    } else {
        zup_plan::DeploymentSnapshot {
            requirements: record.plan.requirements.clone(),
            revision,
            transaction: record.transaction_id,
            plan_hash,
            version: record.app_version.clone(),
            selected_components: record.plan.selected_components.clone(),
            release: record.release.clone(),
            preset: record.plan.preset.clone(),
            materialization,
            maintenance,
            resources: previous
                .map(|previous| previous.current.resources.clone())
                .unwrap_or_default(),
        }
    };
    let mut installation = InstallationRecord {
        commit: zup_plan::InstallationCommit {
            transaction: record.transaction_id,
            plan_hash,
        },
        retired_deployments,
        data: previous
            .map(|previous| previous.data.clone())
            .unwrap_or_default(),
        id: record.plan.installation,
        state: if record.plan.transition.is_maintain() {
            previous.ok_or(OwnershipProjectionError)?.state
        } else if record.plan.transition.is_remove() {
            zup_plan::InstallationState::Removed
        } else {
            zup_plan::InstallationState::Installed
        },
        app_id: record.app_id.clone(),
        scope: record.scope,
        target: record.target,
        directory,
        current,
        retained,
    };
    project_committed_files(record, &mut installation)?;
    for node in &record.plan.nodes {
        if let NodeKind::PersistentDirectory { operation } = &node.kind
            && operation.stage == zup_plan::PersistentDirectoryStage::Access
        {
            let Some(crate::OperationReceipt::PersistentDirectory(receipt)) =
                record.receipt(&node.id)
            else {
                return Err(OwnershipProjectionError);
            };
            let owned = &receipt.owned;
            if receipt.stage != operation.stage
                || owned.directory != operation.intent.directory
                || operation.intent.previous.as_ref().is_some_and(|previous| {
                    previous.ownership != owned.ownership || previous.origin != owned.origin
                })
                || operation.intent.previous.is_none() && owned.origin != record.transaction_id
            {
                return Err(OwnershipProjectionError);
            }
            installation
                .data
                .insert(owned.directory.id.clone(), owned.clone());
        }
    }

    for node in &record.plan.nodes {
        let NodeKind::NativeService { operation } = &node.kind else {
            continue;
        };
        let Some(crate::OperationReceipt::NativeService(receipt)) = record.receipt(&node.id) else {
            return Err(OwnershipProjectionError);
        };
        if !receipt.matches(operation) {
            return Err(OwnershipProjectionError);
        }
        match operation.as_ref() {
            zup_plan::NativeServiceOperation::Configure { .. } => {
                installation.current.resources.insert(
                    operation.key().clone(),
                    OwnedResource::Service(operation.contract()),
                );
            }
            zup_plan::NativeServiceOperation::Remove { .. } => {
                installation.current.resources.remove(operation.key());
            }
        }
    }
    Ok(installation)
}

pub fn project_committed_files(
    record: &crate::TransactionRecord,
    ledger: &mut InstallationRecord,
) -> Result<(), OwnershipProjectionError> {
    use crate::OperationReceipt;
    use zup_plan::OwnedResource;
    let path = |text: &str| {
        zup_core::TargetPath::new(record.target, text).map_err(|_| OwnershipProjectionError)
    };
    if record.release.is_some() || ledger.current.version != record.app_version {
        ledger.current.release = record.release.clone();
    }
    ledger.current.version = record.app_version.clone();
    ledger.current.selected_components = record.plan.selected_components.clone();
    if record
        .plan
        .install_directory
        .as_ref()
        .is_some_and(|path| path != &ledger.directory)
    {
        return Err(OwnershipProjectionError);
    }
    ledger.current.preset = record.plan.preset.clone();
    for node in &record.plan.nodes {
        if let NodeKind::RetainFile { key } = &node.kind {
            let Some(OperationReceipt::RetainedFile {
                object,
                destination: observed,
                size: observed_size,
                sha256: observed_hash,
            }) = record.receipt(&node.id)
            else {
                return Err(OwnershipProjectionError);
            };
            let Some(OwnedResource::File {
                instance,
                destination,
                source_relative,
                replacement,
                sha256,
                size,
                executable,
                privilege,
                ..
            }) = ledger.current.resources.get_mut(key)
            else {
                return Err(OwnershipProjectionError);
            };
            if instance.object != *object
                || !destination.equivalent(observed)
                || *sha256 != *observed_hash
                || *size != *observed_size
                || node.meta.destination.as_ref() != Some(observed)
                || node.meta.expected_sha256 != Some(*observed_hash)
                || node.meta.expected_size != Some(*observed_size)
                || !matches!(node.meta.file_precondition, Some(zup_plan::FilePrecondition::Exact { object: expected, size: expected_size, sha256: expected_hash })
                    if expected == *object && expected_size == *observed_size && expected_hash == *observed_hash)
            {
                return Err(OwnershipProjectionError);
            }
            *destination = observed.clone();
            *source_relative = node
                .meta
                .source_relative
                .clone()
                .ok_or(OwnershipProjectionError)?;
            *replacement = node
                .meta
                .fixed_replacement
                .ok_or(OwnershipProjectionError)?;
            *executable = node.meta.executable.ok_or(OwnershipProjectionError)?;
            *privilege = node.meta.privilege.ok_or(OwnershipProjectionError)?;
            continue;
        }
        let NodeKind::FileMutation { key, .. } = &node.kind else {
            continue;
        };
        let receipt = record.receipt(&node.id).ok_or(OwnershipProjectionError)?;
        let mut directories = match ledger.current.resources.get(key) {
            Some(OwnedResource::File {
                created_directories,
                ..
            }) => created_directories.clone(),
            _ => Vec::new(),
        };
        let (destination, object, digest, size) = match receipt {
            OperationReceipt::CreateFile {
                object,
                destination,
                installed_sha256,
                installed_size,
                created_directories,
                ..
            } => {
                for directory in created_directories {
                    let directory = path(directory)?;
                    if !directories.contains(&directory) {
                        directories.push(directory);
                    }
                }
                (destination, object, installed_sha256, *installed_size)
            }
            OperationReceipt::ReplaceFile {
                object,
                destination,
                new_sha256,
                new_size,
                ..
            } => (destination, object, new_sha256, *new_size),
            _ => return Err(OwnershipProjectionError),
        };
        let sha256 = *digest;
        if node.meta.expected_sha256 != Some(sha256) || node.meta.expected_size != Some(size) {
            return Err(OwnershipProjectionError);
        }
        ledger.current.resources.insert(
            key.clone(),
            OwnedResource::File {
                instance: zup_core::FixedFileInstance {
                    origin: record.transaction_id,
                    object: *object,
                },
                replacement: node
                    .meta
                    .fixed_replacement
                    .ok_or(OwnershipProjectionError)?,
                destination: path(destination)?,
                executable: node.meta.executable.ok_or(OwnershipProjectionError)?,
                source_relative: node
                    .meta
                    .source_relative
                    .clone()
                    .ok_or(OwnershipProjectionError)?,
                sha256,
                size,
                created_directories: directories,
                privilege: node.meta.privilege.ok_or(OwnershipProjectionError)?,
            },
        );
    }
    Ok(())
}

pub fn retire_resources(ledger: &mut InstallationRecord, retired: &BTreeSet<ResourceKey>) {
    use zup_plan::OwnedResource;
    for key in retired {
        let directories = match ledger.current.resources.get(key) {
            Some(OwnedResource::File {
                created_directories,
                ..
            }) => created_directories.clone(),
            _ => Vec::new(),
        };
        for directory in directories {
            let inheritor = ledger
                .current
                .resources
                .iter_mut()
                .find_map(|(candidate, owned)| {
                    if retired.contains(candidate) {
                        return None;
                    }
                    match owned {
                        OwnedResource::File {
                            destination,
                            created_directories,
                            ..
                        } if destination.starts_with(&directory) => Some(created_directories),
                        _ => None,
                    }
                });
            if let Some(directories) = inheritor
                && !directories.contains(&directory)
            {
                directories.push(directory);
            }
        }
        ledger.current.resources.remove(key);
    }
}
