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

use zup_core::{Sha256Digest, hash_reader};
use zup_transaction::{
    FileDelta, FilePrecondition, NodeKind, OperationId, OperationReceipt, ReconcileResult,
    TransactionNode,
};

use crate::fs::{
    EXECUTABLE_PAYLOAD_MODE, EntryKind, FileSystemError, OwnedDirectory, PAYLOAD_FILE_MODE,
    STATE_DIRECTORY_MODE, STATE_FILE_MODE,
};

/// Why a Linux file operation could not be carried out.
#[derive(Debug, thiserror::Error)]
pub enum LinuxFileExecutorError {
    #[error("plan drift at `{path}`: {reason}")]
    PlanDrift { path: String, reason: String },

    #[error("verification failed at `{path}`: {reason}")]
    Verification { path: String, reason: String },

    #[error("rollback cannot restore `{path}`: {reason}")]
    RollbackDrift { path: String, reason: String },

    #[error("`{path}` is not a {expected}")]
    UnexpectedKind {
        path: String,
        expected: &'static str,
    },

    #[error("`{path}` already exists")]
    AlreadyExists { path: String },

    #[error("`{path}` does not exist")]
    Missing { path: String },

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
    /// The mode this backend installs a file with.
    ///
    /// Additive on purpose: `0644` plus the owner's execute bit is `0744`, so a
    /// file that is meant to be runnable becomes runnable without anything else in
    /// the install becoming writable.
    fn mode(self) -> rustix::fs::Mode {
        if self.executable {
            EXECUTABLE_PAYLOAD_MODE
        } else {
            PAYLOAD_FILE_MODE
        }
    }
}

/// Carries out a Linux file transaction.
#[derive(Debug, Default)]
pub struct LinuxFileExecutor {
    files: BTreeMap<String, FileWork>,
}

impl LinuxFileExecutor {
    /// An executor with nothing registered yet.
    ///
    /// No work root: staging happens in the destination's own directory so that
    /// publication is a same-filesystem rename. See the module documentation.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record where one operation's file goes and what it should hold.
    pub fn register(&mut self, id: &OperationId, file: FileWork) {
        self.files.insert(id.to_string(), file);
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
        if let Some(recorded) = self.files.get_mut(id.as_str()) {
            recorded.staged = Some(staged.clone());
            recorded.created_directories = created_directories;
        }
        Ok(OperationReceipt::StageFile {
            staged_path: staged.display().to_string(),
            size: file.intent.size,
            sha256: file.intent.sha256.to_hex(),
        })
    }

    /// Apply one transaction node.
    pub fn apply(
        &mut self,
        node: &TransactionNode,
    ) -> Result<OperationReceipt, LinuxFileExecutorError> {
        match &node.kind {
            NodeKind::Barrier => Ok(OperationReceipt::Control),
            NodeKind::StageFile { .. } => Err(LinuxFileExecutorError::PlanDrift {
                path: node.id.to_string(),
                reason: "a staged file is written with `stage`, not applied".into(),
            }),
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
            NodeKind::BackendOperation { .. } | NodeKind::BackendRemoval { .. } => {
                Err(LinuxFileExecutorError::PlanDrift {
                    path: node.id.to_string(),
                    reason: "a backend resource is not a file operation".into(),
                })
            }
        }
    }

    /// Confirm an applied node's installed state matches its receipt.
    ///
    /// Observe-only: the transaction calls this before the commit barrier, so an
    /// operation that mutated something here would defeat the barrier.
    pub fn verify(
        &self,
        node: &TransactionNode,
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
            OperationReceipt::Backend { .. } => Err(LinuxFileExecutorError::PlanDrift {
                path: node.id.to_string(),
                reason: "a backend receipt is not verified by the file executor".into(),
            }),
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
            OperationReceipt::Backend { .. } => Err(LinuxFileExecutorError::PlanDrift {
                path: String::new(),
                reason: "a backend receipt is not rolled back by the file executor".into(),
            }),
        }
    }

    /// Decide what a node's state is, for a transaction that crashed.
    pub fn reconcile(
        &self,
        _node: &TransactionNode,
        receipt: Option<&OperationReceipt>,
    ) -> Result<ReconcileResult, LinuxFileExecutorError> {
        let Some(receipt) = receipt else {
            return Ok(ReconcileResult::NotApplied);
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
            OperationReceipt::Backend { .. } => Ok(ReconcileResult::Ambiguous),
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
        rustix::fs::chmod(&staged, file.intent.mode()).map_err(|error| {
            LinuxFileExecutorError::Io {
                path: staged.display().to_string(),
                source: std::io::Error::from(error),
            }
        })?;
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

        rustix::fs::chmod(&staged, file.intent.mode()).map_err(|error| {
            LinuxFileExecutorError::Io {
                path: staged.display().to_string(),
                source: std::io::Error::from(error),
            }
        })?;
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
