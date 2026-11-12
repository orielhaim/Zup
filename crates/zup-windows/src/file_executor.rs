use std::fs;
use std::path::{Path, PathBuf};
use zup_plan::{DeploymentNode, NodeKind, OperationId};

use thiserror::Error;
use zup_bundle::{PayloadError, PayloadSource};
use zup_core::{NativeObjectIdentity, ResourceKey, Sha256Digest};
use zup_plan::{FileOperationKind, FilePrecondition};
use zup_transaction::{OperationReceipt, ReconcileResult};

use crate::lowering::{host_path, target_path_from_host};

#[derive(Debug, Error)]
pub enum WindowsFileExecutorError {
    #[error("plan drift on `{path}`: {reason}")]
    PlanDrift { path: String, reason: String },

    #[error("file observation failed: {0}")]
    Observation(#[from] crate::InspectError),

    #[error(transparent)]
    Filesystem(#[from] crate::filesystem::FilesystemError),

    #[error("payload error: {0}")]
    Payload(#[from] PayloadError),

    #[error("staging failed: {0}")]
    Staging(String),

    #[error("verification failed for `{path}`: {reason}")]
    Verification { path: String, reason: String },

    #[error("ownership/drift during rollback of `{path}`: {reason}")]
    RollbackDrift { path: String, reason: String },

    #[error("unsupported operation `{id}` - fail closed")]
    Unsupported { id: String },

    #[error("cannot replace fixed file `{path}`: {requirement}")]
    FixedReplacementUnavailable {
        path: String,
        requirement: &'static str,
    },

    #[error("I/O error at `{path}`: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FixedPlacementState {
    Original,
    Retired,
    Published,
    Ambiguous,
}

pub struct WindowsFileExecutor<P: PayloadSource> {
    state: Option<crate::machine_state::StateAuthority>,
    persistent:
        Option<zup_transaction::PersistentStorage<crate::persistent::WindowsPersistentFilesystem>>,
    payload: P,
    generations: Option<
        zup_transaction::GenerationStorage<crate::generation::WindowsGenerationFilesystem<P>>,
    >,
    tx_id: zup_core::TransactionId,

    executable_suffix: &'static str,
}

impl<P: PayloadSource> WindowsFileExecutor<P> {
    fn payload_boundary(
        &self,
        node: &DeploymentNode,
    ) -> Result<Option<crate::filesystem::authority::DirectoryAncestry>, WindowsFileExecutorError>
    {
        let Some((state, destination)) = self.state.as_ref().zip(node.file_destination()) else {
            return Ok(None);
        };
        if node.meta.privilege != Some(state.scope.authorization()) {
            return Err(WindowsFileExecutorError::PlanDrift {
                path: node.id.to_string(),
                reason: "file authority differs from its protected installation state".into(),
            });
        }
        let path = host_path(destination);
        let parent = path
            .parent()
            .ok_or_else(|| WindowsFileExecutorError::Unsupported {
                id: node.id.to_string(),
            })?;
        Ok(Some(state.payload_directory(parent)?))
    }

    pub fn rollback_transaction_receipt(
        &self,
        node: &DeploymentNode,
        receipt: &zup_transaction::OperationReceipt,
    ) -> Result<(), WindowsFileExecutorError> {
        let _state_boundary = self.payload_boundary(node)?;
        use zup_transaction::OperationReceipt as Receipt;
        match receipt {
            Receipt::PersistentDirectory(owned) => {
                self.verify_persistent(node, owned).map_err(|reason| {
                    WindowsFileExecutorError::Verification {
                        path: node.id.to_string(),
                        reason,
                    }
                })
            }
            Receipt::RetainedFile { .. } if matches!(node.kind, NodeKind::RetainFile { .. }) => {
                Ok(())
            }
            Receipt::StageFile {
                object,
                staged_path,
                size,
                sha256,
            } => {
                let NodeKind::StageFile { key } = &node.kind else {
                    return Err(WindowsFileExecutorError::Unsupported {
                        id: node.id.to_string(),
                    });
                };
                let destination = node.file_destination().ok_or_else(|| {
                    WindowsFileExecutorError::Unsupported {
                        id: node.id.to_string(),
                    }
                })?;
                if Path::new(staged_path) != self.staged_path_for_key(key, &host_path(destination))
                    || node.meta.expected_size != Some(*size)
                    || node.meta.expected_sha256 != Some(*sha256)
                {
                    return Err(WindowsFileExecutorError::RollbackDrift {
                        path: staged_path.clone(),
                        reason: "staging receipt does not match its authorized operation".into(),
                    });
                }
                let path = Path::new(staged_path);
                match fs::symlink_metadata(path) {
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                    Err(source) => {
                        return Err(WindowsFileExecutorError::Io {
                            path: staged_path.clone(),
                            source,
                        });
                    }
                    Ok(_) => {}
                }
                let parent = payload_parent(
                    node,
                    path,
                    crate::filesystem::authority::DirectoryAccess::Read,
                )?;
                let file = parent.open_owned_file(
                    path.file_name()
                        .ok_or_else(|| WindowsFileExecutorError::Unsupported {
                            id: node.id.to_string(),
                        })?,
                    *object,
                    *size,
                    *sha256,
                    crate::filesystem::authority::PayloadAccess::Mutate,
                )?;
                crate::filesystem::discard(&file)?;
                Ok(())
            }
            Receipt::CreateFile {
                object,
                destination,
                installed_sha256,
                installed_size,
                ..
            } => {
                let path = Path::new(destination);
                if !matches!(
                    node.kind,
                    NodeKind::FileMutation {
                        delta: FileOperationKind::Create | FileOperationKind::RestoreOwned,
                        ..
                    }
                ) || node
                    .file_destination()
                    .is_none_or(|target| host_path(target) != path)
                    || node.meta.expected_size != Some(*installed_size)
                    || node.meta.expected_sha256 != Some(*installed_sha256)
                {
                    return Err(WindowsFileExecutorError::RollbackDrift {
                        path: destination.clone(),
                        reason: "creation receipt does not match its authorized operation".into(),
                    });
                }
                match fs::symlink_metadata(path) {
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                    Err(source) => {
                        return Err(WindowsFileExecutorError::Io {
                            path: destination.clone(),
                            source,
                        });
                    }
                    Ok(_) => {}
                }
                let parent = payload_parent(
                    node,
                    path,
                    crate::filesystem::authority::DirectoryAccess::Read,
                )?;
                let file = parent.open_owned_file(
                    path.file_name()
                        .ok_or_else(|| WindowsFileExecutorError::Unsupported {
                            id: node.id.to_string(),
                        })?,
                    *object,
                    *installed_size,
                    *installed_sha256,
                    crate::filesystem::authority::PayloadAccess::Mutate,
                )?;
                crate::filesystem::discard(&file)?;
                Ok(())
            }
            Receipt::ReplaceFile { .. } => self.restore_replacement(node, receipt),
            Receipt::RemoveFile {
                object,
                destination,
                backup_path,
                sha256,
                size,
                ..
            } => {
                let removal = node.meta.removal.as_ref().ok_or_else(|| {
                    WindowsFileExecutorError::Unsupported {
                        id: node.id.to_string(),
                    }
                })?;
                let path = host_path(destination);
                let backup = host_path(backup_path);
                if removal.destination != *destination
                    || removal.instance.object != *object
                    || removal.size != *size
                    || removal.sha256 != *sha256
                    || backup != self.removal_backup(&path)
                {
                    return Err(WindowsFileExecutorError::RollbackDrift {
                        path: destination.to_string(),
                        reason: "removal receipt does not match its authorized operation".into(),
                    });
                }
                use crate::filesystem::authority::{DirectoryAccess, FileAuthority, FileParent};
                let authority = match removal.privilege {
                    zup_core::Privilege::User => {
                        FileAuthority::User(crate::transport::UserSid::current().map_err(
                            |error| WindowsFileExecutorError::Staging(error.to_string()),
                        )?)
                    }
                    zup_core::Privilege::System => FileAuthority::Machine,
                };
                let parent = FileParent::open(
                    path.parent()
                        .ok_or_else(|| WindowsFileExecutorError::Unsupported {
                            id: node.id.to_string(),
                        })?,
                    authority,
                    DirectoryAccess::CreateFiles,
                )?;
                if file_identity(&path)? == Some((*object, *size, *sha256)) {
                    parent.open_owned_file(
                        path.file_name()
                            .ok_or_else(|| WindowsFileExecutorError::Unsupported {
                                id: node.id.to_string(),
                            })?,
                        *object,
                        *size,
                        *sha256,
                        crate::filesystem::authority::PayloadAccess::Mutate,
                    )?;
                    return Ok(());
                }
                parent.move_owned_file(
                    backup
                        .file_name()
                        .ok_or_else(|| WindowsFileExecutorError::Unsupported {
                            id: node.id.to_string(),
                        })?,
                    *object,
                    *size,
                    *sha256,
                    &parent,
                    path.file_name()
                        .ok_or_else(|| WindowsFileExecutorError::Unsupported {
                            id: node.id.to_string(),
                        })?,
                )?;
                Ok(())
            }
            _ => Err(WindowsFileExecutorError::Unsupported {
                id: "receipt".into(),
            }),
        }
    }

    pub fn reconcile_transaction_node(
        &self,
        node: &DeploymentNode,
        receipt: Option<&OperationReceipt>,
        transaction: &zup_transaction::TransactionRecord,
    ) -> Result<ReconcileResult, WindowsFileExecutorError> {
        let _state_boundary = self.payload_boundary(node)?;
        if self.tx_id != transaction.transaction_id || !transaction.plan.nodes.contains(node) {
            return Err(WindowsFileExecutorError::PlanDrift {
                path: node.id.to_string(),
                reason: "file executor does not belong to this transaction".into(),
            });
        }
        if let NodeKind::PersistentDirectory { operation } = &node.kind {
            let storage = self.persistent_storage().map_err(|reason| {
                WindowsFileExecutorError::PlanDrift {
                    path: node.id.to_string(),
                    reason,
                }
            })?;
            return storage.reconcile(operation, transaction).map_err(|reason| {
                WindowsFileExecutorError::Verification {
                    path: node.id.to_string(),
                    reason,
                }
            });
        }
        if let Some(receipt) = receipt {
            return Ok(if verify_installed_file(node, receipt).is_ok() {
                ReconcileResult::AppliedWithReceipt(receipt.clone())
            } else {
                ReconcileResult::Ambiguous
            });
        }
        if matches!(node.kind, NodeKind::RetainFile { .. }) {
            return Ok(match retain_file(node) {
                Ok(receipt) => ReconcileResult::AppliedWithReceipt(receipt),
                Err(_) => ReconcileResult::NotApplied,
            });
        }
        let key = match &node.kind {
            NodeKind::StageFile { key } | NodeKind::FileMutation { key, .. } => key,
            _ => {
                return Err(WindowsFileExecutorError::Unsupported {
                    id: node.id.to_string(),
                });
            }
        };
        let destination =
            node.file_destination()
                .ok_or_else(|| WindowsFileExecutorError::Unsupported {
                    id: node.id.to_string(),
                })?;
        let path = Path::new(destination.as_str());
        match &node.kind {
            NodeKind::StageFile { .. } => Ok(
                if file_identity(&self.staged_path_for_key(key, path))?.is_none() {
                    ReconcileResult::NotApplied
                } else {
                    ReconcileResult::Ambiguous
                },
            ),
            NodeKind::FileMutation {
                delta: FileOperationKind::Create | FileOperationKind::RestoreOwned,
                ..
            } => {
                let Some(OperationReceipt::StageFile {
                    object,
                    staged_path,
                    size,
                    sha256,
                }) = transaction.receipt(&OperationId::stage_file(key))
                else {
                    return Ok(ReconcileResult::Ambiguous);
                };
                let staged = self.staged_path_for_key(key, path);
                if Path::new(staged_path) != staged
                    || node.meta.expected_size != Some(*size)
                    || node.meta.expected_sha256 != Some(*sha256)
                {
                    return Ok(ReconcileResult::Ambiguous);
                }
                let expected = Some((*object, *size, *sha256));
                match (file_identity(&staged)?, file_identity(path)?) {
                    (Some(found), None) if Some(found) == expected => {
                        payload_parent(
                            node,
                            &staged,
                            crate::filesystem::authority::DirectoryAccess::Read,
                        )?
                        .open_owned_file(
                            staged.file_name().ok_or_else(|| {
                                WindowsFileExecutorError::Unsupported {
                                    id: node.id.to_string(),
                                }
                            })?,
                            *object,
                            *size,
                            *sha256,
                            crate::filesystem::authority::PayloadAccess::Mutate,
                        )?;
                        Ok(ReconcileResult::NotApplied)
                    }
                    (None, Some(found)) if Some(found) == expected => {
                        payload_parent(
                            node,
                            path,
                            crate::filesystem::authority::DirectoryAccess::Read,
                        )?
                        .open_owned_file(
                            path.file_name().ok_or_else(|| {
                                WindowsFileExecutorError::Unsupported {
                                    id: node.id.to_string(),
                                }
                            })?,
                            *object,
                            *size,
                            *sha256,
                            crate::filesystem::authority::PayloadAccess::Mutate,
                        )?;
                        Ok(ReconcileResult::AppliedWithReceipt(
                            OperationReceipt::CreateFile {
                                object: *object,
                                destination: path.display().to_string(),
                                installed_sha256: *sha256,
                                installed_size: *size,
                                executable: assert_runnable(
                                    path,
                                    node.meta.executable.unwrap_or(false),
                                    self.executable_suffix,
                                )?,
                                created_directories: Vec::new(),
                            },
                        ))
                    }
                    _ => Ok(ReconcileResult::Ambiguous),
                }
            }
            NodeKind::FileMutation {
                delta: FileOperationKind::Replace | FileOperationKind::RepairOwned,
                ..
            } => {
                let evidence = self.replacement_receipt(node, transaction)?;
                let state = self.replacement_state(node, &evidence)?;
                Ok(match state {
                    FixedPlacementState::Original => ReconcileResult::NotApplied,
                    FixedPlacementState::Retired | FixedPlacementState::Published => {
                        ReconcileResult::AppliedWithReceipt(evidence)
                    }
                    FixedPlacementState::Ambiguous => ReconcileResult::Ambiguous,
                })
            }
            _ => Ok(ReconcileResult::Ambiguous),
        }
    }
    fn replacement_backup(
        &self,
        node: &DeploymentNode,
    ) -> Result<PathBuf, WindowsFileExecutorError> {
        let path =
            node.file_destination()
                .ok_or_else(|| WindowsFileExecutorError::Unsupported {
                    id: node.id.to_string(),
                })?;
        let NodeKind::FileMutation { key, .. } = &node.kind else {
            return Err(WindowsFileExecutorError::Unsupported {
                id: node.id.to_string(),
            });
        };
        Ok(replacement_backup_path(&host_path(path), self.tx_id, key))
    }

    fn replacement_receipt(
        &self,
        node: &DeploymentNode,
        transaction: &zup_transaction::TransactionRecord,
    ) -> Result<OperationReceipt, WindowsFileExecutorError> {
        let invalid = || WindowsFileExecutorError::Unsupported {
            id: node.id.to_string(),
        };
        let NodeKind::FileMutation { key, .. } = &node.kind else {
            return Err(invalid());
        };
        let Some(FilePrecondition::Exact {
            object: previous_object,
            size: previous_size,
            sha256: previous_sha256,
        }) = node.meta.file_precondition
        else {
            return Err(invalid());
        };
        let destination = host_path(node.file_destination().ok_or_else(invalid)?);
        let Some(OperationReceipt::StageFile {
            object,
            staged_path,
            size,
            sha256,
        }) = transaction.receipt(&OperationId::stage_file(key))
        else {
            return Err(invalid());
        };
        if Path::new(staged_path) != self.staged_path_for_key(key, &destination)
            || node.meta.expected_size != Some(*size)
            || node.meta.expected_sha256 != Some(*sha256)
        {
            return Err(invalid());
        }
        Ok(OperationReceipt::ReplaceFile {
            previous_object,
            object: *object,
            destination: destination.display().to_string(),
            previous_sha256,
            previous_size,
            previous_mode: 0,
            backup_path: self.replacement_backup(node)?.display().to_string(),
            new_sha256: *sha256,
            new_size: *size,
            executable: assert_runnable(
                &destination,
                node.meta.executable.unwrap_or(false),
                self.executable_suffix,
            )?,
        })
    }

    fn replacement_state(
        &self,
        node: &DeploymentNode,
        receipt: &OperationReceipt,
    ) -> Result<FixedPlacementState, WindowsFileExecutorError> {
        let invalid = || WindowsFileExecutorError::Unsupported {
            id: node.id.to_string(),
        };
        let NodeKind::FileMutation {
            key,
            delta: FileOperationKind::Replace | FileOperationKind::RepairOwned,
        } = &node.kind
        else {
            return Err(invalid());
        };
        let OperationReceipt::ReplaceFile {
            destination,
            backup_path,
            object,
            previous_object,
            new_size,
            new_sha256,
            previous_size,
            previous_sha256,
            previous_mode,
            ..
        } = receipt
        else {
            return Err(invalid());
        };
        let path = host_path(node.file_destination().ok_or_else(invalid)?);
        if Path::new(destination) != path
            || Path::new(backup_path) != self.replacement_backup(node)?
            || *previous_mode != 0
            || node.meta.expected_size != Some(*new_size)
            || node.meta.expected_sha256 != Some(*new_sha256)
            || node.meta.file_precondition
                != Some(FilePrecondition::Exact {
                    object: *previous_object,
                    size: *previous_size,
                    sha256: *previous_sha256,
                })
        {
            return Err(invalid());
        }
        let staged = self.staged_path_for_key(key, &path);
        let old = Some((*previous_object, *previous_size, *previous_sha256));
        let new = Some((*object, *new_size, *new_sha256));
        let destination = file_identity(&path)?;
        let stage = file_identity(&staged)?;
        let backup = file_identity(Path::new(backup_path))?;
        Ok(if destination == old && stage == new && backup.is_none() {
            FixedPlacementState::Original
        } else if destination.is_none() && stage == new && backup == old {
            FixedPlacementState::Retired
        } else if destination == new && stage.is_none() && backup == old {
            FixedPlacementState::Published
        } else {
            FixedPlacementState::Ambiguous
        })
    }

    fn replace_file(
        &self,
        node: &DeploymentNode,
        transaction: &zup_transaction::TransactionRecord,
    ) -> Result<OperationReceipt, WindowsFileExecutorError> {
        use crate::filesystem::authority::DirectoryAccess;
        if node.meta.fixed_replacement != Some(zup_core::FixedReplacement::Quiescent) {
            return Err(WindowsFileExecutorError::FixedReplacementUnavailable {
                path: node.id.to_string(),
                requirement: "Windows requires explicitly declared quiescent replacement; native replacement does not provide an identity-conditional atomic exchange",
            });
        }
        let receipt = self.replacement_receipt(node, transaction)?;
        let OperationReceipt::ReplaceFile {
            destination,
            backup_path,
            object,
            previous_object,
            new_size,
            new_sha256,
            previous_size,
            previous_sha256,
            ..
        } = &receipt
        else {
            unreachable!()
        };
        let path = Path::new(destination);
        let NodeKind::FileMutation { key, .. } = &node.kind else {
            unreachable!()
        };
        let stage = self.staged_path_for_key(key, path);
        let state = self.replacement_state(node, &receipt)?;
        if state == FixedPlacementState::Published {
            verify_installed_file(node, &receipt)?;
            return Ok(receipt);
        }
        if state == FixedPlacementState::Ambiguous {
            return Err(WindowsFileExecutorError::PlanDrift { path: destination.clone(), reason: "replacement namespace does not match the authenticated old and staged instances".into() });
        }
        let parent = payload_parent(node, path, DirectoryAccess::CreateFiles)?;
        let staged =
            parent.fence_owned_file(stage.file_name().unwrap(), *object, *new_size, *new_sha256)?;
        let old_name = if state == FixedPlacementState::Original {
            path.file_name().unwrap()
        } else {
            Path::new(backup_path).file_name().unwrap()
        };
        let old = parent.fence_owned_file(
            old_name,
            *previous_object,
            *previous_size,
            *previous_sha256,
        )?;
        if state == FixedPlacementState::Original {
            parent.move_fenced_file(
                &old,
                Path::new(backup_path)
                    .file_name()
                    .unwrap()
                    .to_str()
                    .unwrap(),
            )?;
            zup_transaction::fault_injection::boundary(
                zup_transaction::fault_injection::Boundary::FixedInstanceRetired,
                node.id.as_str(),
            );
        }
        parent.move_fenced_file(&staged, path.file_name().unwrap().to_str().unwrap())?;
        zup_transaction::fault_injection::boundary(
            zup_transaction::fault_injection::Boundary::FixedInstancePublished,
            node.id.as_str(),
        );
        drop(old);
        drop(staged);
        drop(parent);
        verify_installed_file(node, &receipt)?;
        Ok(receipt)
    }

    fn restore_replacement(
        &self,
        node: &DeploymentNode,
        receipt: &OperationReceipt,
    ) -> Result<(), WindowsFileExecutorError> {
        use crate::filesystem::authority::DirectoryAccess;
        if matches!(
            node.kind,
            NodeKind::FileMutation {
                delta: FileOperationKind::RepairOwned,
                ..
            }
        ) {
            return Err(WindowsFileExecutorError::RollbackDrift { path: node.id.to_string(), reason: "repair displaced an externally modified instance; retained quarantine requires recovery".into() });
        }
        let OperationReceipt::ReplaceFile {
            destination,
            backup_path,
            object,
            previous_object,
            new_size,
            new_sha256,
            previous_size,
            previous_sha256,
            ..
        } = receipt
        else {
            return Err(WindowsFileExecutorError::Unsupported {
                id: node.id.to_string(),
            });
        };
        let state = self.replacement_state(node, receipt)?;
        if state == FixedPlacementState::Original {
            return Ok(());
        }
        if state == FixedPlacementState::Ambiguous {
            return Err(WindowsFileExecutorError::RollbackDrift {
                path: destination.clone(),
                reason: "replacement compensation cannot identify both owned instances".into(),
            });
        }
        let path = Path::new(destination);
        let NodeKind::FileMutation { key, .. } = &node.kind else {
            unreachable!()
        };
        let stage = self.staged_path_for_key(key, path);
        let parent = payload_parent(node, path, DirectoryAccess::CreateFiles)?;
        let new_name = if state == FixedPlacementState::Published {
            path.file_name().unwrap()
        } else {
            stage.file_name().unwrap()
        };
        let new = parent.fence_owned_file(new_name, *object, *new_size, *new_sha256)?;
        let old = parent.fence_owned_file(
            Path::new(backup_path).file_name().unwrap(),
            *previous_object,
            *previous_size,
            *previous_sha256,
        )?;
        if state == FixedPlacementState::Published {
            parent.move_fenced_file(&new, stage.file_name().unwrap().to_str().unwrap())?;
            zup_transaction::fault_injection::boundary(
                zup_transaction::fault_injection::Boundary::FixedRollbackDisplaced,
                node.id.as_str(),
            );
        }
        parent.move_fenced_file(&old, path.file_name().unwrap().to_str().unwrap())?;
        zup_transaction::fault_injection::boundary(
            zup_transaction::fault_injection::Boundary::FixedRollbackRestored,
            node.id.as_str(),
        );
        drop(new);
        drop(old);
        expect_identity(
            path,
            Some((*previous_object, *previous_size, *previous_sha256)),
            destination,
            "restored owned instance",
        )
    }

    pub fn new(
        payload: P,
        tx_id: zup_core::TransactionId,
        executable_suffix: &'static str,
    ) -> Self {
        Self {
            payload,
            state: None,
            generations: None,
            persistent: None,
            tx_id,
            executable_suffix,
        }
    }

    pub fn with_state_authority(
        mut self,
        root: PathBuf,
        scope: zup_core::SelectedScope,
        app: &zup_core::AppId,
    ) -> Self {
        let state = crate::machine_state::StateAuthority {
            root,
            scope,
            app: app.clone(),
        };
        self.state = Some(state.clone());
        self.persistent = Some(zup_transaction::PersistentStorage(
            crate::persistent::WindowsPersistentFilesystem {
                state: state.clone(),
            },
        ));
        self.generations = Some(zup_transaction::GenerationStorage::new(
            crate::generation::WindowsGenerationFilesystem::new(state),
        ));
        self
    }

    pub fn apply_cleanup(
        &self,
        node: &DeploymentNode,
        transaction: &zup_transaction::TransactionRecord,
    ) -> Result<OperationReceipt, String> {
        if self.tx_id != transaction.transaction_id {
            return Err("cleanup executor transaction differs".into());
        }
        crate::cleanup::apply(
            self.state
                .as_ref()
                .ok_or("cleanup requires protected state storage")?,
            node,
            transaction,
        )
    }

    pub fn verify_cleanup(
        &self,
        node: &DeploymentNode,
        receipt: &OperationReceipt,
    ) -> Result<(), String> {
        crate::cleanup::verify(
            self.state
                .as_ref()
                .ok_or("cleanup requires protected state storage")?,
            self.tx_id,
            node,
            receipt,
        )
    }

    fn persistent_storage(
        &self,
    ) -> Result<
        &zup_transaction::PersistentStorage<crate::persistent::WindowsPersistentFilesystem>,
        String,
    > {
        self.persistent
            .as_ref()
            .ok_or("persistent data requires protected transaction storage".into())
    }

    pub fn apply_persistent(
        &self,
        node: &DeploymentNode,
        transaction: &zup_transaction::TransactionRecord,
    ) -> Result<OperationReceipt, String> {
        let NodeKind::PersistentDirectory { operation } = &node.kind else {
            return Err("persistent directory effect required".into());
        };
        self.persistent_storage()?
            .apply(operation, transaction)
            .map(|owned| OperationReceipt::PersistentDirectory(Box::new(owned)))
    }

    pub fn verify_persistent(
        &self,
        node: &DeploymentNode,
        owned: &zup_transaction::PersistentDirectoryReceipt,
    ) -> Result<(), String> {
        let NodeKind::PersistentDirectory { operation } = &node.kind else {
            return Err("persistent directory receipt requires its typed effect".into());
        };
        self.persistent_storage()?.verify(operation, owned)
    }

    fn generation_storage(
        &self,
    ) -> Result<
        &zup_transaction::GenerationStorage<crate::generation::WindowsGenerationFilesystem<P>>,
        String,
    > {
        self.generations
            .as_ref()
            .ok_or("generation effects require the protected installation state authority".into())
    }

    pub fn apply_generation(
        &self,
        node: &DeploymentNode,
    ) -> Result<zup_transaction::OperationReceipt, String> {
        let NodeKind::Generation { operation, intent } = &node.kind else {
            return Err("expected generation effect".into());
        };
        self.generation_storage()?
            .apply(*operation, intent, self.tx_id, Some(&self.payload))
    }

    fn generation_intent<'a>(
        node: &'a DeploymentNode,
        receipt: &zup_transaction::GenerationReceipt,
    ) -> Result<&'a zup_plan::GenerationIntent, String> {
        let NodeKind::Generation { operation, intent } = &node.kind else {
            return Err("generation receipt belongs to a different effect".into());
        };
        if !receipt.matches(*operation, intent) {
            return Err("generation receipt differs from authorized intent".into());
        }
        Ok(intent)
    }

    pub fn verify_generation(
        &self,
        node: &DeploymentNode,
        receipt: &zup_transaction::GenerationReceipt,
    ) -> Result<(), String> {
        self.generation_storage()?
            .verify(Self::generation_intent(node, receipt)?, receipt)
    }

    pub fn retain_generation(
        &self,
        node: &DeploymentNode,
        receipt: &zup_transaction::GenerationReceipt,
    ) -> Result<(), String> {
        self.generation_storage()?
            .retain_after_compensation(Self::generation_intent(node, receipt)?, receipt)
    }

    pub fn reconcile_generation(
        &self,
        node: &DeploymentNode,
        receipt: Option<&zup_transaction::OperationReceipt>,
    ) -> Result<ReconcileResult, String> {
        if let Some(zup_transaction::OperationReceipt::Generation(receipt)) = receipt {
            self.verify_generation(node, receipt)?;
            return Ok(ReconcileResult::Applied);
        }
        let NodeKind::Generation { operation, intent } = &node.kind else {
            return Err("expected generation effect".into());
        };
        self.generation_storage()?
            .reconcile(*operation, intent, self.tx_id)
    }

    fn staged_path_for_key(&self, key: &ResourceKey, destination: &Path) -> PathBuf {
        staged_file_path(destination, self.tx_id, key)
    }

    fn removal_backup(&self, destination: &Path) -> PathBuf {
        removal_backup_path(destination, self.tx_id)
    }

    pub fn apply_owned_file_removal(
        &self,
        node: &DeploymentNode,
    ) -> Result<zup_transaction::OperationReceipt, WindowsFileExecutorError> {
        let _state_boundary = self.payload_boundary(node)?;
        let Some(removal) = &node.meta.removal else {
            return Err(WindowsFileExecutorError::Unsupported {
                id: node.id.to_string(),
            });
        };
        let destination_path = host_path(&removal.destination);
        if file_identity(&destination_path)?
            != Some((removal.instance.object, removal.size, removal.sha256))
        {
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
        use crate::filesystem::authority::{DirectoryAccess, FileAuthority, FileParent};
        let authority = match removal.privilege {
            zup_core::Privilege::User => FileAuthority::User(
                crate::transport::UserSid::current()
                    .map_err(|error| WindowsFileExecutorError::Staging(error.to_string()))?,
            ),
            zup_core::Privilege::System => FileAuthority::Machine,
        };
        let parent_path =
            destination_path
                .parent()
                .ok_or_else(|| WindowsFileExecutorError::Unsupported {
                    id: node.id.to_string(),
                })?;
        let parent = FileParent::open(parent_path, authority, DirectoryAccess::CreateFiles)?;
        parent.move_owned_file(
            destination_path
                .file_name()
                .ok_or_else(|| WindowsFileExecutorError::Unsupported {
                    id: node.id.to_string(),
                })?,
            removal.instance.object,
            removal.size,
            removal.sha256,
            &parent,
            backup
                .file_name()
                .ok_or_else(|| WindowsFileExecutorError::Unsupported {
                    id: node.id.to_string(),
                })?,
        )?;
        Ok(zup_transaction::OperationReceipt::RemoveFile {
            object: removal.instance.object,
            destination: removal.destination.clone(),
            backup_path: target_path_from_host(&backup, removal.destination.target()).map_err(
                |_| WindowsFileExecutorError::Unsupported {
                    id: node.id.to_string(),
                },
            )?,
            sha256: removal.sha256,
            size: removal.size,
            mode: 0o644,
        })
    }

    pub fn reconcile_owned_file_removal(
        &self,
        node: &DeploymentNode,
    ) -> Result<ReconcileResult, WindowsFileExecutorError> {
        let _state_boundary = self.payload_boundary(node)?;
        let Some(removal) = &node.meta.removal else {
            return Err(WindowsFileExecutorError::Unsupported {
                id: node.id.to_string(),
            });
        };
        let destination_path = host_path(&removal.destination);
        let backup = self.removal_backup(&destination_path);
        let expected = Some((removal.instance.object, removal.size, removal.sha256));
        match (file_identity(&destination_path)?, file_identity(&backup)?) {
            (Some(current), None) if Some(current) == expected => Ok(ReconcileResult::NotApplied),
            (None, Some(saved)) if Some(saved) == expected => {
                Ok(ReconcileResult::AppliedWithReceipt(
                    zup_transaction::OperationReceipt::RemoveFile {
                        object: removal.instance.object,
                        destination: removal.destination.clone(),
                        backup_path: target_path_from_host(&backup, removal.destination.target())
                            .map_err(|_| WindowsFileExecutorError::Unsupported {
                            id: node.id.to_string(),
                        })?,
                        sha256: removal.sha256,
                        size: removal.size,
                        mode: 0o644,
                    },
                ))
            }
            _ => Ok(ReconcileResult::Ambiguous),
        }
    }

    pub(crate) fn retained_reader(
        &self,
        node: &DeploymentNode,
        transaction: &zup_transaction::TransactionRecord,
        size: u64,
        sha256: Sha256Digest,
    ) -> Result<Option<zup_bundle::PayloadReader>, WindowsFileExecutorError> {
        let Some(storage) = &self.persistent else {
            return Ok(None);
        };
        let root = &storage.0.state.root;
        let scope = storage.0.state.scope;
        let installed = crate::InstallationRecordStore::new(root)
            .load(&transaction.app_id, scope)
            .map_err(|error| WindowsFileExecutorError::Staging(error.to_string()))?;
        let Some(installed) = installed else {
            return Ok(None);
        };
        let store = match scope {
            zup_core::SelectedScope::User => {
                zup_transaction::FilesystemTransactionStore::private_at(root)
            }
            zup_core::SelectedScope::Machine => storage.0.state.machine()?.transaction_store(),
        };
        let sources = zup_transaction::retained_file_sources(&store, &installed, transaction, node)
            .map_err(|error| WindowsFileExecutorError::Staging(error.to_string()))?;
        for source in sources {
            let path = host_path(&source.path);
            let parent = payload_parent(
                node,
                &path,
                crate::filesystem::authority::DirectoryAccess::Read,
            )?;
            if let Ok(mut file) = parent.open_owned_file(
                path.file_name().ok_or_else(|| {
                    WindowsFileExecutorError::Staging("retained content has no file name".into())
                })?,
                source.object,
                size,
                sha256,
                crate::filesystem::authority::PayloadAccess::Observe,
            ) {
                use std::io::Seek;
                file.rewind()
                    .map_err(|source| WindowsFileExecutorError::Io {
                        path: path.display().to_string(),
                        source,
                    })?;
                return Ok(Some(Box::new(file)));
            }
        }
        Ok(None)
    }

    fn create_file(
        &mut self,
        op: &DeploymentNode,
        dest: &Path,
        object: NativeObjectIdentity,
        key: &ResourceKey,
    ) -> Result<OperationReceipt, WindowsFileExecutorError> {
        let sha256 =
            op.meta
                .expected_sha256
                .ok_or_else(|| WindowsFileExecutorError::Unsupported {
                    id: op.id.to_string(),
                })?;
        let size = op
            .meta
            .expected_size
            .ok_or_else(|| WindowsFileExecutorError::Unsupported {
                id: op.id.to_string(),
            })?;
        let executable = assert_runnable(
            dest,
            op.meta.executable.unwrap_or(false),
            self.executable_suffix,
        )?;
        let staged = self.staged_path_for_key(key, dest);
        let parent = payload_parent(
            op,
            dest,
            crate::filesystem::authority::DirectoryAccess::CreateFiles,
        )?;
        parent.move_owned_file(
            staged
                .file_name()
                .ok_or_else(|| WindowsFileExecutorError::Unsupported {
                    id: op.id.to_string(),
                })?,
            object,
            size,
            sha256,
            &parent,
            dest.file_name()
                .ok_or_else(|| WindowsFileExecutorError::Unsupported {
                    id: op.id.to_string(),
                })?,
        )?;
        Ok(OperationReceipt::CreateFile {
            object,
            destination: dest.display().to_string(),
            installed_sha256: sha256,
            installed_size: size,
            executable,
            created_directories: Vec::new(),
        })
    }
}

fn payload_parent(
    node: &DeploymentNode,
    path: &Path,
    access: crate::filesystem::authority::DirectoryAccess,
) -> Result<crate::filesystem::authority::FileParent, WindowsFileExecutorError> {
    use crate::filesystem::authority::{FileAuthority, FileParent};
    let authority = match node.meta.privilege {
        Some(zup_core::Privilege::User) => FileAuthority::User(
            crate::transport::UserSid::current()
                .map_err(|error| WindowsFileExecutorError::Staging(error.to_string()))?,
        ),
        Some(zup_core::Privilege::System) => FileAuthority::Machine,
        None => {
            return Err(WindowsFileExecutorError::Unsupported {
                id: node.id.to_string(),
            });
        }
    };
    Ok(FileParent::open(
        path.parent()
            .ok_or_else(|| WindowsFileExecutorError::Unsupported {
                id: node.id.to_string(),
            })?,
        authority,
        access,
    )?)
}

fn file_identity(
    path: &Path,
) -> Result<Option<(NativeObjectIdentity, u64, Sha256Digest)>, WindowsFileExecutorError> {
    match crate::inspect_file(path)? {
        zup_plan::ObservedFileState::Absent => Ok(None),
        zup_plan::ObservedFileState::File {
            object,
            size,
            sha256,
            ..
        } => Ok(Some((object, size, sha256))),
        zup_plan::ObservedFileState::NonFile => Err(WindowsFileExecutorError::PlanDrift {
            path: path.display().to_string(),
            reason: "target is not a regular payload file".into(),
        }),
    }
}

pub fn apply_node<P: PayloadSource>(
    exec: &mut WindowsFileExecutor<P>,
    op: &DeploymentNode,
    transaction: &zup_transaction::TransactionRecord,
) -> Result<OperationReceipt, WindowsFileExecutorError> {
    let _state_boundary = exec.payload_boundary(op)?;
    if exec.tx_id != transaction.transaction_id || !transaction.plan.nodes.contains(op) {
        return Err(WindowsFileExecutorError::PlanDrift {
            path: op.id.to_string(),
            reason: "file executor does not belong to this transaction".into(),
        });
    }
    if matches!(op.kind, NodeKind::RetainFile { .. }) {
        return retain_file(op);
    }
    let missing = || WindowsFileExecutorError::Unsupported {
        id: op.id.to_string(),
    };
    let destination = host_path(op.file_destination().ok_or_else(missing)?);
    let sha256 = op.meta.expected_sha256.ok_or_else(missing)?;
    let size = op.meta.expected_size.ok_or_else(missing)?;
    match &op.kind {
        NodeKind::StageFile { key } => {
            let source = op.meta.source_relative.as_ref().ok_or_else(missing)?;
            let staged = exec.staged_path_for_key(key, &destination);
            let parent = payload_parent(
                op,
                &staged,
                crate::filesystem::authority::DirectoryAccess::PrepareFiles,
            )?;
            let mut reader = match exec.retained_reader(op, transaction, size, sha256)? {
                Some(reader) => reader,
                None => exec.payload.open(source, &sha256, size)?,
            };
            let name = staged.file_name().ok_or_else(missing)?;
            let object = parent.stage_payload(name, &mut reader, size, sha256)?;
            Ok(OperationReceipt::StageFile {
                object,
                staged_path: staged.display().to_string(),
                size,
                sha256,
            })
        }
        NodeKind::FileMutation { delta, key } => {
            let Some(OperationReceipt::StageFile {
                object,
                staged_path,
                size: staged_size,
                sha256: staged_hash,
            }) = transaction.receipt(&OperationId::stage_file(key))
            else {
                return Err(WindowsFileExecutorError::Staging(
                    "the durable staged-file receipt is missing".into(),
                ));
            };
            let expected_path = exec.staged_path_for_key(key, &destination);
            if Path::new(staged_path) != expected_path
                || *staged_size != size
                || *staged_hash != sha256
            {
                return Err(WindowsFileExecutorError::Staging(
                    "staged-file evidence does not authorize this publication".into(),
                ));
            }
            expect_identity(
                &expected_path,
                Some((*object, size, sha256)),
                &expected_path.display().to_string(),
                "staged file",
            )?;
            match delta {
                FileOperationKind::Create | FileOperationKind::RestoreOwned => {
                    exec.create_file(op, &destination, *object, key)
                }
                FileOperationKind::Replace | FileOperationKind::RepairOwned => {
                    exec.replace_file(op, transaction)
                }
                _ => Err(missing()),
            }
        }
        _ => Err(missing()),
    }
}

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

fn retain_file(node: &DeploymentNode) -> Result<OperationReceipt, WindowsFileExecutorError> {
    let invalid = || WindowsFileExecutorError::Unsupported {
        id: node.id.to_string(),
    };
    let destination = node.file_destination().ok_or_else(invalid)?;
    let Some(FilePrecondition::Exact {
        object,
        size,
        sha256,
    }) = node.meta.file_precondition
    else {
        return Err(invalid());
    };
    if node.meta.expected_size != Some(size) || node.meta.expected_sha256 != Some(sha256) {
        return Err(invalid());
    }
    let path = host_path(destination);
    payload_parent(
        node,
        &path,
        crate::filesystem::authority::DirectoryAccess::Read,
    )?
    .open_owned_file(
        path.file_name().ok_or_else(invalid)?,
        object,
        size,
        sha256,
        crate::filesystem::authority::PayloadAccess::Observe,
    )?;
    Ok(OperationReceipt::RetainedFile {
        object,
        destination: destination.clone(),
        size,
        sha256,
    })
}

pub fn verify_installed_file(
    node: &DeploymentNode,
    receipt: &zup_transaction::OperationReceipt,
) -> Result<(), WindowsFileExecutorError> {
    use zup_transaction::OperationReceipt as Receipt;
    match receipt {
        Receipt::PersistentDirectory(_) => Err(WindowsFileExecutorError::Unsupported {
            id: "persistent directory verification requires protected ownership evidence".into(),
        }),
        Receipt::NativeService(_) | Receipt::NativeServiceCompensation { .. } => {
            Err(WindowsFileExecutorError::Unsupported {
                id: "native service verification requires SCM authority".into(),
            })
        }
        Receipt::NativeCache { .. } => Err(WindowsFileExecutorError::Unsupported {
            id: "Linux native cache operation".into(),
        }),
        Receipt::RetainedFile { .. } => {
            if !matches!(node.kind, NodeKind::RetainFile { .. }) || retain_file(node)? != *receipt {
                return Err(WindowsFileExecutorError::Verification {
                    path: node.id.to_string(),
                    reason: "retained file observation differs from its authorized intent".into(),
                });
            }
            Ok(())
        }
        Receipt::Generation(_) => Err(WindowsFileExecutorError::Unsupported {
            id: "generation verification requires the protected storage authority".into(),
        }),
        Receipt::StageFile {
            object,
            staged_path,
            size,
            sha256,
        } => {
            let expected = (*object, *size, *sha256);
            expect_identity(
                Path::new(staged_path),
                Some(expected),
                staged_path,
                "staged payload",
            )
        }
        Receipt::CreateFile {
            object,
            destination,
            installed_sha256,
            installed_size,
            ..
        } => {
            let expected = (*object, *installed_size, *installed_sha256);
            expect_identity(
                Path::new(destination),
                Some(expected),
                destination,
                "installed file",
            )
        }
        Receipt::ReplaceFile {
            object,
            previous_object,
            destination,
            backup_path,
            new_sha256,
            new_size,
            previous_sha256,
            previous_size,
            ..
        } => {
            let installed = (*object, *new_size, *new_sha256);
            expect_identity(
                Path::new(destination),
                Some(installed),
                destination,
                "installed file",
            )?;
            let previous = (*previous_object, *previous_size, *previous_sha256);
            expect_identity(
                Path::new(backup_path),
                Some(previous),
                backup_path,
                "replaced file backup",
            )
        }
        Receipt::RemoveFile {
            object,
            destination,
            backup_path,
            sha256,
            size,
            ..
        } => {
            let destination = host_path(destination);
            let backup_path = host_path(backup_path);
            let removed = (*object, *size, *sha256);
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
        Receipt::Cleanup { .. }
        | Receipt::ServiceControl(_)
        | Receipt::Control
        | Receipt::Backend { .. } => Err(WindowsFileExecutorError::Unsupported {
            id: "receipt".into(),
        }),
    }
}

fn expect_identity(
    path: &Path,
    expected: Option<(NativeObjectIdentity, u64, Sha256Digest)>,
    display: &str,
    what: &str,
) -> Result<(), WindowsFileExecutorError> {
    match (file_identity(path)?, expected) {
        (found, Some(expected)) if found == Some(expected) => Ok(()),
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

pub(crate) fn resource_token(key: &ResourceKey) -> String {
    zup_core::hash_bytes(&serde_json::to_vec(key).expect("resource keys serialize")).to_hex()
}

pub(crate) fn staged_file_path(
    destination: &Path,
    transaction: zup_core::TransactionId,
    key: &ResourceKey,
) -> PathBuf {
    destination.with_file_name(format!(".zup-{transaction}-{}.stage", resource_token(key)))
}
pub(crate) fn replacement_backup_path(
    destination: &Path,
    transaction: zup_core::TransactionId,
    key: &ResourceKey,
) -> PathBuf {
    destination.with_file_name(format!(
        ".zup-{transaction}-{}.retired",
        resource_token(key)
    ))
}

pub(crate) fn removal_backup_path(
    destination: &Path,
    transaction: zup_core::TransactionId,
) -> PathBuf {
    let mut name = std::ffi::OsString::from(".");
    name.push(destination.file_name().unwrap_or_default());
    name.push(format!(".zup-{transaction}.bak"));
    destination.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zup_core::{
        AppId, InstallationId, Privilege, RelativePath, SelectedScope, Target, TransactionId,
    };
    use zup_plan::{DeploymentEffects, FileOperation, compile_deployment};

    struct NativeFiles {
        files: WindowsFileExecutor<zup_bundle::DirectoryPayloadSource>,
        fail_verification: bool,
    }

    impl zup_transaction::OperationExecutor for NativeFiles {
        type Error = String;

        fn prepare(&mut self, _: &DeploymentNode) -> Result<(), String> {
            Ok(())
        }

        fn apply(
            &mut self,
            node: &DeploymentNode,
            transaction: &zup_transaction::TransactionRecord,
        ) -> Result<OperationReceipt, String> {
            if matches!(node.kind, NodeKind::Barrier) {
                return Ok(OperationReceipt::Control);
            }
            apply_node(&mut self.files, node, transaction).map_err(|error| error.to_string())
        }

        fn verify(
            &mut self,
            node: &DeploymentNode,
            receipt: &OperationReceipt,
        ) -> Result<(), String> {
            if self.fail_verification {
                return Err("injected verification failure".into());
            }
            verify_installed_file(node, receipt).map_err(|error| error.to_string())
        }

        fn rollback(
            &mut self,
            node: &DeploymentNode,
            receipt: &OperationReceipt,
        ) -> Result<(), String> {
            if matches!(receipt, OperationReceipt::Control) {
                return Ok(());
            }
            self.files
                .rollback_transaction_receipt(node, receipt)
                .map_err(|error| error.to_string())
        }

        fn reconcile_rollback(
            &mut self,
            _: &DeploymentNode,
            receipt: &OperationReceipt,
        ) -> Result<zup_transaction::RollbackReconcileResult, String> {
            Ok(if matches!(receipt, OperationReceipt::Control) {
                zup_transaction::RollbackReconcileResult::Restored
            } else {
                zup_transaction::RollbackReconcileResult::Retry
            })
        }

        fn reconcile(
            &mut self,
            node: &DeploymentNode,
            receipt: Option<&OperationReceipt>,
            transaction: &zup_transaction::TransactionRecord,
        ) -> Result<ReconcileResult, String> {
            self.files
                .reconcile_transaction_node(node, receipt, transaction)
                .map_err(|error| error.to_string())
        }
    }

    fn fixture(
        root: &Path,
    ) -> (
        WindowsFileExecutor<zup_bundle::DirectoryPayloadSource>,
        zup_transaction::TransactionRecord,
    ) {
        let destination = root.join("install/subdir/installed.bin");
        let target = target_path_from_host(&destination, &Target::WINDOWS_X64).unwrap();
        let key = ResourceKey::File {
            id: zup_core::FileResourceId::Payload {
                id: zup_core::PayloadId::File {
                    declaration: zup_core::ResourceId::new("fixture-0").unwrap(),
                },
            },
        };
        let bytes = b"owned payload";
        let sha256 = zup_core::hash_bytes(bytes);
        let mut effects = DeploymentEffects::new(
            InstallationId::new_v7(),
            crate::requirements_fixture::requirements(
                Target::WINDOWS_X64,
                zup_core::SelectedScope::User,
                "com.zup.compensation",
                "1.0.0",
            ),
        );
        effects.install_directory =
            Some(target_path_from_host(root, &Target::WINDOWS_X64).unwrap());
        effects.files.push(FileOperation {
            replacement: zup_core::FixedReplacement::Atomic,
            key: key.clone(),
            source_relative: RelativePath::new("payload.bin").unwrap(),
            destination: target,
            precondition: FilePrecondition::Absent,
            expected_sha256: sha256,
            expected_size: bytes.len() as u64,
            privilege: Privilege::User,
            executable: false,
            conflict: None,
            kind: FileOperationKind::Create,
        });
        let plan = compile_deployment(&effects).unwrap();
        let transaction = zup_transaction::TransactionRecord::new(
            TransactionId::new_v7(),
            AppId::new("com.zup.compensation").unwrap(),
            SelectedScope::User,
            "1.0.0".parse().unwrap(),
            plan,
        );
        let executor = WindowsFileExecutor::new(
            zup_bundle::DirectoryPayloadSource::new(root),
            transaction.transaction_id,
            ".exe",
        );
        (executor, transaction)
    }

    #[rstest::rstest]
    #[case::created_owned(false, false, false)]
    #[case::created_foreign(false, true, false)]
    #[case::created_wrong_intent(false, false, true)]
    #[case::staged_owned(true, false, false)]
    #[case::staged_foreign(true, true, false)]
    #[case::staged_wrong_intent(true, false, true)]
    fn compensation_removes_only_the_authorized_instance(
        #[case] staged: bool,
        #[case] foreign: bool,
        #[case] wrong_intent: bool,
    ) {
        let root = tempfile::tempdir_in(crate::host_dirs::user_data().unwrap()).unwrap();
        let (executor, transaction) = fixture(root.path());
        let bytes = b"owned payload";
        let sha256 = zup_core::hash_bytes(bytes);
        let destination = root.path().join("install/subdir/installed.bin");
        let key = ResourceKey::File {
            id: zup_core::FileResourceId::Payload {
                id: zup_core::PayloadId::File {
                    declaration: zup_core::ResourceId::new("fixture-0").unwrap(),
                },
            },
        };
        let mut node = transaction
            .plan
            .nodes
            .iter()
            .find(|node| {
                if staged {
                    matches!(node.kind, NodeKind::StageFile { .. })
                } else {
                    matches!(node.kind, NodeKind::FileMutation { .. })
                }
            })
            .unwrap()
            .clone();
        let path = if staged {
            executor.staged_path_for_key(&key, &destination)
        } else {
            destination
        };
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, bytes).unwrap();
        let object = file_identity(&path).unwrap().unwrap().0;
        let receipt = if staged {
            OperationReceipt::StageFile {
                object,
                staged_path: path.display().to_string(),
                size: bytes.len() as u64,
                sha256,
            }
        } else {
            OperationReceipt::CreateFile {
                object,
                destination: path.display().to_string(),
                installed_sha256: sha256,
                installed_size: bytes.len() as u64,
                executable: false,
                created_directories: Vec::new(),
            }
        };
        if foreign {
            fs::rename(&path, root.path().join("original.bin")).unwrap();
            fs::write(&path, bytes).unwrap();
        }
        if wrong_intent {
            node.meta.expected_size = Some(1);
        }
        let result = executor.rollback_transaction_receipt(&node, &receipt);
        if foreign || wrong_intent {
            assert!(result.is_err());
            assert_eq!(fs::read(&path).unwrap(), bytes);
        } else {
            result.unwrap();
            assert!(!path.exists());
            executor
                .rollback_transaction_receipt(&node, &receipt)
                .unwrap();
        }
    }

    #[rstest::rstest]
    #[case::owned(false)]
    #[case::foreign_equal_bytes(true)]
    fn fixed_create_preserves_staged_identity_and_recovers_missing_acknowledgment(
        #[case] foreign: bool,
    ) {
        use zup_transaction::{FilesystemTransactionStore, NodeState, TransactionStore};
        let root = tempfile::tempdir_in(crate::host_dirs::user_data().unwrap()).unwrap();
        let (mut executor, mut transaction) = fixture(root.path());
        fs::write(root.path().join("payload.bin"), b"owned payload").unwrap();
        let store = FilesystemTransactionStore::private_at(root.path().join("state"));
        store.create(&transaction).unwrap();
        let reservation = OperationId::new(OperationId::RESERVE);
        transaction
            .nodes
            .insert(reservation.clone(), NodeState::Running);
        let revision = transaction.revision;
        transaction.touch();
        store.compare_and_swap(revision, &transaction).unwrap();
        transaction.reservation = Some(store.reserve_installation(&transaction).unwrap());
        transaction.nodes.insert(
            reservation,
            NodeState::Applied {
                receipt: Box::new(OperationReceipt::Control),
            },
        );
        let revision = transaction.revision;
        transaction.touch();
        store.compare_and_swap(revision, &transaction).unwrap();
        let stage = transaction
            .plan
            .nodes
            .iter()
            .find(|node| matches!(node.kind, NodeKind::StageFile { .. }))
            .unwrap()
            .clone();
        let create = transaction
            .plan
            .nodes
            .iter()
            .find(|node| matches!(node.kind, NodeKind::FileMutation { .. }))
            .unwrap()
            .clone();
        let receipt = apply_node(&mut executor, &stage, &transaction).unwrap();
        let OperationReceipt::StageFile {
            object,
            staged_path,
            ..
        } = &receipt
        else {
            unreachable!()
        };
        let staged_object = *object;
        let staged_path = PathBuf::from(staged_path);
        transaction.nodes.insert(
            stage.id.clone(),
            NodeState::Applied {
                receipt: Box::new(receipt),
            },
        );
        let revision = transaction.revision;
        transaction.touch();
        store.compare_and_swap(revision, &transaction).unwrap();
        transaction = store.load(&transaction.transaction_id).unwrap();
        if foreign {
            fs::rename(&staged_path, root.path().join("original-stage.bin")).unwrap();
            fs::write(&staged_path, b"owned payload").unwrap();
            assert!(apply_node(&mut executor, &create, &transaction).is_err());
            assert_eq!(fs::read(&staged_path).unwrap(), b"owned payload");
            assert!(matches!(
                executor
                    .reconcile_transaction_node(&create, None, &transaction)
                    .unwrap(),
                ReconcileResult::Ambiguous
            ));
            return;
        }
        assert!(matches!(
            executor
                .reconcile_transaction_node(&create, None, &transaction)
                .unwrap(),
            ReconcileResult::NotApplied
        ));
        let published = apply_node(&mut executor, &create, &transaction).unwrap();
        assert!(!staged_path.exists());
        let OperationReceipt::CreateFile { object, .. } = published else {
            unreachable!()
        };
        assert_eq!(object, staged_object);
        verify_installed_file(&create, &published).unwrap();
        let ReconcileResult::AppliedWithReceipt(recovered) = executor
            .reconcile_transaction_node(&create, None, &transaction)
            .unwrap()
        else {
            panic!("published instance must be recovered from durable staging evidence")
        };
        assert_eq!(recovered, published);
        executor
            .rollback_transaction_receipt(&create, &recovered)
            .unwrap();
        assert!(!host_path(create.file_destination().unwrap()).exists());
    }

    #[rstest::rstest]
    #[case::commit(false)]
    #[case::compensate(true)]
    fn coordinator_settles_native_fixed_creation(#[case] fail_verification: bool) {
        use zup_transaction::{
            FilesystemTransactionStore, TransactionCoordinator, TransactionOutcome,
        };
        let root = tempfile::tempdir_in(crate::host_dirs::user_data().unwrap()).unwrap();
        let (mut files, intent) = fixture(root.path());
        let bytes = b"owned payload";
        fs::write(root.path().join("payload.bin"), bytes).unwrap();
        fs::write(root.path().join("unrelated.bin"), b"foreign").unwrap();
        let state = root.path().join("state");
        let pending = TransactionCoordinator::new(FilesystemTransactionStore::private_at(&state))
            .begin(
                intent.app_id.clone(),
                intent.scope,
                intent.app_version.clone(),
                intent.plan,
            )
            .unwrap();
        files.tx_id = pending.transaction_id;
        let mut executor = NativeFiles {
            files,
            fail_verification,
        };
        let publisher = crate::InstallationRecordStore::new(&state);
        let settled = pending.execute(&mut executor, &publisher).unwrap();
        let destination = root.path().join("install/subdir/installed.bin");
        if fail_verification {
            assert_eq!(settled.outcome, TransactionOutcome::RolledBack);
            assert!(!destination.exists());
        } else {
            assert_eq!(settled.outcome, TransactionOutcome::Committed);
            assert_eq!(fs::read(&destination).unwrap(), bytes);
            let record = publisher
                .load(&intent.app_id, SelectedScope::User)
                .unwrap()
                .unwrap();
            let owned = record
                .current
                .resources
                .values()
                .find_map(|resource| match resource {
                    zup_plan::OwnedResource::File { instance, .. } => Some(instance.object),
                    _ => None,
                })
                .unwrap();
            assert_eq!(file_identity(&destination).unwrap().unwrap().0, owned);
        }
        assert_eq!(
            fs::read(root.path().join("unrelated.bin")).unwrap(),
            b"foreign"
        );
    }

    fn replacement_fixture(
        root: &Path,
        replacement: zup_core::FixedReplacement,
    ) -> (
        WindowsFileExecutor<zup_bundle::DirectoryPayloadSource>,
        zup_transaction::TransactionRecord,
    ) {
        let (executor, original) = fixture(root);
        fs::write(root.join("payload.bin"), b"owned payload").unwrap();
        let requested = original
            .plan
            .nodes
            .iter()
            .find(|node| matches!(node.kind, NodeKind::FileMutation { .. }))
            .unwrap();
        let destination = requested.file_destination().unwrap();
        let path = host_path(destination);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"previous payload").unwrap();
        let previous = file_identity(&path).unwrap().unwrap();
        let mut effects = DeploymentEffects::new(
            original.plan.installation,
            crate::requirements_fixture::requirements(
                Target::WINDOWS_X64,
                zup_core::SelectedScope::User,
                "com.zup.compensation",
                "1.0.0",
            ),
        );
        effects.install_directory = original.plan.install_directory.clone();
        effects.files.push(FileOperation {
            key: ResourceKey::File {
                id: zup_core::FileResourceId::Payload {
                    id: zup_core::PayloadId::File {
                        declaration: zup_core::ResourceId::new("fixture-2").unwrap(),
                    },
                },
            },
            replacement,
            kind: FileOperationKind::Replace,
            destination: destination.clone(),
            source_relative: RelativePath::new("payload.bin").unwrap(),
            precondition: FilePrecondition::Exact {
                object: previous.0,
                size: previous.1,
                sha256: previous.2,
            },
            expected_sha256: zup_core::hash_bytes(b"owned payload"),
            expected_size: 13,
            executable: false,
            privilege: Privilege::User,
            conflict: None,
        });
        let record = zup_transaction::TransactionRecord::new(
            original.transaction_id,
            original.app_id,
            original.scope,
            original.app_version,
            compile_deployment(&effects).unwrap(),
        );
        (executor, record)
    }

    #[rstest::rstest]
    #[case::atomic(zup_core::FixedReplacement::Atomic, false)]
    #[case::quiescent(zup_core::FixedReplacement::Quiescent, false)]
    #[case::locked(zup_core::FixedReplacement::Quiescent, true)]
    fn fixed_replacement_preserves_instances_and_requires_exclusive_access(
        #[case] replacement: zup_core::FixedReplacement,
        #[case] locked: bool,
    ) {
        let root = tempfile::tempdir_in(crate::host_dirs::user_data().unwrap()).unwrap();
        let (mut executor, mut record) = replacement_fixture(root.path(), replacement);
        let mutation = record
            .plan
            .nodes
            .iter()
            .find(|node| matches!(node.kind, NodeKind::FileMutation { .. }))
            .unwrap();
        let path = host_path(mutation.file_destination().unwrap());
        let previous = file_identity(&path).unwrap().unwrap();
        let stage = record
            .plan
            .nodes
            .iter()
            .find(|node| matches!(node.kind, NodeKind::StageFile { .. }))
            .unwrap()
            .clone();
        let mutation = record
            .plan
            .nodes
            .iter()
            .find(|node| matches!(node.kind, NodeKind::FileMutation { .. }))
            .unwrap()
            .clone();
        let staged = apply_node(&mut executor, &stage, &record).unwrap();
        record.nodes.insert(
            stage.id.clone(),
            zup_transaction::NodeState::Applied {
                receipt: Box::new(staged.clone()),
            },
        );
        let reader = locked.then(|| fs::File::open(&path).unwrap());
        let applied = apply_node(&mut executor, &mutation, &record);
        if replacement == zup_core::FixedReplacement::Atomic || locked {
            assert!(applied.is_err());
            assert_eq!(file_identity(&path).unwrap(), Some(previous));
        } else {
            let receipt = applied.unwrap();
            verify_installed_file(&mutation, &receipt).unwrap();
            assert_eq!(
                executor
                    .reconcile_transaction_node(&mutation, None, &record)
                    .unwrap(),
                ReconcileResult::AppliedWithReceipt(receipt.clone())
            );
            executor
                .rollback_transaction_receipt(&mutation, &receipt)
                .unwrap();
            executor
                .rollback_transaction_receipt(&mutation, &receipt)
                .unwrap();
            assert_eq!(file_identity(&path).unwrap(), Some(previous));
        }
        drop(reader);
        verify_installed_file(&stage, &staged).unwrap();
    }

    #[test]
    fn exclusive_fixed_file_handle_excludes_running_and_new_images() {
        use crate::filesystem::authority::{DirectoryAccess, FileAuthority, FileParent};
        const LATCH: &str = "ZUP_FIXED_IMAGE_LATCH";
        if let Some(latch) = std::env::var_os(LATCH) {
            let latch = PathBuf::from(latch);
            fs::write(latch.with_extension("ready"), b"ready").unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
            while !latch.exists() {
                assert!(std::time::Instant::now() < deadline);
                std::thread::park_timeout(std::time::Duration::from_millis(10));
            }
            return;
        }
        let root = tempfile::tempdir_in(crate::host_dirs::user_data().unwrap()).unwrap();
        let path = root.path().join("application.exe");
        fs::copy(std::env::current_exe().unwrap(), &path).unwrap();
        let expected = file_identity(&path).unwrap().unwrap();
        let parent = FileParent::open(
            root.path(),
            FileAuthority::User(crate::transport::UserSid::current().unwrap()),
            DirectoryAccess::Read,
        )
        .unwrap();
        let latch = root.path().join("release");
        let thread = std::thread::current();
        let mut child = std::process::Command::new(&path)
            .args(["--exact", thread.name().unwrap()])
            .env(LATCH, &latch)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !latch.with_extension("ready").exists() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::park_timeout(std::time::Duration::from_millis(10));
        }
        assert!(
            parent
                .fence_owned_file(
                    path.file_name().unwrap(),
                    expected.0,
                    expected.1,
                    expected.2
                )
                .is_err()
        );
        fs::write(&latch, b"release").unwrap();
        assert!(child.wait().unwrap().success());
        let fence = parent
            .fence_owned_file(
                path.file_name().unwrap(),
                expected.0,
                expected.1,
                expected.2,
            )
            .unwrap();
        assert!(
            std::process::Command::new(&path)
                .arg("--list")
                .stdout(std::process::Stdio::null())
                .spawn()
                .is_err()
        );
        drop(fence);
        assert!(
            std::process::Command::new(&path)
                .arg("--list")
                .stdout(std::process::Stdio::null())
                .status()
                .unwrap()
                .success()
        );
        assert_eq!(file_identity(&path).unwrap(), Some(expected));
    }

    #[cfg(feature = "test-faults")]
    #[rstest::rstest]
    #[case("fixed_instance_retired", false)]
    #[case("fixed_instance_published", false)]
    #[case("fixed_rollback_displaced", true)]
    #[case("fixed_rollback_restored", true)]
    fn fixed_replacement_recovers_native_instance_moves(#[case] point: &str, #[case] fail: bool) {
        use zup_transaction::{
            FilesystemTransactionStore, TransactionCoordinator, TransactionOutcome,
            TransactionStore,
        };
        const ROOT: &str = "ZUP_FIXED_MOVE_CRASH_ROOT";
        if let Some(root) = std::env::var_os(ROOT) {
            let root = PathBuf::from(root);
            let (mut files, record) =
                replacement_fixture(&root, zup_core::FixedReplacement::Quiescent);
            let store = FilesystemTransactionStore::private_at(root.join("state"));
            let pending = TransactionCoordinator::new(store)
                .begin(record.app_id, record.scope, record.app_version, record.plan)
                .unwrap();
            files.tx_id = pending.transaction_id;
            let mut executor = NativeFiles {
                files,
                fail_verification: fail,
            };
            pending
                .execute(
                    &mut executor,
                    &crate::InstallationRecordStore::new(root.join("state")),
                )
                .unwrap();
            panic!("fault boundary was not reached");
        }
        let root = tempfile::tempdir_in(crate::host_dirs::user_data().unwrap()).unwrap();
        let thread = std::thread::current();
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", thread.name().unwrap(), "--nocapture"])
            .env(ROOT, root.path())
            .env("ZUP_TEST_FAULT_POINT", point)
            .env("ZUP_TEST_FAULT_ACTION", "crash")
            .output()
            .unwrap();
        assert!(!child.status.success());
        let state = root.path().join("state");
        let store = FilesystemTransactionStore::private_at(&state);
        let ids = store.list_ids().unwrap();
        assert_eq!(ids.len(), 1);
        let pending = store.load(&ids[0]).unwrap();
        let mutation = pending
            .plan
            .nodes
            .iter()
            .find(|node| matches!(node.kind, NodeKind::FileMutation { .. }))
            .unwrap();
        let destination = host_path(mutation.file_destination().unwrap());
        let Some(FilePrecondition::Exact { object: old, .. }) = mutation.meta.file_precondition
        else {
            unreachable!()
        };
        let files = WindowsFileExecutor::new(
            zup_bundle::DirectoryPayloadSource::new(root.path()),
            pending.transaction_id,
            ".exe",
        );
        let mut executor = NativeFiles {
            files,
            fail_verification: false,
        };
        let (_, outcome) = zup_transaction::recover(
            pending,
            &store,
            &mut executor,
            &crate::InstallationRecordStore::new(&state),
        )
        .unwrap();
        if point == "fixed_instance_published" {
            assert_eq!(outcome, TransactionOutcome::Committed);
            assert_eq!(fs::read(&destination).unwrap(), b"owned payload");
        } else {
            assert_eq!(outcome, TransactionOutcome::RolledBack);
            assert_eq!(file_identity(&destination).unwrap().unwrap().0, old);
            assert_eq!(fs::read(&destination).unwrap(), b"previous payload");
        }
    }

    #[rstest::rstest]
    #[case::same_instance(false)]
    #[case::foreign_equal_bytes(true)]
    fn retained_files_publish_new_contracts_without_changing_ownership(#[case] foreign: bool) {
        use zup_transaction::{
            FilesystemTransactionStore, TransactionCoordinator, TransactionOutcome,
        };
        let root = tempfile::tempdir_in(crate::host_dirs::user_data().unwrap()).unwrap();
        let (mut files, intent) = fixture(root.path());
        fs::write(root.path().join("payload.bin"), b"owned payload").unwrap();
        let state = root.path().join("state");
        let publisher = crate::InstallationRecordStore::new(&state);
        let pending = TransactionCoordinator::new(FilesystemTransactionStore::private_at(&state))
            .begin(
                intent.app_id.clone(),
                intent.scope,
                intent.app_version.clone(),
                intent.plan,
            )
            .unwrap();
        files.tx_id = pending.transaction_id;
        let mut executor = NativeFiles {
            files,
            fail_verification: false,
        };
        assert_eq!(
            pending.execute(&mut executor, &publisher).unwrap().outcome,
            TransactionOutcome::Committed
        );
        let previous = publisher
            .load(&intent.app_id, intent.scope)
            .unwrap()
            .unwrap();
        let (key, original) = previous.current.resources.iter().next().unwrap();
        let zup_plan::OwnedResource::File {
            instance,
            destination,
            sha256,
            size,
            ..
        } = original
        else {
            unreachable!()
        };
        let path = host_path(destination);
        if foreign {
            fs::rename(&path, root.path().join("original-instance.bin")).unwrap();
            fs::write(&path, b"owned payload").unwrap();
        }
        let observed = file_identity(&path).unwrap().unwrap();
        let mut effects = DeploymentEffects::new(
            previous.id,
            crate::requirements_fixture::requirements(
                previous.target,
                zup_core::SelectedScope::User,
                "com.zup.compensation",
                "1.0.0",
            ),
        );
        effects.install_directory = Some(previous.directory.clone());
        effects.observed = Some(previous.current.selection());
        effects.observed_transaction = Some(previous.current.transaction);
        effects.files.push(FileOperation {
            key: key.clone(),
            kind: FileOperationKind::NoOp,
            destination: destination.clone(),
            source_relative: RelativePath::new("repacked.bin").unwrap(),
            replacement: zup_core::FixedReplacement::Quiescent,
            precondition: FilePrecondition::Exact {
                object: instance.object,
                size: *size,
                sha256: *sha256,
            },
            expected_sha256: *sha256,
            expected_size: *size,
            privilege: Privilege::User,
            executable: false,
            conflict: None,
        });
        let plan = compile_deployment(&effects).unwrap();
        publisher
            .validate_plan(&intent.app_id, intent.scope, &plan)
            .unwrap();
        let pending = TransactionCoordinator::new(FilesystemTransactionStore::private_at(&state))
            .begin(
                intent.app_id.clone(),
                intent.scope,
                intent.app_version,
                plan,
            )
            .unwrap();
        let files = WindowsFileExecutor::new(
            zup_bundle::DirectoryPayloadSource::new(root.path().join("unavailable-payload")),
            pending.transaction_id,
            ".exe",
        );
        let mut executor = NativeFiles {
            files,
            fail_verification: false,
        };
        let settled = pending.execute(&mut executor, &publisher).unwrap();
        let current = publisher
            .load(&intent.app_id, intent.scope)
            .unwrap()
            .unwrap();
        if foreign {
            assert_eq!(settled.outcome, TransactionOutcome::RolledBack);
            assert_eq!(&current, &previous);
        } else {
            assert_eq!(settled.outcome, TransactionOutcome::Committed);
            let zup_plan::OwnedResource::File {
                instance: retained,
                source_relative,
                replacement,
                ..
            } = &current.current.resources[key]
            else {
                unreachable!()
            };
            assert_eq!(retained, instance);
            assert_eq!(source_relative.as_str(), "repacked.bin");
            assert_eq!(*replacement, zup_core::FixedReplacement::Quiescent);
            assert!(current.references_transaction(instance.origin));
        }
        assert_eq!(file_identity(&path).unwrap(), Some(observed));
    }
}
