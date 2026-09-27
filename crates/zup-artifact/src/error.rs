//! Errors produced by the distribution artifact graph.

use thiserror::Error;

use crate::compat::Incompatible;

/// Failures produced while composing, reading, or selecting artifacts.
#[derive(Debug, Error)]
pub enum ArtifactError {
    #[error("artifact I/O: {0}")]
    Io(#[from] std::io::Error),

    #[error("artifact metadata is not canonical JSON: {0}")]
    Json(#[from] serde_json::Error),

    #[error("artifact {kind} is {size} bytes; the limit is {limit} bytes")]
    MetadataTooLarge {
        kind: &'static str,
        size: u64,
        limit: u64,
    },

    #[error("artifact is truncated, corrupt, canonically unsorted, or has unsafe offsets")]
    Invalid,

    #[error(
        "artifact {kind} requires unknown features {unknown:#x}; this build understands {supported:#x}"
    )]
    UnsupportedFeatures {
        kind: &'static str,
        unknown: u64,
        supported: u64,
    },

    #[error("artifact declares {count} variants; the limit is {limit}")]
    TooManyVariants { count: usize, limit: usize },

    #[error("artifact table declares {count} blobs; the limit is {limit}")]
    TooManyBlobs { count: usize, limit: usize },

    #[error("cannot allocate {size} bytes while processing artifact content")]
    Allocation { size: u64 },

    #[error("{media_type} {digest} is not present in this artifact")]
    Missing {
        media_type: &'static str,
        digest: String,
    },

    #[error("{media_type} {digest} failed digest verification")]
    DigestMismatch {
        media_type: &'static str,
        digest: String,
    },

    #[error("{media_type} {digest} is {found} bytes; the descriptor declares {expected}")]
    SizeMismatch {
        media_type: &'static str,
        digest: String,
        expected: u64,
        found: u64,
    },

    #[error("variant `{id}` is not present in this artifact")]
    UnknownVariant { id: String },

    /// Boxed because a refusal names two variants and a dimension's two values,
    /// which is far larger than any other failure and would otherwise be copied
    /// onto the stack of every function that can produce one.
    #[error(transparent)]
    Incompatible(Box<Incompatible>),

    #[error("no variant in artifact `{id}` supports this host: {detail}")]
    NoCompatibleVariant { id: String, detail: String },

    #[error("variant selection is ambiguous between `{left}` and `{right}`")]
    Ambiguous { left: String, right: String },

    #[error("artifact `{id}` is incomplete: {detail}")]
    Incomplete { id: String, detail: String },

    #[error(transparent)]
    Package(#[from] zup_bundle::PackageError),
}

impl From<Incompatible> for ArtifactError {
    fn from(value: Incompatible) -> Self {
        Self::Incompatible(Box::new(value))
    }
}

impl ArtifactError {
    /// Whether this failure is a host that no variant can serve, which the
    /// dispatcher presents as a supported refusal rather than a defect.
    pub fn is_unsupported_host(&self) -> bool {
        matches!(self, Self::NoCompatibleVariant { .. })
    }
}
