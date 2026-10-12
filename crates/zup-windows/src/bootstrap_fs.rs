use std::io;
use std::path::Path;
use std::sync::Arc;

use zup_bootstrap::BootstrapFileSystem;

use crate::durable::{self, DurableError};

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
        crate::bindings::is_link_or_reparse(path)
    }
}

fn publish_error(error: DurableError) -> io::Error {
    match error {
        DurableError::Io { source, .. } => source,
        other => io::Error::other(other.to_string()),
    }
}
