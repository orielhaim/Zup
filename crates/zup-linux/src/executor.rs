use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use zup_bundle::{PayloadError, PayloadSource};
use zup_core::{RelativePath, ResourceKey, Sha256Digest, hash_reader};
use zup_exec::{HostSnapshot, ObservedFile, ObservedFileState};
use zup_platform::TargetPath;
use zup_transaction::{
    FileDelta, FilePrecondition, FileRemoval, FileRemovalKind, NodeKind, OperationExecutor,
    OperationId, OperationReceipt, ReconcileResult, TransactionId, TransactionNode,
    TransactionPlan,
};

use crate::error::{ExecError, PathError};
use crate::fs::{
    EXECUTABLE_PAYLOAD_MODE, EntryKind, OwnedDirectory, PAYLOAD_FILE_MODE, StreamedRegular,
    USER_INSTALL_DIRECTORY_MODE,
};
use crate::lowering::to_host_path;

#[derive(Debug, Clone)]
pub struct FileWork {
    pub destination: zup_platform::TargetPath,

    pub host_path: PathBuf,

    pub intent: FileIntent,

    pub precondition: FilePrecondition,
}

#[derive(Debug, Clone, Copy)]
pub struct FileIntent {
    pub sha256: Sha256Digest,
    pub size: u64,
    pub executable: bool,
}

impl FileIntent {
    fn mode_for(self, executable_mode: rustix::fs::Mode) -> rustix::fs::Mode {
        if self.executable {
            executable_mode
        } else {
            PAYLOAD_FILE_MODE
        }
    }
}

pub struct LinuxFileExecutor {
    files: BTreeMap<String, FileWork>,
    staged_dirs: BTreeMap<String, Vec<String>>,
    payload: Option<Box<dyn PayloadSource>>,
    executable_mode: rustix::fs::Mode,
    install_dir_mode: rustix::fs::Mode,
    transaction: Option<TransactionId>,
    services: Option<ServiceSupport>,
}

pub struct ServiceSupport {
    roots: crate::locations::MachineRoots,
    systemd: crate::machine::SystemdRoots,
    manager: Box<dyn crate::systemd::SystemdManager>,
    expected_uid: u32,
}

impl ServiceSupport {
    pub fn production() -> Result<Self, String> {
        Ok(Self {
            roots: crate::locations::MachineRoots::production(),
            systemd: crate::machine::SystemdRoots::production(),
            manager: Box::new(
                crate::systemd::RealSystemd::connect().map_err(|error| error.to_string())?,
            ),
            expected_uid: rustix::process::geteuid().as_raw(),
        })
    }

    pub fn isolated(
        roots: crate::locations::MachineRoots,
        systemd: crate::machine::SystemdRoots,
        manager: impl crate::systemd::SystemdManager + 'static,
        expected_uid: u32,
    ) -> Self {
        Self::new(roots, systemd, Box::new(manager), expected_uid)
    }

    pub fn new(
        roots: crate::locations::MachineRoots,
        systemd: crate::machine::SystemdRoots,
        manager: Box<dyn crate::systemd::SystemdManager>,
        expected_uid: u32,
    ) -> Self {
        Self {
            roots,
            systemd,
            manager,
            expected_uid,
        }
    }
}

impl Default for LinuxFileExecutor {
    fn default() -> Self {
        Self {
            files: BTreeMap::new(),
            staged_dirs: BTreeMap::new(),
            payload: None,
            executable_mode: EXECUTABLE_PAYLOAD_MODE,
            install_dir_mode: USER_INSTALL_DIRECTORY_MODE,
            transaction: None,
            services: None,
        }
    }
}

impl std::fmt::Debug for LinuxFileExecutor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LinuxFileExecutor")
            .field("files", &self.files)
            .field("has_payload", &self.payload.is_some())
            .field("has_services", &self.services.is_some())
            .finish()
    }
}

impl LinuxFileExecutor {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_payload(mut self, payload: impl PayloadSource + 'static) -> Self {
        self.payload = Some(Box::new(payload));
        self
    }

    pub fn for_machine(mut self) -> Self {
        self.executable_mode = rustix::fs::Mode::from_bits_truncate(0o755);
        self.install_dir_mode = crate::fs::MACHINE_INSTALL_DIRECTORY_MODE;
        self
    }

    pub fn with_services(mut self, services: ServiceSupport) -> Self {
        self.services = Some(services);
        self
    }

    pub fn set_transaction_id(&mut self, id: TransactionId) {
        self.transaction = Some(id);
    }

    fn transaction_id(&self) -> Result<TransactionId, ExecError> {
        self.transaction.ok_or_else(|| {
            ExecError::Executor("file operations need a transaction identity".into())
        })
    }

    pub fn register(&mut self, id: &OperationId, file: FileWork) {
        self.files.insert(id.to_string(), file);
    }

    pub fn register_plan(&mut self, plan: &TransactionPlan) -> Result<(), ExecError> {
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
                    return Err(ExecError::PlanDrift {
                        path: node.id.to_string(),
                        reason: format!("{other:?} is not a file resource"),
                    });
                }
            };
            let destination = TargetPath::new(plan.target, destination_text).map_err(|error| {
                ExecError::PlanDrift {
                    path: destination_text.clone(),
                    reason: error.to_string(),
                }
            })?;
            let host_path = to_host_path(&destination).map_err(|error| ExecError::PlanDrift {
                path: destination_text.clone(),
                reason: error.to_string(),
            })?;

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
                    return Err(ExecError::PlanDrift {
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
                },
            );
        }
        Ok(())
    }

    pub fn file(&self, id: &str) -> Result<&FileWork, ExecError> {
        self.files.get(id).ok_or_else(|| ExecError::PlanDrift {
            path: id.to_owned(),
            reason: "no registered file for this operation".into(),
        })
    }

    fn executable_mode_for(&self, intent: FileIntent) -> u32 {
        (intent.mode_for(self.executable_mode)).bits() & 0o7777
    }

    pub fn stage(&mut self, id: &OperationId, bytes: &[u8]) -> Result<OperationReceipt, ExecError> {
        let file = self.file(id.as_str())?.clone();
        let (found_size, found) = hash_reader(bytes).map_err(|error| PathError::Io {
            path: file.host_path.display().to_string(),
            source: error,
        })?;
        if found_size != file.intent.size || found != file.intent.sha256 {
            return Err(ExecError::Verification {
                path: file.host_path.display().to_string(),
                reason: "the staged bytes do not match the payload identity".into(),
            });
        }
        let name = staged_name(id);
        let (directory, _, created) =
            crate::fs::ensure_parent(&file.host_path, self.install_dir_mode)?;
        self.staged_dirs
            .entry(file.host_path.display().to_string())
            .or_default()
            .extend(created);
        let mut cursor = std::io::Cursor::new(bytes);
        let (size, sha256) = directory
            .write_stream_durable(
                &name,
                &mut cursor,
                file.intent.size,
                crate::fs::STATE_FILE_MODE,
            )
            .map_err(ExecError::from)?;
        if size != file.intent.size || sha256 != file.intent.sha256 {
            return Err(ExecError::Verification {
                path: file.host_path.display().to_string(),
                reason: "the staged bytes do not match the payload identity".into(),
            });
        }
        let staged = directory.path().join(&name);
        Ok(OperationReceipt::StageFile {
            staged_path: staged.display().to_string(),
            size: file.intent.size,
            sha256: file.intent.sha256.to_hex(),
        })
    }

    pub fn prepare(&mut self, node: &TransactionNode) -> Result<(), ExecError> {
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
                        Some(_) => Err(ExecError::PlanDrift {
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
            NodeKind::FileRemoval { key } => {
                let file = self.file(node.id.as_str())?.clone();
                let removal = removal_identity(node)?;
                if removal.key != *key {
                    return Err(ExecError::PlanDrift {
                        path: file.host_path.display().to_string(),
                        reason: "a removal without its own identity".into(),
                    });
                }
                self.check_removal_target(&file, removal)?;
                let observed = observe(&file.host_path)?.ok_or_else(|| ExecError::PlanDrift {
                    path: file.host_path.display().to_string(),
                    reason: "there is nothing to remove".into(),
                })?;
                if observed.0 != removal.size || observed.1 != removal.sha256 {
                    return Err(ExecError::PlanDrift {
                        path: file.host_path.display().to_string(),
                        reason: format!(
                            "the file changed outside this transaction: expected {} bytes / {}, found {} bytes / {}",
                            removal.size, removal.sha256, observed.0, observed.1,
                        ),
                    });
                }
                Ok(())
            }
            NodeKind::BackendOperation { .. } => {
                if is_service_node(node) {
                    return self.prepare_service(node);
                }

                let request = refresh_request(node)?;
                crate::refresh::preflight(&request)?;
                Ok(())
            }
            NodeKind::BackendRemoval { .. } => {
                if is_service_node(node) {
                    return self.prepare_service(node);
                }
                Err(ExecError::PlanDrift {
                    path: node.id.to_string(),
                    reason: "a backend removal is not a Linux operation".into(),
                })
            }
        }
    }

    fn stage_node(&mut self, node: &TransactionNode) -> Result<OperationReceipt, ExecError> {
        let file = self.file(node.id.as_str())?.clone();
        let (source, sha256, size) = Self::payload_identity(node)?;
        let payload = self.payload.as_ref().ok_or_else(|| ExecError::PlanDrift {
            path: node.id.to_string(),
            reason: "no payload source is attached".into(),
        })?;
        let mut reader =
            payload
                .open(&source, &sha256, size)
                .map_err(|error| ExecError::Payload {
                    path: source.to_string(),
                    source: error,
                })?;
        let name = staged_name(&node.id);
        let (directory, _, created) =
            crate::fs::ensure_parent(&file.host_path, self.install_dir_mode)?;
        self.staged_dirs
            .entry(file.host_path.display().to_string())
            .or_default()
            .extend(created);
        let (found_size, found) = directory
            .write_stream_durable(&name, &mut reader, size, crate::fs::STATE_FILE_MODE)
            .map_err(ExecError::from)?;
        if found_size != size
            || found != sha256
            || found_size != file.intent.size
            || found != file.intent.sha256
        {
            return Err(ExecError::Payload {
                path: source.to_string(),
                source: PayloadError::DigestMismatch {
                    path: source.to_string(),
                },
            });
        }
        let staged = directory.path().join(&name);
        Ok(OperationReceipt::StageFile {
            staged_path: staged.display().to_string(),
            size: file.intent.size,
            sha256: file.intent.sha256.to_hex(),
        })
    }

    fn payload_identity(
        node: &TransactionNode,
    ) -> Result<(RelativePath, Sha256Digest, u64), ExecError> {
        let missing = || ExecError::PlanDrift {
            path: node.id.to_string(),
            reason: "a staging node without payload identity".into(),
        };
        Ok((
            node.meta.source_relative.clone().ok_or_else(missing)?,
            node.meta.expected_sha256.ok_or_else(missing)?,
            node.meta.expected_size.ok_or_else(missing)?,
        ))
    }

    pub fn apply(&mut self, node: &TransactionNode) -> Result<OperationReceipt, ExecError> {
        match &node.kind {
            NodeKind::Barrier => Ok(OperationReceipt::Control),
            NodeKind::StageFile { .. } => self.stage_node(node),
            NodeKind::FileMutation { delta, .. } => match delta {
                FileDelta::Create | FileDelta::RestoreOwned => self.create(node),
                FileDelta::Replace | FileDelta::RepairOwned => self.replace(node),
                FileDelta::NoOp => Ok(OperationReceipt::Control),
                other => Err(ExecError::PlanDrift {
                    path: node.id.to_string(),
                    reason: format!("{other:?} is not applied by the file executor"),
                }),
            },
            NodeKind::FileRemoval { .. } => self.remove(node),
            NodeKind::BackendOperation { .. } => {
                if is_service_node(node) {
                    return self.apply_service(node);
                }
                let request = refresh_request(node)?;
                crate::refresh::run_refresh(&request)?;
                Ok(OperationReceipt::Backend {
                    key: request.key(),
                    payload: request.encode(),
                })
            }
            NodeKind::BackendRemoval { .. } => {
                if is_service_node(node) {
                    return self.apply_service(node);
                }
                Err(ExecError::PlanDrift {
                    path: node.id.to_string(),
                    reason: "a backend removal is not a Linux operation".into(),
                })
            }
        }
    }

    pub fn verify(
        &mut self,
        _node: &TransactionNode,
        receipt: &OperationReceipt,
    ) -> Result<(), ExecError> {
        match receipt {
            OperationReceipt::CreateFile {
                destination,
                installed_sha256,
                installed_size,
                executable,
                ..
            } => {
                let expected = FileIntent {
                    sha256: installed_sha256
                        .parse()
                        .map_err(|_| ExecError::RollbackDrift {
                            path: destination.clone(),
                            reason: "invalid journal digest".into(),
                        })?,
                    size: *installed_size,
                    executable: *executable,
                };
                verify_installed(
                    Path::new(destination),
                    expected,
                    self.executable_mode_for(expected),
                )
            }
            OperationReceipt::ReplaceFile {
                destination,
                new_sha256,
                new_size,
                executable,
                ..
            } => {
                let expected = FileIntent {
                    sha256: new_sha256.parse().map_err(|_| ExecError::RollbackDrift {
                        path: destination.clone(),
                        reason: "invalid journal digest".into(),
                    })?,
                    size: *new_size,
                    executable: *executable,
                };
                verify_installed(
                    Path::new(destination),
                    expected,
                    self.executable_mode_for(expected),
                )?;
                if !receipt_backup_valid(receipt) {
                    return Err(ExecError::Verification {
                        path: destination.clone(),
                        reason: "the backup a rollback would need is gone".into(),
                    });
                }
                Ok(())
            }
            OperationReceipt::RemoveFile { destination, .. } => {
                match observe(&PathBuf::from(destination.to_string()))? {
                    None => {}
                    Some(_) => {
                        return Err(ExecError::Verification {
                            path: destination.to_string(),
                            reason: "the file is still there".into(),
                        });
                    }
                }
                if !receipt_backup_valid(receipt) {
                    return Err(ExecError::Verification {
                        path: destination.to_string(),
                        reason: "the backup a rollback would need is gone".into(),
                    });
                }
                Ok(())
            }
            OperationReceipt::Control | OperationReceipt::StageFile { .. } => Ok(()),
            OperationReceipt::Backend { key, payload } => {
                if is_service_key(key) {
                    return self.verify_service_receipt(key, payload);
                }

                let request = crate::refresh::RefreshRequest::decode(payload)?;
                crate::refresh::preflight(&request)?;
                match std::fs::symlink_metadata(&request.directory) {
                    Ok(metadata) if metadata.is_dir() => Ok(()),
                    _ => Err(ExecError::Verification {
                        path: request.directory.clone(),
                        reason: "the refreshed database directory is not there".into(),
                    }),
                }
            }
        }
    }

    pub fn rollback(
        &mut self,
        node: &TransactionNode,
        receipt: &OperationReceipt,
    ) -> Result<(), ExecError> {
        match receipt {
            OperationReceipt::CreateFile {
                destination,
                installed_sha256,
                installed_size,
                executable,
                created_directories,
            } => {
                let path = PathBuf::from(destination);
                let file = self.file(node.id.as_str())?.clone();
                if path != file.host_path {
                    return Err(ExecError::RollbackDrift {
                        path: destination.clone(),
                        reason: "the journal destination is not this operation's file".into(),
                    });
                }
                let expected = FileIntent {
                    sha256: installed_sha256
                        .parse()
                        .map_err(|_| ExecError::RollbackDrift {
                            path: destination.clone(),
                            reason: "invalid journal digest".into(),
                        })?,
                    size: *installed_size,
                    executable: *executable,
                };
                let expected_mode = self.executable_mode_for(expected);
                let (directory, name) = crate::fs::open_parent(&path)?;
                match directory.kind_or_absent(&name)? {
                    None => Ok(()),
                    Some(EntryKind::Regular) => {
                        let current = stream_expected(
                            &directory,
                            &name,
                            &path,
                            expected.size,
                            expected.sha256,
                        )
                        .map_err(|_| ExecError::RollbackDrift {
                            path: path.display().to_string(),
                            reason: "the file is no longer the one this transaction wrote".into(),
                        })?;
                        if current.mode != expected_mode {
                            return Err(ExecError::RollbackDrift {
                                path: path.display().to_string(),
                                reason: "the file is no longer the one this transaction wrote"
                                    .into(),
                            });
                        }
                        directory.remove_file(&name)?;
                        directory.sync()?;

                        for created in created_directories.iter().rev() {
                            let created = Path::new(created);
                            let Some(parent) = created.parent() else {
                                continue;
                            };
                            let Some(leaf) = created
                                .file_name()
                                .map(|name| name.to_string_lossy().into_owned())
                                .filter(|name| !name.is_empty())
                            else {
                                continue;
                            };
                            if leaf.contains('/') || leaf.contains('\0') {
                                continue;
                            }
                            if let Ok(parent) = OwnedDirectory::open(parent) {
                                let _ = parent.remove_empty_directory(&leaf);
                            }
                        }
                        Ok(())
                    }
                    Some(_) => Err(ExecError::RollbackDrift {
                        path: path.display().to_string(),
                        reason: "the destination is no longer a regular file".into(),
                    }),
                }
            }
            OperationReceipt::ReplaceFile {
                destination,
                previous_sha256,
                previous_size,
                previous_mode,
                backup_path,
                new_sha256,
                new_size,
                executable,
            } => {
                let path = PathBuf::from(destination);
                let file = self.file(node.id.as_str())?.clone();
                if path != file.host_path {
                    return Err(ExecError::RollbackDrift {
                        path: destination.clone(),
                        reason: "the journal destination is not this operation's file".into(),
                    });
                }
                let previous = FileIntent {
                    sha256: previous_sha256
                        .parse()
                        .map_err(|_| ExecError::RollbackDrift {
                            path: destination.clone(),
                            reason: "invalid journal digest".into(),
                        })?,
                    size: *previous_size,
                    executable: previous_mode & 0o111 != 0,
                };
                let new = FileIntent {
                    sha256: new_sha256.parse().map_err(|_| ExecError::RollbackDrift {
                        path: destination.clone(),
                        reason: "invalid journal digest".into(),
                    })?,
                    size: *new_size,
                    executable: *executable,
                };
                let (directory, name) = crate::fs::open_parent(&path)?;
                let transaction = self.transaction_id()?;
                let backup = check_backup(
                    &directory,
                    &path,
                    backup_path,
                    &transaction,
                    node.id.as_str(),
                    "backup",
                )?;

                match current_state(
                    &directory,
                    &name,
                    &path,
                    previous.size,
                    previous.sha256,
                    new.size,
                    new.sha256,
                )? {
                    Current::Absent => {
                        let saved = stream_expected(
                            &directory,
                            &backup,
                            &path,
                            previous.size,
                            previous.sha256,
                        )
                        .map_err(|_| ExecError::RollbackDrift {
                            path: path.display().to_string(),
                            reason: "the backup no longer holds the previous file".into(),
                        })?;
                        let _ = saved;
                        directory
                            .rename_noreplace(&backup, &name)
                            .map_err(ExecError::from)
                            .map_err(|error| match error {
                                ExecError::Path(PathError::AlreadyExists { .. }) => {
                                    ExecError::RollbackDrift {
                                        path: path.display().to_string(),
                                        reason:
                                            "the destination was recreated outside this transaction"
                                                .into(),
                                    }
                                }
                                other => other,
                            })?;
                        verify_previous(&path, previous.size, previous.sha256, *previous_mode)?;
                        Ok(())
                    }
                    Current::MatchesPrevious => {
                        if backup_holds(&directory, &backup, previous.size, previous.sha256) {
                            let _ = directory.remove_file(&backup);
                            let _ = directory.sync();
                        }
                        let current = stream_expected(
                            &directory,
                            &name,
                            &path,
                            previous.size,
                            previous.sha256,
                        )
                        .map_err(|_| ExecError::RollbackDrift {
                            path: path.display().to_string(),
                            reason: "the destination changed during rollback".into(),
                        })?;
                        if current.mode != *previous_mode {
                            directory.chmod_regular(&name, mode_from(*previous_mode))?;
                            let _ = directory.sync();
                        }
                        verify_installed(&path, previous, *previous_mode)?;
                        Ok(())
                    }
                    Current::MatchesNew => {
                        let saved = stream_expected(
                            &directory,
                            &backup,
                            &path,
                            previous.size,
                            previous.sha256,
                        )
                        .map_err(|_| ExecError::RollbackDrift {
                            path: path.display().to_string(),
                            reason: "the backup no longer holds the previous file".into(),
                        })?;
                        if directory.try_exchange(&name, &backup)? {
                            directory.sync()?;
                            let restored = stream_expected(
                                &directory,
                                &name,
                                &path,
                                previous.size,
                                previous.sha256,
                            )
                            .map_err(|_| ExecError::RollbackDrift {
                                path: path.display().to_string(),
                                reason: "the destination changed during rollback".into(),
                            })?;
                            if restored.dev != saved.dev || restored.ino != saved.ino {
                                let _ = directory.try_exchange(&name, &backup);
                                return Err(ExecError::RollbackDrift {
                                    path: path.display().to_string(),
                                    reason: "the destination changed during rollback".into(),
                                });
                            }
                            let _ = directory.remove_file(&backup);
                            let _ = directory.sync();
                        } else {
                            let temporary = rollback_temp_name(&transaction, node.id.as_str());
                            Quarantine {
                                directory: &directory,
                                display: &path,
                            }
                            .reverse_restore(&name, &backup, &temporary, new.size, new.sha256)?;
                        }
                        if saved.mode != *previous_mode {
                            directory.chmod_regular(&name, mode_from(*previous_mode))?;
                            let _ = directory.sync();
                        }
                        verify_installed(&path, previous, *previous_mode)?;
                        Ok(())
                    }
                    Current::Foreign => Err(ExecError::RollbackDrift {
                        path: path.display().to_string(),
                        reason: "the destination was modified outside this transaction".into(),
                    }),
                }
            }
            OperationReceipt::RemoveFile {
                destination,
                backup_path,
                sha256,
                size,
                mode,
            } => {
                let file = self.file(node.id.as_str())?.clone();
                if *destination != file.destination {
                    return Err(ExecError::RollbackDrift {
                        path: destination.to_string(),
                        reason: "the journal destination is not this operation's file".into(),
                    });
                }
                let path = to_host_path(destination).map_err(|error| ExecError::RollbackDrift {
                    path: destination.to_string(),
                    reason: error.to_string(),
                })?;
                if path != file.host_path {
                    return Err(ExecError::RollbackDrift {
                        path: path.display().to_string(),
                        reason: "the journal destination is not this operation's file".into(),
                    });
                }
                let backup_host =
                    to_host_path(backup_path).map_err(|error| ExecError::RollbackDrift {
                        path: backup_path.to_string(),
                        reason: error.to_string(),
                    })?;
                let (directory, name) = crate::fs::open_parent(&path)?;
                let transaction = self.transaction_id()?;
                let backup = check_backup_host(
                    &directory,
                    &path,
                    &backup_host,
                    &transaction,
                    node.id.as_str(),
                    "removed",
                )?;

                match removed_state(&directory, &name, &path, *size, *sha256)? {
                    Restored::Already => {
                        if backup_holds(&directory, &backup, *size, *sha256) {
                            let _ = directory.remove_file(&backup);
                            let _ = directory.sync();
                        }
                        let current = stream_expected(&directory, &name, &path, *size, *sha256)
                            .map_err(|_| ExecError::RollbackDrift {
                                path: path.display().to_string(),
                                reason: "the destination changed during rollback".into(),
                            })?;
                        if current.mode != *mode {
                            directory.chmod_regular(&name, mode_from(*mode))?;
                            let _ = directory.sync();
                        }
                        Ok(())
                    }
                    Restored::Absent => {
                        let saved = stream_expected(&directory, &backup, &path, *size, *sha256)
                            .map_err(|_| ExecError::RollbackDrift {
                                path: path.display().to_string(),
                                reason: "the backup no longer holds the removed file".into(),
                            })?;
                        let _ = saved;
                        directory
                            .rename_noreplace(&backup, &name)
                            .map_err(ExecError::from)
                            .map_err(|error| match error {
                                ExecError::Path(PathError::AlreadyExists { .. }) => {
                                    ExecError::RollbackDrift {
                                        path: path.display().to_string(),
                                        reason:
                                            "the destination was recreated outside this transaction"
                                                .into(),
                                    }
                                }
                                other => other,
                            })?;
                        verify_previous(&path, *size, *sha256, *mode)?;
                        Ok(())
                    }
                    Restored::Foreign => Err(ExecError::RollbackDrift {
                        path: path.display().to_string(),
                        reason: "the destination was modified outside this transaction".into(),
                    }),
                }
            }
            OperationReceipt::Control | OperationReceipt::StageFile { .. } => {
                if matches!(receipt, OperationReceipt::StageFile { .. }) {
                    self.discard_staged(node);
                }
                Ok(())
            }
            OperationReceipt::Backend { key, payload } => {
                if is_service_key(key) {
                    return self.rollback_service(node, key, payload);
                }

                Ok(())
            }
        }
    }

    pub fn reconcile(
        &mut self,
        node: &TransactionNode,
        receipt: Option<&OperationReceipt>,
    ) -> Result<ReconcileResult, ExecError> {
        let Some(receipt) = receipt else {
            return self.reconcile_interrupted(node);
        };
        match receipt {
            OperationReceipt::CreateFile {
                destination,
                installed_sha256,
                installed_size,
                executable,
                ..
            }
            | OperationReceipt::ReplaceFile {
                destination,
                new_sha256: installed_sha256,
                new_size: installed_size,
                executable,
                ..
            } => {
                let expected = FileIntent {
                    sha256: installed_sha256
                        .parse()
                        .map_err(|_| ExecError::RollbackDrift {
                            path: destination.clone(),
                            reason: "invalid journal digest".into(),
                        })?,
                    size: *installed_size,
                    executable: *executable,
                };
                let expected_mode = (expected.mode_for(self.executable_mode)).bits() & 0o7777;
                match observe(Path::new(destination))? {
                    None => Ok(ReconcileResult::NotApplied),
                    Some(_)
                        if verify_installed(Path::new(destination), expected, expected_mode)
                            .is_ok()
                            && receipt_backup_valid(receipt) =>
                    {
                        Ok(ReconcileResult::AppliedWithReceipt(receipt.clone()))
                    }
                    Some(_) => Ok(ReconcileResult::Ambiguous),
                }
            }
            OperationReceipt::RemoveFile {
                destination,
                sha256,
                size,
                ..
            } => {
                let path = PathBuf::from(destination.to_string());
                match observe(&path)? {
                    None if receipt_backup_valid(receipt) => {
                        Ok(ReconcileResult::AppliedWithReceipt(receipt.clone()))
                    }
                    None => Ok(ReconcileResult::Ambiguous),
                    Some((found_size, found)) if found_size == *size && found == *sha256 => {
                        Ok(ReconcileResult::NotApplied)
                    }
                    Some(_) => Ok(ReconcileResult::Ambiguous),
                }
            }
            OperationReceipt::Control | OperationReceipt::StageFile { .. } => {
                Ok(ReconcileResult::AppliedWithReceipt(receipt.clone()))
            }

            OperationReceipt::Backend { key, .. } => {
                if is_service_key(key) {
                    return self.reconcile_service(node, Some(receipt));
                }
                Ok(ReconcileResult::NotApplied)
            }
        }
    }

    fn reconcile_interrupted(
        &mut self,
        node: &TransactionNode,
    ) -> Result<ReconcileResult, ExecError> {
        match &node.kind {
            NodeKind::Barrier => Ok(ReconcileResult::Applied),

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

            NodeKind::BackendOperation { .. } => {
                if is_service_node(node) {
                    return self.reconcile_service(node, None);
                }
                Ok(ReconcileResult::NotApplied)
            }
            NodeKind::BackendRemoval { .. } => {
                if is_service_node(node) {
                    return self.reconcile_service(node, None);
                }
                Ok(ReconcileResult::Ambiguous)
            }
        }
    }

    fn reconcile_interrupted_create(
        &mut self,
        node: &TransactionNode,
    ) -> Result<ReconcileResult, ExecError> {
        let file = self.file(node.id.as_str())?.clone();
        let expected_mode = self.executable_mode_for(file.intent);
        match observe(&file.host_path)? {
            None => Ok(ReconcileResult::NotApplied),
            Some(_) if verify_installed(&file.host_path, file.intent, expected_mode).is_ok() => Ok(
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

    fn reconcile_interrupted_replace(
        &mut self,
        node: &TransactionNode,
    ) -> Result<ReconcileResult, ExecError> {
        let file = self.file(node.id.as_str())?.clone();
        let FilePrecondition::Exact { size, sha256 } = file.precondition else {
            return Ok(ReconcileResult::Ambiguous);
        };
        let Ok(transaction) = self.transaction_id() else {
            return Ok(ReconcileResult::Ambiguous);
        };
        let (directory, name) =
            crate::fs::open_parent(&file.host_path).map_err(|error| match error {
                PathError::Io { source, .. } if source.kind() == std::io::ErrorKind::NotFound => {
                    ExecError::from(PathError::Missing {
                        path: file.host_path.display().to_string(),
                    })
                }
                other => ExecError::from(other),
            })?;
        let backup_name = backup_name(&transaction, node.id.as_str(), "backup");
        let key = mutation_key(node)?;
        let staged_name = staged_name(&OperationId::stage_file(&key));
        let new_mode = self.executable_mode_for(file.intent);
        let backup_old = match directory.stream_regular(&backup_name) {
            Ok(backup) if backup.size == size && backup.sha256 == sha256 => Some(backup.mode),
            _ => None,
        };
        let staged = directory.stream_regular(&staged_name).ok();
        let staged_old = staged
            .as_ref()
            .is_some_and(|staged| staged.size == size && staged.sha256 == sha256);
        let staged_new = staged.as_ref().is_some_and(|staged| {
            staged.size == file.intent.size && staged.sha256 == file.intent.sha256
        });
        match observe(&file.host_path)? {
            None => match (backup_old, staged_new) {
                (Some(previous_mode), true) => {
                    if directory
                        .chmod_regular(&staged_name, mode_from(new_mode))
                        .is_err()
                    {
                        return Ok(ReconcileResult::Ambiguous);
                    }
                    match directory.rename_noreplace(&staged_name, &name) {
                        Ok(()) => {}
                        Err(_) => return Ok(ReconcileResult::Ambiguous),
                    }
                    match verify_installed(&file.host_path, file.intent, new_mode) {
                        Ok(()) => Ok(ReconcileResult::AppliedWithReceipt(
                            OperationReceipt::ReplaceFile {
                                destination: file.host_path.display().to_string(),
                                previous_sha256: sha256.to_hex(),
                                previous_size: size,
                                previous_mode,
                                backup_path: directory
                                    .path()
                                    .join(&backup_name)
                                    .display()
                                    .to_string(),
                                new_sha256: file.intent.sha256.to_hex(),
                                new_size: file.intent.size,
                                executable: file.intent.executable,
                            },
                        )),
                        Err(_) => Ok(ReconcileResult::Ambiguous),
                    }
                }
                (Some(previous_mode), false) => {
                    match directory.rename_noreplace(&backup_name, &name) {
                        Ok(()) => {}
                        Err(_) => return Ok(ReconcileResult::Ambiguous),
                    }
                    match verify_previous(&file.host_path, size, sha256, previous_mode) {
                        Ok(()) => Ok(ReconcileResult::NotApplied),
                        Err(_) => Ok(ReconcileResult::Ambiguous),
                    }
                }
                _ => Ok(ReconcileResult::NotApplied),
            },
            Some(_) if verify_installed(&file.host_path, file.intent, new_mode).is_ok() => {
                if let Some(previous_mode) = backup_old {
                    return Ok(ReconcileResult::AppliedWithReceipt(
                        OperationReceipt::ReplaceFile {
                            destination: file.host_path.display().to_string(),
                            previous_sha256: sha256.to_hex(),
                            previous_size: size,
                            previous_mode,
                            backup_path: directory.path().join(&backup_name).display().to_string(),
                            new_sha256: file.intent.sha256.to_hex(),
                            new_size: file.intent.size,
                            executable: file.intent.executable,
                        },
                    ));
                }
                if staged_old {
                    match directory.rename_noreplace(&staged_name, &backup_name) {
                        Ok(()) => {}
                        Err(_) => return Ok(ReconcileResult::Ambiguous),
                    }
                    let backup = directory.stream_regular(&backup_name).map_err(|_| {
                        ExecError::RollbackDrift {
                            path: file.host_path.display().to_string(),
                            reason: "the backup cannot be confirmed".into(),
                        }
                    })?;
                    return Ok(ReconcileResult::AppliedWithReceipt(
                        OperationReceipt::ReplaceFile {
                            destination: file.host_path.display().to_string(),
                            previous_sha256: sha256.to_hex(),
                            previous_size: size,
                            previous_mode: backup.mode,
                            backup_path: directory.path().join(&backup_name).display().to_string(),
                            new_sha256: file.intent.sha256.to_hex(),
                            new_size: file.intent.size,
                            executable: file.intent.executable,
                        },
                    ));
                }
                Ok(ReconcileResult::Ambiguous)
            }
            Some(_) => Ok(ReconcileResult::Ambiguous),
        }
    }

    fn reconcile_interrupted_removal(
        &mut self,
        node: &TransactionNode,
    ) -> Result<ReconcileResult, ExecError> {
        let removal = removal_identity(node)?.clone();
        let file = self.file(node.id.as_str())?.clone();
        self.check_removal_target(&file, &removal)?;
        let Ok(transaction) = self.transaction_id() else {
            return Ok(ReconcileResult::Ambiguous);
        };
        let (directory, _) =
            crate::fs::open_parent(&file.host_path).map_err(|error| match error {
                PathError::Io { source, .. } if source.kind() == std::io::ErrorKind::NotFound => {
                    ExecError::from(PathError::Missing {
                        path: file.host_path.display().to_string(),
                    })
                }
                other => ExecError::from(other),
            })?;
        let backup_name = backup_name(&transaction, node.id.as_str(), "removed");
        let backup_mode = match directory.stream_regular(&backup_name) {
            Ok(backup) if backup.size == removal.size && backup.sha256 == removal.sha256 => {
                Some(backup.mode)
            }
            _ => None,
        };
        match observe(&file.host_path)? {
            None => match backup_mode {
                Some(mode) => Ok(ReconcileResult::AppliedWithReceipt(
                    OperationReceipt::RemoveFile {
                        destination: removal.destination.clone(),
                        backup_path: as_target(
                            &removal.destination,
                            &directory.path().join(&backup_name),
                        )?,
                        sha256: removal.sha256,
                        size: removal.size,
                        mode,
                    },
                )),
                None => Ok(ReconcileResult::Ambiguous),
            },
            Some((found_size, found)) if found_size == removal.size && found == removal.sha256 => {
                Ok(ReconcileResult::NotApplied)
            }
            Some(_) => Ok(ReconcileResult::Ambiguous),
        }
    }

    fn create(&mut self, node: &TransactionNode) -> Result<OperationReceipt, ExecError> {
        let file = self.file(node.id.as_str())?.clone();
        if file.precondition != FilePrecondition::Absent {
            return Err(ExecError::PlanDrift {
                path: file.host_path.display().to_string(),
                reason: "a create must require an absent destination".into(),
            });
        }
        let key = mutation_key(node)?;
        let staged_name = staged_name(&OperationId::stage_file(&key));
        let mode = file.intent.mode_for(self.executable_mode);

        let mut created_directories = self
            .staged_dirs
            .remove(&file.host_path.display().to_string())
            .unwrap_or_default();
        let (directory, name, created) =
            crate::fs::ensure_parent(&file.host_path, self.install_dir_mode)?;
        created_directories.extend(created);
        created_directories.sort();
        created_directories.dedup();
        match directory.kind_or_absent(&name)? {
            None => {}
            Some(_) => {
                return Err(PathError::AlreadyExists {
                    path: file.host_path.display().to_string(),
                }
                .into());
            }
        }
        self.ensure_staged(node, &file, &directory, &staged_name)?;
        directory.chmod_regular(&staged_name, mode)?;
        directory.publish_exclusive(&name, &staged_name)?;

        let expected_mode = (mode).bits() & 0o7777;
        verify_installed(&file.host_path, file.intent, expected_mode)?;
        Ok(OperationReceipt::CreateFile {
            destination: file.host_path.display().to_string(),
            installed_sha256: file.intent.sha256.to_hex(),
            installed_size: file.intent.size,
            executable: file.intent.executable,
            created_directories,
        })
    }

    fn replace(&mut self, node: &TransactionNode) -> Result<OperationReceipt, ExecError> {
        let file = self.file(node.id.as_str())?.clone();
        let FilePrecondition::Exact { size, sha256 } = file.precondition else {
            return Err(ExecError::PlanDrift {
                path: file.host_path.display().to_string(),
                reason: "a replace must state the exact previous state".into(),
            });
        };
        let key = mutation_key(node)?;
        let staged_name = staged_name(&OperationId::stage_file(&key));
        let mode = file.intent.mode_for(self.executable_mode);
        let transaction = self.transaction_id()?;
        let backup_name = backup_name(&transaction, node.id.as_str(), "backup");

        let (directory, name) = crate::fs::open_parent(&file.host_path)?;
        let previous = stream_expected(&directory, &name, &file.host_path, size, sha256)?;
        occupy_backup_slot(&directory, &backup_name, &file.host_path)?;

        self.ensure_staged(node, &file, &directory, &staged_name)?;
        directory.chmod_regular(&staged_name, mode)?;
        directory.sync()?;
        let staged_stat = directory.stat_child(&staged_name)?;

        if directory.try_exchange(&staged_name, &name)? {
            directory.sync()?;
            let moved = stream_expected(
                &directory,
                &name,
                &file.host_path,
                file.intent.size,
                file.intent.sha256,
            )
            .map_err(|error| match error {
                ExecError::PlanDrift { path, reason } => ExecError::Verification { path, reason },
                other => other,
            })?;
            if moved.dev != staged_stat.dev || moved.ino != staged_stat.ino {
                let _ = directory.try_exchange(&staged_name, &name);
                return Err(ExecError::Verification {
                    path: file.host_path.display().to_string(),
                    reason: "the destination changed while it was replaced".into(),
                });
            }
            let displaced = match directory.stream_regular(&staged_name) {
                Ok(displaced) => displaced,
                Err(_) => {
                    let _ = directory.try_exchange(&staged_name, &name);
                    return Err(ExecError::Verification {
                        path: file.host_path.display().to_string(),
                        reason: "the replaced entry cannot be confirmed".into(),
                    });
                }
            };
            if displaced.dev != previous.dev
                || displaced.ino != previous.ino
                || displaced.size != size
                || displaced.sha256 != sha256
            {
                let _ = directory.try_exchange(&staged_name, &name);
                return Err(ExecError::Verification {
                    path: file.host_path.display().to_string(),
                    reason: "the destination changed while it was replaced".into(),
                });
            }
            match directory.rename_noreplace(&staged_name, &backup_name) {
                Ok(()) => {}
                Err(error) => {
                    return Err(match error {
                        PathError::AlreadyExists { .. } => ExecError::Verification {
                            path: file.host_path.display().to_string(),
                            reason: "the backup slot is occupied".into(),
                        },
                        other => other.into(),
                    });
                }
            }
        } else {
            Quarantine {
                directory: &directory,
                display: &file.host_path,
            }
            .forward_replace(
                &name,
                &staged_name,
                &backup_name,
                &previous,
                file.intent,
                mode.bits() & 0o7777,
            )?;
        }

        let backup_path = directory.path().join(&backup_name);
        verify_installed(&file.host_path, file.intent, mode.bits() & 0o7777)?;
        if !backup_holds(&directory, &backup_name, size, sha256) {
            return Err(ExecError::Verification {
                path: file.host_path.display().to_string(),
                reason: "the replacement applied but its backup cannot be confirmed".into(),
            });
        }

        Ok(OperationReceipt::ReplaceFile {
            destination: file.host_path.display().to_string(),
            previous_sha256: sha256.to_hex(),
            previous_size: size,
            previous_mode: previous.mode,
            backup_path: backup_path.display().to_string(),
            new_sha256: file.intent.sha256.to_hex(),
            new_size: file.intent.size,
            executable: file.intent.executable,
        })
    }

    fn remove(&mut self, node: &TransactionNode) -> Result<OperationReceipt, ExecError> {
        let file = self.file(node.id.as_str())?.clone();
        let removal = removal_identity(node)?.clone();
        self.check_removal_target(&file, &removal)?;
        let (directory, name) = crate::fs::open_parent(&file.host_path)?;
        let transaction = self.transaction_id()?;
        let backup_name = backup_name(&transaction, node.id.as_str(), "removed");
        match directory.kind_or_absent(&name)? {
            None => {
                if backup_holds(&directory, &backup_name, removal.size, removal.sha256) {
                    let backup_path = directory.path().join(&backup_name);
                    let backup = directory.stream_regular(&backup_name).map_err(|_| {
                        ExecError::Verification {
                            path: file.host_path.display().to_string(),
                            reason: "the backup cannot be confirmed".into(),
                        }
                    })?;
                    return Ok(OperationReceipt::RemoveFile {
                        destination: file.destination.clone(),
                        backup_path: as_target(&file.destination, &backup_path)?,
                        sha256: removal.sha256,
                        size: removal.size,
                        mode: backup.mode,
                    });
                }
                return Err(ExecError::from(PathError::Missing {
                    path: file.host_path.display().to_string(),
                }));
            }
            Some(EntryKind::Regular) => {}
            Some(_) => {
                return Err(ExecError::PlanDrift {
                    path: file.host_path.display().to_string(),
                    reason: "the destination is no longer a regular file".into(),
                });
            }
        }
        let previous = stream_expected(
            &directory,
            &name,
            &file.host_path,
            removal.size,
            removal.sha256,
        )?;

        directory
            .rename_noreplace(&name, &backup_name)
            .map_err(|error| match error {
                PathError::AlreadyExists { .. } => ExecError::Verification {
                    path: file.host_path.display().to_string(),
                    reason: "the backup slot is occupied".into(),
                },
                other => other.into(),
            })?;
        let quarantined =
            directory
                .stream_regular(&backup_name)
                .map_err(|_| ExecError::Verification {
                    path: file.host_path.display().to_string(),
                    reason: "the removed entry cannot be confirmed".into(),
                })?;
        if quarantined.dev != previous.dev
            || quarantined.ino != previous.ino
            || quarantined.size != removal.size
            || quarantined.sha256 != removal.sha256
        {
            let _ = directory.rename_noreplace(&backup_name, &name);
            return Err(ExecError::Verification {
                path: file.host_path.display().to_string(),
                reason: "the destination changed while it was removed".into(),
            });
        }
        match directory.kind_or_absent(&name)? {
            None => {}
            Some(_) => {
                let _ = directory.rename_noreplace(&backup_name, &name);
                return Err(ExecError::Verification {
                    path: file.host_path.display().to_string(),
                    reason: "the destination changed while it was removed".into(),
                });
            }
        }

        let backup_path = directory.path().join(&backup_name);
        Ok(OperationReceipt::RemoveFile {
            destination: file.destination.clone(),

            backup_path: as_target(&file.destination, &backup_path)?,
            sha256: removal.sha256,
            size: removal.size,
            mode: quarantined.mode,
        })
    }

    fn ensure_staged(
        &mut self,
        node: &TransactionNode,
        file: &FileWork,
        directory: &OwnedDirectory,
        staged_name: &str,
    ) -> Result<(), ExecError> {
        if let Ok(staged) = directory.stream_regular(staged_name)
            && staged.size == file.intent.size
            && staged.sha256 == file.intent.sha256
        {
            return Ok(());
        }
        let (source, sha256, size) = Self::payload_identity(node)?;
        let payload = self.payload.as_ref().ok_or_else(|| ExecError::PlanDrift {
            path: node.id.to_string(),
            reason: "no payload source is attached".into(),
        })?;
        let mut reader =
            payload
                .open(&source, &sha256, size)
                .map_err(|error| ExecError::Payload {
                    path: source.to_string(),
                    source: error,
                })?;
        let (found_size, found) = directory
            .write_stream_durable(staged_name, &mut reader, size, crate::fs::STATE_FILE_MODE)
            .map_err(|error| match error {
                PathError::Refused { path, reason } => ExecError::Payload {
                    path: source.to_string(),
                    source: PayloadError::Read {
                        path,
                        source: std::io::Error::other(reason),
                    },
                },
                other => other.into(),
            })?;
        if found_size != file.intent.size
            || found != file.intent.sha256
            || found_size != size
            || found != sha256
        {
            return Err(ExecError::Payload {
                path: source.to_string(),
                source: PayloadError::DigestMismatch {
                    path: source.to_string(),
                },
            });
        }
        Ok(())
    }

    fn discard_staged(&mut self, node: &TransactionNode) {
        let Ok(file) = self.file(node.id.as_str()).cloned() else {
            return;
        };
        let key = match &node.kind {
            NodeKind::StageFile { key } => key.clone(),
            NodeKind::FileMutation { key, .. } => key.clone(),
            _ => return,
        };
        let staged_name = staged_name(&OperationId::stage_file(&key));
        let Ok((directory, _)) = crate::fs::open_parent(&file.host_path) else {
            return;
        };
        if let Ok(staged) = directory.stream_regular(&staged_name)
            && staged.size == file.intent.size
            && staged.sha256 == file.intent.sha256
        {
            let _ = directory.remove_file(&staged_name);
            let _ = directory.sync();
        }
    }

    fn check_removal_target(
        &self,
        file: &FileWork,
        removal: &FileRemoval,
    ) -> Result<(), ExecError> {
        if removal.kind != FileRemovalKind::RemoveOwned {
            return Err(ExecError::PlanDrift {
                path: file.host_path.display().to_string(),
                reason: "a removal without owned content is never unlinked".into(),
            });
        }
        let host = to_host_path(&removal.destination).map_err(|error| ExecError::PlanDrift {
            path: removal.destination.to_string(),
            reason: error.to_string(),
        })?;
        if host != file.host_path {
            return Err(ExecError::PlanDrift {
                path: file.host_path.display().to_string(),
                reason: "the removal authorizes a different file".into(),
            });
        }
        Ok(())
    }
}

fn refresh_request(node: &TransactionNode) -> Result<crate::refresh::RefreshRequest, ExecError> {
    let missing = || ExecError::PlanDrift {
        path: node.id.to_string(),
        reason: "a refresh node without its request".into(),
    };
    let backend = node.meta.backend.clone().ok_or_else(missing)?;
    let request = crate::refresh::RefreshRequest::decode(&backend.payload).map_err(|error| {
        ExecError::PlanDrift {
            path: node.id.to_string(),
            reason: error.to_string(),
        }
    })?;
    if backend.key != request.key() || backend.id != request.backend_id() {
        return Err(ExecError::PlanDrift {
            path: node.id.to_string(),
            reason: "a refresh node whose identity is not its request".into(),
        });
    }
    Ok(request)
}

impl OperationExecutor for LinuxFileExecutor {
    type Error = ExecError;

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

fn verify_precondition(
    destination: &Path,
    size: u64,
    sha256: Sha256Digest,
) -> Result<(), ExecError> {
    let Some((found_size, found)) = observe(destination)? else {
        return Err(PathError::Missing {
            path: destination.display().to_string(),
        }
        .into());
    };
    if found_size != size || found != sha256 {
        return Err(ExecError::PlanDrift {
            path: destination.display().to_string(),
            reason: format!("expected {size} bytes / {sha256}, found {found_size} bytes / {found}"),
        });
    }
    Ok(())
}

fn verify_installed(
    destination: &Path,
    intent: FileIntent,
    expected_mode: u32,
) -> Result<(), ExecError> {
    let (directory, name) = crate::fs::open_parent(destination)?;
    let current = directory
        .stream_regular(&name)
        .map_err(|_| ExecError::Verification {
            path: destination.display().to_string(),
            reason: "the published file is not there".into(),
        })?;
    if current.size != intent.size || current.sha256 != intent.sha256 {
        return Err(ExecError::Verification {
            path: destination.display().to_string(),
            reason: format!(
                "expected {} bytes / {}, found {} bytes / {}",
                intent.size, intent.sha256, current.size, current.sha256,
            ),
        });
    }
    if current.mode != expected_mode & 0o7777 {
        return Err(ExecError::Verification {
            path: destination.display().to_string(),
            reason: format!(
                "expected mode {:o}, found {:o}",
                expected_mode & 0o7777,
                current.mode
            ),
        });
    }
    Ok(())
}

fn verify_previous(
    path: &Path,
    size: u64,
    sha256: Sha256Digest,
    mode: u32,
) -> Result<(), ExecError> {
    verify_installed(
        path,
        FileIntent {
            sha256,
            size,
            executable: mode & 0o111 != 0,
        },
        mode,
    )
}

fn is_absent(error: &PathError) -> bool {
    error.is_not_found()
}

fn observe(destination: &Path) -> Result<Option<(u64, Sha256Digest)>, ExecError> {
    let (directory, name) = match crate::fs::open_parent(destination) {
        Ok(opened) => opened,
        Err(error) if is_absent(&error) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    match directory.kind_or_absent(&name)? {
        None => Ok(None),
        Some(EntryKind::Regular) => {
            let current = directory.stream_regular(&name)?;
            Ok(Some((current.size, current.sha256)))
        }
        Some(EntryKind::Symlink) => Err(PathError::UnexpectedKind {
            path: destination.display().to_string(),
            expected: "regular file, not a symbolic link",
        }
        .into()),
        Some(_) => Err(PathError::UnexpectedKind {
            path: destination.display().to_string(),
            expected: "regular file",
        }
        .into()),
    }
}

fn removal_identity(node: &TransactionNode) -> Result<&FileRemoval, ExecError> {
    node.meta
        .removal
        .as_ref()
        .ok_or_else(|| ExecError::PlanDrift {
            path: node.id.to_string(),
            reason: "a removal without the identity of the file it may unlink".into(),
        })
}

fn stream_expected(
    directory: &OwnedDirectory,
    name: &str,
    display: &Path,
    size: u64,
    sha256: Sha256Digest,
) -> Result<StreamedRegular, ExecError> {
    let current = directory
        .stream_regular(name)
        .map_err(|error| match error {
            PathError::UnexpectedKind { .. } => ExecError::from(error),
            _ => ExecError::PlanDrift {
                path: display.display().to_string(),
                reason: "the required file is not a regular file".into(),
            },
        })?;
    if current.size != size || current.sha256 != sha256 {
        return Err(ExecError::PlanDrift {
            path: display.display().to_string(),
            reason: format!(
                "expected {size} bytes / {sha256}, found {} bytes / {}",
                current.size, current.sha256,
            ),
        });
    }
    Ok(current)
}

fn backup_holds(directory: &OwnedDirectory, backup: &str, size: u64, sha256: Sha256Digest) -> bool {
    match directory.stream_regular(backup) {
        Ok(current) => current.size == size && current.sha256 == sha256,
        Err(_) => false,
    }
}

fn occupy_backup_slot(
    directory: &OwnedDirectory,
    backup: &str,
    display: &Path,
) -> Result<(), ExecError> {
    match directory.stat_child(backup) {
        Err(error) if is_absent(&error) => Ok(()),
        Err(error) => Err(error.into()),
        Ok(_) => Err(ExecError::Verification {
            path: display.display().to_string(),
            reason: "the backup slot is occupied".into(),
        }),
    }
}

struct Quarantine<'a> {
    directory: &'a OwnedDirectory,
    display: &'a Path,
}

impl Quarantine<'_> {
    fn forward_replace(
        &self,
        name: &str,
        staged: &str,
        backup: &str,
        previous: &StreamedRegular,
        new: FileIntent,
        new_mode: u32,
    ) -> Result<(), ExecError> {
        let display = self.display.display().to_string();
        let verification = |reason: &str| ExecError::Verification {
            path: display.clone(),
            reason: reason.into(),
        };
        self.directory
            .rename_noreplace(name, backup)
            .map_err(|error| match error {
                PathError::AlreadyExists { .. } => verification("the backup slot is occupied"),
                other => other.into(),
            })?;
        let moved = self
            .directory
            .stream_regular(backup)
            .map_err(|_| verification("the replaced entry cannot be confirmed"))?;
        if moved.dev != previous.dev
            || moved.ino != previous.ino
            || moved.size != previous.size
            || moved.sha256 != previous.sha256
        {
            let _ = self.directory.rename_noreplace(backup, name);
            return Err(verification(
                "the destination changed while it was replaced",
            ));
        }
        match self.directory.kind_or_absent(name)? {
            None => {}
            Some(_) => {
                let _ = self.directory.rename_noreplace(backup, name);
                return Err(verification(
                    "the destination changed while it was replaced",
                ));
            }
        }
        self.directory
            .rename_noreplace(staged, name)
            .map_err(|error| match error {
                PathError::AlreadyExists { .. } => {
                    verification("the destination reappeared while it was replaced")
                }
                other => other.into(),
            })?;
        verify_installed(self.display, new, new_mode)
    }

    fn reverse_restore(
        &self,
        name: &str,
        backup: &str,
        temporary: &str,
        new_size: u64,
        new_sha256: Sha256Digest,
    ) -> Result<(), ExecError> {
        let drift = |reason: &str| ExecError::RollbackDrift {
            path: self.display.display().to_string(),
            reason: reason.into(),
        };
        // A name and a file type never prove a previous attempt parked this
        // slot, so any occupant fails closed. The NOREPLACE rename below is
        // the atomic backstop for a squat racing this check.
        match self.directory.kind_or_absent(temporary)? {
            None => {}
            Some(_) => return Err(drift("the rollback slot is occupied")),
        }
        let dest_stat = self
            .directory
            .stat_child(name)
            .map_err(|_| drift("the destination cannot be read"))?;
        self.directory
            .rename_noreplace(name, temporary)
            .map_err(|error| match error {
                PathError::AlreadyExists { .. } => drift("the rollback slot is occupied"),
                _ => drift("the destination cannot be parked"),
            })?;
        let parked = self
            .directory
            .stream_regular(temporary)
            .map_err(|_| drift("the parked entry cannot be confirmed"))?;
        if parked.dev != dest_stat.dev
            || parked.ino != dest_stat.ino
            || parked.size != new_size
            || parked.sha256 != new_sha256
        {
            let _ = self.directory.rename_noreplace(temporary, name);
            return Err(drift("the destination changed during rollback"));
        }
        self.directory
            .rename_noreplace(backup, name)
            .map_err(|error| match error {
                PathError::AlreadyExists { .. } => {
                    drift("the destination reappeared during rollback")
                }
                _ => drift("the backup cannot be restored"),
            })?;
        // The parked entry was created by the NOREPLACE rename above and its
        // identity re-verified; unlinkat cannot condition on identity, so this
        // trusts the pinned directory like every other mutation does.
        let _ = self.directory.remove_file(temporary);
        let _ = self.directory.sync();
        Ok(())
    }
}

fn mode_from(mode: u32) -> rustix::fs::Mode {
    rustix::fs::Mode::from_bits_truncate(mode & 0o7777)
}

pub(crate) fn backup_name(transaction: &TransactionId, id: &str, prefix: &str) -> String {
    format!(
        "{prefix}-{}-{}.bin",
        short_digest(&transaction.to_string()),
        short_digest(id)
    )
}

fn rollback_temp_name(transaction: &TransactionId, id: &str) -> String {
    format!(
        "rollback-{}-{}.tmp",
        short_digest(&transaction.to_string()),
        short_digest(id)
    )
}

fn check_backup(
    directory: &OwnedDirectory,
    destination: &Path,
    backup_path: &str,
    transaction: &TransactionId,
    node_id: &str,
    prefix: &str,
) -> Result<String, ExecError> {
    check_backup_host(
        directory,
        destination,
        Path::new(backup_path),
        transaction,
        node_id,
        prefix,
    )
}

fn check_backup_host(
    directory: &OwnedDirectory,
    destination: &Path,
    backup: &Path,
    transaction: &TransactionId,
    node_id: &str,
    prefix: &str,
) -> Result<String, ExecError> {
    let expected = backup_name(transaction, node_id, prefix);
    let drift = |reason: &str| ExecError::RollbackDrift {
        path: backup.display().to_string(),
        reason: reason.into(),
    };
    let Some(parent) = backup
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    else {
        return Err(drift("a journal backup names its directory"));
    };
    let Some(leaf) = backup
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
    else {
        return Err(drift("a journal backup names its file"));
    };
    let Some(want) = destination
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    else {
        return Err(drift("a journal backup sits beside its file"));
    };
    if parent != want || leaf != expected {
        return Err(drift("a journal backup is not this operation's backup"));
    }
    let _ = directory;
    Ok(leaf)
}

enum Current {
    Absent,
    MatchesPrevious,
    MatchesNew,
    Foreign,
}

fn current_state(
    directory: &OwnedDirectory,
    name: &str,
    display: &Path,
    previous_size: u64,
    previous_sha256: Sha256Digest,
    new_size: u64,
    new_sha256: Sha256Digest,
) -> Result<Current, ExecError> {
    match directory.kind_or_absent(name)? {
        None => Ok(Current::Absent),
        Some(EntryKind::Regular) => {
            let current = directory
                .stream_regular(name)
                .map_err(|_| ExecError::Verification {
                    path: display.display().to_string(),
                    reason: "the file cannot be read".into(),
                })?;
            if current.size == previous_size && current.sha256 == previous_sha256 {
                Ok(Current::MatchesPrevious)
            } else if current.size == new_size && current.sha256 == new_sha256 {
                Ok(Current::MatchesNew)
            } else {
                Ok(Current::Foreign)
            }
        }
        Some(_) => Ok(Current::Foreign),
    }
}

enum Restored {
    Already,
    Absent,
    Foreign,
}

fn removed_state(
    directory: &OwnedDirectory,
    name: &str,
    display: &Path,
    size: u64,
    sha256: Sha256Digest,
) -> Result<Restored, ExecError> {
    match directory.kind_or_absent(name)? {
        None => Ok(Restored::Absent),
        Some(EntryKind::Regular) => {
            let current = directory
                .stream_regular(name)
                .map_err(|_| ExecError::Verification {
                    path: display.display().to_string(),
                    reason: "the file cannot be read".into(),
                })?;
            if current.size == size && current.sha256 == sha256 {
                Ok(Restored::Already)
            } else {
                Ok(Restored::Foreign)
            }
        }
        Some(_) => Ok(Restored::Foreign),
    }
}

fn receipt_backup_valid(receipt: &OperationReceipt) -> bool {
    match receipt {
        OperationReceipt::CreateFile { .. }
        | OperationReceipt::Control
        | OperationReceipt::StageFile { .. }
        | OperationReceipt::Backend { .. } => true,
        OperationReceipt::ReplaceFile {
            destination,
            backup_path,
            previous_sha256,
            previous_size,
            ..
        } => {
            let Ok(previous) = previous_sha256.parse() else {
                return false;
            };
            let path = PathBuf::from(destination);
            let backup = PathBuf::from(backup_path);
            let Some(parent) = backup
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
            else {
                return false;
            };
            let Ok(directory) = OwnedDirectory::open(parent) else {
                return false;
            };
            let Some(leaf) = backup
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
            else {
                return false;
            };
            let _ = path;
            backup_holds(&directory, &leaf, *previous_size, previous)
        }
        OperationReceipt::RemoveFile {
            backup_path,
            sha256,
            size,
            ..
        } => {
            let Ok(backup) = to_host_path(backup_path) else {
                return false;
            };
            let Some(parent) = backup
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
            else {
                return false;
            };
            let Ok(directory) = OwnedDirectory::open(parent) else {
                return false;
            };
            let Some(leaf) = backup
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
            else {
                return false;
            };
            backup_holds(&directory, &leaf, *size, *sha256)
        }
    }
}

fn mutation_key(node: &TransactionNode) -> Result<ResourceKey, ExecError> {
    match &node.kind {
        NodeKind::FileMutation { key, .. } => Ok(key.clone()),
        _ => Err(ExecError::PlanDrift {
            path: node.id.to_string(),
            reason: "a file mutation without its resource".into(),
        }),
    }
}

fn as_target(
    anchor: &zup_platform::TargetPath,
    host_path: &Path,
) -> Result<zup_platform::TargetPath, ExecError> {
    zup_platform::TargetPath::new(*anchor.target(), host_path.to_string_lossy().as_ref()).map_err(
        |error| ExecError::PlanDrift {
            path: host_path.display().to_string(),
            reason: error.to_string(),
        },
    )
}

fn staged_name(id: &OperationId) -> String {
    format!("staged-{}", short_digest(id.as_str()))
}

fn short_digest(value: &str) -> String {
    let (_, digest) = hash_reader(value.as_bytes()).expect("a string hashes");
    digest.to_hex()[..16].to_owned()
}

fn is_service_node(node: &TransactionNode) -> bool {
    node.meta.backend.as_ref().is_some_and(|backend| {
        backend
            .id
            .as_str()
            .starts_with(crate::service_ops::SERVICE_BACKEND_PREFIX)
    })
}

fn is_service_key(key: &ResourceKey) -> bool {
    match key {
        ResourceKey::Backend { id } => id
            .as_str()
            .starts_with(crate::service_ops::SERVICE_BACKEND_PREFIX),
        _ => false,
    }
}

impl LinuxFileExecutor {
    fn service_context(&mut self) -> Result<crate::service_exec::ServiceContext<'_>, ExecError> {
        let Some(support) = self.services.as_mut() else {
            return Err(ExecError::PlanDrift {
                path: "<service>".into(),
                reason: "a service operation without service support".into(),
            });
        };
        Ok(crate::service_exec::ServiceContext {
            roots: &support.roots,
            systemd: &support.systemd,
            manager: support.manager.as_mut(),
            expected_uid: support.expected_uid,
        })
    }

    fn prepare_service(&mut self, node: &TransactionNode) -> Result<(), ExecError> {
        use crate::service_ops::ServicePayload;
        let payload = crate::service_exec::payload_for(node)?;
        match payload {
            ServicePayload::Apply { service, .. } => {
                let context = self.service_context()?;

                let derived = service_unit_for(&service)?;
                let canonical = crate::machine::authorize_systemd_unit(&derived, context.systemd)
                    .map_err(|error| ExecError::PlanDrift {
                    path: derived.clone(),
                    reason: error.to_string(),
                })?;
                crate::service_ops::refuse_source_symlink(&derived, &canonical)?;
                crate::service_ops::check_collisions(
                    &derived,
                    &canonical,
                    &crate::service_ops::load_path_dirs(),
                )?;

                let manager = context.manager;
                crate::service_ops::require_exec_baseline(manager).map_err(|error| {
                    ExecError::PlanDrift {
                        path: derived.clone(),
                        reason: error.to_string(),
                    }
                })?;
                manager
                    .unit_file_state(&derived)
                    .map_err(|error| ExecError::PlanDrift {
                        path: derived.clone(),
                        reason: format!("systemd is unavailable: {error}"),
                    })?;
                Ok(())
            }
            ServicePayload::Remove { unit, .. } => {
                let context = self.service_context()?;
                let canonical = crate::machine::authorize_systemd_unit(&unit, context.systemd)
                    .map_err(|error| ExecError::PlanDrift {
                        path: unit.clone(),
                        reason: error.to_string(),
                    })?;
                crate::service_ops::refuse_source_symlink(&unit, &canonical)?;
                let manager = context.manager;
                manager
                    .unit_file_state(&unit)
                    .map_err(|error| ExecError::PlanDrift {
                        path: unit.clone(),
                        reason: format!("systemd is unavailable: {error}"),
                    })?;
                Ok(())
            }
        }
    }

    fn apply_service(&mut self, node: &TransactionNode) -> Result<OperationReceipt, ExecError> {
        use crate::service_ops::ServicePayload;
        let payload = crate::service_exec::payload_for(node)?;
        match payload {
            ServicePayload::Apply { service, .. } => {
                let backend = node
                    .meta
                    .backend
                    .clone()
                    .expect("a service node holds a payload");
                let mut context = self.service_context()?;
                let receipt = crate::service_exec::apply(&service, &backend.payload, &mut context)?;
                Ok(OperationReceipt::Backend {
                    key: backend.key,
                    payload: serde_json::to_vec(&receipt).map_err(|_| ExecError::PlanDrift {
                        path: node.id.to_string(),
                        reason: "a service receipt does not serialize".into(),
                    })?,
                })
            }
            ServicePayload::Remove { key, unit, owned } => {
                let backend = node
                    .meta
                    .backend
                    .clone()
                    .expect("a service node holds a payload");
                let mut context = self.service_context()?;
                let receipt = crate::service_exec::apply_remove(&key, &unit, &owned, &mut context)?;
                Ok(OperationReceipt::Backend {
                    key: backend.key,
                    payload: serde_json::to_vec(&receipt).map_err(|_| ExecError::PlanDrift {
                        path: node.id.to_string(),
                        reason: "a service receipt does not serialize".into(),
                    })?,
                })
            }
        }
    }

    fn verify_service_receipt(
        &mut self,
        key: &ResourceKey,
        payload: &[u8],
    ) -> Result<(), ExecError> {
        let receipt: crate::service_ops::ServiceReceipt =
            serde_json::from_slice(payload).map_err(|_| ExecError::Verification {
                path: format!("{key:?}"),
                reason: "a service receipt does not parse".into(),
            })?;
        let mut context = self.service_context()?;
        if receipt.installed_source_sha256.is_none() {
            crate::service_exec::verify_remove_receipt(&receipt, &mut context)?;
        } else {
            crate::service_exec::verify_receipt(&receipt, &mut context)?;
        }
        Ok(())
    }

    fn rollback_service(
        &mut self,
        node: &TransactionNode,
        key: &ResourceKey,
        payload: &[u8],
    ) -> Result<(), ExecError> {
        use crate::service_ops::ServicePayload;
        let receipt: crate::service_ops::ServiceReceipt =
            serde_json::from_slice(payload).map_err(|_| ExecError::RollbackDrift {
                path: format!("{key:?}"),
                reason: "a service receipt does not parse".into(),
            })?;
        let operation = crate::service_exec::payload_for(node)?;
        let mut context = self.service_context()?;
        match &operation {
            ServicePayload::Apply { .. } => {
                crate::service_exec::rollback_apply(&operation, &receipt, &mut context)?;
                Ok(())
            }
            ServicePayload::Remove { key, unit, owned } => {
                crate::service_exec::rollback_remove(key, unit, owned, &receipt, &mut context)?;
                Ok(())
            }
        }
    }

    fn reconcile_service(
        &mut self,
        node: &TransactionNode,
        receipt: Option<&OperationReceipt>,
    ) -> Result<ReconcileResult, ExecError> {
        use crate::service_ops::ServicePayload;
        let operation = crate::service_exec::payload_for(node)?;
        let decoded = receipt
            .map(|receipt| match receipt {
                OperationReceipt::Backend { payload, .. } => serde_json::from_slice::<
                    crate::service_ops::ServiceReceipt,
                >(payload)
                .map_err(|_| ExecError::RollbackDrift {
                    path: node.id.to_string(),
                    reason: "a service receipt does not parse".into(),
                }),
                _ => Err(ExecError::RollbackDrift {
                    path: node.id.to_string(),
                    reason: "a service node without a service receipt".into(),
                }),
            })
            .transpose()?;

        let round_trip = |receipt: crate::service_ops::ServiceReceipt| {
            serde_json::to_vec(&receipt)
                .map(|payload| OperationReceipt::Backend {
                    key: node
                        .meta
                        .backend
                        .clone()
                        .expect("a service node holds a payload")
                        .key,
                    payload,
                })
                .map_err(|_| ExecError::RollbackDrift {
                    path: node.id.to_string(),
                    reason: "a service receipt does not serialize".into(),
                })
        };
        let mut context = self.service_context()?;
        match &operation {
            ServicePayload::Apply {
                service,
                previous_policy,
                ..
            } => {
                let (outcome, receipt) = crate::service_exec::reconcile_apply(
                    service,
                    previous_policy,
                    decoded.as_ref(),
                    &mut context,
                )?;
                Ok(match outcome {
                    crate::service_exec::ServiceReconcile::Applied => {
                        ReconcileResult::AppliedWithReceipt(round_trip(
                            receipt.expect("an applied reconciliation holds a receipt"),
                        )?)
                    }
                    crate::service_exec::ServiceReconcile::NotApplied => {
                        ReconcileResult::NotApplied
                    }
                    crate::service_exec::ServiceReconcile::Ambiguous => ReconcileResult::Ambiguous,
                })
            }
            ServicePayload::Remove { unit, owned, .. } => {
                let (outcome, receipt) = crate::service_exec::reconcile_remove(
                    unit,
                    owned,
                    decoded.as_ref(),
                    &mut context,
                )?;
                Ok(match outcome {
                    crate::service_exec::ServiceReconcile::Applied => {
                        ReconcileResult::AppliedWithReceipt(round_trip(
                            receipt.expect("an applied reconciliation holds a receipt"),
                        )?)
                    }
                    crate::service_exec::ServiceReconcile::NotApplied => {
                        ReconcileResult::NotApplied
                    }
                    crate::service_exec::ServiceReconcile::Ambiguous => ReconcileResult::Ambiguous,
                })
            }
        }
    }
}

fn service_unit_for(service: &zup_exec::ServiceOperation) -> Result<String, ExecError> {
    let id = zup_core::ServiceId::new(&service.id).map_err(|error| ExecError::PlanDrift {
        path: service.name.clone(),
        reason: format!("service id: {error}"),
    })?;
    crate::services::unit_name(&id).map_err(|error| ExecError::PlanDrift {
        path: service.name.clone(),
        reason: error.to_string(),
    })
}

pub fn snapshot_target(target: &zup_platform::TargetPlan) -> HostSnapshot {
    let mut snapshot = HostSnapshot::default();
    for file in &target.files {
        let state = observe_file(file);
        snapshot.files.push(ObservedFile {
            key: file.key.clone(),
            path: file.destination.clone(),
            state,
        });
    }
    snapshot
}

fn observe_file(file: &zup_platform::TargetFile) -> ObservedFileState {
    let host = match to_host_path(&file.destination) {
        Ok(host) => host,

        Err(_) => return ObservedFileState::NonFile,
    };
    let parent = match host
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        Some(parent) => parent,
        None => return ObservedFileState::NonFile,
    };
    let directory = match OwnedDirectory::open(parent) {
        Ok(directory) => directory,

        Err(_) => return ObservedFileState::Absent,
    };
    let name = match host
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
    {
        Some(name) if !name.is_empty() => name,
        _ => return ObservedFileState::NonFile,
    };
    match directory.kind_or_absent(&name) {
        Ok(None) => ObservedFileState::Absent,
        Ok(Some(EntryKind::Regular)) => match directory.stream_regular(&name) {
            Ok(current) => ObservedFileState::File {
                size: current.size,
                sha256: current.sha256,
            },
            Err(_) => ObservedFileState::NonFile,
        },

        Ok(Some(_)) => ObservedFileState::NonFile,
        Err(_) => ObservedFileState::NonFile,
    }
}

#[cfg(test)]
mod fallback_tests {
    use super::*;
    use zup_core::hash_reader;

    fn digest(bytes: &[u8]) -> Sha256Digest {
        hash_reader(bytes).expect("bytes hash").1
    }

    fn transaction() -> TransactionId {
        TransactionId::new_v7()
    }

    fn register_replace(
        executor: &mut LinuxFileExecutor,
        destination: &Path,
        old: &[u8],
        new: &[u8],
    ) {
        let target = zup_core::Target::parse("linux-x64").expect("a target");
        let host = |path: &Path| {
            zup_platform::TargetPath::new(target, path.to_string_lossy()).expect("a path")
        };
        executor.register(
            &OperationId::new("op-replace"),
            FileWork {
                destination: host(destination),
                host_path: destination.to_path_buf(),
                intent: FileIntent {
                    sha256: digest(new),
                    size: new.len() as u64,
                    executable: false,
                },
                precondition: FilePrecondition::Exact {
                    size: old.len() as u64,
                    sha256: digest(old),
                },
            },
        );
    }

    fn replace_node() -> TransactionNode {
        TransactionNode {
            id: OperationId::new("op-replace"),
            phase: zup_transaction::Phase::FileMutation,
            kind: NodeKind::FileMutation {
                key: ResourceKey::File {
                    destination: String::from("/tmp/whatever"),
                },
                delta: FileDelta::Replace,
            },
            declaration_order: 0,
            meta: Default::default(),
        }
    }

    fn plant_staged(install: &Path, key: &ResourceKey, bytes: &[u8]) {
        std::fs::write(install.join(staged_test_name(key)), bytes).expect("a staged file");
    }

    fn staged_test_name(key: &ResourceKey) -> String {
        format!(
            "staged-{}",
            short_digest(OperationId::stage_file(key).as_str())
        )
    }

    fn replace_key() -> ResourceKey {
        ResourceKey::File {
            destination: String::from("/tmp/whatever"),
        }
    }

    fn quarantine_of<'a>(directory: &'a OwnedDirectory, display: &'a Path) -> Quarantine<'a> {
        Quarantine { directory, display }
    }

    #[test]
    fn quarantine_forward_preserves_and_publishes_without_an_exchange() {
        let root = tempfile::tempdir().expect("a temp directory");
        let install = root.path().join("install");
        std::fs::create_dir_all(&install).expect("install");
        let destination = install.join("app.dat");
        std::fs::write(&destination, b"v1").expect("write v1");
        plant_staged(&install, &replace_key(), b"v2");

        let (directory, name) = crate::fs::open_parent(&destination).expect("the parent opens");
        let previous = stream_expected(&directory, &name, &destination, 2, digest(b"v1"))
            .expect("the previous file verifies");
        quarantine_of(&directory, &destination)
            .forward_replace(
                &name,
                &staged_test_name(&replace_key()),
                "backup-test",
                &previous,
                FileIntent {
                    sha256: digest(b"v2"),
                    size: 2,
                    executable: false,
                },
                0o644,
            )
            .expect("the quarantine publishes");
        assert_eq!(std::fs::read(&destination).expect("read"), b"v2");
        assert_eq!(
            std::fs::read(install.join("backup-test")).expect("read"),
            b"v1",
            "the quarantined entry holds the previous contents for rollback"
        );
    }

    #[test]
    fn quarantine_forward_detects_a_swapped_destination() {
        let root = tempfile::tempdir().expect("a temp directory");
        let install = root.path().join("install");
        std::fs::create_dir_all(&install).expect("install");
        let destination = install.join("app.dat");
        std::fs::write(&destination, b"v1").expect("write v1");
        plant_staged(&install, &replace_key(), b"v2");

        let (directory, name) = crate::fs::open_parent(&destination).expect("the parent opens");
        let previous = stream_expected(&directory, &name, &destination, 2, digest(b"v1"))
            .expect("the previous file verifies");
        std::fs::write(&destination, b"vX").expect("a concurrent swap");

        let error = quarantine_of(&directory, &destination)
            .forward_replace(
                &name,
                &staged_test_name(&replace_key()),
                "backup-test",
                &previous,
                FileIntent {
                    sha256: digest(b"v2"),
                    size: 2,
                    executable: false,
                },
                0o644,
            )
            .expect_err("a swapped destination never publishes");
        assert!(
            matches!(error, ExecError::Verification { .. }),
            "the refusal names the race: {error}"
        );
        assert_eq!(
            std::fs::read(&destination).expect("read"),
            b"vX",
            "the swapped file is restored, not overwritten"
        );
    }

    #[test]
    fn quarantine_forward_refuses_an_occupied_backup_slot() {
        let root = tempfile::tempdir().expect("a temp directory");
        let install = root.path().join("install");
        std::fs::create_dir_all(&install).expect("install");
        let destination = install.join("app.dat");
        std::fs::write(&destination, b"v1").expect("write v1");
        plant_staged(&install, &replace_key(), b"v2");
        std::fs::write(install.join("backup-test"), b"planted").expect("a plant");

        let (directory, name) = crate::fs::open_parent(&destination).expect("the parent opens");
        let previous = stream_expected(&directory, &name, &destination, 2, digest(b"v1"))
            .expect("the previous file verifies");
        assert!(
            quarantine_of(&directory, &destination)
                .forward_replace(
                    &name,
                    &staged_test_name(&replace_key()),
                    "backup-test",
                    &previous,
                    FileIntent {
                        sha256: digest(b"v2"),
                        size: 2,
                        executable: false,
                    },
                    0o644,
                )
                .is_err(),
            "an occupied slot fails closed"
        );
        assert_eq!(
            std::fs::read(install.join("backup-test")).expect("read"),
            b"planted",
            "the occupant is never deleted"
        );
        assert_eq!(
            std::fs::read(&destination).expect("read"),
            b"v1",
            "the destination is untouched"
        );
    }

    #[test]
    fn quarantine_reverse_restores_without_an_exchange() {
        let root = tempfile::tempdir().expect("a temp directory");
        let install = root.path().join("install");
        std::fs::create_dir_all(&install).expect("install");
        let destination = install.join("app.dat");
        std::fs::write(&destination, b"v2").expect("write v2");
        std::fs::write(install.join("backup-test"), b"v1").expect("a backup");

        let (directory, name) = crate::fs::open_parent(&destination).expect("the parent opens");
        quarantine_of(&directory, &destination)
            .reverse_restore(&name, "backup-test", "rollback-test.tmp", 2, digest(b"v2"))
            .expect("the quarantine restores");
        assert_eq!(std::fs::read(&destination).expect("read"), b"v1");
        assert!(
            !install.join("rollback-test.tmp").exists(),
            "the parking slot is cleaned"
        );
    }

    #[test]
    fn quarantine_reverse_refuses_a_regular_file_in_the_parking_slot() {
        let root = tempfile::tempdir().expect("a temp directory");
        let install = root.path().join("install");
        std::fs::create_dir_all(&install).expect("install");
        let destination = install.join("app.dat");
        std::fs::write(&destination, b"v2").expect("write v2");
        std::fs::write(install.join("backup-test"), b"v1").expect("a backup");
        std::fs::write(install.join("rollback-test.tmp"), b"squat").expect("a plant");

        let (directory, name) = crate::fs::open_parent(&destination).expect("the parent opens");
        assert!(
            quarantine_of(&directory, &destination)
                .reverse_restore(&name, "backup-test", "rollback-test.tmp", 2, digest(b"v2"))
                .is_err(),
            "an occupied parking slot fails closed"
        );
        assert_eq!(
            std::fs::read(install.join("rollback-test.tmp")).expect("read"),
            b"squat",
            "the occupant is never deleted"
        );
        assert_eq!(
            std::fs::read(&destination).expect("read"),
            b"v2",
            "the destination is untouched"
        );
        assert_eq!(
            std::fs::read(install.join("backup-test")).expect("read"),
            b"v1",
            "the backup is untouched"
        );
    }

    #[test]
    fn a_retry_after_parking_preserves_the_parked_entry() {
        let root = tempfile::tempdir().expect("a temp directory");
        let install = root.path().join("install");
        std::fs::create_dir_all(&install).expect("install");
        let destination = install.join("app.dat");
        std::fs::write(&destination, b"v1").expect("write v1");

        let transaction = transaction();
        let mut executor = LinuxFileExecutor::new();
        executor.set_transaction_id(transaction);
        register_replace(&mut executor, &destination, b"v1", b"v2");
        plant_staged(&install, &replace_key(), b"v2");
        let receipt = executor.replace(&replace_node()).expect("replace");
        let temporary = rollback_temp_name(&transaction, "op-replace");
        std::fs::rename(&destination, install.join(&temporary)).expect("a crash after parking");

        executor
            .rollback(&replace_node(), &receipt)
            .expect("recovery completes from the backup");
        assert_eq!(
            std::fs::read(&destination).expect("read"),
            b"v1",
            "the backup restores the destination"
        );
        assert_eq!(
            std::fs::read(install.join(&temporary)).expect("read"),
            b"v2",
            "the parked entry is preserved, never cleaned blindly"
        );
    }

    #[test]
    fn quarantine_reverse_refuses_a_foreign_parking_slot() {
        let root = tempfile::tempdir().expect("a temp directory");
        let install = root.path().join("install");
        std::fs::create_dir_all(&install).expect("install");
        let destination = install.join("app.dat");
        std::fs::write(&destination, b"v2").expect("write v2");
        std::fs::write(install.join("backup-test"), b"v1").expect("a backup");
        std::fs::create_dir_all(install.join("rollback-test.tmp")).expect("a foreign dir");

        let (directory, name) = crate::fs::open_parent(&destination).expect("the parent opens");
        assert!(
            quarantine_of(&directory, &destination)
                .reverse_restore(&name, "backup-test", "rollback-test.tmp", 2, digest(b"v2"))
                .is_err(),
            "a foreign parking slot fails closed"
        );
        assert_eq!(
            std::fs::read(&destination).expect("read"),
            b"v2",
            "the destination is untouched"
        );
        assert_eq!(
            std::fs::read(install.join("backup-test")).expect("read"),
            b"v1",
            "the backup is untouched"
        );
    }

    #[test]
    fn a_foreign_backup_name_collision_is_refused_without_deletion() {
        let root = tempfile::tempdir().expect("a temp directory");
        let install = root.path().join("install");
        std::fs::create_dir_all(&install).expect("install");
        let destination = install.join("app.dat");
        std::fs::write(&destination, b"v1").expect("write v1");

        let transaction = transaction();
        let planted = backup_name(&transaction, "op-replace", "backup");
        std::fs::write(install.join(&planted), b"unrelated").expect("a plant");

        let mut executor = LinuxFileExecutor::new();
        executor.set_transaction_id(transaction);
        register_replace(&mut executor, &destination, b"v1", b"v2");
        plant_staged(&install, &replace_key(), b"v2");

        let error = executor
            .replace(&replace_node())
            .expect_err("an occupied backup slot fails closed");
        assert!(
            matches!(
                error,
                ExecError::Verification { .. } | ExecError::PlanDrift { .. }
            ),
            "the refusal is explicit: {error}"
        );
        assert_eq!(
            std::fs::read(install.join(&planted)).expect("read"),
            b"unrelated",
            "the unknown file is never deleted"
        );
        assert_eq!(
            std::fs::read(&destination).expect("read"),
            b"v1",
            "the destination is untouched"
        );
    }

    #[test]
    fn backups_from_another_transaction_are_never_touched() {
        let root = tempfile::tempdir().expect("a temp directory");
        let install = root.path().join("install");
        std::fs::create_dir_all(&install).expect("install");
        let destination = install.join("app.dat");
        std::fs::write(&destination, b"v1").expect("write v1");

        let first = transaction();
        let mut executor = LinuxFileExecutor::new();
        executor.set_transaction_id(first);
        register_replace(&mut executor, &destination, b"v1", b"v2");
        plant_staged(&install, &replace_key(), b"v2");
        let receipt = executor.replace(&replace_node()).expect("replace");
        let OperationReceipt::ReplaceFile {
            ref backup_path, ..
        } = receipt
        else {
            panic!("a replace reports its backup");
        };
        assert_eq!(std::fs::read(&destination).expect("read"), b"v2");

        let second = transaction();
        assert_ne!(first, second);
        executor.set_transaction_id(second);
        executor
            .rollback(&replace_node(), &receipt)
            .expect_err("a receipt from another transaction binds nothing here");
        assert_eq!(std::fs::read(&destination).expect("read"), b"v2");
        assert_eq!(
            std::fs::read(Path::new(&backup_path)).expect("read"),
            b"v1",
            "the first transaction's backup survives a foreign rollback"
        );
    }
}

#[cfg(test)]
mod snapshot_tests {
    use super::*;
    use zup_core::{AppId, NonEmptyString, Privilege, RelativePath, SelectedScope, Target};

    fn target_with(files: Vec<(String, zup_platform::TargetPath)>) -> zup_platform::TargetPlan {
        let target = Target::parse("linux-x64").expect("a target");
        zup_platform::TargetPlan {
            app: zup_core::App {
                id: AppId::new("com.example.tool").expect("an id"),
                name: NonEmptyString::new("Tool").expect("a name"),
                version: semver::Version::parse("1.0.0").expect("a version"),
                publisher: None,
                main: None,
                description: None,
            },
            target,
            scope: SelectedScope::User,
            install_directory: zup_platform::TargetPath::new(target, "/tmp/zup-snapshot-test/tool")
                .expect("a path"),
            selected_components: Vec::new(),
            prerequisites: Vec::new(),
            files: files
                .into_iter()
                .map(|(name, destination)| zup_platform::TargetFile {
                    key: zup_core::ResourceKey::File {
                        destination: destination.to_string(),
                    },
                    source_relative: RelativePath::new(&name).expect("a path"),
                    destination,
                    size: 0,
                    sha256: zup_core::hash_bytes(b""),
                    privilege: Privilege::User,
                    executable: false,
                })
                .collect(),
            launchers: Vec::new(),
            path_entries: Vec::new(),
            services: Vec::new(),
            protocols: Vec::new(),
            file_associations: Vec::new(),
            summary: zup_platform::TargetPlanSummary {
                file_count: 0,
                install_bytes: 0,
                resource_count: 0,
                requires_authorization: false,
                selected_component_count: 0,
                prerequisite_count: 0,
                download_bytes: 0,
            },
            preset: None,
        }
    }

    fn destination(root: &std::path::Path, name: &str) -> zup_platform::TargetPath {
        zup_platform::TargetPath::new(
            Target::parse("linux-x64").expect("a target"),
            root.join(name).to_string_lossy(),
        )
        .expect("a path")
    }

    #[test]
    fn absent_stays_absent_and_present_is_identified() {
        let root = tempfile::tempdir().expect("a temp directory");
        std::fs::write(root.path().join("here.dat"), b"the bytes").expect("write");
        let plan = target_with(vec![
            ("here.dat".to_owned(), destination(root.path(), "here.dat")),
            ("gone.dat".to_owned(), destination(root.path(), "gone.dat")),
        ]);

        let snapshot = snapshot_target(&plan);

        assert_eq!(snapshot.files.len(), 2);
        assert!(
            matches!(
                snapshot.files[0].state,
                ObservedFileState::File { size: 9, .. }
            ),
            "{:?}",
            snapshot.files[0].state
        );
        assert_eq!(snapshot.files[1].state, ObservedFileState::Absent);
    }

    #[test]
    fn a_symlink_is_a_non_file_not_a_reading() {
        let root = tempfile::tempdir().expect("a temp directory");
        let elsewhere = tempfile::tempdir().expect("an unrelated tree");
        std::fs::write(elsewhere.path().join("target"), b"elsewhere").expect("write");
        std::os::unix::fs::symlink(
            elsewhere.path().join("target"),
            root.path().join("link.dat"),
        )
        .expect("symlink");
        let plan = target_with(vec![(
            "link.dat".to_owned(),
            destination(root.path(), "link.dat"),
        )]);

        let snapshot = snapshot_target(&plan);

        assert_eq!(snapshot.files[0].state, ObservedFileState::NonFile);
    }
}
