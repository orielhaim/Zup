use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use private_fs::{
    Durability, Error, FileAccess, FileCreation, OpenOptions, PrivateDir, PrivateFile,
};

pub fn ensure_dir(path: &Path) -> std::io::Result<PrivateDir> {
    match PrivateDir::ensure(path) {
        Ok(dir) => Ok(dir),
        Err(error) => {
            let io = io_error(error);
            if io.kind() != std::io::ErrorKind::NotFound {
                return Err(io);
            }
            let Some(parent) = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
            else {
                return Err(io);
            };
            let Some(name) = path.file_name() else {
                return Err(io);
            };
            let parent = ensure_dir(parent)?;
            parent.ensure_dir(name).map_err(io_error)
        }
    }
}

pub fn open_dir(path: &Path) -> std::io::Result<PrivateDir> {
    PrivateDir::open(path).map_err(io_error)
}

pub fn read_at(path: &Path) -> std::io::Result<Vec<u8>> {
    let (parent, name) = parent_file(path)?;
    let dir = PrivateDir::open(&parent).map_err(io_error)?;
    read_file(&dir, name)
}

pub fn read_file(dir: &PrivateDir, name: impl AsRef<Path>) -> std::io::Result<Vec<u8>> {
    let mut file = dir.open_file(name).map_err(io_error)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

pub fn write_durable_at(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let (parent, name) = parent_file(path)?;
    let dir = ensure_dir(&parent)?;
    write_replace(&dir, name, bytes)
}

pub fn write_replace(
    dir: &PrivateDir,
    name: impl AsRef<Path>,
    bytes: &[u8],
) -> std::io::Result<()> {
    dir.write_atomic_durable(name, bytes).map_err(io_error)
}

pub fn write_new(dir: &PrivateDir, name: impl AsRef<Path>, bytes: &[u8]) -> std::io::Result<()> {
    let mut writer = dir
        .atomic_with_policy(name, Durability::Full, private_fs::Publication::CreateNew)
        .map_err(io_error)?;
    writer.write_all(bytes)?;
    writer.commit().map_err(io_error)
}

pub fn open_lock(dir: &PrivateDir, name: &str) -> std::io::Result<std::fs::File> {
    let file = dir
        .open_file_with(
            name,
            OpenOptions {
                access: FileAccess::ReadWrite,
                creation: FileCreation::OpenOrCreate,
                truncate: false,
                verification: private_fs::Verification::OnDemand,
            },
        )
        .map_err(io_error)?;
    duplicate(&file)
}

pub fn already_exists(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::AlreadyExists
}

fn parent_file(path: &Path) -> std::io::Result<(PathBuf, &std::ffi::OsStr)> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty());
    let name = path.file_name();
    match (parent, name) {
        (Some(parent), Some(name)) => Ok((parent.to_path_buf(), name)),
        _ => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "a private file needs a parent directory and a file name",
        )),
    }
}

fn error_already_exists(error: &Error) -> bool {
    match error {
        Error::Io(source) => source.kind() == std::io::ErrorKind::AlreadyExists,
        Error::AtomicCommit { source, .. } => error_already_exists(source),
        _ => false,
    }
}

fn io_error(error: Error) -> std::io::Error {
    if error_already_exists(&error) {
        return std::io::Error::new(std::io::ErrorKind::AlreadyExists, error);
    }
    match error {
        Error::Io(source) => source,
        other => std::io::Error::other(other),
    }
}

fn duplicate(file: &PrivateFile) -> std::io::Result<std::fs::File> {
    #[cfg(unix)]
    {
        use std::os::fd::AsFd;
        let owned = rustix::io::dup(file.as_fd())?;
        Ok(std::fs::File::from(owned))
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::{AsHandle, OwnedHandle};
        let owned: OwnedHandle = file.as_handle().try_clone_to_owned()?;
        Ok(std::fs::File::from(owned))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = file;
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "private lock handles are unavailable",
        ))
    }
}
