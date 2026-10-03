//! Writing and reading a `.zupui`.
//!
//! Writing compresses and measures; reading verifies. A reader never trusts a
//! declared size or digest: it decompresses the bytes, measures them, hashes
//! them, and only then hands back something a caller can execute. Everything
//! that could be wrong with a package is wrong here rather than in the consumer
//! that would otherwise be the first to find out.

use std::collections::BTreeMap;

use sha2::{Digest, Sha256};
use zup_core::{Sha256Digest, TargetTriple};
use zup_preset_protocol::{PresetDescription, Capabilities};

use crate::error::ArtifactError;

use super::format::{
    HEADER_BYTES, Header, MAX_BINARIES, PresetBinary, PresetBlob, PresetPackage, decode_metadata,
    encode_metadata,
};

/// One native binary, as it is handed to the writer.
pub struct Binary {
    pub target: TargetTriple,
    pub bytes: Vec<u8>,
    pub digest: Sha256Digest,
}

/// A package under construction.
///
/// Built by `add_binary` and only readable once `finish` has verified it, so a
/// caller cannot publish a package that was never checked.
pub struct PresetPackageWriter {
    description: PresetDescription,
    binaries: Vec<Binary>,
}

impl PresetPackageWriter {
    /// Start a package from what the preset's own describe mode reported.
    pub fn new(description: PresetDescription) -> Result<Self, ArtifactError> {
        description
            .validate()
            .map_err(|error| ArtifactError::PresetMetadata(error.to_string()))?;
        Ok(Self {
            description,
            binaries: Vec::new(),
        })
    }

    /// Add one target's binary.
    pub fn add_binary(
        &mut self,
        target: TargetTriple,
        bytes: Vec<u8>,
    ) -> Result<(), ArtifactError> {
        if self.binaries.len() >= MAX_BINARIES {
            return Err(ArtifactError::TooManyPresetBinaries {
                count: self.binaries.len() + 1,
                limit: MAX_BINARIES,
            });
        }
        if self.binaries.iter().any(|binary| binary.target == target) {
            return Err(ArtifactError::PresetMetadata(format!(
                "the package already carries a binary for `{}`",
                target.as_str()
            )));
        }
        self.binaries.push(Binary {
            target,
            digest: Sha256Digest::from_bytes(Sha256::digest(&bytes).into()),
            bytes,
        });
        Ok(())
    }

    /// The targets added so far, so a caller can report progress.
    pub fn targets(&self) -> Vec<&str> {
        self.binaries
            .iter()
            .map(|binary| binary.target.as_str())
            .collect()
    }

    /// Compress, order, and encode the package, then read it back and verify it.
    ///
    /// The verification is not a formality. A package is published to other
    /// machines, and a writer that trusted its own output would ship the first
    /// corrupted package nobody had checked.
    pub fn finish(mut self) -> Result<Vec<u8>, ArtifactError> {
        self.binaries
            .sort_by(|left, right| left.target.as_str().cmp(right.target.as_str()));

        // Compressed once per distinct binary, keyed and ordered by digest. Two
        // targets can be built from the same bytes - a runner that cross-compiles
        // nothing in particular, or a publisher that fills in a target with the
        // one binary it has - and storing them twice would make the table's keys
        // ambiguous about which stored bytes belong to which entry.
        let mut stored: BTreeMap<Sha256Digest, Vec<u8>> = BTreeMap::new();
        let mut binaries = Vec::with_capacity(self.binaries.len());
        for binary in &self.binaries {
            stored
                .entry(binary.digest)
                .or_insert_with(|| zstd::encode_all(binary.bytes.as_slice(), 19).expect("memory"));
            binaries.push(PresetBinary {
                target: binary.target.clone(),
                size: binary.bytes.len() as u64,
                sha256: binary.digest,
            });
        }
        let blobs = stored
            .iter()
            .map(|(digest, bytes)| PresetBlob {
                digest: *digest,
                size: bytes.len() as u64,
            })
            .collect();

        let package = PresetPackage {
            schema: super::format::PACKAGE_SCHEMA,
            name: self.description.name.clone(),
            version: self
                .description
                .version
                .parse::<semver::Version>()
                .map_err(|_| {
                    ArtifactError::PresetMetadata(format!(
                        "`{}` is not a version this package can state",
                        self.description.version
                    ))
                })?,
            ui_protocol: self.description.ui_protocol,
            required_capabilities: self.description.required_capabilities.clone(),
            settings_schema: self.description.settings_schema.clone(),
            binaries,
            blobs,
        };
        package.validate()?;

        let metadata = encode_metadata(&package)?;
        let stored: Vec<u8> = stored.values().flatten().copied().collect();

        let header = Header {
            schema: package.schema,
            metadata_bytes: metadata.len() as u64,
            blob_count: package.blobs.len() as u32,
            blobs_bytes: stored.len() as u64,
        };
        let mut bytes = Vec::with_capacity(HEADER_BYTES + metadata.len() + stored.len());
        bytes.extend_from_slice(&header.encode());
        bytes.extend_from_slice(&metadata);
        bytes.extend_from_slice(&stored);

        // A package that was just written is read back through the same reader a
        // consumer would use, so nothing can be published that was never read.
        let view = PresetPackageView::open(bytes.clone())?;
        view.verify()?;
        Ok(bytes)
    }
}

/// A `.zupui` read into memory, with every field checked.
#[derive(Debug)]
pub struct PresetPackageView {
    package: PresetPackage,
    stored: Vec<Vec<u8>>,
}

impl PresetPackageView {
    /// Read a package, verifying its structure and its content.
    pub fn open(bytes: Vec<u8>) -> Result<Self, ArtifactError> {
        let header = Header::parse(&bytes)?;
        let metadata_end = (HEADER_BYTES as u64)
            .checked_add(header.metadata_bytes)
            .ok_or(ArtifactError::PresetTruncated {
                expected: u64::MAX,
                found: 0,
            })?;
        let total = (metadata_end + header.blobs_bytes) as usize;
        if bytes.len() != total {
            return Err(if bytes.len() < total {
                ArtifactError::PresetTruncated {
                    expected: total as u64,
                    found: bytes.len() as u64,
                }
            } else {
                ArtifactError::PresetTrailing {
                    expected: total as u64,
                    found: bytes.len() as u64,
                }
            });
        }

        let metadata = &bytes[HEADER_BYTES..metadata_end as usize];
        let package = decode_metadata(metadata)?;
        if package.blobs.len() != header.blob_count as usize {
            return Err(ArtifactError::PresetMetadata(format!(
                "the header names {} binaries and the document {}",
                header.blob_count,
                package.blobs.len()
            )));
        }

        let mut stored = Vec::with_capacity(package.blobs.len());
        let mut cursor = metadata_end as usize;
        for blob in &package.blobs {
            let end =
                (cursor as u64)
                    .checked_add(blob.size)
                    .ok_or(ArtifactError::BinaryTooLarge {
                        size: u64::MAX,
                        limit: super::format::MAX_TOTAL_BINARY_BYTES,
                    })? as usize;
            stored.push(bytes[cursor..end].to_vec());
            cursor = end;
        }

        Ok(Self { package, stored })
    }

    /// The package's description, without its binaries.
    pub fn package(&self) -> &PresetPackage {
        &self.package
    }

    pub fn name(&self) -> &str {
        &self.package.name
    }

    pub fn version(&self) -> &semver::Version {
        &self.package.version
    }

    pub fn required_capabilities(&self) -> &Capabilities {
        &self.package.required_capabilities
    }

    pub fn wire_protocol(&self) -> u32 {
        self.package.ui_protocol
    }

    /// The targets this package carries.
    pub fn targets(&self) -> Vec<&str> {
        self.package.targets()
    }

    /// Decompress and verify the binary for one target.
    pub fn binary_for(&self, target: &TargetTriple) -> Result<Vec<u8>, ArtifactError> {
        let entry = self.package.binary_for(target)?;
        let stored = self.stored_for(entry.sha256).ok_or_else(|| {
            ArtifactError::PresetMetadata(format!(
                "the package stores no binary for `{}`",
                entry.sha256.to_hex()
            ))
        })?;
        let bytes = zstd::decode_all(stored).map_err(|error| ArtifactError::PresetCorrupt {
            target: entry.target.as_str().to_owned(),
            digest: entry.sha256.to_hex(),
            reason: error.to_string(),
        })?;
        if bytes.len() as u64 != entry.size {
            return Err(ArtifactError::SizeMismatch {
                media_type: "preset binary",
                digest: entry.sha256.to_hex(),
                expected: entry.size,
                found: bytes.len() as u64,
            });
        }
        let found = Sha256Digest::from_bytes(sha2::Sha256::digest(&bytes).into());
        if found != entry.sha256 {
            return Err(ArtifactError::DigestMismatch {
                media_type: "preset binary",
                digest: entry.sha256.to_hex(),
            });
        }
        Ok(bytes)
    }

    /// Decompress and verify every binary the package carries.
    pub fn verify(&self) -> Result<(), ArtifactError> {
        for binary in &self.package.binaries {
            self.binary_for(&binary.target)?;
        }
        Ok(())
    }

    /// The stored bytes of one binary, before they are decompressed.
    fn stored_for(&self, digest: Sha256Digest) -> Option<&[u8]> {
        let index = self
            .package
            .blobs
            .iter()
            .position(|blob| blob.digest == digest)?;
        self.stored.get(index).map(Vec::as_slice)
    }
}
