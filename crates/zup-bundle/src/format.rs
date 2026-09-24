//! Versioned, content-addressed bundle objects stored in an Authenticode-hashed
//! PE RCDATA resource.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::{Cursor, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use zup_build::{BuildPlan, MAX_PLUGIN_SOURCE_BYTES};
use zup_core::{
    ComponentId, Condition, Installer, MAX_PLUGIN_ARTIFACTS, PluginId, RelativePath, Sha256Digest,
    Template, hash_reader,
};
use zup_plugin_contract::{
    AOT_FORMAT_VERSION, MAX_AOT_BYTES, PLUGIN_API_VERSION, WASMTIME_VERSION, engine_fingerprint,
    wit_package_digest,
};

const MAGIC: &[u8; 8] = b"ZUPBNDL\0";
const SCHEMA: u32 = 3;
const HEADER_LEN: u64 = 60;
const MAX_METADATA: u64 = 256 * 1024 * 1024;
const MAX_ENTRIES: usize = 1_000_000;
/// Maximum aggregate uncompressed plugin AOT bytes in one bundle.
pub const MAX_PLUGIN_AOT_TOTAL_BYTES: u64 = 256 * 1024 * 1024;
const MAX_PLUGIN_ID_BYTES: usize = 128;
const MAX_TARGET_BYTES: usize = 256;
const MAX_RESOURCE_SIZE: u64 = u32::MAX as u64;
const RESOURCE_TYPE_RCDATA: usize = 10;
const RESOURCE_ID_BUNDLE: usize = 1;
const WINDOWS_X64_TARGET: &str = "x86_64-pc-windows-msvc";
const WINDOWS_ARM64_TARGET: &str = "aarch64-pc-windows-msvc";

#[derive(Debug, Error)]
pub enum BundleError {
    #[error("bundle I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("bundle metadata: {0}")]
    Json(#[from] serde_json::Error),
    #[error("bundle is truncated, corrupt, unsupported, or has unsafe offsets")]
    Invalid,
    #[error("executable has no embedded bundle resource")]
    MissingResource,
    #[error(
        "installer package is {size} bytes; the Windows RCDATA resource limit is {limit} bytes"
    )]
    ResourceTooLarge { size: u64, limit: u64 },
    #[error("bundle index is {size} bytes; the metadata limit is {limit} bytes")]
    MetadataTooLarge { size: u64, limit: u64 },
    #[error(
        "installer has {count} unique payload blobs; numeric RCDATA identifiers allow at most {limit}"
    )]
    TooManyBlobs { count: usize, limit: usize },
    #[error("bundle has {count} plugin artifacts; the limit is {limit}")]
    TooManyPluginArtifacts { count: usize, limit: usize },
    #[error("aggregate plugin AOT payload is {size} bytes; the limit is {limit} bytes")]
    PluginAotTooLarge { size: u64, limit: u64 },
    #[error("cannot allocate {size} bytes while processing the installer resource")]
    ResourceAllocation { size: u64 },
    #[error("PE resource API failed: {0}")]
    ResourceApi(u32),
    #[error("PE resource APIs are available only on Windows")]
    ResourcesUnavailable,
    #[error(
        "runtime already has an Authenticode certificate table; embed resources before signing"
    )]
    RuntimeAlreadySigned,
    #[error("payload verification failed for {0}")]
    Payload(String),
    #[error("plugin artifact is invalid: {0}")]
    PluginArtifact(String),
}

impl BundleError {
    pub fn is_missing_resource(&self) -> bool {
        matches!(self, Self::MissingResource)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginArtifact {
    pub plugin_id: PluginId,
    pub source_size: u64,
    pub source_sha256: Sha256Digest,
    pub target: String,
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
    pub fn validate(&self) -> Result<(), BundleError> {
        if self.plugin_id.as_str().len() > MAX_PLUGIN_ID_BYTES {
            return Err(BundleError::PluginArtifact(
                "plugin id exceeds the field limit".to_owned(),
            ));
        }
        if self.target.len() > MAX_TARGET_BYTES
            || self.target.is_empty()
            || self.target.contains('\0')
            || self.wasmtime_version.len() > 32
            || self.plugin_api_version.len() > 32
        {
            return Err(BundleError::PluginArtifact(
                "target exceeds the field limit".to_owned(),
            ));
        }
        if self.source_size == 0 || self.source_size > MAX_PLUGIN_SOURCE_BYTES {
            return Err(BundleError::PluginArtifact(
                "source size exceeds the component limit".to_owned(),
            ));
        }
        if self.aot_size == 0 || self.aot_size > u64::try_from(MAX_AOT_BYTES).unwrap_or(u64::MAX) {
            return Err(BundleError::PluginArtifact(
                "AOT size is outside the component limit".to_owned(),
            ));
        }
        if self.wasmtime_version != WASMTIME_VERSION
            || self.aot_format_version != AOT_FORMAT_VERSION
            || self.plugin_api_version != PLUGIN_API_VERSION
        {
            return Err(BundleError::PluginArtifact(
                "component contract version is not supported".to_owned(),
            ));
        }
        if self.wit_digest != Sha256Digest::from_bytes(wit_package_digest()) {
            return Err(BundleError::PluginArtifact(
                "WIT digest does not match the contract".to_owned(),
            ));
        }
        zup_plugin_contract::PluginEngine::new(&self.target)
            .map_err(|error| BundleError::PluginArtifact(error.to_string()))?;
        if self.engine_fingerprint
            != Sha256Digest::from_bytes(*engine_fingerprint(&self.target).as_bytes())
        {
            return Err(BundleError::PluginArtifact(
                "engine fingerprint does not match the target".to_owned(),
            ));
        }
        if self.aot_sha256 != self.blob {
            return Err(BundleError::PluginArtifact(
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
    pub fn new(metadata: PluginArtifact, bytes: Vec<u8>) -> Result<Self, BundleError> {
        metadata.validate()?;
        if bytes.len() as u64 != metadata.aot_size {
            return Err(BundleError::PluginArtifact(
                "AOT byte length does not match metadata".to_owned(),
            ));
        }
        let digest = Sha256Digest::from_bytes(Sha256::digest(&bytes).into());
        if digest != metadata.aot_sha256 {
            return Err(BundleError::PluginArtifact(
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
struct BlobIndex {
    digest: Sha256Digest,
    resource_id: u16,
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

fn validate_plugin_aot_total<'a>(
    mut artifacts: impl Iterator<Item = &'a PluginArtifact>,
) -> Result<u64, BundleError> {
    let total = artifacts.try_fold(0u64, |total, artifact| {
        total
            .checked_add(artifact.aot_size)
            .ok_or(BundleError::PluginAotTooLarge {
                size: u64::MAX,
                limit: MAX_PLUGIN_AOT_TOTAL_BYTES,
            })
    })?;
    if total > MAX_PLUGIN_AOT_TOTAL_BYTES {
        return Err(BundleError::PluginAotTooLarge {
            size: total,
            limit: MAX_PLUGIN_AOT_TOTAL_BYTES,
        });
    }
    Ok(total)
}

fn canonical_artifacts(
    plan: &BuildPlan,
    artifacts: &[CompiledPluginArtifact],
) -> Result<Vec<CompiledPluginArtifact>, BundleError> {
    let count = artifacts.len().max(plan.installer.plugins.len());
    if count > MAX_PLUGIN_ARTIFACTS {
        return Err(BundleError::TooManyPluginArtifacts {
            count,
            limit: MAX_PLUGIN_ARTIFACTS,
        });
    }
    if artifacts.len() != plan.installer.plugins.len() {
        return Err(BundleError::Invalid);
    }
    validate_plugin_aot_total(artifacts.iter().map(|artifact| &artifact.metadata))?;
    let mut expected = BTreeMap::<String, PluginId>::new();
    for plugin in &plan.installer.plugins {
        if expected
            .insert(plugin.id.as_str().to_ascii_lowercase(), plugin.id.clone())
            .is_some()
        {
            return Err(BundleError::Invalid);
        }
    }
    let mut canonical = artifacts.to_vec();
    canonical.sort_by(|a, b| a.metadata.plugin_id.cmp(&b.metadata.plugin_id));
    let mut actual = BTreeSet::<String>::new();
    for artifact in &canonical {
        artifact.metadata.validate()?;
        let key = artifact.metadata.plugin_id.as_str().to_ascii_lowercase();
        let Some(expected_id) = expected.get(&key) else {
            return Err(BundleError::Invalid);
        };
        if artifact.metadata.plugin_id != *expected_id || !actual.insert(key) {
            return Err(BundleError::Invalid);
        }
    }
    validate_plugin_aot_total(canonical.iter().map(|artifact| &artifact.metadata))?;
    if let Some(first) = canonical.first()
        && canonical.iter().any(|artifact| {
            artifact.metadata.target != first.metadata.target
                || artifact.metadata.engine_fingerprint != first.metadata.engine_fingerprint
        })
    {
        return Err(BundleError::Invalid);
    }
    if !plan.plugins.is_empty() {
        if plan.plugins.len() != plan.installer.plugins.len() {
            return Err(BundleError::Invalid);
        }
        for (resolved, binding) in plan.plugins.iter().zip(&plan.installer.plugins) {
            if resolved.id != binding.id {
                return Err(BundleError::Invalid);
            }
            let Some(artifact) = canonical
                .iter()
                .find(|artifact| artifact.metadata.plugin_id == resolved.id)
            else {
                return Err(BundleError::Invalid);
            };
            if artifact.metadata.source_size != resolved.size
                || artifact.metadata.source_sha256 != resolved.sha256
            {
                return Err(BundleError::Invalid);
            }
        }
    }
    Ok(canonical)
}

#[derive(Debug)]
pub struct BundleWriter;

impl BundleWriter {
    /// Produce deterministic package bytes. Blobs are Zstandard-compressed once
    /// per unique SHA-256 identity; all offsets are relative to the data region.
    pub fn encode(
        plan: &BuildPlan,
        artifacts: &[CompiledPluginArtifact],
    ) -> Result<Vec<u8>, BundleError> {
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
                return Err(BundleError::Payload(file.source.display().to_string()));
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
        for artifact in &artifacts {
            let digest = artifact.metadata.blob;
            if let Some(existing) = contents.get(&digest) {
                if existing.as_slice() != artifact.bytes.as_slice() {
                    return Err(BundleError::PluginArtifact(
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
        let total_size = entries.iter().try_fold(0u64, |sum, e| {
            sum.checked_add(e.size).ok_or(BundleError::Invalid)
        })?;
        let mut compressed = Vec::new();
        let mut blobs = Vec::new();
        if contents.len() > u16::MAX as usize - 1 {
            return Err(BundleError::TooManyBlobs {
                count: contents.len(),
                limit: u16::MAX as usize - 1,
            });
        }
        for (index, (digest, bytes)) in contents.into_iter().enumerate() {
            let encoded = zstd::stream::encode_all(Cursor::new(&bytes), 9)?;
            let offset = compressed.len() as u64;
            blobs.push(BlobIndex {
                digest,
                resource_id: u16::try_from(index + 2).map_err(|_| BundleError::Invalid)?,
                offset,
                compressed_size: encoded.len() as u64,
                size: bytes.len() as u64,
            });
            compressed.extend_from_slice(&encoded);
        }
        let plan = PortableBuildPlan {
            installer,
            entries,
            plugins: artifacts
                .into_iter()
                .map(|artifact| artifact.metadata)
                .collect(),
            total_size,
        };
        // Blob offsets are independent of metadata length.
        let metadata = Metadata {
            schema: SCHEMA,
            required_features: 0,
            plan,
            blobs,
        };
        let meta = serde_json::to_vec(&metadata)?;
        if meta.len() as u64 > MAX_METADATA {
            return Err(BundleError::MetadataTooLarge {
                size: meta.len() as u64,
                limit: MAX_METADATA,
            });
        }
        let meta_hash = Sha256::digest(&meta);
        let mut out = Vec::with_capacity(HEADER_LEN as usize + meta.len() + compressed.len());
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&SCHEMA.to_le_bytes());
        out.extend_from_slice(&0u64.to_le_bytes()); // required features
        out.extend_from_slice(&(meta.len() as u64).to_le_bytes());
        out.extend_from_slice(&meta_hash);
        out.extend_from_slice(&meta);
        out.extend_from_slice(&compressed);
        Ok(out)
    }

    /// Stream unique payload files through Zstandard into a spool directory,
    /// then write the index followed by compressed objects to `output`.
    pub fn write_file(
        plan: &BuildPlan,
        artifacts: &[CompiledPluginArtifact],
        output: &Path,
    ) -> Result<u64, BundleError> {
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
                    size = size.checked_add(read as u64).ok_or(BundleError::Invalid)?;
                    hasher.update(&buffer[..read]);
                    encoder.write_all(&buffer[..read])?;
                }
                let output_file = encoder.finish()?;
                if size != file.size || Sha256Digest::from_hasher(hasher) != file.sha256 {
                    return Err(BundleError::Payload(file.source.display().to_string()));
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
                    return Err(BundleError::Payload(file.source.display().to_string()));
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
        let total_size = entries.iter().try_fold(0u64, |sum, e| {
            sum.checked_add(e.size).ok_or(BundleError::Invalid)
        })?;
        let mut offset = 0u64;
        let mut blobs = Vec::with_capacity(objects.len());
        if objects.len() > u16::MAX as usize - 1 {
            return Err(BundleError::TooManyBlobs {
                count: objects.len(),
                limit: u16::MAX as usize - 1,
            });
        }
        for (index, (digest, spool)) in objects.iter().enumerate() {
            blobs.push(BlobIndex {
                digest: *digest,
                resource_id: u16::try_from(index + 2).map_err(|_| BundleError::Invalid)?,
                offset,
                compressed_size: spool.compressed_size,
                size: spool.size,
            });
            offset = offset
                .checked_add(spool.compressed_size)
                .ok_or(BundleError::Invalid)?;
        }
        let metadata = Metadata {
            schema: SCHEMA,
            required_features: 0,
            plan: PortableBuildPlan {
                installer,
                entries,
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
            return Err(BundleError::MetadataTooLarge {
                size: meta.len() as u64,
                limit: MAX_METADATA,
            });
        }
        let mut out = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(output)?;
        out.write_all(MAGIC)?;
        out.write_all(&SCHEMA.to_le_bytes())?;
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

/// Validated content index from the executable's RCDATA resources.
pub struct EmbeddedBundle {
    path: PathBuf,
    metadata: Metadata,
}

impl EmbeddedBundle {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, BundleError> {
        let path = path.as_ref().to_path_buf();
        let temporary = tempfile::tempdir()?;
        let package_path = temporary.path().join("bundle.index");
        extract_bundle_resource(&path, &package_path)?;
        let mut file = File::open(&package_path)?;
        let len = file.metadata()?.len();
        let metadata = parse_metadata(&mut file, 0, len, true)?;
        let bundle = Self { path, metadata };
        bundle.verify_all()?;
        Ok(bundle)
    }

    pub fn plan(&self) -> &PortableBuildPlan {
        &self.metadata.plan
    }
    pub fn build_plan(&self) -> Result<BuildPlan, BundleError> {
        let files = self
            .metadata
            .plan
            .entries
            .iter()
            .map(|entry| {
                Ok(zup_build::ResolvedFile {
                    source: PathBuf::new(),
                    source_relative: entry.path.clone(),
                    destination: entry.destination.clone(),
                    size: entry.size,
                    sha256: entry
                        .sha256
                        .to_string()
                        .parse()
                        .map_err(|_| BundleError::Invalid)?,
                    component: entry.component.clone(),
                    condition: entry.condition.clone(),
                })
            })
            .collect::<Result<Vec<_>, BundleError>>()?;
        Ok(BuildPlan {
            installer: self.metadata.plan.installer.clone(),
            plugins: Vec::new(),
            files,
            total_size: self.metadata.plan.total_size,
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
    pub fn plugin_aot(&self, id: &PluginId) -> Result<Vec<u8>, BundleError> {
        if !self
            .metadata
            .plan
            .installer
            .plugins
            .iter()
            .any(|plugin| &plugin.id == id)
        {
            return Err(BundleError::Invalid);
        }
        let artifact = self.plugin_artifact(id).ok_or(BundleError::Invalid)?;
        let blob = self
            .metadata
            .blobs
            .iter()
            .find(|blob| blob.digest == artifact.blob)
            .ok_or(BundleError::Invalid)?;
        let mut decoder = open_blob(&self.path, blob)?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(
                usize::try_from(artifact.aot_size).map_err(|_| BundleError::Invalid)?,
            )
            .map_err(|_| BundleError::ResourceAllocation {
                size: artifact.aot_size,
            })?;
        decoder
            .by_ref()
            .take(artifact.aot_size.saturating_add(1))
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 != artifact.aot_size
            || Sha256Digest::from_bytes(Sha256::digest(&bytes).into()) != artifact.aot_sha256
        {
            return Err(BundleError::Payload(artifact.blob.to_string()));
        }
        Ok(bytes)
    }
    pub fn payload_source(&self) -> BundlePayloadSource {
        BundlePayloadSource {
            path: self.path.clone(),
            blobs: self.metadata.blobs.clone(),
            entries: self.metadata.plan.entries.clone(),
        }
    }
    fn verify_all(&self) -> Result<(), BundleError> {
        for blob in &self.metadata.blobs {
            let limit = self
                .metadata
                .plan
                .plugins
                .iter()
                .find(|artifact| artifact.blob == blob.digest)
                .map_or(blob.size, |artifact| artifact.aot_size);
            verify_blob(&self.path, blob, limit)?;
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct BundlePayloadSource {
    path: PathBuf,
    blobs: Vec<BlobIndex>,
    entries: Vec<PayloadEntry>,
}

impl crate::PayloadSource for BundlePayloadSource {
    fn open(
        &self,
        path: &RelativePath,
        expected_sha256: &Sha256Digest,
        expected_size: u64,
    ) -> Result<crate::PayloadReader, crate::PayloadError> {
        if path.as_str() == "__zup_maintenance__.exe" {
            let file = File::open(&self.path).map_err(|source| crate::PayloadError::Read {
                path: path.to_string(),
                source,
            })?;
            let (size, digest) = hash_reader(file).map_err(|source| crate::PayloadError::Read {
                path: path.to_string(),
                source,
            })?;
            if size != expected_size {
                return Err(crate::PayloadError::SizeMismatch {
                    path: path.to_string(),
                    expected: expected_size,
                    found: size,
                });
            }
            if digest != *expected_sha256 {
                return Err(crate::PayloadError::DigestMismatch {
                    path: path.to_string(),
                });
            }
            let file = File::open(&self.path).map_err(|source| crate::PayloadError::Read {
                path: path.to_string(),
                source,
            })?;
            return Ok(Box::new(file));
        }
        let entry = self
            .entries
            .iter()
            .find(|e| &e.path == path && &e.sha256 == expected_sha256 && e.size == expected_size)
            .ok_or_else(|| crate::PayloadError::NotFound {
                path: path.to_string(),
            })?;
        let blob = self
            .blobs
            .iter()
            .find(|b| b.digest == entry.blob)
            .ok_or_else(|| crate::PayloadError::NotFound {
                path: path.to_string(),
            })?;
        let decoder = open_blob(&self.path, blob).map_err(|source| crate::PayloadError::Read {
            path: path.to_string(),
            source: std::io::Error::other(source.to_string()),
        })?;
        Ok(Box::new(decoder))
    }
}

fn parse_metadata(
    file: &mut File,
    start: u64,
    package_len: u64,
    resource_index: bool,
) -> Result<Metadata, BundleError> {
    if package_len < HEADER_LEN {
        return Err(BundleError::Invalid);
    }
    file.seek(SeekFrom::Start(start))?;
    let mut header = [0u8; HEADER_LEN as usize];
    file.read_exact(&mut header)?;
    if &header[..8] != MAGIC || u32::from_le_bytes(header[8..12].try_into().unwrap()) != SCHEMA {
        return Err(BundleError::Invalid);
    }
    let features = u64::from_le_bytes(header[12..20].try_into().unwrap());
    let meta_len = u64::from_le_bytes(header[20..28].try_into().unwrap());
    if features != 0
        || HEADER_LEN.checked_add(meta_len).is_none_or(|n| {
            if resource_index {
                n != package_len
            } else {
                n > package_len
            }
        })
    {
        return Err(BundleError::Invalid);
    }
    if meta_len > MAX_METADATA {
        return Err(BundleError::MetadataTooLarge {
            size: meta_len,
            limit: MAX_METADATA,
        });
    }
    let meta_size = usize::try_from(meta_len).map_err(|_| BundleError::Invalid)?;
    let mut bytes = vec![0; meta_size];
    file.read_exact(&mut bytes)?;
    if Sha256::digest(&bytes).as_slice() != &header[28..60] {
        return Err(BundleError::Invalid);
    }
    let metadata: Metadata = serde_json::from_slice(&bytes)?;
    if metadata.plan.plugins.len() > MAX_PLUGIN_ARTIFACTS {
        return Err(BundleError::TooManyPluginArtifacts {
            count: metadata.plan.plugins.len(),
            limit: MAX_PLUGIN_ARTIFACTS,
        });
    }
    if metadata.schema != SCHEMA
        || metadata.required_features != 0
        || metadata.plan.entries.len() > MAX_ENTRIES
        || metadata.blobs.len() > u16::MAX as usize - 1
    {
        return Err(BundleError::Invalid);
    }
    validate_plugin_aot_total(metadata.plan.plugins.iter())?;

    let computed_total = metadata.plan.entries.iter().try_fold(0u64, |sum, entry| {
        sum.checked_add(entry.size).ok_or(BundleError::Invalid)
    })?;
    if computed_total != metadata.plan.total_size {
        return Err(BundleError::Invalid);
    }

    let mut entry_keys = BTreeSet::new();
    for entry in &metadata.plan.entries {
        if entry.size > MAX_RESOURCE_SIZE {
            return Err(BundleError::Invalid);
        }
        let key = (entry.destination.to_string(), entry.path.clone());
        if !entry_keys.insert(key) {
            return Err(BundleError::Invalid);
        }
    }
    if metadata.plan.entries.windows(2).any(|window| {
        let left = (window[0].destination.to_string(), window[0].path.clone());
        let right = (window[1].destination.to_string(), window[1].path.clone());
        left >= right
    }) {
        return Err(BundleError::Invalid);
    }

    let mut expected_plugins = BTreeMap::<String, PluginId>::new();
    for plugin in &metadata.plan.installer.plugins {
        if expected_plugins
            .insert(plugin.id.as_str().to_ascii_lowercase(), plugin.id.clone())
            .is_some()
        {
            return Err(BundleError::Invalid);
        }
    }
    if expected_plugins.len() != metadata.plan.plugins.len() {
        return Err(BundleError::Invalid);
    }
    let mut artifact_plugins = BTreeMap::<String, &PluginArtifact>::new();
    for artifact in &metadata.plan.plugins {
        artifact.validate()?;
        let key = artifact.plugin_id.as_str().to_ascii_lowercase();
        let Some(expected_id) = expected_plugins.get(&key) else {
            return Err(BundleError::Invalid);
        };
        if artifact.plugin_id != *expected_id || artifact_plugins.insert(key, artifact).is_some() {
            return Err(BundleError::Invalid);
        }
    }
    if metadata
        .plan
        .plugins
        .windows(2)
        .any(|window| window[0].plugin_id >= window[1].plugin_id)
    {
        return Err(BundleError::Invalid);
    }
    if let Some(first) = metadata.plan.plugins.first()
        && metadata.plan.plugins.iter().any(|artifact| {
            artifact.target != first.target
                || artifact.engine_fingerprint != first.engine_fingerprint
        })
    {
        return Err(BundleError::Invalid);
    }

    let data_len = package_len
        .checked_sub(HEADER_LEN + meta_len)
        .ok_or(BundleError::Invalid)?;
    let mut previous = 0u64;
    let mut by_digest = BTreeMap::new();
    for (index, blob) in metadata.blobs.iter().enumerate() {
        let end = blob
            .offset
            .checked_add(blob.compressed_size)
            .ok_or(BundleError::Invalid)?;
        if blob.offset != previous
            || (!resource_index && end > data_len)
            || blob.resource_id != u16::try_from(index + 2).map_err(|_| BundleError::Invalid)?
            || blob.compressed_size == 0
            || blob.compressed_size > MAX_RESOURCE_SIZE
            || blob.size > MAX_RESOURCE_SIZE
            || by_digest.insert(blob.digest, blob.size).is_some()
            || (index > 0 && metadata.blobs[index - 1].digest >= blob.digest)
        {
            return Err(BundleError::Invalid);
        }
        previous = end;
    }
    if !resource_index && previous != data_len {
        return Err(BundleError::Invalid);
    }

    let mut referenced = BTreeSet::new();
    for entry in &metadata.plan.entries {
        if entry.blob != entry.sha256 || by_digest.get(&entry.blob) != Some(&entry.size) {
            return Err(BundleError::Invalid);
        }
        referenced.insert(entry.blob);
    }
    for artifact in &metadata.plan.plugins {
        if artifact.blob != artifact.aot_sha256
            || by_digest.get(&artifact.blob) != Some(&artifact.aot_size)
        {
            return Err(BundleError::Invalid);
        }
        referenced.insert(artifact.blob);
    }
    if referenced.len() != metadata.blobs.len() {
        return Err(BundleError::Invalid);
    }
    Ok(metadata)
}

fn verify_blob(executable: &Path, blob: &BlobIndex, limit: u64) -> Result<(), BundleError> {
    let decoder = open_blob(executable, blob)?;
    let (size, digest) = hash_reader(decoder.take(limit.saturating_add(1)))?;
    if size != blob.size || digest != blob.digest || size > limit {
        return Err(BundleError::Payload(blob.digest.to_string()));
    }
    Ok(())
}

fn open_blob(
    executable: &Path,
    blob: &BlobIndex,
) -> Result<
    zstd::stream::read::Decoder<'static, std::io::BufReader<std::io::Cursor<Vec<u8>>>>,
    BundleError,
> {
    let bytes = read_blob_resource(executable, blob.resource_id as usize)?;
    if bytes.len() as u64 != blob.compressed_size {
        return Err(BundleError::Invalid);
    }
    Ok(zstd::stream::read::Decoder::new(std::io::Cursor::new(
        bytes,
    ))?)
}
/// Build the final installer artifact. Authenticode signing must happen after
/// this returns so the signature covers the package resource.
pub fn build_self_contained_executable(
    executable: &Path,
    output: &Path,
    plan: &BuildPlan,
    artifacts: &[CompiledPluginArtifact],
) -> Result<(u64, u64), BundleError> {
    validate_unsigned_pe(executable)?;
    let temporary = tempfile::tempdir()?;
    let package = temporary.path().join("installer.zupbundle");
    let package_size = BundleWriter::write_file(plan, artifacts, &package)?;
    embed_bundle_file(executable, output, &package)?;
    Ok((std::fs::metadata(output)?.len(), package_size))
}

/// Embed a prebuilt bundle file as RCDATA resource 1. This is also used by
/// installer integration tests to exercise the exact native resource path.
pub fn embed_bundle_file(
    executable: &Path,
    output: &Path,
    package: &Path,
) -> Result<(), BundleError> {
    validate_unsigned_pe(executable)?;
    embed_bundle_resource(executable, output, package)
}

pub fn read_pe_target(path: &Path) -> Result<&'static str, BundleError> {
    let machine = read_pe_header(path)?.machine;
    match machine {
        0x8664 => Ok(WINDOWS_X64_TARGET),
        0xaa64 => Ok(WINDOWS_ARM64_TARGET),
        _ => Err(BundleError::Invalid),
    }
}

struct PeHeader {
    machine: u16,
    security_offset: u64,
}

fn read_pe_header(path: &Path) -> Result<PeHeader, BundleError> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    if len < 0x40 {
        return Err(BundleError::Invalid);
    }
    let mut dos = [0u8; 0x40];
    file.read_exact(&mut dos)?;
    if &dos[..2] != b"MZ" {
        return Err(BundleError::Invalid);
    }
    let pe = u32::from_le_bytes(dos[0x3c..0x40].try_into().unwrap()) as u64;
    if pe < 0x40 || pe.checked_add(24).is_none_or(|end| end > len) {
        return Err(BundleError::Invalid);
    }
    file.seek(SeekFrom::Start(pe))?;
    let mut signature = [0u8; 4];
    file.read_exact(&mut signature)?;
    if &signature != b"PE\0\0" {
        return Err(BundleError::Invalid);
    }
    let mut coff = [0u8; 20];
    file.read_exact(&mut coff)?;
    let machine = u16::from_le_bytes(coff[..2].try_into().unwrap());
    let section_count = u16::from_le_bytes(coff[2..4].try_into().unwrap());
    let optional_offset = pe + 24;
    let optional_len = u16::from_le_bytes(coff[16..18].try_into().unwrap()) as u64;
    let optional_end = optional_offset
        .checked_add(optional_len)
        .ok_or(BundleError::Invalid)?;
    if optional_len < 2 || optional_end > len || section_count == 0 {
        return Err(BundleError::Invalid);
    }
    file.seek(SeekFrom::Start(optional_offset))?;
    let mut magic = [0u8; 2];
    file.read_exact(&mut magic)?;
    let data_directory_offset = match u16::from_le_bytes(magic) {
        0x10b => 96,
        0x20b => 112,
        _ => return Err(BundleError::Invalid),
    };
    let security_offset = optional_offset
        .checked_add(data_directory_offset)
        .and_then(|offset| offset.checked_add(8 * 4))
        .ok_or(BundleError::Invalid)?;
    if security_offset
        .checked_add(8)
        .is_none_or(|end| end > optional_end)
    {
        return Err(BundleError::Invalid);
    }
    let section_end = optional_end
        .checked_add(
            u64::from(section_count)
                .checked_mul(40)
                .ok_or(BundleError::Invalid)?,
        )
        .ok_or(BundleError::Invalid)?;
    if section_end > len {
        return Err(BundleError::Invalid);
    }
    Ok(PeHeader {
        machine,
        security_offset,
    })
}

fn validate_unsigned_pe(path: &Path) -> Result<(), BundleError> {
    let header = read_pe_header(path)?;
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(header.security_offset))?;
    let mut certificate = [0u8; 8];
    file.read_exact(&mut certificate)?;
    if certificate != [0; 8] {
        return Err(BundleError::RuntimeAlreadySigned);
    }
    Ok(())
}

#[cfg(windows)]
fn embed_bundle_resource(
    executable: &Path,
    output: &Path,
    package: &Path,
) -> Result<(), BundleError> {
    use std::os::windows::ffi::OsStrExt;
    use windows_link::link;
    type Handle = *mut core::ffi::c_void;
    type Bool = i32;
    type Dword = u32;
    link!("kernel32.dll" "system" fn BeginUpdateResourceW(filename: *const u16, delete_existing: Bool) -> Handle);
    link!("kernel32.dll" "system" fn UpdateResourceW(update: Handle, resource_type: *const u16, name: *const u16, language: u16, data: *const core::ffi::c_void, size: Dword) -> Bool);
    link!("kernel32.dll" "system" fn EndUpdateResourceW(update: Handle, discard: Bool) -> Bool);
    link!("kernel32.dll" "system" fn GetLastError() -> Dword);
    if output.exists() || output == executable {
        return Err(BundleError::Invalid);
    }
    let mut package_file = File::open(package)?;
    let package_size = package_file.metadata()?.len();
    let mut header = [0u8; HEADER_LEN as usize];
    package_file.read_exact(&mut header)?;
    let metadata_size = u64::from_le_bytes(header[20..28].try_into().unwrap());
    let index_size = HEADER_LEN
        .checked_add(metadata_size)
        .ok_or(BundleError::Invalid)?;
    if metadata_size > MAX_METADATA {
        return Err(BundleError::MetadataTooLarge {
            size: metadata_size,
            limit: MAX_METADATA,
        });
    }
    if index_size > MAX_RESOURCE_SIZE {
        return Err(BundleError::ResourceTooLarge {
            size: index_size,
            limit: MAX_RESOURCE_SIZE,
        });
    }
    let metadata = parse_metadata(&mut package_file, 0, package_size, false)?;
    if metadata.blobs.len() > u16::MAX as usize - 1 {
        return Err(BundleError::TooManyBlobs {
            count: metadata.blobs.len(),
            limit: u16::MAX as usize - 1,
        });
    }
    for blob in &metadata.blobs {
        if blob.compressed_size > MAX_RESOURCE_SIZE {
            return Err(BundleError::ResourceTooLarge {
                size: blob.compressed_size,
                limit: MAX_RESOURCE_SIZE,
            });
        }
    }
    let index = read_package_range(&mut package_file, 0, index_size)?;
    std::fs::copy(executable, output)?;
    let wide: Vec<u16> = output.as_os_str().encode_wide().chain(Some(0)).collect();
    let update = unsafe { BeginUpdateResourceW(wide.as_ptr(), 0) };
    if update.is_null() {
        let _ = std::fs::remove_file(output);
        return Err(BundleError::ResourceApi(unsafe { GetLastError() }));
    }
    let apply = |id: usize, data: &[u8]| -> Result<(), u32> {
        let size = u32::try_from(data.len()).map_err(|_| 87u32)?;
        let ok = unsafe {
            UpdateResourceW(
                update,
                RESOURCE_TYPE_RCDATA as *const u16,
                id as *const u16,
                0,
                data.as_ptr().cast(),
                size,
            )
        };
        if ok == 0 {
            Err(unsafe { GetLastError() })
        } else {
            Ok(())
        }
    };
    let result = (|| {
        apply(RESOURCE_ID_BUNDLE, &index).map_err(BundleError::ResourceApi)?;
        let data_start = index_size;
        for blob in &metadata.blobs {
            let start = data_start
                .checked_add(blob.offset)
                .ok_or(BundleError::Invalid)?;
            let bytes = read_package_range(&mut package_file, start, blob.compressed_size)?;
            apply(blob.resource_id as usize, &bytes).map_err(BundleError::ResourceApi)?;
        }
        Ok(())
    })();
    if let Err(error) = result {
        unsafe {
            EndUpdateResourceW(update, 1);
        }
        let _ = std::fs::remove_file(output);
        return Err(error);
    }
    if unsafe { EndUpdateResourceW(update, 0) } == 0 {
        let error = unsafe { GetLastError() };
        let _ = std::fs::remove_file(output);
        return Err(BundleError::ResourceApi(error));
    }
    Ok(())
}

fn read_package_range(file: &mut File, offset: u64, size: u64) -> Result<Vec<u8>, BundleError> {
    let requested_size = size;
    let size = usize::try_from(requested_size).map_err(|_| BundleError::ResourceAllocation {
        size: requested_size,
    })?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(size)
        .map_err(|_| BundleError::ResourceAllocation {
            size: requested_size,
        })?;
    bytes.resize(size, 0);
    file.seek(SeekFrom::Start(offset))?;
    file.read_exact(&mut bytes)?;
    Ok(bytes)
}
#[cfg(not(windows))]
fn embed_bundle_resource(_: &Path, _: &Path, package: &Path) -> Result<(), BundleError> {
    let size = std::fs::metadata(package)?.len();
    if size > MAX_RESOURCE_SIZE {
        return Err(BundleError::ResourceTooLarge {
            size,
            limit: MAX_RESOURCE_SIZE,
        });
    }
    Err(BundleError::ResourcesUnavailable)
}

#[cfg(windows)]
fn read_blob_resource(executable: &Path, resource_id: usize) -> Result<Vec<u8>, BundleError> {
    read_resource(executable, resource_id).map_err(|error| match error {
        BundleError::MissingResource => BundleError::Invalid,
        other => other,
    })
}

#[cfg(windows)]
fn read_resource(executable: &Path, resource_id: usize) -> Result<Vec<u8>, BundleError> {
    use std::{os::windows::ffi::OsStrExt, ptr};
    use windows_link::link;
    type Handle = *mut core::ffi::c_void;
    type Dword = u32;
    link!("kernel32.dll" "system" fn LoadLibraryExW(filename: *const u16, file: Handle, flags: Dword) -> Handle);
    link!("kernel32.dll" "system" fn FindResourceW(module: Handle, name: *const u16, resource_type: *const u16) -> Handle);
    link!("kernel32.dll" "system" fn LoadResource(module: Handle, resource: Handle) -> Handle);
    link!("kernel32.dll" "system" fn SizeofResource(module: Handle, resource: Handle) -> Dword);
    link!("kernel32.dll" "system" fn LockResource(resource: Handle) -> *const core::ffi::c_void);
    link!("kernel32.dll" "system" fn FreeLibrary(module: Handle) -> i32);
    link!("kernel32.dll" "system" fn GetLastError() -> Dword);
    let wide: Vec<u16> = executable
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let module = unsafe { LoadLibraryExW(wide.as_ptr(), ptr::null_mut(), 0x0000_0002) };
    if module.is_null() {
        return Err(BundleError::ResourceApi(unsafe { GetLastError() }));
    }
    let resource = unsafe {
        FindResourceW(
            module,
            resource_id as *const u16,
            RESOURCE_TYPE_RCDATA as *const u16,
        )
    };
    let result = if resource.is_null() {
        Err(BundleError::MissingResource)
    } else {
        let size = unsafe { SizeofResource(module, resource) } as usize;
        if size == 0 {
            Err(BundleError::Invalid)
        } else {
            let loaded = unsafe { LoadResource(module, resource) };
            let data = if loaded.is_null() {
                ptr::null()
            } else {
                unsafe { LockResource(loaded) }
            };
            if data.is_null() {
                Err(BundleError::ResourceApi(unsafe { GetLastError() }))
            } else {
                let mut bytes = Vec::new();
                bytes
                    .try_reserve_exact(size)
                    .map_err(|_| BundleError::ResourceAllocation { size: size as u64 })?;
                bytes.extend_from_slice(unsafe {
                    std::slice::from_raw_parts(data.cast::<u8>(), size)
                });
                Ok(bytes)
            }
        }
    };
    unsafe {
        FreeLibrary(module);
    }
    result
}

#[cfg(windows)]
fn extract_bundle_resource(executable: &Path, output: &Path) -> Result<(), BundleError> {
    std::fs::write(output, read_resource(executable, RESOURCE_ID_BUNDLE)?)?;
    Ok(())
}
#[cfg(not(windows))]
fn extract_bundle_resource(_: &Path, _: &Path) -> Result<(), BundleError> {
    Err(BundleError::MissingResource)
}

#[cfg(not(windows))]
fn read_resource(_: &Path, _: usize) -> Result<Vec<u8>, BundleError> {
    Err(BundleError::ResourcesUnavailable)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zup_core::{
        App, AppId, Install, InstallDirectory, InstallScope, NonEmptyString, PluginBinding,
        PluginId, Template,
    };
    use zup_plugin_contract::HOST_TARGET;

    fn plan_with_plugins(count: usize) -> BuildPlan {
        BuildPlan {
            installer: Installer {
                app: App {
                    id: AppId::new("com.example.bundle-limits").unwrap(),
                    name: NonEmptyString::new("Bundle Limits").unwrap(),
                    version: "1.0.0".parse().unwrap(),
                    publisher: None,
                    main: None,
                    description: None,
                },
                updates: None,
                install: Install {
                    scope: InstallScope::User,
                    directory: InstallDirectory {
                        user: Some(
                            Template::parse("${known.local_app_data}/BundleLimits").unwrap(),
                        ),
                        machine: None,
                    },
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
                shortcuts: Vec::new(),
                path: Vec::new(),
                services: Vec::new(),
                protocols: Vec::new(),
                file_types: Vec::new(),
            },
            plugins: Vec::new(),
            files: Vec::new(),
            total_size: 0,
        }
    }

    fn unchecked_artifact(id: &str) -> CompiledPluginArtifact {
        let digest = Sha256Digest::from_bytes([0; 32]);
        CompiledPluginArtifact {
            metadata: PluginArtifact {
                plugin_id: PluginId::new(id).unwrap(),
                source_size: 1,
                source_sha256: digest,
                target: HOST_TARGET.to_owned(),
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
            Err(BundleError::PluginAotTooLarge {
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
            BundleError::PluginAotTooLarge {
                size,
                limit: MAX_PLUGIN_AOT_TOTAL_BYTES,
            } if size == 5 * MAX_AOT_BYTES as u64
        ));
        assert!(!output.exists());
    }
}
