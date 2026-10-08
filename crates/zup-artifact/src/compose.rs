use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use zup_core::Sha256Digest;

use crate::compat::check_compatibility;
use crate::format::ArtifactError;
use crate::format::Descriptor;
use crate::format::{MAX_VARIANTS, MediaType};
use crate::index::{
    ARTIFACT_SCHEMA, ArtifactDescriptor, ArtifactIndex, ArtifactKind, ArtifactMode, ArtifactPin,
    ArtifactSavings, ArtifactTables, FEATURE_CHANNEL_PIN, FEATURE_SHARED_CAS,
    FEATURE_VARIANT_MANIFESTS, LauncherStrategy, VariantContentSet, savings,
};
use crate::table::{BlobEntry, BlobTable};
use crate::variant::{
    DistributionVariant, VariantDescriptor, VariantDescriptorContent, VariantManifest,
};

const CONTENT_LEVEL: i32 = 9;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactRequest {
    pub id: String,
    pub kind: ArtifactKind,
    pub mode: ArtifactMode,
    pub pin: ArtifactPin,
    pub launcher: LauncherStrategy,
    pub output: String,
    pub trust: Option<zup_acquire::OnlineTrust>,
}

impl ArtifactRequest {
    pub fn universal_offline(
        id: impl Into<String>,
        application: &zup_core::App,
        output: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            kind: ArtifactKind::Universal,
            mode: ArtifactMode::Offline,
            pin: ArtifactPin::Pinned {
                version: application.version.clone(),
            },
            launcher: LauncherStrategy::EmbeddedDispatcher,
            output: output.into(),
            trust: None,
        }
    }

    pub fn thin_online(
        id: impl Into<String>,
        application: &zup_core::App,
        trust: zup_acquire::OnlineTrust,
        output: impl Into<String>,
    ) -> Self {
        let pin = match &trust.pin {
            zup_acquire::ReleasePin::Version { version } => ArtifactPin::Pinned {
                version: semver::Version::parse(version)
                    .unwrap_or_else(|_| application.version.clone()),
            },
            zup_acquire::ReleasePin::Channel { channel } => ArtifactPin::Channel {
                channel: channel.clone(),
            },
        };
        Self {
            id: id.into(),
            kind: ArtifactKind::Universal,
            mode: ArtifactMode::Thin,
            pin,
            launcher: LauncherStrategy::EmbeddedDispatcher,
            output: output.into(),
            trust: Some(trust),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ComposedManifest {
    pub id: String,
    pub descriptor: Descriptor,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct ComposedImage {
    pub id: String,
    pub descriptor: Descriptor,
    pub bytes: Vec<u8>,
}

#[derive(Debug)]
pub struct ArtifactGraph {
    index: ArtifactIndex,
    table: BlobTable,
    manifests: Vec<ComposedManifest>,
    runtimes: Vec<ComposedImage>,
    presets: Vec<ComposedImage>,
    segments_root: Option<PathBuf>,
    segment_sizes: Vec<u64>,
    _spool: Option<tempfile::TempDir>,
    savings: ArtifactSavings,
    content: BTreeMap<String, VariantContentSet>,
}

impl ArtifactGraph {
    pub fn index(&self) -> &ArtifactIndex {
        &self.index
    }

    pub fn table(&self) -> &BlobTable {
        &self.table
    }

    pub fn manifests(&self) -> &[ComposedManifest] {
        &self.manifests
    }

    pub fn runtimes(&self) -> &[ComposedImage] {
        &self.runtimes
    }

    pub fn presets(&self) -> &[ComposedImage] {
        &self.presets
    }

    pub fn segments_root(&self) -> Option<&Path> {
        self.segments_root.as_deref()
    }

    pub fn segment_sizes(&self) -> &[u64] {
        &self.segment_sizes
    }

    pub fn savings(&self) -> ArtifactSavings {
        self.savings
    }

    pub fn content_of(&self, id: &str) -> Option<&VariantContentSet> {
        self.content.get(id)
    }

    pub fn segments(&self) -> Result<crate::store::FileSegments, ArtifactError> {
        let root = self
            .segments_root
            .as_ref()
            .ok_or_else(|| ArtifactError::Incomplete {
                id: self.index.artifact.id.clone(),
                detail: "this graph was composed in memory and has no segments on disk".into(),
            })?;
        Ok(crate::store::FileSegments::new(root.clone()))
    }

    pub fn index_bytes(&self) -> Result<Vec<u8>, ArtifactError> {
        self.index.encode()
    }

    pub fn table_bytes(&self) -> Result<Vec<u8>, ArtifactError> {
        self.table.encode()
    }

    pub fn manifest_bytes(&self) -> Result<Vec<(String, Vec<u8>)>, ArtifactError> {
        Ok(self
            .manifests
            .iter()
            .map(|manifest| (manifest.id.clone(), manifest.bytes.clone()))
            .collect())
    }

    pub fn runtime_bytes(&self) -> Result<Vec<(String, Vec<u8>)>, ArtifactError> {
        Ok(self
            .runtimes
            .iter()
            .map(|runtime| (runtime.id.clone(), runtime.bytes.clone()))
            .collect())
    }

    pub fn preset_bytes(&self) -> Vec<(String, Vec<u8>)> {
        self.presets
            .iter()
            .map(|preset| (preset.id.clone(), preset.bytes.clone()))
            .collect()
    }

    pub fn carries_content(&self) -> bool {
        self.index.artifact.mode.carries_content()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CompositionStorage {
    #[default]
    Spooled,
    Memory,
}

#[derive(Debug)]
pub struct ArtifactComposer {
    request: ArtifactRequest,
    storage: CompositionStorage,
    level: i32,
}

impl ArtifactComposer {
    /// incompatible set never costs a hash or a compression pass.
    pub fn new(
        request: ArtifactRequest,
        variants: &[&DistributionVariant],
    ) -> Result<Self, ArtifactError> {
        if variants.is_empty() || variants.len() > MAX_VARIANTS {
            return Err(ArtifactError::TooManyVariants {
                count: variants.len(),
                limit: MAX_VARIANTS,
            });
        }
        if variants.len() > request.kind.variant_capacity() {
            return Err(ArtifactError::TooManyVariants {
                count: variants.len(),
                limit: request.kind.variant_capacity(),
            });
        }
        if request.launcher.requires_shared_subsystem() {
            let subsystem = variants[0].subsystem();
            if variants
                .iter()
                .any(|variant| variant.subsystem() != subsystem)
            {
                return Err(crate::compat::Incompatible {
                    left: variants[0].id().to_owned(),
                    right: variants
                        .iter()
                        .find(|variant| variant.subsystem() != subsystem)
                        .map(|variant| variant.id().to_owned())
                        .unwrap_or_default(),
                    reason: Box::new(crate::compat::Incompatibility::LauncherSubsystem {
                        left: subsystem,
                        right: variants
                            .iter()
                            .find(|variant| variant.subsystem() != subsystem)
                            .map(|variant| variant.subsystem())
                            .unwrap_or(subsystem),
                    }),
                }
                .into());
            }
        }
        check_compatibility(variants)?;
        Ok(Self {
            request,
            storage: CompositionStorage::default(),
            level: CONTENT_LEVEL,
        })
    }

    pub fn with_storage(mut self, storage: CompositionStorage) -> Self {
        self.storage = storage;
        self
    }

    pub fn with_level(mut self, level: i32) -> Self {
        self.level = level;
        self
    }

    pub fn compose(
        self,
        variants: &[&DistributionVariant],
    ) -> Result<ArtifactGraph, ArtifactError> {
        let mut ordered: Vec<&DistributionVariant> = variants.to_vec();
        ordered.sort_by(|left, right| left.id().cmp(right.id()));
        for pair in ordered.windows(2) {
            if pair[0].target() == pair[1].target() {
                return Err(ArtifactError::Invalid);
            }
        }

        let (spool, segments_root) = match self.storage {
            CompositionStorage::Spooled => {
                let directory = tempfile::tempdir()?;
                let root = directory.path().to_path_buf();
                (Some(directory), Some(root))
            }
            CompositionStorage::Memory => (None, None),
        };
        let mut spooled = BlobSpool::new(segments_root.clone());

        let mut unique: BTreeSet<Sha256Digest> = BTreeSet::new();
        for variant in &ordered {
            for digest in variant.content_digests() {
                unique.insert(digest);
            }
        }
        let mut sizes: BTreeMap<Sha256Digest, u64> = BTreeMap::new();
        for digest in &unique {
            let (size, compressed) = self.compress_once(&ordered, digest)?;
            sizes.insert(*digest, size);
            spooled.insert(*digest, &compressed)?;
        }

        let entries = unique
            .iter()
            .map(|digest| BlobEntry {
                digest: *digest,
                segment: 0,
                offset: 0,
                compressed_size: spooled.compressed_size(digest),
                size: sizes[digest],
            })
            .collect::<Vec<_>>();
        let table = match self.request.mode {
            ArtifactMode::Offline => BlobTable::pack(entries)?,
            ArtifactMode::Thin => BlobTable::detached(entries)?,
        };
        spooled.seal(&table)?;

        let mut manifests = Vec::with_capacity(ordered.len());
        let mut runtimes = Vec::with_capacity(ordered.len());
        let mut presets = Vec::with_capacity(ordered.len());
        let mut descriptors = Vec::with_capacity(ordered.len());
        let mut content = BTreeMap::new();
        for variant in &ordered {
            let bytes = VariantManifest::encode(variant)?;
            let manifest_descriptor = Descriptor::of(MediaType::VARIANT_MANIFEST, &bytes);
            let digests = variant.content_digests();
            let set = VariantContentSet {
                id: variant.id().to_owned(),
                unique_blob_count: digests.len() as u64,
                digests,
                logical_size: variant.logical_size(),
            };
            content.insert(variant.id().to_owned(), set);
            let runtime = match (self.request.mode, variant.runtime()) {
                (ArtifactMode::Thin, Some(runtime)) => Some(*runtime),
                (ArtifactMode::Offline, Some(runtime)) => {
                    let bytes = self.runtime_bytes(variant)?;
                    runtimes.push(ComposedImage {
                        id: variant.id().to_owned(),
                        descriptor: *runtime,
                        bytes,
                    });
                    Some(*runtime)
                }
                (ArtifactMode::Offline, None) => {
                    return Err(ArtifactError::Incomplete {
                        id: self.request.id.clone(),
                        detail: format!(
                            "variant `{}` has no native runtime, and an offline artifact must be able to execute it",
                            variant.id()
                        ),
                    });
                }
                (ArtifactMode::Thin, None) => None,
            };
            if self.request.mode == ArtifactMode::Offline
                && let Some(preset) = variant.preset()
            {
                presets.push(ComposedImage {
                    id: variant.id().to_owned(),
                    descriptor: *preset,
                    bytes: self.native_image_bytes(variant, preset)?,
                });
            }
            let content_entry = content[variant.id()].clone();
            descriptors.push(VariantDescriptor {
                id: variant.id().to_owned(),
                target: variant.target().clone(),
                platform: variant.platform().clone(),
                frontend: variant.frontend(),
                manifest: manifest_descriptor,
                requirements: variant.requirements().clone(),
                runtime,
                preset: variant.preset().cloned(),
                content: VariantDescriptorContent {
                    logical_size: variant.logical_size(),
                    blob_count: content_entry.digests.len() as u64,
                    unique_blob_count: content_entry.unique_blob_count,
                    file_count: variant.plan().entries.len() as u64,
                    prerequisite_count: variant.plan().prerequisite_artifacts.len() as u64,
                    plugin_count: variant.plan().plugins.len() as u64,
                },
                logical_size: variant
                    .logical_size()
                    .saturating_add(variant.runtime().map_or(0, |runtime| runtime.size)),
            });
            manifests.push(ComposedManifest {
                id: variant.id().to_owned(),
                descriptor: manifest_descriptor,
                bytes,
            });
        }

        let application = ordered[0].application().clone();
        let index = ArtifactIndex {
            schema: ARTIFACT_SCHEMA,
            required_features: FEATURE_SHARED_CAS | FEATURE_VARIANT_MANIFESTS | FEATURE_CHANNEL_PIN,
            media_type: MediaType::INDEX,
            artifact: ArtifactDescriptor {
                id: self.request.id.clone(),
                kind: self.request.kind,
                mode: self.request.mode,
                pin: self.request.pin,
                application,
                launcher: self.request.launcher,
                subsystem: ordered[0].subsystem(),
                output: self.request.output.clone(),
                trust: self.request.trust.clone(),
            },
            tables: ArtifactTables {
                blobs: Descriptor::of(MediaType::BLOB_TABLE, &table.encode()?),
            },
            variants: descriptors,
        };
        index.validate()?;
        let savings = savings(&content.values().cloned().collect::<Vec<_>>(), &table);
        let segment_sizes = (0..table.segments)
            .map(|segment| table.segment_size(segment))
            .collect();
        Ok(ArtifactGraph {
            index,
            table,
            manifests,
            runtimes,
            presets,
            segments_root,
            segment_sizes,
            _spool: spool,
            savings,
            content,
        })
    }

    fn runtime_bytes(&self, variant: &DistributionVariant) -> Result<Vec<u8>, ArtifactError> {
        let runtime = variant.runtime().ok_or_else(|| ArtifactError::Incomplete {
            id: self.request.id.clone(),
            detail: format!("variant `{}` has no native runtime image", variant.id()),
        })?;
        self.native_image_bytes(variant, runtime)
    }

    fn native_image_bytes(
        &self,
        variant: &DistributionVariant,
        image: &Descriptor,
    ) -> Result<Vec<u8>, ArtifactError> {
        let sources = variant.sources();
        if let Some(bytes) = sources.bytes(&image.digest) {
            return Ok(bytes.to_vec());
        }
        let path = sources
            .file(&image.digest)
            .ok_or_else(|| ArtifactError::Missing {
                media_type: image.media_type.label(),
                digest: image.digest.to_hex(),
            })?;
        let bytes = std::fs::read(path)?;
        if bytes.len() as u64 != image.size {
            return Err(ArtifactError::SizeMismatch {
                media_type: image.media_type.label(),
                digest: image.digest.to_hex(),
                expected: image.size,
                found: bytes.len() as u64,
            });
        }
        Ok(bytes)
    }

    fn compress_once(
        &self,
        variants: &[&DistributionVariant],
        digest: &Sha256Digest,
    ) -> Result<(u64, Vec<u8>), ArtifactError> {
        let mut expected: Option<u64> = None;
        let mut compressed: Option<Vec<u8>> = None;
        for variant in variants {
            if !variant.content_digests().contains(digest) {
                continue;
            }
            let (size, bytes) = self.compress_source(variant, digest)?;
            if let Some(expected) = expected
                && expected != size
            {
                return Err(ArtifactError::Invalid);
            }
            expected = Some(size);
            compressed.get_or_insert(bytes);
        }
        Ok((expected.unwrap_or(0), compressed.unwrap_or_default()))
    }

    fn compress_source(
        &self,
        variant: &DistributionVariant,
        digest: &Sha256Digest,
    ) -> Result<(u64, Vec<u8>), ArtifactError> {
        let sources = variant.sources();
        let mut raw: Box<dyn Read> = match sources.bytes(digest) {
            Some(bytes) => Box::new(std::io::Cursor::new(bytes)),
            None => {
                let path = sources.file(digest).ok_or_else(|| ArtifactError::Missing {
                    media_type: MediaType::BLOB.label(),
                    digest: digest.to_hex(),
                })?;
                Box::new(BufReader::new(File::open(path)?))
            }
        };
        let mut hasher = Sha256::new();
        let mut sink = CompressingSink::new(self.level)?;
        let mut buffer = vec![0u8; 64 * 1024];
        let mut size = 0u64;
        loop {
            let read = raw.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
            size = size
                .checked_add(read as u64)
                .ok_or(ArtifactError::Invalid)?;
            sink.push(&buffer[..read])?;
        }
        let bytes = sink.finish()?;
        if Sha256Digest::from_hasher(hasher) != *digest {
            return Err(ArtifactError::DigestMismatch {
                media_type: MediaType::BLOB.label(),
                digest: digest.to_hex(),
            });
        }
        Ok((size, bytes))
    }
}

/// Streams bytes into a Zstandard frame, so a large blob is never buffered
struct CompressingSink {
    encoder: zstd::stream::write::Encoder<'static, Vec<u8>>,
}

impl CompressingSink {
    fn new(level: i32) -> Result<Self, ArtifactError> {
        let encoder = zstd::stream::write::Encoder::new(Vec::new(), level)?;
        Ok(Self { encoder })
    }

    fn push(&mut self, bytes: &[u8]) -> Result<(), ArtifactError> {
        self.encoder.write_all(bytes)?;
        Ok(())
    }

    fn finish(self) -> Result<Vec<u8>, ArtifactError> {
        Ok(self.encoder.finish()?)
    }
}

enum BlobSpool {
    Memory {
        segments: BTreeMap<u16, Vec<u8>>,
        sizes: BTreeMap<Sha256Digest, u64>,
    },
    Disk {
        root: PathBuf,
        pending: BTreeMap<Sha256Digest, Vec<u8>>,
        sizes: BTreeMap<Sha256Digest, u64>,
    },
}

impl BlobSpool {
    fn new(root: Option<PathBuf>) -> Self {
        match root {
            Some(root) => Self::Disk {
                root,
                pending: BTreeMap::new(),
                sizes: BTreeMap::new(),
            },
            None => Self::Memory {
                segments: BTreeMap::new(),
                sizes: BTreeMap::new(),
            },
        }
    }

    fn insert(&mut self, digest: Sha256Digest, compressed: &[u8]) -> Result<(), ArtifactError> {
        let size = compressed.len() as u64;
        match self {
            Self::Memory { segments, sizes } => {
                segments.insert(0, compressed.to_vec());
                sizes.insert(digest, size);
            }
            Self::Disk { pending, sizes, .. } => {
                pending.insert(digest, compressed.to_vec());
                sizes.insert(digest, size);
            }
        }
        Ok(())
    }

    fn compressed_size(&self, digest: &Sha256Digest) -> u64 {
        let (Self::Disk { sizes, .. } | Self::Memory { sizes, .. }) = self;
        sizes.get(digest).copied().unwrap_or(0)
    }

    fn seal(&mut self, table: &BlobTable) -> Result<(), ArtifactError> {
        let root = match self {
            Self::Disk { root, .. } => root.clone(),
            Self::Memory { .. } => return Ok(()),
        };
        let Self::Disk { pending, .. } = self else {
            unreachable!("only a disk spool seals into files")
        };
        std::fs::create_dir_all(&root)?;
        let mut writers: BTreeMap<u16, File> = BTreeMap::new();
        for entry in table.entries() {
            let bytes = pending
                .remove(&entry.digest)
                .ok_or(ArtifactError::Invalid)?;
            if bytes.len() as u64 != entry.compressed_size {
                return Err(ArtifactError::Invalid);
            }
            let file = match writers.entry(entry.segment) {
                std::collections::btree_map::Entry::Occupied(slot) => slot.into_mut(),
                std::collections::btree_map::Entry::Vacant(slot) => slot.insert(File::create(
                    root.join(format!("segment-{:05}", entry.segment)),
                )?),
            };
            file.write_all(&bytes)?;
        }
        for file in writers.into_values() {
            file.sync_all()?;
        }
        Ok(())
    }
}
