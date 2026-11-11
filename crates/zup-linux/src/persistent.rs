use crate::{error::ExecError, fs::OwnedDirectory};
use rustix::fs::Mode;
use std::io::Read;
use zup_core::{NativeObjectIdentity, Privilege, TransactionId};
use zup_plan::PersistentDirectoryOperation;
use zup_transaction::{PersistentFilesystem, PersistentPreparation};

pub(crate) struct LinuxPersistentFilesystem(pub crate::state::StateAuthority);
pub struct PersistentDirectoryHandle {
    directory: OwnedDirectory,
    binding: zup_plan::FilesystemDataBinding,
}

pub(crate) fn discover(
    root: &std::path::Path,
    scope: zup_core::SelectedScope,
    app: &zup_core::AppId,
    target: zup_core::Target,
    previous: Option<&zup_plan::InstallationRecord>,
) -> Result<Option<zup_plan::PersistentClaimSelection>, ExecError> {
    let store = zup_transaction::FilesystemTransactionStore::private_at(root);
    zup_transaction::PersistentStorage(LinuxPersistentFilesystem(crate::state::StateAuthority {
        root: root.to_owned(),
        app: app.clone(),
        scope,
    }))
    .discover_claims(&store, app, scope, target, previous)
}

impl LinuxPersistentFilesystem {
    fn metadata(
        &self,
        transaction: TransactionId,
        operation: &PersistentDirectoryOperation,
        create: bool,
    ) -> Result<(OwnedDirectory, String), ExecError> {
        let path = self.0.root.join("data");
        let directory = if create {
            OwnedDirectory::create(&path, Mode::from_bits_truncate(0o700))?
        } else {
            OwnedDirectory::open(&path)?
        };
        crate::fs::verify_ancestor_trust(&path, rustix::process::geteuid().as_raw())?;
        Ok((
            directory,
            format!(
                "{}-{}.json",
                transaction,
                zup_core::hash_bytes(operation.intent.directory.id.as_str().as_bytes()).to_hex()
            ),
        ))
    }
}

impl PersistentFilesystem for LinuxPersistentFilesystem {
    type Directory = PersistentDirectoryHandle;
    type Error = ExecError;
    fn refused(reason: &str) -> ExecError {
        ExecError::refused("persistent data", reason)
    }
    fn recovery_required(record: &zup_transaction::TransactionRecord) -> ExecError {
        ExecError::RecoveryRequired {
            transaction: Some(record.transaction_id),
            detail: record.recovery_detail(),
        }
    }
    fn parent(
        &self,
        operation: &PersistentDirectoryOperation,
    ) -> Result<PersistentDirectoryHandle, ExecError> {
        if operation.intent.privilege != self.0.scope.authorization() {
            return Err(Self::refused(
                "persistent directory authority differs from its protected store",
            ));
        }
        let path = crate::to_host_path(&binding(operation)?.path)?;
        let _state_boundary = self.0.payload_directory(&path)?;
        if operation.intent.privilege == Privilege::System {
            if rustix::process::geteuid().as_raw() != 0 {
                return Err(Self::refused(
                    "machine data requires native machine authority",
                ));
            }
            let destination = crate::machine::authorize_machine_destination(
                &path,
                &crate::locations::MachineRoots::production(),
            )?;
            if destination == crate::machine::MachineDestination::MachineState {
                return Err(Self::refused(
                    "application data cannot occupy deployment engine state",
                ));
            }
        }
        let parent = path
            .parent()
            .ok_or_else(|| Self::refused("data root has no parent"))?;
        crate::fs::verify_ancestor_trust(parent, rustix::process::geteuid().as_raw())?;
        Ok(PersistentDirectoryHandle {
            directory: OwnedDirectory::open(parent)?,
            binding: binding(operation)?.clone(),
        })
    }
    fn open(
        &self,
        parent: &PersistentDirectoryHandle,
        name: &str,
    ) -> Result<Option<PersistentDirectoryHandle>, ExecError> {
        if parent.directory.kind_or_absent(name)?.is_none() {
            return Ok(None);
        }
        let directory = parent.directory.open_child(name)?;
        crate::data_access::verify_preparation(&directory, &parent.binding)?;
        Ok(Some(PersistentDirectoryHandle {
            directory,
            binding: parent.binding.clone(),
        }))
    }
    fn create(
        &self,
        parent: &PersistentDirectoryHandle,
        name: &str,
        _token: &zup_transaction::PersistentCreationToken,
    ) -> Result<PersistentDirectoryHandle, ExecError> {
        Ok(PersistentDirectoryHandle {
            directory: parent
                .directory
                .create_exclusive_directory(name, Mode::from_bits_truncate(0o700))?,
            binding: parent.binding.clone(),
        })
    }
    fn object(directory: &PersistentDirectoryHandle) -> Result<NativeObjectIdentity, ExecError> {
        Ok(directory.directory.stat_self()?.object())
    }
    fn publish(
        &self,
        parent: &PersistentDirectoryHandle,
        from: &str,
        to: &str,
        expected: NativeObjectIdentity,
    ) -> Result<(), ExecError> {
        if parent.directory.open_child(from)?.stat_self()?.object() != expected {
            return Err(Self::refused("staged data root identity changed"));
        }
        parent.directory.rename_noreplace(from, to)?;
        if parent.directory.open_child(to)?.stat_self()?.object() != expected {
            return Err(Self::refused("published data root identity changed"));
        }
        Ok(())
    }
    fn load(
        &self,
        transaction: TransactionId,
        operation: &PersistentDirectoryOperation,
    ) -> Result<Option<PersistentPreparation>, ExecError> {
        let (directory, name) = match self.metadata(transaction, operation, false) {
            Ok(value) => value,
            Err(ExecError::Path(error)) if error.is_not_found() => return Ok(None),
            Err(error) => return Err(error),
        };
        if directory.kind_or_absent(&name)?.is_none() {
            return Ok(None);
        }
        let mut file = directory.open_regular_read(&name)?;
        let state = rustix::fs::fstat(&file).map_err(|error| Self::refused(&error.to_string()))?;
        if state.st_uid != rustix::process::geteuid().as_raw()
            || state.st_mode & 0o077 != 0
            || state.st_nlink != 1
        {
            return Err(Self::refused(
                "persistent ownership evidence is not private",
            ));
        }
        let mut bytes = Vec::new();
        file.by_ref()
            .take(1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| Self::refused(&error.to_string()))?;
        if bytes.len() > 1024 * 1024 {
            return Err(Self::refused(
                "persistent ownership evidence exceeds its bound",
            ));
        }
        Ok(Some(serde_json::from_slice(&bytes)?))
    }
    fn checkpoint(
        &self,
        record: &PersistentPreparation,
        previous: Option<&PersistentPreparation>,
    ) -> Result<(), ExecError> {
        let (directory, name) = self.metadata(record.transaction, &record.operation, true)?;
        if self.load(record.transaction, &record.operation)?.as_ref() != previous {
            return Err(Self::refused(
                "persistent preparation changed before checkpoint",
            ));
        }
        let bytes = serde_json::to_vec(record)?;
        if previous.is_some() {
            directory.write_durable(&name, &bytes, Mode::from_bits_truncate(0o600))?;
        } else {
            directory.create_durable_exclusive(&name, &bytes, Mode::from_bits_truncate(0o600))?;
        }
        if self.load(record.transaction, &record.operation)?.as_ref() != Some(record) {
            return Err(Self::refused("persistent checkpoint differs on readback"));
        }
        Ok(())
    }
    fn native_state_available(
        &self,
        binding: &zup_plan::SystemdStateBinding,
    ) -> Result<(), ExecError> {
        validate_native(binding)?;
        for parent in ["/var/lib", "/var/lib/private"] {
            let path = std::path::Path::new(parent);
            let directory = match OwnedDirectory::open(path) {
                Ok(directory) => directory,
                Err(error) if error.is_not_found() && parent.ends_with("/private") => continue,
                Err(error) => return Err(error.into()),
            };
            crate::fs::verify_ancestor_trust(path, 0)?;
            if directory.kind_or_absent(binding.name.as_str())?.is_some() {
                return Err(Self::refused(
                    "native state namespace already exists without its protected claim",
                ));
            }
        }
        Ok(())
    }
    fn verify_native_state(
        &self,
        binding: &zup_plan::SystemdStateBinding,
    ) -> Result<(), ExecError> {
        validate_native(binding)?;
        let path = crate::machine::authorize_systemd_unit(
            &binding.unit,
            &crate::machine::SystemdRoots::production(),
        )?;
        let mut manager = crate::systemd::RealSystemd::connect()?;
        let state = crate::service_ops::observe_unit(
            &binding.unit,
            &path,
            zup_core::Target::host().map_err(|error| Self::refused(&error.to_string()))?,
            &mut manager,
        )?;
        if !matches!(state, zup_plan::ObservedServiceState::Service { state_directories, .. } if state_directories.contains(&binding.name))
        {
            return Err(Self::refused(
                "systemd state namespace is not present in the verified native registration",
            ));
        }
        Ok(())
    }
    fn provision_access(
        &self,
        directory: &PersistentDirectoryHandle,
        binding: &zup_plan::FilesystemDataBinding,
    ) -> Result<(), ExecError> {
        crate::data_access::provision(&directory.directory, binding)
    }
    fn verify_access(
        &self,
        directory: &PersistentDirectoryHandle,
        binding: &zup_plan::FilesystemDataBinding,
    ) -> Result<(), ExecError> {
        crate::data_access::verify(&directory.directory, binding)
    }
    fn origin(
        &self,
        transaction: TransactionId,
    ) -> Result<zup_transaction::TransactionRecord, ExecError> {
        self.0.source(transaction)
    }
}

fn binding(
    operation: &PersistentDirectoryOperation,
) -> Result<&zup_plan::FilesystemDataBinding, ExecError> {
    match &operation.intent.binding {
        zup_plan::PersistentBinding::Filesystem(binding) => Ok(binding),
        _ => Err(ExecError::refused(
            "persistent data",
            "native systemd state has no installer-owned directory",
        )),
    }
}

pub(crate) fn validate_native(binding: &zup_plan::SystemdStateBinding) -> Result<(), ExecError> {
    if rustix::process::geteuid().as_raw() != 0
        || crate::machine::authorize_systemd_unit(
            &binding.unit,
            &crate::machine::SystemdRoots::production(),
        )
        .is_err()
    {
        return Err(ExecError::refused(
            "native state",
            "native namespace requires machine authority and the exact service unit",
        ));
    }
    Ok(())
}

pub fn resolve_persistent_bindings(
    target: &zup_plan::BoundDeployment,
) -> Result<std::collections::BTreeMap<zup_core::ResourceId, zup_plan::PersistentBinding>, String> {
    target.data.iter().map(|directory| {
            if let zup_core::PersistentLocation::Service { service: id } = &directory.location {
                let service = target.services.iter().find(|service| &service.id == id)
                    .ok_or_else(|| format!("data {} requires service {id}", directory.id))?;
                if matches!(service.execution, zup_core::ServiceExecution::SystemdUser { .. } | zup_core::ServiceExecution::SystemdDynamicUser) {
                    return Ok((directory.id.clone(), zup_plan::PersistentBinding::SystemdState(zup_plan::SystemdStateBinding {
                        application: target.app.id.clone(), service: id.clone(), unit: service.name.to_string(),
                        name: zup_plan::service_state_name(&target.app.id, id, &directory.id),
                    })));
                }
            }
            let writers = directory.location.services().map(|id| {
                let service = target.services.iter().find(|service| &service.id == id)
                    .ok_or_else(|| format!("data {} requires service {id}", directory.id))?;
                let user = match &service.execution {
                    zup_core::ServiceExecution::SystemdUser { user, .. } | zup_core::ServiceExecution::OpenrcUser { user, .. } => user,
                    _ => return Err(format!("data {} needs a static service account; use service placement for DynamicUser", directory.id)),
                };
                let account = uzers::get_user_by_name(user).ok_or_else(|| format!("service data account {user} does not exist"))?;
                Ok(zup_plan::ServiceDataWriter { service: id.clone(), principal: zup_plan::ServiceDataPrincipal::LinuxUser { name: user.clone(), uid: account.uid() } })
            }).collect::<Result<Vec<_>, String>>()?;
            let path = match &directory.location {
                zup_core::PersistentLocation::Path { path, .. } => path.clone(),
                zup_core::PersistentLocation::Service { service } => zup_core::TargetPath::new(target.target,
                    format!("/var/lib/{}", zup_plan::service_state_name(&target.app.id, service, &directory.id))).map_err(|error| error.to_string())?,
            };
            Ok((directory.id.clone(), zup_plan::PersistentBinding::Filesystem(zup_plan::FilesystemDataBinding { path, writers })))
        }).collect()
}
