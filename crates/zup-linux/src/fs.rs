use std::ffi::OsStr;
use std::io::{Read, Write};
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

use crate::error::PathError;
use rustix::fs::{AtFlags, Mode, OFlags, RenameFlags, renameat_with, statat};
use zup_core::Sha256Digest;

pub const STATE_DIRECTORY_MODE: Mode = Mode::from_bits_truncate(0o700);

pub const STATE_FILE_MODE: Mode = Mode::from_bits_truncate(0o600);

pub const PAYLOAD_FILE_MODE: Mode = Mode::from_bits_truncate(0o644);

pub const EXECUTABLE_PAYLOAD_MODE: Mode = Mode::from_bits_truncate(0o744);

pub const USER_INSTALL_DIRECTORY_MODE: Mode = Mode::from_bits_truncate(0o700);

pub const MACHINE_INSTALL_DIRECTORY_MODE: Mode = Mode::from_bits_truncate(0o755);

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

fn exchange_unsupported(error: rustix::io::Errno) -> bool {
    matches!(
        error,
        rustix::io::Errno::NOSYS
            | rustix::io::Errno::INVAL
            | rustix::io::Errno::OPNOTSUPP
            | rustix::io::Errno::XDEV
    )
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChildStat {
    pub kind: EntryKind,
    pub mode: u32,
    pub dev: u64,
    pub ino: u64,
    pub uid: u32,
}

pub struct StreamedRegular {
    pub file: std::fs::File,
    pub size: u64,
    pub sha256: Sha256Digest,
    pub mode: u32,
    pub dev: u64,
    pub ino: u64,
    pub uid: u32,
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

    pub fn stat_child(&self, name: &str) -> Result<ChildStat, PathError> {
        check_child_name(name)?;
        let display = self.display_child(name);
        let stat = statat(self.as_fd(), name, AtFlags::SYMLINK_NOFOLLOW)
            .map_err(|error| PathError::errno(&display, error))?;
        Ok(ChildStat {
            kind: EntryKind::from_raw_mode(stat.st_mode),
            mode: stat.st_mode & 0o7777,
            dev: stat.st_dev,
            ino: stat.st_ino,
            uid: stat.st_uid,
        })
    }

    pub fn stat_self(&self) -> Result<ChildStat, PathError> {
        let stat =
            rustix::fs::fstat(self.as_fd()).map_err(|error| PathError::errno(&self.path, error))?;
        Ok(ChildStat {
            kind: EntryKind::from_raw_mode(stat.st_mode),
            mode: stat.st_mode & 0o7777,
            dev: stat.st_dev,
            ino: stat.st_ino,
            uid: stat.st_uid,
        })
    }

    pub fn chmod_self(&self, mode: Mode) -> Result<(), PathError> {
        rustix::fs::fchmod(self.as_fd(), mode).map_err(|error| PathError::errno(&self.path, error))
    }

    pub fn chmod_child(&self, name: &str, mode: Mode) -> Result<(), PathError> {
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
        if EntryKind::from_raw_mode(stat.st_mode) == EntryKind::Symlink {
            return Err(PathError::UnexpectedKind {
                path: display.display().to_string(),
                expected: "a real file or directory, not a symbolic link",
            });
        }
        rustix::fs::fchmod(file.as_fd(), mode).map_err(|error| PathError::errno(&display, error))
    }

    pub fn stream_regular(&self, name: &str) -> Result<StreamedRegular, PathError> {
        check_child_name(name)?;
        let display = self.display_child(name);
        let file = rustix::fs::openat(
            self.as_fd(),
            name,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .map_err(|error| refuse_or_io(&display, error))?;
        let mut file = std::fs::File::from(file);
        let stat =
            rustix::fs::fstat(file.as_fd()).map_err(|error| PathError::errno(&display, error))?;
        if EntryKind::from_raw_mode(stat.st_mode) != EntryKind::Regular {
            return Err(PathError::UnexpectedKind {
                path: display.display().to_string(),
                expected: "regular file",
            });
        }
        let (size, sha256) =
            zup_core::hash_reader(&mut file).map_err(|error| PathError::io(&display, error))?;
        Ok(StreamedRegular {
            file,
            size,
            sha256,
            mode: stat.st_mode & 0o7777,
            dev: stat.st_dev,
            ino: stat.st_ino,
            uid: stat.st_uid,
        })
    }

    pub fn try_exchange(&self, first: &str, second: &str) -> Result<bool, PathError> {
        check_child_name(first)?;
        check_child_name(second)?;
        let display = self.display_child(second);
        match renameat_with(
            self.as_fd(),
            first,
            self.as_fd(),
            second,
            RenameFlags::EXCHANGE,
        ) {
            Ok(()) => Ok(true),
            Err(error) if exchange_unsupported(error) => Ok(false),
            Err(error) => Err(PathError::errno(&display, error)),
        }
    }

    pub fn rename_noreplace(&self, from: &str, to: &str) -> Result<(), PathError> {
        check_child_name(from)?;
        check_child_name(to)?;
        match renameat_with(self.as_fd(), from, self.as_fd(), to, RenameFlags::NOREPLACE) {
            Ok(()) => self.sync(),
            Err(rustix::io::Errno::EXIST) => Err(PathError::AlreadyExists {
                path: self.display_child(to).display().to_string(),
            }),
            Err(error) => Err(PathError::errno(&self.display_child(to), error)),
        }
    }

    pub fn stage_stream(
        &self,
        temporary: &str,
        reader: &mut dyn Read,
        limit: u64,
        mode: Mode,
    ) -> Result<(u64, Sha256Digest), PathError> {
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
        let outcome = copy_bounded(reader, &mut file, limit, &display);
        let (size, sha256) = match outcome {
            Ok(identity) => identity,
            Err(error) => {
                let _ = self.remove_file(temporary);
                return Err(error);
            }
        };
        if let Err(error) = rustix::fs::fchmod(file.as_fd(), mode)
            .map_err(|error| PathError::errno(&display, error))
            .and_then(|()| {
                file.sync_all()
                    .map_err(|error| PathError::io(&display, error))
            })
        {
            let _ = self.remove_file(temporary);
            return Err(error);
        }
        Ok((size, sha256))
    }

    pub fn write_stream_durable(
        &self,
        name: &str,
        reader: &mut dyn Read,
        limit: u64,
        mode: Mode,
    ) -> Result<(u64, Sha256Digest), PathError> {
        check_child_name(name)?;
        let temporary = self.temporary_name(name);
        let (size, sha256) = match self.stage_stream(&temporary, reader, limit, mode) {
            Ok(identity) => identity,
            Err(error) => {
                let _ = self.remove_file(&temporary);
                return Err(error);
            }
        };
        if let Err(error) =
            rustix::fs::renameat(self.as_fd(), temporary.as_str(), self.as_fd(), name)
                .map_err(|error| PathError::errno(&self.display_child(name), error))
        {
            let _ = self.remove_file(&temporary);
            return Err(error);
        }
        self.sync()?;
        Ok((size, sha256))
    }

    pub fn open_child_verified(&self, name: &str) -> Result<(Self, u64, u64), PathError> {
        check_child_name(name)?;
        let display = self.display_child(name);
        let before = statat(self.as_fd(), name, AtFlags::SYMLINK_NOFOLLOW)
            .map_err(|error| PathError::errno(&display, error))?;
        if EntryKind::from_raw_mode(before.st_mode) != EntryKind::Directory {
            return Err(PathError::UnexpectedKind {
                path: display.display().to_string(),
                expected: "a directory",
            });
        }
        let file = rustix::fs::openat(self.as_fd(), name, DIR_FLAGS, Mode::empty())
            .map_err(|error| PathError::errno(&display, error))?;
        let after =
            rustix::fs::fstat(file.as_fd()).map_err(|error| PathError::errno(&display, error))?;
        if after.st_dev != before.st_dev || after.st_ino != before.st_ino {
            return Err(PathError::Refused {
                path: display.display().to_string(),
                reason: "the directory was replaced while it was opened".to_owned(),
            });
        }
        Ok((
            Self {
                path: display,
                file: std::fs::File::from(file),
            },
            before.st_dev,
            before.st_ino,
        ))
    }

    pub fn verify_descendants(&self, relative: &Path, expected_uid: u32) -> Result<(), PathError> {
        if relative.is_absolute() {
            return Err(PathError::Refused {
                path: relative.display().to_string(),
                reason: "a descendant walk names a relative path".to_owned(),
            });
        }
        let mut names = Vec::new();
        for component in relative.components() {
            match component {
                Component::CurDir => {}
                Component::Prefix(_) => {
                    return Err(PathError::Refused {
                        path: relative.display().to_string(),
                        reason: "a descendant walk names no prefix".to_owned(),
                    });
                }
                Component::RootDir | Component::ParentDir => {
                    return Err(PathError::Refused {
                        path: relative.display().to_string(),
                        reason: "a descendant walk stays below its base".to_owned(),
                    });
                }
                Component::Normal(name) => {
                    let Some(text) = name.to_str() else {
                        return Err(PathError::InvalidComponent {
                            component: relative.display().to_string(),
                            reason: "a descendant walk names text".to_owned(),
                        });
                    };
                    check_child_name(text)?;
                    names.push(text.to_owned());
                }
            }
        }
        check_trusted_self(self.as_fd(), &self.path, expected_uid)?;
        let duplicate = rustix::fs::openat(self.as_fd(), ".", DIR_FLAGS, Mode::empty())
            .map_err(|error| PathError::errno(&self.path, error))?;
        walk_checked(
            std::fs::File::from(duplicate),
            self.path.clone(),
            &names,
            expected_uid,
        )?;
        Ok(())
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

fn copy_bounded(
    reader: &mut dyn Read,
    writer: &mut std::fs::File,
    limit: u64,
    display: &Path,
) -> Result<(u64, Sha256Digest), PathError> {
    use sha2::Digest as _;
    let mut hasher = sha2::Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    let mut size = 0u64;
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|error| PathError::io(display, error))?;
        if read == 0 {
            break;
        }
        size = size.saturating_add(read as u64);
        if size > limit {
            return Err(PathError::Refused {
                path: display.display().to_string(),
                reason: format!("the stream holds more than the declared {limit} bytes"),
            });
        }
        hasher.update(&buffer[..read]);
        writer
            .write_all(&buffer[..read])
            .map_err(|error| PathError::io(display, error))?;
    }
    Ok((size, Sha256Digest::from_hasher(hasher)))
}

fn remove_child_os(
    parent: BorrowedFd<'_>,
    parent_path: &Path,
    name: &OsStr,
) -> Result<(), PathError> {
    let display = parent_path.join(name);
    let before = statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)
        .map_err(|error| PathError::errno(&display, error))?;
    if EntryKind::from_raw_mode(before.st_mode) != EntryKind::Directory {
        rustix::fs::unlinkat(parent, name, AtFlags::empty())
            .map_err(|error| PathError::errno(&display, error))?;
        return Ok(());
    }
    let child = rustix::fs::openat(parent, name, DIR_FLAGS, Mode::empty())
        .map_err(|error| PathError::errno(&display, error))?;
    let opened =
        rustix::fs::fstat(child.as_fd()).map_err(|error| PathError::errno(&display, error))?;
    if opened.st_dev != before.st_dev || opened.st_ino != before.st_ino {
        return Err(PathError::Refused {
            path: display.display().to_string(),
            reason: "the directory was replaced while it was opened".to_owned(),
        });
    }
    let listing = rustix::fs::openat(child.as_fd(), ".", DIR_FLAGS, Mode::empty())
        .map_err(|error| PathError::errno(&display, error))?;
    let entries =
        rustix::fs::Dir::new(listing).map_err(|error| PathError::errno(&display, error))?;
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
        remove_child_os(
            child.as_fd(),
            &display,
            OsStr::from_bytes(nested.as_slice()),
        )?;
    }
    let after = statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)
        .map_err(|error| PathError::errno(&display, error))?;
    if after.st_dev != before.st_dev || after.st_ino != before.st_ino {
        return Err(PathError::Refused {
            path: display.display().to_string(),
            reason: "the directory was replaced during removal".to_owned(),
        });
    }
    rustix::fs::unlinkat(parent, name, AtFlags::REMOVEDIR)
        .map_err(|error| PathError::errno(&display, error))
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

pub fn ensure_parent(
    destination: &Path,
    mode: Mode,
) -> Result<(OwnedDirectory, String, Vec<String>), PathError> {
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
    let (directory, created) = walk(parent, mode, true)?;
    directory.sync()?;
    Ok((directory, name.to_owned(), created))
}

pub fn verify_ancestor_trust(path: &Path, expected_uid: u32) -> Result<(), PathError> {
    let names = ancestry_names(path)?;
    let root = rustix::fs::open("/", DIR_FLAGS, Mode::empty())
        .map_err(|error| PathError::errno(Path::new("/"), error))?;
    let current = std::fs::File::from(root);
    let built = PathBuf::from("/");
    check_trusted_self(current.as_fd(), &built, expected_uid)?;
    walk_checked(current, built, &names, expected_uid)?;
    Ok(())
}

pub fn ancestry_names(path: &Path) -> Result<Vec<String>, PathError> {
    let refused = |reason: &str| PathError::Refused {
        path: path.display().to_string(),
        reason: reason.to_owned(),
    };
    if !path.is_absolute() {
        return Err(refused("a trusted ancestry starts at an absolute path"));
    }
    let mut names = Vec::new();
    for component in path.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Prefix(_) => return Err(refused("a trusted ancestry names no prefix")),
            Component::ParentDir => {
                return Err(refused("a trusted ancestry names no `..`"));
            }
            Component::Normal(name) => {
                let Some(text) = name.to_str() else {
                    return Err(PathError::InvalidComponent {
                        component: path.display().to_string(),
                        reason: "a trusted ancestry names text".to_owned(),
                    });
                };
                check_child_name(text)?;
                names.push(text.to_owned());
            }
        }
    }
    Ok(names)
}

fn walk_checked(
    mut current: std::fs::File,
    mut built: PathBuf,
    names: &[String],
    expected_uid: u32,
) -> Result<std::fs::File, PathError> {
    use std::os::fd::AsFd as _;
    for name in names {
        built.push(name);
        match rustix::fs::openat(current.as_fd(), name.as_str(), DIR_FLAGS, Mode::empty()) {
            Ok(next) => {
                check_trusted_self(next.as_fd(), &built, expected_uid)?;
                current = std::fs::File::from(next);
            }
            Err(rustix::io::Errno::NOENT) => return Ok(current),
            Err(error) => return Err(traverse_refused(&built, error)),
        }
    }
    Ok(current)
}

fn check_trusted_self(
    directory: BorrowedFd<'_>,
    display: &Path,
    expected_uid: u32,
) -> Result<(), PathError> {
    let stat = rustix::fs::fstat(directory).map_err(|error| PathError::errno(display, error))?;
    if rustix::fs::FileType::from_raw_mode(stat.st_mode) != rustix::fs::FileType::Directory {
        return Err(PathError::UnexpectedKind {
            path: display.display().to_string(),
            expected: "a directory",
        });
    }
    if stat.st_uid != expected_uid && stat.st_uid != 0 {
        return Err(PathError::Refused {
            path: display.display().to_string(),
            reason: format!(
                "a privileged ancestry is owned by uid {}, not {expected_uid} or root",
                stat.st_uid
            ),
        });
    }
    if stat.st_mode & 0o022 != 0 && stat.st_mode & 0o1000 == 0 {
        return Err(PathError::Refused {
            path: display.display().to_string(),
            reason: "a privileged ancestry is writable by group or other without a sticky bit"
                .to_owned(),
        });
    }
    Ok(())
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

    #[test]
    fn exchange_capability_failures_stay_distinct_from_real_failures() {
        use rustix::io::Errno;
        for unsupported in [Errno::NOSYS, Errno::INVAL, Errno::OPNOTSUPP, Errno::XDEV] {
            assert!(
                exchange_unsupported(unsupported),
                "{unsupported:?} means no atomic exchange"
            );
        }
        for genuine in [
            Errno::PERM,
            Errno::ACCESS,
            Errno::IO,
            Errno::EXIST,
            Errno::NOENT,
            Errno::NOTDIR,
        ] {
            assert!(
                !exchange_unsupported(genuine),
                "{genuine:?} is a real failure, not a missing capability"
            );
        }
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
    fn a_stream_past_its_declared_size_is_refused_without_writing_it() {
        let root = directory();
        let owned = OwnedDirectory::create(root.path(), STATE_DIRECTORY_MODE).expect("create");
        let mut reader = std::io::Cursor::new(vec![0xA5u8; 1024]);
        let error = owned
            .stage_stream("big.bin.zup-tmp", &mut reader, 16, STATE_FILE_MODE)
            .expect_err("sixteen declared bytes cannot hold a kilobyte");
        assert!(
            matches!(error, PathError::Refused { .. }),
            "the refusal names the dishonest size: {error}"
        );
        assert!(
            owned
                .kind_or_absent("big.bin.zup-tmp")
                .expect("stat")
                .is_none()
                && std::fs::read_dir(root.path()).expect("read dir").count() == 0,
            "the aborted stream leaves no temporary behind"
        );
    }

    #[test]
    fn a_stream_at_exactly_its_declared_size_is_kept() {
        let root = directory();
        let owned = OwnedDirectory::create(root.path(), STATE_DIRECTORY_MODE).expect("create");
        let bytes = vec![0xA5u8; 1024];
        let mut reader = std::io::Cursor::new(bytes.clone());
        let (size, sha256) = owned
            .stage_stream("exact.bin.zup-tmp", &mut reader, 1024, STATE_FILE_MODE)
            .expect("an exact stream stages");
        assert_eq!(size, 1024);
        assert_eq!(sha256, zup_core::hash_bytes(&bytes));
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

    #[test]
    fn removing_a_tree_keeps_a_swapped_sibling_whole() {
        use std::sync::{Arc, Barrier};

        for _ in 0..30 {
            let root = directory();
            let owned = OwnedDirectory::create(root.path(), STATE_DIRECTORY_MODE).expect("create");
            plant_tree(&owned, "first", b"one", true);
            plant_tree(&owned, "second", b"two", false);

            let barrier = Arc::new(Barrier::new(2));
            let swapper_barrier = Arc::clone(&barrier);
            let path = owned.path().to_path_buf();
            let swapper = std::thread::spawn(move || {
                let parent = OwnedDirectory::open(&path).expect("open");
                swapper_barrier.wait();
                for _ in 0..200 {
                    let _ = parent.try_exchange("first", "second");
                }
            });
            barrier.wait();
            let outcome = owned.remove_child_tree("first");
            swapper.join().expect("join");

            match outcome {
                Err(_) => {
                    assert!(owned.kind_or_absent("first").expect("stat").is_some());
                    assert!(owned.kind_or_absent("second").expect("stat").is_some());
                    assert!(
                        is_whole(&owned, "first") || is_whole(&owned, "second"),
                        "a refused removal never touches the tree it did not select"
                    );
                }
                Ok(()) => {
                    let first = owned.kind_or_absent("first").expect("stat");
                    let second = owned.kind_or_absent("second").expect("stat");
                    assert!(
                        first.is_none() != second.is_none(),
                        "exactly one sibling is gone"
                    );
                    let survivor = if first.is_none() { "second" } else { "first" };
                    assert!(
                        is_whole(&owned, survivor) || is_empty(&owned, survivor),
                        "the survivor is a whole tree or a fully removed one"
                    );
                }
            }
        }

        fn plant_tree(parent: &OwnedDirectory, name: &str, sentinel: &[u8], nested: bool) {
            let child = OwnedDirectory::create(&parent.path().join(name), STATE_DIRECTORY_MODE)
                .expect("create");
            child
                .write_durable("sentinel", sentinel, STATE_FILE_MODE)
                .expect("sentinel");
            if nested {
                let deep =
                    OwnedDirectory::create(&child.path().join("nested"), STATE_DIRECTORY_MODE)
                        .expect("nested");
                deep.write_durable("deep", b"deep", STATE_FILE_MODE)
                    .expect("deep");
            }
        }

        fn is_whole(parent: &OwnedDirectory, name: &str) -> bool {
            let Ok(Some(EntryKind::Directory)) = parent.kind_or_absent(name) else {
                return false;
            };
            let Ok(child) = parent.open_child(name) else {
                return false;
            };
            let Ok(sentinel) = child.read_regular("sentinel") else {
                return false;
            };
            if sentinel == b"one" {
                child
                    .open_child("nested")
                    .and_then(|deep| deep.read_regular("deep"))
                    .is_ok_and(|deep| deep == b"deep")
            } else if sentinel == b"two" {
                child
                    .kind_or_absent("nested")
                    .is_ok_and(|absent| absent.is_none())
            } else {
                false
            }
        }

        fn is_empty(parent: &OwnedDirectory, name: &str) -> bool {
            let Ok(Some(EntryKind::Directory)) = parent.kind_or_absent(name) else {
                return false;
            };
            let Ok(child) = parent.open_child(name) else {
                return false;
            };
            child.child_names().is_ok_and(|names| names.is_empty())
        }
    }
}
