use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
#[cfg(test)]
use zup_core::TargetPath;
use zup_core::TransactionId;
use zup_plan::RemovalKind;
use zup_plan::{DeploymentNode, DeploymentPlan, FileRemoval, NodeKind, OperationId};

use zup_bundle::{PayloadError, PayloadSource};
use zup_core::{RelativePath, ResourceKey, Sha256Digest, hash_reader};
use zup_plan::{HostSnapshot, ObservedFile, ObservedFileState};

use zup_plan::{FileOperationKind, FilePrecondition};
use zup_transaction::{OperationExecutor, OperationReceipt, ReconcileResult};

use crate::error::{ExecError, PathError};
use crate::fs::{
    EXECUTABLE_PAYLOAD_MODE, EntryKind, OwnedDirectory, PAYLOAD_FILE_MODE, StreamedRegular,
    USER_INSTALL_DIRECTORY_MODE,
};
use crate::lowering::to_host_path;

#[derive(Debug, Clone)]
pub struct FileOperation {
    pub destination: zup_core::TargetPath,

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

#[derive(Clone, Copy)]
struct ExpectedFile {
    object: zup_core::NativeObjectIdentity,
    size: u64,
    sha256: Sha256Digest,
}

impl ExpectedFile {
    fn matches(self, current: &StreamedRegular) -> bool {
        current.object() == self.object
            && current.size == self.size
            && current.sha256 == self.sha256
    }
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
    state: Option<crate::state::StateAuthority>,
    persistent:
        Option<zup_transaction::PersistentStorage<crate::persistent::LinuxPersistentFilesystem>>,
    caches: BTreeMap<OperationId, crate::refresh::PreparedCache>,
    generations:
        Option<zup_transaction::GenerationStorage<crate::generation::LinuxGenerationFilesystem>>,
    openrc_executables: std::collections::BTreeSet<PathBuf>,
    service_units: Vec<String>,
    files: BTreeMap<String, FileOperation>,
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
    manager: Option<Box<dyn crate::systemd::SystemdManager>>,
    openrc: Option<crate::openrc_native::Openrc>,
    expected_uid: u32,
}

impl ServiceSupport {
    pub fn production() -> Result<Self, String> {
        Self::for_host(
            crate::locations::MachineRoots::production(),
            crate::machine::SystemdRoots::production(),
        )
    }

    pub fn for_host(
        roots: crate::locations::MachineRoots,
        systemd: crate::machine::SystemdRoots,
    ) -> Result<Self, String> {
        if crate::openrc_native::active() {
            let openrc = crate::openrc_native::Openrc::connect(roots.clone())?;
            return Ok(Self {
                roots,
                systemd,
                manager: None,
                openrc: Some(openrc),
                expected_uid: rustix::process::geteuid().as_raw(),
            });
        }
        Ok(Self::new(
            roots,
            systemd,
            Box::new(crate::systemd::RealSystemd::connect().map_err(|error| error.to_string())?),
            rustix::process::geteuid().as_raw(),
        ))
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
            manager: Some(manager),
            openrc: None,
            expected_uid,
        }
    }
}

impl Default for LinuxFileExecutor {
    fn default() -> Self {
        Self {
            state: None,
            persistent: None,
            caches: BTreeMap::new(),
            generations: None,
            openrc_executables: Default::default(),
            service_units: Vec::new(),
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

    pub fn set_state_authority(
        &mut self,
        root: PathBuf,
        app: zup_core::AppId,
        scope: zup_core::SelectedScope,
    ) {
        let state = crate::state::StateAuthority { root, app, scope };
        self.state = Some(state.clone());
        self.persistent = Some(zup_transaction::PersistentStorage(
            crate::persistent::LinuxPersistentFilesystem(state.clone()),
        ));
        self.generations = Some(zup_transaction::GenerationStorage::new(
            crate::generation::LinuxGenerationFilesystem::new(state),
        ));
    }

    fn persistent_storage(
        &self,
    ) -> Result<
        &zup_transaction::PersistentStorage<crate::persistent::LinuxPersistentFilesystem>,
        ExecError,
    > {
        self.persistent.as_ref().ok_or_else(|| {
            ExecError::refused(
                "persistent data",
                "protected transaction storage is required",
            )
        })
    }

    fn generation_storage(
        &self,
    ) -> Result<
        &zup_transaction::GenerationStorage<crate::generation::LinuxGenerationFilesystem>,
        ExecError,
    > {
        self.generations.as_ref().ok_or_else(|| {
            ExecError::Executor(
                "generation effects require the protected installation state authority".into(),
            )
        })
    }

    pub fn set_transaction_id(&mut self, id: TransactionId) {
        self.transaction = Some(id);
    }

    fn transaction_id(&self) -> Result<TransactionId, ExecError> {
        self.transaction.ok_or_else(|| {
            ExecError::Executor("file operations need a transaction identity".into())
        })
    }

    pub fn register(&mut self, id: &OperationId, file: FileOperation) {
        self.files.insert(id.to_string(), file);
    }

    pub fn register_plan(&mut self, plan: &DeploymentPlan) -> Result<(), ExecError> {
        self.openrc_executables.clear();
        for node in &plan.nodes {
            if let NodeKind::ServiceControl { operation } = &node.kind
                && let zup_plan::ServiceState::Registration {
                    execution: zup_core::ServiceExecution::OpenrcUser { .. },
                    command,
                    ..
                } = &operation.configuration
            {
                self.openrc_executables
                    .insert(to_host_path(&command.executable)?);
            }
            if let NodeKind::NativeService { operation } = &node.kind
                && let zup_plan::NativeServiceOperation::Configure { service, .. } =
                    operation.as_ref()
                && matches!(
                    service.execution,
                    zup_core::ServiceExecution::OpenrcUser { .. }
                )
            {
                self.openrc_executables
                    .insert(to_host_path(&service.command.executable)?);
            }
        }
        self.service_units = plan
            .nodes
            .iter()
            .filter_map(|node| match &node.kind {
                NodeKind::ServiceControl { operation } => Some(operation.native_name.clone()),
                _ => None,
            })
            .collect();
        self.service_units.sort();
        self.service_units.dedup();
        for node in &plan.nodes {
            if !matches!(
                node.kind,
                NodeKind::StageFile { .. }
                    | NodeKind::FileMutation { .. }
                    | NodeKind::FileRemoval { .. }
            ) {
                continue;
            }
            let destination =
                node.file_destination()
                    .cloned()
                    .ok_or_else(|| ExecError::PlanDrift {
                        path: node.id.to_string(),
                        reason: "file effect has no validated physical placement".into(),
                    })?;
            let host_path = to_host_path(&destination).map_err(|error| ExecError::PlanDrift {
                path: destination.to_string(),
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
                FileOperation {
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

    pub fn file(&self, id: &str) -> Result<&FileOperation, ExecError> {
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
        self.validate_openrc_executable(&file, &directory, &name)?;
        Ok(OperationReceipt::StageFile {
            object: directory.stream_regular(&name)?.object(),
            staged_path: staged.display().to_string(),
            size: file.intent.size,
            sha256: file.intent.sha256,
        })
    }

    fn cache(
        &mut self,
        node: &DeploymentNode,
    ) -> Result<&crate::refresh::PreparedCache, ExecError> {
        let NodeKind::NativeCache { operation } = &node.kind else {
            return Err(ExecError::refused(
                node.id.as_str(),
                "operation is not a native cache effect",
            ));
        };
        if !self.caches.contains_key(&node.id) {
            self.caches.insert(
                node.id.clone(),
                crate::refresh::PreparedCache::prepare(operation)?,
            );
        }
        Ok(&self.caches[&node.id])
    }

    pub fn prepare(&mut self, node: &DeploymentNode) -> Result<(), ExecError> {
        match &node.kind {
            NodeKind::Cleanup { operation } => {
                let state = self.state.as_ref().ok_or_else(|| {
                    ExecError::refused(
                        node.id.as_str(),
                        "cleanup requires protected transaction authority",
                    )
                })?;
                let source = state.source(self.transaction_id()?)?;
                operation
                    .validate(source.target)
                    .map_err(|reason| ExecError::refused(node.id.as_str(), reason))
            }
            NodeKind::PersistentDirectory { operation } => {
                if let zup_plan::PersistentBinding::SystemdState(binding) =
                    &operation.intent.binding
                {
                    return crate::persistent::validate_native(binding);
                }
                zup_transaction::PersistentFilesystem::parent(
                    &self.persistent_storage()?.0,
                    operation,
                )
                .map(|_| ())
            }
            NodeKind::NativeService { operation } => self
                .native_service_context()?
                .prepare_registration(operation),
            NodeKind::NativeCache { .. } => self.cache(node).map(|_| ()),
            NodeKind::Retention { .. }
            | NodeKind::ReserveInstallation
            | NodeKind::PublishInstallation => Err(ExecError::refused(
                node.id.as_str(),
                "installation publication belongs to the coordinator",
            )),
            NodeKind::Generation { intent, .. } => intent
                .validate(intent.materialization.descriptor().target())
                .map_err(|error| ExecError::refused(node.id.as_str(), error.to_string())),
            NodeKind::ServiceControl { operation } => operation
                .validate()
                .map_err(|reason| ExecError::refused(&operation.native_name, reason)),
            NodeKind::Barrier => Ok(()),
            NodeKind::RetainFile { .. } => Ok(()),
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
                    FilePrecondition::Exact {
                        object,
                        size,
                        sha256,
                    } => verify_precondition(&file.host_path, object, size, sha256),
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
            NodeKind::BackendOperation { .. } => Err(ExecError::refused(
                node.id.as_str(),
                "unsupported Linux backend operation",
            )),
            NodeKind::BackendRemoval { .. } => Err(ExecError::PlanDrift {
                path: node.id.to_string(),
                reason: "a backend removal is not a Linux operation".into(),
            }),
        }
    }

    fn retained_reader(
        &self,
        node: &DeploymentNode,
        transaction: &zup_transaction::TransactionRecord,
        size: u64,
        sha256: Sha256Digest,
    ) -> Result<Option<zup_bundle::PayloadReader>, ExecError> {
        let Some(storage) = &self.persistent else {
            return Ok(None);
        };
        storage.0.0.validate(transaction)?;
        let root = &storage.0.0.root;
        let installed = crate::ledger::LinuxInstallationStore::new(root)
            .load(&transaction.app_id, transaction.scope)?;
        let Some(installed) = installed else {
            return Ok(None);
        };
        let store = zup_transaction::FilesystemTransactionStore::private_at(root);
        let sources = zup_transaction::retained_file_sources(&store, &installed, transaction, node)
            .map_err(|error| ExecError::refused("retained content", error.to_string()))?;
        for source in sources {
            let path = crate::to_host_path(&source.path)?;
            let parent = path
                .parent()
                .ok_or_else(|| ExecError::refused("retained content", "source has no parent"))?;
            crate::fs::verify_ancestor_trust(parent, rustix::process::geteuid().as_raw())?;
            let (directory, name) = crate::fs::open_parent(&path)?;
            if let Ok(mut file) = stream_expected(&directory, &name, &path, size, sha256)
                && file.object() == source.object
                && file.uid == rustix::process::geteuid().as_raw()
                && file.mode & 0o022 == 0
            {
                use std::io::Seek;
                file.file
                    .rewind()
                    .map_err(|error| ExecError::refused("retained content", error.to_string()))?;
                return Ok(Some(Box::new(file.file)));
            }
        }
        Ok(None)
    }

    fn stage_node(
        &mut self,
        node: &DeploymentNode,
        transaction: &zup_transaction::TransactionRecord,
    ) -> Result<OperationReceipt, ExecError> {
        let file = self.file(node.id.as_str())?.clone();
        let (source, sha256, size) = Self::payload_identity(node)?;
        let mut reader =
            if let Some(reader) = self.retained_reader(node, transaction, size, sha256)? {
                reader
            } else {
                self.payload
                    .as_ref()
                    .ok_or_else(|| ExecError::PlanDrift {
                        path: node.id.to_string(),
                        reason: "no authenticated content source is available".into(),
                    })?
                    .open(&source, &sha256, size)
                    .map_err(|error| ExecError::Payload {
                        path: source.to_string(),
                        source: error,
                    })?
            };
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
        self.validate_openrc_executable(&file, &directory, &name)?;
        Ok(OperationReceipt::StageFile {
            object: directory.stream_regular(&name)?.object(),
            staged_path: staged.display().to_string(),
            size: file.intent.size,
            sha256: file.intent.sha256,
        })
    }

    fn validate_openrc_executable(
        &self,
        file: &FileOperation,
        directory: &OwnedDirectory,
        name: &str,
    ) -> Result<(), ExecError> {
        if self.openrc_executables.contains(&file.host_path) {
            crate::openrc::validate_executable(
                &directory.read_regular(name)?,
                file.destination.target(),
            )
            .map_err(|reason| ExecError::refused(&file.host_path.display().to_string(), reason))?;
        }
        Ok(())
    }

    fn payload_identity(
        node: &DeploymentNode,
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

    fn payload_boundary(&self, node: &DeploymentNode) -> Result<Option<OwnedDirectory>, ExecError> {
        let Some((state, destination)) = self.state.as_ref().zip(node.file_destination()) else {
            return Ok(None);
        };
        if node.meta.privilege != Some(state.scope.authorization()) {
            return Err(ExecError::refused(
                node.id.as_str(),
                "file authority differs from its protected installation state",
            ));
        }
        let path = to_host_path(destination)?;
        let parent = path
            .parent()
            .ok_or_else(|| ExecError::refused(node.id.as_str(), "file has no parent"))?;
        state.payload_directory(parent).map(Some)
    }

    pub fn apply(
        &mut self,
        node: &DeploymentNode,
        transaction: &zup_transaction::TransactionRecord,
    ) -> Result<OperationReceipt, ExecError> {
        if let Some(state) = &self.state {
            state.validate(transaction)?;
        }
        let _state_boundary = self.payload_boundary(node)?;
        if self.transaction_id()? != transaction.transaction_id
            || !transaction.plan.nodes.contains(node)
        {
            return Err(ExecError::refused(
                node.id.as_str(),
                "operation is not part of this authenticated transaction",
            ));
        }
        match &node.kind {
            NodeKind::Cleanup { .. } => crate::cleanup::apply(
                self.state.as_ref().ok_or_else(|| {
                    ExecError::refused(
                        node.id.as_str(),
                        "cleanup requires protected transaction storage",
                    )
                })?,
                node,
                transaction,
            ),
            NodeKind::PersistentDirectory { operation } => self
                .persistent_storage()?
                .apply(operation, transaction)
                .map(|owned| OperationReceipt::PersistentDirectory(Box::new(owned))),
            NodeKind::NativeService { operation } => {
                let source_retained = operation
                    .source_node(&transaction.plan.nodes)
                    .map_err(|error| ExecError::refused(node.id.as_str(), error.to_string()))?
                    .is_some_and(|source| matches!(source.kind, NodeKind::RetainFile { .. }));
                self.native_service_context()?
                    .apply_registration(operation, source_retained)
            }
            NodeKind::NativeCache { operation } => self.cache(node)?.run(operation),
            NodeKind::Retention { .. }
            | NodeKind::ReserveInstallation
            | NodeKind::PublishInstallation => Err(ExecError::refused(
                node.id.as_str(),
                "installation publication belongs to the coordinator",
            )),
            NodeKind::Generation { operation, intent } => self.generation_storage()?.apply(
                *operation,
                intent,
                self.transaction_id()?,
                self.payload.as_deref(),
            ),
            NodeKind::ServiceControl { operation } => zup_transaction::apply_service_control(
                &mut self.native_service_context()?,
                operation,
            )
            .map_err(|reason| ExecError::refused(&operation.native_name, reason)),
            NodeKind::Barrier => Ok(OperationReceipt::Control),
            NodeKind::RetainFile { .. } => self.retain_file(node),
            NodeKind::StageFile { .. } => self.stage_node(node, transaction),
            NodeKind::FileMutation { delta, .. } => match delta {
                FileOperationKind::Create | FileOperationKind::RestoreOwned => {
                    self.create(node, transaction)
                }
                FileOperationKind::Replace | FileOperationKind::RepairOwned => {
                    self.replace(node, transaction)
                }
                FileOperationKind::NoOp => Ok(OperationReceipt::Control),
                other => Err(ExecError::PlanDrift {
                    path: node.id.to_string(),
                    reason: format!("{other:?} is not applied by the file executor"),
                }),
            },
            NodeKind::FileRemoval { .. } => self.remove(node),
            NodeKind::BackendOperation { .. } => Err(ExecError::refused(
                node.id.as_str(),
                "unsupported Linux backend operation",
            )),
            NodeKind::BackendRemoval { .. } => Err(ExecError::PlanDrift {
                path: node.id.to_string(),
                reason: "a backend removal is not a Linux operation".into(),
            }),
        }
    }

    pub fn verify(
        &mut self,
        node: &DeploymentNode,
        receipt: &OperationReceipt,
    ) -> Result<(), ExecError> {
        match receipt {
            OperationReceipt::Cleanup { .. } => crate::cleanup::verify(
                self.state.as_ref().ok_or_else(|| {
                    ExecError::refused(
                        node.id.as_str(),
                        "cleanup requires protected transaction storage",
                    )
                })?,
                self.transaction_id()?,
                node,
                receipt,
            ),
            OperationReceipt::PersistentDirectory(owned) => {
                let NodeKind::PersistentDirectory { operation } = &node.kind else {
                    return Err(ExecError::refused(
                        node.id.as_str(),
                        "persistent directory receipt requires its typed operation",
                    ));
                };
                self.persistent_storage()?.verify(operation, owned)
            }
            OperationReceipt::NativeService(_)
            | OperationReceipt::NativeServiceCompensation { .. } => {
                let operation = crate::native_service::operation(node)?;
                self.native_service_context()?
                    .verify_registration(operation, receipt)
            }
            OperationReceipt::NativeCache { .. } => {
                let operation = cache_intent(node, receipt)?;
                self.cache(node)?.verify(operation, receipt)
            }
            OperationReceipt::RetainedFile { .. } => {
                if self.retain_file(node)? != *receipt {
                    return Err(ExecError::refused(
                        node.id.as_str(),
                        "retained file differs from its authorized observation",
                    ));
                }
                Ok(())
            }
            OperationReceipt::Generation(receipt) => {
                let intent = generation_intent(node, receipt)?;
                self.generation_storage()?.verify(intent, receipt)
            }
            OperationReceipt::ServiceControl(receipt) => zup_transaction::verify_service_control(
                &mut self.native_service_context()?,
                receipt,
            )
            .map_err(|reason| ExecError::refused(&receipt.operation.native_name, reason)),
            OperationReceipt::CreateFile {
                destination,
                installed_sha256,
                installed_size,
                executable,
                object,
                ..
            }
            | OperationReceipt::ReplaceFile {
                destination,
                new_sha256: installed_sha256,
                new_size: installed_size,
                executable,
                object,
                ..
            } => {
                let expected = FileIntent {
                    sha256: *installed_sha256,
                    size: *installed_size,
                    executable: *executable,
                };
                let current = verify_installed(
                    Path::new(destination),
                    expected,
                    self.executable_mode_for(expected),
                )?;
                if current.object() != *object || !receipt_backup_valid(receipt) {
                    return Err(ExecError::Verification {
                        path: destination.clone(),
                        reason: "the published file or its rollback instance has changed".into(),
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
            OperationReceipt::Backend { .. } => Err(ExecError::refused(
                node.id.as_str(),
                "unsupported Linux backend receipt",
            )),
        }
    }

    pub fn rollback(
        &mut self,
        node: &DeploymentNode,
        receipt: &OperationReceipt,
    ) -> Result<(), ExecError> {
        let _state_boundary = self.payload_boundary(node)?;
        if matches!(
            node.kind,
            NodeKind::FileMutation {
                delta: FileOperationKind::RepairOwned,
                ..
            }
        ) {
            return Err(ExecError::refused(
                node.id.as_str(),
                "repair displaced an externally modified file; automatic restoration is unsafe; retained instances require recovery",
            ));
        }
        match receipt {
            OperationReceipt::Cleanup { .. } => Err(ExecError::refused(
                node.id.as_str(),
                "committed cleanup cannot be compensated",
            )),
            OperationReceipt::PersistentDirectory(_) => self.verify(node, receipt),
            OperationReceipt::NativeService(_)
            | OperationReceipt::NativeServiceCompensation { .. } => {
                let operation = crate::native_service::operation(node)?;
                self.native_service_context()?
                    .rollback_registration(operation, receipt)
            }
            OperationReceipt::NativeCache { .. } => {
                let operation = cache_intent(node, receipt)?;
                self.cache(node)?.run(operation).map(|_| ())
            }
            OperationReceipt::RetainedFile { .. } => Ok(()),
            OperationReceipt::Generation(receipt) => {
                let intent = generation_intent(node, receipt)?;
                self.generation_storage()?
                    .retain_after_compensation(intent, receipt)
            }
            OperationReceipt::ServiceControl(receipt) => zup_transaction::rollback_service_control(
                &mut self.native_service_context()?,
                receipt,
            )
            .map_err(|reason| ExecError::refused(&receipt.operation.native_name, reason)),
            OperationReceipt::CreateFile {
                destination,
                installed_sha256,
                installed_size,
                executable,
                created_directories: _,
                object,
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
                    sha256: *installed_sha256,
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
                        if current.mode != expected_mode || current.object() != *object {
                            return Err(ExecError::RollbackDrift {
                                path: path.display().to_string(),
                                reason: "the file is no longer the one this transaction wrote"
                                    .into(),
                            });
                        }
                        directory.remove_file(&name)?;
                        directory.sync()?;

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
                object,
                previous_object,
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
                    sha256: *previous_sha256,
                    size: *previous_size,
                    executable: previous_mode & 0o111 != 0,
                };
                let new = FileIntent {
                    sha256: *new_sha256,
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
                    ExpectedFile {
                        object: *previous_object,
                        size: previous.size,
                        sha256: previous.sha256,
                    },
                    ExpectedFile {
                        object: *object,
                        size: new.size,
                        sha256: new.sha256,
                    },
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
                        if saved.object() != *previous_object {
                            return Err(ExecError::RollbackDrift {
                                path: path.display().to_string(),
                                reason: "the backup is not the original owned instance".into(),
                            });
                        }
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
                        let restored =
                            verify_previous(&path, previous.size, previous.sha256, *previous_mode)?;
                        if restored.object() != *previous_object {
                            return Err(ExecError::RollbackDrift {
                                path: path.display().to_string(),
                                reason: "the restored file is not the original owned instance"
                                    .into(),
                            });
                        }
                        Ok(())
                    }
                    Current::MatchesPrevious => {
                        if backup_holds(
                            &directory,
                            &backup,
                            *previous_object,
                            previous.size,
                            previous.sha256,
                        ) {
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
                        let restored = verify_installed(&path, previous, *previous_mode)?;
                        if restored.object() != *previous_object {
                            return Err(ExecError::RollbackDrift {
                                path: path.display().to_string(),
                                reason: "the restored file is not the original owned instance"
                                    .into(),
                            });
                        }
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
                        if saved.object() != *previous_object {
                            return Err(ExecError::RollbackDrift {
                                path: path.display().to_string(),
                                reason: "the backup is not the original owned instance".into(),
                            });
                        }
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
                            let displaced =
                                stream_expected(&directory, &backup, &path, new.size, new.sha256)?;
                            if displaced.object() != *object {
                                return Err(ExecError::RollbackDrift {
                                    path: directory.path().join(&backup).display().to_string(),
                                    reason: "the displaced file is not the published instance"
                                        .into(),
                                });
                            }
                            directory.remove_file(&backup)?;
                            directory.sync()?;
                        } else {
                            return Err(ExecError::RollbackDrift {
                                path: path.display().to_string(),
                                reason: "filesystem cannot atomically restore the owned instance; recovery requires an authenticated quiescence boundary".into(),
                            });
                        }
                        if saved.mode != *previous_mode {
                            directory.chmod_regular(&name, mode_from(*previous_mode))?;
                            let _ = directory.sync();
                        }
                        let restored = verify_installed(&path, previous, *previous_mode)?;
                        if restored.object() != *previous_object {
                            return Err(ExecError::RollbackDrift {
                                path: path.display().to_string(),
                                reason: "the restored file is not the original owned instance"
                                    .into(),
                            });
                        }
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
                object,
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

                match removed_state(&directory, &name, &path, *object, *size, *sha256)? {
                    Restored::Already => {
                        if backup_holds(&directory, &backup, *object, *size, *sha256) {
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
                        if saved.object() != *object {
                            return Err(ExecError::RollbackDrift {
                                path: path.display().to_string(),
                                reason: "the backup is not the removed owned instance".into(),
                            });
                        }
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
                        let restored = verify_previous(&path, *size, *sha256, *mode)?;
                        if restored.object() != *object {
                            return Err(ExecError::RollbackDrift {
                                path: path.display().to_string(),
                                reason: "the restored file is not the removed owned instance"
                                    .into(),
                            });
                        }
                        Ok(())
                    }
                    Restored::Foreign => Err(ExecError::RollbackDrift {
                        path: path.display().to_string(),
                        reason: "the destination was modified outside this transaction".into(),
                    }),
                }
            }
            OperationReceipt::StageFile { object, .. } => self.discard_staged(node, *object),
            OperationReceipt::Control => Ok(()),
            OperationReceipt::Backend { .. } => Ok(()),
        }
    }

    pub fn reconcile(
        &mut self,
        node: &DeploymentNode,
        receipt: Option<&OperationReceipt>,
        transaction: &zup_transaction::TransactionRecord,
    ) -> Result<ReconcileResult, ExecError> {
        if let Some(state) = &self.state {
            state.validate(transaction)?;
        }
        let _state_boundary = self.payload_boundary(node)?;
        if self.transaction_id()? != transaction.transaction_id
            || !transaction.plan.nodes.contains(node)
        {
            return Err(ExecError::refused(
                node.id.as_str(),
                "operation is not part of this authenticated transaction",
            ));
        }
        let Some(receipt) = receipt else {
            return self.reconcile_interrupted(node, transaction);
        };
        match receipt {
            OperationReceipt::Cleanup { .. } => self
                .verify(node, receipt)
                .map(|()| ReconcileResult::AppliedWithReceipt(receipt.clone())),
            OperationReceipt::PersistentDirectory(_) => self
                .verify(node, receipt)
                .map(|_| ReconcileResult::AppliedWithReceipt(receipt.clone())),
            OperationReceipt::NativeService(_)
            | OperationReceipt::NativeServiceCompensation { .. } => {
                let operation = crate::native_service::operation(node)?;
                Ok(
                    if self
                        .native_service_context()?
                        .verify_registration(operation, receipt)
                        .is_ok()
                    {
                        ReconcileResult::AppliedWithReceipt(receipt.clone())
                    } else {
                        ReconcileResult::Ambiguous
                    },
                )
            }
            OperationReceipt::NativeCache { .. } => {
                let _ = cache_intent(node, receipt)?;
                Ok(ReconcileResult::AppliedWithReceipt(receipt.clone()))
            }
            OperationReceipt::RetainedFile { .. } => Ok(if self.verify(node, receipt).is_ok() {
                ReconcileResult::AppliedWithReceipt(receipt.clone())
            } else {
                ReconcileResult::Ambiguous
            }),
            OperationReceipt::Generation(receipt) => {
                let intent = generation_intent(node, receipt)?;
                self.generation_storage()?
                    .verify(intent, receipt)
                    .map(|()| ReconcileResult::Applied)
            }
            OperationReceipt::ServiceControl(receipt) => {
                zup_transaction::reconcile_service_control(
                    &mut self.native_service_context()?,
                    &receipt.operation,
                    Some(receipt),
                )
                .map_err(|reason| ExecError::refused(&receipt.operation.native_name, reason))
            }
            OperationReceipt::CreateFile {
                destination,
                installed_sha256,
                installed_size,
                executable,
                object,
                ..
            }
            | OperationReceipt::ReplaceFile {
                destination,
                new_sha256: installed_sha256,
                new_size: installed_size,
                executable,
                object,
                ..
            } => {
                let expected = FileIntent {
                    sha256: *installed_sha256,
                    size: *installed_size,
                    executable: *executable,
                };
                let expected_mode = (expected.mode_for(self.executable_mode)).bits() & 0o7777;
                match observe(Path::new(destination))? {
                    None => Ok(ReconcileResult::Ambiguous),
                    Some(_)
                        if verify_installed(Path::new(destination), expected, expected_mode)
                            .is_ok_and(|current| current.object() == *object)
                            && receipt_backup_valid(receipt) =>
                    {
                        Ok(ReconcileResult::AppliedWithReceipt(receipt.clone()))
                    }
                    Some(_) => Ok(ReconcileResult::Ambiguous),
                }
            }
            OperationReceipt::RemoveFile { destination, .. } => {
                let path = PathBuf::from(destination.to_string());
                match observe(&path)? {
                    None if receipt_backup_valid(receipt) => {
                        Ok(ReconcileResult::AppliedWithReceipt(receipt.clone()))
                    }
                    None => Ok(ReconcileResult::Ambiguous),
                    Some(_) => Ok(ReconcileResult::Ambiguous),
                }
            }
            OperationReceipt::Control | OperationReceipt::StageFile { .. } => {
                Ok(ReconcileResult::AppliedWithReceipt(receipt.clone()))
            }

            OperationReceipt::Backend { .. } => Ok(ReconcileResult::NotApplied),
        }
    }

    fn reconcile_interrupted(
        &mut self,
        node: &DeploymentNode,
        evidence: &zup_transaction::TransactionRecord,
    ) -> Result<ReconcileResult, ExecError> {
        match &node.kind {
            NodeKind::Cleanup { .. } => crate::cleanup::apply(
                self.state.as_ref().ok_or_else(|| {
                    ExecError::refused(
                        node.id.as_str(),
                        "cleanup requires protected transaction storage",
                    )
                })?,
                node,
                evidence,
            )
            .map(ReconcileResult::AppliedWithReceipt),
            NodeKind::PersistentDirectory { operation } => {
                self.persistent_storage()?.reconcile(operation, evidence)
            }
            NodeKind::NativeService { operation } => self
                .native_service_context()?
                .reconcile_registration(operation),
            NodeKind::NativeCache { operation } => Ok(ReconcileResult::AppliedWithReceipt(
                OperationReceipt::NativeCache {
                    intent: operation.fingerprint(),
                    completion: zup_transaction::NativeCacheCompletion::CompensationOnly,
                },
            )),
            NodeKind::Retention { .. }
            | NodeKind::ReserveInstallation
            | NodeKind::PublishInstallation => Ok(ReconcileResult::Ambiguous),
            NodeKind::Generation { operation, intent } => {
                self.generation_storage()?
                    .reconcile(*operation, intent, self.transaction_id()?)
            }
            NodeKind::ServiceControl { operation } => zup_transaction::reconcile_service_control(
                &mut self.native_service_context()?,
                operation,
                None,
            )
            .map_err(|reason| ExecError::refused(&operation.native_name, reason)),
            NodeKind::Barrier => Ok(ReconcileResult::Applied),
            NodeKind::RetainFile { .. } => Ok(match self.retain_file(node) {
                Ok(receipt) => ReconcileResult::AppliedWithReceipt(receipt),
                Err(_) => ReconcileResult::NotApplied,
            }),

            NodeKind::StageFile { .. } => {
                let file = self.file(node.id.as_str())?;
                match crate::fs::open_parent(&file.host_path) {
                    Err(error) if error.is_not_found() => Ok(ReconcileResult::NotApplied),
                    Err(error) => Err(error.into()),
                    Ok((parent, _)) => match parent.kind_or_absent(&staged_name(&node.id))? {
                        None => Ok(ReconcileResult::NotApplied),
                        Some(_) => Ok(ReconcileResult::Ambiguous),
                    },
                }
            }
            NodeKind::FileMutation { delta, .. } => match delta {
                FileOperationKind::Create | FileOperationKind::RestoreOwned => {
                    self.reconcile_interrupted_create(node, evidence)
                }
                FileOperationKind::Replace | FileOperationKind::RepairOwned => {
                    self.reconcile_interrupted_replace(node, evidence)
                }
                FileOperationKind::NoOp => Ok(ReconcileResult::NotApplied),
                FileOperationKind::Conflict | FileOperationKind::Drift => {
                    Ok(ReconcileResult::Ambiguous)
                }
            },
            NodeKind::FileRemoval { .. } => self.reconcile_interrupted_removal(node),

            NodeKind::BackendOperation { .. } => Ok(ReconcileResult::Ambiguous),
            NodeKind::BackendRemoval { .. } => Ok(ReconcileResult::Ambiguous),
        }
    }

    fn reconcile_interrupted_create(
        &mut self,
        node: &DeploymentNode,
        evidence: &zup_transaction::TransactionRecord,
    ) -> Result<ReconcileResult, ExecError> {
        let key = mutation_key(node)?;
        let Some(OperationReceipt::StageFile {
            object: staged_object,
            ..
        }) = evidence.receipt(&OperationId::stage_file(&key))
        else {
            return Ok(ReconcileResult::Ambiguous);
        };
        if evidence.transaction_id != self.transaction_id()? {
            return Ok(ReconcileResult::Ambiguous);
        }
        let file = self.file(node.id.as_str())?.clone();
        let expected_mode = self.executable_mode_for(file.intent);
        match observe(&file.host_path)? {
            None => Ok(ReconcileResult::NotApplied),
            Some(_)
                if verify_installed(&file.host_path, file.intent, expected_mode)
                    .is_ok_and(|file| file.object() == *staged_object) =>
            {
                Ok(ReconcileResult::AppliedWithReceipt(
                    OperationReceipt::CreateFile {
                        object: *staged_object,
                        destination: file.host_path.display().to_string(),
                        installed_sha256: file.intent.sha256,
                        installed_size: file.intent.size,
                        executable: file.intent.executable,
                        created_directories: Vec::new(),
                    },
                ))
            }
            Some(_) => Ok(ReconcileResult::Ambiguous),
        }
    }

    fn reconcile_interrupted_replace(
        &mut self,
        node: &DeploymentNode,
        evidence: &zup_transaction::TransactionRecord,
    ) -> Result<ReconcileResult, ExecError> {
        let key = mutation_key(node)?;
        let Some(OperationReceipt::StageFile {
            object: staged_object,
            ..
        }) = evidence.receipt(&OperationId::stage_file(&key))
        else {
            return Ok(ReconcileResult::Ambiguous);
        };
        if evidence.transaction_id != self.transaction_id()? {
            return Ok(ReconcileResult::Ambiguous);
        }
        let file = self.file(node.id.as_str())?.clone();
        let FilePrecondition::Exact {
            object,
            size,
            sha256,
        } = file.precondition
        else {
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
            Ok(backup)
                if backup.size == size
                    && backup.sha256 == sha256
                    && object
                        == (zup_core::NativeObjectIdentity::Linux {
                            device: backup.dev,
                            inode: backup.ino,
                        }) =>
            {
                Some(backup.mode)
            }
            _ => None,
        };
        let staged = directory.stream_regular(&staged_name).ok();
        let staged_old = staged.as_ref().is_some_and(|staged| {
            staged.size == size
                && staged.sha256 == sha256
                && object
                    == (zup_core::NativeObjectIdentity::Linux {
                        device: staged.dev,
                        inode: staged.ino,
                    })
        });
        let staged_new = staged.as_ref().is_some_and(|staged| {
            staged.object() == *staged_object
                && staged.size == file.intent.size
                && staged.sha256 == file.intent.sha256
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
                        Ok(found) if found.object() == *staged_object => Ok(
                            ReconcileResult::AppliedWithReceipt(OperationReceipt::ReplaceFile {
                                previous_object: object,
                                object: directory.stream_regular(&name)?.object(),
                                destination: file.host_path.display().to_string(),
                                previous_sha256: sha256,
                                previous_size: size,
                                previous_mode,
                                backup_path: directory
                                    .path()
                                    .join(&backup_name)
                                    .display()
                                    .to_string(),
                                new_sha256: file.intent.sha256,
                                new_size: file.intent.size,
                                executable: file.intent.executable,
                            }),
                        ),
                        _ => Ok(ReconcileResult::Ambiguous),
                    }
                }
                (Some(previous_mode), false) => {
                    match directory.rename_noreplace(&backup_name, &name) {
                        Ok(()) => {}
                        Err(_) => return Ok(ReconcileResult::Ambiguous),
                    }
                    match verify_previous(&file.host_path, size, sha256, previous_mode)
                        .and_then(|_| verify_precondition(&file.host_path, object, size, sha256))
                    {
                        Ok(()) => Ok(ReconcileResult::NotApplied),
                        Err(_) => Ok(ReconcileResult::Ambiguous),
                    }
                }
                _ => Ok(ReconcileResult::Ambiguous),
            },
            Some(_) if verify_precondition(&file.host_path, object, size, sha256).is_ok() => {
                Ok(ReconcileResult::NotApplied)
            }
            Some(_)
                if verify_installed(&file.host_path, file.intent, new_mode)
                    .is_ok_and(|file| file.object() == *staged_object) =>
            {
                if let Some(previous_mode) = backup_old {
                    return Ok(ReconcileResult::AppliedWithReceipt(
                        OperationReceipt::ReplaceFile {
                            previous_object: object,
                            object: directory.stream_regular(&name)?.object(),
                            destination: file.host_path.display().to_string(),
                            previous_sha256: sha256,
                            previous_size: size,
                            previous_mode,
                            backup_path: directory.path().join(&backup_name).display().to_string(),
                            new_sha256: file.intent.sha256,
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
                            previous_object: object,
                            object: directory.stream_regular(&name)?.object(),
                            destination: file.host_path.display().to_string(),
                            previous_sha256: sha256,
                            previous_size: size,
                            previous_mode: backup.mode,
                            backup_path: directory.path().join(&backup_name).display().to_string(),
                            new_sha256: file.intent.sha256,
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
        node: &DeploymentNode,
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
                        object: removal.instance.object,
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

    fn retain_file(&self, node: &DeploymentNode) -> Result<OperationReceipt, ExecError> {
        let invalid = || {
            ExecError::refused(
                node.id.as_str(),
                "retained file has no exact owned content contract",
            )
        };
        if !matches!(node.kind, NodeKind::RetainFile { .. }) {
            return Err(invalid());
        }
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
        let intent = FileIntent {
            size,
            sha256,
            executable: node.meta.executable.ok_or_else(invalid)?,
        };
        let path = to_host_path(destination)?;
        let observed = verify_installed(&path, intent, self.executable_mode_for(intent))?;
        if observed.object() != object {
            return Err(ExecError::refused(
                node.id.as_str(),
                "retained path is not the owned native instance",
            ));
        }
        Ok(OperationReceipt::RetainedFile {
            object,
            destination: destination.clone(),
            size,
            sha256,
        })
    }

    fn create(
        &mut self,
        node: &DeploymentNode,
        evidence: &zup_transaction::TransactionRecord,
    ) -> Result<OperationReceipt, ExecError> {
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
        let staged = self.ensure_staged(node, &file, &directory, &staged_name, evidence)?;
        rustix::fs::fchmod(&staged.file, mode)
            .map_err(|error| PathError::errno(&directory.path().join(&staged_name), error))?;
        staged
            .file
            .sync_all()
            .map_err(|error| PathError::io(&directory.path().join(&staged_name), error))?;
        let object = staged.object();
        directory.publish_exclusive(&name, &staged_name)?;

        let expected_mode = (mode).bits() & 0o7777;
        let published = verify_installed(&file.host_path, file.intent, expected_mode)?;
        if published.object() != object {
            return Err(ExecError::refused(
                node.id.as_str(),
                "published file is not the staged instance",
            ));
        }
        Ok(OperationReceipt::CreateFile {
            object,
            destination: file.host_path.display().to_string(),
            installed_sha256: file.intent.sha256,
            installed_size: file.intent.size,
            executable: file.intent.executable,
            created_directories,
        })
    }

    fn replace(
        &mut self,
        node: &DeploymentNode,
        evidence: &zup_transaction::TransactionRecord,
    ) -> Result<OperationReceipt, ExecError> {
        let file = self.file(node.id.as_str())?.clone();
        let FilePrecondition::Exact {
            object,
            size,
            sha256,
        } = file.precondition
        else {
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
        if object
            != (zup_core::NativeObjectIdentity::Linux {
                device: previous.dev,
                inode: previous.ino,
            })
        {
            return Err(ExecError::PlanDrift {
                path: file.host_path.display().to_string(),
                reason: "target object changed since snapshot".into(),
            });
        }
        occupy_backup_slot(&directory, &backup_name, &file.host_path)?;

        let staged_stat = self.ensure_staged(node, &file, &directory, &staged_name, evidence)?;
        rustix::fs::fchmod(&staged_stat.file, mode)
            .map_err(|error| PathError::errno(&directory.path().join(&staged_name), error))?;
        staged_stat
            .file
            .sync_all()
            .map_err(|error| PathError::io(&directory.path().join(&staged_name), error))?;
        directory.sync()?;

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
            return Err(ExecError::refused(
                node.id.as_str(),
                "filesystem lacks atomic exchange; a temporary path gap requires an authenticated quiescence boundary",
            ));
        }

        let backup_path = directory.path().join(&backup_name);
        verify_installed(&file.host_path, file.intent, mode.bits() & 0o7777)?;
        if !backup_holds(&directory, &backup_name, previous.object(), size, sha256) {
            return Err(ExecError::Verification {
                path: file.host_path.display().to_string(),
                reason: "the replacement applied but its backup cannot be confirmed".into(),
            });
        }

        Ok(OperationReceipt::ReplaceFile {
            previous_object: previous.object(),
            object: staged_stat.object(),
            destination: file.host_path.display().to_string(),
            previous_sha256: sha256,
            previous_size: size,
            previous_mode: previous.mode,
            backup_path: backup_path.display().to_string(),
            new_sha256: file.intent.sha256,
            new_size: file.intent.size,
            executable: file.intent.executable,
        })
    }

    fn remove(&mut self, node: &DeploymentNode) -> Result<OperationReceipt, ExecError> {
        let file = self.file(node.id.as_str())?.clone();
        let removal = removal_identity(node)?.clone();
        self.check_removal_target(&file, &removal)?;
        let (directory, name) = crate::fs::open_parent(&file.host_path)?;
        let transaction = self.transaction_id()?;
        let backup_name = backup_name(&transaction, node.id.as_str(), "removed");
        match directory.kind_or_absent(&name)? {
            None => {
                if backup_holds(
                    &directory,
                    &backup_name,
                    removal.instance.object,
                    removal.size,
                    removal.sha256,
                ) {
                    let backup_path = directory.path().join(&backup_name);
                    let backup = directory.stream_regular(&backup_name).map_err(|_| {
                        ExecError::Verification {
                            path: file.host_path.display().to_string(),
                            reason: "the backup cannot be confirmed".into(),
                        }
                    })?;
                    return Ok(OperationReceipt::RemoveFile {
                        object: removal.instance.object,
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

        if previous.object() != removal.instance.object {
            return Err(ExecError::PlanDrift {
                path: file.host_path.display().to_string(),
                reason: "owned file object changed before removal".into(),
            });
        }

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
            object: removal.instance.object,
            destination: file.destination.clone(),

            backup_path: as_target(&file.destination, &backup_path)?,
            sha256: removal.sha256,
            size: removal.size,
            mode: quarantined.mode,
        })
    }

    fn ensure_staged(
        &self,
        node: &DeploymentNode,
        file: &FileOperation,
        directory: &OwnedDirectory,
        staged_name: &str,
        evidence: &zup_transaction::TransactionRecord,
    ) -> Result<StreamedRegular, ExecError> {
        let key = mutation_key(node)?;
        let Some(OperationReceipt::StageFile {
            object,
            staged_path,
            size,
            sha256,
        }) = evidence.receipt(&OperationId::stage_file(&key))
        else {
            return Err(ExecError::refused(
                node.id.as_str(),
                "the durable staged-file receipt is missing",
            ));
        };
        if evidence.transaction_id != self.transaction_id()?
            || Path::new(staged_path) != directory.path().join(staged_name)
            || *size != file.intent.size
            || *sha256 != file.intent.sha256
        {
            return Err(ExecError::refused(
                node.id.as_str(),
                "staged-file evidence does not authorize this publication",
            ));
        }
        let staged = directory.stream_regular(staged_name)?;
        if staged.object() != *object || staged.size != *size || staged.sha256 != *sha256 {
            return Err(ExecError::refused(
                node.id.as_str(),
                "the staged file is no longer the recorded instance",
            ));
        }
        Ok(staged)
    }

    fn discard_staged(
        &mut self,
        node: &DeploymentNode,
        object: zup_core::NativeObjectIdentity,
    ) -> Result<(), ExecError> {
        let file = self.file(node.id.as_str())?;
        let key = match &node.kind {
            NodeKind::StageFile { key } | NodeKind::FileMutation { key, .. } => key,
            _ => {
                return Err(ExecError::refused(
                    node.id.as_str(),
                    "not a staged file operation",
                ));
            }
        };
        let staged_name = staged_name(&OperationId::stage_file(key));
        let (directory, _) = crate::fs::open_parent(&file.host_path)?;
        match directory.kind_or_absent(&staged_name)? {
            None => Ok(()),
            Some(EntryKind::Regular) => {
                let staged = directory.stream_regular(&staged_name)?;
                if staged.object() != object
                    || staged.size != file.intent.size
                    || staged.sha256 != file.intent.sha256
                {
                    return Err(ExecError::RollbackDrift {
                        path: directory.path().join(&staged_name).display().to_string(),
                        reason: "the staged file is no longer the recorded instance".into(),
                    });
                }
                directory.remove_file(&staged_name)?;
                directory.sync()?;
                Ok(())
            }
            Some(_) => Err(ExecError::RollbackDrift {
                path: directory.path().join(&staged_name).display().to_string(),
                reason: "the staged file is no longer a regular file".into(),
            }),
        }
    }

    fn check_removal_target(
        &self,
        file: &FileOperation,
        removal: &FileRemoval,
    ) -> Result<(), ExecError> {
        if removal.kind != RemovalKind::RemoveOwned {
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

fn cache_intent<'a>(
    node: &'a DeploymentNode,
    receipt: &OperationReceipt,
) -> Result<&'a zup_plan::NativeCacheOperation, ExecError> {
    match (&node.kind, receipt) {
        (NodeKind::NativeCache { operation }, OperationReceipt::NativeCache { intent, .. })
            if operation.fingerprint() == *intent =>
        {
            Ok(operation)
        }
        _ => Err(ExecError::refused(
            node.id.as_str(),
            "native cache receipt differs from deployment intent",
        )),
    }
}

impl OperationExecutor for LinuxFileExecutor {
    type Error = ExecError;

    fn prepare_compensation(
        &mut self,
        operation: &DeploymentNode,
        receipt: &OperationReceipt,
    ) -> Result<OperationReceipt, ExecError> {
        match receipt {
            OperationReceipt::ServiceControl(receipt) => {
                zup_transaction::prepare_service_compensation(
                    &mut self.native_service_context()?,
                    receipt,
                )
                .map(|receipt| OperationReceipt::ServiceControl(Box::new(receipt)))
                .map_err(|reason| ExecError::refused(operation.id.as_str(), reason))
            }
            _ => Ok(receipt.clone()),
        }
    }

    fn reconcile_rollback(
        &mut self,
        _operation: &DeploymentNode,
        receipt: &OperationReceipt,
    ) -> Result<zup_transaction::RollbackReconcileResult, Self::Error> {
        use zup_transaction::RollbackReconcileResult;
        if let OperationReceipt::PersistentDirectory(_) = receipt {
            self.verify(_operation, receipt)?;
            return Ok(RollbackReconcileResult::Restored);
        }
        if let OperationReceipt::ServiceControl(receipt) = receipt {
            return zup_transaction::reconcile_service_rollback(
                &mut self.native_service_context()?,
                receipt,
            )
            .map_err(|reason| ExecError::refused(&receipt.operation.native_name, reason));
        }
        if let OperationReceipt::Generation(receipt) = receipt {
            let intent = generation_intent(_operation, receipt)?;
            self.generation_storage()?
                .retain_after_compensation(intent, receipt)?;
            return Ok(RollbackReconcileResult::Restored);
        }
        Ok(match receipt {
            // These compensators verify journal-bound paths and both content generations
            // before each mutation, and accept an already restored destination.
            OperationReceipt::StageFile { .. }
            | OperationReceipt::CreateFile { .. }
            | OperationReceipt::ReplaceFile { .. }
            | OperationReceipt::RemoveFile { .. } => RollbackReconcileResult::Retry,
            OperationReceipt::NativeCache { .. } => {
                cache_intent(_operation, receipt)?;
                RollbackReconcileResult::Retry
            }
            OperationReceipt::Control => RollbackReconcileResult::Restored,
            _ => RollbackReconcileResult::Ambiguous,
        })
    }

    fn prepare(&mut self, operation: &DeploymentNode) -> Result<(), Self::Error> {
        LinuxFileExecutor::prepare(self, operation)
    }

    fn apply(
        &mut self,
        operation: &DeploymentNode,
        transaction: &zup_transaction::TransactionRecord,
    ) -> Result<OperationReceipt, Self::Error> {
        LinuxFileExecutor::apply(self, operation, transaction)
    }

    fn verify(
        &mut self,
        operation: &DeploymentNode,
        receipt: &OperationReceipt,
    ) -> Result<(), Self::Error> {
        LinuxFileExecutor::verify(self, operation, receipt)
    }

    fn rollback(
        &mut self,
        operation: &DeploymentNode,
        receipt: &OperationReceipt,
    ) -> Result<(), Self::Error> {
        LinuxFileExecutor::rollback(self, operation, receipt)
    }

    fn reconcile(
        &mut self,
        operation: &DeploymentNode,
        receipt: Option<&OperationReceipt>,
        transaction: &zup_transaction::TransactionRecord,
    ) -> Result<ReconcileResult, Self::Error> {
        LinuxFileExecutor::reconcile(self, operation, receipt, transaction)
    }
}

fn generation_intent<'a>(
    node: &'a DeploymentNode,
    receipt: &zup_transaction::GenerationReceipt,
) -> Result<&'a zup_plan::GenerationIntent, ExecError> {
    match &node.kind {
        NodeKind::Generation { operation, intent } if receipt.matches(*operation, intent) => {
            Ok(intent)
        }
        _ => Err(ExecError::refused(
            node.id.as_str(),
            "generation receipt does not match the authorized effect",
        )),
    }
}

fn verify_precondition(
    destination: &Path,
    object: zup_core::NativeObjectIdentity,
    size: u64,
    sha256: Sha256Digest,
) -> Result<(), ExecError> {
    let (directory, name) = crate::fs::open_parent(destination)?;
    let current = stream_expected(&directory, &name, destination, size, sha256)?;
    let found = zup_core::NativeObjectIdentity::Linux {
        device: current.dev,
        inode: current.ino,
    };
    if found != object {
        return Err(ExecError::PlanDrift {
            path: destination.display().to_string(),
            reason: "target object changed since snapshot".into(),
        });
    }
    Ok(())
}

fn verify_installed(
    destination: &Path,
    intent: FileIntent,
    expected_mode: u32,
) -> Result<StreamedRegular, ExecError> {
    let (directory, name) = crate::fs::open_parent(destination)?;
    let current = directory
        .stream_regular(&name)
        .map_err(|_| ExecError::Verification {
            path: destination.display().to_string(),
            reason: "the published file is not there".into(),
        })?;
    verify_payload_authority(&current, destination)?;
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
    Ok(current)
}

fn verify_previous(
    path: &Path,
    size: u64,
    sha256: Sha256Digest,
    mode: u32,
) -> Result<StreamedRegular, ExecError> {
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

fn removal_identity(node: &DeploymentNode) -> Result<&FileRemoval, ExecError> {
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
    verify_payload_authority(&current, display)?;
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

fn verify_payload_authority(file: &StreamedRegular, path: &Path) -> Result<(), ExecError> {
    if file.uid != rustix::process::geteuid().as_raw() || file.mode & 0o7022 != 0 {
        return Err(ExecError::Verification {
            path: path.display().to_string(),
            reason: "payload requires the installation owner, no foreign write permissions, and no special permission bits".into(),
        });
    }
    Ok(())
}

fn backup_holds(
    directory: &OwnedDirectory,
    backup: &str,
    object: zup_core::NativeObjectIdentity,
    size: u64,
    sha256: Sha256Digest,
) -> bool {
    match directory.stream_regular(backup) {
        Ok(current) => {
            current.object() == object && current.size == size && current.sha256 == sha256
        }
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
    previous: ExpectedFile,
    new: ExpectedFile,
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
            if previous.matches(&current) {
                Ok(Current::MatchesPrevious)
            } else if new.matches(&current) {
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
    object: zup_core::NativeObjectIdentity,
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
            if current.object() == object && current.size == size && current.sha256 == sha256 {
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
        OperationReceipt::Cleanup { .. } => false,
        OperationReceipt::PersistentDirectory(_) => false,
        OperationReceipt::NativeService(_) | OperationReceipt::NativeServiceCompensation { .. } => {
            false
        }
        OperationReceipt::NativeCache { .. } => true,
        OperationReceipt::Generation(_) => true,
        OperationReceipt::ServiceControl(_) => true,
        OperationReceipt::CreateFile { .. }
        | OperationReceipt::RetainedFile { .. }
        | OperationReceipt::Control
        | OperationReceipt::StageFile { .. }
        | OperationReceipt::Backend { .. } => true,
        OperationReceipt::ReplaceFile {
            destination,
            backup_path,
            previous_sha256,
            previous_size,
            previous_object,
            ..
        } => {
            let previous = *previous_sha256;
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
            backup_holds(
                &directory,
                &leaf,
                *previous_object,
                *previous_size,
                previous,
            )
        }
        OperationReceipt::RemoveFile {
            backup_path,
            sha256,
            size,
            object,
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
            backup_holds(&directory, &leaf, *object, *size, *sha256)
        }
    }
}

fn mutation_key(node: &DeploymentNode) -> Result<ResourceKey, ExecError> {
    match &node.kind {
        NodeKind::FileMutation { key, .. } => Ok(key.clone()),
        _ => Err(ExecError::PlanDrift {
            path: node.id.to_string(),
            reason: "a file mutation without its resource".into(),
        }),
    }
}

fn as_target(
    anchor: &zup_core::TargetPath,
    host_path: &Path,
) -> Result<zup_core::TargetPath, ExecError> {
    zup_core::TargetPath::new(*anchor.target(), &*host_path.to_string_lossy()).map_err(|error| {
        ExecError::PlanDrift {
            path: host_path.display().to_string(),
            reason: error.to_string(),
        }
    })
}

fn staged_name(id: &OperationId) -> String {
    format!("staged-{}", short_digest(id.as_str()))
}

fn short_digest(value: &str) -> String {
    let (_, digest) = hash_reader(value.as_bytes()).expect("a string hashes");
    digest.to_hex()[..16].to_owned()
}

impl LinuxFileExecutor {
    fn native_service_context(&mut self) -> Result<NativeServiceContext<'_>, ExecError> {
        let support = self
            .services
            .as_mut()
            .ok_or_else(|| ExecError::refused("", "service support is unavailable"))?;
        if let Some(openrc) = &mut support.openrc {
            return Ok(NativeServiceContext::Openrc(openrc));
        }
        Ok(NativeServiceContext::Systemd(
            crate::service_exec::ServiceContext {
                controlled_units: &self.service_units,
                roots: &support.roots,
                systemd: &support.systemd,
                manager: support
                    .manager
                    .as_mut()
                    .ok_or_else(|| ExecError::refused("", "systemd support is unavailable"))?
                    .as_mut(),
                expected_uid: support.expected_uid,
            },
        ))
    }
}

pub fn snapshot_target(target: &zup_plan::BoundDeployment) -> HostSnapshot {
    let mut snapshot = HostSnapshot::default();
    for file in target.fixed_files() {
        snapshot.files.push(ObservedFile {
            key: file.key.clone(),
            path: file.destination().clone(),
            state: observe_file(file),
        });
    }
    snapshot
}

fn observe_file(file: &zup_plan::DesiredFile<zup_core::TargetPath>) -> ObservedFileState {
    let host = match to_host_path(file.destination()) {
        Ok(host) => host,
        Err(_) => return ObservedFileState::NonFile,
    };
    let (directory, name) = match crate::fs::open_parent(&host) {
        Ok(parent) => parent,
        Err(error) if error.is_not_found() => return ObservedFileState::Absent,
        Err(_) => return ObservedFileState::NonFile,
    };
    match directory.kind_or_absent(&name) {
        Ok(None) => ObservedFileState::Absent,
        Ok(Some(EntryKind::Regular)) => match directory.stream_regular(&name) {
            Ok(current) => ObservedFileState::File {
                object: current.object(),
                size: current.size,
                sha256: current.sha256,
                executable: Some(current.mode & 0o100 != 0),
            },
            Err(_) => ObservedFileState::NonFile,
        },
        _ => ObservedFileState::NonFile,
    }
}

#[cfg(test)]
mod file_protocol_tests {
    use super::*;
    use zup_core::hash_reader;

    fn digest(bytes: &[u8]) -> Sha256Digest {
        hash_reader(bytes).expect("bytes hash").1
    }

    #[rstest::rstest]
    #[case::native(true)]
    #[case::shebang(false)]
    fn openrc_staging_rejects_scripts_before_replacing_the_installed_image(#[case] native: bool) {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("service");
        let previous = b"previous image";
        std::fs::write(&destination, previous).unwrap();
        let bytes = if native {
            std::fs::read("/proc/self/exe").unwrap()
        } else {
            b"#!/bin/sh\nexec sleep 60\n".to_vec()
        };
        let mut executor = LinuxFileExecutor::new();
        register_replace(&mut executor, &destination, previous, &bytes);
        executor.files.get_mut("op-replace").unwrap().destination = TargetPath::new(
            zup_core::Target::host().unwrap(),
            destination.to_string_lossy(),
        )
        .unwrap();
        executor.openrc_executables.insert(destination.clone());
        let staged = executor.stage(&OperationId::new("op-replace"), &bytes);
        assert_eq!(staged.is_ok(), native);
        assert_eq!(std::fs::read(&destination).unwrap(), previous);
    }

    fn register_replace(
        executor: &mut LinuxFileExecutor,
        destination: &Path,
        old: &[u8],
        new: &[u8],
    ) {
        let target = zup_core::Target::parse("linux-x64").expect("a target");
        let host = |path: &Path| {
            zup_core::TargetPath::new(target, path.to_string_lossy()).expect("a path")
        };
        executor.register(
            &OperationId::new("op-replace"),
            FileOperation {
                destination: host(destination),
                host_path: destination.to_path_buf(),
                intent: FileIntent {
                    sha256: digest(new),
                    size: new.len() as u64,
                    executable: false,
                },
                precondition: FilePrecondition::Exact {
                    object: {
                        use std::os::unix::fs::MetadataExt;
                        let metadata = std::fs::symlink_metadata(destination).unwrap();
                        zup_core::NativeObjectIdentity::Linux {
                            device: metadata.dev(),
                            inode: metadata.ino(),
                        }
                    },
                    size: old.len() as u64,
                    sha256: digest(old),
                },
            },
        );
    }

    #[test]
    fn an_occupied_backup_slot_is_refused_without_deletion() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("application.bin");
        std::fs::write(&path, b"owned").unwrap();
        std::fs::write(root.path().join("occupied"), b"foreign").unwrap();
        let (parent, _) = crate::fs::open_parent(&path).unwrap();
        assert!(occupy_backup_slot(&parent, "occupied", &path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"owned");
        assert_eq!(
            std::fs::read(root.path().join("occupied")).unwrap(),
            b"foreign"
        );
    }
}

#[cfg(test)]
mod snapshot_tests {
    use super::*;
    use zup_core::{AppId, NonEmptyString, Privilege, RelativePath, SelectedScope, Target};

    fn target_with(files: Vec<(String, zup_core::TargetPath)>) -> zup_plan::BoundDeployment {
        let target = Target::parse("linux-x64").expect("a target");
        zup_plan::DesiredState {
            generation_readers: Default::default(),
            data: Vec::new(),
            requirements: (),
            maintenance: None,
            installation: zup_core::InstallationId::new_v7(),
            generation: None,
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
            install_directory: zup_core::TargetPath::new(target, "/tmp/zup-snapshot-test/tool")
                .expect("a path"),
            selected_components: Vec::new(),
            prerequisites: Vec::new(),
            files: files
                .into_iter()
                .map(|(name, destination)| zup_plan::DesiredFile {
                    key: zup_core::ResourceKey::File {
                        id: zup_core::FileResourceId::Payload {
                            id: zup_core::PayloadId::File {
                                declaration: zup_core::ResourceId::new(&name).unwrap(),
                            },
                        },
                    },
                    source_relative: RelativePath::new(&name).expect("a path"),
                    placement: zup_core::PayloadPlacement::Fixed {
                        replacement: zup_core::FixedReplacement::Atomic,
                        path: destination,
                    },
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

            preset: None,
        }
        .bind_fixed()
        .expect("fixed deployment requirements")
    }

    fn destination(root: &std::path::Path, name: &str) -> zup_core::TargetPath {
        zup_core::TargetPath::new(
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

enum NativeServiceContext<'a> {
    Systemd(crate::service_exec::ServiceContext<'a>),
    Openrc(&'a mut crate::openrc_native::Openrc),
}
impl NativeServiceContext<'_> {
    fn prepare_registration(
        &mut self,
        operation: &zup_plan::NativeServiceOperation,
    ) -> Result<(), ExecError> {
        match self {
            Self::Systemd(context) => crate::native_service::prepare(context, operation),
            Self::Openrc(manager) => manager.prepare_registration(operation),
        }
    }
    fn apply_registration(
        &mut self,
        operation: &zup_plan::NativeServiceOperation,
        source_retained: bool,
    ) -> Result<OperationReceipt, ExecError> {
        match self {
            Self::Systemd(context) => {
                crate::native_service::apply(context, operation, source_retained)
            }
            Self::Openrc(manager) => manager.apply_registration(operation),
        }
    }
    fn verify_registration(
        &mut self,
        operation: &zup_plan::NativeServiceOperation,
        receipt: &OperationReceipt,
    ) -> Result<(), ExecError> {
        match self {
            Self::Systemd(context) => crate::native_service::verify(context, operation, receipt),
            Self::Openrc(manager) => manager.verify_registration(operation, receipt),
        }
    }
    fn rollback_registration(
        &mut self,
        operation: &zup_plan::NativeServiceOperation,
        receipt: &OperationReceipt,
    ) -> Result<(), ExecError> {
        match self {
            Self::Systemd(context) => crate::native_service::rollback(context, operation, receipt),
            Self::Openrc(manager) => manager.rollback_registration(operation, receipt),
        }
    }
    fn reconcile_registration(
        &mut self,
        operation: &zup_plan::NativeServiceOperation,
    ) -> Result<ReconcileResult, ExecError> {
        match self {
            Self::Systemd(context) => crate::native_service::reconcile(context, operation),
            Self::Openrc(manager) => manager.reconcile_registration(operation),
        }
    }
    fn manager(&mut self) -> &mut dyn zup_transaction::NativeServiceManager {
        match self {
            Self::Systemd(context) => context,
            Self::Openrc(manager) => *manager,
        }
    }
}
impl zup_transaction::NativeServiceManager for NativeServiceContext<'_> {
    fn reconcile_control_request(
        &mut self,
        operation: &zup_plan::ServiceControlOperation,
        deadline: std::time::Instant,
    ) -> Result<(), String> {
        self.manager()
            .reconcile_control_request(operation, deadline)
    }
    fn finish_startup_inhibition(
        &mut self,
        operation: &zup_plan::ServiceControlOperation,
    ) -> Result<(), String> {
        self.manager().finish_startup_inhibition(operation)
    }
    fn stopped_runtime_is_quiescent(
        &mut self,
        operation: &zup_plan::ServiceControlOperation,
        configuration: &zup_plan::ServiceState,
    ) -> Result<bool, String> {
        self.manager()
            .stopped_runtime_is_quiescent(operation, configuration)
    }
    fn observe(
        &mut self,
        operation: &zup_plan::ServiceControlOperation,
    ) -> Result<zup_transaction::ServiceObservation, String> {
        self.manager().observe(operation)
    }
    fn transition(
        &mut self,
        operation: &zup_plan::ServiceControlOperation,
        state: zup_plan::ServiceTerminalState,
        expected: &zup_transaction::ServiceObservation,
        deadline: std::time::Instant,
    ) -> Result<zup_transaction::ServiceObservation, String> {
        self.manager()
            .transition(operation, state, expected, deadline)
    }
    fn stopped_start_can_be_compensated(
        &mut self,
        operation: &zup_plan::ServiceControlOperation,
    ) -> Result<bool, String> {
        self.manager().stopped_start_can_be_compensated(operation)
    }
}
