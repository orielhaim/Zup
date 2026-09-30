//! Versioned media types for the artifact graph.
//!
//! A media type is a stable, versioned name for one content shape. It is the
//! only thing that decides how a descriptor's bytes are interpreted, so a
//! reader never guesses: an unrecognized type does not deserialize, and the type
//! string carries its own version, so a future incompatible shape is a new
//! variant rather than a widened parser.

use std::fmt;

use serde::{Deserialize, Serialize};

/// One artifact content shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum MediaType {
    /// The artifact index: the root of the graph.
    #[serde(rename = "application/vnd.zup.artifact.index.v1+json")]
    Index,
    /// The content-addressed store table that locates every blob by digest.
    #[serde(rename = "application/vnd.zup.artifact.blob-table.v1+json")]
    BlobTable,
    /// One variant's content graph: installer intent plus its content set.
    #[serde(rename = "application/vnd.zup.artifact.variant.v1+json")]
    VariantManifest,
    /// One Zstandard-compressed content blob of the shared store.
    #[serde(rename = "application/vnd.zup.artifact.blob.v1")]
    Blob,
    /// One variant's native maintenance runtime image.
    #[serde(rename = "application/vnd.zup.artifact.runtime.v1")]
    Runtime,
    /// One variant's native UI preset image.
    #[serde(rename = "application/vnd.zup.artifact.preset.v1")]
    Preset,
}

impl MediaType {
    /// The artifact index.
    pub const INDEX: Self = Self::Index;
    /// The content-addressed store table.
    pub const BLOB_TABLE: Self = Self::BlobTable;
    /// One variant's content graph.
    pub const VARIANT_MANIFEST: Self = Self::VariantManifest;
    /// One compressed content blob.
    pub const BLOB: Self = Self::Blob;
    /// One native runtime image.
    pub const RUNTIME: Self = Self::Runtime;
    /// One native preset image.
    pub const PRESET: Self = Self::Preset;

    /// The stable type string, which also carries the shape version.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Index => "application/vnd.zup.artifact.index.v1+json",
            Self::BlobTable => "application/vnd.zup.artifact.blob-table.v1+json",
            Self::VariantManifest => "application/vnd.zup.artifact.variant.v1+json",
            Self::Blob => "application/vnd.zup.artifact.blob.v1",
            Self::Runtime => "application/vnd.zup.artifact.runtime.v1",
            Self::Preset => "application/vnd.zup.artifact.preset.v1",
        }
    }

    /// A short name for diagnostics and reports.
    pub const fn label(self) -> &'static str {
        self.as_str()
    }

    /// The size bound a document of this type is checked against before it is
    /// deserialized.
    pub const fn limit(self) -> u64 {
        match self {
            Self::Index => MAX_INDEX_BYTES,
            Self::BlobTable => MAX_BLOB_TABLE_BYTES,
            Self::VariantManifest => MAX_VARIANT_MANIFEST_BYTES,
            Self::Runtime => MAX_RUNTIME_BYTES,
            // A preset is one native binary per target, the same kind of thing a
            // runtime is, and is bounded the same way.
            Self::Preset => MAX_RUNTIME_BYTES,
            Self::Blob => u64::MAX,
        }
    }

    /// The name a diagnostic uses for this type.
    pub const fn kind(self) -> &'static str {
        match self {
            Self::Index => "artifact index",
            Self::BlobTable => "blob table",
            Self::VariantManifest => "variant manifest",
            Self::Blob => "content blob",
            Self::Runtime => "native runtime",
            Self::Preset => "native preset",
        }
    }
}

impl fmt::Display for MediaType {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Upper bound on the artifact index. The index references content by digest
/// and never inlines it, so it stays small enough to read before trusting
/// anything else in the artifact.
pub const MAX_INDEX_BYTES: u64 = 4 * 1024 * 1024;

/// Upper bound on the blob table, which grows with the unique content count.
pub const MAX_BLOB_TABLE_BYTES: u64 = 32 * 1024 * 1024;

/// Upper bound on one variant manifest.
pub const MAX_VARIANT_MANIFEST_BYTES: u64 = 256 * 1024 * 1024;

/// Upper bound on one native runtime image.
pub const MAX_RUNTIME_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// Largest number of variants one artifact may carry.
pub const MAX_VARIANTS: usize = 64;

/// Largest number of unique blobs one artifact may carry.
pub const MAX_BLOBS: usize = 4_000_000;

/// Largest number of blob segments one artifact may use.
///
/// Segments are the unit a platform container writes as a single opaque
/// container region, so their count is bounded by what a container can address.
pub const MAX_SEGMENTS: u16 = 4096;
