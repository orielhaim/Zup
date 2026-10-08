use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum MediaType {
    #[serde(rename = "application/vnd.zup.artifact.index.v1+json")]
    Index,
    #[serde(rename = "application/vnd.zup.artifact.blob-table.v1+json")]
    BlobTable,
    #[serde(rename = "application/vnd.zup.artifact.variant.v1+json")]
    VariantManifest,
    #[serde(rename = "application/vnd.zup.artifact.blob.v1")]
    Blob,
    #[serde(rename = "application/vnd.zup.artifact.runtime.v1")]
    Runtime,
    #[serde(rename = "application/vnd.zup.artifact.preset.v1")]
    Preset,
}

impl MediaType {
    pub const INDEX: Self = Self::Index;
    pub const BLOB_TABLE: Self = Self::BlobTable;
    pub const VARIANT_MANIFEST: Self = Self::VariantManifest;
    pub const BLOB: Self = Self::Blob;
    pub const RUNTIME: Self = Self::Runtime;
    pub const PRESET: Self = Self::Preset;

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

    pub const fn label(self) -> &'static str {
        self.as_str()
    }

    pub const fn limit(self) -> u64 {
        match self {
            Self::Index => MAX_INDEX_BYTES,
            Self::BlobTable => MAX_BLOB_TABLE_BYTES,
            Self::VariantManifest => MAX_VARIANT_MANIFEST_BYTES,
            Self::Runtime => MAX_RUNTIME_BYTES,
            Self::Preset => MAX_RUNTIME_BYTES,
            Self::Blob => u64::MAX,
        }
    }

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

/// and never inlines it, so it stays small enough to read before trusting
pub const MAX_INDEX_BYTES: u64 = 4 * 1024 * 1024;

pub const MAX_BLOB_TABLE_BYTES: u64 = 32 * 1024 * 1024;

pub const MAX_VARIANT_MANIFEST_BYTES: u64 = 256 * 1024 * 1024;

pub const MAX_RUNTIME_BYTES: u64 = 4 * 1024 * 1024 * 1024;

pub const MAX_VARIANTS: usize = 64;

pub const MAX_BLOBS: usize = 4_000_000;

pub const MAX_SEGMENTS: u16 = 4096;

use thiserror::Error;

use crate::compat::Incompatible;
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

    #[error("`{found}` is not the start of a preset package")]
    PresetMagic { found: String },

    #[error("this preset package is schema {found}; this build reads schema {expected}")]
    PresetSchema { expected: u32, found: u32 },

    #[error("this preset package is {found} bytes; its header accounts for {expected}")]
    PresetTruncated { expected: u64, found: u64 },

    #[error("this preset package is {found} bytes; its header accounts for {expected}")]
    PresetTrailing { expected: u64, found: u64 },

    #[error("this preset package cannot be used: {0}")]
    PresetMetadata(String),

    #[error("a preset binary is {size} bytes; the limit is {limit}")]
    BinaryTooLarge { size: u64, limit: u64 },

    #[error("a preset package would carry {count} binaries; the limit is {limit}")]
    TooManyPresetBinaries { count: usize, limit: usize },

    #[error("the preset binary for `{target}` is damaged ({digest}): {reason}")]
    PresetCorrupt {
        target: String,
        digest: String,
        reason: String,
    },

    #[error("this preset package has no binary for `{target}`; it carries: {available}")]
    PresetTargetUnavailable { target: String, available: String },
}

impl From<Incompatible> for ArtifactError {
    fn from(value: Incompatible) -> Self {
        Self::Incompatible(Box::new(value))
    }
}

impl ArtifactError {
    pub fn is_unsupported_host(&self) -> bool {
        matches!(self, Self::NoCompatibleVariant { .. })
    }
}

use sha2::{Digest, Sha256};
use zup_core::Sha256Digest;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Descriptor {
    pub media_type: MediaType,
    pub digest: Sha256Digest,
    pub size: u64,
}

impl Descriptor {
    pub fn of(media_type: MediaType, bytes: &[u8]) -> Self {
        Self {
            media_type,
            digest: Sha256Digest::from_bytes(Sha256::digest(bytes).into()),
            size: bytes.len() as u64,
        }
    }

    pub fn label(&self) -> &'static str {
        self.media_type.label()
    }

    pub fn verify(&self, bytes: &[u8]) -> Result<(), ArtifactError> {
        if bytes.len() as u64 != self.size {
            return Err(ArtifactError::SizeMismatch {
                media_type: self.label(),
                digest: self.digest.to_hex(),
                expected: self.size,
                found: bytes.len() as u64,
            });
        }
        let actual = Sha256Digest::from_bytes(Sha256::digest(bytes).into());
        if actual != self.digest {
            return Err(ArtifactError::DigestMismatch {
                media_type: self.label(),
                digest: self.digest.to_hex(),
            });
        }
        Ok(())
    }
}

impl fmt::Display for Descriptor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} sha256:{} ({} bytes)",
            self.media_type, self.digest, self.size
        )
    }
}

pub fn to_canonical_json<T: Serialize>(value: &T) -> Result<Vec<u8>, ArtifactError> {
    Ok(serde_json::to_vec(value)?)
}

pub fn from_bounded_json<T: for<'de> Deserialize<'de>>(
    bytes: &[u8],
    limit: u64,
    kind: &'static str,
) -> Result<T, ArtifactError> {
    if bytes.len() as u64 > limit {
        return Err(ArtifactError::MetadataTooLarge {
            kind,
            size: bytes.len() as u64,
            limit,
        });
    }
    Ok(serde_json::from_slice(bytes)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_verifies_length_before_digest() {
        let descriptor = Descriptor::of(MediaType::BLOB, b"payload");
        assert!(descriptor.verify(b"payload").is_ok());
        let short = descriptor.verify(b"payloa").unwrap_err();
        assert!(matches!(short, ArtifactError::SizeMismatch { .. }));
        let wrong = descriptor.verify(b"payloae").unwrap_err();
        assert!(matches!(wrong, ArtifactError::DigestMismatch { .. }));
    }

    #[test]
    fn an_unrecognized_media_type_does_not_deserialize() {
        let unknown = r#"{"media_type":"application/vnd.zup.artifact.index.v9+json","digest":"0000000000000000000000000000000000000000000000000000000000000000","size":0}"#;
        assert!(serde_json::from_str::<Descriptor>(unknown).is_err());
    }
}
