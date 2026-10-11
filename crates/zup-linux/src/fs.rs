use std::ffi::OsStr;
use std::io::Write;
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

use crate::error::PathError;
use rustix::fs::{AtFlags, Mode, OFlags, RenameFlags, renameat_with, statat};

pub const STATE_DIRECTORY_MODE: Mode = Mode::from_bits_truncate(0o700);

pub const STATE_FILE_MODE: Mode = Mode::from_bits_truncate(0o600);

pub const PAYLOAD_FILE_MODE: Mode = Mode::from_bits_truncate(0o644);

pub const EXECUTABLE_PAYLOAD_MODE: Mode = Mode::from_bits_truncate(0o744);

const DIR_FLAGS: OFlags = OFlags::from_bits_truncate(
    OFlags::RDONLY.bits()
        | OFlags::DIRECTORY.bits()
        | OFlags::CLOEXEC.bits()
        | OFlags::NOFOLLOW.bits(),
);

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

fn check_child_name(name: &str) -> Result<(), PathError> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\0') {
        return Err(PathError::InvalidComponent {
            component: name.to_owned(),
            reason: "a child name is a single plain file name".to_owned(),
        });
    }
    Ok(())
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
        let (directory, _) = walk(path, Mode::empty(), false)?;
        Ok(directory)
    }

    pub fn create(path: &Path, mode: Mode) -> Result<Self, PathError> {
        let (directory, _) = walk(path, mode, true)?;
        Ok(directory)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn as_fd(&self) -> BorrowedFd<'_> {
        self.file.as_fd()
    }

    fn display_child(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }

    pub fn sync(&self) -> Result<(), PathError> {
        rustix::fs::fsync(self.file.as_fd()).map_err(|error| PathError::errno(&self.path, error))
    }

    pub fn entry_kind(&self, name: &str) -> Result<EntryKind, PathError> {
        check_child_name(name)?;
        let stat = statat(self.as_fd(), name, AtFlags::SYMLINK_NOFOLLOW)
            .map_err(|error| PathError::errno(&self.display_child(name), error))?;
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
        check_child_name(name)?;
        let temporary = self.temporary_name(name);
        if let Err(error) = self.write_temporary(&temporary, contents, mode) {
            let _ = self.remove_file(&temporary);
            return Err(error);
        }
        let renamed = rustix::fs::renameat(self.as_fd(), temporary.as_str(), self.as_fd(), name)
            .map_err(|error| PathError::errno(&self.display_child(name), error));
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
        check_child_name(name)?;
        let temporary = self.temporary_name(name);
        if let Err(error) = self.write_temporary(&temporary, contents, mode) {
            let _ = self.remove_file(&temporary);
            return Err(error);
        }
        let renamed = renameat_with(
            self.as_fd(),
            temporary.as_str(),
            self.as_fd(),
            name,
            RenameFlags::NOREPLACE,
        );
        let _ = self.remove_file(&temporary);
        match renamed {
            Ok(()) => self.sync(),
            Err(rustix::io::Errno::EXIST) => Err(PathError::AlreadyExists {
                path: self.display_child(name).display().to_string(),
            }),
            Err(error) => Err(PathError::errno(&self.display_child(name), error)),
        }
    }

    pub fn publish_exclusive(&self, name: &str, source: &str) -> Result<(), PathError> {
        check_child_name(name)?;
        check_child_name(source)?;
        match renameat_with(
            self.as_fd(),
            source,
            self.as_fd(),
            name,
            RenameFlags::NOREPLACE,
        ) {
            Ok(()) => self.sync(),
            Err(rustix::io::Errno::EXIST) => Err(PathError::AlreadyExists {
                path: self.display_child(name).display().to_string(),
            }),
            Err(error) => Err(PathError::errno(&self.display_child(name), error)),
        }
    }

    pub fn publish_replacing(&self, name: &str, source: &str) -> Result<(), PathError> {
        check_child_name(name)?;
        check_child_name(source)?;
        rustix::fs::renameat(self.as_fd(), source, self.as_fd(), name)
            .map_err(|error| PathError::errno(&self.display_child(name), error))?;
        self.sync()
    }

    pub fn chmod_regular(&self, name: &str, mode: Mode) -> Result<(), PathError> {
        check_child_name(name)?;
        let display = self.display_child(name);
        let file = rustix::fs::openat(
            self.as_fd(),
            name,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .map_err(|error| refuse_or_io(&display, error))?;
        let stat =
            rustix::fs::fstat(file.as_fd()).map_err(|error| PathError::errno(&display, error))?;
        if EntryKind::from_raw_mode(stat.st_mode) != EntryKind::Regular {
            return Err(PathError::UnexpectedKind {
                path: display.display().to_string(),
                expected: "regular file",
            });
        }
        rustix::fs::fchmod(file.as_fd(), mode)
            .map_err(|error| PathError::errno(&display, error))?;
        Ok(())
    }

    pub fn open_regular_read(&self, name: &str) -> Result<std::fs::File, PathError> {
        check_child_name(name)?;
        let display = self.display_child(name);
        let file = rustix::fs::openat(
            self.as_fd(),
            name,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .map_err(|error| refuse_or_io(&display, error))?;
        let stat =
            rustix::fs::fstat(file.as_fd()).map_err(|error| PathError::errno(&display, error))?;
        if EntryKind::from_raw_mode(stat.st_mode) != EntryKind::Regular {
            return Err(PathError::UnexpectedKind {
                path: display.display().to_string(),
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
            .map_err(|error| PathError::io(&self.display_child(name), error))?;
        Ok(bytes)
    }

    pub fn read_regular_stat(&self, name: &str) -> Result<(Vec<u8>, u32), PathError> {
        use std::io::Read as _;
        let mut file = self.open_regular_read(name)?;
        let stat = rustix::fs::fstat(file.as_fd())
            .map_err(|error| PathError::errno(&self.display_child(name), error))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|error| PathError::io(&self.display_child(name), error))?;
        Ok((bytes, stat.st_mode & 0o7777))
    }

    pub fn remove_file(&self, name: &str) -> Result<(), PathError> {
        check_child_name(name)?;
        rustix::fs::unlinkat(self.as_fd(), name, AtFlags::empty())
            .map_err(|error| PathError::errno(&self.display_child(name), error))
    }

    pub fn read_link_target(&self, name: &str) -> Result<PathBuf, PathError> {
        check_child_name(name)?;
        match self.kind_or_absent(name)? {
            Some(EntryKind::Symlink) => {}
            Some(_) => {
                return Err(PathError::UnexpectedKind {
                    path: self.display_child(name).display().to_string(),
                    expected: "a symbolic link",
                });
            }
            None => {
                return Err(PathError::Missing {
                    path: self.display_child(name).display().to_string(),
                });
            }
        }
        let target = rustix::fs::readlinkat(self.as_fd(), name, Vec::new())
            .map_err(|error| PathError::errno(&self.display_child(name), error))?;
        Ok(PathBuf::from(std::ffi::OsStr::from_bytes(
            target.as_bytes(),
        )))
    }

    pub fn remove_empty_directory(&self, name: &str) -> Result<(), PathError> {
        check_child_name(name)?;
        rustix::fs::unlinkat(self.as_fd(), name, AtFlags::REMOVEDIR)
            .map_err(|error| PathError::errno(&self.display_child(name), error))
    }

    pub fn is_empty_directory(&self, name: &str) -> Result<bool, PathError> {
        check_child_name(name)?;
        let display = self.display_child(name);
        let file = rustix::fs::openat(self.as_fd(), name, DIR_FLAGS, Mode::empty())
            .map_err(|error| PathError::errno(&display, error))?;
        let mut entries =
            rustix::fs::Dir::new(file).map_err(|error| PathError::errno(&display, error))?;
        Ok(entries
            .find_map(|entry| match entry {
                Ok(entry) => {
                    let name = entry.file_name().to_bytes();
                    if name == b"." || name == b".." {
                        None
                    } else {
                        Some(())
                    }
                }
                Err(_) => Some(()),
            })
            .is_none())
    }

    pub fn remove_contents(&self) -> Result<(), PathError> {
        let file = rustix::fs::openat(self.as_fd(), ".", DIR_FLAGS, Mode::empty())
            .map_err(|error| PathError::errno(&self.path, error))?;
        let entries =
            rustix::fs::Dir::new(file).map_err(|error| PathError::errno(&self.path, error))?;
        let mut names = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|error| PathError::errno(&self.path, error))?;
            let name = entry.file_name().to_bytes().to_vec();
            if name == b"." || name == b".." {
                continue;
            }
            names.push(name);
        }
        for name in &names {
            remove_child_os(self.as_fd(), &self.path, OsStr::from_bytes(name.as_slice()))?;
        }
        self.sync()
    }

    pub fn remove_child_tree(&self, name: &str) -> Result<(), PathError> {
        check_child_name(name)?;
        remove_child_os(self.as_fd(), &self.path, OsStr::from_bytes(name.as_bytes()))
    }

    pub fn open_child(&self, name: &str) -> Result<Self, PathError> {
        check_child_name(name)?;
        let display = self.display_child(name);
        let file = rustix::fs::openat(self.as_fd(), name, DIR_FLAGS, Mode::empty())
            .map_err(|error| PathError::errno(&display, error))?;
        Ok(Self {
            path: display,
            file: std::fs::File::from(file),
        })
    }

    pub fn child_names(&self) -> Result<Vec<String>, PathError> {
        let file = rustix::fs::openat(self.as_fd(), ".", DIR_FLAGS, Mode::empty())
            .map_err(|error| PathError::errno(&self.path, error))?;
        let entries =
            rustix::fs::Dir::new(file).map_err(|error| PathError::errno(&self.path, error))?;
        let mut names = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|error| PathError::errno(&self.path, error))?;
            let name = entry.file_name().to_bytes();
            if name == b"." || name == b".." {
                continue;
            }
            let Some(text) = std::str::from_utf8(name).ok().map(str::to_owned) else {
                return Err(PathError::InvalidComponent {
                    component: self.path.display().to_string(),
                    reason: "a directory entry is text".to_owned(),
                });
            };
            names.push(text);
        }
        names.sort();
        Ok(names)
    }

    fn write_temporary(
        &self,
        temporary: &str,
        contents: &[u8],
        mode: Mode,
    ) -> Result<(), PathError> {
        check_child_name(temporary)?;
        let display = self.display_child(temporary);
        let file = rustix::fs::openat(
            self.as_fd(),
            temporary,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            mode,
        )
        .map_err(|error| PathError::errno(&display, error))?;
        let mut file = std::fs::File::from(file);
        file.write_all(contents)
            .map_err(|error| PathError::io(&display, error))?;
        rustix::fs::fchmod(file.as_fd(), mode)
            .map_err(|error| PathError::errno(&display, error))?;
        file.sync_all()
            .map_err(|error| PathError::io(&display, error))
    }

    fn temporary_name(&self, name: &str) -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        format!(".{name}.zup-tmp-{}-{sequence:x}", std::process::id())
    }
}

fn remove_child_os(
    parent: BorrowedFd<'_>,
    parent_path: &Path,
    name: &OsStr,
) -> Result<(), PathError> {
    let display = parent_path.join(name);
    let stat = statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)
        .map_err(|error| PathError::errno(&display, error))?;
    if EntryKind::from_raw_mode(stat.st_mode) == EntryKind::Directory {
        let child = rustix::fs::openat(parent, name, DIR_FLAGS, Mode::empty())
            .map_err(|error| PathError::errno(&display, error))?;
        let entries =
            rustix::fs::Dir::new(child).map_err(|error| PathError::errno(&display, error))?;
        let mut names = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|error| PathError::errno(&display, error))?;
            let nested = entry.file_name().to_bytes().to_vec();
            if nested == b"." || nested == b".." {
                continue;
            }
            names.push(nested);
        }
        for nested in &names {
            let child_borrowed = rustix::fs::openat(parent, name, DIR_FLAGS, Mode::empty())
                .map_err(|error| PathError::errno(&display, error))?;
            remove_child_os(
                child_borrowed.as_fd(),
                &display,
                OsStr::from_bytes(nested.as_slice()),
            )?;
        }
        rustix::fs::unlinkat(parent, name, AtFlags::REMOVEDIR)
            .map_err(|error| PathError::errno(&display, error))
    } else {
        rustix::fs::unlinkat(parent, name, AtFlags::empty())
            .map_err(|error| PathError::errno(&display, error))
    }
}

fn walk(
    path: &Path,
    mode: Mode,
    create_missing: bool,
) -> Result<(OwnedDirectory, Vec<String>), PathError> {
    if !path.is_absolute() {
        return Err(PathError::Refused {
            path: path.display().to_string(),
            reason: "a trusted directory is opened by absolute path".to_owned(),
        });
    }
    let mut names = Vec::new();
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::CurDir => {}
            Component::Prefix(_) => {
                return Err(PathError::Refused {
                    path: path.display().to_string(),
                    reason: "a trusted directory names no prefix".to_owned(),
                });
            }
            Component::ParentDir => {
                return Err(PathError::Refused {
                    path: path.display().to_string(),
                    reason: "a trusted directory names no `..`".to_owned(),
                });
            }
            Component::Normal(name) => {
                let Some(text) = name.to_str() else {
                    return Err(PathError::InvalidComponent {
                        component: path.display().to_string(),
                        reason: "a trusted directory names text".to_owned(),
                    });
                };
                check_child_name(text)?;
                names.push(text.to_owned());
            }
        }
    }
    let root = rustix::fs::open("/", DIR_FLAGS, Mode::empty())
        .map_err(|error| PathError::errno(Path::new("/"), error))?;
    let mut current = std::fs::File::from(root);
    let mut created = Vec::new();
    let mut built = PathBuf::from("/");
    for name in &names {
        built.push(name);
        match rustix::fs::openat(current.as_fd(), name.as_str(), DIR_FLAGS, Mode::empty()) {
            Ok(next) => {
                current = std::fs::File::from(next);
            }
            Err(rustix::io::Errno::NOENT) if create_missing => {
                rustix::fs::mkdirat(current.as_fd(), name.as_str(), mode)
                    .map_err(|error| PathError::errno(&built, error))?;
                let next =
                    rustix::fs::openat(current.as_fd(), name.as_str(), DIR_FLAGS, Mode::empty())
                        .map_err(|error| PathError::errno(&built, error))?;
                rustix::fs::fchmod(next.as_fd(), mode)
                    .map_err(|error| PathError::errno(&built, error))?;
                rustix::fs::fsync(current.as_fd())
                    .map_err(|error| PathError::errno(&built, error))?;
                current = std::fs::File::from(next);
                created.push(built.display().to_string());
            }
            Err(error) => return Err(traverse_refused(&built, error)),
        }
    }
    if !created.is_empty() {
        rustix::fs::fsync(current.as_fd()).map_err(|error| PathError::errno(path, error))?;
    }
    Ok((
        OwnedDirectory {
            path: path.to_path_buf(),
            file: current,
        },
        created,
    ))
}

fn traverse_refused(path: &Path, error: rustix::io::Errno) -> PathError {
    if error == rustix::io::Errno::LOOP {
        PathError::UnexpectedKind {
            path: path.display().to_string(),
            expected: "a real directory, not a symbolic link",
        }
    } else if error == rustix::io::Errno::NOTDIR {
        PathError::UnexpectedKind {
            path: path.display().to_string(),
            expected: "a directory",
        }
    } else {
        PathError::errno(path, error)
    }
}

pub fn create_parents(destination: &Path, mode: Mode) -> Result<Vec<String>, PathError> {
    let Some(parent) = destination
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    else {
        return Err(PathError::Refused {
            path: destination.display().to_string(),
            reason: "a destination has a parent directory".to_owned(),
        });
    };
    let (_, created) = walk(parent, mode, true)?;
    sync_directory(parent)?;
    Ok(created)
}

pub fn open_parent(destination: &Path) -> Result<(OwnedDirectory, String), PathError> {
    let refused = |reason: &str| PathError::Refused {
        path: destination.display().to_string(),
        reason: reason.to_owned(),
    };
    let Some(parent) = destination
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    else {
        return Err(refused("a destination has a parent directory"));
    };
    let Some(name) = destination
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
    else {
        return Err(refused("a destination names a text final component"));
    };
    check_child_name(name)?;
    Ok((OwnedDirectory::open(parent)?, name.to_owned()))
}

pub fn sync_directory(path: &Path) -> Result<(), PathError> {
    OwnedDirectory::open(path)?.sync()
}

pub fn anchor_reachable(path: &Path) -> Result<(), PathError> {
    if !path.is_absolute() {
        return Err(PathError::Refused {
            path: path.display().to_string(),
            reason: "a trusted path is absolute".to_owned(),
        });
    }
    let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    else {
        return Ok(());
    };
    let Some(leaf) = path
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
    else {
        return Err(PathError::Refused {
            path: path.display().to_string(),
            reason: "a trusted path names a text final component".to_owned(),
        });
    };
    check_child_name(leaf)?;
    let directory = match OwnedDirectory::open(parent) {
        Ok(directory) => directory,
        Err(PathError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    match directory.kind_or_absent(leaf)? {
        None => Ok(()),
        Some(EntryKind::Symlink) => Err(PathError::UnexpectedKind {
            path: path.display().to_string(),
            expected: "a real path, not a symbolic link",
        }),
        Some(EntryKind::Directory) => {
            OwnedDirectory::open(path)?;
            Ok(())
        }
        Some(_) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn a_durable_write_replaces_a_link_without_following_it() {
        let root = directory();
        let owned = OwnedDirectory::create(root.path(), STATE_DIRECTORY_MODE).expect("create");
        let outside = directory();
        let victim = outside.path().join("victim");
        std::fs::write(&victim, b"someone else's file").expect("write");
        std::os::unix::fs::symlink(&victim, owned.path().join("tool")).expect("symlink");

        owned
            .write_durable("tool", b"the payload", STATE_FILE_MODE)
            .expect("replace the link itself");
        assert_eq!(
            std::fs::read(&victim).expect("read"),
            b"someone else's file",
            "the file outside the install directory is untouched"
        );
        assert_eq!(owned.read_regular("tool").expect("read"), b"the payload");
    }

    #[test]
    fn opening_through_a_replaced_parent_stays_pinned() {
        let root = directory();
        let elsewhere = directory();
        let install = root.path().join("install");
        std::fs::create_dir_all(&install).expect("install");
        std::fs::write(install.join("kept"), b"kept").expect("write");

        let pinned = OwnedDirectory::open(&install).expect("pin before the swap");
        std::fs::rename(&install, root.path().join("install-old")).expect("swap away");
        std::os::unix::fs::symlink(elsewhere.path(), &install).expect("symlink");

        assert_eq!(pinned.read_regular("kept").expect("read"), b"kept");
        assert!(matches!(
            pinned.kind_or_absent("kept"),
            Ok(Some(EntryKind::Regular))
        ));
        assert!(
            std::fs::read(install.join("kept")).is_err(),
            "the pathname now resolves elsewhere, but the pinned handle did not follow"
        );
    }

    #[test]
    fn opening_through_a_symlink_parent_is_refused() {
        let root = directory();
        let elsewhere = directory();
        let install = root.path().join("install");
        std::os::unix::fs::symlink(elsewhere.path(), &install).expect("symlink");
        assert!(OwnedDirectory::open(&install.join("nested")).is_err());
        assert!(OwnedDirectory::open(&install).is_err());
    }

    #[test]
    fn child_names_cannot_escape() {
        let root = directory();
        let owned = OwnedDirectory::create(root.path(), STATE_DIRECTORY_MODE).expect("create");
        for hostile in ["", ".", "..", "a/b", "a\0b"] {
            assert!(
                owned.read_regular(hostile).is_err(),
                "{hostile:?} is refused"
            );
            assert!(
                owned.remove_file(hostile).is_err(),
                "{hostile:?} is refused"
            );
        }
    }

    use rstest::rstest;

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
    fn a_temporary_write_sets_the_mode_exactly() {
        let root = directory();
        let owned = OwnedDirectory::create(root.path(), STATE_DIRECTORY_MODE).expect("create");
        owned
            .write_durable("tool", b"#!/bin/sh\n", EXECUTABLE_PAYLOAD_MODE)
            .expect("write");
        assert_eq!(mode_of(&owned.path().join("tool")), 0o744);
        owned
            .write_durable("tool", b"plain\n", PAYLOAD_FILE_MODE)
            .expect("overwrite");
        assert_eq!(mode_of(&owned.path().join("tool")), 0o644);
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
            .write_durable("payload", b"zup's", STATE_FILE_MODE)
            .expect("payload");
        install
            .write_durable("user-notes.txt", b"the user's", STATE_FILE_MODE)
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
        owned
            .write_durable("real", b"real", STATE_FILE_MODE)
            .expect("write");

        owned.remove_contents().expect("remove");

        assert!(
            victim.exists(),
            "the linked-to file is outside the tree and survives"
        );
        assert!(
            owned.read_regular("real").is_err(),
            "the tree's own files are gone"
        );
        assert!(
            owned.kind_or_absent("scratch").expect("stat").is_none(),
            "the link itself is gone"
        );
    }

    #[test]
    fn removing_a_child_tree_does_not_escape() {
        let root = directory();
        let owned = OwnedDirectory::create(root.path(), STATE_DIRECTORY_MODE).expect("create");
        let outside = directory();
        let victim = outside.path().join("precious");
        std::fs::write(&victim, b"not part of the work tree").expect("write");
        let nested = owned.path().join("nested");
        std::fs::create_dir_all(&nested).expect("nested");
        std::os::unix::fs::symlink(&victim, nested.join("link")).expect("symlink");
        std::fs::write(nested.join("file"), b"file").expect("write");

        owned.remove_child_tree("nested").expect("remove");

        assert!(victim.exists(), "the linked-to file survives");
        assert!(owned.kind_or_absent("nested").expect("stat").is_none());
    }
}
