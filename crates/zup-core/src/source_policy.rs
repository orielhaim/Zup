//! Host policy for inspecting build input sources.
//!
//! A prerequisite read through a link is a read of bytes the project never
//! declared, so materialization refuses sources whose ancestry contains one.
//! What counts as a link is a property of the host filesystem: `std::fs`
//! reports a link by each platform's own rules, while a Windows host also has
//! reparse points that redirect a read without presenting as a symlink.
//!
//! The trait lives here, in the platform layer, rather than in the crate that
//! materializes. The materializer and the host adapter are two different halves
//! of the same decision, and putting the question next to the platform's other
//! questions is what lets a host answer it without depending on the build plane.

use std::path::Path;

/// The link guarantees source inspection depends on.
///
/// An implementation reports whatever its host can see. Claiming a link that is
/// not there would reject valid projects, so a policy that cannot see a host
/// indirection must say so rather than guess.
pub trait SourceFilePolicy: Send + Sync {
    /// Report whether `path` is a link that source inspection must refuse.
    ///
    /// A path that does not exist is not a link.
    fn is_link(&self, path: &Path) -> Result<bool, std::io::Error>;
}

/// The `std::fs` policy: whatever `std::fs` reports as a symlink, and nothing
/// else.
///
/// This is the portable default for a caller with no host adapter, not a
/// hardened build policy. `std::fs` decides what a link is by each platform's own
/// rules and stops there, so a host with a wider notion of indirection injects
/// its own adapter, which refuses every reparse point it can see.
#[derive(Debug, Default, Clone, Copy)]
pub struct PortableSourceFilePolicy;

impl SourceFilePolicy for PortableSourceFilePolicy {
    fn is_link(&self, path: &Path) -> Result<bool, std::io::Error> {
        match std::fs::symlink_metadata(path) {
            Ok(metadata) => Ok(metadata.file_type().is_symlink()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }
}
