use super::*;
use std::io::Read;

use zup_core::{NativeObjectIdentity, SelectedScope};
use zup_plan::MetadataFile;
use zup_transaction::{HistoryFilesystem, StateNamespace};

pub(crate) struct WindowsHistoryFilesystem(pub crate::machine_state::StateAuthority);

pub(crate) enum MetadataDirectory {
    User(private_fs::PrivateDir),
    Machine(MachineDir),
}

fn display(error: impl std::fmt::Display) -> String {
    error.to_string()
}

fn private_error(error: private_fs::Error) -> FilesystemError {
    match error {
        private_fs::Error::Io(source) => io_std(source),
        error => io_std(std::io::Error::other(error)),
    }
}

impl MetadataDirectory {
    fn handle(&self) -> Result<OwnedHandle, FilesystemError> {
        match self {
            Self::User(directory) => directory.as_handle().try_clone_to_owned().map_err(io_std),
            Self::Machine(directory) => {
                directory.validate()?;
                directory.handle.try_clone().map_err(io_std)
            }
        }
    }
    fn sync(&self) -> Result<(), String> {
        match self {
            Self::User(directory) => directory.sync_all().map_err(display),
            Self::Machine(directory) => directory.sync().map_err(display),
        }
    }
}

impl HistoryFilesystem for WindowsHistoryFilesystem {
    type Directory = MetadataDirectory;

    fn namespace(
        &self,
        namespace: StateNamespace,
        create: bool,
    ) -> Result<Option<MetadataDirectory>, String> {
        let result = match self.0.scope {
            SelectedScope::Machine => self
                .0
                .machine()
                .and_then(|machine| machine.state_directory(namespace, create))
                .map(MetadataDirectory::Machine),
            SelectedScope::User => {
                if crate::transport::is_process_elevated().map_err(display)? {
                    return Err("user history maintenance requires an unelevated user token".into());
                }
                let root = private_fs::PrivateDir::open(&self.0.root).map_err(display)?;
                let name = match namespace {
                    StateNamespace::Transactions => "transactions",
                    StateNamespace::History => "history",
                    StateNamespace::Materializations(_) => "materializations",
                    StateNamespace::Data => "data",
                    StateNamespace::Collections => "collections",
                };
                let parent = if create {
                    root.ensure_dir(name)
                } else {
                    root.open_dir(name)
                };
                parent
                    .and_then(|parent| match namespace {
                        StateNamespace::Materializations(installation) => {
                            let name = installation.to_string();
                            if create {
                                parent.ensure_dir(&name)
                            } else {
                                parent.open_dir(&name)
                            }
                        }
                        _ => Ok(parent),
                    })
                    .map(MetadataDirectory::User)
                    .map_err(private_error)
            }
        };
        match result {
            Ok(directory) => Ok(Some(directory)),
            Err(FilesystemError::Io { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                Ok(None)
            }
            Err(error) => Err(display(error)),
        }
    }

    fn open_directory(
        &self,
        parent: &MetadataDirectory,
        name: &str,
    ) -> Result<Option<MetadataDirectory>, String> {
        let result = match parent {
            MetadataDirectory::User(directory) => directory
                .open_dir(name)
                .map(MetadataDirectory::User)
                .map_err(private_error),
            MetadataDirectory::Machine(directory) => directory
                .child(name, PolicyKind::Administrators, Open::MovableDirectory)
                .map(MetadataDirectory::Machine),
        };
        match result {
            Ok(directory) => Ok(Some(directory)),
            Err(FilesystemError::Io { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                Ok(None)
            }
            Err(error) => Err(display(error)),
        }
    }

    fn object(directory: &MetadataDirectory) -> Result<NativeObjectIdentity, String> {
        let (volume, file) = identity(&directory.handle().map_err(display)?).map_err(display)?;
        Ok(NativeObjectIdentity::Windows { volume, file })
    }
    fn names(directory: &MetadataDirectory) -> Result<Vec<String>, String> {
        match directory {
            MetadataDirectory::User(directory) => directory
                .entries()
                .map_err(display)?
                .into_iter()
                .map(|name| {
                    name.into_string()
                        .map_err(|_| "history entry is not Unicode".to_owned())
                })
                .collect(),
            MetadataDirectory::Machine(directory) => directory.list().map_err(display),
        }
    }

    fn read_file(
        directory: &MetadataDirectory,
        name: &str,
    ) -> Result<Option<(MetadataFile, Vec<u8>)>, String> {
        let result = match directory {
            MetadataDirectory::User(directory) => directory
                .open_file(name)
                .map_err(private_error)
                .and_then(|file| file.as_handle().try_clone_to_owned().map_err(io_std)),
            MetadataDirectory::Machine(directory) => {
                directory.validate().map_err(display)?;
                open_relative(&directory.handle, OsStr::new(name), Open::Read, None).and_then(
                    |opened| {
                        verify_file(&opened.handle, PolicyKind::Administrators)?;
                        Ok(opened.handle)
                    },
                )
            }
        };
        let handle = match result {
            Ok(handle) => handle,
            Err(FilesystemError::Io { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                return Ok(None);
            }
            Err(error) => return Err(display(error)),
        };
        reject_reparse(&handle).map_err(display)?;
        if attributes(&handle).map_err(display)? & FILE_ATTRIBUTE_DIRECTORY != 0 {
            return Err("history metadata is not a regular file".into());
        }
        let file = File::from(handle);
        let size = file.metadata().map_err(display)?.len();
        if size > 64 * 1024 * 1024 {
            return Err("history metadata exceeds its read limit".into());
        }
        let (volume, id) = identity(&file).map_err(display)?;
        let mut bytes = Vec::new();
        file.take(64 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map_err(display)?;
        if size != bytes.len() as u64 {
            return Err("history metadata changed during observation".into());
        }
        Ok(Some((
            MetadataFile {
                object: NativeObjectIdentity::Windows { volume, file: id },
                size,
                sha256: zup_core::hash_bytes(&bytes),
            },
            bytes,
        )))
    }

    fn move_directory(
        &self,
        parent: &MetadataDirectory,
        name: &str,
        destination: &MetadataDirectory,
        object: NativeObjectIdentity,
    ) -> Result<(), String> {
        let NativeObjectIdentity::Windows { volume, file } = object else {
            return Err("non-Windows journal identity".into());
        };
        match (parent, destination) {
            (MetadataDirectory::User(parent), MetadataDirectory::User(destination)) => {
                rename_private_directory(parent, name, destination, name, (volume, file))
                    .map_err(display)
            }
            (MetadataDirectory::Machine(parent), MetadataDirectory::Machine(destination)) => parent
                .rename_directory(name, destination, name, (volume, file))
                .map_err(display),
            _ => Err("journal archival changes installation scope".into()),
        }
    }

    fn remove_file(
        directory: &MetadataDirectory,
        name: &str,
        expected: &MetadataFile,
    ) -> Result<(), String> {
        let Some((observed, _)) = Self::read_file(directory, name)? else {
            return Err("history file disappeared before removal".into());
        };
        if observed != *expected {
            return Err("history file differs from its protected native instance".into());
        }
        let parent = directory.handle().map_err(display)?;
        let handle = open_relative(&parent, OsStr::new(name), Open::FenceFile, None)
            .map_err(display)?
            .handle;
        reject_reparse(&handle).map_err(display)?;
        let (volume, id) = identity(&handle).map_err(display)?;
        if expected.object != (NativeObjectIdentity::Windows { volume, file: id }) {
            return Err("history file changed before deletion".into());
        }
        let mut file = File::from(handle);
        let mut bytes = Vec::new();
        (&mut file)
            .take(64 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map_err(display)?;
        if expected.size != bytes.len() as u64 || expected.sha256 != zup_core::hash_bytes(&bytes) {
            return Err("history file changed under its native fence".into());
        }
        discard(&file).map_err(display)?;
        drop(file);
        directory.sync()
    }

    fn remove_directory(
        parent: &MetadataDirectory,
        name: &str,
        object: NativeObjectIdentity,
    ) -> Result<(), String> {
        let parent_handle = parent.handle().map_err(display)?;
        let handle = open_relative(
            &parent_handle,
            OsStr::new(name),
            Open::MovableDirectory,
            None,
        )
        .map_err(display)?
        .handle;
        reject_reparse(&handle).map_err(display)?;
        let (volume, file) = identity(&handle).map_err(display)?;
        if object != (NativeObjectIdentity::Windows { volume, file })
            || !list_entries(&handle).map_err(display)?.is_empty()
        {
            return Err("history directory identity or contents changed before removal".into());
        }
        discard(&handle).map_err(display)?;
        drop(handle);
        parent.sync()
    }
}
