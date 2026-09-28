//! What one piece of content is, and what it costs to move.
//!
//! A [`ContentDescriptor`] is the unit an acquisition session schedules. It
//! carries the identity a release graph authenticates - digest, compressed
//! size, logical size, and content kind - plus the scheduler's view of how
//! urgent it is. Nothing here knows how the bytes travel.

use std::cmp::Ordering;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zup_core::Sha256Digest;

/// What a piece of content is, which decides how it is verified and staged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentKind {
    /// One Zstandard-compressed payload object, addressed by the digest of its
    /// *decompressed* content.
    Payload,
    /// One native maintenance runtime image. Executable code, so it gets the
    /// strictest bounds: a launch descriptor is a promise to run these bytes.
    Runtime,
    /// One authenticated metadata document, such as a variant manifest.
    Metadata,
    /// One content catalog: the digest-to-size map for a release.
    Catalog,
}

impl ContentKind {
    /// Stable name used in diagnostics and error messages.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Payload => "payload blob",
            Self::Runtime => "native runtime",
            Self::Metadata => "metadata document",
            Self::Catalog => "content catalog",
        }
    }

    /// Whether this kind is executed, which raises its size bound and its
    /// verification strictness.
    pub const fn is_executable(self) -> bool {
        matches!(self, Self::Runtime)
    }

    /// The largest content of this kind this build will accept.
    pub const fn size_limit(self) -> u64 {
        match self {
            Self::Payload => MAX_PAYLOAD_BYTES,
            Self::Runtime => MAX_RUNTIME_BYTES,
            Self::Metadata => MAX_METADATA_BYTES,
            Self::Catalog => MAX_CATALOG_BYTES,
        }
    }

    /// How many times content of this kind may be decompressed relative to its
    /// compressed size.
    ///
    /// Payload is already bounded on both sides, so the ratio is a second line
    /// of defence against a compression bomb. Metadata and catalogs are
    /// required to be near-linear, because a document that expands wildly is
    /// not a document.
    pub const fn max_expansion_ratio(self) -> u64 {
        match self {
            Self::Payload => 4096,
            Self::Runtime | Self::Metadata | Self::Catalog => 256,
        }
    }
}

impl fmt::Display for ContentKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// How urgently the scheduler should move one item.
///
/// Ordering is `Critical` first. A large payload blob and a small blocking
/// document can both be `Normal`, and the scheduler breaks that tie by size, so
/// a 4 GiB payload never sits in front of a 2 KiB document that the installer
/// is blocked on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentPriority {
    /// The installer cannot start without it: a native runtime to hand
    /// control to.
    Critical,
    /// Authenticated metadata the plan depends on.
    Normal,
    /// Payload the selected closure needs.
    Payload,
    /// Content that makes a later operation cheaper, such as a blob kept for
    /// offline repair, or work for a variant this machine will not install.
    Background,
}

impl ContentPriority {
    /// Stable name used in diagnostics and event payloads.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Critical => "critical",
            Self::Normal => "normal",
            Self::Payload => "payload",
            Self::Background => "background",
        }
    }
}

/// Why one item is in a closure, which is what makes a download explainable.
///
/// A machine that downloads 184 MiB should be able to say which 184 MiB. Every
/// item carries the reason it was selected, and a group lets the UI collapse
/// the detail without losing it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentReason {
    /// The native runtime that runs the lifecycle.
    Runtime,
    /// The variant manifest that describes the content plan.
    Manifest,
    /// The digest-to-size catalog a release is measured against.
    Catalog,
    /// A payload file the selected components install.
    File { component: Option<String> },
    /// An embedded prerequisite package.
    Prerequisite { id: String },
    /// A plugin's AOT image.
    Plugin { id: String },
    /// Content the same machine already has, retained for offline repair.
    Retained,
}

impl ContentReason {
    /// Stable group name, which is what a frontend shows instead of the detail.
    pub const fn group(&self) -> &'static str {
        match self {
            Self::Runtime => "runtime",
            Self::Manifest | Self::Catalog => "metadata",
            Self::File { .. } => "files",
            Self::Prerequisite { .. } => "prerequisites",
            Self::Plugin { .. } => "plugins",
            Self::Retained => "retained",
        }
    }
}

/// How a piece of content is carried.
///
/// This is stated rather than inferred from the two sizes, because the two are
/// not enough: incompressible content that happens to compress to exactly its
/// original length is indistinguishable from content that was never
/// compressed, and reading those two cases differently is a correctness bug
/// waiting to happen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentCompression {
    /// The wire form is the logical form. A range request over the wire length
    /// is a range request over the content.
    None,
    /// The wire form is a Zstandard frame whose decoded length is the logical
    /// length. This is how payload blobs travel.
    Zstandard,
}

impl ContentCompression {
    /// Stable name used in diagnostics.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "stored",
            Self::Zstandard => "zstd",
        }
    }
}

/// One piece of authenticated, immutable content.
///
/// `digest` is the identity of the *logical* bytes. `compressed_size` is the
/// length of the *wire* form, which is what a range request, a resume, or a
/// size cap operates on. `size` is what the logical bytes measure, and it is
/// what an install-size estimate budgets with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContentDescriptor {
    pub kind: ContentKind,
    pub digest: Sha256Digest,
    /// How the wire form reaches the logical form.
    pub compression: ContentCompression,
    /// Length of the wire form.
    pub compressed_size: u64,
    /// Length of the logical form.
    pub size: u64,
    pub priority: ContentPriority,
}

impl ContentDescriptor {
    /// Describe content with both of its sizes stated.
    pub fn new(
        kind: ContentKind,
        digest: Sha256Digest,
        compression: ContentCompression,
        compressed_size: u64,
        size: u64,
    ) -> Self {
        Self {
            kind,
            digest,
            compression,
            compressed_size,
            size,
            priority: default_priority(kind),
        }
    }

    /// Describe content carried without compression.
    pub fn stored(kind: ContentKind, digest: Sha256Digest, size: u64) -> Self {
        Self::new(kind, digest, ContentCompression::None, size, size)
    }

    /// Describe content carried as a Zstandard frame.
    pub fn compressed(
        kind: ContentKind,
        digest: Sha256Digest,
        compressed_size: u64,
        size: u64,
    ) -> Self {
        Self::new(
            kind,
            digest,
            ContentCompression::Zstandard,
            compressed_size,
            size,
        )
    }

    /// Override the scheduler priority.
    pub fn with_priority(mut self, priority: ContentPriority) -> Self {
        self.priority = priority;
        self
    }

    /// This content's kind.
    pub const fn kind(&self) -> ContentKind {
        self.kind
    }

    /// This content's identity.
    pub const fn digest(&self) -> &Sha256Digest {
        &self.digest
    }

    /// How this content's wire form reaches its logical form.
    pub const fn compression(&self) -> ContentCompression {
        self.compression
    }

    /// Length of the wire form.
    pub const fn compressed_size(&self) -> u64 {
        self.compressed_size
    }

    /// Length of the logical form.
    pub const fn size(&self) -> u64 {
        self.size
    }

    /// Whether this content is carried compressed, so a source must
    /// decompress it to reach the logical bytes.
    pub const fn is_compressed(&self) -> bool {
        matches!(self.compression, ContentCompression::Zstandard)
    }

    /// The largest number of logical bytes this descriptor's compressed form
    /// may expand into.
    pub const fn expansion_limit(&self) -> u64 {
        self.compressed_size
            .saturating_mul(self.kind.max_expansion_ratio())
    }

    /// Reject a descriptor a hostile graph could use to make a reader
    /// allocate, or to understate what a transfer will produce.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.size == 0 {
            return Err("content cannot be zero bytes");
        }
        if self.compressed_size == 0 {
            return Err("content cannot be zero bytes on the wire");
        }
        if self.size > self.kind.size_limit() {
            return Err("content exceeds the size limit for its kind");
        }
        if self.size > self.expansion_limit() {
            return Err("content declares an expansion ratio beyond its limit");
        }
        Ok(())
    }

    /// Build a descriptor for a complete in-memory document.
    pub fn of_document(
        kind: ContentKind,
        bytes: &[u8],
        priority: ContentPriority,
    ) -> Result<Self, crate::AcquireError> {
        if bytes.len() as u64 > kind.size_limit() {
            return Err(crate::AcquireError::TooLarge {
                kind: kind.as_str(),
                size: bytes.len() as u64,
                limit: kind.size_limit(),
            });
        }
        Ok(Self {
            kind,
            digest: Sha256Digest::from_bytes(Sha256::digest(bytes).into()),
            compression: ContentCompression::None,
            compressed_size: bytes.len() as u64,
            size: bytes.len() as u64,
            priority,
        })
    }
}

impl fmt::Display for ContentDescriptor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} {}", self.kind, self.digest)
    }
}

/// The order the scheduler takes work in: priority first, then the smaller
/// item, so a small blocking document overtakes a large payload blob.
pub fn schedule_order(left: &ContentDescriptor, right: &ContentDescriptor) -> Ordering {
    left.priority
        .cmp(&right.priority)
        .then_with(|| left.compressed_size.cmp(&right.compressed_size))
        .then_with(|| left.digest.cmp(&right.digest))
}

/// The priority a kind gets when a caller states nothing.
pub const fn default_priority(kind: ContentKind) -> ContentPriority {
    match kind {
        ContentKind::Runtime => ContentPriority::Critical,
        ContentKind::Metadata | ContentKind::Catalog => ContentPriority::Normal,
        ContentKind::Payload => ContentPriority::Payload,
    }
}

/// Largest payload object the acquisition engine will accept.
pub const MAX_PAYLOAD_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// Largest executable content object, which is a native runtime image.
pub const MAX_RUNTIME_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// Largest single metadata document.
pub const MAX_METADATA_BYTES: u64 = 256 * 1024 * 1024;

/// Largest content catalog.
pub const MAX_CATALOG_BYTES: u64 = 64 * 1024 * 1024;

/// Largest closure one transaction may name.
pub const MAX_CLOSURE_ITEMS: usize = 4_000_000;

/// A relative path inside a content tree, checked once at construction.
///
/// Sources and the cache both resolve untrusted names against a root, and both
/// use this to refuse a path that climbs out before they touch the
/// filesystem. It is deliberately a `/`-separated string rather than a
/// `Path`, so the same check governs a URL path and a filesystem path.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RelativeContentPath(String);

impl RelativeContentPath {
    /// Check a relative path, refusing anything that could leave a root.
    pub fn parse(value: &str) -> Result<Self, &'static str> {
        if value.is_empty() {
            return Err("a content path cannot be empty");
        }
        if value.starts_with('/') || value.starts_with('\\') {
            return Err("a content path cannot be absolute");
        }
        if value.len() >= 2 && value.as_bytes()[1] == b':' {
            return Err("a content path cannot name a drive");
        }
        if value.contains('\\') {
            return Err("a content path uses `/` separators");
        }
        if value.contains('\0') {
            return Err("a content path cannot contain a NUL");
        }
        for segment in value.split('/') {
            if segment.is_empty() {
                return Err("a content path has an empty segment");
            }
            if segment == "." || segment == ".." {
                return Err("a content path cannot contain `.` or `..`");
            }
        }
        Ok(Self(value.to_owned()))
    }
}

impl FromStr for RelativeContentPath {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl TryFrom<String> for RelativeContentPath {
    type Error = &'static str;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

impl From<RelativeContentPath> for String {
    fn from(value: RelativeContentPath) -> Self {
        value.0
    }
}

impl fmt::Display for RelativeContentPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}
