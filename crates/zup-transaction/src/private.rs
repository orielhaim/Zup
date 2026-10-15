use std::io::{Read, Write};
use std::path::Path;

use private_fs::{
    Durability, Error, FileAccess, FileCreation, OpenOptions, PrivateDir, PrivateFile, Publication,
};

pub fn ensure_dir(path: &Path) -> Result<PrivateDir, Error> {
    match PrivateDir::ensure(path) {
        Ok(dir) => Ok(dir),
        Err(Error::Io(source)) if source.kind() == std::io::ErrorKind::NotFound => {
            let Some(parent) = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
            else {
                return Err(Error::Io(source));
            };
            let Some(name) = path.file_name() else {
                return Err(Error::Io(source));
            };
            let parent = ensure_dir(parent)?;
            parent.ensure_dir(name)
        }
        Err(error) => Err(error),
    }
}

pub fn read_private(path: &Path) -> Result<Vec<u8>, Error> {
    let (parent, name) = parent_file(path)?;
    let dir = PrivateDir::open(parent)?;
    read_file(&dir, name)
}

pub fn read_file(dir: &PrivateDir, name: impl AsRef<Path>) -> Result<Vec<u8>, Error> {
    let mut file = dir.open_file(name)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(Error::from)?;
    Ok(bytes)
}

pub fn publish_private(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    let (parent, name) = parent_file(path)?;
    let dir = ensure_dir(parent)?;
    publish_full(&dir, name, bytes, Publication::Replace)
}

/// Publish `bytes` with file sync, atomic visibility, and parent-directory sync.
///
/// A commit that fails after the rename may have made the bytes visible without
/// finishing directory durability. That result stays an error unless a fresh
/// `PrivateDir::sync_all` completes and the visible private file is exactly
/// `bytes`. A matching read without that sync is not success. A validation
/// failure after publication is not retried into success.
pub fn publish_full(
    dir: &PrivateDir,
    name: impl AsRef<Path>,
    bytes: &[u8],
    policy: Publication,
) -> Result<(), Error> {
    let name = name.as_ref();
    let mut writer = dir.atomic_with_policy(name, Durability::Full, policy)?;
    writer.write_all(bytes)?;
    match writer.commit() {
        Ok(()) => Ok(()),
        Err(Error::AtomicCommit {
            may_have_published,
            source,
        }) => finish_uncertain(dir, name, bytes, may_have_published, *source),
        Err(error) => Err(error),
    }
}

pub fn open_lock(dir: &PrivateDir, name: &str) -> Result<std::fs::File, Error> {
    let file = dir.open_file_with(
        name,
        OpenOptions {
            access: FileAccess::ReadWrite,
            creation: FileCreation::OpenOrCreate,
            truncate: false,
            verification: private_fs::Verification::OnDemand,
        },
    )?;
    duplicate(&file)
}

pub fn absent_collision(error: &Error) -> bool {
    match error {
        Error::Io(source) => source.kind() == std::io::ErrorKind::AlreadyExists,
        Error::AtomicCommit {
            may_have_published: false,
            source,
        } => absent_collision(source),
        _ => false,
    }
}

fn finish_uncertain(
    dir: &PrivateDir,
    name: &Path,
    bytes: &[u8],
    may_have_published: bool,
    source: Error,
) -> Result<(), Error> {
    let failure_is_directory_sync = sync_failure(&source);
    if !may_have_published || !failure_is_directory_sync {
        return Err(Error::AtomicCommit {
            may_have_published,
            source: Box::new(source),
        });
    }
    let synced = dir.sync_all();
    let bytes_match = synced.is_ok() && read_file(dir, name).is_ok_and(|found| found == bytes);
    if durable_after_uncertain(true, true, synced.is_ok(), bytes_match) {
        return Ok(());
    }
    match synced {
        Err(sync_error) => Err(Error::AtomicCommit {
            may_have_published: true,
            source: Box::new(sync_error),
        }),
        Ok(()) => Err(Error::AtomicCommit {
            may_have_published: true,
            source: Box::new(source),
        }),
    }
}

fn sync_failure(error: &Error) -> bool {
    match error {
        Error::Io(_) => true,
        Error::AtomicCommit { source, .. } => sync_failure(source),
        _ => false,
    }
}

/// Directory durability is established only when publication may have happened,
/// the failure is the directory sync itself, that sync is then completed, and
/// the visible bytes are the ones just written. Any missing part is failure.
pub(crate) fn durable_after_uncertain(
    may_have_published: bool,
    failure_is_directory_sync: bool,
    directory_sync_completed: bool,
    bytes_match: bool,
) -> bool {
    may_have_published && failure_is_directory_sync && directory_sync_completed && bytes_match
}

fn parent_file(path: &Path) -> Result<(&Path, &std::ffi::OsStr), Error> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty());
    let name = path.file_name();
    match (parent, name) {
        (Some(parent), Some(name)) => Ok((parent, name)),
        _ => Err(Error::InvalidRelativePath),
    }
}

fn duplicate(file: &PrivateFile) -> Result<std::fs::File, Error> {
    #[cfg(unix)]
    {
        use std::os::fd::AsFd;
        let owned = rustix::io::dup(file.as_fd()).map_err(Error::from)?;
        Ok(std::fs::File::from(owned))
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::{AsHandle, OwnedHandle};
        let owned: OwnedHandle = file.as_handle().try_clone_to_owned().map_err(Error::from)?;
        Ok(std::fs::File::from(owned))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = file;
        Err(Error::UnsupportedFilesystem {
            capability: "private lock handles",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::durable_after_uncertain;

    #[test]
    fn visible_bytes_without_directory_sync_are_not_durable() {
        assert!(
            !durable_after_uncertain(true, true, false, true),
            "a readable record whose parent sync failed is not a committed revision"
        );
    }

    #[test]
    fn a_pre_publication_failure_stays_a_failure_when_bytes_match() {
        assert!(!durable_after_uncertain(false, true, true, true));
    }

    #[test]
    fn a_validation_failure_is_not_repaired_by_syncing() {
        assert!(!durable_after_uncertain(true, false, true, true));
    }

    #[test]
    fn directory_sync_accepts_only_the_bytes_that_were_published() {
        assert!(durable_after_uncertain(true, true, true, true));
        assert!(!durable_after_uncertain(true, true, true, false));
    }
}
