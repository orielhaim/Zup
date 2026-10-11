use std::io::Write;
use std::os::fd::{AsFd, BorrowedFd};
use std::path::{Path, PathBuf};

use crate::error::PathError;
use rustix::fs::{AtFlags, Mode, OFlags, RenameFlags, renameat_with, statat};

pub const STATE_DIRECTORY_MODE: Mode = Mode::from_bits_truncate(0o700);

pub const STATE_FILE_MODE: Mode = Mode::from_bits_truncate(0o600);

pub const PAYLOAD_FILE_MODE: Mode = Mode::from_bits_truncate(0o644);

pub const EXECUTABLE_PAYLOAD_MODE: Mode = Mode::from_bits_truncate(0o744);

fn refuse_or_io(path: &Path, error: rustix::io::Errno) -> PathError {
    if error == rustix::io::Errno::LOOP {
        PathError::UnexpectedKind {
            path: path.display().to_string(),
            expected: "regular file, not a symbolic link",
        }
    } else {
        PathError::errno(path, error)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    Regular,
    Directory,
    Symlink,
    Special,
}

impl EntryKind {
    pub fn from_raw_mode(mode: u32) -> Self {
        match rustix::fs::FileType::from_raw_mode(mode) {
            rustix::fs::FileType::RegularFile => Self::Regular,
            rustix::fs::FileType::Directory => Self::Directory,
            rustix::fs::FileType::Symlink => Self::Symlink,
            _ => Self::Special,
        }
    }
}

#[derive(Debug)]
pub struct OwnedDirectory {
    path: PathBuf,
    file: std::fs::File,
}

impl OwnedDirectory {
    pub fn open(path: &Path) -> Result<Self, PathError> {
        let file = rustix::fs::open(
            path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .map_err(|error| PathError::errno(path, error))?;
        Ok(Self {
            path: path.to_path_buf(),
            file: std::fs::File::from(file),
        })
    }

    pub fn create(path: &Path, mode: Mode) -> Result<Self, PathError> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
            && !parent.exists()
        {
            Self::create(parent, mode)?;
        }
        match rustix::fs::mkdir(path, mode) {
            Ok(()) => {
                let directory = Self::open(path)?;
                directory.chmod(mode)?;
                return Ok(directory);
            }
            Err(rustix::io::Errno::EXIST) => {}
            Err(error) => return Err(PathError::errno(path, error)),
        }
        Self::open(path)
    }

    pub fn chmod(&self, mode: Mode) -> Result<(), PathError> {
        rustix::fs::fchmod(self.file.as_fd(), mode)
            .map_err(|error| PathError::errno(&self.path, error))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn as_fd(&self) -> BorrowedFd<'_> {
        self.file.as_fd()
    }

    fn child(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }

    pub fn sync(&self) -> Result<(), PathError> {
        rustix::fs::fsync(self.file.as_fd()).map_err(|error| PathError::errno(&self.path, error))
    }

    pub fn entry_kind(&self, name: &str) -> Result<EntryKind, PathError> {
        let stat = statat(self.as_fd(), name, AtFlags::SYMLINK_NOFOLLOW)
            .map_err(|error| PathError::errno(&self.child(name), error))?;
        Ok(EntryKind::from_raw_mode(stat.st_mode))
    }

    pub fn kind_or_absent(&self, name: &str) -> Result<Option<EntryKind>, PathError> {
        match self.entry_kind(name) {
            Ok(kind) => Ok(Some(kind)),
            Err(PathError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    pub fn write_durable(&self, name: &str, contents: &[u8], mode: Mode) -> Result<(), PathError> {
        let destination = self.child(name);
        let temporary = self.temporary_name(name);
        let temporary_path = self.child(&temporary);

        let written = self.write_temporary(&temporary, contents, mode);
        if let Err(error) = written {
            let _ = self.remove_file(&temporary);
            return Err(error);
        }
        let renamed = rustix::fs::rename(&temporary_path, &destination)
            .map_err(|error| PathError::errno(&destination, error));
        if let Err(error) = renamed {
            let _ = self.remove_file(&temporary);
            return Err(error);
        }
        self.sync()
    }

    pub fn create_durable_exclusive(
        &self,
        name: &str,
        contents: &[u8],
        mode: Mode,
    ) -> Result<(), PathError> {
        let destination = self.child(name);
        let temporary = self.temporary_name(name);
        let temporary_path = self.child(&temporary);

        if let Err(error) = self.write_temporary(&temporary, contents, mode) {
            let _ = self.remove_file(&temporary);
            return Err(error);
        }

        let renamed = renameat_with(
            self.as_fd(),
            &temporary_path,
            self.as_fd(),
            &destination,
            RenameFlags::NOREPLACE,
        );
        let _ = self.remove_file(&temporary);
        match renamed {
            Ok(()) => self.sync(),
            Err(rustix::io::Errno::EXIST) => Err(PathError::AlreadyExists {
                path: destination.display().to_string(),
            }),
            Err(error) => Err(PathError::errno(&destination, error)),
        }
    }

    pub fn publish_exclusive(&self, name: &str, source: &str) -> Result<(), PathError> {
        let destination = self.child(name);
        match renameat_with(
            self.as_fd(),
            self.child(source),
            self.as_fd(),
            &destination,
            RenameFlags::NOREPLACE,
        ) {
            Ok(()) => self.sync(),
            Err(rustix::io::Errno::EXIST) => Err(PathError::AlreadyExists {
                path: destination.display().to_string(),
            }),
            Err(error) => Err(PathError::errno(&destination, error)),
        }
    }

    pub fn publish_replacing(&self, name: &str, source: &str) -> Result<(), PathError> {
        let destination = self.child(name);
        rustix::fs::rename(self.child(source), &destination)
            .map_err(|error| PathError::errno(&destination, error))?;
        self.sync()
    }

    pub fn write_payload(
        &self,
        name: &str,
        contents: &[u8],
        executable: bool,
    ) -> Result<(), PathError> {
        let path = self.child(name);
        let mode = if executable {
            EXECUTABLE_PAYLOAD_MODE
        } else {
            PAYLOAD_FILE_MODE
        };
        let file = rustix::fs::open(
            &path,
            OFlags::WRONLY | OFlags::CREATE | OFlags::TRUNC | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            mode,
        )
        .map_err(|error| refuse_or_io(&path, error))?;
        let mut file = std::fs::File::from(file);
        file.write_all(contents)
            .map_err(|error| PathError::io(&path, error))?;
        rustix::fs::fchmod(file.as_fd(), mode).map_err(|error| PathError::errno(&path, error))
    }

    pub fn open_regular_read(&self, name: &str) -> Result<std::fs::File, PathError> {
        let path = self.child(name);
        let file = rustix::fs::open(
            &path,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .map_err(|error| refuse_or_io(&path, error))?;
        let stat =
            rustix::fs::fstat(file.as_fd()).map_err(|error| PathError::errno(&path, error))?;
        if EntryKind::from_raw_mode(stat.st_mode) != EntryKind::Regular {
            return Err(PathError::UnexpectedKind {
                path: path.display().to_string(),
                expected: "regular file",
            });
        }
        Ok(std::fs::File::from(file))
    }

    pub fn read_regular(&self, name: &str) -> Result<Vec<u8>, PathError> {
        use std::io::Read as _;
        let mut file = self.open_regular_read(name)?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|error| PathError::io(&self.child(name), error))?;
        Ok(bytes)
    }

    pub fn remove_file(&self, name: &str) -> Result<(), PathError> {
        rustix::fs::unlinkat(self.as_fd(), name, AtFlags::empty())
            .map_err(|error| PathError::errno(&self.child(name), error))
    }

    pub fn read_link_target(&self, name: &str) -> Result<PathBuf, PathError> {
        match self.kind_or_absent(name)? {
            Some(EntryKind::Symlink) => {}
            Some(_) => {
                return Err(PathError::UnexpectedKind {
                    path: self.child(name).display().to_string(),
                    expected: "a symbolic link",
                });
            }
            None => {
                return Err(PathError::Missing {
                    path: self.child(name).display().to_string(),
                });
            }
        }
        use std::os::unix::ffi::OsStrExt as _;
        let target = rustix::fs::readlinkat(self.as_fd(), name, Vec::new())
            .map_err(|error| PathError::errno(&self.child(name), error))?;
        Ok(PathBuf::from(std::ffi::OsStr::from_bytes(
            target.as_bytes(),
        )))
    }

    pub fn remove_empty_directory(&self, name: &str) -> Result<(), PathError> {
        rustix::fs::unlinkat(self.as_fd(), name, AtFlags::REMOVEDIR)
            .map_err(|error| PathError::errno(&self.child(name), error))
    }

    pub fn is_empty_directory(&self, name: &str) -> Result<bool, PathError> {
        let file = rustix::fs::open(
            self.child(name),
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .map_err(|error| PathError::errno(&self.child(name), error))?;
        let mut entries = rustix::fs::Dir::new(file)
            .map_err(|error| PathError::errno(&self.child(name), error))?;
        Ok(entries.next().is_none())
    }

    pub fn remove_tree(&self) -> Result<(), PathError> {
        let entries =
            std::fs::read_dir(&self.path).map_err(|error| PathError::io(&self.path, error))?;
        for entry in entries {
            let entry = entry.map_err(|error| PathError::io(&self.path, error))?;
            let file_type = entry
                .file_type()
                .map_err(|error| PathError::io(&entry.path(), error))?;
            if file_type.is_dir() {
                OwnedDirectory::open(&entry.path())?.remove_tree()?;
            }

            let at = rustix::fs::CWD;
            let _ = rustix::fs::unlinkat(
                at,
                entry.path(),
                if file_type.is_dir() {
                    AtFlags::REMOVEDIR
                } else {
                    AtFlags::empty()
                },
            );
        }
        rustix::fs::unlinkat(rustix::fs::CWD, &self.path, AtFlags::REMOVEDIR)
            .map_err(|error| PathError::errno(&self.path, error))
    }

    fn write_temporary(
        &self,
        temporary: &str,
        contents: &[u8],
        mode: Mode,
    ) -> Result<(), PathError> {
        let path = self.child(temporary);

        let file = rustix::fs::open(
            &path,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            mode,
        )
        .map_err(|error| PathError::errno(&path, error))?;
        let mut file = std::fs::File::from(file);
        file.write_all(contents)
            .map_err(|error| PathError::io(&path, error))?;

        file.sync_all().map_err(|error| PathError::io(&path, error))
    }

    fn temporary_name(&self, name: &str) -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        format!(".{name}.zup-tmp-{}-{sequence:x}", std::process::id())
    }
}

pub fn sync_directory(path: &Path) -> Result<(), PathError> {
    OwnedDirectory::open(path)?.sync()
}

pub fn refuse_symlink_ancestors(path: &Path) -> Result<(), PathError> {
    let mut prefix = PathBuf::new();
    for component in path.components() {
        prefix.push(component.as_os_str());
        let metadata = match std::fs::symlink_metadata(&prefix) {
            Ok(metadata) => metadata,

            Err(source) if source.kind() == std::io::ErrorKind::NotFound => continue,
            Err(source) => {
                return Err(PathError::io(&prefix, source));
            }
        };
        if metadata.file_type().is_symlink() {
            return Err(PathError::UnexpectedKind {
                path: prefix.display().to_string(),
                expected: "a real directory, not a symbolic link",
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn directory() -> tempfile::TempDir {
        tempfile::tempdir().expect("a temp directory")
    }

    #[test]
    fn a_durable_write_is_readable_afterwards() {
        let root = directory();
        let owned = OwnedDirectory::create(root.path(), STATE_DIRECTORY_MODE).expect("create");
        owned
            .write_durable("ledger", b"the ledger", STATE_FILE_MODE)
            .expect("write");
        assert_eq!(owned.read_regular("ledger").expect("read"), b"the ledger");
    }

    #[test]
    fn a_durable_write_replaces_what_was_there() {
        let root = directory();
        let owned = OwnedDirectory::create(root.path(), STATE_DIRECTORY_MODE).expect("create");
        owned
            .write_durable("ledger", b"v1", STATE_FILE_MODE)
            .expect("first");
        owned
            .write_durable("ledger", b"v2", STATE_FILE_MODE)
            .expect("second");
        assert_eq!(owned.read_regular("ledger").expect("read"), b"v2");
    }

    #[test]
    fn a_durable_write_leaves_no_temporary_behind() {
        let root = directory();
        let owned = OwnedDirectory::create(root.path(), STATE_DIRECTORY_MODE).expect("create");
        owned
            .write_durable("ledger", b"v1", STATE_FILE_MODE)
            .expect("write");
        let names: Vec<_> = std::fs::read_dir(root.path())
            .expect("read dir")
            .map(|entry| {
                entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(names, vec!["ledger".to_string()]);
    }

    #[test]
    fn an_exclusive_create_refuses_to_overwrite() {
        let root = directory();
        let owned = OwnedDirectory::create(root.path(), STATE_DIRECTORY_MODE).expect("create");
        owned
            .create_durable_exclusive("lock", b"first", STATE_FILE_MODE)
            .expect("first");
        assert!(matches!(
            owned.create_durable_exclusive("lock", b"second", STATE_FILE_MODE),
            Err(PathError::AlreadyExists { .. })
        ));
        assert_eq!(
            owned.read_regular("lock").expect("read"),
            b"first",
            "the refused creation must not have touched the contents"
        );
    }

    #[test]
    fn a_created_directory_ends_up_with_the_requested_mode_not_the_umasks() {
        let root = directory();

        let owned =
            OwnedDirectory::create(&root.path().join("state"), Mode::from_bits_truncate(0o777))
                .expect("create");
        assert_eq!(
            rustix::fs::fstat(owned.as_fd()).expect("stat").st_mode & 0o777,
            0o777,
            "the requested mode is the resulting mode, not the umask's version of it"
        );
    }

    #[test]
    fn zup_state_is_private() {
        let root = directory();
        let owned = OwnedDirectory::create(&root.path().join("state"), STATE_DIRECTORY_MODE)
            .expect("create");
        assert_eq!(
            rustix::fs::fstat(owned.as_fd()).expect("stat").st_mode & 0o777,
            0o700,
            "the ledger, journal and work directory are zup's private state"
        );
    }

    #[test]
    fn a_payload_will_not_be_written_through_a_symlink() {
        let root = directory();
        let owned = OwnedDirectory::create(root.path(), STATE_DIRECTORY_MODE).expect("create");
        let outside = directory();
        let victim = outside.path().join("victim");
        std::fs::write(&victim, b"someone else's file").expect("write");
        std::os::unix::fs::symlink(&victim, owned.path().join("tool")).expect("symlink");

        assert!(matches!(
            owned.write_payload("tool", b"the payload", true),
            Err(PathError::UnexpectedKind { .. })
        ));
        assert_eq!(
            std::fs::read(&victim).expect("read"),
            b"someone else's file",
            "the file outside the install directory is untouched"
        );
    }

    #[rstest]
    #[case::symlink(EntryKind::Symlink)]
    #[case::directory(EntryKind::Directory)]
    fn reading_refuses_anything_that_is_not_a_regular_file(
        #[case] planted: EntryKind,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let root = directory();
        let owned = OwnedDirectory::create(root.path(), STATE_DIRECTORY_MODE)?;
        match planted {
            EntryKind::Symlink => {
                let other = directory();
                std::fs::write(other.path().join("target"), b"elsewhere")?;
                std::os::unix::fs::symlink(
                    other.path().join("target"),
                    owned.path().join("thing"),
                )?;
            }
            EntryKind::Directory => {
                OwnedDirectory::create(&owned.path().join("thing"), STATE_DIRECTORY_MODE)?;
            }
            _ => unreachable!("the case matrix only plants two kinds"),
        }
        assert!(matches!(
            owned.read_regular("thing"),
            Err(PathError::UnexpectedKind { .. })
        ));
        Ok(())
    }

    #[test]
    fn executable_intent_is_applied_and_can_be_withdrawn() {
        let root = directory();
        let owned = OwnedDirectory::create(root.path(), STATE_DIRECTORY_MODE).expect("create");

        owned
            .write_payload("tool", b"#!/bin/sh\n", true)
            .expect("as executable");
        assert_eq!(
            mode_of(&owned.path().join("tool")) & 0o111,
            0o100,
            "owner-executable"
        );

        owned
            .write_payload("data", b"plain\n", false)
            .expect("as data");
        assert_eq!(
            mode_of(&owned.path().join("data")) & 0o111,
            0,
            "not executable"
        );

        owned
            .write_payload("tool", b"now a data file\n", false)
            .expect("as data");
        assert_eq!(
            mode_of(&owned.path().join("tool")) & 0o111,
            0,
            "an existing file's mode is set explicitly, or a retired executable stays executable"
        );
        assert_eq!(
            mode_of(&owned.path().join("tool")) & 0o222,
            0o200,
            "and nothing became writable"
        );
    }

    fn mode_of(path: &Path) -> u32 {
        std::fs::symlink_metadata(path)
            .expect("stat")
            .permissions()
            .mode()
            & 0o777
    }

    use std::os::unix::fs::PermissionsExt as _;

    #[test]
    fn a_directory_with_someone_elses_file_in_it_is_not_removed() {
        let root = directory();
        let owned = OwnedDirectory::create(root.path(), STATE_DIRECTORY_MODE).expect("create");
        let install = OwnedDirectory::create(&owned.path().join("install"), STATE_DIRECTORY_MODE)
            .expect("create install");
        install
            .write_payload("payload", b"zup's", false)
            .expect("payload");
        install
            .write_payload("user-notes.txt", b"the user's", false)
            .expect("user file");

        assert!(matches!(
            owned.remove_empty_directory("install"),
            Err(PathError::Io { .. })
        ));
        assert!(
            owned.path().join("install").join("user-notes.txt").exists(),
            "a refused removal leaves the whole tree alone"
        );
    }

    #[test]
    fn removing_a_work_tree_unlinks_a_link_rather_than_following_it() {
        let root = directory();
        let owned = OwnedDirectory::create(root.path(), STATE_DIRECTORY_MODE).expect("create");
        let outside = directory();
        let victim = outside.path().join("precious");
        std::fs::write(&victim, b"not part of the work tree").expect("write");
        std::os::unix::fs::symlink(&victim, owned.path().join("scratch")).expect("symlink");

        owned.remove_tree().expect("remove");

        assert!(
            victim.exists(),
            "the linked-to file is outside the tree and survives"
        );
        assert!(!root.path().exists(), "the work tree itself is gone");
    }

    #[test]
    fn removing_an_absent_tree_is_not_a_failure() {
        let root = directory();
        let owned = OwnedDirectory::create(root.path(), STATE_DIRECTORY_MODE).expect("create");
        let work = OwnedDirectory::create(&owned.path().join("work"), STATE_DIRECTORY_MODE)
            .expect("create");
        work.remove_tree().expect("first removal");
        assert!(matches!(work.remove_tree(), Err(PathError::Io { .. })));
    }
}
