//! Journal filesystem semantics for `fs_transaction`.
//!
//! # What durability the journal claims
//!
//! The journal is the file that decides whether an installation is the old state
//! or the new one. It gets the strongest guarantee that is available portably,
//! and the residue is named rather than glossed over:
//!
//! - **Atomic namespace transition.** Every write is a temp file plus a rename,
//!   so a reader sees the whole record or the previous one, never half.
//! - **Process-crash durability.** True by construction: a killed process leaves
//!   either the temp file or the renamed one.
//! - **Power-loss durability of the record's contents.** The temp file is flushed
//!   before it is renamed, so the bytes are on the medium before anything points
//!   at them.
//! - **Not claimed: a flush of the directory entry.** Windows cannot open a
//!   directory for `FlushFileBuffers`, so the rename itself is as durable as the
//!   filesystem's own journal makes it. `zup-windows` gets
//!   `MoveFileExW(MOVEFILE_WRITE_THROUGH)` for the formats that need the stronger
//!   claim; a portable crate cannot, and pretending otherwise would be a claim
//!   nobody could check.
//!
//! An earlier version of this file skipped the file flush as well, on the grounds
//! that crash atomicity came from the rename. That is true of a *process* crash
//! and false of a power cut, and the journal is exactly the file whose contents
//! decide which of two states a machine is in.

use std::future::Future;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use fs_transaction::fs::{
    Capabilities, DirEntry, Durability, Metadata, ReadStorage, StdFs, Storage,
};

/// The suffix of the temp file a journal write publishes through.
const TEMP_SUFFIX: &str = "fstx-tmp";

/// Write `contents` to a new file at `path` and flush it.
///
/// Create-only and flushed, in that order, because a flush of a file somebody
/// else can already see is a flush of a file that may already be replaced.
fn write_flushed(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(contents)?;
    file.sync_all()
}

/// The temp file a journal write publishes through.
///
/// A process-unique name rather than a fixed one, so two writers in one
/// process — the coordinator and a recovery pass, which the store deliberately
/// allows to overlap — cannot collide on a temp path and have one of them
/// publish the other's bytes.
fn temp_path(path: &Path) -> PathBuf {
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let sequence = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    path.with_file_name(format!(
        ".{}.{pid:x}-{sequence:x}.{TEMP_SUFFIX}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("journal"),
        pid = std::process::id(),
    ))
}

/// Filesystem behavior required by the durable journal.
#[derive(Debug, Default, Clone, Copy)]
pub struct JournalFs;

impl ReadStorage for JournalFs {
    fn read(&self, path: &Path) -> impl Future<Output = std::io::Result<Vec<u8>>> {
        StdFs.read(path)
    }

    fn read_to_string(&self, path: &Path) -> impl Future<Output = std::io::Result<String>> {
        StdFs.read_to_string(path)
    }

    fn read_dir(&self, path: &Path) -> impl Future<Output = std::io::Result<Vec<DirEntry>>> {
        StdFs.read_dir(path)
    }

    fn metadata(&self, path: &Path) -> impl Future<Output = std::io::Result<Metadata>> {
        StdFs.metadata(path)
    }
}

impl Storage for JournalFs {
    fn write(&self, path: &Path, contents: &[u8]) -> impl Future<Output = std::io::Result<()>> {
        StdFs.write(path, contents)
    }

    fn create_new(
        &self,
        path: &Path,
        contents: &[u8],
    ) -> impl Future<Output = std::io::Result<()>> {
        StdFs.create_new(path, contents)
    }

    fn create_dir_all(&self, path: &Path) -> impl Future<Output = std::io::Result<()>> {
        StdFs.create_dir_all(path)
    }

    fn remove_file(&self, path: &Path) -> impl Future<Output = std::io::Result<()>> {
        StdFs.remove_file(path)
    }

    fn remove_dir_all(&self, path: &Path) -> impl Future<Output = std::io::Result<()>> {
        StdFs.remove_dir_all(path)
    }

    fn rename(&self, from: &Path, to: &Path) -> impl Future<Output = std::io::Result<()>> {
        StdFs.rename(from, to)
    }

    fn copy_permissions(
        &self,
        from: &Path,
        to: &Path,
    ) -> impl Future<Output = std::io::Result<()>> {
        StdFs.copy_permissions(from, to)
    }

    fn set_executable(
        &self,
        path: &Path,
        executable: bool,
    ) -> impl Future<Output = std::io::Result<()>> {
        StdFs.set_executable(path, executable)
    }

    fn set_link(&self, path: &Path, target: &Path) -> impl Future<Output = std::io::Result<()>> {
        StdFs.set_link(path, target)
    }

    fn capabilities(&self) -> Capabilities {
        StdFs.capabilities()
    }

    fn sync(&self, path: &Path, need: Durability) -> impl Future<Output = std::io::Result<()>> {
        // The only thing that needs syncing here is the record's own bytes, and
        // `replace` flushes them before publishing. There is no directory to
        // flush: `StdFs::sync` opens its argument and calls `sync_all`, which
        // fails with Access denied on Windows for a directory and, before that,
        // for any path opened read-only.
        let _ = (path, need);
        async move { Ok(()) }
    }

    fn replace(&self, path: &Path, contents: &[u8]) -> impl Future<Output = std::io::Result<()>> {
        let path = path.to_path_buf();
        let contents = contents.to_vec();
        async move {
            // Write temp, flush it, then rename. The flush is the step that makes
            // a power cut during a transaction recoverable: without it the rename
            // can be durable while the bytes it points at are not, and the machine
            // comes back to a journal that describes a mutation it has no record
            // of having applied.
            let tmp = temp_path(&path);
            write_flushed(&tmp, &contents)?;
            match StdFs.rename(&tmp, &path).await {
                Ok(()) => Ok(()),
                Err(error) => {
                    let _ = StdFs.remove_file(&tmp).await;
                    Err(error)
                }
            }
        }
    }

    fn write_atomic(
        &self,
        path: &Path,
        contents: &[u8],
    ) -> impl Future<Output = std::io::Result<()>> {
        self.replace(path, contents)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block_on<T>(future: impl Future<Output = T>) -> T {
        fs_transaction::exec::block_on(future)
    }

    #[test]
    fn a_journal_write_publishes_whole_and_leaves_no_temp_file() {
        let directory = tempfile::TempDir::new().expect("temp dir");
        let path = directory.path().join("transaction.json");
        let journal = JournalFs;
        block_on(journal.replace(&path, b"{\"schema\":1}")).expect("first write");
        assert_eq!(
            block_on(JournalFs.read(&path)).expect("read"),
            b"{\"schema\":1}"
        );
        // Overwriting, which is every journal update after the first.
        block_on(journal.replace(&path, b"{\"schema\":1,\"revision\":1}")).expect("second write");
        assert_eq!(
            block_on(JournalFs.read(&path)).expect("read"),
            b"{\"schema\":1,\"revision\":1}"
        );
        let residue: Vec<String> = std::fs::read_dir(directory.path())
            .expect("list")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(TEMP_SUFFIX))
            .collect();
        assert!(residue.is_empty(), "{residue:?}");
    }

    #[test]
    fn a_journal_write_that_cannot_start_is_a_refusal_and_leaves_nothing() {
        // A path whose directory does not exist: the write cannot begin, so the
        // failure has to be the refusal, not a half-written file a later run
        // would treat as a journal.
        let directory = tempfile::TempDir::new().expect("temp dir");
        let path = directory.path().join("absent").join("transaction.json");
        let error = block_on(JournalFs.replace(&path, b"{}")).expect_err("no directory");
        assert!(!path.exists(), "{error}");
    }
}
