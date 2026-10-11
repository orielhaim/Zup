use serde::{Deserialize, Serialize};
use zup_core::{Sha256Digest, TargetTriple};
use zup_preset_protocol::{Capabilities, Capability, PRESET_PROTOCOL_VERSION, PresetDescription};

use crate::format::ArtifactError;

pub const MAGIC: [u8; 4] = *b"ZPUI";

pub const PACKAGE_SCHEMA: u32 = 1;

pub const HEADER_BYTES: usize = 32;

pub const MAX_METADATA_BYTES: u64 = 4 * 1024 * 1024;

pub const MAX_BINARIES: usize = 64;

pub const MAX_BINARY_BYTES: u64 = 256 * 1024 * 1024;

pub const MAX_TOTAL_BINARY_BYTES: u64 = 1024 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PresetBinary {
    pub target: TargetTriple,
    pub size: u64,
    pub sha256: Sha256Digest,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PresetBlob {
    pub digest: Sha256Digest,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PresetPackage {
    pub schema: u32,
    pub name: String,
    pub version: semver::Version,
    pub ui_protocol: u32,
    pub required_capabilities: Capabilities,
    pub settings_schema: serde_json::Value,
    pub binaries: Vec<PresetBinary>,
    pub blobs: Vec<PresetBlob>,
}

impl PresetPackage {
    pub fn binary_for(&self, target: &TargetTriple) -> Result<&PresetBinary, ArtifactError> {
        self.binaries
            .iter()
            .find(|binary| &binary.target == target)
            .ok_or(ArtifactError::PresetTargetUnavailable {
                target: target.as_str().to_owned(),
                available: self.targets().join(", "),
            })
    }

    pub fn targets(&self) -> Vec<&str> {
        self.binaries
            .iter()
            .map(|binary| binary.target.as_str())
            .collect()
    }

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

    pub fn is_compatible_with(&self, protocol: u32) -> bool {
        self.ui_protocol == protocol
    }

    pub fn required_capabilities(&self) -> &Capabilities {
        &self.required_capabilities
    }

    pub fn missing_capabilities(&self, provided: &Capabilities) -> Vec<&'static str> {
        provided.missing(&self.required_capabilities)
    }

    pub fn wire_protocol(&self) -> u32 {
        self.ui_protocol
    }

    pub fn current_wire_protocol() -> u32 {
        PRESET_PROTOCOL_VERSION
    }
}

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

use std::collections::BTreeMap;

use sha2::{Digest, Sha256};

pub struct Binary {
    pub target: TargetTriple,
    pub bytes: Vec<u8>,
    pub digest: Sha256Digest,
}

/// caller cannot publish a package that was never checked.
pub struct PresetPackageWriter {
    description: PresetDescription,
    binaries: Vec<Binary>,
}

impl PresetPackageWriter {
    pub fn new(description: PresetDescription) -> Result<Self, ArtifactError> {
        description
            .validate()
            .map_err(|error| ArtifactError::PresetMetadata(error.to_string()))?;
        Ok(Self {
            description,
            binaries: Vec::new(),
        })
    }

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

    pub fn targets(&self) -> Vec<&str> {
        self.binaries
            .iter()
            .map(|binary| binary.target.as_str())
            .collect()
    }

    pub fn finish(mut self) -> Result<Vec<u8>, ArtifactError> {
        self.binaries
            .sort_by(|left, right| left.target.as_str().cmp(right.target.as_str()));

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
            schema: PACKAGE_SCHEMA,
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

        // consumer would use, so nothing can be published that was never read.
        let view = PresetPackageView::open(bytes.clone())?;
        view.verify()?;
        Ok(bytes)
    }
}

#[derive(Debug)]
pub struct PresetPackageView {
    package: PresetPackage,
    stored: Vec<Vec<u8>>,
}

impl PresetPackageView {
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
                        limit: MAX_TOTAL_BINARY_BYTES,
                    })? as usize;
            stored.push(bytes[cursor..end].to_vec());
            cursor = end;
        }

        Ok(Self { package, stored })
    }

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

    pub fn targets(&self) -> Vec<&str> {
        self.package.targets()
    }

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

    pub fn verify(&self) -> Result<(), ArtifactError> {
        for binary in &self.package.binaries {
            self.binary_for(&binary.target)?;
        }
        Ok(())
    }

    fn stored_for(&self, digest: Sha256Digest) -> Option<&[u8]> {
        let index = self
            .package
            .blobs
            .iter()
            .position(|blob| blob.digest == digest)?;
        self.stored.get(index).map(Vec::as_slice)
    }
}

use zup_core::Installer;

pub fn offers(installer: &Installer) -> Capabilities {
    let mut capabilities = Capabilities::new([
        Capability::Diagnostics,
        Capability::PlanPreview,
        Capability::InstallDirectory,
    ]);
    if !installer.components.is_empty() {
        capabilities = capabilities.with(Capability::Components);
    }
    if installer.updates.is_some() {
        capabilities = capabilities.with(Capability::Updates);
    }
    if !installer.launchers.is_empty() {
        capabilities = capabilities.with(Capability::Launch);
    }
    capabilities.with(Capability::Maintenance)
}

pub fn offers_for(installer: &Installer, maintenance: bool) -> Capabilities {
    if maintenance {
        offers(installer)
    } else {
        offers(installer).without(Capability::Maintenance)
    }
}
