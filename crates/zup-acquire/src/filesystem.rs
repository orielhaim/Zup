//! Publication and link checks for the disposable content cache.
//!
//! These path-based checks do not establish filesystem authority. Callers must
//! access untrusted cache paths under the requesting user's permissions.

use std::path::Path;

/// Host operations used during cache publication and validation.
pub trait CacheFileSystem: Send + Sync {
    /// Replace `to` with `from`. Lost cache entries can be acquired again.
    fn publish_replace(&self, from: &Path, to: &Path) -> Result<(), std::io::Error>;

    /// Report whether `path` is a link - a symlink, a reparse point, or the
    /// local equivalent - that the cache must not write through or read across.
    ///
    /// A path that does not exist is not a link.
    fn is_link(&self, path: &Path) -> Result<bool, std::io::Error>;
}

/// Standard filesystem publication; replacement can leave a missing entry on Windows.
#[derive(Debug, Default, Clone, Copy)]
pub struct PortableCacheFileSystem;

impl PortableCacheFileSystem {
    pub fn new() -> Self {
        Self
    }
}

impl CacheFileSystem for PortableCacheFileSystem {
    fn publish_replace(&self, from: &Path, to: &Path) -> Result<(), std::io::Error> {
        match std::fs::rename(from, to) {
            Ok(()) => Ok(()),
            Err(error) => {
                // Windows refuses to rename onto an existing file. The entry we
                // are replacing is content-addressed, so removing it can only
                // ever cost a re-fetch.
                if !to.exists() {
                    return Err(error);
                }
                std::fs::remove_file(to)?;
                std::fs::rename(from, to)
            }
        }
    }

    fn is_link(&self, path: &Path) -> Result<bool, std::io::Error> {
        match std::fs::symlink_metadata(path) {
            Ok(metadata) => {
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt;
                    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
                    Ok(metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0)
                }
                #[cfg(not(windows))]
                {
                    Ok(metadata.file_type().is_symlink())
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }
}
