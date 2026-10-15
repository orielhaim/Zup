use std::io::{Read, Write};
use std::path::Path;

use private_fs::{
    Durability, Error, FileAccess, FileCreation, OpenOptions, PrivateDir, PrivateFile, Publication,
};

use crate::record::PublishError;

/// Create missing ancestors and make each new directory entry durable in its parent.
///
/// `PrivateDir::ensure_dir_all` creates components sequentially and does not sync
/// those parents. A journal must not depend on a directory whose name can still
/// disappear in a crash, so each newly created component is followed by
/// `sync_all` on the parent that names it.
pub fn ensure_dir(path: &Path) -> Result<PrivateDir, Error> {
    if let Ok(dir) = PrivateDir::open(path) {
        return Ok(dir);
    }
    let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    else {
        return PrivateDir::ensure(path);
    };
    let Some(name) = path.file_name() else {
        return Err(Error::InvalidRelativePath);
    };
    if !parent.exists() {
        let parent = ensure_dir(parent)?;
        let created = parent.ensure_dir(name)?;
        parent.sync_all()?;
        return Ok(created);
    }
    match PrivateDir::open(parent) {
        Ok(parent_dir) => {
            let created = parent_dir.ensure_dir(name)?;
            parent_dir.sync_all()?;
            Ok(created)
        }
        Err(_) => {
            let created = PrivateDir::ensure(path)?;
            sync_parent(parent)?;
            Ok(created)
        }
    }
}

fn sync_parent(parent: &Path) -> Result<(), Error> {
    #[cfg(windows)]
    let file = {
        use std::os::windows::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .access_mode(0x4000_0000)
            .share_mode(0x7)
            .custom_flags(0x0200_0000)
            .open(parent)
            .map_err(Error::from)?
    };
    #[cfg(not(windows))]
    let file = std::fs::File::open(parent).map_err(Error::from)?;
    file.sync_all().map_err(Error::from)?;
    Ok(())
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
    publish_full(&dir, name, bytes, Publication::Replace).map_err(Error::from)
}

/// Publish `bytes` with `Durability::Full`.
///
/// A successful `AtomicFile::commit` is the only normal success. When
/// `may_have_published` is false the namespace is unchanged and the error is
/// returned. When it is true, success is returned only if a capability-relative
/// open verifies the private object, its bytes are exactly `bytes`, the parent
/// directory sync completes, and a second verified read still matches. Matching
/// bytes without that sync, or a failed security check, stay indeterminate.
pub fn publish_full(
    dir: &PrivateDir,
    name: impl AsRef<Path>,
    bytes: &[u8],
    policy: Publication,
) -> Result<(), PublishError> {
    let name = name.as_ref();
    let mut writer = dir
        .atomic_with_policy(name, Durability::Full, policy)
        .map_err(not_published)?;
    writer
        .write_all(bytes)
        .map_err(|error| PublishError::NotPublished {
            kind: Some(error.kind()),
            detail: error.to_string(),
        })?;
    match writer.commit() {
        Ok(()) => Ok(()),
        Err(Error::AtomicCommit {
            may_have_published: false,
            source,
        }) => Err(not_published(*source)),
        Err(Error::AtomicCommit {
            may_have_published: true,
            source,
        }) => confirm_published(dir, name, bytes).map_err(|detail| PublishError::Indeterminate {
            may_have_published: true,
            detail: format!("{source}; {detail}"),
        }),
        Err(error) => Err(not_published(error)),
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

pub fn absent_collision(error: &PublishError) -> bool {
    matches!(
        error,
        PublishError::NotPublished {
            kind: Some(std::io::ErrorKind::AlreadyExists),
            ..
        }
    )
}

fn confirm_published(dir: &PrivateDir, name: &Path, bytes: &[u8]) -> Result<(), String> {
    let found = read_file(dir, name).map_err(|error| error.to_string())?;
    if found != bytes {
        return Err("visible bytes are not the attempted record".into());
    }
    dir.sync_all()
        .map_err(|error| format!("directory sync: {error}"))?;
    let again = read_file(dir, name).map_err(|error| error.to_string())?;
    if again != bytes {
        return Err("verified bytes changed across the durability barrier".into());
    }
    Ok(())
}

fn not_published(error: Error) -> PublishError {
    PublishError::NotPublished {
        kind: io_kind(&error),
        detail: error.to_string(),
    }
}

fn io_kind(error: &Error) -> Option<std::io::ErrorKind> {
    match error {
        Error::Io(source) => Some(source.kind()),
        Error::AtomicCommit { source, .. } => io_kind(source),
        _ => None,
    }
}

impl From<PublishError> for Error {
    fn from(error: PublishError) -> Self {
        match error {
            PublishError::NotPublished { kind, detail } => Error::Io(std::io::Error::new(
                kind.unwrap_or(std::io::ErrorKind::Other),
                detail,
            )),
            PublishError::Indeterminate {
                may_have_published,
                detail,
            } => Error::AtomicCommit {
                may_have_published,
                source: Box::new(Error::Io(std::io::Error::other(detail))),
            },
        }
    }
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
    use super::*;
    use std::sync::Arc;

    #[test]
    fn a_private_publication_is_durable_and_rereadable() {
        let root = tempfile::tempdir().expect("temp");
        let dir = ensure_dir(&root.path().join("journals")).expect("dir");
        publish_full(
            &dir,
            "transaction.json",
            b"{\"revision\":1}",
            Publication::CreateNew,
        )
        .expect("publish");
        let bytes = read_file(&dir, "transaction.json").expect("read");
        assert_eq!(bytes, b"{\"revision\":1}");
    }

    #[test]
    fn a_second_create_is_a_collision_and_does_not_replace() {
        let root = tempfile::tempdir().expect("temp");
        let dir = ensure_dir(&root.path().join("journals")).expect("dir");
        publish_full(&dir, "transaction.json", b"first", Publication::CreateNew).expect("first");
        let error = publish_full(&dir, "transaction.json", b"second", Publication::CreateNew)
            .expect_err("create-new collides");
        assert!(absent_collision(&error), "{error}");
        assert_eq!(read_file(&dir, "transaction.json").expect("read"), b"first");
    }

    #[test]
    fn mismatched_visible_bytes_stay_indeterminate() {
        let root = tempfile::tempdir().expect("temp");
        let dir = ensure_dir(&root.path().join("journals")).expect("dir");
        publish_full(&dir, "transaction.json", b"current", Publication::CreateNew).expect("seed");
        let error = confirm_published(&dir, Path::new("transaction.json"), b"attempted")
            .expect_err("different bytes");
        assert!(error.contains("not the attempted record"), "{error}");
    }

    #[test]
    fn verified_bytes_survive_a_directory_sync() {
        let root = tempfile::tempdir().expect("temp");
        let dir = Arc::new(ensure_dir(&root.path().join("journals")).expect("dir"));
        publish_full(&dir, "transaction.json", b"record", Publication::CreateNew).expect("seed");
        confirm_published(&dir, Path::new("transaction.json"), b"record").expect("confirm");
    }
}
