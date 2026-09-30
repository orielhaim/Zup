//! `.zupui`: a preset's own portable artifact.
//!
//! A preset is compiled, not scripted, so the thing an application consumes is a
//! native binary for a target - and a project that supports three of them has
//! three binaries and one description of what they are. This module is that
//! description plus its content, in one file, verified without running any of it.
//!
//! A package is a fixed header, a canonical JSON document, and the compressed
//! binaries that document names. It is not an archive format: there is exactly
//! one kind of thing a `.zupui` can contain, and a reader that found anything
//! else has found a file that is not one.

use serde::{Deserialize, Serialize};
use zup_core::{Sha256Digest, TargetTriple};
use zup_ui_protocol::{UI_PROTOCOL_VERSION, UiCapabilities};

use crate::error::ArtifactError;

/// The bytes that open every package.
pub const MAGIC: [u8; 4] = *b"ZPUI";

/// The package schema this build reads and writes.
pub const PACKAGE_SCHEMA: u32 = 1;

/// The fixed header: magic, schema, and the three lengths that bound the rest.
pub const HEADER_BYTES: usize = 32;

/// The most metadata one package may carry.
///
/// A settings schema is generated code, so this is a statement about what a
/// generated schema can weigh rather than a limit an application hits.
pub const MAX_METADATA_BYTES: u64 = 4 * 1024 * 1024;

/// The most target binaries one package may carry.
pub const MAX_BINARIES: usize = 64;

/// The most bytes one compressed binary may occupy.
pub const MAX_BINARY_BYTES: u64 = 256 * 1024 * 1024;

/// The most bytes every binary in one package may occupy, compressed.
pub const MAX_TOTAL_BINARY_BYTES: u64 = 1024 * 1024 * 1024;

/// One target's native preset binary.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PresetBinary {
    pub target: TargetTriple,
    /// The size of the binary as it will be written to disk.
    pub size: u64,
    /// The digest of those bytes, and the content address they are stored under.
    pub sha256: Sha256Digest,
}

/// One stored binary.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PresetBlob {
    pub digest: Sha256Digest,
    /// The compressed size, which is what the header's byte count accounts for.
    pub size: u64,
}

/// What a package says about the preset it carries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PresetPackage {
    pub schema: u32,
    pub name: String,
    pub version: semver::Version,
    /// The UI wire protocol this build speaks.
    pub ui_protocol: u32,
    /// What this preset cannot present without.
    pub required_capabilities: UiCapabilities,
    /// The JSON Schema of this preset's settings, generated from its own types.
    pub settings_schema: serde_json::Value,
    /// One entry per supported target, ordered by target so two builds of the
    /// same package produce the same document.
    pub binaries: Vec<PresetBinary>,
    /// The stored form of each binary, ordered by digest.
    pub blobs: Vec<PresetBlob>,
}

impl PresetPackage {
    /// The binary for one target, or a message naming what the package has.
    pub fn binary_for(&self, target: &TargetTriple) -> Result<&PresetBinary, ArtifactError> {
        self.binaries
            .iter()
            .find(|binary| &binary.target == target)
            .ok_or(ArtifactError::PresetTargetUnavailable {
                target: target.as_str().to_owned(),
                available: self.targets().join(", "),
            })
    }

    /// The targets this package carries, in the order the document names them.
    pub fn targets(&self) -> Vec<&str> {
        self.binaries
            .iter()
            .map(|binary| binary.target.as_str())
            .collect()
    }

    /// Reject a document that could not be written as a package.
    pub fn validate(&self) -> Result<(), ArtifactError> {
        if self.schema != PACKAGE_SCHEMA {
            return Err(ArtifactError::PresetSchema {
                expected: PACKAGE_SCHEMA,
                found: self.schema,
            });
        }
        if self.name.trim().is_empty() {
            return Err(ArtifactError::PresetMetadata(
                "the preset has no name".into(),
            ));
        }
        if !self.settings_schema.is_object() {
            return Err(ArtifactError::PresetMetadata(
                "the settings schema is not a JSON object".into(),
            ));
        }
        if self.binaries.is_empty() {
            return Err(ArtifactError::PresetMetadata(
                "the package carries no binaries".into(),
            ));
        }
        if self.binaries.len() > MAX_BINARIES {
            return Err(ArtifactError::TooManyPresetBinaries {
                count: self.binaries.len(),
                limit: MAX_BINARIES,
            });
        }
        for pair in self.binaries.windows(2) {
            if pair[0].target == pair[1].target {
                return Err(ArtifactError::PresetMetadata(format!(
                    "the package lists `{}` more than once",
                    pair[0].target.as_str()
                )));
            }
        }
        for blob in &self.blobs {
            if blob.size > MAX_BINARY_BYTES {
                return Err(ArtifactError::BinaryTooLarge {
                    size: blob.size,
                    limit: MAX_BINARY_BYTES,
                });
            }
        }
        let stored = self
            .blobs
            .iter()
            .try_fold(0u64, |total, blob| total.checked_add(blob.size))
            .ok_or(ArtifactError::BinaryTooLarge {
                size: u64::MAX,
                limit: MAX_TOTAL_BINARY_BYTES,
            })?;
        if stored > MAX_TOTAL_BINARY_BYTES {
            return Err(ArtifactError::BinaryTooLarge {
                size: stored,
                limit: MAX_TOTAL_BINARY_BYTES,
            });
        }
        for pair in self.blobs.windows(2) {
            if pair[0].digest == pair[1].digest {
                return Err(ArtifactError::PresetMetadata(format!(
                    "the package stores `{}` more than once",
                    pair[0].digest.to_hex()
                )));
            }
        }
        // Every binary has to be stored, and nothing else may be: a binary whose
        // bytes are absent is a package that cannot produce a preset, and a blob
        // nothing names is content a reader would have no reason to verify.
        for binary in &self.binaries {
            if !self.blobs.iter().any(|blob| blob.digest == binary.sha256) {
                return Err(ArtifactError::PresetMetadata(format!(
                    "the package names `{}` for {} but stores no such binary",
                    binary.sha256.to_hex(),
                    binary.target.as_str()
                )));
            }
        }
        for blob in &self.blobs {
            if !self
                .binaries
                .iter()
                .any(|binary| binary.sha256 == blob.digest)
            {
                return Err(ArtifactError::PresetMetadata(format!(
                    "the package stores `{}` for no target",
                    blob.digest.to_hex()
                )));
            }
        }
        if self
            .binaries
            .iter()
            .any(|binary| binary.size > MAX_BINARY_BYTES)
        {
            return Err(ArtifactError::BinaryTooLarge {
                size: self
                    .binaries
                    .iter()
                    .map(|binary| binary.size)
                    .max()
                    .unwrap_or_default(),
                limit: MAX_BINARY_BYTES,
            });
        }
        Ok(())
    }

    /// Whether this preset can be presented by a host speaking `protocol`.
    ///
    /// The wire protocol is a single version rather than a negotiation, so this is
    /// equality. It lives here rather than in the consumer because a package that
    /// cannot state its own compatibility would leave every consumer to guess.
    pub fn is_compatible_with(&self, protocol: u32) -> bool {
        self.ui_protocol == protocol
    }

    /// What a host would have to provide for this preset to work.
    pub fn required_capabilities(&self) -> &UiCapabilities {
        &self.required_capabilities
    }

    /// Capability names this preset needs that `provided` does not have.
    pub fn missing_capabilities(&self, provided: &UiCapabilities) -> Vec<&'static str> {
        provided.missing(&self.required_capabilities)
    }

    /// The wire protocol this package's build speaks.
    pub fn wire_protocol(&self) -> u32 {
        self.ui_protocol
    }

    /// The wire protocol this crate speaks, for a consumer comparing against it.
    pub fn current_wire_protocol() -> u32 {
        UI_PROTOCOL_VERSION
    }
}

/// The fixed header every package opens with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub schema: u32,
    pub metadata_bytes: u64,
    pub blob_count: u32,
    pub blobs_bytes: u64,
}

impl Header {
    pub fn encode(&self) -> [u8; HEADER_BYTES] {
        let mut bytes = [0u8; HEADER_BYTES];
        bytes[0..4].copy_from_slice(&MAGIC);
        bytes[4..8].copy_from_slice(&self.schema.to_le_bytes());
        bytes[8..16].copy_from_slice(&self.metadata_bytes.to_le_bytes());
        bytes[16..20].copy_from_slice(&self.blob_count.to_le_bytes());
        bytes[20..24].copy_from_slice(&0u32.to_le_bytes());
        bytes[24..32].copy_from_slice(&self.blobs_bytes.to_le_bytes());
        bytes
    }

    /// Read a header, refusing anything that is not the start of a package.
    pub fn parse(bytes: &[u8]) -> Result<Self, ArtifactError> {
        if bytes.len() < HEADER_BYTES {
            return Err(ArtifactError::PresetTruncated {
                expected: HEADER_BYTES as u64,
                found: bytes.len() as u64,
            });
        }
        if bytes[0..4] != MAGIC {
            return Err(ArtifactError::PresetMagic {
                found: describe_magic(&bytes[0..4]),
            });
        }
        let schema = u32::from_le_bytes(bytes[4..8].try_into().expect("four bytes"));
        if schema != PACKAGE_SCHEMA {
            return Err(ArtifactError::PresetSchema {
                expected: PACKAGE_SCHEMA,
                found: schema,
            });
        }
        let metadata_bytes = u64::from_le_bytes(bytes[8..16].try_into().expect("eight bytes"));
        if metadata_bytes > MAX_METADATA_BYTES {
            return Err(ArtifactError::MetadataTooLarge {
                kind: "preset package metadata",
                size: metadata_bytes,
                limit: MAX_METADATA_BYTES,
            });
        }
        let blob_count = u32::from_le_bytes(bytes[16..20].try_into().expect("four bytes"));
        if blob_count as usize > MAX_BINARIES {
            return Err(ArtifactError::TooManyPresetBinaries {
                count: blob_count as usize,
                limit: MAX_BINARIES,
            });
        }
        let blobs_bytes = u64::from_le_bytes(bytes[24..32].try_into().expect("eight bytes"));
        if blobs_bytes > MAX_TOTAL_BINARY_BYTES {
            return Err(ArtifactError::BinaryTooLarge {
                size: blobs_bytes,
                limit: MAX_TOTAL_BINARY_BYTES,
            });
        }
        Ok(Self {
            schema,
            metadata_bytes,
            blob_count,
            blobs_bytes,
        })
    }
}

/// What a file's first four bytes say, in the form a person can act on.
///
/// Usually this is the first four characters of some other format's magic, and
/// naming the file the caller actually has is more use to them than four numbers.
fn describe_magic(magic: &[u8]) -> String {
    let readable = magic
        .iter()
        .all(|byte| byte.is_ascii_graphic() || *byte == b' ');
    if readable {
        String::from_utf8_lossy(magic).into_owned()
    } else {
        format!("{magic:02x?}")
    }
}

/// Canonical JSON for the metadata document.
///
/// A package's identity is its metadata digest, so the document has to be the
/// same bytes for the same logical package on any machine.
pub fn encode_metadata(package: &PresetPackage) -> Result<Vec<u8>, ArtifactError> {
    let bytes = serde_json::to_vec(package)?;
    if bytes.len() as u64 > MAX_METADATA_BYTES {
        return Err(ArtifactError::MetadataTooLarge {
            kind: "preset package metadata",
            size: bytes.len() as u64,
            limit: MAX_METADATA_BYTES,
        });
    }
    Ok(bytes)
}

/// Parse a metadata document after checking it against its bound.
pub fn decode_metadata(bytes: &[u8]) -> Result<PresetPackage, ArtifactError> {
    if bytes.len() as u64 > MAX_METADATA_BYTES {
        return Err(ArtifactError::MetadataTooLarge {
            kind: "preset package metadata",
            size: bytes.len() as u64,
            limit: MAX_METADATA_BYTES,
        });
    }
    let package: PresetPackage = serde_json::from_slice(bytes)?;
    package.validate()?;
    Ok(package)
}
