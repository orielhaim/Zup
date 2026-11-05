use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use zup_core::{AppId, ResourceKey, SelectedScope};
use zup_plan::{INSTALL_LEDGER_SCHEMA, InstallLedger, OwnedResource};

use crate::{NodeKind, OwnershipError, TransactionNode, TransactionPlan};

pub fn ledger_path(root: &Path, app_id: &AppId, scope: SelectedScope) -> PathBuf {
    root.join("installations").join(format!(
        "{}-{}.json",
        zup_core::hash_bytes(app_id.as_str().as_bytes()).to_hex(),
        crate::scope_name(scope)
    ))
}

pub fn validate_ledger_identity(
    ledger: &InstallLedger,
    app_id: &AppId,
    scope: SelectedScope,
) -> bool {
    let mut identities = BTreeSet::new();
    ledger.schema == INSTALL_LEDGER_SCHEMA
        && ledger.app_id == *app_id
        && ledger.scope == scope
        && ledger.install_directory.as_ref().is_none_or(|path| path.target() == &ledger.target)
        && ledger.resources.iter().all(|(key, owned)| {
            if owned.privilege() != scope.authorization() || !identities.insert(key.canonical_for(&ledger.target)) { return false; }
            match owned {
            zup_plan::OwnedResource::File {
                destination,
                created_directories,
                ..
            } => {
                let identity = match key {
                    ResourceKey::File { destination }
                    | ResourceKey::Maintenance { destination, .. } => destination,
                    _ => return false,
                };
                destination.target() == &ledger.target
                    && zup_plan::TargetPath::new(ledger.target, identity)
                        .is_ok_and(|identity| destination.equivalent(&identity))
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
            OwnedResource::PathEntry { value, .. } => matches!(key, ResourceKey::PathEntry { value: identity }
                if value.target() == &ledger.target && zup_plan::TargetPath::new(ledger.target, identity).is_ok_and(|identity| value == &identity)),
            OwnedResource::Service { name, previous, installed, requires_files, privilege } =>
                matches!(key, ResourceKey::Service { .. }) && !name.is_empty()
                  && requires_files.len() <= 256
                  && requires_files.iter().collect::<BTreeSet<_>>().len() == requires_files.len()
                  && requires_files.iter().all(|path| path.target() == &ledger.target
                      && matches!(ledger.resources.get(&ResourceKey::File { destination: path.to_string() }.canonical_for(&ledger.target)),
                          Some(OwnedResource::File { destination, privilege: file_privilege, .. }) if destination == path && file_privilege == privilege))
                && [previous, installed].iter().all(|state| match state {
                    zup_plan::ServiceState::Absent => true,
                    zup_plan::ServiceState::Registration { command, execution, .. } => command.executable.target() == &ledger.target && execution.validate_for(ledger.target).is_ok(),
                }),
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
                && payload.len() <= crate::MAX_BACKEND_PAYLOAD_BYTES,
        }})
}

/// Native backends translate their operation keys into the resource identity stored in the ledger.
pub fn retired_ledger_keys<E>(
    plan: &TransactionPlan,
    mut backend_key: impl FnMut(&TransactionNode) -> Result<ResourceKey, E>,
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

pub fn valid_file_key(
    key: &ResourceKey,
    app_id: &AppId,
    version: &semver::Version,
    target: &zup_core::Target,
) -> bool {
    match key {
        ResourceKey::File { .. } => true,
        ResourceKey::Maintenance {
            app_id: owner,
            version: resource_version,
            destination,
        } => {
            owner == app_id.as_str()
                && semver::Version::parse(resource_version).is_ok_and(|parsed| &parsed == version)
                && zup_plan::TargetPath::new(*target, destination).is_ok()
        }
        _ => false,
    }
}

#[derive(Debug, thiserror::Error)]
#[error("committed file receipt does not match the transaction plan")]
pub struct LedgerProjectionError;

pub fn project_committed_files(
    record: &crate::TransactionRecord,
    ledger: &mut InstallLedger,
) -> Result<(), LedgerProjectionError> {
    use crate::OperationReceipt;
    use zup_plan::OwnedResource;
    let path = |text: &str| {
        zup_plan::TargetPath::new(record.target, text).map_err(|_| LedgerProjectionError)
    };
    if record.release.is_some() || ledger.version != record.app_version {
        ledger.release = record.release.clone();
    }
    ledger.version = record.app_version.clone();
    ledger.selected_components = record.plan.selected_components.clone();
    ledger.install_directory = record.plan.install_directory.clone();
    ledger.preset = record.plan.preset.clone();
    for node in &record.plan.nodes {
        let NodeKind::FileMutation { key, .. } = &node.kind else {
            continue;
        };
        let receipt = record.receipt(&node.id).ok_or(LedgerProjectionError)?;
        let mut directories = match ledger.resources.get(key) {
            Some(OwnedResource::File {
                created_directories,
                ..
            }) => created_directories.clone(),
            _ => Vec::new(),
        };
        let (destination, digest, size) = match receipt {
            OperationReceipt::CreateFile {
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
                (destination, installed_sha256, *installed_size)
            }
            OperationReceipt::ReplaceFile {
                destination,
                new_sha256,
                new_size,
                ..
            } => (destination, new_sha256, *new_size),
            _ => return Err(LedgerProjectionError),
        };
        let sha256 = digest.parse().map_err(|_| LedgerProjectionError)?;
        if node.meta.expected_sha256 != Some(sha256) || node.meta.expected_size != Some(size) {
            return Err(LedgerProjectionError);
        }
        ledger.resources.insert(
            key.clone(),
            OwnedResource::File {
                destination: path(destination)?,
                executable: node.meta.executable.ok_or(LedgerProjectionError)?,
                source_relative: node
                    .meta
                    .source_relative
                    .clone()
                    .ok_or(LedgerProjectionError)?,
                sha256,
                size,
                created_directories: directories,
                privilege: node.meta.privilege.ok_or(LedgerProjectionError)?,
            },
        );
    }
    Ok(())
}

pub fn retire_resources(ledger: &mut InstallLedger, retired: &BTreeSet<ResourceKey>) {
    use zup_plan::OwnedResource;
    for key in retired {
        let directories = match ledger.resources.get(key) {
            Some(OwnedResource::File {
                created_directories,
                ..
            }) => created_directories.clone(),
            _ => Vec::new(),
        };
        for directory in directories {
            let inheritor = ledger.resources.iter_mut().find_map(|(candidate, owned)| {
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
        ledger.resources.remove(key);
    }
}
