//! Windows adapter for the bootstrap filesystem seam.
//!
//! Publication reuses the crate's durable file primitives so acquired
//! prerequisites and bootstrap state land through `MoveFileExW` with
//! `MOVEFILE_WRITE_THROUGH`, and any entry carrying
//! `FILE_ATTRIBUTE_REPARSE_POINT` is treated as a link: junctions, mount
//! points, and symlinks all redirect a write out of the quarantine root.

use std::io;
use std::path::Path;
use std::sync::Arc;

use zup_bootstrap::BootstrapFileSystem;

use crate::durable::{self, DurableError};
use crate::fs_bindings;

/// The filesystem seam Windows bootstrap production paths run on.
pub fn windows_bootstrap_file_system() -> Arc<dyn BootstrapFileSystem> {
    Arc::new(WindowsBootstrapFileSystem)
}

#[derive(Debug, Default, Clone, Copy)]
pub struct WindowsBootstrapFileSystem;

impl WindowsBootstrapFileSystem {
    pub fn new() -> Self {
        Self
    }
}

impl BootstrapFileSystem for WindowsBootstrapFileSystem {
    fn publish_replace(&self, from: &Path, to: &Path) -> Result<(), io::Error> {
        durable::move_durable(from, to).map_err(publish_error)
    }

    fn is_link(&self, path: &Path) -> Result<bool, io::Error> {
        match path.symlink_metadata() {
            Ok(metadata) => {
                Ok(metadata.file_type().is_symlink() || fs_bindings::is_reparse_point(path))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }
}

/// Preserve the underlying `io::Error` so callers keep its `ErrorKind`; Win32
/// failures with no errno equivalent travel as text.
fn publish_error(error: DurableError) -> io::Error {
    match error {
        DurableError::Io { source, .. } => source,
        other => io::Error::other(other.to_string()),
    }
}
