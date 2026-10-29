use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use thiserror::Error;
use zup_core::ReleaseIdentity;
use zup_core::{AppId, ResourceKey, SelectedScope, Sha256Digest};
use zup_exec::{INSTALL_LEDGER_SCHEMA, InstallLedger, OwnedResource};
use zup_platform::TargetPath;
use zup_transaction::{
    BackendOperationIntent, FilesystemTransactionStore, NodeKind, OperationReceipt,
    TransactionPhase, TransactionPlan, TransactionRecord, TransactionStore, maintenance_directory,
    maintenance_root as transaction_maintenance_root,
};

use crate::durable::DurableError;
use crate::lowering::{host_path, target_path_from_host};
use crate::transaction_payload::{
    BackendReceipt, apps_owned_payload, ledger_key_for_backend_node, receipt_from_bytes,
};

#[derive(Debug, Error)]
pub enum LedgerError {
    #[error("ledger I/O at {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("ledger JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("ledger identity or schema mismatch")]
    Invalid,
    #[error("ledger publish requires a committed transaction")]
    Uncommitted,
    #[error("transaction plan does not match committed ownership: {0}")]
    Ownership(String),
    #[error("unfinished transaction {0} requires recovery")]
    RecoveryRequired(String),
    #[error("committed ledger reconciliation: {0}")]
    Reconciliation(String),
    #[error(transparent)]
    Durable(#[from] DurableError),
}

pub struct InstallLedgerStore {
    root: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleaseRecord<'a> {
    Identity(&'a ReleaseIdentity),

    None,

    Preserve,
}

impl InstallLedgerStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn path(&self, app_id: &AppId, scope: SelectedScope) -> PathBuf {
        let mut hash = Sha256::new();
        hash.update(app_id.as_str().as_bytes());
        let digest = hash.finalize();
        let name: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
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
    ) -> Result<Option<InstallLedger>, LedgerError> {
        let path = self.path_for(app_id, scope);
        let bytes = match read_ledger(&path, scope) {
            Ok(bytes) => bytes,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(LedgerError::Io { path, source }),
        };
        let ledger: InstallLedger = serde_json::from_slice(&bytes)?;
        if ledger.schema != INSTALL_LEDGER_SCHEMA
            || ledger.app_id != *app_id
            || ledger.scope != scope
        {
            return Err(LedgerError::Invalid);
        }
        Ok(Some(ledger))
    }

    pub fn path_for(&self, app_id: &AppId, scope: SelectedScope) -> PathBuf {
        self.path(app_id, scope)
    }

    pub fn repair_committed(
        &self,
        app_id: &AppId,
        scope: SelectedScope,
    ) -> Result<(), LedgerError> {
        let directory = self.root.join("transactions");
        let journals = if scope == SelectedScope::Machine {
            crate::machine_authority::MachineRoot::acquire()
                .map_err(|error| LedgerError::Io {
                    path: self.root.clone(),
                    source: std::io::Error::other(error),
                })?
                .transaction_store()
        } else {
            FilesystemTransactionStore::private_at(&self.root)
        };
        let ids = journals.list_ids().map_err(|error| LedgerError::Io {
            path: directory.clone(),
            source: std::io::Error::other(error),
        })?;
        let mut committed = Vec::new();
        for id in ids {
            let record = journals.load(&id).map_err(|_| LedgerError::Invalid)?;
            if record.app_id != *app_id || record.scope != scope {
                continue;
            }
            if self
                .load(app_id, scope)?
                .as_ref()
                .is_some_and(|ledger| ledger.target != record.target)
            {
                return Err(LedgerError::Ownership("target identity".into()));
            }
            match record.phase {
                TransactionPhase::Committed => committed.push(record),
                TransactionPhase::RolledBack => self.cleanup_work(&record)?,
                _ => {
                    return Err(LedgerError::RecoveryRequired(
                        record.transaction_id.to_string(),
                    ));
                }
            }
        }
        committed.sort_by_key(|record| record.transaction_id.as_uuid());
        let mut latest = self
            .load(app_id, scope)?
            .map(|ledger| ledger.committed_transaction.parse::<uuid::Uuid>())
            .transpose()
            .map_err(|_| LedgerError::Invalid)?;
        for record in committed {
            let id = record.transaction_id.as_uuid();
            if latest.is_none_or(|previous| id > previous) {
                self.publish_committed(&record, scope, ReleaseRecord::Preserve)?;
                latest = Some(id);
            } else {
                cleanup_committed_files(&record)?;
                self.cleanup_work(&record)?;
            }
        }
        Ok(())
    }

    pub(crate) fn cleanup_work(&self, record: &TransactionRecord) -> Result<(), LedgerError> {
        if !matches!(
            record.phase,
            TransactionPhase::Committed | TransactionPhase::RolledBack
        ) {
            return Err(LedgerError::Uncommitted);
        }
        if record.scope == SelectedScope::Machine {
            crate::machine_authority::MachineRoot::acquire()
                .and_then(|machine| machine.remove_work(&record.transaction_id))
                .map_err(|error| LedgerError::Io {
                    path: self
                        .root
                        .join("work")
                        .join(record.transaction_id.to_string()),
                    source: std::io::Error::other(error),
                })?;
        }
        Ok(())
    }

    pub fn validate_plan(
        &self,
        app_id: &AppId,
        scope: SelectedScope,
        app_version: &semver::Version,
        plan: &TransactionPlan,
    ) -> Result<(), LedgerError> {
        plan.validate()
            .map_err(|error| LedgerError::Ownership(error.to_string()))?;
        let ledger = self.load(app_id, scope)?;
        let retired_ledger_keys = retired_ledger_keys(plan)?;
        let retired =
            zup_transaction::validate_ownership(plan, ledger.as_ref(), &retired_ledger_keys)
                .map_err(|error| LedgerError::Ownership(error.to_string()))?;
        for node in &plan.nodes {
            match &node.kind {
                NodeKind::FileMutation { key, .. } => {
                    if !valid_file_key(key, app_id, app_version, &plan.target) {
                        return Err(LedgerError::Ownership(node.id.to_string()));
                    }
                }
                NodeKind::BackendOperation { key, intent } => {
                    let Some(operation) = &node.meta.backend else {
                        return Err(LedgerError::Ownership(node.id.to_string()));
                    };
                    if operation.key != *key
                        || operation.intent != *intent
                        || operation.payload.len() > zup_transaction::MAX_BACKEND_PAYLOAD_BYTES
                    {
                        return Err(LedgerError::Ownership(node.id.to_string()));
                    }
                    if let Err(reason) = crate::transaction_payload::validate_backend_node(
                        node,
                        ledger.as_ref(),
                        scope,
                    ) {
                        return Err(LedgerError::Ownership(reason));
                    }
                }
                NodeKind::BackendRemoval { key } => {
                    let Some(operation) = &node.meta.backend else {
                        return Err(LedgerError::Ownership(node.id.to_string()));
                    };
                    let ledger_key = ledger_key_for_backend_node(node)
                        .map_err(|error| LedgerError::Ownership(error.to_string()))?;
                    if operation.key != *key
                        || operation.intent != BackendOperationIntent::Remove
                        || operation.payload.len() > zup_transaction::MAX_BACKEND_PAYLOAD_BYTES
                        || !retired.contains(key)
                        || !retired_ledger_keys.contains(&ledger_key)
                        || ledger
                            .as_ref()
                            .and_then(|ledger| ledger.resources.get(&ledger_key))
                            .is_none()
                    {
                        return Err(LedgerError::Ownership(node.id.to_string()));
                    }
                    if let Err(reason) = crate::transaction_payload::validate_backend_node(
                        node,
                        ledger.as_ref(),
                        scope,
                    ) {
                        return Err(LedgerError::Ownership(reason));
                    }
                }
                NodeKind::FileRemoval { .. } | NodeKind::Barrier | NodeKind::StageFile { .. } => {}
            }
        }
        self.validate_ui(app_id, scope, app_version, plan, ledger.as_ref())?;
        Ok(())
    }

    fn validate_ui(
        &self,
        app_id: &AppId,
        scope: SelectedScope,
        app_version: &semver::Version,
        plan: &TransactionPlan,
        ledger: Option<&InstallLedger>,
    ) -> Result<(), LedgerError> {
        let root = crate::plain_path_text(&transaction_maintenance_root(&self.root, app_id, scope));
        let owned: BTreeMap<&str, &Sha256Digest> = ledger
            .into_iter()
            .flat_map(|ledger| ledger.resources.iter())
            .filter_map(|(key, resource)| match (key, resource) {
                (ResourceKey::File { destination }, OwnedResource::File { sha256, .. })
                    if zup_bundle::is_content_path(Path::new(&root), destination) =>
                {
                    Some((destination.as_str(), sha256))
                }
                _ => None,
            })
            .collect();
        let installed: BTreeMap<&str, Sha256Digest> = plan
            .nodes
            .iter()
            .filter_map(|node| match &node.kind {
                NodeKind::FileMutation {
                    key: ResourceKey::File { destination },
                    ..
                } => Some((
                    destination.as_str(),
                    node.meta
                        .expected_sha256
                        .expect("a file mutation states the digest it installs"),
                )),
                _ => None,
            })
            .collect();
        let retired: BTreeSet<&ResourceKey> = plan.retired_keys.iter().collect();

        let wanted: BTreeMap<String, Sha256Digest> = match &plan.preset {
            Some(ui) => {
                let directory = maintenance_directory(&self.root, app_id, scope, app_version);
                std::iter::once((
                    crate::plain_path_text(&zup_bundle::preset_path(
                        &directory,
                        &ui.executable,
                        plan.target.executable_suffix(),
                    )),
                    ui.executable,
                ))
                .chain(ui.preset.assets.iter().map(|asset| {
                    (
                        crate::plain_path_text(&zup_bundle::asset_path(
                            &directory,
                            asset.name.as_str(),
                            &asset.sha256,
                        )),
                        asset.sha256,
                    )
                }))
                .collect()
            }
            None => BTreeMap::new(),
        };
        for (path, digest) in &wanted {
            if installed.get(path.as_str()) != Some(digest)
                && !owned
                    .get(path.as_str())
                    .is_some_and(|found| **found == *digest)
            {
                return Err(LedgerError::Ownership(format!(
                    "the preset runtime needs {path}, and this plan neither installs it nor already \
                     owns it"
                )));
            }
        }
        if plan.uninstall {
            return Ok(());
        }
        for destination in owned.keys() {
            if wanted.contains_key(*destination) {
                continue;
            }
            let Some(ledger) = ledger else {
                continue;
            };
            let retired_here = ledger.resources.keys().any(|key| {
                matches!(key, ResourceKey::File { destination: d } if d == destination
                    && retired.contains(key))
            });
            if !retired_here {
                return Err(LedgerError::Ownership(format!(
                    "this plan stops presenting the window that owns {destination}, without \
                     retiring it"
                )));
            }
        }
        Ok(())
    }

    pub fn publish_committed(
        &self,
        record: &TransactionRecord,
        scope: SelectedScope,
        release: ReleaseRecord<'_>,
    ) -> Result<InstallLedger, LedgerError> {
        if record.scope != scope {
            return Err(LedgerError::Invalid);
        }
        let store = if scope == SelectedScope::Machine {
            crate::machine_authority::MachineRoot::acquire()
                .map_err(|error| LedgerError::Io {
                    path: self.root.clone(),
                    source: std::io::Error::other(error),
                })?
                .transaction_store()
        } else {
            FilesystemTransactionStore::private_at(&self.root)
        };
        zup_transaction::reconcile_committed(&store, record, &self.root, |durable| {
            self.publish_ledger(durable, scope, release)
                .map_err(|error| error.to_string())
        })
        .map_err(LedgerError::Reconciliation)
    }

    fn publish_ledger(
        &self,
        record: &TransactionRecord,
        scope: SelectedScope,
        release: ReleaseRecord<'_>,
    ) -> Result<InstallLedger, LedgerError> {
        if record.phase != TransactionPhase::Committed {
            return Err(LedgerError::Uncommitted);
        }
        if record.scope != scope {
            return Err(LedgerError::Invalid);
        }
        record.validate().map_err(|_| LedgerError::Invalid)?;
        if record.plan.nodes.iter().any(|node| {
            !matches!(node.kind, NodeKind::Barrier) && record.receipt(&node.id).is_none()
        }) {
            return Err(LedgerError::Uncommitted);
        }
        let previous = self.load(&record.app_id, scope)?;
        if previous
            .as_ref()
            .is_some_and(|ledger| ledger.target != record.target)
        {
            return Err(LedgerError::Ownership("target identity".into()));
        }
        let installed_transaction = previous
            .as_ref()
            .map(|ledger| ledger.committed_transaction.parse::<uuid::Uuid>())
            .transpose()
            .map_err(|_| LedgerError::Invalid)?;
        if installed_transaction.is_some_and(|id| id > record.transaction_id.as_uuid()) {
            cleanup_committed_files(record)?;
            self.cleanup_work(record)?;
            return Ok(previous.expect("newer installation exists"));
        }
        if previous
            .as_ref()
            .is_some_and(|ledger| ledger.committed_transaction == record.transaction_id.to_string())
        {
            cleanup_committed_files(record)?;
            self.cleanup_work(record)?;
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
                return Err(LedgerError::Invalid);
            }
            let path = self.path(&record.app_id, scope);
            if scope == SelectedScope::Machine {
                let name = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .ok_or(LedgerError::Invalid)?;
                crate::machine_authority::MachineRoot::acquire()
                    .map_err(|error| LedgerError::Io {
                        path: path.clone(),
                        source: std::io::Error::other(error),
                    })?
                    .remove_installation(name)
                    .map_err(|error| LedgerError::Io {
                        path: path.clone(),
                        source: std::io::Error::other(error),
                    })?;
            } else {
                match std::fs::remove_file(&path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(source) => return Err(LedgerError::Io { path, source }),
                }
            }
            cleanup_committed_files(record)?;
            cleanup_removed_directories(record)?;
            cleanup_application_state(&self.root, record)?;
            return Ok(InstallLedger::new(
                record.app_id.clone(),
                record.target,
                scope,
            ));
        }
        let mut ledger = previous
            .unwrap_or_else(|| InstallLedger::new(record.app_id.clone(), record.target, scope));
        if ledger.target != record.target {
            return Err(LedgerError::Ownership("target identity".into()));
        }
        ledger.version = record.app_version.clone();
        ledger.selected_components = record.plan.selected_components.clone();
        ledger.install_directory = record.plan.install_directory.clone();

        ledger.preset = record.plan.preset.clone();

        ledger.release = match release {
            ReleaseRecord::Identity(identity) => Some(identity.clone()),
            ReleaseRecord::None => None,
            ReleaseRecord::Preserve => ledger.release,
        };
        let retired_ledger_keys = retired_ledger_keys(&record.plan)?;
        for node in &record.plan.nodes {
            let key = match &node.kind {
                NodeKind::FileMutation { key, .. } => key.clone(),
                NodeKind::BackendOperation { .. } | NodeKind::BackendRemoval { .. } => {
                    ledger_key_for_backend_node(node)
                        .map_err(|error| LedgerError::Ownership(error.to_string()))?
                }
                _ => continue,
            };
            let Some(receipt) = record.receipt(&node.id) else {
                continue;
            };
            if let NodeKind::FileMutation { .. } = node.kind {
                let Some(source_relative) = node.meta.source_relative.clone() else {
                    return Err(LedgerError::Invalid);
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
                                    .map_err(|_| LedgerError::Invalid)
                            })
                            .collect::<Result<Vec<_>, _>>()?;
                        if let Some(OwnedResource::File {
                            created_directories: old,
                            ..
                        }) = ledger.resources.get(&key)
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
                        let directories = match ledger.resources.get(&key) {
                            Some(OwnedResource::File {
                                created_directories,
                                ..
                            }) => created_directories.clone(),
                            _ => Vec::new(),
                        };
                        (destination, new_sha256, *new_size, directories)
                    }
                    _ => return Err(LedgerError::Invalid),
                };
                let expected_path = match &key {
                    ResourceKey::File { destination }
                    | ResourceKey::Maintenance { destination, .. } => destination,
                    _ => return Err(LedgerError::Invalid),
                };
                let sha256 = digest.parse().map_err(|_| LedgerError::Invalid)?;
                if *destination != *expected_path
                    || node.meta.expected_sha256 != Some(sha256)
                    || node.meta.expected_size != Some(size)
                {
                    return Err(LedgerError::Invalid);
                }
                ledger.resources.insert(
                    key.clone(),
                    OwnedResource::File {
                        destination: target_path_from_host(Path::new(destination), &record.target)
                            .map_err(|_| LedgerError::Invalid)?,
                        source_relative,
                        sha256,
                        size,
                        created_directories,

                        privilege: node.meta.privilege.ok_or(LedgerError::Invalid)?,
                    },
                );
                continue;
            }
            let OperationReceipt::Backend { payload, .. } = receipt else {
                return Err(LedgerError::Invalid);
            };
            if let Some(owned) = owned_from_backend_receipt(
                &key,
                ledger.resources.get(&key),
                payload,
                &record.target,
            )? {
                ledger.resources.insert(key, owned);
            }
        }
        for transaction_key in &record.plan.retired_keys {
            let key = record
                .plan
                .nodes
                .iter()
                .find(|node| {
                    matches!(
                        &node.kind,
                        NodeKind::BackendOperation { key: node_key, .. }
                            | NodeKind::BackendRemoval { key: node_key }
                            if node_key == transaction_key
                    )
                })
                .map(ledger_key_for_backend_node)
                .transpose()
                .map_err(|error| LedgerError::Ownership(error.to_string()))?
                .unwrap_or_else(|| transaction_key.clone());
            let directories = match ledger.resources.get(&key) {
                Some(OwnedResource::File {
                    created_directories,
                    ..
                }) => created_directories.clone(),
                _ => Vec::new(),
            };
            for directory in directories {
                let inheritor = ledger.resources.iter().find_map(|(candidate, owned)| {
                    if candidate == &key || retired_ledger_keys.contains(candidate) {
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
        let path = self.path(&record.app_id, scope);
        let bytes = serde_json::to_vec_pretty(&ledger)?;
        if scope == SelectedScope::User {
            zup_transaction::publish_private(Path::new(&path), &bytes).map_err(|error| {
                LedgerError::Io {
                    path: PathBuf::from(&path),
                    source: match error {
                        private_fs::Error::Io(source) => source,
                        other => std::io::Error::other(other),
                    },
                }
            })?;
        } else {
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or(LedgerError::Invalid)?;
            crate::machine_authority::MachineRoot::acquire()
                .map_err(|error| LedgerError::Io {
                    path: path.clone(),
                    source: std::io::Error::other(error),
                })?
                .publish_installation(name, &bytes)
                .map_err(|error| LedgerError::Io {
                    path,
                    source: std::io::Error::other(error),
                })?;
        }
        cleanup_committed_files(record)?;

        cleanup_removed_directories(record)?;
        self.cleanup_work(record)?;
        Ok(ledger)
    }
}

fn read_ledger(path: &Path, scope: SelectedScope) -> std::io::Result<Vec<u8>> {
    if scope == SelectedScope::User {
        return zup_transaction::read_private(path).map_err(|error| match error {
            private_fs::Error::Io(source) => source,
            other => std::io::Error::other(other),
        });
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "ledger name"))?;
    let root = crate::machine_state::default_state_root(SelectedScope::Machine)
        .map_err(std::io::Error::other)?;
    match crate::machine_fs::MachineDir::read_ledger(&root, name) {
        Ok(Some(bytes)) => Ok(bytes),
        Ok(None) => Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "machine ledger is absent",
        )),
        Err(crate::machine_state::MachineStateError::Io { source, .. }) => Err(source),
        Err(error) => Err(std::io::Error::other(error)),
    }
}

fn retired_ledger_keys(plan: &TransactionPlan) -> Result<BTreeSet<ResourceKey>, LedgerError> {
    let mut keys = BTreeSet::new();
    for key in &plan.retired_keys {
        let ledger_key = plan
            .nodes
            .iter()
            .find(|node| {
                matches!(
                    &node.kind,
                    NodeKind::BackendOperation { key: node_key, .. }
                        | NodeKind::BackendRemoval { key: node_key }
                        if node_key == key
                )
            })
            .map(ledger_key_for_backend_node)
            .transpose()
            .map_err(|error| LedgerError::Ownership(error.to_string()))?
            .unwrap_or_else(|| key.clone());
        if !keys.insert(ledger_key) {
            return Err(LedgerError::Ownership("duplicate retired resources".into()));
        }
    }
    Ok(keys)
}

fn owned_from_backend_receipt(
    key: &ResourceKey,
    old: Option<&OwnedResource>,
    payload: &[u8],
    target: &zup_core::Target,
) -> Result<Option<OwnedResource>, LedgerError> {
    let receipt = receipt_from_bytes(payload).map_err(|_| LedgerError::Invalid)?;
    Ok(match receipt {
        BackendReceipt::Launcher {
            launcher_path,
            privilege,
            previous,
            installed,
        } => Some(OwnedResource::Launcher {
            launcher_path,
            privilege,
            previous: old
                .and_then(|value| match value {
                    OwnedResource::Launcher { previous, .. } => Some(previous.clone()),
                    _ => None,
                })
                .unwrap_or(previous),
            installed,
        }),
        BackendReceipt::Path {
            scope: _,
            entry,
            value_type,
            privilege,
        } => Some(OwnedResource::PathEntry {
            value: target_path_from_host(Path::new(&entry), target)
                .map_err(|_| LedgerError::Invalid)?,
            value_type,
            privilege,
        }),
        BackendReceipt::RemovePath { .. } => None,
        BackendReceipt::Service {
            name,
            privilege,
            previous,
            installed,
        } => Some(OwnedResource::Service {
            name,
            privilege,
            previous: old
                .and_then(|value| match value {
                    OwnedResource::Service { previous, .. } => Some(previous.clone()),
                    _ => None,
                })
                .unwrap_or(previous),
            installed,
        }),
        BackendReceipt::Protocol {
            privilege,
            previous,
            installed,
            ..
        } => Some(OwnedResource::Protocol {
            privilege,
            previous: old
                .and_then(|value| match value {
                    OwnedResource::Protocol { previous, .. } => Some(previous.clone()),
                    _ => None,
                })
                .unwrap_or(previous),
            installed,
        }),
        BackendReceipt::FileAssociation {
            privilege,
            previous,
            installed,
            ..
        } => Some(OwnedResource::FileAssociation {
            privilege,
            previous: old
                .and_then(|value| match value {
                    OwnedResource::FileAssociation { previous, .. } => Some(previous.clone()),
                    _ => None,
                })
                .unwrap_or(previous),
            installed,
        }),
        BackendReceipt::Extension {
            privilege,
            previous,
            installed,
            ..
        } => Some(OwnedResource::Extension {
            privilege,
            previous: old
                .and_then(|value| match value {
                    OwnedResource::Extension { previous, .. } => Some(previous.clone()),
                    _ => None,
                })
                .unwrap_or(previous),
            installed,
        }),
        BackendReceipt::AppsFeatures {
            privilege,
            installed: Some(state),
            ..
        } => Some(OwnedResource::Backend {
            id: match key {
                ResourceKey::Backend { id } => id.clone(),
                _ => return Err(LedgerError::Invalid),
            },
            privilege,
            payload: apps_owned_payload(&state),
        }),
        BackendReceipt::AppsFeatures {
            installed: None, ..
        } => None,
    })
}

fn valid_file_key(
    key: &ResourceKey,
    app_id: &AppId,
    app_version: &semver::Version,
    target: &zup_core::Target,
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

fn cleanup_application_state(
    root: &Path,
    uninstall: &TransactionRecord,
) -> Result<(), LedgerError> {
    let transactions = root.join("transactions");
    let machine = if uninstall.scope == SelectedScope::Machine {
        Some(
            crate::machine_authority::MachineRoot::acquire().map_err(|error| LedgerError::Io {
                path: root.to_path_buf(),
                source: std::io::Error::other(error),
            })?,
        )
    } else {
        None
    };
    let _machine_work = machine
        .as_ref()
        .map(|machine| machine.work_dir())
        .transpose()
        .map_err(|error| LedgerError::Io {
            path: root.join("work"),
            source: std::io::Error::other(error),
        })?;
    let store = if let Some(machine) = &machine {
        machine.transaction_store()
    } else {
        FilesystemTransactionStore::private_at(root)
    };
    let ids = store.list_ids().map_err(|error| LedgerError::Io {
        path: transactions.clone(),
        source: std::io::Error::other(error),
    })?;
    for id in ids {
        let record = store.load(&id).map_err(|_| LedgerError::Invalid)?;
        if record.app_id != uninstall.app_id
            || record.scope != uninstall.scope
            || record.transaction_id.as_uuid() >= uninstall.transaction_id.as_uuid()
        {
            continue;
        }
        if !matches!(
            record.phase,
            TransactionPhase::Committed | TransactionPhase::RolledBack
        ) {
            return Err(LedgerError::RecoveryRequired(
                record.transaction_id.to_string(),
            ));
        }
        let work_directory = root.join("work").join(record.transaction_id.to_string());
        match std::fs::remove_dir_all(&work_directory) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(LedgerError::Io {
                    path: work_directory,
                    source,
                });
            }
        }
        if let Some(work) = &_machine_work {
            work.sync().map_err(|error| LedgerError::Io {
                path: root.join("work"),
                source: std::io::Error::other(error),
            })?;
        }
        if let Some(machine) = &machine {
            machine
                .remove_journal(&record.transaction_id)
                .map_err(|error| LedgerError::Io {
                    path: transactions.join(record.transaction_id.to_string()),
                    source: std::io::Error::other(error),
                })?;
        } else {
            let directory = transactions.join(record.transaction_id.to_string());
            if std::fs::symlink_metadata(&directory)
                .is_ok_and(|metadata| metadata.file_type().is_dir())
            {
                std::fs::remove_dir_all(&directory).map_err(|source| LedgerError::Io {
                    path: directory.clone(),
                    source,
                })?;
            } else {
                return Err(LedgerError::Invalid);
            }
        }
    }
    let work_directory = root.join("work").join(uninstall.transaction_id.to_string());
    if let Some(machine) = &machine {
        machine
            .remove_work(&uninstall.transaction_id)
            .map_err(|error| LedgerError::Io {
                path: work_directory,
                source: std::io::Error::other(error),
            })?;
    } else {
        match std::fs::remove_dir_all(&work_directory) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(LedgerError::Io {
                    path: work_directory,
                    source,
                });
            }
        }
    }
    if machine.is_none() {
        for directory in [
            root.join("transactions"),
            root.join("work"),
            root.join("installations"),
        ] {
            cleanup_empty_directory_tree(&directory)?;
        }
    }
    Ok(())
}

fn maintenance_root(path: &TargetPath, app_id: &AppId) -> Option<TargetPath> {
    let mut current = Some(path.clone());
    while let Some(candidate) = current {
        if candidate.file_name() == Some(app_id.as_str())
            && candidate
                .parent()
                .is_some_and(|parent| parent.file_name() == Some("maintenance"))
        {
            return Some(candidate);
        }
        current = candidate.parent();
    }
    None
}

fn cleanup_empty_directory_tree(root: &Path) -> Result<(), LedgerError> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(LedgerError::Io {
                path: root.to_path_buf(),
                source,
            });
        }
    };
    let mut directories = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| LedgerError::Io {
            path: root.to_path_buf(),
            source,
        })?;
        let file_type = entry.file_type().map_err(|source| LedgerError::Io {
            path: entry.path(),
            source,
        })?;
        if file_type.is_dir() {
            directories.push(entry.path());
        }
    }
    directories.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    for directory in directories {
        cleanup_empty_directory_tree(&directory)?;
    }
    match std::fs::remove_dir(root) {
        Ok(()) => {}
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty
            ) => {}
        Err(source) => {
            return Err(LedgerError::Io {
                path: root.to_path_buf(),
                source,
            });
        }
    }
    Ok(())
}

fn cleanup_removed_directories(record: &TransactionRecord) -> Result<(), LedgerError> {
    let mut directories = std::collections::BTreeSet::new();
    let mut maintenance_roots = std::collections::BTreeSet::new();
    for node in &record.plan.nodes {
        if !matches!(node.kind, NodeKind::FileRemoval { .. }) {
            continue;
        }
        let Some(removal) = &node.meta.removal else {
            continue;
        };
        if let Some(root) = maintenance_root(&removal.destination, &record.app_id) {
            maintenance_roots.insert(root);
        }
        for directory in &removal.created_directories {
            directories.insert(directory.clone());
            let Some(root) = maintenance_root(directory, &record.app_id) else {
                continue;
            };
            maintenance_roots.insert(root.clone());
            let mut current = directory.parent();
            while let Some(candidate) = current {
                directories.insert(candidate.clone());
                if candidate == root {
                    break;
                }
                if !candidate.starts_with(&root) {
                    break;
                }
                current = candidate.parent();
            }
        }
    }
    let mut directories: Vec<_> = directories.into_iter().collect();
    directories.sort_by_key(|path| std::cmp::Reverse(path.as_str().len()));
    for directory in directories {
        let path = host_path(&directory);
        match std::fs::remove_dir(&path) {
            Ok(()) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty
                ) => {}
            Err(source) => {
                return Err(LedgerError::Io { path, source });
            }
        }
    }
    for root in maintenance_roots {
        cleanup_empty_directory_tree(&host_path(&root))?;
    }
    Ok(())
}

fn cleanup_committed_files(record: &TransactionRecord) -> Result<(), LedgerError> {
    for node in &record.plan.nodes {
        let Some(receipt) = record.receipt(&node.id) else {
            continue;
        };
        let (backup, sha256, size) = match receipt {
            OperationReceipt::StageFile {
                staged_path,
                sha256,
                size,
            } => (
                PathBuf::from(staged_path),
                sha256.parse().map_err(|_| LedgerError::Invalid)?,
                *size,
            ),
            OperationReceipt::ReplaceFile {
                backup_path,
                previous_sha256,
                previous_size,
                ..
            } => (
                PathBuf::from(backup_path),
                previous_sha256.parse().map_err(|_| LedgerError::Invalid)?,
                *previous_size,
            ),
            OperationReceipt::RemoveFile {
                backup_path,
                sha256,
                size,
                ..
            } => (host_path(backup_path), *sha256, *size),
            _ => continue,
        };
        match std::fs::symlink_metadata(&backup) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(LedgerError::Io {
                    path: backup.to_path_buf(),
                    source,
                });
            }
            Ok(metadata) if metadata.file_type().is_file() => {
                let file = std::fs::File::open(&backup).map_err(|source| LedgerError::Io {
                    path: backup.to_path_buf(),
                    source,
                })?;
                if zup_core::hash_reader(file).map_err(|source| LedgerError::Io {
                    path: backup.to_path_buf(),
                    source,
                })? != (size, sha256)
                {
                    return Err(LedgerError::Invalid);
                }
                std::fs::remove_file(&backup).map_err(|source| LedgerError::Io {
                    path: backup.to_path_buf(),
                    source,
                })?;
            }
            Ok(_) => return Err(LedgerError::Invalid),
        }
    }
    Ok(())
}
