use crate::{
    filesystem::{
        PolicyKind,
        authority::{DataDirectory, DirectoryAccess, FileAuthority, FileParent},
    },
    generation::GenerationDirectory,
    machine_authority::MachineRoot,
};
use private_fs::{Durability, PrivateDir, Publication};
use std::{io::Read, path::PathBuf};
use zup_core::{NativeObjectIdentity, SelectedScope, TransactionId};
use zup_plan::PersistentDirectoryOperation;
use zup_transaction::{PersistentFilesystem, PersistentPreparation};

pub(crate) struct WindowsPersistentFilesystem {
    pub state: crate::machine_state::StateAuthority,
}

pub(crate) fn discover(
    root: &std::path::Path,
    scope: SelectedScope,
    app: &zup_core::AppId,
    target: zup_core::Target,
    previous: Option<&zup_plan::InstallationRecord>,
) -> Result<Option<zup_plan::PersistentClaimSelection>, String> {
    let filesystem = WindowsPersistentFilesystem {
        state: crate::machine_state::StateAuthority {
            root: root.to_owned(),
            scope,
            app: app.clone(),
        },
    };
    let store = match scope {
        SelectedScope::User => zup_transaction::FilesystemTransactionStore::private_at(root),
        SelectedScope::Machine => match filesystem.state.machine() {
            Ok(machine) => machine.transaction_store(),
            Err(crate::filesystem::FilesystemError::Io { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound && previous.is_none() =>
            {
                return Ok(None);
            }
            Err(error) => return Err(display(error)),
        },
    };
    zup_transaction::PersistentStorage(filesystem)
        .discover_claims(&store, app, scope, target, previous)
}

impl WindowsPersistentFilesystem {
    fn machine(&self) -> Result<MachineRoot, String> {
        self.state.machine().map_err(display)
    }
    fn metadata(&self, create: bool) -> Result<Option<GenerationDirectory>, String> {
        match self.state.scope {
            SelectedScope::User => {
                let root = PrivateDir::open(&self.state.root).map_err(display)?;
                let directory = if create {
                    root.ensure_dir_all("data")
                } else {
                    root.open_dir("data")
                };
                match directory {
                    Ok(directory) => Ok(Some(GenerationDirectory::User(directory))),
                    Err(private_fs::Error::Io(error))
                        if !create && error.kind() == std::io::ErrorKind::NotFound =>
                    {
                        Ok(None)
                    }
                    Err(error) => Err(display(error)),
                }
            }
            SelectedScope::Machine => match self
                .machine()?
                .state_directory(zup_transaction::StateNamespace::Data, create)
            {
                Ok(directory) => Ok(Some(GenerationDirectory::Machine(directory))),
                Err(crate::filesystem::FilesystemError::Io { source, .. })
                    if !create && source.kind() == std::io::ErrorKind::NotFound =>
                {
                    Ok(None)
                }
                Err(error) => Err(display(error)),
            },
        }
    }
    fn name(transaction: TransactionId, operation: &PersistentDirectoryOperation) -> String {
        format!(
            "{}-{}.json",
            transaction,
            zup_core::hash_bytes(operation.intent.directory.id.as_str().as_bytes()).to_hex()
        )
    }
}

impl PersistentFilesystem for WindowsPersistentFilesystem {
    type Directory = PersistentDirectoryHandle;
    type Error = String;
    fn refused(reason: &str) -> String {
        reason.to_owned()
    }
    fn parent(
        &self,
        operation: &PersistentDirectoryOperation,
    ) -> Result<PersistentDirectoryHandle, String> {
        if operation.intent.privilege != self.state.scope.authorization() {
            return Err("persistent directory authority differs from its protected store".into());
        }
        let path = PathBuf::from(binding(operation)?.path.as_str());
        let _namespace = self.state.payload_directory(&path).map_err(display)?;
        let parent = path.parent().ok_or("persistent data root has no parent")?;
        let authority = match self.state.scope {
            SelectedScope::User => {
                FileAuthority::User(crate::transport::UserSid::current().map_err(display)?)
            }
            SelectedScope::Machine => FileAuthority::Machine,
        };
        let directory = FileParent::open(parent, authority, DirectoryAccess::CreateDirectories)
            .map_err(display)?;
        Ok(PersistentDirectoryHandle::Parent(
            directory,
            binding(operation)?.clone(),
        ))
    }
    fn open(
        &self,
        parent: &PersistentDirectoryHandle,
        name: &str,
    ) -> Result<Option<PersistentDirectoryHandle>, String> {
        let PersistentDirectoryHandle::Parent(parent, binding) = parent else {
            return Err("persistent operation requires a held parent".into());
        };
        let opened = match parent.open_data_child(name, &binding.writers) {
            Ok(directory) => directory,
            Err(crate::filesystem::FilesystemError::Io { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                return Ok(None);
            }
            Err(error) => return Err(display(error)),
        };
        Ok(Some(PersistentDirectoryHandle::Root(opened)))
    }
    fn creation_name(
        operation: &PersistentDirectoryOperation,
        _transaction: TransactionId,
    ) -> String {
        binding(operation)
            .expect("validated filesystem data")
            .path
            .file_name()
            .expect("validated data root")
            .to_owned()
    }
    fn authenticate_creation(
        &self,
        directory: &PersistentDirectoryHandle,
        token: &zup_transaction::PersistentCreationToken,
    ) -> Result<bool, String> {
        match directory {
            PersistentDirectoryHandle::Root(directory) => {
                directory.authenticates(token).map_err(display)
            }
            _ => Err("creation proof requires a held data root".into()),
        }
    }
    fn create(
        &self,
        parent: &PersistentDirectoryHandle,
        name: &str,
        token: &zup_transaction::PersistentCreationToken,
    ) -> Result<PersistentDirectoryHandle, String> {
        let PersistentDirectoryHandle::Parent(parent, _) = parent else {
            return Err("persistent operation requires a held parent".into());
        };
        let directory = parent
            .create_authenticated_data_child(name, token)
            .map_err(display)?;
        Ok(PersistentDirectoryHandle::Root(directory))
    }
    fn object(directory: &PersistentDirectoryHandle) -> Result<NativeObjectIdentity, String> {
        match directory {
            PersistentDirectoryHandle::Root(directory) => directory.identity().map_err(display),
            _ => Err("data root identity requires a held root".into()),
        }
    }
    fn publish(
        &self,
        parent: &PersistentDirectoryHandle,
        from: &str,
        to: &str,
        expected: NativeObjectIdentity,
    ) -> Result<(), String> {
        let _ = (parent, from, to, expected);
        Err("Windows persistent roots are exclusively created at their final path".into())
    }

    fn load(
        &self,
        transaction: TransactionId,
        operation: &PersistentDirectoryOperation,
    ) -> Result<Option<PersistentPreparation>, String> {
        let Some(directory) = self.metadata(false)? else {
            return Ok(None);
        };
        let name = Self::name(transaction, operation);
        let bytes = match directory {
            GenerationDirectory::User(directory) => match directory.open_file(&name) {
                Ok(mut file) => {
                    let mut bytes = Vec::new();
                    file.by_ref()
                        .take(1024 * 1024 + 1)
                        .read_to_end(&mut bytes)
                        .map_err(display)?;
                    Some(bytes)
                }
                Err(private_fs::Error::Io(error))
                    if error.kind() == std::io::ErrorKind::NotFound =>
                {
                    None
                }
                Err(error) => return Err(display(error)),
            },
            GenerationDirectory::Machine(directory) => directory
                .read_bounded(&name, PolicyKind::Administrators, 1024 * 1024 + 1)
                .map_err(display)?,
        };
        bytes
            .map(|bytes| {
                if bytes.len() > 1024 * 1024 {
                    return Err("persistent ownership evidence exceeds its bound".into());
                }
                serde_json::from_slice(&bytes).map_err(display)
            })
            .transpose()
    }
    fn checkpoint(
        &self,
        record: &PersistentPreparation,
        previous: Option<&PersistentPreparation>,
    ) -> Result<(), String> {
        if self.load(record.transaction, &record.operation)?.as_ref() != previous {
            return Err("persistent preparation changed before checkpoint".into());
        }
        let bytes = serde_json::to_vec(record).map_err(display)?;
        let name = Self::name(record.transaction, &record.operation);
        match self
            .metadata(true)?
            .ok_or("persistent metadata directory creation is not visible")?
        {
            GenerationDirectory::User(directory) => {
                use std::io::Write;
                let mut file = directory
                    .atomic_with_policy(
                        &name,
                        Durability::Full,
                        if previous.is_some() {
                            Publication::Replace
                        } else {
                            Publication::CreateNew
                        },
                    )
                    .map_err(display)?;
                file.write_all(&bytes).map_err(display)?;
                file.commit().map_err(display)?;
            }
            GenerationDirectory::Machine(directory) => directory
                .publish(
                    &name,
                    &bytes,
                    previous.is_none(),
                    PolicyKind::Administrators,
                )
                .map_err(display)?,
        }
        let found = self
            .load(record.transaction, &record.operation)?
            .ok_or("persistent evidence publication is not visible")?;
        if found != *record {
            return Err("persistent evidence publication differs on readback".into());
        }
        Ok(())
    }
    fn provision_access(
        &self,
        directory: &PersistentDirectoryHandle,
        binding: &zup_plan::FilesystemDataBinding,
    ) -> Result<(), String> {
        let PersistentDirectoryHandle::Root(directory) = directory else {
            return Err("data access requires the owned root".into());
        };
        directory
            .provision_access(&binding.writers)
            .map_err(display)
    }
    fn verify_access(
        &self,
        directory: &PersistentDirectoryHandle,
        binding: &zup_plan::FilesystemDataBinding,
    ) -> Result<(), String> {
        let PersistentDirectoryHandle::Root(directory) = directory else {
            return Err("data access requires the owned root".into());
        };
        directory.verify_access(&binding.writers).map_err(display)
    }
    fn origin(
        &self,
        transaction: TransactionId,
    ) -> Result<zup_transaction::TransactionRecord, String> {
        self.state.source(transaction)
    }
}

pub(crate) enum PersistentDirectoryHandle {
    Parent(FileParent, zup_plan::FilesystemDataBinding),
    Root(DataDirectory),
}
fn display(error: impl std::fmt::Display) -> String {
    error.to_string()
}

fn binding(
    operation: &PersistentDirectoryOperation,
) -> Result<&zup_plan::FilesystemDataBinding, String> {
    match &operation.intent.binding {
        zup_plan::PersistentBinding::Filesystem(binding) => Ok(binding),
        _ => Err("Windows data requires a filesystem binding".into()),
    }
}
