use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use zup_core::{AppId, ResourceKey, SelectedScope, Sha256Digest};
use zup_exec::{
    INSTALL_LEDGER_SCHEMA, InstallLedger, ObservedServiceState, OwnedResource, ServiceState,
};
use zup_transaction::{
    FileDelta, FilePrecondition, NodeKind, OperationReceipt, TransactionNode, TransactionPhase,
    TransactionPlan, TransactionRecord, TransactionStore,
};

use crate::error::{ExecError, PathError};
use crate::fs::OwnedDirectory;
use crate::lowering::{target_path_from_host, to_host_path};

pub struct LinuxLedgerStore {
    root: PathBuf,
}

impl LinuxLedgerStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn path_for(&self, app_id: &AppId, scope: SelectedScope) -> PathBuf {
        let digest = zup_core::hash_bytes(app_id.as_str().as_bytes());
        let name: String = digest
            .as_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let scope = match scope {
            SelectedScope::User => "user",
            SelectedScope::Machine => "machine",
        };
        self.root
            .join("installations")
            .join(format!("{name}-{scope}.json"))
    }

    pub fn load(
        &self,
        app_id: &AppId,
        scope: SelectedScope,
    ) -> Result<Option<InstallLedger>, ExecError> {
        let path = self.path_for(app_id, scope);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(PathError::io(&(path), source).into()),
        };
        let ledger: InstallLedger = serde_json::from_slice(&bytes)?;
        if ledger.schema != INSTALL_LEDGER_SCHEMA
            || ledger.app_id != *app_id
            || ledger.scope != scope
        {
            return Err(ExecError::LedgerInvalid);
        }
        Ok(Some(ledger))
    }

    pub fn validate_plan(
        &self,
        app_id: &AppId,
        scope: SelectedScope,
        app_version: &semver::Version,
        plan: &TransactionPlan,
    ) -> Result<(), ExecError> {
        plan.validate()
            .map_err(|error| ExecError::Ownership(error.to_string()))?;
        let ledger = self.load(app_id, scope)?;
        if ledger
            .as_ref()
            .is_some_and(|ledger| ledger.target != plan.target)
        {
            return Err(ExecError::Ownership("target identity".into()));
        }
        let retired: BTreeSet<_> = plan.retired_keys.iter().collect();

        let retired_ledger = retired_ledger_keys(plan)?;
        if retired.len() != plan.retired_keys.len()
            || retired_ledger.iter().any(|key| {
                ledger
                    .as_ref()
                    .is_none_or(|ledger| !ledger.resources.contains_key(key))
            })
            || (plan.uninstall
                && ledger.as_ref().is_some_and(|ledger| {
                    retired_ledger.len() != ledger.resources.len()
                        || retired_ledger
                            .iter()
                            .any(|key| !ledger.resources.contains_key(key))
                }))
        {
            return Err(ExecError::Ownership("retired resources".into()));
        }
        for node in &plan.nodes {
            match &node.kind {
                NodeKind::FileMutation { key, delta } => {
                    let owned = ledger.as_ref().and_then(|ledger| ledger.resources.get(key));
                    let valid = match (delta, owned) {
                        (FileDelta::Create, None) => {
                            matches!(node.meta.file_precondition, Some(FilePrecondition::Absent))
                        }
                        (FileDelta::Replace, Some(OwnedResource::File { sha256, size, .. })) => {
                            matches!(
                                node.meta.file_precondition,
                                Some(FilePrecondition::Exact {
                                    size: found_size,
                                    sha256: found_hash
                                })
                                if found_size == *size && found_hash == *sha256
                            )
                        }
                        (
                            FileDelta::RestoreOwned,
                            Some(OwnedResource::File { sha256, size, .. }),
                        ) => {
                            matches!(node.meta.file_precondition, Some(FilePrecondition::Absent))
                                && node.meta.expected_sha256 == Some(*sha256)
                                && node.meta.expected_size == Some(*size)
                        }
                        (
                            FileDelta::RepairOwned,
                            Some(OwnedResource::File { sha256, size, .. }),
                        ) => {
                            matches!(
                                node.meta.file_precondition,
                                Some(FilePrecondition::Exact { .. })
                            ) && node.meta.expected_sha256 == Some(*sha256)
                                && node.meta.expected_size == Some(*size)
                        }
                        _ => false,
                    };
                    if !valid
                        || !valid_file_key(key, app_id, app_version, &plan.target)
                        || node.meta.source_relative.is_none()
                    {
                        return Err(ExecError::Ownership(node.id.to_string()));
                    }
                }
                NodeKind::FileRemoval { key } => {
                    let Some(removal) = &node.meta.removal else {
                        return Err(ExecError::Ownership(node.id.to_string()));
                    };
                    let Some(OwnedResource::File {
                        destination,
                        sha256,
                        size,
                        ..
                    }) = ledger.as_ref().and_then(|ledger| ledger.resources.get(key))
                    else {
                        return Err(ExecError::Ownership(node.id.to_string()));
                    };
                    if !retired.contains(key)
                        || removal.key != *key
                        || removal.destination != *destination
                        || removal.sha256 != *sha256
                        || removal.size != *size
                    {
                        return Err(ExecError::Ownership(node.id.to_string()));
                    }
                }

                NodeKind::BackendOperation { key, .. } => {
                    let Some(backend) = &node.meta.backend else {
                        return Err(ExecError::Ownership(node.id.to_string()));
                    };
                    if backend.key != *key {
                        return Err(ExecError::Ownership(node.id.to_string()));
                    }
                    if is_service_backend(&backend.key) {
                        validate_service_apply(node, backend, ledger.as_ref())?;
                        continue;
                    }
                    let request = crate::refresh::RefreshRequest::decode(&backend.payload)
                        .map_err(|_| ExecError::Ownership(node.id.to_string()))?;
                    if request.key() != *key {
                        return Err(ExecError::Ownership(node.id.to_string()));
                    }
                }
                NodeKind::BackendRemoval { key } => {
                    let Some(backend) = &node.meta.backend else {
                        return Err(ExecError::Ownership(node.id.to_string()));
                    };
                    if is_service_backend(&backend.key) && backend.key == *key {
                        validate_service_removal(node, backend, ledger.as_ref(), &retired)?;
                        continue;
                    }
                    return Err(ExecError::Ownership(node.id.to_string()));
                }
                NodeKind::Barrier | NodeKind::StageFile { .. } => {}
            }
        }
        if plan.preset.is_some() {
            return Err(ExecError::Ownership(
                "a Linux console or headless transaction presents no window".into(),
            ));
        }
        Ok(())
    }

    pub fn publish_committed(
        &self,
        record: &TransactionRecord,
        scope: SelectedScope,
    ) -> Result<InstallLedger, ExecError> {
        if record.phase != TransactionPhase::Committed {
            return Err(ExecError::LedgerUncommitted);
        }
        if record.scope != scope {
            return Err(ExecError::LedgerInvalid);
        }
        record.validate().map_err(|_| ExecError::LedgerInvalid)?;
        if record.plan.nodes.iter().any(|node| {
            !matches!(node.kind, NodeKind::Barrier) && record.receipt(&node.id).is_none()
        }) {
            return Err(ExecError::LedgerUncommitted);
        }
        let previous = self.load(&record.app_id, scope)?;
        if previous
            .as_ref()
            .is_some_and(|ledger| ledger.target != record.target)
        {
            return Err(ExecError::Ownership("target identity".into()));
        }
        if previous
            .as_ref()
            .is_some_and(|ledger| ledger.committed_transaction == record.transaction_id.to_string())
        {
            cleanup_committed_files(record)?;
            return Ok(previous.expect("checked above"));
        }
        if record.plan.uninstall {
            let retired_ledger_keys = retired_ledger_keys(&record.plan)?;
            if let Some(ledger) = previous.as_ref()
                && (retired_ledger_keys.len() != ledger.resources.len()
                    || retired_ledger_keys
                        .iter()
                        .any(|key| !ledger.resources.contains_key(key)))
            {
                return Err(ExecError::LedgerInvalid);
            }
            let path = self.path_for(&record.app_id, scope);
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(source) => return Err(PathError::io(&(path), source).into()),
            }
            cleanup_committed_files(record)?;
            cleanup_removed_directories(record)?;
            cleanup_application_state(&self.root, record)?;

            let maintenance = zup_transaction::maintenance_root(&self.root, &record.app_id, scope);
            if let Ok(entries) = std::fs::read_dir(&maintenance) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if std::fs::symlink_metadata(&path)
                        .map(|metadata| metadata.is_dir())
                        .unwrap_or(false)
                    {
                        match std::fs::remove_dir(&path) {
                            Ok(()) => {}
                            Err(error)
                                if matches!(
                                    error.kind(),
                                    std::io::ErrorKind::NotFound
                                        | std::io::ErrorKind::DirectoryNotEmpty
                                ) => {}
                            Err(source) => {
                                return Err(PathError::io(&(path), source).into());
                            }
                        }
                    }
                }
                let _ = std::fs::remove_dir(&maintenance);
            }
            return Ok(InstallLedger::new(
                record.app_id.clone(),
                record.target.clone(),
                scope,
            ));
        }
        let mut ledger = previous.unwrap_or_else(|| {
            InstallLedger::new(record.app_id.clone(), record.target.clone(), scope)
        });
        if ledger.target != record.target {
            return Err(ExecError::Ownership("target identity".into()));
        }
        ledger.version = record.app_version.clone();
        ledger.selected_components = record.plan.selected_components.clone();
        ledger.install_directory = record.plan.install_directory.clone();
        ledger.preset = None;

        ledger.release = None;
        for node in &record.plan.nodes {
            if matches!(
                &node.kind,
                NodeKind::BackendOperation { .. } | NodeKind::BackendRemoval { .. }
            ) {
                publish_service_node(&mut ledger, record, node)?;
                continue;
            }
            let NodeKind::FileMutation { key, .. } = &node.kind else {
                continue;
            };
            let Some(receipt) = record.receipt(&node.id) else {
                continue;
            };
            let Some(source_relative) = node.meta.source_relative.clone() else {
                return Err(ExecError::LedgerInvalid);
            };
            let (destination, digest, size, created_directories) = match receipt {
                OperationReceipt::CreateFile {
                    destination,
                    installed_sha256,
                    installed_size,
                    created_directories,
                    ..
                } => {
                    let mut directories = created_directories
                        .iter()
                        .map(|path| {
                            target_path_from_host(Path::new(path), &record.target)
                                .map_err(|_| ExecError::LedgerInvalid)
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    if let Some(OwnedResource::File {
                        created_directories: old,
                        ..
                    }) = ledger.resources.get(key)
                    {
                        for directory in old {
                            if !directories.contains(directory) {
                                directories.push(directory.clone());
                            }
                        }
                    }
                    (destination, installed_sha256, *installed_size, directories)
                }
                OperationReceipt::ReplaceFile {
                    destination,
                    new_sha256,
                    new_size,
                    ..
                } => {
                    let directories = match ledger.resources.get(key) {
                        Some(OwnedResource::File {
                            created_directories,
                            ..
                        }) => created_directories.clone(),
                        _ => Vec::new(),
                    };
                    (destination, new_sha256, *new_size, directories)
                }
                _ => return Err(ExecError::LedgerInvalid),
            };
            let expected_path = match key {
                ResourceKey::File { destination }
                | ResourceKey::Maintenance { destination, .. } => destination,
                _ => return Err(ExecError::LedgerInvalid),
            };
            let sha256: Sha256Digest = digest.parse().map_err(|_| ExecError::LedgerInvalid)?;
            if *destination != *expected_path
                || node.meta.expected_sha256 != Some(sha256)
                || node.meta.expected_size != Some(size)
            {
                return Err(ExecError::LedgerInvalid);
            }
            ledger.resources.insert(
                key.clone(),
                OwnedResource::File {
                    destination: target_path_from_host(Path::new(destination), &record.target)
                        .map_err(|_| ExecError::LedgerInvalid)?,
                    source_relative,
                    sha256,
                    size,
                    created_directories,
                    privilege: node.meta.privilege.ok_or(ExecError::LedgerInvalid)?,
                },
            );
        }
        for key in retired_ledger_keys(&record.plan)? {
            let directories = match ledger.resources.get(&key) {
                Some(OwnedResource::File {
                    created_directories,
                    ..
                }) => created_directories.clone(),
                _ => Vec::new(),
            };
            for directory in directories {
                let inheritor = ledger.resources.iter().find_map(|(candidate, owned)| {
                    if candidate == &key {
                        return None;
                    }
                    match owned {
                        OwnedResource::File { destination, .. }
                            if destination.starts_with(&directory) =>
                        {
                            Some(candidate.clone())
                        }
                        _ => None,
                    }
                });
                if let Some(inheritor) = inheritor
                    && let Some(OwnedResource::File {
                        created_directories,
                        ..
                    }) = ledger.resources.get_mut(&inheritor)
                    && !created_directories.contains(&directory)
                {
                    created_directories.push(directory);
                }
            }
            ledger.resources.remove(&key);
        }
        ledger.committed_transaction = record.transaction_id.to_string();
        let path = self.path_for(&record.app_id, scope);
        let bytes = serde_json::to_vec_pretty(&ledger)?;
        write_ledger(&path, &bytes)?;
        cleanup_committed_files(record)?;

        cleanup_removed_directories(record)?;
        Ok(ledger)
    }

    pub fn repair_committed(&self, app_id: &AppId, scope: SelectedScope) -> Result<(), ExecError> {
        let directory = self.root.join("transactions");
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(source) => {
                return Err(PathError::io(&(directory), source).into());
            }
        };
        let journals = zup_transaction::FilesystemTransactionStore::new(&self.root);
        let mut committed = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| PathError::io(&(directory.clone()), source))?;
            let Some(id) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<uuid::Uuid>().ok())
            else {
                continue;
            };
            let record = journals
                .load(&zup_transaction::TransactionId::from_uuid(id))
                .map_err(|_| ExecError::LedgerInvalid)?;
            if record.app_id != *app_id || record.scope != scope {
                continue;
            }
            if self
                .load(app_id, scope)?
                .as_ref()
                .is_some_and(|ledger| ledger.target != record.target)
            {
                return Err(ExecError::Ownership("target identity".into()));
            }
            match record.phase {
                TransactionPhase::Committed => committed.push(record),
                TransactionPhase::RolledBack => {}
                _ => {
                    return Err(ExecError::RecoveryRequired(
                        record.transaction_id.to_string(),
                    ));
                }
            }
        }
        committed.sort_by_key(|record| record.transaction_id.as_uuid());
        let mut latest = self
            .load(app_id, scope)?
            .and_then(|ledger| ledger.committed_transaction.parse::<uuid::Uuid>().ok());
        for record in committed {
            let id = record.transaction_id.as_uuid();
            if latest.is_none_or(|previous| id > previous) {
                self.publish_committed(&record, scope)?;
                latest = Some(id);
            } else {
                cleanup_committed_files(&record)?;
            }
        }
        Ok(())
    }
}

fn valid_file_key(
    key: &ResourceKey,
    app_id: &AppId,
    app_version: &semver::Version,
    target: &zup_core::TargetTriple,
) -> bool {
    match key {
        ResourceKey::File { .. } => true,
        ResourceKey::Maintenance {
            app_id: resource_app,
            version,
            destination,
        } => {
            resource_app == app_id.as_str()
                && semver::Version::parse(version).is_ok_and(|parsed| &parsed == app_version)
                && target_path_from_host(Path::new(destination), target).is_ok()
        }
        _ => false,
    }
}

fn retired_ledger_keys(plan: &TransactionPlan) -> Result<BTreeSet<ResourceKey>, ExecError> {
    let mut keys = BTreeSet::new();
    for key in &plan.retired_keys {
        let ledger_key = plan
            .nodes
            .iter()
            .find(|node| {
                matches!(
                    &node.kind,
                    NodeKind::BackendRemoval { key: node_key }
                        if node_key == key
                )
            })
            .and_then(service_ledger_key)
            .unwrap_or_else(|| key.clone());
        if !keys.insert(ledger_key) {
            return Err(ExecError::Ownership("duplicate retired resources".into()));
        }
    }
    Ok(keys)
}

fn is_service_backend(key: &ResourceKey) -> bool {
    match key {
        ResourceKey::Backend { id } => id
            .as_str()
            .starts_with(crate::service_ops::SERVICE_BACKEND_PREFIX),
        _ => false,
    }
}

fn service_ledger_key(node: &TransactionNode) -> Option<ResourceKey> {
    let backend = node.meta.backend.as_ref()?;
    if !is_service_backend(&backend.key) {
        return None;
    }
    let payload = crate::service_ops::decode_payload(&backend.payload).ok()?;
    Some(crate::service_ops::ledger_key_for_payload(&payload))
}

fn validate_service_apply(
    node: &TransactionNode,
    backend: &zup_transaction::BackendOperation,
    ledger: Option<&InstallLedger>,
) -> Result<(), ExecError> {
    let payload = crate::service_ops::decode_payload(&backend.payload)
        .map_err(|_| ExecError::Ownership(node.id.to_string()))?;
    let crate::service_ops::ServicePayload::Apply {
        service,
        unit,
        binary_owned,
        ..
    } = payload
    else {
        return Err(ExecError::Ownership(node.id.to_string()));
    };
    if crate::service_ops::backend_key_for_unit(&unit) != backend.key {
        return Err(ExecError::Ownership(node.id.to_string()));
    }
    if !binary_owned {
        return Err(ExecError::Ownership(node.id.to_string()));
    }
    match ledger.and_then(|ledger| ledger.resources.get(&service.key)) {
        None | Some(OwnedResource::Service { .. }) => Ok(()),
        _ => Err(ExecError::Ownership(node.id.to_string())),
    }
}

fn validate_service_removal(
    node: &TransactionNode,
    backend: &zup_transaction::BackendOperation,
    ledger: Option<&InstallLedger>,
    retired: &BTreeSet<&ResourceKey>,
) -> Result<(), ExecError> {
    let payload = crate::service_ops::decode_payload(&backend.payload)
        .map_err(|_| ExecError::Ownership(node.id.to_string()))?;
    let crate::service_ops::ServicePayload::Remove { key, owned, .. } = payload else {
        return Err(ExecError::Ownership(node.id.to_string()));
    };
    if !retired.contains(&backend.key) {
        return Err(ExecError::Ownership(node.id.to_string()));
    }
    if ledger.and_then(|ledger| ledger.resources.get(&key)) != Some(&owned) {
        return Err(ExecError::Ownership(node.id.to_string()));
    }
    Ok(())
}

fn publish_service_node(
    ledger: &mut InstallLedger,
    record: &TransactionRecord,
    node: &TransactionNode,
) -> Result<(), ExecError> {
    let NodeKind::BackendOperation { .. } = &node.kind else {
        return Ok(());
    };
    let Some(backend) = &node.meta.backend else {
        return Err(ExecError::LedgerInvalid);
    };
    if !is_service_backend(&backend.key) {
        return Ok(());
    }
    let payload = crate::service_ops::decode_payload(&backend.payload)
        .map_err(|_| ExecError::LedgerInvalid)?;
    let crate::service_ops::ServicePayload::Apply { service, .. } = payload else {
        return Err(ExecError::LedgerInvalid);
    };
    let Some(receipt) = record.receipt(&node.id) else {
        return Ok(());
    };
    let OperationReceipt::Backend { payload, .. } = receipt else {
        return Err(ExecError::LedgerInvalid);
    };
    let service_receipt: crate::service_ops::ServiceReceipt =
        serde_json::from_slice(payload).map_err(|_| ExecError::LedgerInvalid)?;
    let key = service.key.clone();
    let previous = match &service.previous {
        ObservedServiceState::Absent => ServiceState::Absent,
        ObservedServiceState::Service {
            display_name,
            command,
            start,
            ..
        } => ServiceState::Registration {
            display_name: display_name.clone(),
            command: command.clone(),
            start: *start,
        },
    };
    let installed = ServiceState::Registration {
        display_name: service.display_name.clone(),
        command: service.command.clone(),
        start: service.start,
    };

    let _ = service_receipt;
    let old_previous = match ledger.resources.get(&key) {
        Some(OwnedResource::Service { previous, .. }) => Some(previous.clone()),
        _ => None,
    };
    ledger.resources.insert(
        key,
        OwnedResource::Service {
            name: service.name.clone(),
            privilege: service.privilege,
            previous: old_previous.unwrap_or(previous),
            installed,
        },
    );
    Ok(())
}

fn write_ledger(path: &Path, bytes: &[u8]) -> Result<(), ExecError> {
    let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    else {
        return Err(PathError::io(
            path,
            std::io::Error::other("a ledger path has a parent directory"),
        )
        .into());
    };
    let directory = OwnedDirectory::create(parent, crate::fs::STATE_DIRECTORY_MODE)
        .map_err(|error| PathError::io(path, std::io::Error::other(error.to_string())))?;
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .ok_or_else(|| {
            PathError::io(path, std::io::Error::other("a ledger path has a file name"))
        })?;
    directory
        .write_durable(&name, bytes, crate::fs::STATE_FILE_MODE)
        .map_err(|error| PathError::io(path, std::io::Error::other(error.to_string())))?;
    Ok(())
}

fn cleanup_committed_files(record: &TransactionRecord) -> Result<(), ExecError> {
    for node in &record.plan.nodes {
        let Some(receipt) = record.receipt(&node.id) else {
            continue;
        };
        let OperationReceipt::RemoveFile {
            backup_path,
            sha256,
            size,
            ..
        } = receipt
        else {
            continue;
        };
        let backup = to_host_path(backup_path).map_err(|_| ExecError::LedgerInvalid)?;
        match std::fs::symlink_metadata(&backup) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(PathError::io(&(backup.clone()), source).into());
            }
            Ok(metadata) if metadata.file_type().is_file() => {
                let file = std::fs::File::open(&backup)
                    .map_err(|source| PathError::io(&(backup.clone()), source))?;
                if zup_core::hash_reader(file)
                    .map_err(|source| PathError::io(&(backup.clone()), source))?
                    != (*size, *sha256)
                {
                    return Err(ExecError::LedgerInvalid);
                }
                std::fs::remove_file(&backup)
                    .map_err(|source| PathError::io(&(backup.clone()), source))?;
            }
            Ok(_) => return Err(ExecError::LedgerInvalid),
        }
    }
    Ok(())
}

fn cleanup_removed_directories(record: &TransactionRecord) -> Result<(), ExecError> {
    let mut directories = BTreeSet::new();
    for node in &record.plan.nodes {
        if !matches!(node.kind, NodeKind::FileRemoval { .. }) {
            continue;
        }
        let Some(removal) = &node.meta.removal else {
            continue;
        };
        for directory in &removal.created_directories {
            directories.insert(directory.clone());
        }
    }
    let mut directories: Vec<_> = directories.into_iter().collect();
    directories.sort_by_key(|path| std::cmp::Reverse(path.to_string().len()));
    for directory in directories {
        let path = to_host_path(&directory).map_err(|_| ExecError::LedgerInvalid)?;
        match std::fs::remove_dir(&path) {
            Ok(()) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty
                ) => {}
            Err(source) => {
                return Err(PathError::io(&(path), source).into());
            }
        }
    }
    Ok(())
}

fn cleanup_application_state(root: &Path, uninstall: &TransactionRecord) -> Result<(), ExecError> {
    let transactions = root.join("transactions");
    let entries = match std::fs::read_dir(&transactions) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(PathError::io(&(transactions), source).into());
        }
    };
    let store = zup_transaction::FilesystemTransactionStore::new(root);
    for entry in entries {
        let entry = entry.map_err(|source| PathError::io(&(transactions.clone()), source))?;
        let Some(id) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<uuid::Uuid>().ok())
        else {
            continue;
        };
        let record = store
            .load(&zup_transaction::TransactionId::from_uuid(id))
            .map_err(|_| ExecError::LedgerInvalid)?;
        if record.app_id != uninstall.app_id || record.scope != uninstall.scope {
            continue;
        }
        if !matches!(
            record.phase,
            TransactionPhase::Committed | TransactionPhase::RolledBack
        ) {
            return Err(ExecError::RecoveryRequired(
                record.transaction_id.to_string(),
            ));
        }
        let directory = entry.path();
        if std::fs::symlink_metadata(&directory).is_ok_and(|metadata| metadata.is_dir()) {
            std::fs::remove_dir_all(&directory)
                .map_err(|source| PathError::io(&(directory.clone()), source))?;
        } else {
            return Err(ExecError::LedgerInvalid);
        }
    }
    for directory in [root.join("transactions"), root.join("installations")] {
        match std::fs::remove_dir(&directory) {
            Ok(()) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty
                ) => {}
            Err(source) => {
                return Err(PathError::io(&(directory), source).into());
            }
        }
    }
    Ok(())
}
