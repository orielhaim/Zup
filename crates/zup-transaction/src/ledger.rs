use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use zup_core::{AppId, ResourceKey, SelectedScope};
use zup_exec::{INSTALL_LEDGER_SCHEMA, InstallLedger};

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
    ledger
        .committed_transaction
        .parse::<crate::TransactionId>()
        .is_ok()
        && ledger.schema == INSTALL_LEDGER_SCHEMA
        && ledger.app_id == *app_id
        && ledger.scope == scope
        && ledger.resources.iter().all(|(key, owned)| match owned {
            zup_exec::OwnedResource::File {
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
                    && destination.as_str() == identity
                    && created_directories.iter().all(|directory| {
                        directory.target() == &ledger.target
                            && directory.parent().is_some()
                            && directory != destination
                            && destination.starts_with(directory)
                    })
            }
            _ => true,
        })
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
                && zup_platform::TargetPath::new(*target, destination).is_ok()
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
    use zup_exec::OwnedResource;
    let path = |text: &str| {
        zup_platform::TargetPath::new(record.target, text).map_err(|_| LedgerProjectionError)
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
    use zup_exec::OwnedResource;
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
