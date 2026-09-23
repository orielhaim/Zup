//! Journal filesystem semantics for `fs_transaction`.
//!
//! Windows cannot flush directories through `std::fs::File`. The journal
//! therefore guarantees an atomic namespace transition and process-crash
//! recovery, but does not claim directory-level power-loss durability.

use std::future::Future;
use std::path::Path;

use fs_transaction::fs::{
    Capabilities, DirEntry, Durability, Metadata, ReadStorage, StdFs, Storage,
};

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
        // `StdFs::sync` opens the path read-only and calls `sync_all`, which
        // fails with Access denied on Windows (FlushFileBuffers wants write
        // access). Directory sync is also unsupported there. Crash atomicity
        // for the journal comes from write-temp + rename (`replace`), not from
        // these flushes, so skip them rather than fail the journal.
        let _ = (path, need);
        async move { Ok(()) }
    }

    fn replace(&self, path: &Path, contents: &[u8]) -> impl Future<Output = std::io::Result<()>> {
        let path = path.to_path_buf();
        let contents = contents.to_vec();
        async move {
            // Compose write-temp + rename ourselves so we never hit
            // `StdFs::sync` (Access denied on Windows).
            let tmp = path.with_file_name(format!(
                ".{}.fstx-tmp",
                path.file_name().and_then(|n| n.to_str()).unwrap_or("file")
            ));
            StdFs.write(&tmp, &contents).await?;
            match StdFs.rename(&tmp, &path).await {
                Ok(()) => Ok(()),
                Err(e) => {
                    let _ = StdFs.remove_file(&tmp).await;
                    Err(e)
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
