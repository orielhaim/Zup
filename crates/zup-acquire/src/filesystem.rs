//! Host filesystem seam for the verified content cache.
//!
//! The cache must publish atomically and must refuse to traverse links. How
//! strongly a host can honour that is a property of the platform, so the cache
//! takes an injected [`CacheFileSystem`] rather than assuming one.
//!
//! [`PortableCacheFileSystem`] is the default for development, tests, and
//! non-Windows hosts. Production Windows hosts inject an adapter that publishes
//! through `MoveFileExW` and rejects reparse points.

use std::path::Path;

/// The filesystem guarantees the content cache depends on.
///
/// Neither operation may follow a link. The cache writes below a root it has
/// already cleared of links, so a link met at any stage means the read or the
/// write would leave the root.
pub trait CacheFileSystem: Send + Sync {
    /// Publish `from` at `to`, replacing any existing entry, and return only
    /// once the destination content survives a crash.
    fn publish_replace(&self, from: &Path, to: &Path) -> Result<(), std::io::Error>;

    /// Report whether `path` is a link - a symlink, a reparse point, or the
    /// local equivalent - that the cache must not write through or read across.
    ///
    /// A path that does not exist is not a link.
    fn is_link(&self, path: &Path) -> Result<bool, std::io::Error>;
}

/// `std::fs`-backed adapter: rename publication and symlink detection.
///
/// On a platform where `rename` replaces its destination this publishes
/// atomically. Where it does not - Windows - the existing entry is removed
/// first, which is a window a crash can be caught in; a production Windows
/// host injects the durable adapter instead. A cache that is caught in that
/// window simply has one missing blob, which the next acquisition re-fetches.
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
            Ok(metadata) => Ok(metadata.file_type().is_symlink()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }
}
