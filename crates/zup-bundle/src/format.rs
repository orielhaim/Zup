//! Portable, content-addressed package format.
//!
//! A package is a schema-1 header followed by a SHA-256-protected JSON plan and
//! Zstandard-compressed blobs. Blob offsets are relative to the data region and
//! blobs are ordered by digest. The package contains one target plan and does
//! not depend on an executable or an operating-system container format.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::{BufReader, Cursor, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use zup_core::{BuildPlan, ResolvedFile, ResolvedPrerequisite, TargetBuildPlan};

/// The largest a plugin module may be, at build time and in a bundle.
///
/// A limit on the format, not on a phase: the runtime never compiles a plugin,
/// but it does have to refuse a bundle claiming a module larger than any build
/// could have produced, because such a bundle is not one this writer wrote.
pub const MAX_PLUGIN_SOURCE_BYTES: u64 = 16 * 1024 * 1024;
use zup_core::{
    ComponentId, Condition, Installer, MAX_PLUGIN_ARTIFACTS, PluginId, PrerequisiteId,
    RelativePath, Sha256Digest, TargetTriple, Template, hash_reader,
};
use zup_plugin_contract::{
    AOT_FORMAT_VERSION, MAX_AOT_BYTES, PLUGIN_API_VERSION, WASMTIME_VERSION, engine_fingerprint,
    wit_package_digest,
};

/// Current portable package schema.
pub const PACKAGE_SCHEMA: u32 = 1;

/// A package that carries its plan but not its payload.
///
/// This is the **thin runtime**: a native maintenance runtime that knows exactly
/// what it would install and holds none of the bytes, because the bytes come
/// from a verified content-addressed cache the release graph authenticated. It
/// is what makes a thin installer's runtime a few megabytes instead of the whole
/// application, and it is what lets one engine serve an offline artifact (which
/// carries its payload) and an online one (which does not).
///
/// The flag is load-bearing rather than cosmetic. Without it a reader has to
/// assume every named digest is present, and with it a reader that finds no blob
/// index knows it must be handed a source — there is no in-between state in
/// which a plan claims content the package does not have.
pub const PACKAGE_FEATURE_EXTERNAL_PAYLOAD: u64 = 1 << 0;
const HEADER_LEN: u64 = 60;
const MAX_METADATA: u64 = 256 * 1024 * 1024;
const MAX_ENTRIES: usize = 1_000_000;
const MAX_BLOBS: usize = 1_000_000;
const MAX_PREREQUISITES: usize = 256;
/// Maximum aggregate uncompressed plugin AOT bytes in one package.
pub const MAX_PLUGIN_AOT_TOTAL_BYTES: u64 = 256 * 1024 * 1024;
const MAX_PLUGIN_ID_BYTES: usize = 128;
const MAX_TARGET_BYTES: usize = 256;

/// Failures produced by the portable package format and payload reader.
#[derive(Debug, Error)]
pub enum PackageError {
    #[error("package I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("package metadata: {0}")]
    Json(#[from] serde_json::Error),
    #[error("package is truncated, corrupt, unsupported, or has unsafe offsets")]
    Invalid,
    #[error("package index is {size} bytes; the metadata limit is {limit} bytes")]
    MetadataTooLarge { size: u64, limit: u64 },
    #[error("package has {count} unique blobs; the limit is {limit}")]
    TooManyBlobs { count: usize, limit: usize },
    #[error("package has {count} plugin artifacts; the limit is {limit}")]
    TooManyPluginArtifacts { count: usize, limit: usize },
    #[error("aggregate plugin AOT payload is {size} bytes; the limit is {limit} bytes")]
    PluginAotTooLarge { size: u64, limit: u64 },
    #[error("cannot allocate {size} bytes while processing the package")]
    Allocation { size: u64 },
    #[error("package payload verification failed for {0}")]
    Payload(String),
    #[error("package is missing {media_type} {digest}")]
    Missing {
        media_type: &'static str,
        digest: String,
    },
    #[error("target mismatch: expected `{expected}`, found `{found}`")]
    TargetMismatch {
        expected: TargetTriple,
        found: TargetTriple,
    },
    #[error("plugin artifact is invalid: {0}")]
    PluginArtifact(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginArtifact {
    pub plugin_id: PluginId,
    pub source_size: u64,
    pub source_sha256: Sha256Digest,
    pub target: TargetTriple,
    pub wasmtime_version: String,
    pub aot_format_version: u32,
    pub plugin_api_version: String,
    pub wit_digest: Sha256Digest,
    pub engine_fingerprint: Sha256Digest,
    pub aot_size: u64,
    pub aot_sha256: Sha256Digest,
    pub blob: Sha256Digest,
}

impl PluginArtifact {
    pub fn validate(&self) -> Result<(), PackageError> {
        if self.plugin_id.as_str().len() > MAX_PLUGIN_ID_BYTES {
            return Err(PackageError::PluginArtifact(
                "plugin id exceeds the field limit".to_owned(),
            ));
        }
        if self.target.as_str().len() > MAX_TARGET_BYTES {
            return Err(PackageError::PluginArtifact(
                "target exceeds the field limit".to_owned(),
            ));
        }
        if self.wasmtime_version.len() > 32 || self.plugin_api_version.len() > 32 {
            return Err(PackageError::PluginArtifact(
                "contract version exceeds the field limit".to_owned(),
            ));
        }
        if self.source_size == 0 || self.source_size > MAX_PLUGIN_SOURCE_BYTES {
            return Err(PackageError::PluginArtifact(
                "source size exceeds the component limit".to_owned(),
            ));
        }
        if self.aot_size == 0 || self.aot_size > u64::try_from(MAX_AOT_BYTES).unwrap_or(u64::MAX) {
            return Err(PackageError::PluginArtifact(
                "AOT size is outside the component limit".to_owned(),
            ));
        }
        if self.wasmtime_version != WASMTIME_VERSION
            || self.aot_format_version != AOT_FORMAT_VERSION
            || self.plugin_api_version != PLUGIN_API_VERSION
        {
            return Err(PackageError::PluginArtifact(
                "component contract version is not supported".to_owned(),
            ));
        }
        if self.wit_digest != Sha256Digest::from_bytes(wit_package_digest()) {
            return Err(PackageError::PluginArtifact(
                "WIT digest does not match the contract".to_owned(),
            ));
        }
        zup_plugin_contract::PluginEngine::new(self.target.as_str())
            .map_err(|error| PackageError::PluginArtifact(error.to_string()))?;
        if self.engine_fingerprint
            != Sha256Digest::from_bytes(*engine_fingerprint(self.target.as_str()).as_bytes())
        {
            return Err(PackageError::PluginArtifact(
                "engine fingerprint does not match the target".to_owned(),
            ));
        }
        if self.aot_sha256 != self.blob {
            return Err(PackageError::PluginArtifact(
                "AOT and blob digests differ".to_owned(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompiledPluginArtifact {
    metadata: PluginArtifact,
    bytes: Vec<u8>,
}

impl CompiledPluginArtifact {
    pub fn new(metadata: PluginArtifact, bytes: Vec<u8>) -> Result<Self, PackageError> {
        metadata.validate()?;
        if bytes.len() as u64 != metadata.aot_size {
            return Err(PackageError::PluginArtifact(
                "AOT byte length does not match metadata".to_owned(),
            ));
        }
        let digest = Sha256Digest::from_bytes(Sha256::digest(&bytes).into());
        if digest != metadata.aot_sha256 {
            return Err(PackageError::PluginArtifact(
                "AOT digest does not match metadata".to_owned(),
            ));
        }
        Ok(Self { metadata, bytes })
    }

    pub fn metadata(&self) -> &PluginArtifact {
        &self.metadata
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PortableBuildPlan {
    pub installer: Installer,
    pub entries: Vec<PayloadEntry>,
    #[serde(default)]
    pub prerequisite_artifacts: Vec<PrerequisiteArtifact>,
    pub plugins: Vec<PluginArtifact>,
    pub total_size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PayloadEntry {
    pub path: RelativePath,
    pub destination: Template,
    pub size: u64,
    pub sha256: Sha256Digest,
    pub blob: Sha256Digest,
    pub component: Option<ComponentId>,
    pub condition: Option<Condition>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrerequisiteArtifact {
    pub prerequisite_id: PrerequisiteId,
    pub path: RelativePath,
    pub size: u64,
    pub sha256: Sha256Digest,
    pub blob: Sha256Digest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BlobIndex {
    digest: Sha256Digest,
    offset: u64,
    compressed_size: u64,
    size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Metadata {
    schema: u32,
    required_features: u64,
    plan: PortableBuildPlan,
    blobs: Vec<BlobIndex>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageIndex {
    index_size: u64,
    blobs: Vec<IndexedBlob>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct IndexedBlob {
    compressed_size: u64,
    size: u64,
}

impl PackageIndex {
    pub fn index_size(&self) -> u64 {
        self.index_size
    }

    pub fn blob_count(&self) -> usize {
        self.blobs.len()
    }

    pub fn compressed_size(&self, index: usize) -> Option<u64> {
        self.blobs.get(index).map(|blob| blob.compressed_size)
    }

    pub fn size(&self, index: usize) -> Option<u64> {
        self.blobs.get(index).map(|blob| blob.size)
    }
}

#[derive(Debug, Clone)]
enum PackageStorage {
    File(PathBuf),
    Memory(Arc<Vec<u8>>),
}

impl PackageStorage {
    fn read_range(&self, offset: u64, size: u64) -> Result<Vec<u8>, PackageError> {
        let end = offset.checked_add(size).ok_or(PackageError::Invalid)?;
        let size_usize = usize::try_from(size).map_err(|_| PackageError::Invalid)?;
        match self {
            Self::File(path) => {
                let mut file = File::open(path)?;
                if end > file.metadata()?.len() {
                    return Err(PackageError::Invalid);
                }
                file.seek(SeekFrom::Start(offset))?;
                let mut bytes = Vec::new();
                bytes
                    .try_reserve_exact(size_usize)
                    .map_err(|_| PackageError::Allocation { size })?;
                bytes.resize(size_usize, 0);
                file.read_exact(&mut bytes)?;
                Ok(bytes)
            }
            Self::Memory(bytes) => {
                let start = usize::try_from(offset).map_err(|_| PackageError::Invalid)?;
                let end = usize::try_from(end).map_err(|_| PackageError::Invalid)?;
                bytes
                    .get(start..end)
                    .map(<[u8]>::to_vec)
                    .ok_or(PackageError::Invalid)
            }
        }
    }
}

/// A verified standalone package.
#[derive(Debug, Clone)]
pub struct Package {
    storage: PackageStorage,
    metadata: Metadata,
    index_size: u64,
}

impl Package {
    /// Open and verify a package file.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, PackageError> {
        let path = path.as_ref().to_path_buf();
        let len = File::open(&path)?.metadata()?.len();
        let mut file = File::open(&path)?;
        let (metadata, index_size) = parse_metadata(&mut file, Some(len))?;
        let package = Self {
            storage: PackageStorage::File(path),
            metadata,
            index_size,
        };
        package.verify()?;
        Ok(package)
    }

    /// Open package structure without decompressing or hashing blob data.
    pub fn open_unverified(path: impl AsRef<Path>) -> Result<Self, PackageError> {
        let path = path.as_ref().to_path_buf();
        let len = File::open(&path)?.metadata()?.len();
        let mut file = File::open(&path)?;
        let (metadata, index_size) = parse_metadata(&mut file, Some(len))?;
        Ok(Self {
            storage: PackageStorage::File(path),
            metadata,
            index_size,
        })
    }

    /// Parse and verify package bytes.
    pub fn parse(bytes: impl AsRef<[u8]>) -> Result<Self, PackageError> {
        Self::from_bytes(bytes.as_ref().to_vec())
    }

    /// Parse and verify owned package bytes.
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self, PackageError> {
        let len = bytes.len() as u64;
        let mut cursor = Cursor::new(bytes.as_slice());
        let (metadata, index_size) = parse_metadata(&mut cursor, Some(len))?;
        let package = Self {
            storage: PackageStorage::Memory(Arc::new(bytes)),
            metadata,
            index_size,
        };
        package.verify()?;
        Ok(package)
    }

    /// Parse the index portion used by a package adapter.
    pub fn parse_index(bytes: &[u8]) -> Result<PackageIndex, PackageError> {
        if (bytes.len() as u64) < HEADER_LEN {
            return Err(PackageError::Invalid);
        }
        let mut cursor = Cursor::new(bytes);
        let (metadata, index_size) = parse_metadata(&mut cursor, None)?;
        if index_size != bytes.len() as u64 {
            return Err(PackageError::Invalid);
        }
        Ok(PackageIndex {
            index_size,
            blobs: metadata
                .blobs
                .iter()
                .map(|blob| IndexedBlob {
                    compressed_size: blob.compressed_size,
                    size: blob.size,
                })
                .collect(),
        })
    }

    pub fn plan(&self) -> &PortableBuildPlan {
        &self.metadata.plan
    }

    /// Whether this package names content it does not carry.
    ///
    /// A plan-only package is a thin runtime: it can plan, and it must be handed
    /// a content source. `Package::payload_source` on one of these is a bug, so
    /// it returns `None` rather than a source that would always miss.
    pub const fn is_plan_only(&self) -> bool {
        self.metadata.required_features & PACKAGE_FEATURE_EXTERNAL_PAYLOAD != 0
    }

    /// Every digest this package's plan can need, in ascending order.
    ///
    /// This is what an acquisition engine schedules: the exact content set for
    /// this variant, before any component selection narrows it.
    pub fn required_digests(&self) -> Vec<Sha256Digest> {
        let mut digests: Vec<Sha256Digest> = self
            .metadata
            .plan
            .entries
            .iter()
            .map(|entry| entry.blob)
            .chain(
                self.metadata
                    .plan
                    .prerequisite_artifacts
                    .iter()
                    .map(|artifact| artifact.blob),
            )
            .chain(
                self.metadata
                    .plan
                    .plugins
                    .iter()
                    .map(|artifact| artifact.blob),
            )
            .collect();
        digests.sort_unstable();
        digests.dedup();
        digests
    }

    /// The digest each payload path resolves to, as the plan declares it.
    ///
    /// A content source is asked for a path, a digest, and a length, so this is
    /// the map it needs and nothing more: the caller already holds the digest the
    /// plan authenticated, so this cannot be used to substitute content.
    pub fn payload_index(&self) -> BTreeMap<RelativePath, Sha256Digest> {
        self.metadata
            .plan
            .entries
            .iter()
            .map(|entry| (entry.path.clone(), entry.blob))
            .collect()
    }

    pub fn build_plan(&self) -> Result<BuildPlan, PackageError> {
        let files = self
            .metadata
            .plan
            .entries
            .iter()
            .map(|entry| {
                Ok(ResolvedFile {
                    source: PathBuf::new(),
                    source_relative: entry.path.clone(),
                    destination: entry.destination.clone(),
                    size: entry.size,
                    sha256: entry.sha256,
                    component: entry.component.clone(),
                    condition: entry.condition.clone(),
                })
            })
            .collect::<Result<Vec<_>, PackageError>>()?;
        let prerequisites = self
            .metadata
            .plan
            .prerequisite_artifacts
            .iter()
            .map(|artifact| ResolvedPrerequisite {
                id: artifact.prerequisite_id.clone(),
                source: PathBuf::new(),
                source_relative: artifact.path.clone(),
                size: artifact.size,
                sha256: artifact.sha256,
            })
            .collect();
        Ok(BuildPlan {
            targets: vec![TargetBuildPlan {
                installer: self.metadata.plan.installer.clone(),
                prerequisites,
                plugins: Vec::new(),
                files,
                total_size: self.metadata.plan.total_size,
                prerequisite_size: self
                    .metadata
                    .plan
                    .prerequisite_artifacts
                    .iter()
                    .map(|artifact| artifact.size)
                    .sum(),
            }],
        })
    }

    pub fn plugin_artifacts(&self) -> &[PluginArtifact] {
        &self.metadata.plan.plugins
    }

    pub fn plugin_artifact(&self, id: &PluginId) -> Option<&PluginArtifact> {
        self.metadata
            .plan
            .plugins
            .iter()
            .find(|artifact| &artifact.plugin_id == id)
    }

    pub fn plugin_aot(&self, id: &PluginId) -> Result<Vec<u8>, PackageError> {
        if !self
            .metadata
            .plan
            .installer
            .plugins
            .iter()
            .any(|plugin| &plugin.id == id)
        {
            return Err(PackageError::Invalid);
        }
        let artifact = self.plugin_artifact(id).ok_or(PackageError::Invalid)?;
        let blob = self
            .metadata
            .blobs
            .iter()
            .find(|blob| blob.digest == artifact.blob)
            .ok_or(PackageError::Invalid)?;
        let mut decoder = self.open_blob(blob)?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(
                usize::try_from(artifact.aot_size).map_err(|_| PackageError::Invalid)?,
            )
            .map_err(|_| PackageError::Allocation {
                size: artifact.aot_size,
            })?;
        decoder
            .by_ref()
            .take(artifact.aot_size.saturating_add(1))
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 != artifact.aot_size
            || Sha256Digest::from_bytes(Sha256::digest(&bytes).into()) != artifact.aot_sha256
        {
            return Err(PackageError::Payload(artifact.blob.to_string()));
        }
        Ok(bytes)
    }

    pub fn prerequisite_artifact(&self, id: &PrerequisiteId) -> Option<&PrerequisiteArtifact> {
        self.metadata
            .plan
            .prerequisite_artifacts
            .iter()
            .find(|artifact| &artifact.prerequisite_id == id)
    }

    pub fn prerequisite_bytes(&self, id: &PrerequisiteId) -> Result<Vec<u8>, PackageError> {
        let artifact = self
            .prerequisite_artifact(id)
            .ok_or(PackageError::Invalid)?;
        let blob = self
            .metadata
            .blobs
            .iter()
            .find(|blob| blob.digest == artifact.blob)
            .ok_or(PackageError::Invalid)?;
        let decoder = self.open_blob(blob)?;
        let mut bytes = Vec::new();
        decoder
            .take(artifact.size.saturating_add(1))
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 != artifact.size
            || Sha256Digest::from_bytes(Sha256::digest(&bytes).into()) != artifact.sha256
        {
            return Err(PackageError::Payload(artifact.sha256.to_string()));
        }
        Ok(bytes)
    }

    pub fn payload_source(&self) -> PackagePayloadSource {
        PackagePayloadSource {
            storage: self.storage.clone(),
            index_size: self.index_size,
            blobs: self.metadata.blobs.clone(),
            entries: self.metadata.plan.entries.clone(),
        }
    }

    pub fn verify(&self) -> Result<(), PackageError> {
        for blob in &self.metadata.blobs {
            let limit = self
                .metadata
                .plan
                .plugins
                .iter()
                .find(|artifact| artifact.blob == blob.digest)
                .map_or_else(
                    || {
                        self.metadata
                            .plan
                            .prerequisite_artifacts
                            .iter()
                            .find(|artifact| artifact.blob == blob.digest)
                            .map_or(blob.size, |artifact| artifact.size)
                    },
                    |artifact| artifact.aot_size,
                );
            self.verify_blob(blob, limit)?;
        }
        Ok(())
    }

    pub fn index_bytes(&self) -> Result<Vec<u8>, PackageError> {
        self.storage.read_range(0, self.index_size)
    }

    pub fn index_info(&self) -> PackageIndex {
        PackageIndex {
            index_size: self.index_size,
            blobs: self
                .metadata
                .blobs
                .iter()
                .map(|blob| IndexedBlob {
                    compressed_size: blob.compressed_size,
                    size: blob.size,
                })
                .collect(),
        }
    }

    pub fn blob_count(&self) -> usize {
        self.metadata.blobs.len()
    }

    pub fn compressed_blob(&self, index: usize) -> Result<Vec<u8>, PackageError> {
        let blob = self
            .metadata
            .blobs
            .get(index)
            .ok_or(PackageError::Invalid)?;
        self.storage.read_range(
            self.index_size
                .checked_add(blob.offset)
                .ok_or(PackageError::Invalid)?,
            blob.compressed_size,
        )
    }

    fn verify_blob(&self, blob: &BlobIndex, limit: u64) -> Result<(), PackageError> {
        let decoder = self.open_blob(blob)?;
        let (size, digest) = hash_reader(decoder.take(limit.saturating_add(1)))?;
        if size != blob.size || digest != blob.digest || size > limit {
            return Err(PackageError::Payload(blob.digest.to_string()));
        }
        Ok(())
    }

    fn open_blob(
        &self,
        blob: &BlobIndex,
    ) -> Result<zstd::stream::read::Decoder<'static, BufReader<Cursor<Vec<u8>>>>, PackageError>
    {
        let bytes = self.storage.read_range(
            self.index_size
                .checked_add(blob.offset)
                .ok_or(PackageError::Invalid)?,
            blob.compressed_size,
        )?;
        Ok(zstd::stream::read::Decoder::new(Cursor::new(bytes))?)
    }
}

impl crate::PayloadSource for Package {
    fn open(
        &self,
        path: &RelativePath,
        expected_sha256: &Sha256Digest,
        expected_size: u64,
    ) -> Result<crate::PayloadReader, crate::PayloadError> {
        self.payload_source()
            .open(path, expected_sha256, expected_size)
    }
}

#[derive(Clone)]
pub struct PackagePayloadSource {
    storage: PackageStorage,
    index_size: u64,
    blobs: Vec<BlobIndex>,
    entries: Vec<PayloadEntry>,
}

impl crate::PayloadSource for PackagePayloadSource {
    fn open(
        &self,
        path: &RelativePath,
        expected_sha256: &Sha256Digest,
        expected_size: u64,
    ) -> Result<crate::PayloadReader, crate::PayloadError> {
        let entry = self
            .entries
            .iter()
            .find(|entry| {
                &entry.path == path
                    && &entry.sha256 == expected_sha256
                    && entry.size == expected_size
            })
            .ok_or_else(|| crate::PayloadError::NotFound {
                path: path.to_string(),
            })?;
        let blob = self
            .blobs
            .iter()
            .find(|blob| blob.digest == entry.blob)
            .ok_or_else(|| crate::PayloadError::NotFound {
                path: path.to_string(),
            })?;
        let compressed = self
            .storage
            .read_range(
                self.index_size
                    .checked_add(blob.offset)
                    .ok_or_else(|| std::io::Error::other(PackageError::Invalid))
                    .map_err(|source| crate::PayloadError::Read {
                        path: path.to_string(),
                        source,
                    })?,
                blob.compressed_size,
            )
            .map_err(|error| crate::PayloadError::Read {
                path: path.to_string(),
                source: std::io::Error::other(error.to_string()),
            })?;
        let decoder =
            zstd::stream::read::Decoder::new(Cursor::new(compressed)).map_err(|source| {
                crate::PayloadError::Read {
                    path: path.to_string(),
                    source,
                }
            })?;
        Ok(Box::new(decoder))
    }
}

fn parse_metadata(
    file: &mut (impl Read + Seek),
    package_len: Option<u64>,
) -> Result<(Metadata, u64), PackageError> {
    if package_len.is_some_and(|length| length < HEADER_LEN) {
        return Err(PackageError::Invalid);
    }
    let mut header = [0u8; HEADER_LEN as usize];
    file.seek(SeekFrom::Start(0))?;
    file.read_exact(&mut header)?;
    if &header[..8] != b"ZUPBNDL\0"
        || u32::from_le_bytes(header[8..12].try_into().unwrap()) != PACKAGE_SCHEMA
    {
        return Err(PackageError::Invalid);
    }
    let features = u64::from_le_bytes(header[12..20].try_into().unwrap());
    let external = features & PACKAGE_FEATURE_EXTERNAL_PAYLOAD != 0;
    if features & !PACKAGE_FEATURE_EXTERNAL_PAYLOAD != 0 {
        return Err(PackageError::Invalid);
    }
    let meta_len = u64::from_le_bytes(header[20..28].try_into().unwrap());
    let index_size = HEADER_LEN
        .checked_add(meta_len)
        .ok_or(PackageError::Invalid)?;
    if meta_len > MAX_METADATA {
        return Err(PackageError::MetadataTooLarge {
            size: meta_len,
            limit: MAX_METADATA,
        });
    }
    if package_len.is_some_and(|length| index_size > length) {
        return Err(PackageError::Invalid);
    }
    let meta_size = usize::try_from(meta_len).map_err(|_| PackageError::Invalid)?;
    let mut bytes = vec![0; meta_size];
    file.read_exact(&mut bytes)?;
    if Sha256::digest(&bytes).as_slice() != &header[28..60] {
        return Err(PackageError::Invalid);
    }
    let metadata: Metadata = serde_json::from_slice(&bytes)?;
    if metadata.schema != PACKAGE_SCHEMA
        || metadata.required_features != features
        || metadata.plan.entries.len() > MAX_ENTRIES
        || metadata.plan.prerequisite_artifacts.len() > MAX_PREREQUISITES
        || metadata.plan.plugins.len() > MAX_PLUGIN_ARTIFACTS
        || metadata.blobs.len() > MAX_BLOBS
    {
        return Err(if metadata.plan.plugins.len() > MAX_PLUGIN_ARTIFACTS {
            PackageError::TooManyPluginArtifacts {
                count: metadata.plan.plugins.len(),
                limit: MAX_PLUGIN_ARTIFACTS,
            }
        } else {
            PackageError::Invalid
        });
    }
    validate_plugin_aot_total(metadata.plan.plugins.iter())?;

    let computed_total = metadata.plan.entries.iter().try_fold(0u64, |sum, entry| {
        sum.checked_add(entry.size).ok_or(PackageError::Invalid)
    })?;
    if computed_total != metadata.plan.total_size {
        return Err(PackageError::Invalid);
    }

    let mut entry_keys = BTreeSet::new();
    for entry in &metadata.plan.entries {
        let key = (entry.destination.to_string(), entry.path.clone());
        if !entry_keys.insert(key) {
            return Err(PackageError::Invalid);
        }
    }
    if metadata.plan.entries.windows(2).any(|window| {
        let left = (window[0].destination.to_string(), window[0].path.clone());
        let right = (window[1].destination.to_string(), window[1].path.clone());
        left >= right
    }) {
        return Err(PackageError::Invalid);
    }

    let mut expected_plugins = BTreeMap::<String, PluginId>::new();
    for plugin in &metadata.plan.installer.plugins {
        if expected_plugins
            .insert(plugin.id.as_str().to_ascii_lowercase(), plugin.id.clone())
            .is_some()
        {
            return Err(PackageError::Invalid);
        }
    }
    if expected_plugins.len() != metadata.plan.plugins.len() {
        return Err(PackageError::Invalid);
    }
    let mut artifact_plugins = BTreeMap::<String, &PluginArtifact>::new();
    for artifact in &metadata.plan.plugins {
        validate_plugin_target(&metadata.plan.installer.target, artifact)?;
        artifact.validate()?;
        let key = artifact.plugin_id.as_str().to_ascii_lowercase();
        let Some(expected_id) = expected_plugins.get(&key) else {
            return Err(PackageError::Invalid);
        };
        if artifact.plugin_id != *expected_id || artifact_plugins.insert(key, artifact).is_some() {
            return Err(PackageError::Invalid);
        }
    }
    if metadata
        .plan
        .plugins
        .windows(2)
        .any(|window| window[0].plugin_id >= window[1].plugin_id)
    {
        return Err(PackageError::Invalid);
    }

    let expected_prerequisites: BTreeMap<_, _> = metadata
        .plan
        .installer
        .prerequisites
        .iter()
        .filter_map(|prerequisite| {
            matches!(
                prerequisite.package,
                zup_core::PrerequisitePackage::Embedded { .. }
            )
            .then_some((prerequisite.id.clone(), prerequisite))
        })
        .collect();
    if expected_prerequisites.len() != metadata.plan.prerequisite_artifacts.len()
        || metadata
            .plan
            .prerequisite_artifacts
            .windows(2)
            .any(|window| window[0].prerequisite_id >= window[1].prerequisite_id)
    {
        return Err(PackageError::Invalid);
    }
    for artifact in &metadata.plan.prerequisite_artifacts {
        let Some(prerequisite) = expected_prerequisites.get(&artifact.prerequisite_id) else {
            return Err(PackageError::Invalid);
        };
        let zup_core::PrerequisitePackage::Embedded {
            sha256,
            size,
            path: expected_path,
        } = &prerequisite.package
        else {
            return Err(PackageError::Invalid);
        };
        if artifact.sha256 != *sha256
            || artifact.size != *size
            || artifact.path != *expected_path
            || artifact.blob != artifact.sha256
        {
            return Err(PackageError::Invalid);
        }
    }

    let mut previous = 0u64;
    let mut by_digest = BTreeMap::new();
    for (index, blob) in metadata.blobs.iter().enumerate() {
        let end = blob
            .offset
            .checked_add(blob.compressed_size)
            .ok_or(PackageError::Invalid)?;
        if blob.offset != previous
            || blob.compressed_size == 0
            || by_digest.insert(blob.digest, blob.size).is_some()
            || (index > 0 && metadata.blobs[index - 1].digest >= blob.digest)
        {
            return Err(PackageError::Invalid);
        }
        if let Some(length) = package_len {
            let data_len = length
                .checked_sub(index_size)
                .ok_or(PackageError::Invalid)?;
            if end > data_len {
                return Err(PackageError::Invalid);
            }
        }
        previous = end;
    }
    if let Some(length) = package_len {
        let data_len = length
            .checked_sub(index_size)
            .ok_or(PackageError::Invalid)?;
        if previous != data_len {
            return Err(PackageError::Invalid);
        }
    }

    let mut referenced = BTreeSet::new();
    for entry in &metadata.plan.entries {
        if entry.blob != entry.sha256 {
            return Err(PackageError::Invalid);
        }
        // A plan-only package names content it does not hold, so there is
        // nothing to check the size against here. What proves those bytes is the
        // release graph, and what proves them again at install time is the
        // content source's own digest check.
        if !external && by_digest.get(&entry.blob) != Some(&entry.size) {
            return Err(PackageError::Invalid);
        }
        referenced.insert(entry.blob);
    }
    for artifact in &metadata.plan.prerequisite_artifacts {
        if artifact.blob != artifact.sha256 {
            return Err(PackageError::Invalid);
        }
        if !external && by_digest.get(&artifact.blob) != Some(&artifact.size) {
            return Err(PackageError::Invalid);
        }
        referenced.insert(artifact.blob);
    }
    for artifact in &metadata.plan.plugins {
        if artifact.blob != artifact.aot_sha256 {
            return Err(PackageError::Invalid);
        }
        if !external && by_digest.get(&artifact.blob) != Some(&artifact.aot_size) {
            return Err(PackageError::Invalid);
        }
        referenced.insert(artifact.blob);
    }
    // An external-payload package carries no blob index at all. A partial one —
    // some content in the plan, some outside it — is the state that would let a
    // reader believe it has a self-contained copy of something it does not.
    if external {
        if !metadata.blobs.is_empty() {
            return Err(PackageError::Invalid);
        }
    } else if referenced.len() != metadata.blobs.len() {
        return Err(PackageError::Invalid);
    }
    Ok((metadata, index_size))
}

fn validate_plugin_target(
    expected: &TargetTriple,
    artifact: &PluginArtifact,
) -> Result<(), PackageError> {
    if artifact.target != *expected {
        return Err(PackageError::TargetMismatch {
            expected: expected.clone(),
            found: artifact.target.clone(),
        });
    }
    Ok(())
}

fn validate_plugin_aot_total<'a>(
    mut artifacts: impl Iterator<Item = &'a PluginArtifact>,
) -> Result<u64, PackageError> {
    let total = artifacts.try_fold(0u64, |total, artifact| {
        total
            .checked_add(artifact.aot_size)
            .ok_or(PackageError::PluginAotTooLarge {
                size: u64::MAX,
                limit: MAX_PLUGIN_AOT_TOTAL_BYTES,
            })
    })?;
    if total > MAX_PLUGIN_AOT_TOTAL_BYTES {
        return Err(PackageError::PluginAotTooLarge {
            size: total,
            limit: MAX_PLUGIN_AOT_TOTAL_BYTES,
        });
    }
    Ok(total)
}

fn canonical_artifacts(
    plan: &TargetBuildPlan,
    artifacts: &[CompiledPluginArtifact],
) -> Result<Vec<CompiledPluginArtifact>, PackageError> {
    let count = artifacts.len().max(plan.installer.plugins.len());
    if count > MAX_PLUGIN_ARTIFACTS {
        return Err(PackageError::TooManyPluginArtifacts {
            count,
            limit: MAX_PLUGIN_ARTIFACTS,
        });
    }
    if artifacts.len() != plan.installer.plugins.len() {
        return Err(PackageError::Invalid);
    }
    validate_plugin_aot_total(artifacts.iter().map(|artifact| &artifact.metadata))?;
    let mut expected = BTreeMap::<String, PluginId>::new();
    for plugin in &plan.installer.plugins {
        if expected
            .insert(plugin.id.as_str().to_ascii_lowercase(), plugin.id.clone())
            .is_some()
        {
            return Err(PackageError::Invalid);
        }
    }
    let mut canonical = artifacts.to_vec();
    canonical.sort_by(|a, b| a.metadata.plugin_id.cmp(&b.metadata.plugin_id));
    let mut actual = BTreeSet::<String>::new();
    for artifact in &canonical {
        validate_plugin_target(&plan.installer.target, &artifact.metadata)?;
        artifact.metadata.validate()?;
        let key = artifact.metadata.plugin_id.as_str().to_ascii_lowercase();
        let Some(expected_id) = expected.get(&key) else {
            return Err(PackageError::Invalid);
        };
        if artifact.metadata.plugin_id != *expected_id || !actual.insert(key) {
            return Err(PackageError::Invalid);
        }
    }
    validate_plugin_aot_total(canonical.iter().map(|artifact| &artifact.metadata))?;
    if !plan.plugins.is_empty() {
        if plan.plugins.len() != plan.installer.plugins.len() {
            return Err(PackageError::Invalid);
        }
        for (resolved, binding) in plan.plugins.iter().zip(&plan.installer.plugins) {
            if resolved.id != binding.id {
                return Err(PackageError::Invalid);
            }
            let Some(artifact) = canonical
                .iter()
                .find(|artifact| artifact.metadata.plugin_id == resolved.id)
            else {
                return Err(PackageError::Invalid);
            };
            if artifact.metadata.source_size != resolved.size
                || artifact.metadata.source_sha256 != resolved.sha256
            {
                return Err(PackageError::Invalid);
            }
        }
    }
    Ok(canonical)
}

fn canonical_prerequisites(
    plan: &TargetBuildPlan,
) -> Result<Vec<PrerequisiteArtifact>, PackageError> {
    let mut resolved: Vec<&ResolvedPrerequisite> = plan.prerequisites.iter().collect();
    resolved.sort_by(|left, right| left.id.cmp(&right.id));
    let mut seen = BTreeSet::new();
    let mut out = Vec::with_capacity(resolved.len());
    for prerequisite in resolved {
        if !seen.insert(prerequisite.id.clone()) {
            return Err(PackageError::Invalid);
        }
        let Some(declaration) = plan
            .installer
            .prerequisites
            .iter()
            .find(|item| item.id == prerequisite.id)
        else {
            return Err(PackageError::Invalid);
        };
        let zup_core::PrerequisitePackage::Embedded { sha256, size, .. } = &declaration.package
        else {
            return Err(PackageError::Invalid);
        };
        if *sha256 != prerequisite.sha256 || *size != prerequisite.size {
            return Err(PackageError::Invalid);
        }
        out.push(PrerequisiteArtifact {
            prerequisite_id: prerequisite.id.clone(),
            path: prerequisite.source_relative.clone(),
            size: prerequisite.size,
            sha256: prerequisite.sha256,
            blob: prerequisite.sha256,
        });
    }
    Ok(out)
}

#[derive(Debug)]
pub struct BundleWriter;

/// Read a build-machine source and prove it is what the plan says it is.
///
/// Returns the size and digest it found, so a caller that has already trusted
/// the plan still cannot publish a plan whose files are missing or changed.
fn verify_source(
    source: &Path,
    expected_size: u64,
    expected_digest: Sha256Digest,
) -> Result<(u64, Sha256Digest), PackageError> {
    let bytes =
        std::fs::read(source).map_err(|_| PackageError::Payload(source.display().to_string()))?;
    let digest = Sha256Digest::from_bytes(Sha256::digest(&bytes).into());
    if bytes.len() as u64 != expected_size || digest != expected_digest {
        return Err(PackageError::Payload(source.display().to_string()));
    }
    Ok((bytes.len() as u64, digest))
}

impl BundleWriter {
    /// Produce deterministic package bytes. Each unique digest is compressed
    /// once and all blob offsets are relative to the data region.
    pub fn encode(
        plan: &TargetBuildPlan,
        artifacts: &[CompiledPluginArtifact],
    ) -> Result<Vec<u8>, PackageError> {
        let artifacts = canonical_artifacts(plan, artifacts)?;
        let mut installer = plan.installer.clone();
        for mapping in &mut installer.files {
            mapping.source = "embedded".to_owned();
        }
        let mut entries = Vec::with_capacity(plan.files.len());
        let mut contents = BTreeMap::<Sha256Digest, Vec<u8>>::new();
        for file in &plan.files {
            let bytes = std::fs::read(&file.source)?;
            if bytes.len() as u64 != file.size
                || Sha256Digest::from_bytes(Sha256::digest(&bytes).into()) != file.sha256
            {
                return Err(PackageError::Payload(file.source.display().to_string()));
            }
            contents.entry(file.sha256).or_insert(bytes);
            entries.push(PayloadEntry {
                path: file.source_relative.clone(),
                destination: file.destination.clone(),
                size: file.size,
                sha256: file.sha256,
                blob: file.sha256,
                component: file.component.clone(),
                condition: file.condition.clone(),
            });
        }
        let prerequisite_artifacts = canonical_prerequisites(plan)?;
        for prerequisite in &plan.prerequisites {
            let bytes = std::fs::read(&prerequisite.source)?;
            if bytes.len() as u64 != prerequisite.size
                || Sha256Digest::from_bytes(Sha256::digest(&bytes).into()) != prerequisite.sha256
            {
                return Err(PackageError::Payload(
                    prerequisite.source.display().to_string(),
                ));
            }
            if let Some(existing) = contents.get(&prerequisite.sha256) {
                if existing.as_slice() != bytes.as_slice() {
                    return Err(PackageError::Payload(
                        prerequisite.source.display().to_string(),
                    ));
                }
            } else {
                contents.insert(prerequisite.sha256, bytes);
            }
        }
        for artifact in &artifacts {
            let digest = artifact.metadata.blob;
            if let Some(existing) = contents.get(&digest) {
                if existing.as_slice() != artifact.bytes.as_slice() {
                    return Err(PackageError::PluginArtifact(
                        "digest collision in plugin artifacts".to_owned(),
                    ));
                }
            } else {
                contents.insert(digest, artifact.bytes.clone());
            }
        }
        entries.sort_by(|a, b| {
            a.destination
                .to_string()
                .cmp(&b.destination.to_string())
                .then(a.path.cmp(&b.path))
        });
        let total_size = entries.iter().try_fold(0u64, |sum, entry| {
            sum.checked_add(entry.size).ok_or(PackageError::Invalid)
        })?;
        if contents.len() > MAX_BLOBS {
            return Err(PackageError::TooManyBlobs {
                count: contents.len(),
                limit: MAX_BLOBS,
            });
        }
        let mut compressed = Vec::new();
        let mut blobs = Vec::with_capacity(contents.len());
        for (digest, bytes) in contents {
            let encoded = zstd::stream::encode_all(Cursor::new(&bytes), 9)?;
            let offset = compressed.len() as u64;
            blobs.push(BlobIndex {
                digest,
                offset,
                compressed_size: encoded.len() as u64,
                size: bytes.len() as u64,
            });
            compressed.extend_from_slice(&encoded);
        }
        let portable_plan = PortableBuildPlan {
            installer,
            entries,
            prerequisite_artifacts,
            plugins: artifacts
                .into_iter()
                .map(|artifact| artifact.metadata)
                .collect(),
            total_size,
        };
        let metadata = Metadata {
            schema: PACKAGE_SCHEMA,
            required_features: 0,
            plan: portable_plan,
            blobs,
        };
        let meta = serde_json::to_vec(&metadata)?;
        if meta.len() as u64 > MAX_METADATA {
            return Err(PackageError::MetadataTooLarge {
                size: meta.len() as u64,
                limit: MAX_METADATA,
            });
        }
        let mut out = Vec::with_capacity(HEADER_LEN as usize + meta.len() + compressed.len());
        out.extend_from_slice(b"ZUPBNDL\0");
        out.extend_from_slice(&PACKAGE_SCHEMA.to_le_bytes());
        out.extend_from_slice(&0u64.to_le_bytes());
        out.extend_from_slice(&(meta.len() as u64).to_le_bytes());
        out.extend_from_slice(&Sha256::digest(&meta));
        out.extend_from_slice(&meta);
        out.extend_from_slice(&compressed);
        Ok(out)
    }

    /// Produce a package that carries its plan and none of its payload.
    ///
    /// This is the thin runtime image. The plan is canonical and complete — the
    /// application identity, the component definitions, the file destinations,
    /// the prerequisites, the plugin bindings — so the runtime can plan and
    /// execute a lifecycle with no manifest and no other architecture's bytes.
    /// The content itself is named by digest and comes from a verified cache.
    ///
    /// Nothing is read from the build machine except to *prove* the plan: each
    /// declared file is checked against its own size and digest, so a plan whose
    /// files do not exist is refused here rather than at install time.
    pub fn encode_plan_only(
        plan: &TargetBuildPlan,
        artifacts: &[CompiledPluginArtifact],
    ) -> Result<Vec<u8>, PackageError> {
        let artifacts = canonical_artifacts(plan, artifacts)?;
        let mut installer = plan.installer.clone();
        for mapping in &mut installer.files {
            mapping.source = "embedded".to_owned();
        }
        let mut entries = Vec::with_capacity(plan.files.len());
        for file in &plan.files {
            let (size, digest) = verify_source(&file.source, file.size, file.sha256)?;
            entries.push(PayloadEntry {
                path: file.source_relative.clone(),
                destination: file.destination.clone(),
                size,
                sha256: file.sha256,
                blob: digest,
                component: file.component.clone(),
                condition: file.condition.clone(),
            });
        }
        let prerequisite_artifacts = canonical_prerequisites(plan)?;
        for prerequisite in &plan.prerequisites {
            verify_source(&prerequisite.source, prerequisite.size, prerequisite.sha256)?;
        }
        entries.sort_by(|a, b| {
            a.destination
                .to_string()
                .cmp(&b.destination.to_string())
                .then(a.path.cmp(&b.path))
        });
        let total_size = entries
            .iter()
            .try_fold(0u64, |sum, entry| sum.checked_add(entry.size))
            .ok_or(PackageError::Invalid)?;
        if entries.len() > MAX_ENTRIES {
            return Err(PackageError::TooManyBlobs {
                count: entries.len(),
                limit: MAX_ENTRIES,
            });
        }
        let portable_plan = PortableBuildPlan {
            installer,
            entries,
            prerequisite_artifacts,
            plugins: artifacts
                .into_iter()
                .map(|artifact| artifact.metadata)
                .collect(),
            total_size,
        };
        let metadata = Metadata {
            schema: PACKAGE_SCHEMA,
            required_features: PACKAGE_FEATURE_EXTERNAL_PAYLOAD,
            plan: portable_plan,
            blobs: Vec::new(),
        };
        let meta = serde_json::to_vec(&metadata)?;
        if meta.len() as u64 > MAX_METADATA {
            return Err(PackageError::MetadataTooLarge {
                size: meta.len() as u64,
                limit: MAX_METADATA,
            });
        }
        let mut out = Vec::with_capacity(HEADER_LEN as usize + meta.len());
        out.extend_from_slice(b"ZUPBNDL\0");
        out.extend_from_slice(&PACKAGE_SCHEMA.to_le_bytes());
        out.extend_from_slice(&PACKAGE_FEATURE_EXTERNAL_PAYLOAD.to_le_bytes());
        out.extend_from_slice(&(meta.len() as u64).to_le_bytes());
        out.extend_from_slice(&Sha256::digest(&meta));
        out.extend_from_slice(&meta);
        Ok(out)
    }

    /// Re-wrap a canonical plan as a plan-only package.
    ///
    /// This is what a runtime does at install time: it has the plan the release
    /// authenticated, and it needs a `Package` so the content source can be
    /// addressed by path and digest. It proves nothing and copies nothing — the
    /// plan is already canonical, and the digests it names are the keys the
    /// verified cache is addressed by. `artifacts` is only the plugin *metadata*,
    /// whose AOT bytes come from the cache rather than from here.
    pub fn encode_plan_only_plan(
        plan: &PortableBuildPlan,
        artifacts: &[PluginArtifact],
    ) -> Result<Vec<u8>, PackageError> {
        let mut metadata = Metadata {
            schema: PACKAGE_SCHEMA,
            required_features: PACKAGE_FEATURE_EXTERNAL_PAYLOAD,
            plan: plan.clone(),
            blobs: Vec::new(),
        };
        metadata.plan.plugins = artifacts.to_vec();
        metadata
            .plan
            .plugins
            .sort_by(|left, right| left.plugin_id.cmp(&right.plugin_id));
        let meta = serde_json::to_vec(&metadata)?;
        if meta.len() as u64 > MAX_METADATA {
            return Err(PackageError::MetadataTooLarge {
                size: meta.len() as u64,
                limit: MAX_METADATA,
            });
        }
        let mut out = Vec::with_capacity(HEADER_LEN as usize + meta.len());
        out.extend_from_slice(b"ZUPBNDL\0");
        out.extend_from_slice(&PACKAGE_SCHEMA.to_le_bytes());
        out.extend_from_slice(&PACKAGE_FEATURE_EXTERNAL_PAYLOAD.to_le_bytes());
        out.extend_from_slice(&(meta.len() as u64).to_le_bytes());
        out.extend_from_slice(&Sha256::digest(&meta));
        out.extend_from_slice(&meta);
        Ok(out)
    }

    /// Write a package for one variant from a shared content store.
    ///
    /// The caller supplies blobs that are already Zstandard-compressed and
    /// already digest-verified, which is what lets a selected variant be
    /// materialized out of an artifact that stores many variants' content once.
    /// `Package::open` re-verifies every blob, so handing over compressed bytes
    /// moves no trust: it only avoids compressing the same content twice.
    ///
    /// Every digest the plan references must be present, and nothing else is
    /// written, so the resulting package contains exactly one variant's content.
    pub fn write_plan(
        plan: &PortableBuildPlan,
        compressed: &std::collections::BTreeMap<Sha256Digest, Vec<u8>>,
        output: &Path,
    ) -> Result<u64, PackageError> {
        let mut required: Vec<(Sha256Digest, u64)> = Vec::new();
        for entry in &plan.entries {
            required.push((entry.blob, entry.size));
        }
        for artifact in &plan.prerequisite_artifacts {
            required.push((artifact.blob, artifact.size));
        }
        for artifact in &plan.plugins {
            required.push((artifact.blob, artifact.aot_size));
        }
        required.sort_unstable();
        required.dedup();
        for (digest, size) in &required {
            let Some(bytes) = compressed.get(digest) else {
                return Err(PackageError::Missing {
                    media_type: "content blob",
                    digest: digest.to_hex(),
                });
            };
            if *size == 0 || bytes.is_empty() || bytes.len() as u64 > *size {
                return Err(PackageError::Invalid);
            }
        }
        let mut installer = plan.installer.clone();
        for mapping in &mut installer.files {
            mapping.source = "embedded".to_owned();
        }
        let mut blobs = Vec::with_capacity(required.len());
        let mut offset = 0u64;
        for (digest, size) in &required {
            let bytes = compressed[digest].len() as u64;
            blobs.push(BlobIndex {
                digest: *digest,
                offset,
                compressed_size: bytes,
                size: *size,
            });
            offset = offset.checked_add(bytes).ok_or(PackageError::Invalid)?;
        }
        let metadata = Metadata {
            schema: PACKAGE_SCHEMA,
            required_features: 0,
            plan: PortableBuildPlan {
                installer,
                entries: plan.entries.clone(),
                prerequisite_artifacts: plan.prerequisite_artifacts.clone(),
                plugins: plan.plugins.clone(),
                total_size: plan.total_size,
            },
            blobs,
        };
        let meta = serde_json::to_vec(&metadata)?;
        if meta.len() as u64 > MAX_METADATA {
            return Err(PackageError::MetadataTooLarge {
                size: meta.len() as u64,
                limit: MAX_METADATA,
            });
        }
        let mut out = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(output)?;
        out.write_all(b"ZUPBNDL\0")?;
        out.write_all(&PACKAGE_SCHEMA.to_le_bytes())?;
        out.write_all(&0u64.to_le_bytes())?;
        out.write_all(&(meta.len() as u64).to_le_bytes())?;
        out.write_all(&Sha256::digest(&meta))?;
        out.write_all(&meta)?;
        for (digest, _) in &required {
            out.write_all(&compressed[digest])?;
        }
        out.sync_all()?;
        Ok(out.metadata()?.len())
    }

    /// Stream unique source files through Zstandard into a spool directory,
    /// then write the package index and compressed objects to `output`.
    pub fn write_file(
        plan: &TargetBuildPlan,
        artifacts: &[CompiledPluginArtifact],
        output: &Path,
    ) -> Result<u64, PackageError> {
        let artifacts = canonical_artifacts(plan, artifacts)?;
        struct Spool {
            path: PathBuf,
            size: u64,
            compressed_size: u64,
        }
        let spool_dir = tempfile::tempdir()?;
        let mut installer = plan.installer.clone();
        for mapping in &mut installer.files {
            mapping.source = "embedded".to_owned();
        }
        let mut entries = Vec::with_capacity(plan.files.len());
        let mut objects = BTreeMap::<Sha256Digest, Spool>::new();
        for file in &plan.files {
            if let std::collections::btree_map::Entry::Vacant(entry) = objects.entry(file.sha256) {
                let mut input = File::open(&file.source)?;
                let spool_path = spool_dir.path().join(file.sha256.to_hex());
                let output_file = File::create(&spool_path)?;
                let mut encoder = zstd::stream::write::Encoder::new(output_file, 9)?;
                let mut hasher = Sha256::new();
                let mut size = 0u64;
                let mut buffer = [0u8; 64 * 1024];
                loop {
                    let read = input.read(&mut buffer)?;
                    if read == 0 {
                        break;
                    }
                    size = size.checked_add(read as u64).ok_or(PackageError::Invalid)?;
                    hasher.update(&buffer[..read]);
                    encoder.write_all(&buffer[..read])?;
                }
                let output_file = encoder.finish()?;
                if size != file.size || Sha256Digest::from_hasher(hasher) != file.sha256 {
                    return Err(PackageError::Payload(file.source.display().to_string()));
                }
                let compressed_size = output_file.metadata()?.len();
                entry.insert(Spool {
                    path: spool_path,
                    size,
                    compressed_size,
                });
            } else {
                let (size, digest) = hash_reader(File::open(&file.source)?)?;
                if size != file.size || digest != file.sha256 {
                    return Err(PackageError::Payload(file.source.display().to_string()));
                }
            }
            entries.push(PayloadEntry {
                path: file.source_relative.clone(),
                destination: file.destination.clone(),
                size: file.size,
                sha256: file.sha256,
                blob: file.sha256,
                component: file.component.clone(),
                condition: file.condition.clone(),
            });
        }
        let prerequisite_artifacts = canonical_prerequisites(plan)?;
        for prerequisite in &plan.prerequisites {
            if let std::collections::btree_map::Entry::Vacant(entry) =
                objects.entry(prerequisite.sha256)
            {
                let spool_path = spool_dir.path().join(prerequisite.sha256.to_hex());
                let output_file = File::create(&spool_path)?;
                let mut encoder = zstd::stream::write::Encoder::new(output_file, 9)?;
                let mut input = File::open(&prerequisite.source)?;
                let mut hasher = Sha256::new();
                let mut size = 0u64;
                let mut buffer = [0u8; 64 * 1024];
                loop {
                    let read = input.read(&mut buffer)?;
                    if read == 0 {
                        break;
                    }
                    size = size.checked_add(read as u64).ok_or(PackageError::Invalid)?;
                    hasher.update(&buffer[..read]);
                    encoder.write_all(&buffer[..read])?;
                }
                let output_file = encoder.finish()?;
                if size != prerequisite.size
                    || Sha256Digest::from_hasher(hasher) != prerequisite.sha256
                {
                    return Err(PackageError::Payload(
                        prerequisite.source.display().to_string(),
                    ));
                }
                let compressed_size = output_file.metadata()?.len();
                entry.insert(Spool {
                    path: spool_path,
                    size,
                    compressed_size,
                });
            } else {
                let (size, digest) = hash_reader(File::open(&prerequisite.source)?)?;
                if size != prerequisite.size || digest != prerequisite.sha256 {
                    return Err(PackageError::Payload(
                        prerequisite.source.display().to_string(),
                    ));
                }
            }
        }
        for artifact in &artifacts {
            if let std::collections::btree_map::Entry::Vacant(entry) =
                objects.entry(artifact.metadata.blob)
            {
                let spool_path = spool_dir.path().join(artifact.metadata.blob.to_hex());
                let output_file = File::create(&spool_path)?;
                let mut encoder = zstd::stream::write::Encoder::new(output_file, 9)?;
                encoder.write_all(artifact.bytes.as_slice())?;
                let output_file = encoder.finish()?;
                entry.insert(Spool {
                    path: spool_path,
                    size: artifact.metadata.aot_size,
                    compressed_size: output_file.metadata()?.len(),
                });
            }
        }
        entries.sort_by(|a, b| {
            a.destination
                .to_string()
                .cmp(&b.destination.to_string())
                .then(a.path.cmp(&b.path))
        });
        let total_size = entries.iter().try_fold(0u64, |sum, entry| {
            sum.checked_add(entry.size).ok_or(PackageError::Invalid)
        })?;
        if objects.len() > MAX_BLOBS {
            return Err(PackageError::TooManyBlobs {
                count: objects.len(),
                limit: MAX_BLOBS,
            });
        }
        let mut offset = 0u64;
        let mut blobs = Vec::with_capacity(objects.len());
        for (digest, spool) in objects.iter() {
            blobs.push(BlobIndex {
                digest: *digest,
                offset,
                compressed_size: spool.compressed_size,
                size: spool.size,
            });
            offset = offset
                .checked_add(spool.compressed_size)
                .ok_or(PackageError::Invalid)?;
        }
        let metadata = Metadata {
            schema: PACKAGE_SCHEMA,
            required_features: 0,
            plan: PortableBuildPlan {
                installer,
                entries,
                prerequisite_artifacts,
                plugins: artifacts
                    .into_iter()
                    .map(|artifact| artifact.metadata)
                    .collect(),
                total_size,
            },
            blobs,
        };
        let meta = serde_json::to_vec(&metadata)?;
        if meta.len() as u64 > MAX_METADATA {
            return Err(PackageError::MetadataTooLarge {
                size: meta.len() as u64,
                limit: MAX_METADATA,
            });
        }
        let mut out = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(output)?;
        out.write_all(b"ZUPBNDL\0")?;
        out.write_all(&PACKAGE_SCHEMA.to_le_bytes())?;
        out.write_all(&0u64.to_le_bytes())?;
        out.write_all(&(meta.len() as u64).to_le_bytes())?;
        out.write_all(&Sha256::digest(&meta))?;
        out.write_all(&meta)?;
        for spool in objects.values() {
            std::io::copy(&mut File::open(&spool.path)?, &mut out)?;
        }
        out.sync_all()?;
        Ok(out.metadata()?.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zup_core::{
        App, AppId, Install, InstallDirectory, InstallScope, NonEmptyString, PluginBinding,
        PluginId, Template,
    };
    use zup_plugin_contract::{
        AOT_FORMAT_VERSION, HOST_TARGET, PLUGIN_API_VERSION, WASMTIME_VERSION, engine_fingerprint,
        wit_package_digest,
    };

    fn plan_with_plugins(count: usize) -> TargetBuildPlan {
        TargetBuildPlan {
            installer: Installer {
                app: App {
                    id: AppId::new("com.example.bundle-limits").unwrap(),
                    name: NonEmptyString::new("Bundle Limits").unwrap(),
                    version: "1.0.0".parse().unwrap(),
                    publisher: None,
                    main: None,
                    description: None,
                },
                target: TargetTriple::parse(HOST_TARGET).unwrap(),
                frontend: Default::default(),
                ui: None,
                updates: None,
                prerequisites: Vec::new(),
                install: Install {
                    scope: InstallScope::User,
                    directory: InstallDirectory {
                        user: Some(Template::parse("${location.user_data}/BundleLimits").unwrap()),
                        machine: None,
                    },
                    allow_directory_override: false,
                },
                components: Vec::new(),
                plugins: (0..count)
                    .map(|index| PluginBinding {
                        id: PluginId::new(format!("plugin-{index}")).unwrap(),
                        component: None,
                        when: None,
                    })
                    .collect(),
                files: Vec::new(),
                launchers: Vec::new(),
                path: Vec::new(),
                services: Vec::new(),
                protocols: Vec::new(),
                file_associations: Vec::new(),
            },
            prerequisites: Vec::new(),
            plugins: Vec::new(),
            files: Vec::new(),
            total_size: 0,
            prerequisite_size: 0,
        }
    }

    fn unchecked_artifact(id: &str) -> CompiledPluginArtifact {
        let digest = Sha256Digest::from_bytes([0; 32]);
        CompiledPluginArtifact {
            metadata: PluginArtifact {
                plugin_id: PluginId::new(id).unwrap(),
                source_size: 1,
                source_sha256: digest,
                target: TargetTriple::parse(HOST_TARGET).unwrap(),
                wasmtime_version: WASMTIME_VERSION.to_owned(),
                aot_format_version: AOT_FORMAT_VERSION,
                plugin_api_version: PLUGIN_API_VERSION.to_owned(),
                wit_digest: Sha256Digest::from_bytes(wit_package_digest()),
                engine_fingerprint: Sha256Digest::from_bytes(
                    *engine_fingerprint(HOST_TARGET).as_bytes(),
                ),
                aot_size: MAX_AOT_BYTES as u64,
                aot_sha256: digest,
                blob: digest,
            },
            bytes: Vec::new(),
        }
    }

    #[test]
    fn canonicalization_checks_aggregate_uncompressed_aot_size() {
        let plan = plan_with_plugins(5);
        let artifacts = plan
            .installer
            .plugins
            .iter()
            .map(|binding| unchecked_artifact(binding.id.as_str()))
            .collect::<Vec<_>>();

        assert!(matches!(
            canonical_artifacts(&plan, &artifacts),
            Err(PackageError::PluginAotTooLarge {
                size,
                limit: MAX_PLUGIN_AOT_TOTAL_BYTES,
            }) if size == 5 * MAX_AOT_BYTES as u64
        ));
    }

    #[test]
    fn writer_rejects_aggregate_before_reading_payload_sources() {
        let plan = plan_with_plugins(5);
        let artifacts = plan
            .installer
            .plugins
            .iter()
            .map(|binding| unchecked_artifact(binding.id.as_str()))
            .collect::<Vec<_>>();
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("bundle.zupbundle");

        let error = BundleWriter::write_file(&plan, &artifacts, &output).unwrap_err();
        assert!(matches!(
            error,
            PackageError::PluginAotTooLarge {
                size,
                limit: MAX_PLUGIN_AOT_TOTAL_BYTES,
            } if size == 5 * MAX_AOT_BYTES as u64
        ));
        assert!(!output.exists());
    }
}
