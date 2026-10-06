//! Where one installation's ownership record lives, and what committing means.
//!
//! The journal format and the file names are identical to the Windows
//! backend's: `installations/<sha256(app_id)>-<scope>.json` holds the ledger,
//! and the ledger is the same `InstallLedger` document. Two backends
//! disagreeing about either would mean one of them could not read the other's
//! installations, so there is exactly one spelling and it lives in the
//! portable types both backends build on.
//!
//! What is Linux-specific is everything around that document: durable writes
//! through this backend's filesystem primitives, host-path conversions through
//! Linux lowering, and scope. Only file mutations are published - a plan with
//! backend operations in it cannot reach a Linux transaction, and a ledger
//! store that accepted one would be recording ownership of work no Linux
//! executor performed.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use zup_core::{AppId, ResourceKey, SelectedScope, Sha256Digest};
use zup_exec::{INSTALL_LEDGER_SCHEMA, InstallLedger, OwnedResource};
use zup_transaction::{
    FileDelta, FilePrecondition, NodeKind, OperationReceipt, TransactionPhase, TransactionPlan,
    TransactionRecord, TransactionStore,
};

use crate::fs::OwnedDirectory;
use crate::lowering::{target_path_from_host, to_host_path};

/// Why the ownership record could not be read or published.
#[derive(Debug, thiserror::Error)]
pub enum LinuxLedgerError {
    #[error("ledger I/O at `{path}`: {source}")]
    Io {
        path: PathBuf,
        #[source]
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
}

/// One scope's installation ownership records, rooted at a state root.
pub struct LinuxLedgerStore {
    root: PathBuf,
}

impl LinuxLedgerStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Where one application's ownership record is kept.
    ///
    /// The name is a digest of the application id plus the scope, so two
    /// applications never share a file and neither does a scope. Identical to
    /// the Windows backend's naming, because the ledger is one document with
    /// one spelling no matter which backend wrote it.
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

    /// Read one application's ownership record, or `None` when absent.
    ///
    /// Absence is a normal answer - a machine that never installed this
    /// application has no record of it - and anything unreadable is an error
    /// rather than an invitation to install over unknown state.
    pub fn load(
        &self,
        app_id: &AppId,
        scope: SelectedScope,
    ) -> Result<Option<InstallLedger>, LinuxLedgerError> {
        let path = self.path_for(app_id, scope);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(LinuxLedgerError::Io { path, source }),
        };
        let ledger: InstallLedger = serde_json::from_slice(&bytes)?;
        if ledger.schema != INSTALL_LEDGER_SCHEMA
            || ledger.app_id != *app_id
            || ledger.scope != scope
        {
            return Err(LinuxLedgerError::Invalid);
        }
        Ok(Some(ledger))
    }

    /// Check a compiled plan against committed ownership before it runs.
    ///
    /// A transaction that would create what is already owned, replace what is
    /// owned differently, or retire what was never owned is refused here, while
    /// nothing is held and nothing is staged. The checks mirror the journal's
    /// own validation, because a plan that passes one and fails the other
    /// would be executable but unpublishable - work performed for a ledger
    /// that can never record it.
    pub fn validate_plan(
        &self,
        app_id: &AppId,
        scope: SelectedScope,
        app_version: &semver::Version,
        plan: &TransactionPlan,
    ) -> Result<(), LinuxLedgerError> {
        plan.validate()
            .map_err(|error| LinuxLedgerError::Ownership(error.to_string()))?;
        let ledger = self.load(app_id, scope)?;
        if ledger
            .as_ref()
            .is_some_and(|ledger| ledger.target != plan.target)
        {
            return Err(LinuxLedgerError::Ownership("target identity".into()));
        }
        let retired: BTreeSet<_> = plan.retired_keys.iter().collect();
        if retired.len() != plan.retired_keys.len()
            || retired.iter().any(|key| {
                ledger
                    .as_ref()
                    .is_none_or(|ledger| !ledger.resources.contains_key(*key))
            })
            || (plan.uninstall
                && ledger.as_ref().is_some_and(|ledger| {
                    retired.len() != ledger.resources.len()
                        || retired
                            .iter()
                            .any(|key| !ledger.resources.contains_key(*key))
                }))
        {
            return Err(LinuxLedgerError::Ownership("retired resources".into()));
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
                        return Err(LinuxLedgerError::Ownership(node.id.to_string()));
                    }
                }
                NodeKind::FileRemoval { key } => {
                    let Some(removal) = &node.meta.removal else {
                        return Err(LinuxLedgerError::Ownership(node.id.to_string()));
                    };
                    let Some(OwnedResource::File {
                        destination,
                        sha256,
                        size,
                        ..
                    }) = ledger.as_ref().and_then(|ledger| ledger.resources.get(key))
                    else {
                        return Err(LinuxLedgerError::Ownership(node.id.to_string()));
                    };
                    if !retired.contains(key)
                        || removal.key != *key
                        || removal.destination != *destination
                        || removal.sha256 != *sha256
                        || removal.size != *size
                    {
                        return Err(LinuxLedgerError::Ownership(node.id.to_string()));
                    }
                }
                // A refresh regenerates derived freedesktop databases from the
                // authoritative files above. It owns no bytes, so the ledger
                // records nothing for it; validation only proves the node is
                // one this backend emitted, with a bounded, well-formed
                // request. Anything else backend-shaped is foreign.
                NodeKind::BackendOperation { key, .. } => {
                    let Some(backend) = &node.meta.backend else {
                        return Err(LinuxLedgerError::Ownership(node.id.to_string()));
                    };
                    if backend.key != *key {
                        return Err(LinuxLedgerError::Ownership(node.id.to_string()));
                    }
                    let request = crate::refresh::RefreshRequest::decode(&backend.payload)
                        .map_err(|_| LinuxLedgerError::Ownership(node.id.to_string()))?;
                    if request.key() != *key {
                        return Err(LinuxLedgerError::Ownership(node.id.to_string()));
                    }
                }
                NodeKind::BackendRemoval { .. } => {
                    return Err(LinuxLedgerError::Ownership(node.id.to_string()));
                }
                NodeKind::Barrier | NodeKind::StageFile { .. } => {}
            }
        }
        if plan.preset.is_some() {
            return Err(LinuxLedgerError::Ownership(
                "a Linux console or headless transaction presents no window".into(),
            ));
        }
        Ok(())
    }

    /// Publish a committed transaction as the installation's ownership state.
    ///
    /// File mutations become owned files with the receipt's identity;
    /// retired keys leave; an uninstall removes the ledger itself and then the
    /// transaction state the lifecycle rules say goes with it. Every write is
    /// durable before it is visible, because a ledger that names files the
    /// disk never received is worse than no ledger at all.
    pub fn publish_committed(
        &self,
        record: &TransactionRecord,
        scope: SelectedScope,
    ) -> Result<InstallLedger, LinuxLedgerError> {
        if record.phase != TransactionPhase::Committed {
            return Err(LinuxLedgerError::Uncommitted);
        }
        if record.scope != scope {
            return Err(LinuxLedgerError::Invalid);
        }
        record.validate().map_err(|_| LinuxLedgerError::Invalid)?;
        if record.plan.nodes.iter().any(|node| {
            !matches!(node.kind, NodeKind::Barrier) && record.receipt(&node.id).is_none()
        }) {
            return Err(LinuxLedgerError::Uncommitted);
        }
        let previous = self.load(&record.app_id, scope)?;
        if previous
            .as_ref()
            .is_some_and(|ledger| ledger.target != record.target)
        {
            return Err(LinuxLedgerError::Ownership("target identity".into()));
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
                return Err(LinuxLedgerError::Invalid);
            }
            let path = self.path_for(&record.app_id, scope);
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(source) => return Err(LinuxLedgerError::Io { path, source }),
            }
            cleanup_committed_files(record)?;
            cleanup_removed_directories(record)?;
            cleanup_application_state(&self.root, record)?;
            // The maintenance generations are all retired with the files they
            // held: whatever version directories are now empty go, and one
            // holding anything else stays, because zup cannot prove it owns it.
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
                                return Err(LinuxLedgerError::Io { path, source });
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
            return Err(LinuxLedgerError::Ownership("target identity".into()));
        }
        ledger.version = record.app_version.clone();
        ledger.selected_components = record.plan.selected_components.clone();
        ledger.install_directory = record.plan.install_directory.clone();
        ledger.preset = None;
        // A self-contained installer carries no release graph: the bytes are
        // the authority, and claiming a graph they did not come from would
        // make a later repair restore the wrong ones.
        ledger.release = None;
        for node in &record.plan.nodes {
            let NodeKind::FileMutation { key, .. } = &node.kind else {
                continue;
            };
            let Some(receipt) = record.receipt(&node.id) else {
                continue;
            };
            let Some(source_relative) = node.meta.source_relative.clone() else {
                return Err(LinuxLedgerError::Invalid);
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
                                .map_err(|_| LinuxLedgerError::Invalid)
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
                _ => return Err(LinuxLedgerError::Invalid),
            };
            let expected_path = match key {
                ResourceKey::File { destination }
                | ResourceKey::Maintenance { destination, .. } => destination,
                _ => return Err(LinuxLedgerError::Invalid),
            };
            let sha256: Sha256Digest = digest.parse().map_err(|_| LinuxLedgerError::Invalid)?;
            if *destination != *expected_path
                || node.meta.expected_sha256 != Some(sha256)
                || node.meta.expected_size != Some(size)
            {
                return Err(LinuxLedgerError::Invalid);
            }
            ledger.resources.insert(
                key.clone(),
                OwnedResource::File {
                    destination: target_path_from_host(Path::new(destination), &record.target)
                        .map_err(|_| LinuxLedgerError::Invalid)?,
                    source_relative,
                    sha256,
                    size,
                    created_directories,
                    privilege: node.meta.privilege.ok_or(LinuxLedgerError::Invalid)?,
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
        // Whatever this transaction retired, the directories it emptied go
        // too. A state root that accumulates one directory per version is a
        // state root nobody can read.
        cleanup_removed_directories(record)?;
        Ok(ledger)
    }

    /// Replay committed journals newer than the ledger, or refuse when an
    /// unfinished transaction needs recovery first.
    ///
    /// A machine that crashed between commit and publish has a journal that
    /// says more than its ledger does. Publishing every committed record newer
    /// than the ledger's own transaction closes that gap, and refusing on any
    /// other phase keeps a half-written future from becoming ownership.
    pub fn repair_committed(
        &self,
        app_id: &AppId,
        scope: SelectedScope,
    ) -> Result<(), LinuxLedgerError> {
        let directory = self.root.join("transactions");
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(source) => {
                return Err(LinuxLedgerError::Io {
                    path: directory,
                    source,
                });
            }
        };
        let journals = zup_transaction::FilesystemTransactionStore::new(&self.root);
        let mut committed = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| LinuxLedgerError::Io {
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
                .load(&zup_transaction::TransactionId::from_uuid(id))
                .map_err(|_| LinuxLedgerError::Invalid)?;
            if record.app_id != *app_id || record.scope != scope {
                continue;
            }
            if self
                .load(app_id, scope)?
                .as_ref()
                .is_some_and(|ledger| ledger.target != record.target)
            {
                return Err(LinuxLedgerError::Ownership("target identity".into()));
            }
            match record.phase {
                TransactionPhase::Committed => committed.push(record),
                TransactionPhase::RolledBack => {}
                _ => {
                    return Err(LinuxLedgerError::RecoveryRequired(
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

/// Whether a file key belongs to this application and version.
///
/// Maintenance keys name their owner and version explicitly, so a maintenance
/// file for another application - or another version - is refused rather than
/// recorded as this installation's own.
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

fn retired_ledger_keys(plan: &TransactionPlan) -> Result<BTreeSet<ResourceKey>, LinuxLedgerError> {
    let mut keys = BTreeSet::new();
    for key in &plan.retired_keys {
        if !keys.insert(key.clone()) {
            return Err(LinuxLedgerError::Ownership(
                "duplicate retired resources".into(),
            ));
        }
    }
    Ok(keys)
}

/// Write a ledger durably: temporary sibling, flush, rename, flush directory.
///
/// The ledger is the machine's memory of what it owns. A torn write is not a
/// corrupt file - it is an installation the machine has forgotten, and
/// forgetting means the next run plans over files it believes are nobody's.
fn write_ledger(path: &Path, bytes: &[u8]) -> Result<(), LinuxLedgerError> {
    let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    else {
        return Err(LinuxLedgerError::Io {
            path: path.to_path_buf(),
            source: std::io::Error::other("a ledger path has a parent directory"),
        });
    };
    let directory =
        OwnedDirectory::create(parent, crate::fs::STATE_DIRECTORY_MODE).map_err(|error| {
            LinuxLedgerError::Io {
                path: path.to_path_buf(),
                source: std::io::Error::other(error.to_string()),
            }
        })?;
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .ok_or_else(|| LinuxLedgerError::Io {
            path: path.to_path_buf(),
            source: std::io::Error::other("a ledger path has a file name"),
        })?;
    directory
        .write_durable(&name, bytes, crate::fs::STATE_FILE_MODE)
        .map_err(|error| LinuxLedgerError::Io {
            path: path.to_path_buf(),
            source: std::io::Error::other(error.to_string()),
        })
}

/// Remove the verified backups of committed removals.
///
/// A backup that survived its transaction's commit is garbage with a purpose
/// already served: the removal it could have undone is now ownership, and
/// ownership is restored by planning, not by keeping every replaced byte
/// forever.
fn cleanup_committed_files(record: &TransactionRecord) -> Result<(), LinuxLedgerError> {
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
        let backup = to_host_path(backup_path).map_err(|_| LinuxLedgerError::Invalid)?;
        match std::fs::symlink_metadata(&backup) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(LinuxLedgerError::Io {
                    path: backup.clone(),
                    source,
                });
            }
            Ok(metadata) if metadata.file_type().is_file() => {
                let file = std::fs::File::open(&backup).map_err(|source| LinuxLedgerError::Io {
                    path: backup.clone(),
                    source,
                })?;
                if zup_core::hash_reader(file).map_err(|source| LinuxLedgerError::Io {
                    path: backup.clone(),
                    source,
                })? != (*size, *sha256)
                {
                    return Err(LinuxLedgerError::Invalid);
                }
                std::fs::remove_file(&backup).map_err(|source| LinuxLedgerError::Io {
                    path: backup.clone(),
                    source,
                })?;
            }
            Ok(_) => return Err(LinuxLedgerError::Invalid),
        }
    }
    Ok(())
}

/// Remove the directories a transaction emptied, deepest first.
///
/// Only directories the transaction's own removals named, and only while
/// empty: a directory that has acquired content of its own since is left
/// alone, because zup cannot prove it owns it.
fn cleanup_removed_directories(record: &TransactionRecord) -> Result<(), LinuxLedgerError> {
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
        let path = to_host_path(&directory).map_err(|_| LinuxLedgerError::Invalid)?;
        match std::fs::remove_dir(&path) {
            Ok(()) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty
                ) => {}
            Err(source) => {
                return Err(LinuxLedgerError::Io { path, source });
            }
        }
    }
    Ok(())
}

/// Remove all of one application's transaction state after an uninstall.
///
/// Every journal for this application must already be committed or rolled
/// back: anything else means a transaction is still in flight, and deleting
/// its record would be destroying the evidence of what it did.
fn cleanup_application_state(
    root: &Path,
    uninstall: &TransactionRecord,
) -> Result<(), LinuxLedgerError> {
    let transactions = root.join("transactions");
    let entries = match std::fs::read_dir(&transactions) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(LinuxLedgerError::Io {
                path: transactions,
                source,
            });
        }
    };
    let store = zup_transaction::FilesystemTransactionStore::new(root);
    for entry in entries {
        let entry = entry.map_err(|source| LinuxLedgerError::Io {
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
        let record = store
            .load(&zup_transaction::TransactionId::from_uuid(id))
            .map_err(|_| LinuxLedgerError::Invalid)?;
        if record.app_id != uninstall.app_id || record.scope != uninstall.scope {
            continue;
        }
        if !matches!(
            record.phase,
            TransactionPhase::Committed | TransactionPhase::RolledBack
        ) {
            return Err(LinuxLedgerError::RecoveryRequired(
                record.transaction_id.to_string(),
            ));
        }
        let directory = entry.path();
        if std::fs::symlink_metadata(&directory).is_ok_and(|metadata| metadata.is_dir()) {
            std::fs::remove_dir_all(&directory).map_err(|source| LinuxLedgerError::Io {
                path: directory.clone(),
                source,
            })?;
        } else {
            return Err(LinuxLedgerError::Invalid);
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
                return Err(LinuxLedgerError::Io {
                    path: directory,
                    source,
                });
            }
        }
    }
    Ok(())
}
