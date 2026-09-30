use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use thiserror::Error;
use zup_core::ReleaseIdentity;
use zup_core::{AppId, ResourceKey, SelectedScope, Sha256Digest};
use zup_exec::{INSTALL_LEDGER_SCHEMA, InstallLedger, OwnedResource};
use zup_platform::TargetPath;
use zup_transaction::{
    BackendOperationIntent, FileDelta, FilePrecondition, FilesystemTransactionStore, NodeKind,
    OperationReceipt, TransactionId, TransactionPhase, TransactionPlan, TransactionRecord,
    TransactionStore,
};

use crate::durable::{DurableError, write_durable};
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
    #[error(transparent)]
    Durable(#[from] DurableError),
}

pub struct InstallLedgerStore {
    root: PathBuf,
}

/// What a committing transaction knows about the release graph it came from.
///
/// Three cases, not two. Collapsing "no graph" into "keep the old one" would
/// leave a development run claiming a release it never touched, and collapsing
/// "keep the old one" into "no graph" would erase a real identity the first time
/// the machine replayed its journal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleaseRecord<'a> {
    /// Record this authenticated graph.
    Identity(&'a ReleaseIdentity),
    /// This transaction was not produced from a release graph.
    None,
    /// This is a replay of an already-committed transaction.
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
        let bytes = match std::fs::read(&path) {
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

    /// Where one application's ownership record is kept.
    pub fn path_for(&self, app_id: &AppId, scope: SelectedScope) -> PathBuf {
        self.path(app_id, scope)
    }

    pub fn repair_committed(
        &self,
        app_id: &AppId,
        scope: SelectedScope,
    ) -> Result<(), LedgerError> {
        let directory = self.root.join("transactions");
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(source) => {
                return Err(LedgerError::Io {
                    path: directory,
                    source,
                });
            }
        };
        let journals = FilesystemTransactionStore::new(&self.root);
        let mut committed = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| LedgerError::Io {
                path: directory.clone(),
                source,
            })?;
            let Some(id) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<uuid::Uuid>().ok())
            else {
                continue;
            };
            let record = journals
                .load(&TransactionId::from_uuid(id))
                .map_err(|_| LedgerError::Invalid)?;
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
                TransactionPhase::RolledBack => {}
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
            .and_then(|ledger| ledger.committed_transaction.parse::<uuid::Uuid>().ok());
        for record in committed {
            let id = record.transaction_id.as_uuid();
            if latest.is_none_or(|previous| id > previous) {
                self.publish_committed(&record, scope, ReleaseRecord::Preserve)?;
                latest = Some(id);
            } else {
                cleanup_committed_files(&record)?;
            }
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
        if ledger
            .as_ref()
            .is_some_and(|ledger| ledger.target != plan.target)
        {
            return Err(LedgerError::Ownership("target identity".into()));
        }
        let retired: BTreeSet<_> = plan.retired_keys.iter().collect();
        let retired_ledger_keys = retired_ledger_keys(plan)?;
        if retired.len() != plan.retired_keys.len()
            || retired_ledger_keys.len() != retired.len()
            || retired_ledger_keys.iter().any(|key| {
                ledger
                    .as_ref()
                    .is_none_or(|ledger| !ledger.resources.contains_key(key))
            })
            || (plan.uninstall
                && ledger
                    .as_ref()
                    .is_some_and(|ledger| retired_ledger_keys.len() != ledger.resources.len()))
        {
            return Err(LedgerError::Ownership("retired resources".into()));
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
                            matches!(node.meta.file_precondition, Some(FilePrecondition::Exact { size: found_size, sha256: found_hash }) if found_size == *size && found_hash == *sha256)
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
                        return Err(LedgerError::Ownership(node.id.to_string()));
                    }
                }
                NodeKind::FileRemoval { key } => {
                    let Some(removal) = &node.meta.removal else {
                        return Err(LedgerError::Ownership(node.id.to_string()));
                    };
                    let Some(OwnedResource::File {
                        destination,
                        sha256,
                        size,
                        ..
                    }) = ledger.as_ref().and_then(|ledger| ledger.resources.get(key))
                    else {
                        return Err(LedgerError::Ownership(node.id.to_string()));
                    };
                    if !retired.contains(key)
                        || removal.key != *key
                        || removal.destination != *destination
                        || removal.sha256 != *sha256
                        || removal.size != *size
                    {
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
                NodeKind::Barrier | NodeKind::StageFile { .. } => {}
            }
        }
        self.validate_ui(app_id, scope, app_version, plan, ledger.as_ref())?;
        Ok(())
    }

    /// The plan's UI runtime and the plan's files have to be the same generation.
    ///
    /// A plan that names a window must install exactly the content that window
    /// needs, at the paths the runtime will look in - otherwise a machine
    /// believes it can present a preset whose bytes it never wrote. A plan that
    /// names no window must retire the bytes the previous one owned, so an
    /// update cannot leave an installation holding a preset nothing will launch.
    fn validate_ui(
        &self,
        app_id: &AppId,
        scope: SelectedScope,
        app_version: &semver::Version,
        plan: &TransactionPlan,
        ledger: Option<&InstallLedger>,
    ) -> Result<(), LedgerError> {
        let root = crate::plain_path_text(&crate::content_store::maintenance_root(
            &self.root, app_id, scope,
        ));
        let owned: BTreeMap<&str, &Sha256Digest> = ledger
            .into_iter()
            .flat_map(|ledger| ledger.resources.iter())
            .filter_map(|(key, resource)| match (key, resource) {
                (ResourceKey::File { destination }, OwnedResource::File { sha256, .. })
                    if crate::ui_runtime::is_content_path(Path::new(&root), destination) =>
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
        // Compared as the ledger spells a path, which is without the extended
        // length prefix Windows hands back once a component is long. Two
        // spellings of one file are two identities to everything that stores
        // ownership, and this is one of the things that stores it.
        let wanted: BTreeMap<String, Sha256Digest> = match &plan.ui {
            Some(ui) => {
                let directory = crate::content_store::maintenance_directory(
                    &self.root,
                    app_id,
                    scope,
                    app_version,
                );
                std::iter::once((
                    crate::plain_path_text(&crate::ui_runtime::preset_path(
                        &directory,
                        &ui.executable,
                    )),
                    ui.executable,
                ))
                .chain(ui.preset.assets.iter().map(|asset| {
                    (
                        crate::plain_path_text(&crate::ui_runtime::asset_path(
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
            // Provided either by this plan or already owned at exactly this
            // content: a repair of a working installation installs nothing, and
            // requiring it to would make repair impossible on a healthy machine.
            if installed.get(path.as_str()) != Some(digest)
                && !owned
                    .get(path.as_str())
                    .is_some_and(|found| **found == *digest)
            {
                return Err(LedgerError::Ownership(format!(
                    "the UI runtime needs {path}, and this plan neither installs it nor already \
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
                // Nothing was owned, so there is nothing to retire.
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

    /// Publish a committed transaction as the installation's ownership state.
    ///
    /// `release` says what this transaction knows about the graph it came from.
    /// The three cases are genuinely different and conflating any two of them
    /// would make the ledger lie.
    pub fn publish_committed(
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
                return Err(LedgerError::Invalid);
            }
            let path = self.path(&record.app_id, scope);
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(source) => return Err(LedgerError::Io { path, source }),
            }
            cleanup_committed_files(record)?;
            cleanup_removed_directories(record)?;
            cleanup_application_state(&self.root, record)?;
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
            return Err(LedgerError::Ownership("target identity".into()));
        }
        ledger.version = record.app_version.clone();
        ledger.selected_components = record.plan.selected_components.clone();
        ledger.install_directory = record.plan.install_directory.clone();
        // The window this installation presents, replaced as one value. Carried
        // in the plan rather than passed in beside it, so a replay of a journal
        // restores the same window the first commit recorded and recovery has
        // something to present with.
        ledger.ui = record.plan.ui.clone();
        // The identity follows the transaction, not the ledger. A replay keeps
        // whatever the first commit recorded, because the journal cannot supply
        // it; a development run clears it, because claiming a graph it did not
        // come from would make a later repair restore the wrong bytes.
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
                        // File authority is stated per operation by the plan.
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
        write_durable(Path::new(&path), &bytes)?;
        cleanup_committed_files(record)?;
        // Whatever this transaction retired, the directories it emptied go too.
        // A generation that has been replaced is not a directory tree somebody
        // left to find, and a state root that accumulates one per version is a
        // state root nobody can read.
        cleanup_removed_directories(record)?;
        Ok(ledger)
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
    target: &zup_core::TargetTriple,
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

fn cleanup_application_state(
    root: &Path,
    uninstall: &TransactionRecord,
) -> Result<(), LedgerError> {
    let transactions = root.join("transactions");
    let entries = match std::fs::read_dir(&transactions) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(LedgerError::Io {
                path: transactions,
                source,
            });
        }
    };
    let store = FilesystemTransactionStore::new(root);
    for entry in entries {
        let entry = entry.map_err(|source| LedgerError::Io {
            path: transactions.clone(),
            source,
        })?;
        let Some(id) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<uuid::Uuid>().ok())
        else {
            continue;
        };
        let transaction_id = TransactionId::from_uuid(id);
        let record = store
            .load(&transaction_id)
            .map_err(|_| LedgerError::Invalid)?;
        if record.app_id != uninstall.app_id || record.scope != uninstall.scope {
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
        let directory = entry.path();
        if std::fs::symlink_metadata(&directory).is_ok_and(|metadata| metadata.file_type().is_dir())
        {
            std::fs::remove_dir_all(&directory).map_err(|source| LedgerError::Io {
                path: directory.clone(),
                source,
            })?;
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
        } else {
            return Err(LedgerError::Invalid);
        }
    }
    let work_directory = root.join("work").join(uninstall.transaction_id.to_string());
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
    for directory in [
        root.join("transactions"),
        root.join("work"),
        root.join("installations"),
    ] {
        cleanup_empty_directory_tree(&directory)?;
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
        let OperationReceipt::RemoveFile {
            backup_path,
            sha256,
            size,
            ..
        } = receipt
        else {
            continue;
        };
        let backup = host_path(backup_path);
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
                })? != (*size, *sha256)
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
