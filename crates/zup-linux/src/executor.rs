//! The Linux file transaction executor.
//!
//! This implements the existing portable [`OperationExecutor`] contract with Linux
//! filesystem semantics. It is not the Windows executor behind a conditional, and
//! it is not a copy of one: the operations, their preconditions and their
//! durability are expressed with the primitives this platform actually has.
//!
//! # What is different, and why
//!
//! Three things genuinely differ, and each is a capability rather than a
//! translation.
//!
//! **A create is refused by the kernel.** `FileDelta::Create` carries a precondition
//! of `Absent`, and on Linux that condition can be a condition of the namespace
//! operation itself with `RENAME_NOREPLACE`. There is no check-then-write window in
//! which a concurrent process claims the name, and no fallback to one: a fallback
//! that quietly became race-prone would be a lie about a guarantee the
//! transaction's recovery semantics depend on.
//!
//! **A replace publishes atomically and backs up first.** The previous contents are
//! copied to a backup and flushed *before* the new bytes are published, so a crash
//! between the two leaves a backup holding what the receipt says it holds.
//!
//! **A removal unlinks, and a running executable may unlink itself.** Linux lets a
//! process remove a directory entry that is the image it is executing. The Windows
//! uninstall runner exists because Windows does not, so there is nothing here to
//! port and nothing to wait for.
//!
//! # The seam
//!
//! An executor is registered with a *lowered host path* per operation. Path
//! resolution is not this type's business: the platform's own lowering already
//! produced the path, and re-deriving it here would mean two places that could
//! disagree about where a file goes. What lives here is the mechanism.
//!
//! Staged bytes are written into the *destination's own directory* rather than a
//! separate work root. That is what makes publication a sibling rename, and a
//! sibling rename is what makes it atomic: a rename across two filesystems is a
//! copy, and a copy has none of the guarantees this module states. The cost is a
//! staging entry inside the install directory, which is why a staged name is a
//! digest of its operation id and why an interrupted transaction leaves something
//! the journal can name and remove.
//!
//! # Receipts
//!
//! Every receipt records what was actually mutated rather than a control marker:
//! the installed identity, the backup taken, the directories created as rollback
//! candidates, and whether the file is executable. The executable bit is recorded
//! rather than assumed because the two backends satisfy the same manifest intent by
//! different means, and a receipt that omitted it would leave reconcile with
//! nothing to compare against.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use zup_bundle::{PayloadError, PayloadSource};
use zup_core::{RelativePath, ResourceKey, Sha256Digest, hash_reader};
use zup_platform::TargetPath;
use zup_transaction::{
    FileDelta, FilePrecondition, NodeKind, OperationExecutor, OperationId, OperationReceipt,
    ReconcileResult, TransactionNode, TransactionPlan,
};

use crate::fs::{
    EXECUTABLE_PAYLOAD_MODE, EntryKind, FileSystemError, OwnedDirectory, PAYLOAD_FILE_MODE,
    STATE_DIRECTORY_MODE, STATE_FILE_MODE,
};
use crate::lowering::to_host_path;

/// Why a Linux file operation could not be carried out.
#[derive(Debug, thiserror::Error)]
pub enum LinuxFileExecutorError {
    #[error("plan drift at `{path}`: {reason}")]
    PlanDrift { path: String, reason: String },

    #[error("verification failed at `{path}`: {reason}")]
    Verification { path: String, reason: String },

    #[error("rollback cannot restore `{path}`: {reason}")]
    RollbackDrift { path: String, reason: String },

    #[error("integration refresh failed: {0}")]
    Refresh(#[from] crate::refresh::RefreshError),

    #[error("`{path}` is not a {expected}")]
    UnexpectedKind {
        path: String,
        expected: &'static str,
    },

    #[error("`{path}` already exists")]
    AlreadyExists { path: String },

    #[error("`{path}` does not exist")]
    Missing { path: String },

    #[error("payload for `{path}` could not be staged: {source}")]
    Payload {
        path: String,
        #[source]
        source: PayloadError,
    },

    #[error("i/o at `{path}`: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

impl From<FileSystemError> for LinuxFileExecutorError {
    fn from(error: FileSystemError) -> Self {
        match error {
            FileSystemError::UnexpectedKind { path, expected } => {
                Self::UnexpectedKind { path, expected }
            }
            FileSystemError::AlreadyExists { path } => Self::AlreadyExists { path },
            FileSystemError::Missing { path } => Self::Missing { path },
            FileSystemError::Io { path, source } => Self::Io { path, source },
        }
    }
}

/// What one operation is supposed to install, known before it is applied.
///
/// Held per operation id rather than read from the node, because a recovery run
/// replays a plan whose nodes carry identity but not the lowered host path, and an
/// executor asked about a node it did not build itself must still know where that
/// node's file goes.
#[derive(Debug, Clone)]
pub struct FileWork {
    /// Where the file lands, as the target spells it. Receipts carry this rather
    /// than a host path, so a journal stays portable.
    pub destination: zup_platform::TargetPath,
    /// Where the file lands on this host, as lowering spelled it.
    pub host_path: PathBuf,
    /// What the file must hold, and whether it is meant to be runnable.
    pub intent: FileIntent,
    /// The state the destination is required to be in, if it exists at all.
    pub precondition: FilePrecondition,
    /// Bytes already written to the work directory, for the staging step.
    pub staged: Option<PathBuf>,
    /// Directories staging had to create, carried forward to the receipt.
    ///
    /// Recorded where they are *made*, not where they are noticed. Staging writes
    /// inside the destination's parents, so a publish step that looked only for
    /// itself would find them already there and report none - and an uninstall
    /// with no record of a directory has no way to remove it.
    pub created_directories: Vec<String>,
}

/// The identity and intent of one installed file.
#[derive(Debug, Clone, Copy)]
pub struct FileIntent {
    pub sha256: Sha256Digest,
    pub size: u64,
    pub executable: bool,
}

impl FileIntent {
    /// The mode this executor installs a file with: data is always `0644`,
    /// and executables follow the executor's scope policy.
    ///
    /// Additive on purpose: the executable mode adds execute bits without
    /// making anything writable that was not already, so declaring one
    /// helper executable never loosens the directory it lands in.
    fn mode_for(self, executable_mode: rustix::fs::Mode) -> rustix::fs::Mode {
        if self.executable {
            executable_mode
        } else {
            PAYLOAD_FILE_MODE
        }
    }
}

/// Carries out a Linux file transaction.
///
/// A payload source is attached separately from registration because the two
/// come from different places: the plan says *what* each operation installs,
/// and the package says *where the bytes are*. An executor with no payload
/// source can still verify, roll back, and reconcile from receipts - it just
/// cannot stage.
pub struct LinuxFileExecutor {
    files: BTreeMap<String, FileWork>,
    payload: Option<Box<dyn PayloadSource>>,
    executable_mode: rustix::fs::Mode,
}

impl Default for LinuxFileExecutor {
    fn default() -> Self {
        Self {
            files: BTreeMap::new(),
            payload: None,
            executable_mode: EXECUTABLE_PAYLOAD_MODE,
        }
    }
}

impl std::fmt::Debug for LinuxFileExecutor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LinuxFileExecutor")
            .field("files", &self.files)
            .field("has_payload", &self.payload.is_some())
            .finish()
    }
}

impl LinuxFileExecutor {
    /// An executor with nothing registered yet.
    ///
    /// No work root: staging happens in the destination's own directory so that
    /// publication is a same-filesystem rename. See the module documentation.
    pub fn new() -> Self {
        Self::default()
    }

    /// Attach the package this transaction stages its bytes from.
    pub fn with_payload(mut self, payload: impl PayloadSource + 'static) -> Self {
        self.payload = Some(Box::new(payload));
        self
    }

    /// Install executables runnable by every account, for machine scope.
    ///
    /// User scope keeps the additive `0744`: a file becomes runnable by its
    /// owner and nothing else loosens. Machine scope installs programs every
    /// account may run, so the owner's execute bit extends to group and
    /// other - `0755` - while nothing becomes writable that was not already.
    /// Data files stay `0644` in both scopes.
    pub fn for_machine(mut self) -> Self {
        self.executable_mode = rustix::fs::Mode::from_bits_truncate(0o755);
        self
    }

    /// Record where one operation's file goes and what it should hold.
    pub fn register(&mut self, id: &OperationId, file: FileWork) {
        self.files.insert(id.to_string(), file);
    }

    /// Record every file operation in a compiled transaction plan.
    ///
    /// The destinations come from the nodes' own keys lowered for this host,
    /// and the intent and preconditions from the nodes' metadata - the same
    /// record a recovery run replays. A plan whose nodes name a destination the
    /// target cannot spell, or omit the identity a file operation needs, is
    /// refused here rather than halfway through the transaction.
    pub fn register_plan(&mut self, plan: &TransactionPlan) -> Result<(), LinuxFileExecutorError> {
        for node in &plan.nodes {
            let key = match &node.kind {
                NodeKind::StageFile { key }
                | NodeKind::FileMutation { key, .. }
                | NodeKind::FileRemoval { key } => key,
                _ => continue,
            };
            let destination_text = match key {
                ResourceKey::File { destination } => destination,
                ResourceKey::Maintenance { destination, .. } => destination,
                other => {
                    return Err(LinuxFileExecutorError::PlanDrift {
                        path: node.id.to_string(),
                        reason: format!("{other:?} is not a file resource"),
                    });
                }
            };
            let destination =
                TargetPath::new(plan.target.clone(), destination_text).map_err(|error| {
                    LinuxFileExecutorError::PlanDrift {
                        path: destination_text.clone(),
                        reason: error.to_string(),
                    }
                })?;
            let host_path =
                to_host_path(&destination).map_err(|error| LinuxFileExecutorError::PlanDrift {
                    path: destination_text.clone(),
                    reason: error.to_string(),
                })?;
            // Removal nodes carry no payload identity: there is nothing to
            // install, so there is nothing to describe. The placeholder is
            // never read on that path, and inventing a digest for it would be
            // worse than admitting it is absent.
            let removal = matches!(node.kind, NodeKind::FileRemoval { .. });
            let intent = if removal {
                FileIntent {
                    sha256: Sha256Digest::from_bytes([0; 32]),
                    size: 0,
                    executable: false,
                }
            } else {
                let (Some(sha256), Some(size)) =
                    (node.meta.expected_sha256, node.meta.expected_size)
                else {
                    return Err(LinuxFileExecutorError::PlanDrift {
                        path: node.id.to_string(),
                        reason: "a file operation without payload identity".into(),
                    });
                };
                FileIntent {
                    sha256,
                    size,
                    executable: node.meta.executable.unwrap_or(false),
                }
            };
            self.register(
                &node.id,
                FileWork {
                    destination,
                    host_path,
                    intent,
                    precondition: node
                        .meta
                        .file_precondition
                        .unwrap_or(FilePrecondition::Absent),
                    staged: None,
                    created_directories: Vec::new(),
                },
            );
        }
        Ok(())
    }

    /// One operation's recorded work.
    pub fn file(&self, id: &str) -> Result<&FileWork, LinuxFileExecutorError> {
        self.files
            .get(id)
            .ok_or_else(|| LinuxFileExecutorError::PlanDrift {
                path: id.to_owned(),
                reason: "no registered file for this operation".into(),
            })
    }

    /// Write an operation's payload beside its destination.
    ///
    /// Separate from the mutation that publishes it, because the plan's
    /// `StageFile` node is where a transaction decides to touch the disk at all.
    pub fn stage(
        &mut self,
        id: &OperationId,
        bytes: &[u8],
    ) -> Result<OperationReceipt, LinuxFileExecutorError> {
        let file = self.file(id.as_str())?.clone();
        let name = staged_name(id);
        let created_directories = create_parents(&file.host_path)?;
        let directory = OwnedDirectory::open(&parent_of(&file.host_path)?)?;
        directory.write_durable(&name, bytes, STATE_FILE_MODE)?;
        let staged = directory.path().join(&name);
        // The staging node and the mutation node that publishes its bytes are
        // different operations over the same destination, so the staged path
        // is shared by destination rather than by operation id. A mutation
        // that looked only at its own record would find nothing staged and
        // refuse work whose bytes are sitting beside its destination.
        let destination = file.destination.to_string();
        for recorded in self.files.values_mut() {
            if recorded.destination.to_string() == destination {
                recorded.staged = Some(staged.clone());
                recorded.created_directories = created_directories.clone();
            }
        }
        Ok(OperationReceipt::StageFile {
            staged_path: staged.display().to_string(),
            size: file.intent.size,
            sha256: file.intent.sha256.to_hex(),
        })
    }

    /// Check one node before the transaction mutates anything.
    ///
    /// Observe-only: a prepare that changed state would defeat the barrier it
    /// guards. A node whose precondition already fails is refused here rather
    /// than halfway through the transaction, which is the entire purpose of a
    /// preflight.
    pub fn prepare(&mut self, node: &TransactionNode) -> Result<(), LinuxFileExecutorError> {
        match &node.kind {
            NodeKind::Barrier => Ok(()),
            NodeKind::StageFile { .. } => {
                let _ = self.file(node.id.as_str())?;
                Self::payload_identity(node)?;
                Ok(())
            }
            NodeKind::FileMutation { .. } => {
                let file = self.file(node.id.as_str())?.clone();
                match file.precondition {
                    FilePrecondition::Absent => match observe(&file.host_path)? {
                        None => Ok(()),
                        Some(_) => Err(LinuxFileExecutorError::PlanDrift {
                            path: file.host_path.display().to_string(),
                            reason: "the destination a create requires to be absent is present"
                                .into(),
                        }),
                    },
                    FilePrecondition::Exact { size, sha256 } => {
                        verify_precondition(&file.host_path, size, sha256)
                    }
                }
            }
            NodeKind::FileRemoval { .. } => {
                let file = self.file(node.id.as_str())?.clone();
                match observe(&file.host_path)? {
                    None => Err(LinuxFileExecutorError::PlanDrift {
                        path: file.host_path.display().to_string(),
                        reason: "there is nothing to remove".into(),
                    }),
                    Some(_) => Ok(()),
                }
            }
            NodeKind::BackendOperation { .. } => {
                // The one backend operation this executor answers: regenerating
                // a derived freedesktop database. The tool must resolve before
                // anything mutates, and only transactions carrying integration
                // sources hold such an operation.
                let request = refresh_request(node)?;
                crate::refresh::preflight(&request)?;
                Ok(())
            }
            NodeKind::BackendRemoval { .. } => Err(LinuxFileExecutorError::PlanDrift {
                path: node.id.to_string(),
                reason: "a backend removal is not a Linux operation".into(),
            }),
        }
    }

    /// Stage one node from the attached payload source.
    ///
    /// The coordinator drives every node through `apply`, so staging-bytes has
    /// to happen here rather than only through the lower-level [`Self::stage`]
    /// a caller drives by hand. The identity comes from the node's own
    /// metadata - the same record a recovery run replays - and the bytes are
    /// verified by the payload source on open, so what is staged is what the
    /// plan asked for.
    fn stage_node(
        &mut self,
        node: &TransactionNode,
    ) -> Result<OperationReceipt, LinuxFileExecutorError> {
        use std::io::Read as _;
        let _ = self.file(node.id.as_str())?;
        let (source, sha256, size) = Self::payload_identity(node)?;
        let payload = self
            .payload
            .as_ref()
            .ok_or_else(|| LinuxFileExecutorError::PlanDrift {
                path: node.id.to_string(),
                reason: "no payload source is attached".into(),
            })?;
        let mut reader = payload.open(&source, &sha256, size).map_err(|error| {
            LinuxFileExecutorError::Payload {
                path: source.to_string(),
                source: error,
            }
        })?;
        let mut bytes = Vec::new();
        reader
            .read_to_end(&mut bytes)
            .map_err(|error| LinuxFileExecutorError::Payload {
                path: source.to_string(),
                source: PayloadError::Read {
                    path: source.to_string(),
                    source: error,
                },
            })?;
        self.stage(&node.id, &bytes)
    }

    /// What a staging node must carry: where its bytes live and what they are.
    fn payload_identity(
        node: &TransactionNode,
    ) -> Result<(RelativePath, Sha256Digest, u64), LinuxFileExecutorError> {
        let missing = || LinuxFileExecutorError::PlanDrift {
            path: node.id.to_string(),
            reason: "a staging node without payload identity".into(),
        };
        Ok((
            node.meta.source_relative.clone().ok_or_else(missing)?,
            node.meta.expected_sha256.ok_or_else(missing)?,
            node.meta.expected_size.ok_or_else(missing)?,
        ))
    }

    /// Apply one transaction node.
    pub fn apply(
        &mut self,
        node: &TransactionNode,
    ) -> Result<OperationReceipt, LinuxFileExecutorError> {
        match &node.kind {
            NodeKind::Barrier => Ok(OperationReceipt::Control),
            NodeKind::StageFile { .. } => self.stage_node(node),
            NodeKind::FileMutation { delta, .. } => match delta {
                FileDelta::Create | FileDelta::RestoreOwned => self.create(node),
                FileDelta::Replace | FileDelta::RepairOwned => self.replace(node),
                FileDelta::NoOp => Ok(OperationReceipt::Control),
                other => Err(LinuxFileExecutorError::PlanDrift {
                    path: node.id.to_string(),
                    reason: format!("{other:?} is not applied by the file executor"),
                }),
            },
            NodeKind::FileRemoval { .. } => self.remove(node),
            NodeKind::BackendOperation { .. } => {
                let request = refresh_request(node)?;
                crate::refresh::run_refresh(&request)?;
                Ok(OperationReceipt::Backend {
                    key: request.key(),
                    payload: request.encode(),
                })
            }
            NodeKind::BackendRemoval { .. } => Err(LinuxFileExecutorError::PlanDrift {
                path: node.id.to_string(),
                reason: "a backend removal is not a Linux operation".into(),
            }),
        }
    }

    /// Confirm an applied node's installed state matches its receipt.
    ///
    /// Observe-only: the transaction calls this before the commit barrier, so an
    /// operation that mutated something here would defeat the barrier.
    pub fn verify(
        &self,
        _node: &TransactionNode,
        receipt: &OperationReceipt,
    ) -> Result<(), LinuxFileExecutorError> {
        match receipt {
            OperationReceipt::CreateFile {
                destination,
                installed_sha256,
                installed_size,
                executable,
                ..
            } => {
                let expected = FileIntent {
                    sha256: installed_sha256.parse().map_err(|_| {
                        LinuxFileExecutorError::RollbackDrift {
                            path: destination.clone(),
                            reason: "invalid journal digest".into(),
                        }
                    })?,
                    size: *installed_size,
                    executable: *executable,
                };
                verify_installed(Path::new(destination), expected)
            }
            OperationReceipt::ReplaceFile {
                destination,
                new_sha256,
                new_size,
                executable,
                ..
            } => {
                let expected = FileIntent {
                    sha256: new_sha256.parse().map_err(|_| {
                        LinuxFileExecutorError::RollbackDrift {
                            path: destination.clone(),
                            reason: "invalid journal digest".into(),
                        }
                    })?,
                    size: *new_size,
                    executable: *executable,
                };
                verify_installed(Path::new(destination), expected)
            }
            OperationReceipt::RemoveFile { destination, .. } => {
                match observe(&PathBuf::from(destination.to_string()))? {
                    None => Ok(()),
                    Some(_) => Err(LinuxFileExecutorError::Verification {
                        path: destination.to_string(),
                        reason: "the file is still there".into(),
                    }),
                }
            }
            OperationReceipt::Control | OperationReceipt::StageFile { .. } => Ok(()),
            OperationReceipt::Backend { payload, .. } => {
                // A refresh regenerates derived state deterministically, so
                // verification confirms the tool still resolves and the
                // database directory still exists. The authoritative files
                // carry their own verification above.
                let request = crate::refresh::RefreshRequest::decode(payload)?;
                crate::refresh::preflight(&request)?;
                match std::fs::symlink_metadata(&request.directory) {
                    Ok(metadata) if metadata.is_dir() => Ok(()),
                    _ => Err(LinuxFileExecutorError::Verification {
                        path: request.directory.clone(),
                        reason: "the refreshed database directory is not there".into(),
                    }),
                }
            }
        }
    }

    /// Undo an applied node from its receipt.
    pub fn rollback(
        &mut self,
        _node: &TransactionNode,
        receipt: &OperationReceipt,
    ) -> Result<(), LinuxFileExecutorError> {
        match receipt {
            OperationReceipt::CreateFile {
                destination,
                installed_sha256,
                installed_size,
                created_directories,
                ..
            } => {
                let path = PathBuf::from(destination);
                // The file must be the one this transaction installed. Removing a
                // file that changed underneath would take a user's edit with it.
                let expected = FileIntent {
                    sha256: installed_sha256.parse().map_err(|_| {
                        LinuxFileExecutorError::RollbackDrift {
                            path: destination.clone(),
                            reason: "invalid journal digest".into(),
                        }
                    })?,
                    size: *installed_size,
                    executable: false,
                };
                verify_installed(&path, expected)?;
                let directory = OwnedDirectory::open(&parent_of(&path)?)?;
                directory.remove_file(&file_name(&path)?)?;
                directory.sync()?;
                // Reverse order: the deepest directory was made last, so it is
                // removed first. Each is removed only while empty, so a directory
                // that has acquired content of its own is left alone.
                for created in created_directories.iter().rev() {
                    let created = Path::new(created);
                    let Ok(name) = file_name(created) else {
                        continue;
                    };
                    let Some(parent) = created.parent() else {
                        continue;
                    };
                    if let Ok(parent) = OwnedDirectory::open(parent) {
                        let _ = parent.remove_empty_directory(&name);
                    }
                }
                Ok(())
            }
            OperationReceipt::ReplaceFile {
                destination,
                backup_path,
                ..
            } => {
                let path = PathBuf::from(destination);
                let backup = backup_path.clone();
                let directory = OwnedDirectory::open(&parent_of(&path)?)?;
                let backup_name = file_name(&backup)?;
                let previous = directory.read_regular(&backup_name)?;
                // Restored durably, so a crash during the rollback leaves the old
                // contents rather than a half-copied file.
                directory.write_durable(&file_name(&path)?, &previous, PAYLOAD_FILE_MODE)?;
                let _ = directory.remove_file(&backup_name);
                Ok(())
            }
            OperationReceipt::RemoveFile {
                backup_path,
                sha256,
                size,
                ..
            } => {
                let backup = PathBuf::from(backup_path.to_string());
                let bytes = std::fs::read(&backup).map_err(|error| LinuxFileExecutorError::Io {
                    path: backup.display().to_string(),
                    source: error,
                })?;
                let (found_size, found) =
                    hash_reader(bytes.as_slice()).map_err(|error| LinuxFileExecutorError::Io {
                        path: backup.display().to_string(),
                        source: error,
                    })?;
                if found != *sha256 || found_size != *size {
                    return Err(LinuxFileExecutorError::RollbackDrift {
                        path: backup.display().to_string(),
                        reason: "the backup no longer holds the removed file".into(),
                    });
                }
                Ok(())
            }
            OperationReceipt::Control | OperationReceipt::StageFile { .. } => Ok(()),
            OperationReceipt::Backend { .. } => {
                // Nothing to undo: a refresh owns no bytes, and re-running it
                // here would regenerate from sources the file rollbacks below
                // are about to restore. The runner sweeps once more after
                // rollback, when the authoritative state is final.
                Ok(())
            }
        }
    }

    /// Decide what a node's state is, for a transaction that crashed.
    ///
    /// Two cases. With a receipt, the destination is compared against what the
    /// receipt says was installed. Without one - a node the journal caught
    /// mid-flight - the destination is compared against the registered intent:
    /// a file that already holds exactly what the transaction would publish is
    /// applied, whatever else is either absent or ambiguous. Reconciliation
    /// never invents a receipt for work it cannot prove happened, which is why
    /// the no-receipt applied case reports [`ReconcileResult::Applied`] rather
    /// than a reconstructed record of backups that may never have been taken.
    pub fn reconcile(
        &self,
        node: &TransactionNode,
        receipt: Option<&OperationReceipt>,
    ) -> Result<ReconcileResult, LinuxFileExecutorError> {
        let Some(receipt) = receipt else {
            return self.reconcile_interrupted(node);
        };
        match receipt {
            OperationReceipt::CreateFile {
                destination,
                installed_sha256,
                installed_size,
                ..
            }
            | OperationReceipt::ReplaceFile {
                destination,
                new_sha256: installed_sha256,
                new_size: installed_size,
                ..
            } => {
                let expected = FileIntent {
                    sha256: installed_sha256.parse().map_err(|_| {
                        LinuxFileExecutorError::RollbackDrift {
                            path: destination.clone(),
                            reason: "invalid journal digest".into(),
                        }
                    })?,
                    size: *installed_size,
                    executable: false,
                };
                match observe(Path::new(destination))? {
                    None => Ok(ReconcileResult::NotApplied),
                    Some(_) if verify_installed(Path::new(destination), expected).is_ok() => {
                        Ok(ReconcileResult::AppliedWithReceipt(receipt.clone()))
                    }
                    Some(_) => Ok(ReconcileResult::Ambiguous),
                }
            }
            OperationReceipt::RemoveFile { destination, .. } => {
                match observe(Path::new(&destination.to_string()))? {
                    None => Ok(ReconcileResult::AppliedWithReceipt(receipt.clone())),
                    Some(_) => Ok(ReconcileResult::Ambiguous),
                }
            }
            OperationReceipt::Control | OperationReceipt::StageFile { .. } => {
                Ok(ReconcileResult::AppliedWithReceipt(receipt.clone()))
            }
            // A refresh is idempotent regeneration with no owned bytes: after
            // a crash it is re-run rather than reconstructed. Recovery replays
            // it from the journaled payload, which names the tool and the
            // database directory.
            OperationReceipt::Backend { .. } => Ok(ReconcileResult::NotApplied),
        }
    }

    /// Reconcile a node the journal caught mid-flight, with no receipt.
    ///
    /// A bare `Applied` cannot be journaled for a file node - the journal
    /// requires a kind-matching receipt - so an applied conclusion is reported
    /// with a reconstructed receipt instead. The reconstruction is conservative
    /// by construction: a create is only rebuilt from the installed bytes
    /// themselves, and a replace or removal additionally requires the backup to
    /// hold the bytes the receipt claims. Anything less is `Ambiguous`, which
    /// is the only honest answer when the proofs do not all agree.
    fn reconcile_interrupted(
        &self,
        node: &TransactionNode,
    ) -> Result<ReconcileResult, LinuxFileExecutorError> {
        match &node.kind {
            NodeKind::Barrier => Ok(ReconcileResult::Applied),
            // Re-staging is idempotent - it overwrites the same derived name -
            // so there is no state to recover, only work to redo.
            NodeKind::StageFile { .. } => Ok(ReconcileResult::NotApplied),
            NodeKind::FileMutation { delta, .. } => match delta {
                FileDelta::Create | FileDelta::RestoreOwned => {
                    self.reconcile_interrupted_create(node)
                }
                FileDelta::Replace | FileDelta::RepairOwned => {
                    self.reconcile_interrupted_replace(node)
                }
                FileDelta::NoOp => Ok(ReconcileResult::NotApplied),
                FileDelta::Conflict | FileDelta::Drift => Ok(ReconcileResult::Ambiguous),
            },
            NodeKind::FileRemoval { .. } => self.reconcile_interrupted_removal(node),
            // A refresh with no receipt never established anything: it is
            // re-run on recovery. A backend removal is foreign to this
            // backend and stays ambiguous.
            NodeKind::BackendOperation { .. } => Ok(ReconcileResult::NotApplied),
            NodeKind::BackendRemoval { .. } => Ok(ReconcileResult::Ambiguous),
        }
    }

    /// A create whose publish may or may not have happened.
    ///
    /// The destination holding exactly the intended bytes is the proof: a
    /// no-clobber publish either installed those bytes or refused, so matching
    /// bytes mean the publish happened. The rebuilt receipt carries no created
    /// directories - that record died with the crashed process - which is a
    /// known concession, not a silent one.
    fn reconcile_interrupted_create(
        &self,
        node: &TransactionNode,
    ) -> Result<ReconcileResult, LinuxFileExecutorError> {
        let file = self.file(node.id.as_str())?.clone();
        match observe(&file.host_path)? {
            None => Ok(ReconcileResult::NotApplied),
            Some((size, sha256)) if size == file.intent.size && sha256 == file.intent.sha256 => Ok(
                ReconcileResult::AppliedWithReceipt(OperationReceipt::CreateFile {
                    destination: file.host_path.display().to_string(),
                    installed_sha256: file.intent.sha256.to_hex(),
                    installed_size: file.intent.size,
                    executable: file.intent.executable,
                    created_directories: Vec::new(),
                }),
            ),
            Some(_) => Ok(ReconcileResult::Ambiguous),
        }
    }

    /// A replace whose publish may or may not have happened.
    ///
    /// Both halves must agree: the destination holds the new bytes *and* the
    /// backup beside it holds the previous ones. A new destination with no
    /// backup is not a completed replace - the backup is flushed before the
    /// publish, so its absence means the publish never happened or never
    /// finished proving itself.
    fn reconcile_interrupted_replace(
        &self,
        node: &TransactionNode,
    ) -> Result<ReconcileResult, LinuxFileExecutorError> {
        let file = self.file(node.id.as_str())?.clone();
        let FilePrecondition::Exact { size, sha256 } = file.precondition else {
            return Ok(ReconcileResult::Ambiguous);
        };
        let directory = OwnedDirectory::open(&parent_of(&file.host_path)?)?;
        let backup_name = format!("backup-{}.bin", short_digest(node.id.as_str()));
        let backup_ok = match directory.read_regular(&backup_name) {
            Ok(previous) => hash_reader(previous.as_slice())
                .map(|(found_size, found)| found_size == size && found == sha256)
                .unwrap_or(false),
            Err(_) => false,
        };
        match observe(&file.host_path)? {
            None => Ok(ReconcileResult::NotApplied),
            Some((found_size, found))
                if found_size == file.intent.size && found == file.intent.sha256 && backup_ok =>
            {
                Ok(ReconcileResult::AppliedWithReceipt(
                    OperationReceipt::ReplaceFile {
                        destination: file.host_path.display().to_string(),
                        previous_sha256: sha256.to_hex(),
                        previous_size: size,
                        backup_path: directory.path().join(&backup_name).display().to_string(),
                        new_sha256: file.intent.sha256.to_hex(),
                        new_size: file.intent.size,
                        executable: file.intent.executable,
                    },
                ))
            }
            Some(_) => Ok(ReconcileResult::Ambiguous),
        }
    }

    /// A removal whose unlink may or may not have happened.
    ///
    /// The backup is the proof in both directions: an absent destination with
    /// the removed bytes beside it means the unlink happened, and a present
    /// destination holding the expected bytes means it did not.
    fn reconcile_interrupted_removal(
        &self,
        node: &TransactionNode,
    ) -> Result<ReconcileResult, LinuxFileExecutorError> {
        let Some(removal) = &node.meta.removal else {
            return Ok(ReconcileResult::Ambiguous);
        };
        let file = self.file(node.id.as_str())?.clone();
        let directory = OwnedDirectory::open(&parent_of(&file.host_path)?)?;
        let backup_name = format!("removed-{}.bin", short_digest(node.id.as_str()));
        let backup_ok = match directory.read_regular(&backup_name) {
            Ok(saved) => hash_reader(saved.as_slice())
                .map(|(found_size, found)| found_size == removal.size && found == removal.sha256)
                .unwrap_or(false),
            Err(_) => false,
        };
        match observe(&file.host_path)? {
            None if backup_ok => Ok(ReconcileResult::AppliedWithReceipt(
                OperationReceipt::RemoveFile {
                    destination: removal.destination.clone(),
                    backup_path: as_target(
                        &removal.destination,
                        &directory.path().join(&backup_name),
                    )?,
                    sha256: removal.sha256,
                    size: removal.size,
                },
            )),
            None => Ok(ReconcileResult::Ambiguous),
            Some((found_size, found)) if found_size == removal.size && found == removal.sha256 => {
                Ok(ReconcileResult::NotApplied)
            }
            Some(_) => Ok(ReconcileResult::Ambiguous),
        }
    }

    /// Create the destination, proving it was absent at the moment of publication.
    ///
    /// `Absent` is enforced by the kernel at the rename, not by a check. The
    /// difference is the whole point: a check leaves a window in which a file that
    /// appeared in the meantime is overwritten by a rename that believed it was
    /// alone.
    fn create(
        &mut self,
        node: &TransactionNode,
    ) -> Result<OperationReceipt, LinuxFileExecutorError> {
        let file = self.file(node.id.as_str())?.clone();
        if file.precondition != FilePrecondition::Absent {
            return Err(LinuxFileExecutorError::PlanDrift {
                path: file.host_path.display().to_string(),
                reason: "a create must require an absent destination".into(),
            });
        }
        let staged = file
            .staged
            .clone()
            .ok_or_else(|| LinuxFileExecutorError::PlanDrift {
                path: file.host_path.display().to_string(),
                reason: "the file was never staged".into(),
            })?;

        // Whatever staging had to create is what this operation made. Anything
        // missing from that record was already there, and a directory zup cannot
        // prove it owns has to survive an uninstall.
        let created_directories = file.created_directories.clone();
        let directory = OwnedDirectory::open(&parent_of(&file.host_path)?)?;
        let name = file_name(&file.host_path)?;

        // The mode is applied to the staged file *before* the rename, because a
        // rename preserves the mode the file was given. Setting the bit afterwards
        // would leave a window in which the destination is published and not yet
        // runnable.
        rustix::fs::chmod(&staged, file.intent.mode_for(self.executable_mode)).map_err(
            |error| LinuxFileExecutorError::Io {
                path: staged.display().to_string(),
                source: std::io::Error::from(error),
            },
        )?;
        directory.publish_exclusive(&name, &file_name(&staged)?)?;

        verify_installed(&file.host_path, file.intent)?;
        Ok(OperationReceipt::CreateFile {
            destination: file.host_path.display().to_string(),
            installed_sha256: file.intent.sha256.to_hex(),
            installed_size: file.intent.size,
            executable: file.intent.executable,
            created_directories,
        })
    }

    /// Replace the destination, keeping its previous contents as a durable backup.
    ///
    /// The backup is flushed *before* the new bytes are published. A crash between
    /// the two therefore leaves a backup holding what the receipt claims, which is
    /// the difference between a recoverable rollback and a receipt pointing at a
    /// file that holds the new contents.
    fn replace(
        &mut self,
        node: &TransactionNode,
    ) -> Result<OperationReceipt, LinuxFileExecutorError> {
        let file = self.file(node.id.as_str())?.clone();
        let FilePrecondition::Exact { size, sha256 } = file.precondition else {
            return Err(LinuxFileExecutorError::PlanDrift {
                path: file.host_path.display().to_string(),
                reason: "a replace must state the exact previous state".into(),
            });
        };
        // Proved against the bytes actually on disk, at the moment the operation is
        // about to overwrite them - not against what the plan expected to find
        // when it was written.
        verify_precondition(&file.host_path, size, sha256)?;

        let staged = file
            .staged
            .clone()
            .ok_or_else(|| LinuxFileExecutorError::PlanDrift {
                path: file.host_path.display().to_string(),
                reason: "the file was never staged".into(),
            })?;
        let directory = OwnedDirectory::open(&parent_of(&file.host_path)?)?;
        let name = file_name(&file.host_path)?;
        let previous = directory.read_regular(&name)?;

        let backup_name = format!("backup-{}.bin", short_digest(node.id.as_str()));
        directory.write_durable(&backup_name, &previous, STATE_FILE_MODE)?;
        let backup_path = directory.path().join(&backup_name);

        rustix::fs::chmod(&staged, file.intent.mode_for(self.executable_mode)).map_err(
            |error| LinuxFileExecutorError::Io {
                path: staged.display().to_string(),
                source: std::io::Error::from(error),
            },
        )?;
        directory.publish_replacing(&name, &file_name(&staged)?)?;
        verify_installed(&file.host_path, file.intent)?;

        Ok(OperationReceipt::ReplaceFile {
            destination: file.host_path.display().to_string(),
            previous_sha256: sha256.to_hex(),
            previous_size: size,
            backup_path: backup_path.display().to_string(),
            new_sha256: file.intent.sha256.to_hex(),
            new_size: file.intent.size,
            executable: file.intent.executable,
        })
    }

    /// Remove an owned file, keeping its contents as a backup.
    ///
    /// Linux lets a running process unlink its own image, so the maintenance
    /// executable this installs can be retired without a second runner waiting for
    /// it to exit. That is why there is no Linux equivalent of the Windows
    /// self-delete mechanism: there is no need for one.
    fn remove(
        &mut self,
        node: &TransactionNode,
    ) -> Result<OperationReceipt, LinuxFileExecutorError> {
        let file = self.file(node.id.as_str())?.clone();
        let Some((size, sha256)) = observe(&file.host_path)? else {
            return Err(LinuxFileExecutorError::Missing {
                path: file.host_path.display().to_string(),
            });
        };
        let directory = OwnedDirectory::open(&parent_of(&file.host_path)?)?;
        let bytes = directory.read_regular(&file_name(&file.host_path)?)?;
        let backup_name = format!("removed-{}.bin", short_digest(node.id.as_str()));
        directory.write_durable(&backup_name, &bytes, STATE_FILE_MODE)?;
        directory.remove_file(&file_name(&file.host_path)?)?;

        let backup_path = directory.path().join(&backup_name);
        Ok(OperationReceipt::RemoveFile {
            destination: file.destination.clone(),
            // The journal carries target paths, not host paths: a backup written by
            // a recovery run has to be readable by a plan recorded against the
            // target rather than against whichever machine wrote it.
            backup_path: as_target(&file.destination, &backup_path)?,
            sha256,
            size,
        })
    }
}

/// A refresh request from a backend node's journaled payload.
fn refresh_request(
    node: &TransactionNode,
) -> Result<crate::refresh::RefreshRequest, LinuxFileExecutorError> {
    let missing = || LinuxFileExecutorError::PlanDrift {
        path: node.id.to_string(),
        reason: "a refresh node without its request".into(),
    };
    let backend = node.meta.backend.clone().ok_or_else(missing)?;
    let request = crate::refresh::RefreshRequest::decode(&backend.payload).map_err(|error| {
        LinuxFileExecutorError::PlanDrift {
            path: node.id.to_string(),
            reason: error.to_string(),
        }
    })?;
    if backend.key != request.key() || backend.id != request.backend_id() {
        return Err(LinuxFileExecutorError::PlanDrift {
            path: node.id.to_string(),
            reason: "a refresh node whose identity is not its request".into(),
        });
    }
    Ok(request)
}

/// The portable coordinator contract, implemented with Linux semantics.
///
/// This is what lets the real [`TransactionCoordinator`] drive this executor:
/// the same prepare / apply / verify / commit phases as every other backend,
/// the same journal format, the same recovery. Nothing here re-states the
/// transaction protocol; it only answers each step with this platform's
/// mechanisms.
impl OperationExecutor for LinuxFileExecutor {
    type Error = LinuxFileExecutorError;

    fn prepare(&mut self, operation: &TransactionNode) -> Result<(), Self::Error> {
        LinuxFileExecutor::prepare(self, operation)
    }

    fn apply(&mut self, operation: &TransactionNode) -> Result<OperationReceipt, Self::Error> {
        LinuxFileExecutor::apply(self, operation)
    }

    fn verify(
        &mut self,
        operation: &TransactionNode,
        receipt: &OperationReceipt,
    ) -> Result<(), Self::Error> {
        LinuxFileExecutor::verify(self, operation, receipt)
    }

    fn rollback(
        &mut self,
        operation: &TransactionNode,
        receipt: &OperationReceipt,
    ) -> Result<(), Self::Error> {
        LinuxFileExecutor::rollback(self, operation, receipt)
    }

    fn reconcile(
        &mut self,
        operation: &TransactionNode,
        receipt: Option<&OperationReceipt>,
    ) -> Result<ReconcileResult, Self::Error> {
        LinuxFileExecutor::reconcile(self, operation, receipt)
    }
}

/// Prove the destination holds exactly the state the operation expects.
fn verify_precondition(
    destination: &Path,
    size: u64,
    sha256: Sha256Digest,
) -> Result<(), LinuxFileExecutorError> {
    let Some((found_size, found)) = observe(destination)? else {
        return Err(LinuxFileExecutorError::Missing {
            path: destination.display().to_string(),
        });
    };
    if found_size != size || found != sha256 {
        return Err(LinuxFileExecutorError::PlanDrift {
            path: destination.display().to_string(),
            reason: format!("expected {size} bytes / {sha256}, found {found_size} bytes / {found}"),
        });
    }
    Ok(())
}

/// Confirm a published file is the file the plan asked for.
fn verify_installed(destination: &Path, intent: FileIntent) -> Result<(), LinuxFileExecutorError> {
    let Some((size, sha256)) = observe(destination)? else {
        return Err(LinuxFileExecutorError::Verification {
            path: destination.display().to_string(),
            reason: "the published file is not there".into(),
        });
    };
    if size != intent.size || sha256 != intent.sha256 {
        return Err(LinuxFileExecutorError::Verification {
            path: destination.display().to_string(),
            reason: format!(
                "expected {} bytes / {}, found {size} bytes / {sha256}",
                intent.size, intent.sha256
            ),
        });
    }
    if intent.executable {
        let mode = file_mode(destination)?;
        if mode & 0o111 == 0 {
            return Err(LinuxFileExecutorError::Verification {
                path: destination.display().to_string(),
                reason: format!("declared runnable but installed with mode {mode:o}"),
            });
        }
    }
    Ok(())
}

/// A file's identity, refusing anything that is not a regular file.
///
/// A symbolic link at an owned path is refused rather than followed: it is either
/// drift or an attack, and reading through it would report a file's identity from a
/// tree this installation does not own.
fn observe(destination: &Path) -> Result<Option<(u64, Sha256Digest)>, LinuxFileExecutorError> {
    let Ok(directory) = OwnedDirectory::open(&parent_of(destination)?) else {
        return Ok(None);
    };
    let Ok(name) = file_name(destination) else {
        return Ok(None);
    };
    match directory.kind_or_absent(&name)? {
        None => Ok(None),
        Some(EntryKind::Regular) => {
            let bytes = directory.read_regular(&name)?;
            Ok(Some(hash_reader(bytes.as_slice()).map_err(|error| {
                LinuxFileExecutorError::Io {
                    path: destination.display().to_string(),
                    source: error,
                }
            })?))
        }
        Some(EntryKind::Symlink) => Err(LinuxFileExecutorError::UnexpectedKind {
            path: destination.display().to_string(),
            expected: "regular file, not a symbolic link",
        }),
        Some(_) => Err(LinuxFileExecutorError::UnexpectedKind {
            path: destination.display().to_string(),
            expected: "regular file",
        }),
    }
}

/// A file's own permission bits.
fn file_mode(destination: &Path) -> Result<u32, LinuxFileExecutorError> {
    use std::os::fd::AsFd as _;
    let directory = OwnedDirectory::open(&parent_of(destination)?)?;
    let file = directory.open_regular_read(&file_name(destination)?)?;
    // Read through the descriptor, so the mode reported belongs to the file itself
    // rather than to whatever a second path lookup happened to find.
    let stat = rustix::fs::fstat(file.as_fd()).map_err(|error| LinuxFileExecutorError::Io {
        path: destination.display().to_string(),
        source: std::io::Error::from(error),
    })?;
    Ok(stat.st_mode & 0o777)
}

/// The directory a path's file lives in.
fn parent_of(destination: &Path) -> Result<PathBuf, LinuxFileExecutorError> {
    destination
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .ok_or_else(|| LinuxFileExecutorError::PlanDrift {
            path: destination.display().to_string(),
            reason: "destination has no parent directory".into(),
        })
}

/// The final component of a path.
fn file_name(destination: impl AsRef<Path>) -> Result<String, LinuxFileExecutorError> {
    let destination = destination.as_ref();
    destination
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .ok_or_else(|| LinuxFileExecutorError::PlanDrift {
            path: destination.display().to_string(),
            reason: "destination has no final component".into(),
        })
}

/// Create the missing parents of a destination, shallowest first.
///
/// Each one is created `0700` and returned in the order they were made, so an
/// uninstall removes them in reverse and only while they are empty. A directory that
/// already exists is not reported: zup cannot prove it created that one, and a
/// directory it cannot prove it owns must survive the uninstall.
fn create_parents(destination: &Path) -> Result<Vec<String>, LinuxFileExecutorError> {
    let mut missing = Vec::new();
    let mut cursor = parent_of(destination)?;
    while !cursor.as_os_str().is_empty() && !cursor.exists() {
        missing.push(cursor.clone());
        if !cursor.pop() {
            break;
        }
    }
    missing.reverse();
    let mut created = Vec::new();
    for directory in missing {
        rustix::fs::mkdir(&directory, STATE_DIRECTORY_MODE).map_err(|error| {
            LinuxFileExecutorError::Io {
                path: directory.display().to_string(),
                source: std::io::Error::from(error),
            }
        })?;
        created.push(directory.display().to_string());
    }
    Ok(created)
}

/// Re-spell a host path the way the target spells it, for a journal.
fn as_target(
    anchor: &zup_platform::TargetPath,
    host_path: &Path,
) -> Result<zup_platform::TargetPath, LinuxFileExecutorError> {
    zup_platform::TargetPath::new(
        anchor.target().clone(),
        host_path.to_string_lossy().as_ref(),
    )
    .map_err(|error| LinuxFileExecutorError::PlanDrift {
        path: host_path.display().to_string(),
        reason: error.to_string(),
    })
}

/// A staged file's name, derived from its operation.
fn staged_name(id: &OperationId) -> String {
    // Operation ids are opaque strings, not filenames, and a plan is not trusted
    // to supply one that is. A digest of the id keeps the name stable across a
    // recovery run - which matters, because a recovering executor has to find the
    // bytes the crashed one staged - and safe to create with `O_EXCL`.
    format!("staged-{}", short_digest(id.as_str()))
}

fn short_digest(value: &str) -> String {
    let (_, digest) = hash_reader(value.as_bytes()).expect("a string hashes");
    digest.to_hex()[..16].to_owned()
}
