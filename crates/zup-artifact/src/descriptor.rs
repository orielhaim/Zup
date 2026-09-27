//! Content descriptors: the addressing unit of the artifact graph.
//!
//! A descriptor names a media type, a content digest, and a length. It is the
//! only way content is referenced. Nothing in the graph inlines a large
//! structure to be shared, so the same bytes are described by the same
//! descriptor no matter how many variants point at them.

use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zup_core::Sha256Digest;

use crate::error::ArtifactError;
use crate::media_type::MediaType;

/// An immutable, content-addressed pointer to one blob of artifact content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Descriptor {
    pub media_type: MediaType,
    pub digest: Sha256Digest,
    pub size: u64,
}

impl Descriptor {
    /// Describe `bytes` under `media_type`.
    pub fn of(media_type: MediaType, bytes: &[u8]) -> Self {
        Self {
            media_type,
            digest: Sha256Digest::from_bytes(Sha256::digest(bytes).into()),
            size: bytes.len() as u64,
        }
    }

    /// A short name for diagnostics.
    pub fn label(&self) -> &'static str {
        self.media_type.label()
    }

    /// Verify `bytes` against this descriptor's length and digest.
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

/// Serialization used for every embedded JSON document in the graph.
///
/// Canonical here means: struct fields in declaration order, map keys sorted,
/// integer values only, and no insignificant whitespace. Two builds of the
/// same graph therefore produce byte-identical documents, and a digest computed
/// over the bytes is a stable identifier for the content shape itself.
pub fn to_canonical_json<T: Serialize>(value: &T) -> Result<Vec<u8>, ArtifactError> {
    Ok(serde_json::to_vec(value)?)
}

/// Parse a canonical JSON document after checking it against its size bound.
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
        // The type string carries its own version, so a future shape is a new
        // variant and an old reader refuses it instead of guessing.
        let unknown = r#"{"media_type":"application/vnd.zup.artifact.index.v9+json","digest":"0000000000000000000000000000000000000000000000000000000000000000","size":0}"#;
        assert!(serde_json::from_str::<Descriptor>(unknown).is_err());
        let known = format!(
            r#"{{"media_type":"{}","digest":"{}","size":3}}"#,
            MediaType::BLOB.as_str(),
            "0".repeat(64)
        );
        assert!(serde_json::from_str::<Descriptor>(&known).is_ok());
    }
}
