//! Failures acquisition can report.
//!
//! Every variant names what could not be established. Nothing here is a
//! network or transport condition: a source translates those into
//! [`SourceError`](crate::SourceError), and the session turns a source failure
//! into an [`AcquireError::Unavailable`] carrying the source's own report.

use std::path::PathBuf;

use zup_core::Sha256Digest;

use crate::descriptor::ContentDescriptor;

/// Why one blob could not be acquired from any source.
#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    /// The source is structurally unable to serve this descriptor. A source
    /// chain treats this as "ask the next one", not as a failure.
    #[error("{origin} does not carry {kind} {digest}")]
    Absent {
        origin: String,
        kind: &'static str,
        digest: String,
    },
    /// The source found bytes and they did not match the descriptor.
    #[error("{origin} returned {kind} {digest} that failed verification: {detail}")]
    Corrupt {
        origin: String,
        kind: &'static str,
        digest: String,
        detail: &'static str,
    },
    /// The source reached a transport, origin, or filesystem failure. The
    /// detail is already free of credentials; a source never reports a URL
    /// that carries them.
    #[error("{origin} could not acquire {kind} {digest}: {detail}")]
    Unavailable {
        origin: String,
        kind: &'static str,
        digest: String,
        detail: String,
    },
}

impl SourceError {
    /// Build a "this source has nothing" report.
    pub fn absent(source: &str, descriptor: &ContentDescriptor) -> Self {
        Self::Absent {
            origin: source.to_owned(),
            kind: descriptor.kind().as_str(),
            digest: descriptor.digest.to_hex(),
        }
    }

    /// Build a "the bytes did not verify" report.
    pub fn corrupt(source: &str, descriptor: &ContentDescriptor, detail: &'static str) -> Self {
        Self::Corrupt {
            origin: source.to_owned(),
            kind: descriptor.kind().as_str(),
            digest: descriptor.digest.to_hex(),
            detail,
        }
    }

    /// Build a "the transfer did not work" report.
    pub fn unavailable(
        source: &str,
        descriptor: &ContentDescriptor,
        detail: impl Into<String>,
    ) -> Self {
        Self::Unavailable {
            origin: source.to_owned(),
            kind: descriptor.kind().as_str(),
            digest: descriptor.digest.to_hex(),
            detail: detail.into(),
        }
    }

    /// Which source reported this.
    pub fn source(&self) -> &str {
        match self {
            Self::Absent { origin, .. }
            | Self::Corrupt { origin, .. }
            | Self::Unavailable { origin, .. } => origin,
        }
    }

    /// Whether a source chain should try the next source.
    ///
    /// Every source error is recoverable by another source: content identity is
    /// the same wherever the bytes come from, so a source that has nothing or
    /// has the wrong bytes says nothing about the next one.
    pub const fn is_tryable(&self) -> bool {
        true
    }
}

/// Why an acquisition could not complete.
#[derive(Debug, thiserror::Error)]
pub enum AcquireError {
    /// A descriptor the release graph names is internally inconsistent.
    #[error("content descriptor is invalid: {0}")]
    Descriptor(&'static str),
    /// A document exceeded the bound its media type declares.
    #[error("{kind} is {size} bytes; the limit is {limit} bytes")]
    TooLarge {
        kind: &'static str,
        size: u64,
        limit: u64,
    },
    /// The graph named more content than this build will accept.
    #[error("acquisition names {count} items; the limit is {limit}")]
    TooManyItems { count: usize, limit: usize },
    /// The caller cancelled.
    #[error("acquisition was cancelled")]
    Cancelled,
    /// No source could satisfy the closure.
    #[error("no source could acquire {kind} {digest}")]
    Unavailable {
        kind: &'static str,
        digest: String,
        /// One report per source tried, in the order they were tried.
        reports: Vec<SourceError>,
    },
    /// A source reported the closure could not be satisfied at all.
    #[error("{0}")]
    Source(#[from] SourceError),
    /// The verified cache could not be used.
    #[error("content cache: {0}")]
    Cache(#[from] CacheError),
    /// A blob is present but a required resource is not, so the barrier holds.
    #[error("{kind} {digest} is still missing after acquisition")]
    Missing { kind: &'static str, digest: String },
    /// Staging a verified blob failed after the barrier should have been met.
    #[error("staging {kind} {digest} failed: {detail}")]
    Staging {
        kind: &'static str,
        digest: String,
        detail: String,
    },
    /// A path left the root it was resolved against.
    #[error("{path} escapes {root}")]
    Escapes { root: String, path: String },
    /// A path inside the cache or a source root is a link.
    #[error("{path} is a link")]
    Link { path: String },
    /// Filesystem failure.
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// A serializable document was not the shape it claims to be.
    #[error("acquisition document: {0}")]
    Json(#[from] serde_json::Error),
}

impl AcquireError {
    /// Attach a path to an I/O failure.
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }

    /// Whether this failure left the machine untouched.
    ///
    /// The acquisition barrier is the guarantee: every variant of this error
    /// is raised before any application-owned mutation begins, so the answer
    /// is always yes.
    pub const fn left_machine_unchanged(&self) -> bool {
        true
    }
}

/// Why the verified cache could not serve or accept content.
#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    /// The descriptor is not one this build will store.
    #[error("content descriptor is invalid: {0}")]
    Descriptor(&'static str),
    /// A file is larger than the cache will accept.
    #[error("{digest} declares {declared} bytes; the cache limit is {limit}")]
    TooLarge {
        digest: String,
        declared: u64,
        limit: u64,
    },
    /// A file on disk is not the length the descriptor declares.
    #[error("{digest} is {found} bytes; the descriptor declares {expected}")]
    SizeMismatch {
        digest: String,
        expected: u64,
        found: u64,
    },
    /// Bytes were written that the descriptor does not account for.
    #[error("{digest} grew past {limit} bytes while it was being written")]
    Overflow { digest: String, limit: u64 },
    /// Bytes were written that the descriptor does not account for.
    #[error("{digest} received {written} bytes at offset {offset}, past its {limit}-byte length")]
    OutOfRange {
        digest: String,
        offset: u64,
        written: u64,
        limit: u64,
    },
    /// The digest of the assembled content did not match.
    #[error("{digest} hashed to {found}")]
    DigestMismatch { digest: String, found: String },
    /// A path inside the cache is a link or a reparse point.
    #[error("{path} is a link")]
    Link { path: String },
    /// A path inside the cache left the cache root.
    #[error("{path} escapes the content cache at {root}")]
    Escapes { root: String, path: String },
    /// Another writer holds the blob's reservation.
    #[error("{digest} is reserved by another acquisition")]
    Reserved { digest: String },
    /// The cache root is not usable.
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl CacheError {
    /// Attach a path to an I/O failure.
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }

    /// A refusal that names a path and a reason, for state this crate read or
    /// wrote rather than a blob it was transferring.
    pub fn invalid(path: impl Into<PathBuf>, detail: impl Into<String>) -> Self {
        Self::Io {
            path: path.into(),
            source: std::io::Error::new(std::io::ErrorKind::InvalidInput, detail.into()),
        }
    }
}

/// The digest a failure refers to, when it names one.
pub fn failing_digest(error: &AcquireError) -> Option<Sha256Digest> {
    let digest = match error {
        AcquireError::Unavailable { digest, .. }
        | AcquireError::Missing { digest, .. }
        | AcquireError::Staging { digest, .. } => digest,
        AcquireError::Source(error) => return source_digest(error),
        _ => return None,
    };
    digest.parse().ok()
}

fn source_digest(error: &SourceError) -> Option<Sha256Digest> {
    let digest = match error {
        SourceError::Absent { digest, .. }
        | SourceError::Corrupt { digest, .. }
        | SourceError::Unavailable { digest, .. } => digest,
    };
    digest.parse().ok()
}
