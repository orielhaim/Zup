use std::fmt::Debug;
use std::io;

/// A verified metadata transition that must be published as one atomic batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetadataChange {
    /// Publish a metadata entry.
    Write {
        /// Logical metadata filename.
        name: String,
        /// Serialized metadata bytes.
        bytes: Vec<u8>,
    },
    /// Invalidate previously trusted metadata.
    Remove {
        /// Logical metadata filename.
        name: String,
    },
}

/// Persistent metadata authority. Implementations retain their repository lease
/// for their entire lifetime and durably publish each complete batch before returning.
pub trait MetadataStore: Debug + Send + Sync {
    /// Load a committed metadata entry.
    fn bytes(&self, name: &str) -> io::Result<Option<Vec<u8>>>;
    /// Atomically and durably publish the complete transition.
    fn apply(&self, changes: &[MetadataChange]) -> io::Result<()>;
}
