//! Windows file mutation executor (Create / Replace / Stage / Verify).
//!
//! Files only. No registry, PATH, launchers, services, protocols, or file
//! associations. All payload access goes through `PayloadSource`.
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use sha2::Digest as _;
use thiserror::Error;
use zup_bundle::{PayloadError, PayloadSource};
use zup_core::{ResourceKey, Sha256Digest, hash_reader};
use zup_transaction::{
    FileDelta, FilePrecondition, NodeKind, OperationId, ReconcileResult, TransactionNode,
};

use crate::durable::{DurableError, move_durable};
use crate::lowering::{host_path, target_path_from_host};

/// Receipt recorded after staging a payload.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StageFileReceipt {
    pub staged_path: String,
    pub size: u64,
    pub sha256: Sha256Digest,
}

/// Receipt recorded after creating a file.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CreateFileReceipt {
    pub destination: String,
    pub installed_sha256: Sha256Digest,
    pub installed_size: u64,
    /// Whether the installed file is executable. See the journal receipt.
    pub executable: bool,
    /// Directories created by zup for this operation (rollback candidates).
    pub created_directories: Vec<String>,
}

/// Receipt recorded after replacing a file.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReplaceFileReceipt {
    pub destination: String,
    pub previous_sha256: Sha256Digest,
    pub previous_size: u64,
    pub backup_path: String,
    pub new_sha256: Sha256Digest,
    pub new_size: u64,
    /// Whether the installed file is executable. See the journal receipt.
    pub executable: bool,
}

/// Typed operation receipt (journal schema).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum OperationReceipt {
    StageFile(StageFileReceipt),
    CreateFile(CreateFileReceipt),
    ReplaceFile(ReplaceFileReceipt),
    Control,
}

/// Synchronous progress events (no Tokio).
#[derive(Debug, Clone)]
pub enum FileProgress {
    PreflightStarted,
    StagingFile { id: String },
    FileCreateStarted { id: String },
    FileReplaceStarted { id: String },
    Verifying,
    RollingBack,
    Committed,
}

/// Progress sink trait.
pub trait ProgressSink: Send {
    fn on_event(&mut self, event: FileProgress);
}

/// No-op progress sink.
pub struct NullProgress;
impl ProgressSink for NullProgress {
    fn on_event(&mut self, _event: FileProgress) {}
}

/// File executor errors.
#[derive(Debug, Error)]
pub enum WindowsFileExecutorError {
    #[error("plan drift on `{path}`: {reason}")]
    PlanDrift { path: String, reason: String },

    #[error("payload error: {0}")]
    Payload(#[from] PayloadError),

    #[error("staging failed: {0}")]
    Staging(String),

    #[error("durability error: {0}")]
    Durable(#[from] DurableError),

    #[error("verification failed for `{path}`: {reason}")]
    Verification { path: String, reason: String },

    #[error("ownership/drift during rollback of `{path}`: {reason}")]
    RollbackDrift { path: String, reason: String },

    #[error("unsupported operation `{id}` - fail closed")]
    Unsupported { id: String },

    #[error("I/O error at `{path}`: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

/// Windows file mutation executor.
pub struct WindowsFileExecutor<P: PayloadSource> {
    payload: P,
    work_root: PathBuf,
    tx_id: String,
    /// operation id → precondition
    preconditions: BTreeMap<String, FilePrecondition>,
    /// operation id → desired digest/size
    desired: BTreeMap<String, (Sha256Digest, u64)>,
    /// The suffix a runnable payload carries on the target this executor
    /// installs for.
    ///
    /// Read from the target rather than the host: an executor holds a plan for
    /// a target, and the two differ whenever zup is preparing another
    /// machine's installation.
    executable_suffix: &'static str,
    progress: Box<dyn ProgressSink>,
}

impl<P: PayloadSource> WindowsFileExecutor<P> {
    pub fn rollback_transaction_receipt(
        &self,
        receipt: &zup_transaction::OperationReceipt,
    ) -> Result<(), WindowsFileExecutorError> {
        use zup_transaction::OperationReceipt as Receipt;
        match receipt {
            Receipt::StageFile { staged_path, .. } => {
                let path = Path::new(staged_path);
                if path.exists() {
                    fs::remove_file(path).map_err(|source| WindowsFileExecutorError::Io {
                        path: staged_path.clone(),
                        source,
                    })?;
                }
                Ok(())
            }
            Receipt::CreateFile {
                destination,
                installed_sha256,
                installed_size,
                created_directories,
                ..
            } => {
                let path = Path::new(destination);
                let expected = installed_sha256.parse::<Sha256Digest>().map_err(|_| {
                    WindowsFileExecutorError::RollbackDrift {
                        path: destination.clone(),
                        reason: "invalid journal digest".into(),
                    }
                })?;
                if file_identity(path)? != Some((*installed_size, expected)) {
                    return Err(WindowsFileExecutorError::RollbackDrift {
                        path: destination.clone(),
                        reason: "installed file changed".into(),
                    });
                }
                fs::remove_file(path).map_err(|source| WindowsFileExecutorError::Io {
                    path: destination.clone(),
                    source,
                })?;
                for dir in created_directories.iter().rev() {
                    let _ = fs::remove_dir(dir);
                }
                Ok(())
            }
            Receipt::ReplaceFile {
                destination,
                backup_path,
                new_sha256,
                new_size,
                previous_sha256,
                previous_size,
                ..
            } => {
                let path = Path::new(destination);
                let new_hash = new_sha256.parse::<Sha256Digest>().map_err(|_| {
                    WindowsFileExecutorError::RollbackDrift {
                        path: destination.clone(),
                        reason: "invalid journal digest".into(),
                    }
                })?;
                let old_hash = previous_sha256.parse::<Sha256Digest>().map_err(|_| {
                    WindowsFileExecutorError::RollbackDrift {
                        path: backup_path.clone(),
                        reason: "invalid journal digest".into(),
                    }
                })?;
                if file_identity(path)? != Some((*new_size, new_hash))
                    || file_identity(Path::new(backup_path))? != Some((*previous_size, old_hash))
                {
                    return Err(WindowsFileExecutorError::RollbackDrift {
                        path: destination.clone(),
                        reason: "installed file or backup changed".into(),
                    });
                }
                move_durable(Path::new(backup_path), path)?;
                Ok(())
            }
            Receipt::RemoveFile {
                destination,
                backup_path,
                sha256,
                size,
            } => {
                if file_identity(&host_path(destination))?.is_some()
                    || file_identity(&host_path(backup_path))? != Some((*size, *sha256))
                {
                    return Err(WindowsFileExecutorError::RollbackDrift {
                        path: destination.to_string(),
                        reason: "removed file or backup changed".into(),
                    });
                }
                move_durable(&host_path(backup_path), &host_path(destination))?;
                Ok(())
            }
            _ => Err(WindowsFileExecutorError::Unsupported {
                id: "receipt".into(),
            }),
        }
    }

    pub fn reconcile_transaction_node(
        &self,
        node: &TransactionNode,
    ) -> Result<ReconcileResult, WindowsFileExecutorError> {
        let expected = match (node.meta.expected_size, node.meta.expected_sha256) {
            (Some(size), Some(hash)) => (size, hash),
            _ => {
                return Err(WindowsFileExecutorError::Unsupported {
                    id: node.id.to_string(),
                });
            }
        };
        let key = match &node.kind {
            NodeKind::StageFile { key } | NodeKind::FileMutation { key, .. } => key,
            _ => {
                return Err(WindowsFileExecutorError::Unsupported {
                    id: node.id.to_string(),
                });
            }
        };
        let destination = match key {
            ResourceKey::File { destination } | ResourceKey::Maintenance { destination, .. } => {
                destination
            }
            _ => {
                return Err(WindowsFileExecutorError::Unsupported {
                    id: node.id.to_string(),
                });
            }
        };
        let destination = Path::new(destination);
        match &node.kind {
            NodeKind::StageFile { .. } => {
                let volume = crate::durable::volume_root(destination)?;
                let staged = self.staged_path_for_key(key, &volume);
                match file_identity(&staged)? {
                    None => Ok(ReconcileResult::NotApplied),
                    Some(found) if found == expected => Ok(ReconcileResult::AppliedWithReceipt(
                        zup_transaction::OperationReceipt::StageFile {
                            staged_path: staged.display().to_string(),
                            size: expected.0,
                            sha256: expected.1.to_hex(),
                        },
                    )),
                    _ => Ok(ReconcileResult::Ambiguous),
                }
            }
            NodeKind::FileMutation {
                delta: FileDelta::Create | FileDelta::RestoreOwned,
                ..
            } => match file_identity(destination)? {
                None => Ok(ReconcileResult::NotApplied),
                Some(found) if found == expected => Ok(ReconcileResult::AppliedWithReceipt(
                    zup_transaction::OperationReceipt::CreateFile {
                        destination: destination.display().to_string(),
                        installed_sha256: expected.1.to_hex(),
                        installed_size: expected.0,
                        executable: node.meta.executable.unwrap_or(false),
                        created_directories: Vec::new(),
                    },
                )),
                _ => Ok(ReconcileResult::Ambiguous),
            },
            NodeKind::FileMutation {
                delta: FileDelta::Replace | FileDelta::RepairOwned,
                ..
            } => {
                let FilePrecondition::Exact { size, sha256 } = node
                    .meta
                    .file_precondition
                    .ok_or_else(|| WindowsFileExecutorError::Unsupported {
                        id: node.id.to_string(),
                    })?
                else {
                    return Err(WindowsFileExecutorError::Unsupported {
                        id: node.id.to_string(),
                    });
                };
                match file_identity(destination)? {
                    Some(found) if found == (size, sha256) => Ok(ReconcileResult::NotApplied),
                    Some(found) if found == expected => {
                        let backup = self.backup_path_for_key(key);
                        if file_identity(&backup)? != Some((size, sha256)) {
                            return Ok(ReconcileResult::Ambiguous);
                        }
                        Ok(ReconcileResult::AppliedWithReceipt(
                            zup_transaction::OperationReceipt::ReplaceFile {
                                destination: destination.display().to_string(),
                                previous_sha256: sha256.to_hex(),
                                previous_size: size,
                                backup_path: backup.display().to_string(),
                                new_sha256: expected.1.to_hex(),
                                new_size: expected.0,
                                executable: node.meta.executable.unwrap_or(false),
                            },
                        ))
                    }
                    _ => Ok(ReconcileResult::Ambiguous),
                }
            }
            _ => Err(WindowsFileExecutorError::Unsupported {
                id: node.id.to_string(),
            }),
        }
    }
    pub fn new(
        payload: P,
        work_root: PathBuf,
        tx_id: String,
        executable_suffix: &'static str,
        progress: Box<dyn ProgressSink>,
    ) -> Self {
        Self {
            payload,
            work_root,
            tx_id,
            preconditions: BTreeMap::new(),
            desired: BTreeMap::new(),
            executable_suffix,
            progress,
        }
    }

    pub fn register_file_op(
        &mut self,
        op_id: &OperationId,
        precondition: FilePrecondition,
        sha256: Sha256Digest,
        size: u64,
    ) {
        self.preconditions
            .insert(op_id.as_str().to_owned(), precondition);
        self.desired
            .insert(op_id.as_str().to_owned(), (sha256, size));
    }

    fn backup_dir(&self) -> PathBuf {
        self.work_root.join(&self.tx_id).join("backup")
    }

    fn staged_path_for_key(&self, key: &ResourceKey, volume: &Path) -> PathBuf {
        let vol_name = volume
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("root");
        self.work_root
            .join(&self.tx_id)
            .join("staging")
            .join(sanitize(vol_name))
            .join(format!("{}.payload", resource_token(key)))
    }

    fn backup_path_for_key(&self, key: &ResourceKey) -> PathBuf {
        self.backup_dir()
            .join(format!("{}.bak", resource_token(key)))
    }

    fn removal_backup(&self, destination: &Path) -> PathBuf {
        let mut name = std::ffi::OsString::from(".");
        name.push(destination.file_name().unwrap_or_default());
        name.push(format!(".zup-{}.bak", self.tx_id));
        destination.with_file_name(name)
    }

    pub fn apply_owned_file_removal(
        &self,
        node: &TransactionNode,
    ) -> Result<zup_transaction::OperationReceipt, WindowsFileExecutorError> {
        let Some(removal) = &node.meta.removal else {
            return Err(WindowsFileExecutorError::Unsupported {
                id: node.id.to_string(),
            });
        };
        let destination_path = host_path(&removal.destination);
        if file_identity(&destination_path)? != Some((removal.size, removal.sha256)) {
            return Err(WindowsFileExecutorError::PlanDrift {
                path: removal.destination.to_string(),
                reason: "owned file changed before removal".into(),
            });
        }
        let backup = self.removal_backup(&destination_path);
        if fs::symlink_metadata(&backup).is_ok() {
            return Err(WindowsFileExecutorError::PlanDrift {
                path: backup.display().to_string(),
                reason: "removal backup path is occupied".into(),
            });
        }
        move_durable(&destination_path, &backup)?;
        Ok(zup_transaction::OperationReceipt::RemoveFile {
            destination: removal.destination.clone(),
            backup_path: target_path_from_host(&backup, removal.destination.target()).map_err(
                |_| WindowsFileExecutorError::Unsupported {
                    id: node.id.to_string(),
                },
            )?,
            sha256: removal.sha256,
            size: removal.size,
        })
    }

    pub fn reconcile_owned_file_removal(
        &self,
        node: &TransactionNode,
    ) -> Result<ReconcileResult, WindowsFileExecutorError> {
        let Some(removal) = &node.meta.removal else {
            return Err(WindowsFileExecutorError::Unsupported {
                id: node.id.to_string(),
            });
        };
        let destination_path = host_path(&removal.destination);
        let backup = self.removal_backup(&destination_path);
        let expected = Some((removal.size, removal.sha256));
        match (file_identity(&destination_path)?, file_identity(&backup)?) {
            (Some(current), None) if Some(current) == expected => Ok(ReconcileResult::NotApplied),
            (None, Some(saved)) if Some(saved) == expected => {
                Ok(ReconcileResult::AppliedWithReceipt(
                    zup_transaction::OperationReceipt::RemoveFile {
                        destination: removal.destination.clone(),
                        backup_path: target_path_from_host(&backup, removal.destination.target())
                            .map_err(|_| WindowsFileExecutorError::Unsupported {
                            id: node.id.to_string(),
                        })?,
                        sha256: removal.sha256,
                        size: removal.size,
                    },
                ))
            }
            _ => Ok(ReconcileResult::Ambiguous),
        }
    }

    fn verify_precondition(
        &self,
        path: &Path,
        precondition: &FilePrecondition,
    ) -> Result<(), WindowsFileExecutorError> {
        let display = path.display().to_string();
        match precondition {
            FilePrecondition::Absent => {
                let meta = match fs::symlink_metadata(path) {
                    Ok(m) => m,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                    Err(source) => {
                        return Err(WindowsFileExecutorError::Io {
                            path: display,
                            source,
                        });
                    }
                };
                if meta.file_type().is_symlink() {
                    return Err(WindowsFileExecutorError::PlanDrift {
                        path: display,
                        reason: "target became a symlink".into(),
                    });
                }
                Err(WindowsFileExecutorError::PlanDrift {
                    path: display,
                    reason: "target is no longer absent".into(),
                })
            }
            FilePrecondition::Exact { size, sha256 } => {
                let meta = match fs::symlink_metadata(path) {
                    Ok(m) => m,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        return Err(WindowsFileExecutorError::PlanDrift {
                            path: display,
                            reason: "target vanished".into(),
                        });
                    }
                    Err(source) => {
                        return Err(WindowsFileExecutorError::Io {
                            path: display,
                            source,
                        });
                    }
                };
                if meta.file_type().is_symlink() || !meta.file_type().is_file() {
                    return Err(WindowsFileExecutorError::PlanDrift {
                        path: display,
                        reason: "target is not a regular file".into(),
                    });
                }
                let file = fs::File::open(path).map_err(|source| WindowsFileExecutorError::Io {
                    path: display.clone(),
                    source,
                })?;
                let (found_size, found_hash) =
                    hash_reader(file).map_err(|source| WindowsFileExecutorError::Io {
                        path: display.clone(),
                        source,
                    })?;
                if found_size != *size || found_hash != *sha256 {
                    return Err(WindowsFileExecutorError::PlanDrift {
                        path: display,
                        reason: "target content changed since snapshot".into(),
                    });
                }
                Ok(())
            }
        }
    }

    fn stage_payload(
        &mut self,
        op: &TransactionNode,
        source_relative: &zup_core::RelativePath,
        dest: &Path,
        sha256: Sha256Digest,
        size: u64,
        key: &ResourceKey,
    ) -> Result<OperationReceipt, WindowsFileExecutorError> {
        self.progress.on_event(FileProgress::StagingFile {
            id: op.id.to_string(),
        });
        let volume = crate::durable::volume_root(dest)?;
        let staged = self.staged_path_for_key(key, &volume);
        if let Some(parent) = staged.parent() {
            fs::create_dir_all(parent).map_err(|source| WindowsFileExecutorError::Io {
                path: parent.display().to_string(),
                source,
            })?;
        }

        let mut reader = self.payload.open(source_relative, &sha256, size)?;
        let mut out = fs::File::create(&staged).map_err(|source| WindowsFileExecutorError::Io {
            path: staged.display().to_string(),
            source,
        })?;
        let mut buf = vec![0u8; 64 * 1024];
        let mut written = 0u64;
        let mut hasher = sha2::Sha256::new();
        loop {
            let n = reader.read(&mut buf).map_err(|source| {
                WindowsFileExecutorError::Staging(format!("payload read failed: {source}"))
            })?;
            if n == 0 {
                break;
            }
            out.write_all(&buf[..n])
                .map_err(|source| WindowsFileExecutorError::Io {
                    path: staged.display().to_string(),
                    source,
                })?;
            hasher.update(&buf[..n]);
            written = written.saturating_add(n as u64);
        }
        // Flushed, not just written. A staged payload is what a later commit
        // barrier moves into the install location, and a power cut between the
        // write and the rename would otherwise leave a destination holding bytes
        // that were never on the medium - an installed file that hashes to
        // nothing anybody can reproduce. The size and digest are checked right
        // after, so the file is also known to be complete before it is published.
        out.sync_all().map_err(|source| {
            let _ = fs::remove_file(&staged);
            WindowsFileExecutorError::Io {
                path: staged.display().to_string(),
                source,
            }
        })?;
        drop(out);

        if written != size {
            let _ = fs::remove_file(&staged);
            return Err(WindowsFileExecutorError::Staging(format!(
                "size mismatch: expected {size}, wrote {written}"
            )));
        }
        let digest = Sha256Digest::from_hasher(hasher);
        if digest != sha256 {
            let _ = fs::remove_file(&staged);
            return Err(WindowsFileExecutorError::Staging(
                "digest mismatch while staging".into(),
            ));
        }

        Ok(OperationReceipt::StageFile(StageFileReceipt {
            staged_path: staged.display().to_string(),
            size,
            sha256: digest,
        }))
    }

    fn create_file(
        &mut self,
        op: &TransactionNode,
        dest: &Path,
        sha256: Sha256Digest,
        size: u64,
        key: &ResourceKey,
    ) -> Result<OperationReceipt, WindowsFileExecutorError> {
        self.progress.on_event(FileProgress::FileCreateStarted {
            id: op.id.to_string(),
        });
        self.verify_precondition(dest, &FilePrecondition::Absent)?;
        // Refused here rather than after the write: an unrunnable file published
        // and reported as installed is worse than a refused transaction.
        let executable = assert_runnable(
            dest,
            op.meta.executable.unwrap_or(false),
            self.executable_suffix,
        )?;

        // Create missing parents deliberately (rollback candidates).
        let mut created_dirs = Vec::new();
        if let Some(parent) = dest.parent() {
            let mut missing = Vec::new();
            let mut cur = parent.to_path_buf();
            while !cur.as_os_str().is_empty() && !cur.exists() {
                missing.push(cur.clone());
                if !cur.pop() {
                    break;
                }
            }
            missing.reverse();
            for dir in &missing {
                fs::create_dir_all(dir).map_err(|source| WindowsFileExecutorError::Io {
                    path: dir.display().to_string(),
                    source,
                })?;
                created_dirs.push(dir.display().to_string());
            }
        }

        let volume = crate::durable::volume_root(dest)?;
        let staged = self.staged_path_for_key(key, &volume);
        // Re-check staged integrity before publish.
        let file = fs::File::open(&staged).map_err(|source| WindowsFileExecutorError::Io {
            path: staged.display().to_string(),
            source,
        })?;
        let (s_size, s_hash) =
            hash_reader(file).map_err(|source| WindowsFileExecutorError::Io {
                path: staged.display().to_string(),
                source,
            })?;
        if s_size != size || s_hash != sha256 {
            return Err(WindowsFileExecutorError::Staging(
                "staged payload corrupt before publish".into(),
            ));
        }

        // Create-only publish: refuse if destination appeared.
        if dest.symlink_metadata().is_ok() {
            return Err(WindowsFileExecutorError::PlanDrift {
                path: dest.display().to_string(),
                reason: "destination appeared before publish".into(),
            });
        }
        move_durable(&staged, dest)?;

        // Verify final file.
        let file = fs::File::open(dest).map_err(|source| WindowsFileExecutorError::Io {
            path: dest.display().to_string(),
            source,
        })?;
        let (f_size, f_hash) =
            hash_reader(file).map_err(|source| WindowsFileExecutorError::Io {
                path: dest.display().to_string(),
                source,
            })?;
        if f_size != size || f_hash != sha256 {
            return Err(WindowsFileExecutorError::Verification {
                path: dest.display().to_string(),
                reason: "published file does not match desired digest".into(),
            });
        }

        Ok(OperationReceipt::CreateFile(CreateFileReceipt {
            destination: dest.display().to_string(),
            installed_sha256: sha256,
            installed_size: size,
            executable,
            created_directories: created_dirs,
        }))
    }

    fn replace_file(
        &mut self,
        op: &TransactionNode,
        dest: &Path,
        sha256: Sha256Digest,
        size: u64,
        precondition: &FilePrecondition,
        key: &ResourceKey,
    ) -> Result<OperationReceipt, WindowsFileExecutorError> {
        self.progress.on_event(FileProgress::FileReplaceStarted {
            id: op.id.to_string(),
        });
        self.verify_precondition(dest, precondition)?;
        let executable = assert_runnable(
            dest,
            op.meta.executable.unwrap_or(false),
            self.executable_suffix,
        )?;
        let FilePrecondition::Exact {
            size: prev_size,
            sha256: prev_hash,
        } = precondition
        else {
            return Err(WindowsFileExecutorError::PlanDrift {
                path: dest.display().to_string(),
                reason: "replace requires exact previous state".into(),
            });
        };

        // Backup original on the same volume.
        let backup = self.backup_path_for_key(key);
        if let Some(parent) = backup.parent() {
            fs::create_dir_all(parent).map_err(|source| WindowsFileExecutorError::Io {
                path: parent.display().to_string(),
                source,
            })?;
        }
        // The backup is the *only* copy of what was there: rollback restores it,
        // and if the machine loses power after the new file is published, this is
        // what decides whether the installation can go back. A plain `fs::copy`
        // leaves that copy in the write cache, so it is flushed before anything
        // is replaced.
        crate::durable::copy_new_durable(dest, &backup).map_err(|source| {
            WindowsFileExecutorError::Io {
                path: backup.display().to_string(),
                source: match source {
                    crate::durable::DurableError::Io { source, .. } => source,
                    other => std::io::Error::other(other.to_string()),
                },
            }
        })?;

        let volume = crate::durable::volume_root(dest)?;
        let staged = self.staged_path_for_key(key, &volume);
        move_durable(&staged, dest)?;

        let file = fs::File::open(dest).map_err(|source| WindowsFileExecutorError::Io {
            path: dest.display().to_string(),
            source,
        })?;
        let (f_size, f_hash) =
            hash_reader(file).map_err(|source| WindowsFileExecutorError::Io {
                path: dest.display().to_string(),
                source,
            })?;
        if f_size != size || f_hash != sha256 {
            return Err(WindowsFileExecutorError::Verification {
                path: dest.display().to_string(),
                reason: "replaced file does not match desired digest".into(),
            });
        }

        Ok(OperationReceipt::ReplaceFile(ReplaceFileReceipt {
            destination: dest.display().to_string(),
            previous_sha256: *prev_hash,
            previous_size: *prev_size,
            backup_path: backup.display().to_string(),
            new_sha256: sha256,
            new_size: size,
            executable,
        }))
    }

    /// Register file operations from a compiled plan's file nodes.
    pub fn note_file(
        &mut self,
        op_id: &OperationId,
        precondition: FilePrecondition,
        sha256: Sha256Digest,
        size: u64,
    ) {
        self.register_file_op(op_id, precondition, sha256, size);
    }
}

fn file_identity(path: &Path) -> Result<Option<(u64, Sha256Digest)>, WindowsFileExecutorError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(WindowsFileExecutorError::Io {
                path: path.display().to_string(),
                source,
            });
        }
    };
    if !metadata.file_type().is_file() {
        return Ok(None);
    }
    let file = fs::File::open(path).map_err(|source| WindowsFileExecutorError::Io {
        path: path.display().to_string(),
        source,
    })?;
    hash_reader(file)
        .map(Some)
        .map_err(|source| WindowsFileExecutorError::Io {
            path: path.display().to_string(),
            source,
        })
}

/// Apply one transaction node (file kinds only).
pub fn apply_node<P: PayloadSource>(
    exec: &mut WindowsFileExecutor<P>,
    op: &TransactionNode,
    source_relative: &zup_core::RelativePath,
    dest: &Path,
) -> Result<OperationReceipt, WindowsFileExecutorError> {
    match &op.kind {
        NodeKind::Barrier => Ok(OperationReceipt::Control),
        NodeKind::StageFile { key } => {
            let (sha256, size) = exec.desired.get(op.id.as_str()).copied().ok_or_else(|| {
                WindowsFileExecutorError::Unsupported {
                    id: op.id.to_string(),
                }
            })?;
            exec.stage_payload(op, source_relative, dest, sha256, size, key)
        }
        NodeKind::FileMutation { delta, key, .. } => {
            let (sha256, size) = exec.desired.get(op.id.as_str()).copied().ok_or_else(|| {
                WindowsFileExecutorError::Unsupported {
                    id: op.id.to_string(),
                }
            })?;
            let precondition = exec
                .preconditions
                .get(op.id.as_str())
                .cloned()
                .unwrap_or(FilePrecondition::Absent);
            match delta {
                FileDelta::Create | FileDelta::RestoreOwned => {
                    exec.create_file(op, dest, sha256, size, key)
                }
                FileDelta::Replace | FileDelta::RepairOwned => {
                    exec.replace_file(op, dest, sha256, size, &precondition, key)
                }
                _ => Err(WindowsFileExecutorError::Unsupported {
                    id: op.id.to_string(),
                }),
            }
        }
        NodeKind::FileRemoval { .. }
        | NodeKind::BackendOperation { .. }
        | NodeKind::BackendRemoval { .. } => Err(WindowsFileExecutorError::Unsupported {
            id: op.id.to_string(),
        }),
    }
}

/// Whether a payload path is executable on Windows.
///
/// Windows has no execute bit: a file is runnable because of what it *is*, not
/// because of a permission. So the intent is not applied here - there is nothing
/// to apply - but it is not ignored either. A payload declared executable that is
/// not a Windows image would install as a file nothing can run, which is exactly
/// the outcome the manifest said should not happen, and the only honest response
/// to that is to refuse before the file is written rather than publish it and
/// report success.
///
/// The reverse is not a failure: Windows legitimately needs no mode change for the
/// same intent that Linux lowers into a permission bit. The manifest's portable
/// claim is "this file should be runnable", and a PE image at that path satisfies
/// it with no filesystem state at all.
fn assert_runnable(
    dest: &Path,
    executable: bool,
    suffix: &str,
) -> Result<bool, WindowsFileExecutorError> {
    if !executable {
        return Ok(false);
    }
    if !suffix.is_empty()
        && !dest
            .to_string_lossy()
            .to_lowercase()
            .ends_with(&suffix.to_lowercase())
    {
        return Err(WindowsFileExecutorError::Verification {
            path: dest.display().to_string(),
            reason: "declared executable but is not a Windows executable".into(),
        });
    }
    Ok(true)
}

/// Lower a Windows file receipt to the transaction journal's receipt shape.
pub fn transaction_receipt(receipt: OperationReceipt) -> zup_transaction::OperationReceipt {
    use zup_transaction::OperationReceipt as Journal;
    match receipt {
        OperationReceipt::Control => Journal::Control,
        OperationReceipt::StageFile(receipt) => Journal::StageFile {
            staged_path: receipt.staged_path,
            size: receipt.size,
            sha256: receipt.sha256.to_hex(),
        },
        OperationReceipt::CreateFile(receipt) => Journal::CreateFile {
            destination: receipt.destination,
            installed_sha256: receipt.installed_sha256.to_hex(),
            installed_size: receipt.installed_size,
            executable: receipt.executable,
            created_directories: receipt.created_directories,
        },
        OperationReceipt::ReplaceFile(receipt) => Journal::ReplaceFile {
            destination: receipt.destination,
            previous_sha256: receipt.previous_sha256.to_hex(),
            previous_size: receipt.previous_size,
            backup_path: receipt.backup_path,
            new_sha256: receipt.new_sha256.to_hex(),
            new_size: receipt.new_size,
            executable: receipt.executable,
        },
    }
}

/// Verify an applied file operation against the receipt that recorded it.
///
/// The receipt is the only durable record of what the apply installed, so the
/// installed bytes, the retained backup, and the vacated path are all read
/// back from it. A receipt kind that names no file state is not a file
/// verification, and says so rather than passing.
pub fn verify_installed_file(
    receipt: &zup_transaction::OperationReceipt,
) -> Result<(), WindowsFileExecutorError> {
    use zup_transaction::OperationReceipt as Receipt;
    match receipt {
        Receipt::StageFile {
            staged_path,
            size,
            sha256,
        } => {
            let expected = (*size, digest_of(sha256, staged_path)?);
            expect_identity(
                Path::new(staged_path),
                Some(expected),
                staged_path,
                "staged payload",
            )
        }
        Receipt::CreateFile {
            destination,
            installed_sha256,
            installed_size,
            ..
        } => {
            let expected = (*installed_size, digest_of(installed_sha256, destination)?);
            expect_identity(
                Path::new(destination),
                Some(expected),
                destination,
                "installed file",
            )
        }
        Receipt::ReplaceFile {
            destination,
            backup_path,
            new_sha256,
            new_size,
            previous_sha256,
            previous_size,
            ..
        } => {
            let installed = (*new_size, digest_of(new_sha256, destination)?);
            expect_identity(
                Path::new(destination),
                Some(installed),
                destination,
                "installed file",
            )?;
            let previous = (*previous_size, digest_of(previous_sha256, backup_path)?);
            expect_identity(
                Path::new(backup_path),
                Some(previous),
                backup_path,
                "replaced file backup",
            )
        }
        Receipt::RemoveFile {
            destination,
            backup_path,
            sha256,
            size,
        } => {
            let destination = host_path(destination);
            let backup_path = host_path(backup_path);
            let removed = (*size, *sha256);
            expect_identity(
                &destination,
                None,
                &destination.display().to_string(),
                "removed file",
            )?;
            expect_identity(
                &backup_path,
                Some(removed),
                &backup_path.display().to_string(),
                "removal backup",
            )
        }
        Receipt::Control | Receipt::Backend { .. } => Err(WindowsFileExecutorError::Unsupported {
            id: "receipt".into(),
        }),
    }
}

fn digest_of(value: &str, path: &str) -> Result<Sha256Digest, WindowsFileExecutorError> {
    value
        .parse::<Sha256Digest>()
        .map_err(|_| WindowsFileExecutorError::Verification {
            path: path.to_owned(),
            reason: "invalid journal digest".into(),
        })
}

fn expect_identity(
    path: &Path,
    expected: Option<(u64, Sha256Digest)>,
    display: &str,
    what: &str,
) -> Result<(), WindowsFileExecutorError> {
    match (file_identity(path)?, expected) {
        (found, Some((size, digest))) if found == Some((size, digest)) => Ok(()),
        (Some(_), _) => Err(WindowsFileExecutorError::Verification {
            path: display.to_owned(),
            reason: format!("{what} no longer matches its receipt"),
        }),
        (None, None) => Ok(()),
        (None, Some(_)) => Err(WindowsFileExecutorError::Verification {
            path: display.to_owned(),
            reason: format!("{what} is missing"),
        }),
    }
}

/// Reconcile a `Running` file node.
pub fn reconcile_node(
    _op: &TransactionNode,
    dest: &Path,
    desired: (Sha256Digest, u64),
) -> ReconcileResult {
    match fs::symlink_metadata(dest) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => ReconcileResult::NotApplied,
        Err(_) => ReconcileResult::Ambiguous,
        Ok(meta) => {
            if meta.file_type().is_symlink() || !meta.file_type().is_file() {
                return ReconcileResult::Ambiguous;
            }
            let Ok(file) = fs::File::open(dest) else {
                return ReconcileResult::Ambiguous;
            };
            let Ok((size, hash)) = hash_reader(file) else {
                return ReconcileResult::Ambiguous;
            };
            if size == desired.1 && hash == desired.0 {
                ReconcileResult::Applied
            } else {
                ReconcileResult::Ambiguous
            }
        }
    }
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn resource_token(key: &ResourceKey) -> String {
    let digest = sha2::Sha256::digest(format!("{key:?}").as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}
