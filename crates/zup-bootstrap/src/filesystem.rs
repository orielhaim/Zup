//! Host filesystem seam for quarantine acquisition and bootstrap state
//! persistence.
//!
//! Bootstrap must publish artifacts and state atomically and must refuse to
//! traverse links. How strongly a host can honour that is a property of the
//! platform, so quarantine and the state store take an injected
//! [`BootstrapFileSystem`] instead of assuming one.
//!
//! [`PortableBootstrapFileSystem`] is the default for development and
//! non-Windows hosts. Windows production paths inject the `zup-windows`
//! adapter, which publishes through `MoveFileExW` and rejects reparse points.

use std::path::Path;

/// The filesystem guarantees bootstrap persistence depends on.
///
/// Implementations own their host's durability and link policy. Neither
/// operation may follow a link: bootstrap writes below a root it has already
/// cleared of links, so a link encountered at any stage means the write would
/// leave the root.
pub trait BootstrapFileSystem: Send + Sync {
    /// Publish `from` at `to`, replacing any existing entry, and return only
    /// once the destination content survives a crash.
    fn publish_replace(&self, from: &Path, to: &Path) -> Result<(), std::io::Error>;

    /// Report whether `path` is a link — a symlink, a reparse point, or the
    /// local equivalent — that bootstrap must not write through or read across.
    ///
    /// A path that does not exist is not a link.
    fn is_link(&self, path: &Path) -> Result<bool, std::io::Error>;
}

/// `std::fs`-backed adapter: `rename` publication and symlink detection.
///
/// This is the portable baseline for development and non-Windows hosts, not a
/// platform installer backend. It publishes atomically within a filesystem but
/// has no power-loss barrier, and it cannot see a reparse point that is not a
/// symlink, so production Windows hosts inject an adapter that can.
#[derive(Debug, Default, Clone, Copy)]
pub struct PortableBootstrapFileSystem;

impl PortableBootstrapFileSystem {
    pub fn new() -> Self {
        Self
    }
}

impl BootstrapFileSystem for PortableBootstrapFileSystem {
    fn publish_replace(&self, from: &Path, to: &Path) -> Result<(), std::io::Error> {
        std::fs::rename(from, to)
    }

    fn is_link(&self, path: &Path) -> Result<bool, std::io::Error> {
        match std::fs::symlink_metadata(path) {
            Ok(metadata) => Ok(metadata.file_type().is_symlink()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }
}
