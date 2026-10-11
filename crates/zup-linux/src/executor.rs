use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use zup_bundle::{PayloadError, PayloadSource};
use zup_core::{RelativePath, ResourceKey, Sha256Digest, hash_reader};
use zup_exec::{HostSnapshot, ObservedFile, ObservedFileState};
use zup_platform::TargetPath;
use zup_transaction::{
    FileDelta, FilePrecondition, NodeKind, OperationExecutor, OperationId, OperationReceipt,
    ReconcileResult, TransactionNode, TransactionPlan,
};

use crate::error::{ExecError, PathError};
use crate::fs::{
    EXECUTABLE_PAYLOAD_MODE, EntryKind, OwnedDirectory, PAYLOAD_FILE_MODE, STATE_DIRECTORY_MODE,
    STATE_FILE_MODE,
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
        self
    }

    pub fn with_services(mut self, services: ServiceSupport) -> Self {
        self.services = Some(services);
        self
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
        let created = crate::fs::create_parents(&file.host_path, STATE_DIRECTORY_MODE)?;
        self.staged_dirs
            .entry(file.host_path.display().to_string())
            .or_default()
            .extend(created);
        let (directory, _) = crate::fs::open_parent(&file.host_path)?;
        directory.write_durable(&name, bytes, STATE_FILE_MODE)?;
        let (current_bytes, _, _) = read_current(&directory, &name, &file.host_path)?;
        let (found_size, found) =
            hash_reader(current_bytes.as_slice()).map_err(|error| PathError::Io {
                path: file.host_path.display().to_string(),
                source: error,
            })?;
        if found_size != file.intent.size || found != file.intent.sha256 {
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
            NodeKind::FileRemoval { .. } => {
                let file = self.file(node.id.as_str())?.clone();
                match observe(&file.host_path)? {
                    None => Err(ExecError::PlanDrift {
                        path: file.host_path.display().to_string(),
                        reason: "there is nothing to remove".into(),
                    }),
                    Some(_) => Ok(()),
                }
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
        use std::io::Read as _;
        let _ = self.file(node.id.as_str())?;
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
        let mut bytes = Vec::new();
        reader
            .read_to_end(&mut bytes)
            .map_err(|error| ExecError::Payload {
                path: source.to_string(),
                source: PayloadError::Read {
                    path: source.to_string(),
                    source: error,
                },
            })?;
        self.stage(&node.id, &bytes)
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
                )
            }
            OperationReceipt::RemoveFile { destination, .. } => {
                match observe(&PathBuf::from(destination.to_string()))? {
                    None => Ok(()),
                    Some(_) => Err(ExecError::Verification {
                        path: destination.to_string(),
                        reason: "the file is still there".into(),
                    }),
                }
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
                        let (bytes, mode) = read_stat(&directory, &name, &path)?;
                        let (found_size, found) =
                            hash_reader(bytes.as_slice()).map_err(|error| PathError::Io {
                                path: path.display().to_string(),
                                source: error,
                            })?;
                        if found_size != expected.size
                            || found != expected.sha256
                            || mode != expected_mode
                        {
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
                let backup =
                    check_backup(&directory, &path, backup_path, node.id.as_str(), "backup")?;

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
                        let saved = read_verified_backup(
                            &directory,
                            &backup,
                            &path,
                            previous.size,
                            previous.sha256,
                            "the destination is gone and the backup cannot be read",
                        )?;
                        restore_exclusive(
                            &directory,
                            &name,
                            &path,
                            &saved,
                            mode_from(*previous_mode),
                        )?;
                        let _ = directory.remove_file(&backup);
                        let _ = directory.sync();
                        verify_installed(&path, previous, *previous_mode)?;
                        Ok(())
                    }
                    Current::MatchesPrevious => {
                        directory.chmod_regular(&name, mode_from(*previous_mode))?;
                        let _ = directory.sync();
                        if backup_valid(&directory, &backup, previous.size, previous.sha256) {
                            let _ = directory.remove_file(&backup);
                            let _ = directory.sync();
                        }
                        verify_installed(&path, previous, *previous_mode)?;
                        Ok(())
                    }
                    Current::MatchesNew => {
                        let saved = read_verified_backup(
                            &directory,
                            &backup,
                            &path,
                            previous.size,
                            previous.sha256,
                            "the backup no longer holds the previous file",
                        )?;
                        directory.write_durable(&name, &saved, mode_from(*previous_mode))?;
                        let _ = directory.remove_file(&backup);
                        let _ = directory.sync();
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
                let backup = check_backup_host(
                    &directory,
                    &path,
                    &backup_host,
                    node.id.as_str(),
                    "removed",
                )?;

                match removed_state(&directory, &name, &path, *size, *sha256)? {
                    Restored::Already => {
                        directory.chmod_regular(&name, mode_from(*mode))?;
                        let _ = directory.sync();
                        if backup_valid(&directory, &backup, *size, *sha256) {
                            let _ = directory.remove_file(&backup);
                            let _ = directory.sync();
                        }
                        Ok(())
                    }
                    Restored::Absent => {
                        let saved = read_verified_backup(
                            &directory,
                            &backup,
                            &path,
                            *size,
                            *sha256,
                            "the backup no longer holds the removed file",
                        )?;
                        restore_exclusive(&directory, &name, &path, &saved, mode_from(*mode))?;
                        verify_previous(&path, *size, *sha256, *mode)?;
                        let _ = directory.remove_file(&backup);
                        let _ = directory.sync();
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
        let (directory, _) = crate::fs::open_parent(&file.host_path)?;
        let backup_name = backup_name(node.id.as_str(), "backup");
        let backup_mode = match directory.read_regular_stat(&backup_name) {
            Ok((previous, mode)) => hash_reader(previous.as_slice())
                .map(|(found_size, found)| (found_size == size && found == sha256).then_some(mode))
                .unwrap_or(None),
            Err(_) => None,
        };
        let new_mode = self.executable_mode_for(file.intent);
        match observe(&file.host_path)? {
            None => Ok(ReconcileResult::NotApplied),
            Some(_) if verify_installed(&file.host_path, file.intent, new_mode).is_ok() => {
                let Some(previous_mode) = backup_mode else {
                    return Ok(ReconcileResult::Ambiguous);
                };
                Ok(ReconcileResult::AppliedWithReceipt(
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
                ))
            }
            Some(_) => Ok(ReconcileResult::Ambiguous),
        }
    }

    fn reconcile_interrupted_removal(
        &mut self,
        node: &TransactionNode,
    ) -> Result<ReconcileResult, ExecError> {
        let Some(removal) = &node.meta.removal else {
            return Ok(ReconcileResult::Ambiguous);
        };
        let file = self.file(node.id.as_str())?.clone();
        let (directory, _) = crate::fs::open_parent(&file.host_path)?;
        let backup_name = backup_name(node.id.as_str(), "removed");
        let backup_mode = match directory.read_regular_stat(&backup_name) {
            Ok((saved, mode)) => hash_reader(saved.as_slice())
                .map(|(found_size, found)| {
                    (found_size == removal.size && found == removal.sha256).then_some(mode)
                })
                .unwrap_or(None),
            Err(_) => None,
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
        created_directories.extend(crate::fs::create_parents(
            &file.host_path,
            STATE_DIRECTORY_MODE,
        )?);
        created_directories.sort();
        created_directories.dedup();
        let (directory, name) = crate::fs::open_parent(&file.host_path)?;
        match directory.kind_or_absent(&name)? {
            None => {}
            Some(_) => {
                return Err(PathError::AlreadyExists {
                    path: file.host_path.display().to_string(),
                }
                .into());
            }
        }
        self.ensure_staged(node, &key, &file, &directory, &staged_name)?;
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

        let (directory, name) = crate::fs::open_parent(&file.host_path)?;
        let (previous_bytes, previous_mode) =
            directory
                .read_regular_stat(&name)
                .map_err(|_| ExecError::PlanDrift {
                    path: file.host_path.display().to_string(),
                    reason: "the destination a replace requires is not a regular file".into(),
                })?;
        let (found_size, found) =
            hash_reader(previous_bytes.as_slice()).map_err(|error| PathError::Io {
                path: file.host_path.display().to_string(),
                source: error,
            })?;
        if found_size != size || found != sha256 {
            return Err(ExecError::PlanDrift {
                path: file.host_path.display().to_string(),
                reason: format!(
                    "expected {size} bytes / {sha256}, found {found_size} bytes / {found}"
                ),
            });
        }

        let backup_name = backup_name(node.id.as_str(), "backup");
        directory.write_durable(&backup_name, &previous_bytes, mode_from(previous_mode))?;
        let backup_path = directory.path().join(&backup_name);

        self.ensure_staged(node, &key, &file, &directory, &staged_name)?;
        directory.chmod_regular(&staged_name, mode)?;
        directory.publish_replacing(&name, &staged_name)?;
        let expected_mode = (mode).bits() & 0o7777;
        verify_installed(&file.host_path, file.intent, expected_mode)?;

        Ok(OperationReceipt::ReplaceFile {
            destination: file.host_path.display().to_string(),
            previous_sha256: sha256.to_hex(),
            previous_size: size,
            previous_mode,
            backup_path: backup_path.display().to_string(),
            new_sha256: file.intent.sha256.to_hex(),
            new_size: file.intent.size,
            executable: file.intent.executable,
        })
    }

    fn remove(&mut self, node: &TransactionNode) -> Result<OperationReceipt, ExecError> {
        let file = self.file(node.id.as_str())?.clone();
        let (directory, name) = crate::fs::open_parent(&file.host_path)?;
        let (bytes, file_mode) = directory.read_regular_stat(&name).map_err(|_| {
            ExecError::from(PathError::Missing {
                path: file.host_path.display().to_string(),
            })
        })?;
        let (size, sha256) = hash_reader(bytes.as_slice()).map_err(|error| PathError::Io {
            path: file.host_path.display().to_string(),
            source: error,
        })?;
        let backup_name = backup_name(node.id.as_str(), "removed");
        directory.write_durable(&backup_name, &bytes, mode_from(file_mode))?;
        directory.remove_file(&name)?;
        directory.sync()?;

        let backup_path = directory.path().join(&backup_name);
        Ok(OperationReceipt::RemoveFile {
            destination: file.destination.clone(),

            backup_path: as_target(&file.destination, &backup_path)?,
            sha256,
            size,
            mode: file_mode,
        })
    }

    fn ensure_staged(
        &mut self,
        node: &TransactionNode,
        key: &ResourceKey,
        file: &FileWork,
        directory: &OwnedDirectory,
        staged_name: &str,
    ) -> Result<(), ExecError> {
        let staged_ok = match directory.read_regular(staged_name) {
            Ok(bytes) => hash_reader(bytes.as_slice())
                .map(|(found_size, found)| {
                    found_size == file.intent.size && found == file.intent.sha256
                })
                .unwrap_or(false),
            Err(_) => false,
        };
        if staged_ok {
            return Ok(());
        }
        let _ = key;
        let bytes = self.payload_bytes(node)?;
        directory.write_durable(staged_name, &bytes, STATE_FILE_MODE)?;
        let valid = match directory.read_regular(staged_name) {
            Ok(bytes) => hash_reader(bytes.as_slice())
                .map(|(found_size, found)| {
                    found_size == file.intent.size && found == file.intent.sha256
                })
                .unwrap_or(false),
            Err(_) => false,
        };
        if !valid {
            return Err(ExecError::Verification {
                path: file.host_path.display().to_string(),
                reason: "the staged bytes do not match the payload identity".into(),
            });
        }
        Ok(())
    }

    fn payload_bytes(&mut self, node: &TransactionNode) -> Result<Vec<u8>, ExecError> {
        use std::io::Read as _;
        let (source, sha256, size) = Self::payload_identity(node)?;
        let file = self.file(node.id.as_str())?.clone();
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
        let mut bytes = Vec::new();
        reader
            .read_to_end(&mut bytes)
            .map_err(|error| ExecError::Payload {
                path: source.to_string(),
                source: PayloadError::Read {
                    path: source.to_string(),
                    source: error,
                },
            })?;
        let (found_size, found) =
            hash_reader(bytes.as_slice()).map_err(|error| ExecError::Payload {
                path: source.to_string(),
                source: PayloadError::Read {
                    path: source.to_string(),
                    source: error,
                },
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
        Ok(bytes)
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
        if let Ok(bytes) = directory.read_regular(&staged_name) {
            let valid = hash_reader(bytes.as_slice())
                .map(|(found_size, found)| {
                    found_size == file.intent.size && found == file.intent.sha256
                })
                .unwrap_or(false);
            if valid {
                let _ = directory.remove_file(&staged_name);
                let _ = directory.sync();
            }
        }
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
    let Some((size, sha256)) = observe(destination)? else {
        return Err(ExecError::Verification {
            path: destination.display().to_string(),
            reason: "the published file is not there".into(),
        });
    };
    if size != intent.size || sha256 != intent.sha256 {
        return Err(ExecError::Verification {
            path: destination.display().to_string(),
            reason: format!(
                "expected {} bytes / {}, found {size} bytes / {sha256}",
                intent.size, intent.sha256
            ),
        });
    }
    let mode = installed_mode(destination)?;
    if mode != expected_mode & 0o7777 {
        return Err(ExecError::Verification {
            path: destination.display().to_string(),
            reason: format!("expected mode {:o}, found {mode:o}", expected_mode & 0o7777),
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

fn observe(destination: &Path) -> Result<Option<(u64, Sha256Digest)>, ExecError> {
    let Ok((directory, name)) = crate::fs::open_parent(destination) else {
        return Ok(None);
    };
    match directory.kind_or_absent(&name)? {
        None => Ok(None),
        Some(EntryKind::Regular) => {
            let bytes = directory.read_regular(&name)?;
            Ok(Some(hash_reader(bytes.as_slice()).map_err(|error| {
                PathError::Io {
                    path: destination.display().to_string(),
                    source: error,
                }
            })?))
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

fn installed_mode(destination: &Path) -> Result<u32, ExecError> {
    let (directory, name) = crate::fs::open_parent(destination)?;
    let (_, mode) = read_stat(&directory, &name, destination)?;
    Ok(mode)
}

fn read_stat(
    directory: &OwnedDirectory,
    name: &str,
    display: &Path,
) -> Result<(Vec<u8>, u32), ExecError> {
    directory
        .read_regular_stat(name)
        .map_err(|error| match error {
            PathError::UnexpectedKind { .. } => ExecError::from(error),
            _ => ExecError::Verification {
                path: display.display().to_string(),
                reason: "the file is not there".into(),
            },
        })
}

fn read_current(
    directory: &OwnedDirectory,
    name: &str,
    display: &Path,
) -> Result<(Vec<u8>, u64, Sha256Digest), ExecError> {
    let (bytes, _) = read_stat(directory, name, display)?;
    let (size, sha256) = hash_reader(bytes.as_slice()).map_err(|error| PathError::Io {
        path: display.display().to_string(),
        source: error,
    })?;
    Ok((bytes, size, sha256))
}

fn mode_from(mode: u32) -> rustix::fs::Mode {
    rustix::fs::Mode::from_bits_truncate(mode & 0o7777)
}

fn backup_name(id: &str, prefix: &str) -> String {
    format!("{prefix}-{}.bin", short_digest(id))
}

fn check_backup(
    directory: &OwnedDirectory,
    destination: &Path,
    backup_path: &str,
    node_id: &str,
    prefix: &str,
) -> Result<String, ExecError> {
    check_backup_host(
        directory,
        destination,
        Path::new(backup_path),
        node_id,
        prefix,
    )
}

fn check_backup_host(
    directory: &OwnedDirectory,
    destination: &Path,
    backup: &Path,
    node_id: &str,
    prefix: &str,
) -> Result<String, ExecError> {
    let expected = backup_name(node_id, prefix);
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
            let (bytes, _) = read_stat(directory, name, display)?;
            let (found_size, found) =
                hash_reader(bytes.as_slice()).map_err(|error| PathError::Io {
                    path: display.display().to_string(),
                    source: error,
                })?;
            if found_size == previous_size && found == previous_sha256 {
                Ok(Current::MatchesPrevious)
            } else if found_size == new_size && found == new_sha256 {
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
            let (bytes, _) = read_stat(directory, name, display)?;
            let (found_size, found) =
                hash_reader(bytes.as_slice()).map_err(|error| PathError::Io {
                    path: display.display().to_string(),
                    source: error,
                })?;
            if found_size == size && found == sha256 {
                Ok(Restored::Already)
            } else {
                Ok(Restored::Foreign)
            }
        }
        Some(_) => Ok(Restored::Foreign),
    }
}

fn backup_valid(directory: &OwnedDirectory, backup: &str, size: u64, sha256: Sha256Digest) -> bool {
    read_verified_backup(directory, backup, directory.path(), size, sha256, "").is_ok()
}

fn read_verified_backup(
    directory: &OwnedDirectory,
    backup: &str,
    display: &Path,
    size: u64,
    sha256: Sha256Digest,
    missing: &str,
) -> Result<Vec<u8>, ExecError> {
    let saved = directory
        .read_regular(backup)
        .map_err(|_| ExecError::RollbackDrift {
            path: display.display().to_string(),
            reason: missing.into(),
        })?;
    let (found_size, found) = hash_reader(saved.as_slice()).map_err(|error| PathError::Io {
        path: display.display().to_string(),
        source: error,
    })?;
    if found_size != size || found != sha256 {
        return Err(ExecError::RollbackDrift {
            path: display.display().to_string(),
            reason: "the backup no longer holds the previous file".into(),
        });
    }
    Ok(saved)
}

fn restore_exclusive(
    directory: &OwnedDirectory,
    name: &str,
    display: &Path,
    saved: &[u8],
    mode: rustix::fs::Mode,
) -> Result<(), ExecError> {
    directory
        .create_durable_exclusive(name, saved, mode)
        .map_err(|error| match error {
            PathError::AlreadyExists { .. } => ExecError::RollbackDrift {
                path: display.display().to_string(),
                reason: "the destination was recreated outside this transaction".into(),
            },
            other => other.into(),
        })
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
            backup_valid(&directory, &leaf, *previous_size, previous)
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
            backup_valid(&directory, &leaf, *size, *sha256)
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
        Ok(Some(EntryKind::Regular)) => match directory.read_regular(&name) {
            Ok(bytes) => match zup_core::hash_reader(bytes.as_slice()) {
                Ok((size, sha256)) => ObservedFileState::File { size, sha256 },
                Err(_) => ObservedFileState::NonFile,
            },
            Err(_) => ObservedFileState::NonFile,
        },

        Ok(Some(_)) => ObservedFileState::NonFile,
        Err(_) => ObservedFileState::NonFile,
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
