//! The GitHub transport package: a container, not an installer.
//!
//! # What it is for
//!
//! GitHub's release page should read like a release page. A project with nine
//! thousand content objects must not turn that into nine thousand assets, and
//! the reason it must not is not aesthetic: the per-release asset limit is a
//! thousand, the Release UI is unusable past a few dozen, and a repository
//! whose graph has become a list of hexadecimal filenames has stopped being a
//! graph.
//!
//! So content travels as **one package per variant**:
//!
//! ```text
//! Acme-Windows-x64.zup
//! Acme-Windows-arm64.zup
//! ```
//!
//! and the acquisition engine reads a package the way it reads any other
//! physical representation of the same objects. After import, a blob is an
//! ordinary verified CAS object at a path derived from its digest, and nothing
//! downstream - selection, repair, update, the installer lifecycle - can tell
//! whether it arrived from a per-blob CDN, a GitHub package, an offline
//! artefact, or a directory on a share.
//!
//! # The layout
//!
//! ```text
//! offset 0   "ZUPGPKG\0"  8 bytes magic
//! offset 8   u32 LE       schema
//! offset 12  u64 LE       required feature bits
//! offset 20  u64 LE       metadata length
//! offset 28  [u8; 32]     SHA-256 of the metadata bytes
//! offset 60  metadata     JSON, SHA-256 protected
//!            frames       one Zstandard frame per blob, ascending digest
//! ```
//!
//! # Why the header is a header
//!
//! Because sharding needs somewhere to say where the shards are, and the first
//! shard is where the answer should be. A sharded package's descriptor is
//! therefore readable from shard 0 alone, which is what lets a content source
//! open a package by fetching one small file rather than a directory listing.
//!
//! # Identity is not repackaged
//!
//! A frame's digest is the digest of the *uncompressed* blob, byte for byte the
//! digest the content catalog names. Packing a hundred blobs into one file
//! changes how they are transported and nothing about what they are, which is
//! what makes moving a project from GitHub-only distribution to an R2 or CDN CAS
//! a configuration change rather than a content migration.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use zup_core::Sha256Digest;

use crate::error::PackageError;

/// The container's magic, including its terminator so a truncated read is
/// refused rather than matched.
pub const MAGIC: &[u8; 8] = b"ZUPGPKG\0";

/// The version of the container this build writes and reads.
pub const SCHEMA: u32 = 1;

/// The fixed header length, in bytes.
pub const HEADER_LEN: usize = 60;

/// The Zstandard level frames are written at.
///
/// Nine is the level the artifact composer already uses for the same content, so
/// a project does not get two compression profiles for the same blobs depending
/// on which transport carried them.
pub const LEVEL: i32 = 9;

/// The largest metadata block this reader will accept.
///
/// A megabyte is a variant manifest, a content catalog for a few thousand
/// objects, and a shard map. Anything larger is not a package descriptor.
pub const MAX_METADATA: u64 = 16 * 1024 * 1024;

/// Feature bit: the package carries blob frames.
pub const FEATURE_FRAMES: u64 = 1 << 0;

/// Every feature bit this build understands.
pub const SUPPORTED_FEATURES: u64 = FEATURE_FRAMES;

/// One blob's position in the package.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Frame {
    pub digest: Sha256Digest,
    /// Byte offset of the frame within the *logical* package.
    ///
    /// Logical, not per-shard, so a sharded package and an unsharded one have
    /// the same index and the acquisition engine does not have to care which it
    /// was handed.
    pub offset: u64,
    /// Bytes the compressed frame takes.
    pub compressed_size: u64,
    /// Bytes the blob decompresses to.
    pub size: u64,
}

/// One piece of a sharded package, as the package itself can describe it.
///
/// Deliberately thinner than [`crate::ShardRef`]: a piece's *name* and *digest*
/// cannot be written into the package, because the name is chosen by whoever
/// stages the release and the digest is a hash of bytes that are not finished
/// until the metadata that would carry it is. Both are recorded in the
/// authenticated descriptor, which is the one that decides, and a package that
/// carried placeholder digests of its own pieces would be asserting something
/// false in a file meant to be believed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Shard {
    pub index: u32,
    /// The piece's length.
    pub size: u64,
    /// Where this piece starts within the logical package.
    pub start: u64,
}

/// What a package holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Metadata {
    pub schema: u32,
    pub required_features: u64,
    /// The variant this package carries.
    pub variant: String,
    /// That variant's canonical target triple.
    pub target: String,
    /// The application the release is for.
    pub application: String,
    /// The variant manifest's own digest and length.
    pub manifest: Document,
    /// The content catalog this package's blobs come from.
    pub catalog: Document,
    /// One frame per blob, ascending by digest.
    pub blobs: Vec<Frame>,
    /// The pieces of this package. One entry means it is not sharded.
    pub shards: Vec<Shard>,
}

impl Metadata {
    /// The frame for `digest`, if the package has it.
    pub fn frame(&self, digest: &Sha256Digest) -> Option<&Frame> {
        self.blobs
            .binary_search_by(|frame| frame.digest.cmp(digest))
            .ok()
            .map(|index| &self.blobs[index])
    }

    /// The piece of the logical package that holds byte `offset`.
    pub fn shard_at(&self, offset: u64) -> Option<&Shard> {
        self.shards
            .iter()
            .find(|shard| offset >= shard.start && offset < shard.start.saturating_add(shard.size))
    }

    /// Whether the frames are in the order the index promises.
    fn frames_sorted(&self) -> bool {
        self.blobs
            .windows(2)
            .all(|pair| pair[0].digest < pair[1].digest)
    }

    fn validate(&self) -> Result<(), PackageError> {
        if self.schema != SCHEMA {
            return Err(PackageError::Schema {
                found: self.schema,
                schema: SCHEMA,
            });
        }
        if self.required_features & !SUPPORTED_FEATURES != 0 {
            return Err(PackageError::Features {
                required: self.required_features,
                supported: SUPPORTED_FEATURES,
            });
        }
        if self.variant.is_empty() {
            return Err(PackageError::Field("variant"));
        }
        if self.target.is_empty() {
            return Err(PackageError::Field("target"));
        }
        if !self.frames_sorted() {
            return Err(PackageError::Order("frames"));
        }
        if self.shards.is_empty() {
            return Err(PackageError::Field("shards"));
        }
        for (index, shard) in self.shards.iter().enumerate() {
            if usize::try_from(shard.index).ok() != Some(index) {
                return Err(PackageError::Order("shards"));
            }
        }
        for frame in &self.blobs {
            if frame.size == 0 {
                return Err(PackageError::Field("frame size"));
            }
            if self.shard_at(frame.offset).is_none() {
                return Err(PackageError::Unmapped {
                    offset: frame.offset,
                });
            }
        }
        Ok(())
    }
}

/// A document's identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Document {
    pub digest: Sha256Digest,
    pub size: u64,
}

/// The header, in the order it is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub schema: u32,
    pub required_features: u64,
    pub metadata_len: u64,
    pub metadata_digest: Sha256Digest,
}

impl Header {
    /// The 60 header bytes.
    pub fn to_bytes(self) -> [u8; HEADER_LEN] {
        let mut out = [0u8; HEADER_LEN];
        out[..8].copy_from_slice(MAGIC);
        out[8..12].copy_from_slice(&self.schema.to_le_bytes());
        out[12..20].copy_from_slice(&self.required_features.to_le_bytes());
        out[20..28].copy_from_slice(&self.metadata_len.to_le_bytes());
        out[28..60].copy_from_slice(self.metadata_digest.as_bytes());
        out
    }

    /// Read a header, refusing anything that is not this container.
    pub fn parse(bytes: &[u8]) -> Result<Self, PackageError> {
        if bytes.len() < HEADER_LEN {
            return Err(PackageError::Short {
                expected: HEADER_LEN as u64,
                found: bytes.len() as u64,
            });
        }
        if &bytes[..8] != MAGIC {
            return Err(PackageError::Magic);
        }
        let schema = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
        let mut features = [0u8; 8];
        features.copy_from_slice(&bytes[12..20]);
        let mut length = [0u8; 8];
        length.copy_from_slice(&bytes[20..28]);
        let mut raw = [0u8; 32];
        raw.copy_from_slice(&bytes[28..60]);
        let digest = Sha256Digest::from_bytes(raw);
        Ok(Self {
            schema,
            required_features: u64::from_le_bytes(features),
            metadata_len: u64::from_le_bytes(length),
            metadata_digest: digest,
        })
    }
}

/// A parsed package, held as its index rather than its bytes.
///
/// A source that opened a multi-gigabyte package into memory to read a thousand
/// lines of JSON would defeat the point of the format, so opening one means
/// reading the header and the metadata and nothing else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Index {
    pub header: Header,
    pub metadata: Metadata,
    /// Where the frames begin, in the logical package.
    pub frames_start: u64,
    /// The whole logical package's length.
    pub total: u64,
}

impl Index {
    /// Read the index from the front of `bytes`.
    pub fn read(bytes: &[u8]) -> Result<Self, PackageError> {
        let header = Header::parse(bytes)?;
        if header.metadata_len > MAX_METADATA {
            return Err(PackageError::Metadata {
                limit: MAX_METADATA,
                found: header.metadata_len,
            });
        }
        let end =
            (HEADER_LEN as u64)
                .checked_add(header.metadata_len)
                .ok_or(PackageError::Short {
                    expected: u64::MAX,
                    found: bytes.len() as u64,
                })?;
        if (bytes.len() as u64) < end {
            return Err(PackageError::Short {
                expected: end,
                found: bytes.len() as u64,
            });
        }
        let raw = &bytes[HEADER_LEN..end as usize];
        // The metadata is authenticated by the header before it is parsed. A
        // package whose index has been altered is refused rather than believed,
        // because the index is what says which frame is which digest.
        let found = zup_core::hash_bytes(raw);
        if found != header.metadata_digest {
            return Err(PackageError::MetadataDigest {
                expected: header.metadata_digest.to_hex(),
                found: found.to_hex(),
            });
        }
        let metadata: Metadata =
            serde_json::from_slice(raw).map_err(|error| PackageError::Json(error.to_string()))?;
        metadata.validate()?;
        let total = metadata
            .shards
            .last()
            .map(|shard| shard.start.saturating_add(shard.size))
            .unwrap_or(end);
        Ok(Self {
            header,
            metadata,
            frames_start: end,
            total,
        })
    }

    /// The frames this package carries, keyed by digest.
    pub fn frames(&self) -> BTreeMap<Sha256Digest, Frame> {
        self.metadata
            .blobs
            .iter()
            .map(|frame| (frame.digest, *frame))
            .collect()
    }
}

/// Builds a package from already-compressed frames.
///
/// The frames are handed in rather than compressed here so that the same
/// compressed bytes can be shared between every package that carries a blob,
/// which is what makes a per-variant package cost the union of its blobs rather
/// than the sum.
#[derive(Debug, Default)]
pub struct Writer {
    frames: BTreeMap<Sha256Digest, (Vec<u8>, u64)>,
    order: Vec<Sha256Digest>,
}

impl Writer {
    /// A writer with nothing in it.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add one blob, given its compressed bytes and its logical size.
    pub fn insert(
        &mut self,
        digest: Sha256Digest,
        compressed: Vec<u8>,
        size: u64,
    ) -> Result<(), PackageError> {
        if let Some((existing, existing_size)) = self.frames.get(&digest) {
            // The same digest twice means the same bytes twice. Disagreeing about
            // the logical size means one of the two is not what its digest says,
            // and a package must not carry both.
            if existing != &compressed || *existing_size != size {
                return Err(PackageError::FrameConflict { digest });
            }
            return Ok(());
        }
        self.frames.insert(digest, (compressed, size));
        self.order.push(digest);
        Ok(())
    }

    /// Compress and add one blob.
    pub fn compress(&mut self, digest: Sha256Digest, bytes: &[u8]) -> Result<(), PackageError> {
        let compressed = zstd::stream::encode_all(bytes, LEVEL)
            .map_err(|error| PackageError::Json(error.to_string()))?;
        self.insert(digest, compressed, bytes.len() as u64)
    }

    /// How many blobs are in the package.
    pub fn len(&self) -> usize {
        self.order.len()
    }

    /// Whether the package is empty.
    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    /// The frames, in ascending digest order.
    fn sorted(&self) -> Vec<(Sha256Digest, &Vec<u8>, u64)> {
        self.frames
            .iter()
            .map(|(digest, (compressed, size))| (*digest, compressed, *size))
            .collect()
    }

    /// Write the package, splitting it into pieces of at most `shard_bytes`.
    ///
    /// The sink is called once per piece, in order, with the piece's index. So a
    /// caller streams straight to files and never holds the whole package, which
    /// matters because a package is allowed to be larger than memory.
    ///
    /// Returns the metadata, so a caller can publish the authenticated
    /// descriptor that names the pieces it just wrote.
    ///
    /// # Why this iterates
    ///
    /// A frame's offset depends on where the frames start, which is the length of
    /// the metadata, which contains the offsets. The length is therefore a fixed
    /// point rather than a value, and it is found by iterating until two passes
    /// agree. The sequence is monotonically non-decreasing and each pass changes
    /// it by at most a few bytes, so it converges in a handful of rounds; the
    /// bound is there so a pathological input cannot loop.
    pub fn write(
        &self,
        header: PackageHeader,
        shard_bytes: u64,
        mut sink: impl FnMut(usize, &[u8]) -> std::io::Result<()>,
    ) -> Result<Metadata, PackageError> {
        let layout = self.sorted();
        let mut metadata_len = 0u64;
        for _ in 0..MAX_LAYOUT_PASSES {
            let (frames, shards) = layout_offsets(&layout, metadata_len, shard_bytes);
            let metadata = self.metadata(&header, &frames, shards);
            let encoded = encode_metadata(&metadata)?;
            if encoded.len() as u64 > MAX_METADATA {
                return Err(PackageError::Metadata {
                    limit: MAX_METADATA,
                    found: encoded.len() as u64,
                });
            }
            if encoded.len() as u64 == metadata_len {
                let head = Header {
                    schema: SCHEMA,
                    required_features: FEATURE_FRAMES,
                    metadata_len,
                    metadata_digest: zup_core::hash_bytes(&encoded),
                };
                emit(
                    &head.to_bytes(),
                    &encoded,
                    &layout,
                    &metadata.shards,
                    &mut sink,
                )
                .map_err(|error| PackageError::Io(error.to_string()))?;
                return Ok(metadata);
            }
            metadata_len = encoded.len() as u64;
        }
        Err(PackageError::UnstableMetadata)
    }

    fn metadata(&self, header: &PackageHeader, frames: &[Frame], shards: Vec<Shard>) -> Metadata {
        Metadata {
            schema: SCHEMA,
            required_features: FEATURE_FRAMES,
            variant: header.variant.clone(),
            target: header.target.clone(),
            application: header.application.clone(),
            manifest: header.manifest,
            catalog: header.catalog,
            blobs: frames.to_vec(),
            shards,
        }
    }
}

/// How many times the metadata length is recomputed before giving up.
const MAX_LAYOUT_PASSES: usize = 8;

/// Call the sink once per piece, in order.
///
/// The logical package is three regions - header, metadata, frames - and a piece
/// is a contiguous range across them, so a piece is built by taking each region's
/// intersection with the range rather than by buffering the whole package.
fn emit(
    head: &[u8],
    metadata: &[u8],
    layout: &[(Sha256Digest, &Vec<u8>, u64)],
    shards: &[Shard],
    sink: &mut impl FnMut(usize, &[u8]) -> std::io::Result<()>,
) -> std::io::Result<()> {
    let mut regions: Vec<(&[u8], u64)> = vec![(head, 0)];
    let mut cursor = head.len() as u64;
    regions.push((metadata, cursor));
    cursor += metadata.len() as u64;
    for (_, compressed, _) in layout {
        regions.push((compressed.as_slice(), cursor));
        cursor += compressed.len() as u64;
    }
    for (index, shard) in shards.iter().enumerate() {
        let start = shard.start;
        let end = shard.start.saturating_add(shard.size);
        let mut piece = Vec::with_capacity((end - start) as usize);
        for (bytes, base) in &regions {
            let region_end = base + bytes.len() as u64;
            let from = start.max(*base);
            let to = end.min(region_end);
            if to > from {
                piece.extend_from_slice(&bytes[(from - base) as usize..(to - base) as usize]);
            }
        }
        sink(index, &piece)?;
    }
    Ok(())
}

/// Place a frame's offset for each frame, and the shard boundaries that follow.
///
/// Piece 0 begins at byte 0 and therefore holds the header and the metadata as
/// well as whatever frames fit. That is what makes a sharded package openable
/// from its first piece alone, and it is also what makes the pieces tile the
/// whole package: a boundary that started after the metadata would leave those
/// bytes in no piece at all.
fn layout_offsets(
    layout: &[(Sha256Digest, &Vec<u8>, u64)],
    metadata_len: u64,
    shard_bytes: u64,
) -> (Vec<Frame>, Vec<Shard>) {
    let start = HEADER_LEN as u64 + metadata_len;
    let mut frames = Vec::with_capacity(layout.len());
    let mut shards: Vec<Shard> = Vec::new();
    let mut offset = start;
    let mut index = 0u32;
    let mut shard_start = 0u64;
    // The header and the metadata are already in piece 0, whether or not any
    // frame follows them.
    let mut shard_size = start;
    for (digest, compressed, size) in layout {
        let length = compressed.len() as u64;
        if !layout.is_empty() && shard_size + length > shard_bytes && shard_size > start {
            shards.push(Shard {
                index,
                size: shard_size,
                start: shard_start,
            });
            index += 1;
            shard_start = offset;
            shard_size = 0;
        }
        frames.push(Frame {
            digest: *digest,
            offset,
            compressed_size: length,
            size: *size,
        });
        shard_size += length;
        offset += length;
    }
    shards.push(Shard {
        index,
        size: shard_size,
        start: shard_start,
    });
    (frames, shards)
}

fn encode_metadata(metadata: &Metadata) -> Result<Vec<u8>, PackageError> {
    serde_json::to_vec(metadata).map_err(|error| PackageError::Json(error.to_string()))
}

/// What a package is built around.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageHeader {
    pub variant: String,
    pub target: String,
    pub application: String,
    /// The variant manifest's own identity, which the package carries by digest
    /// so a reader can check that the manifest it holds is the one the release
    /// named.
    pub manifest: Document,
    pub catalog: Document,
}

impl PackageHeader {
    /// A header for one variant.
    pub fn new(
        variant: impl Into<String>,
        target: impl Into<String>,
        application: impl Into<String>,
        manifest: Document,
        catalog: Document,
    ) -> Self {
        Self {
            variant: variant.into(),
            target: target.into(),
            application: application.into(),
            manifest,
            catalog,
        }
    }
}

/// Decompress one frame and check it against the digest the index named.
///
/// The acquisition path does not use this: it hands the frame to the cache in its
/// wire form and lets the cache decode, bound, and verify in one pass. It is here
/// for a reader that needs to inspect a frame without storing it - a `zup doctor`
/// content check, a benchmark - and it is the reference for what the cache does.
pub fn decode_frame(
    compressed: &[u8],
    expected: Sha256Digest,
    limit: u64,
) -> Result<Vec<u8>, PackageError> {
    let bytes = zstd::stream::decode_all(compressed)
        .map_err(|error| PackageError::Compression(error.to_string()))?;
    if bytes.len() as u64 > limit {
        return Err(PackageError::Expansion {
            limit,
            found: bytes.len() as u64,
        });
    }
    let found = zup_core::hash_bytes(&bytes);
    if found != expected {
        return Err(PackageError::BlobDigest {
            expected: expected.to_hex(),
            found: found.to_hex(),
        });
    }
    Ok(bytes)
}
