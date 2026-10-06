//! Durable and safe filesystem primitives for this backend.
//!
//! Everything the Linux executor does to a machine goes through this module, and
//! the reason is that the *guarantee* is the interesting part. Two of them are
//! stated here because `std` cannot state them at all:
//!
//! - **A durable replacement** is: write a temporary sibling, flush its
//!   contents, rename it into place, then flush the directory. `std::fs::rename`
//!   does the middle step and none of the flushing, so a process that reported
//!   success may have published a name whose contents a crash would discard.
//! - **A no-clobber creation** is: ask the kernel to fail the rename if the
//!   destination exists. `if !path.exists() { rename(..) }` is *not* that: it is
//!   two operations with a window between them, and the window is exactly where a
//!   concurrent installer - or anything else on the machine - can put a file
//!   there. `renameat2(RENAME_NOREPLACE)` makes the condition part of the
//!   namespace operation itself.
//!
//! # Directories rather than paths
//!
//! The operations here are directory-relative: a caller opens the directory it
//! owns once and passes a bare name to every call beneath it. That is not
//! tidiness. Resolving `a/b/c` by absolute path on every operation means each
//! component is resolved again, and a component that has been replaced by a
//! symbolic link since the last resolution resolves to wherever it now points.
//! With a held directory descriptor the components above it cannot move out from
//! under the operation.
//!
//! # What is refused
//!
//! A path that is expected to be a regular file or a directory must *be* one.
//! Every entry is read with `O_NOFOLLOW`, so a symbolic link where a file
//! belongs is a refusal rather than a redirect into a tree the caller does not
//! own; and a device node, FIFO or socket where a regular file belongs is a
//! refusal rather than a read that blocks forever or talks to hardware.

use std::io::Write;
use std::os::fd::{AsFd, BorrowedFd};
use std::path::{Path, PathBuf};

use rustix::fs::{AtFlags, Mode, OFlags, RenameFlags, renameat_with, statat};

/// The mode a Zup-owned state directory is created with: `0700`.
///
/// The directory holds the ledger, the transaction journal and the work
/// directory, and none of that is another account's business. Inheriting whatever
/// the process umask happens to produce would make a machine's bookkeeping depend
/// on the login shell that started the installer.
pub const STATE_DIRECTORY_MODE: Mode = Mode::from_bits_truncate(0o700);

/// The mode a Zup-owned private file is created with: `0600`.
///
/// The journal is written by the installer and read by its recovery pass; it is
/// not a shared document.
pub const STATE_FILE_MODE: Mode = Mode::from_bits_truncate(0o600);

/// The mode an installed payload file is created with: `0644`.
///
/// A payload is a file the installed application reads, and the application may
/// legitimately be run by another account. The execute bit is a separate,
/// explicit decision.
pub const PAYLOAD_FILE_MODE: Mode = Mode::from_bits_truncate(0o644);

/// The payload mode with the owner's execute bit set: `0744`.
///
/// Additive on purpose. `0644` plus the execute bit is `0744`, not `0755`: the
/// file becomes runnable by its owner and nothing is made writable that was not
/// already, so declaring one helper executable never loosens the directory it
/// lands in.
pub const EXECUTABLE_PAYLOAD_MODE: Mode = Mode::from_bits_truncate(0o744);

/// Failures from the durable filesystem layer.
#[derive(Debug, thiserror::Error)]
pub enum FileSystemError {
    #[error("`{path}` is not a {expected}")]
    UnexpectedKind {
        path: String,
        expected: &'static str,
    },

    #[error("`{path}` already exists")]
    AlreadyExists { path: String },

    #[error("`{path}` does not exist")]
    Missing { path: String },

    #[error("`{path}`: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

impl FileSystemError {
    fn errno(path: &Path, error: rustix::io::Errno) -> Self {
        Self::Io {
            path: path.display().to_string(),
            source: std::io::Error::from(error),
        }
    }

    fn io(path: &Path, error: std::io::Error) -> Self {
        Self::Io {
            path: path.display().to_string(),
            source: error,
        }
    }

    /// Turn a link-related open failure into the refusal it is, rather than the
    /// generic I/O error `std` reports for `ELOOP`.
    ///
    /// `ELOOP` on an `O_NOFOLLOW` open *means* "the thing you asked for is a
    /// symbolic link and you said not to follow it". Reporting it as an I/O
    /// failure loses the one fact that tells a caller it was refused on purpose.
    fn refuse_or_io(path: &Path, error: rustix::io::Errno) -> Self {
        if error == rustix::io::Errno::LOOP {
            Self::UnexpectedKind {
                path: path.display().to_string(),
                expected: "regular file, not a symbolic link",
            }
        } else {
            Self::errno(path, error)
        }
    }
}

/// What a directory entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    Regular,
    Directory,
    Symlink,
    Special,
}

impl EntryKind {
    /// The kind a raw st_mode names.
    pub fn from_raw_mode(mode: u32) -> Self {
        match rustix::fs::FileType::from_raw_mode(mode) {
            rustix::fs::FileType::RegularFile => Self::Regular,
            rustix::fs::FileType::Directory => Self::Directory,
            rustix::fs::FileType::Symlink => Self::Symlink,
            _ => Self::Special,
        }
    }
}

/// A directory this backend owns, held open for the operations beneath it.
///
/// The descriptor is the point: it is what makes a name beneath this directory
/// resolve relative to *this* directory rather than to whatever a component was
/// replaced with since the last time the path was spelled out.
#[derive(Debug)]
pub struct OwnedDirectory {
    path: PathBuf,
    file: std::fs::File,
}

impl OwnedDirectory {
    /// Open an existing directory, refusing anything that is not one.
    pub fn open(path: &Path) -> Result<Self, FileSystemError> {
        let file = rustix::fs::open(
            path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .map_err(|error| FileSystemError::errno(path, error))?;
        Ok(Self {
            path: path.to_path_buf(),
            file: std::fs::File::from(file),
        })
    }

    /// Create a directory and any missing parent, then open it.
    ///
    /// `mode` is a mode the directory *ends up with*, not merely one it is asked
    /// for: `mkdir`'s argument is masked by the process umask, so a caller
    /// passing `0700` from a shell with `umask 0000` gets `0700` by luck and
    /// `0000` only if the umask happens to agree. The mode is therefore applied
    /// again after creation, which is the step that makes zup's private state
    /// private regardless of who started the installer.
    ///
    /// An *existing* directory is opened but never re-permissioned. Narrowing or
    /// widening a directory whose permissions the user chose is not this
    /// operation's business, and a caller that wants the invariant enforced can
    /// assert it explicitly rather than having creation silently take ownership of
    /// the permissions.
    pub fn create(path: &Path, mode: Mode) -> Result<Self, FileSystemError> {
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
            Err(error) => return Err(FileSystemError::errno(path, error)),
        }
        Self::open(path)
    }

    /// Set this directory's own permissions.
    pub fn chmod(&self, mode: Mode) -> Result<(), FileSystemError> {
        rustix::fs::fchmod(self.file.as_fd(), mode)
            .map_err(|error| FileSystemError::errno(&self.path, error))
    }

    /// This directory's path, as the caller named it.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The directory's own descriptor.
    pub fn as_fd(&self) -> BorrowedFd<'_> {
        self.file.as_fd()
    }

    /// The path of a child entry.
    fn child(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }

    /// Flush this directory's entries.
    ///
    /// A rename is a change to the *directory*, so a directory that is not
    /// flushed can lose a rename it has already reported as done. This is the
    /// step `std` has no equivalent of, and the step that makes a published name
    /// survive a power cut rather than merely appearing to.
    pub fn sync(&self) -> Result<(), FileSystemError> {
        rustix::fs::fsync(self.file.as_fd())
            .map_err(|error| FileSystemError::errno(&self.path, error))
    }

    /// What kind of entry `name` is, without following it.
    pub fn entry_kind(&self, name: &str) -> Result<EntryKind, FileSystemError> {
        let stat = statat(self.as_fd(), name, AtFlags::SYMLINK_NOFOLLOW)
            .map_err(|error| FileSystemError::errno(&self.child(name), error))?;
        Ok(EntryKind::from_raw_mode(stat.st_mode))
    }

    /// What kind of entry `name` is, treating absence as an answer rather than an
    /// error.
    pub fn kind_or_absent(&self, name: &str) -> Result<Option<EntryKind>, FileSystemError> {
        match self.entry_kind(name) {
            Ok(kind) => Ok(Some(kind)),
            Err(FileSystemError::Io { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    /// Write `contents` to `name`, durably replacing whatever is there.
    ///
    /// The sequence is: create a temporary sibling, write it, flush it, rename it
    /// over the destination, flush this directory. The flush before the rename is
    /// what makes the contents durable *before* the name referring to them exists;
    /// the flush after it is what makes the name durable. Reporting success after
    /// only the first two would claim a guarantee the crash test does not honour.
    pub fn write_durable(
        &self,
        name: &str,
        contents: &[u8],
        mode: Mode,
    ) -> Result<(), FileSystemError> {
        let destination = self.child(name);
        let temporary = self.temporary_name(name);
        let temporary_path = self.child(&temporary);

        let written = self.write_temporary(&temporary, contents, mode);
        if let Err(error) = written {
            let _ = self.remove_file(&temporary);
            return Err(error);
        }
        let renamed = rustix::fs::rename(&temporary_path, &destination)
            .map_err(|error| FileSystemError::errno(&destination, error));
        if let Err(error) = renamed {
            let _ = self.remove_file(&temporary);
            return Err(error);
        }
        self.sync()
    }

    /// Write `contents` to `name` only if it is not already there.
    ///
    /// The absence condition is enforced by the kernel at the rename, not by a
    /// check first. A caller that asks for "create only if absent" and is given a
    /// check-then-write has been handed a race, and the window between the two
    /// operations is where a concurrent installer's file gets overwritten.
    pub fn create_durable_exclusive(
        &self,
        name: &str,
        contents: &[u8],
        mode: Mode,
    ) -> Result<(), FileSystemError> {
        let destination = self.child(name);
        let temporary = self.temporary_name(name);
        let temporary_path = self.child(&temporary);

        if let Err(error) = self.write_temporary(&temporary, contents, mode) {
            let _ = self.remove_file(&temporary);
            return Err(error);
        }
        // `RENAME_NOREPLACE` is the whole point: the destination's absence is a
        // condition of the namespace operation, so there is no window in which
        // something else can claim the name between the check and the rename.
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
            Err(rustix::io::Errno::EXIST) => Err(FileSystemError::AlreadyExists {
                path: destination.display().to_string(),
            }),
            Err(error) => Err(FileSystemError::errno(&destination, error)),
        }
    }

    /// Publish an already-written sibling under `name`, only if it is not there.
    ///
    /// The transaction's create step, and the reason it is a primitive rather
    /// than something a caller composes: the destination's absence has to be a
    /// condition of the namespace operation, and the only way to make it one is to
    /// ask the kernel for it. `if !contains(name) { rename(..) }` has a window
    /// between the check and the rename, and a file that appears in that window
    /// is the exact thing a create is supposed to refuse.
    ///
    /// The source is a sibling rather than an arbitrary path so the rename cannot
    /// cross a filesystem, where it would become a copy with none of this
    /// guarantee.
    pub fn publish_exclusive(&self, name: &str, source: &str) -> Result<(), FileSystemError> {
        let destination = self.child(name);
        match renameat_with(
            self.as_fd(),
            self.child(source),
            self.as_fd(),
            &destination,
            RenameFlags::NOREPLACE,
        ) {
            Ok(()) => self.sync(),
            Err(rustix::io::Errno::EXIST) => Err(FileSystemError::AlreadyExists {
                path: destination.display().to_string(),
            }),
            Err(error) => Err(FileSystemError::errno(&destination, error)),
        }
    }

    /// Publish an already-written sibling over `name`, replacing whatever is there.
    pub fn publish_replacing(&self, name: &str, source: &str) -> Result<(), FileSystemError> {
        let destination = self.child(name);
        rustix::fs::rename(self.child(source), &destination)
            .map_err(|error| FileSystemError::errno(&destination, error))?;
        self.sync()
    }

    /// Write a payload file inside this directory.
    ///
    /// No per-file durability claim. A payload file's durability comes from the
    /// flush that publishes the installation generation, not from one `fsync` per
    /// installed file; paying for both would be a cost with no additional
    /// guarantee.
    ///
    /// The mode is set explicitly rather than left to `open`, because `open`
    /// honours its mode argument only when it *creates* the file. An existing file
    /// keeps whatever mode it had, and a file that was executable for a previous
    /// version but is a data file in this one has to lose the bit. The executable
    /// bit is a property of this install, not of whatever was here before.
    pub fn write_payload(
        &self,
        name: &str,
        contents: &[u8],
        executable: bool,
    ) -> Result<(), FileSystemError> {
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
        .map_err(|error| FileSystemError::refuse_or_io(&path, error))?;
        let mut file = std::fs::File::from(file);
        file.write_all(contents)
            .map_err(|error| FileSystemError::io(&path, error))?;
        rustix::fs::fchmod(file.as_fd(), mode).map_err(|error| FileSystemError::errno(&path, error))
    }

    /// Open `name` for reading, refusing anything that is not a regular file.
    ///
    /// `O_NOFOLLOW` is what refuses a symbolic link here: without it the open
    /// would succeed and hand back the link's target, and the caller would read
    /// bytes from a tree it does not own while believing it had verified a file it
    /// owns.
    pub fn open_regular_read(&self, name: &str) -> Result<std::fs::File, FileSystemError> {
        let path = self.child(name);
        let file = rustix::fs::open(
            &path,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .map_err(|error| FileSystemError::refuse_or_io(&path, error))?;
        let stat = rustix::fs::fstat(file.as_fd())
            .map_err(|error| FileSystemError::errno(&path, error))?;
        if EntryKind::from_raw_mode(stat.st_mode) != EntryKind::Regular {
            return Err(FileSystemError::UnexpectedKind {
                path: path.display().to_string(),
                expected: "regular file",
            });
        }
        Ok(std::fs::File::from(file))
    }

    /// Read `name`, refusing anything that is not a regular file.
    pub fn read_regular(&self, name: &str) -> Result<Vec<u8>, FileSystemError> {
        use std::io::Read as _;
        let mut file = self.open_regular_read(name)?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|error| FileSystemError::io(&self.child(name), error))?;
        Ok(bytes)
    }

    /// Remove `name`, whether a file, a link, or anything else that is not a
    /// directory.
    pub fn remove_file(&self, name: &str) -> Result<(), FileSystemError> {
        rustix::fs::unlinkat(self.as_fd(), name, AtFlags::empty())
            .map_err(|error| FileSystemError::errno(&self.child(name), error))
    }

    /// Remove an empty directory, refusing a non-empty one.
    ///
    /// Empty-only on purpose. A recursive delete of an installation directory is
    /// how an uninstall takes a user's own files with it: a directory zup created
    /// is removed when it is empty, and anything left in it belonged to somebody.
    pub fn remove_empty_directory(&self, name: &str) -> Result<(), FileSystemError> {
        rustix::fs::unlinkat(self.as_fd(), name, AtFlags::REMOVEDIR)
            .map_err(|error| FileSystemError::errno(&self.child(name), error))
    }

    /// Whether `name` is a directory with nothing in it.
    ///
    /// "Empty" is a *precondition of a removal*, so it is asked through the same
    /// `O_NOFOLLOW` open every other entry lookup uses: a symlink where a
    /// directory belongs must not be reported as an empty directory.
    pub fn is_empty_directory(&self, name: &str) -> Result<bool, FileSystemError> {
        let file = rustix::fs::open(
            self.child(name),
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .map_err(|error| FileSystemError::errno(&self.child(name), error))?;
        let mut entries = rustix::fs::Dir::new(file)
            .map_err(|error| FileSystemError::errno(&self.child(name), error))?;
        Ok(entries.next().is_none())
    }

    /// Remove every entry this directory holds, then the directory itself.
    ///
    /// Only ever called on a directory zup owns end to end - the transaction work
    /// directory, or a retired maintenance generation - and only where the model
    /// already says the whole tree is zup's. Never on an installation directory,
    /// which holds a user's payload.
    pub fn remove_tree(&self) -> Result<(), FileSystemError> {
        let entries = std::fs::read_dir(&self.path)
            .map_err(|error| FileSystemError::io(&self.path, error))?;
        for entry in entries {
            let entry = entry.map_err(|error| FileSystemError::io(&self.path, error))?;
            let file_type = entry
                .file_type()
                .map_err(|error| FileSystemError::io(&entry.path(), error))?;
            if file_type.is_dir() {
                OwnedDirectory::open(&entry.path())?.remove_tree()?;
            }
            // A directory is removed with `REMOVEDIR` and anything else without
            // it, and a symlink is always the latter: `file_type` reported it as
            // not a directory, so a link into a directory tree is unlinked rather
            // than descended into.
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
            .map_err(|error| FileSystemError::errno(&self.path, error))
    }

    /// Write and flush a temporary sibling, leaving the rename to the caller.
    fn write_temporary(
        &self,
        temporary: &str,
        contents: &[u8],
        mode: Mode,
    ) -> Result<(), FileSystemError> {
        let path = self.child(temporary);
        // `EXCL` rather than a truncation flag: a temporary name that already
        // exists is either a collision or something else on the machine, and
        // overwriting it silently would be the wrong answer to both.
        let file = rustix::fs::open(
            &path,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            mode,
        )
        .map_err(|error| FileSystemError::errno(&path, error))?;
        let mut file = std::fs::File::from(file);
        file.write_all(contents)
            .map_err(|error| FileSystemError::io(&path, error))?;
        // Contents first, then the name. The reverse order can leave a name that
        // refers to contents a crash never wrote.
        file.sync_all()
            .map_err(|error| FileSystemError::io(&path, error))
    }

    /// A temporary name beside `name`, unique within this process.
    ///
    /// The sequence number matters as much as the process id: two publications in
    /// one process must not collide, and `EXCL` means a name left behind by a
    /// crashed process is a refusal rather than a silent overwrite.
    fn temporary_name(&self, name: &str) -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        format!(".{name}.zup-tmp-{}-{sequence:x}", std::process::id())
    }
}

/// Flush a directory by path.
///
/// For the one case where no descriptor is already held - flushing a directory
/// that was just created through a path rather than through [`OwnedDirectory`].
pub fn sync_directory(path: &Path) -> Result<(), FileSystemError> {
    OwnedDirectory::open(path)?.sync()
}

/// Refuse a path whose existing ancestors include a symbolic link.
///
/// Directory-relative operations protect the final components, but the walk
/// up to them still resolves through whatever the ancestors name today. A
/// state root or install directory reached through a link lives wherever the
/// link points, so a hierarchy zup is about to treat as its own is checked
/// first: every prefix that exists must *be* a directory, not name one.
/// Absent prefixes cannot be links, so only what exists is judged.
///
/// This is a pre-operation check, not a lock: it converts a planted redirect
/// into a refusal, while the descriptor-relative operations below remain what
/// enforces safety moment to moment.
pub fn refuse_symlink_ancestors(path: &Path) -> Result<(), FileSystemError> {
    let mut prefix = PathBuf::new();
    for component in path.components() {
        prefix.push(component.as_os_str());
        let metadata = match std::fs::symlink_metadata(&prefix) {
            Ok(metadata) => metadata,
            // Absent means nothing to judge; unreadable for any other reason
            // is reported rather than skipped, because skipping is how a
            // hierarchy that cannot be examined becomes one that is trusted.
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => continue,
            Err(source) => {
                return Err(FileSystemError::io(&prefix, source));
            }
        };
        if metadata.file_type().is_symlink() {
            return Err(FileSystemError::UnexpectedKind {
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

    /// A durable write leaves no temporary behind. A leftover `.zup-tmp-` entry
    /// is a real defect: it accumulates across runs and tells anyone reading the
    /// directory that a publication was interrupted.
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

    /// The no-clobber guarantee is enforced by the kernel at the rename, so a
    /// second exclusive creation of the same name fails even though the caller
    /// never checked. This is the invariant a `path.exists()` guard does not have.
    #[test]
    fn an_exclusive_create_refuses_to_overwrite() {
        let root = directory();
        let owned = OwnedDirectory::create(root.path(), STATE_DIRECTORY_MODE).expect("create");
        owned
            .create_durable_exclusive("lock", b"first", STATE_FILE_MODE)
            .expect("first");
        assert!(matches!(
            owned.create_durable_exclusive("lock", b"second", STATE_FILE_MODE),
            Err(FileSystemError::AlreadyExists { .. })
        ));
        assert_eq!(
            owned.read_regular("lock").expect("read"),
            b"first",
            "the refused creation must not have touched the contents"
        );
    }

    /// The directory mode is the backend's decision, not the umask's.
    ///
    /// `mkdir`'s mode argument is masked by the process umask, so a caller that
    /// only passes `0700` from a shell running `umask 0000` gets `0700` and from
    /// one running `umask 0777` gets `0000`. Neither is the invariant. Applying
    /// the mode after creation is what makes zup's private state private
    /// regardless of which shell started the installer.
    #[test]
    fn a_created_directory_ends_up_with_the_requested_mode_not_the_umasks() {
        let root = directory();
        // Requested `0777` on purpose: under the `022` umask every test process
        // inherits, `mkdir` alone yields `0755`. Getting `0777` back is therefore
        // evidence that the mode was applied after creation and not merely passed
        // to `mkdir`, which is the only way the requested mode is the resulting
        // mode regardless of who started the installer.
        let owned =
            OwnedDirectory::create(&root.path().join("state"), Mode::from_bits_truncate(0o777))
                .expect("create");
        assert_eq!(
            rustix::fs::fstat(owned.as_fd()).expect("stat").st_mode & 0o777,
            0o777,
            "the requested mode is the resulting mode, not the umask's version of it"
        );
    }

    /// The mode zup actually uses for its own state.
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

    /// A symbolic link where a payload file belongs is refused rather than
    /// followed. Following it would write the installer's payload into whatever
    /// tree the link points at, under a name the caller never chose.
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
            Err(FileSystemError::UnexpectedKind { .. })
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
            Err(FileSystemError::UnexpectedKind { .. })
        ));
        Ok(())
    }

    /// Executable intent is a property of the file, applied deliberately rather
    /// than inferred from the bytes. A payload that was executable in a previous
    /// version and is a data file in this one has to lose the bit, which is why
    /// the mode is set rather than only requested at creation.
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

        // The same name reverting to data: the earlier mode must not survive.
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

    /// Removing an installation directory recursively is how an uninstall takes a
    /// user's files with it. `remove_empty_directory` refuses a directory with
    /// anything left in it.
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
            Err(FileSystemError::Io { .. })
        ));
        assert!(
            owned.path().join("install").join("user-notes.txt").exists(),
            "a refused removal leaves the whole tree alone"
        );
    }

    /// A link into a directory tree is unlinked, not descended into. `file_type`
    /// reports a symlink as not a directory, so `remove_tree` must not follow it
    /// into a tree outside the work directory.
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

    /// A work directory that no longer exists is not an error. Cleanup that fails
    /// because it already succeeded is a false alarm, and treating it as one
    /// teaches callers to ignore the errors that matter.
    #[test]
    fn removing_an_absent_tree_is_not_a_failure() {
        let root = directory();
        let owned = OwnedDirectory::create(root.path(), STATE_DIRECTORY_MODE).expect("create");
        let work = OwnedDirectory::create(&owned.path().join("work"), STATE_DIRECTORY_MODE)
            .expect("create");
        work.remove_tree().expect("first removal");
        assert!(matches!(
            work.remove_tree(),
            Err(FileSystemError::Io { .. })
        ));
    }
}
