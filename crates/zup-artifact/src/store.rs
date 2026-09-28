//! Content sources: the platform-neutral read/write boundary.
//!
//! A selected variant needs bytes it does not own. Those bytes may sit in the
//! executable that started it, in a universal artifact that has not chosen a
//! variant yet, in a store the installation persisted, or somewhere a release
//! publishes. Nothing above this module changes when that answer changes, which
//! is the whole point: the lifecycle planner only ever asks for a portable path,
//! a digest, and a length.
//!
//! Every read is verified against its descriptor. A source that cannot prove the
//! bytes it returns does not return them.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufReader, Cursor, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use zup_core::Sha256Digest;

use crate::descriptor::Descriptor;
use crate::error::ArtifactError;
use crate::index::ArtifactIndex;
use crate::media_type::MediaType;
use crate::table::{BlobEntry, BlobTable};
use crate::variant::VariantManifest;

/// Reads verified artifact content by descriptor.
pub trait ContentSource {
    /// Read the whole of `descriptor`, verifying its length and digest.
    fn read(&self, descriptor: &Descriptor) -> Result<Vec<u8>, ArtifactError>;

    /// Whether this source can produce `descriptor` at all.
    fn contains(&self, descriptor: &Descriptor) -> bool;
}

/// A source backed by bytes already in memory.
#[derive(Debug, Clone, Default)]
pub struct MemorySource {
    entries: BTreeMap<(u64, &'static str), Vec<u8>>,
}

impl MemorySource {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add verified content. The bytes must match `descriptor`.
    pub fn insert(&mut self, descriptor: &Descriptor, bytes: Vec<u8>) -> Result<(), ArtifactError> {
        descriptor.verify(&bytes)?;
        self.entries
            .insert((descriptor.size, descriptor.media_type.as_str()), bytes);
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl ContentSource for MemorySource {
    fn read(&self, descriptor: &Descriptor) -> Result<Vec<u8>, ArtifactError> {
        let bytes = self
            .entries
            .get(&(descriptor.size, descriptor.media_type.as_str()))
            .ok_or_else(|| ArtifactError::Missing {
                media_type: descriptor.label(),
                digest: descriptor.digest.to_hex(),
            })?;
        descriptor.verify(bytes)?;
        Ok(bytes.clone())
    }

    fn contains(&self, descriptor: &Descriptor) -> bool {
        self.entries
            .contains_key(&(descriptor.size, descriptor.media_type.as_str()))
    }
}

/// A source backed by one file per digest, named by the digest itself.
///
/// Composition writes into this shape so a multi-gigabyte store never has to
/// exist in memory, and a thin fetch writes into the same shape, so the
/// difference between "already here" and "just fetched" is not visible above
/// this module.
#[derive(Debug, Clone)]
pub struct SpoolSource {
    root: PathBuf,
    present: BTreeMap<Sha256Digest, u64>,
}

impl SpoolSource {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            present: BTreeMap::new(),
        }
    }

    /// The directory holding the spooled files.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The file a digest is or would be spooled at.
    pub fn path(&self, digest: &Sha256Digest) -> PathBuf {
        self.root.join(digest.to_hex())
    }

    /// Spool `bytes` at `digest`, verifying the content first.
    pub fn write(&mut self, digest: &Sha256Digest, bytes: &[u8]) -> Result<u64, ArtifactError> {
        let actual = Sha256Digest::from_bytes(Sha256::digest(bytes).into());
        if actual != *digest {
            return Err(ArtifactError::DigestMismatch {
                media_type: MediaType::BLOB.label(),
                digest: digest.to_hex(),
            });
        }
        std::fs::create_dir_all(&self.root)?;
        let path = self.path(digest);
        let mut file = File::create(&path)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        let size = bytes.len() as u64;
        self.present.insert(*digest, size);
        Ok(size)
    }

    /// Spool a readable stream at `digest`, writing through a temporary file so
    /// a partial transfer is never visible under its own digest.
    pub fn write_from(
        &mut self,
        digest: &Sha256Digest,
        mut reader: impl Read,
    ) -> Result<u64, ArtifactError> {
        std::fs::create_dir_all(&self.root)?;
        let temporary = self.root.join(format!("{}.partial", digest.to_hex()));
        let mut file = File::create(&temporary)?;
        let mut hasher = Sha256::new();
        let mut size = 0u64;
        let mut buffer = vec![0u8; 64 * 1024];
        loop {
            let read = reader.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
            file.write_all(&buffer[..read])?;
            size = size
                .checked_add(read as u64)
                .ok_or(ArtifactError::Invalid)?;
        }
        file.sync_all()?;
        drop(file);
        if Sha256Digest::from_hasher(hasher) != *digest {
            let _ = std::fs::remove_file(&temporary);
            return Err(ArtifactError::DigestMismatch {
                media_type: MediaType::BLOB.label(),
                digest: digest.to_hex(),
            });
        }
        let path = self.path(digest);
        if std::fs::rename(&temporary, &path).is_err() {
            let _ = std::fs::remove_file(&temporary);
            return Err(ArtifactError::DigestMismatch {
                media_type: MediaType::BLOB.label(),
                digest: digest.to_hex(),
            });
        }
        self.present.insert(*digest, size);
        Ok(size)
    }

    /// Forget a digest, for collection after a selection changes.
    pub fn forget(&mut self, digest: &Sha256Digest) {
        self.present.remove(digest);
        let _ = std::fs::remove_file(self.path(digest));
    }

    /// Every digest this source currently carries.
    pub fn digests(&self) -> impl Iterator<Item = Sha256Digest> + '_ {
        self.present.keys().copied()
    }

    pub fn len(&self) -> usize {
        self.present.len()
    }

    pub fn is_empty(&self) -> bool {
        self.present.is_empty()
    }

    /// Verify every spooled file against its digest.
    pub fn verify_all(&self) -> Result<(), ArtifactError> {
        for (digest, size) in &self.present {
            let path = self.path(digest);
            let (actual_size, actual) = zup_core::hash_reader(BufReader::new(File::open(&path)?))?;
            if actual_size != *size || actual != *digest {
                return Err(ArtifactError::DigestMismatch {
                    media_type: MediaType::BLOB.label(),
                    digest: digest.to_hex(),
                });
            }
        }
        Ok(())
    }
}

impl ContentSource for SpoolSource {
    fn read(&self, descriptor: &Descriptor) -> Result<Vec<u8>, ArtifactError> {
        let path = self.path(&descriptor.digest);
        let mut file = File::open(&path).map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => ArtifactError::Missing {
                media_type: descriptor.label(),
                digest: descriptor.digest.to_hex(),
            },
            _ => ArtifactError::Io(error),
        })?;
        let metadata = file.metadata()?;
        if metadata.len() != descriptor.size {
            return Err(ArtifactError::SizeMismatch {
                media_type: descriptor.label(),
                digest: descriptor.digest.to_hex(),
                expected: descriptor.size,
                found: metadata.len(),
            });
        }
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(
                usize::try_from(descriptor.size).map_err(|_| ArtifactError::Invalid)?,
            )
            .map_err(|_| ArtifactError::Allocation {
                size: descriptor.size,
            })?;
        file.seek(SeekFrom::Start(0))?;
        file.read_to_end(&mut bytes)?;
        descriptor.verify(&bytes)?;
        Ok(bytes)
    }

    fn contains(&self, descriptor: &Descriptor) -> bool {
        self.present.contains_key(&descriptor.digest)
    }
}

/// Reads a byte range of one segment of a composed store.
pub trait SegmentReader: Send + Sync {
    /// Read `len` bytes at `offset` within `segment`.
    fn read_range(&self, segment: u16, offset: u64, len: u64) -> Result<Vec<u8>, ArtifactError>;

    /// How many segments this reader can address. A reader reports zero for a
    /// store that carries no bytes, which is what a thin artifact is.
    fn segment_count(&self) -> u16;
}

/// Segment reader over a directory of segment files.
#[derive(Debug, Clone)]
pub struct FileSegments {
    root: PathBuf,
}

impl FileSegments {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The file a segment lives at.
    pub fn path(&self, segment: u16) -> PathBuf {
        self.root.join(format!("segment-{segment:05}"))
    }
}

impl SegmentReader for FileSegments {
    fn read_range(&self, segment: u16, offset: u64, len: u64) -> Result<Vec<u8>, ArtifactError> {
        let mut file = File::open(self.path(segment))?;
        file.seek(SeekFrom::Start(offset))?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(usize::try_from(len).map_err(|_| ArtifactError::Invalid)?)
            .map_err(|_| ArtifactError::Allocation { size: len })?;
        let read = Read::by_ref(&mut file.take(len)).read_to_end(&mut bytes)?;
        if read as u64 != len {
            return Err(ArtifactError::Invalid);
        }
        Ok(bytes)
    }

    fn segment_count(&self) -> u16 {
        let mut segments = 0u16;
        while self.path(segments).exists() && segments < crate::media_type::MAX_SEGMENTS {
            segments += 1;
        }
        segments
    }
}

/// Segment reader over segments held in memory.
#[derive(Debug, Clone, Default)]
pub struct MemorySegments {
    segments: BTreeMap<u16, Vec<u8>>,
}

impl MemorySegments {
    pub fn new() -> Self {
        Self::default()
    }

    /// Store one segment's bytes.
    pub fn insert(&mut self, segment: u16, bytes: Vec<u8>) {
        self.segments.insert(segment, bytes);
    }

    pub fn len(&self) -> usize {
        self.segments.len()
    }

    pub fn is_empty(&self) -> bool {
        self.segments.is_empty()
    }
}

impl SegmentReader for MemorySegments {
    fn read_range(&self, segment: u16, offset: u64, len: u64) -> Result<Vec<u8>, ArtifactError> {
        let bytes = self.segments.get(&segment).ok_or(ArtifactError::Invalid)?;
        let start = usize::try_from(offset).map_err(|_| ArtifactError::Invalid)?;
        let end = start
            .checked_add(usize::try_from(len).map_err(|_| ArtifactError::Invalid)?)
            .ok_or(ArtifactError::Invalid)?;
        bytes
            .get(start..end)
            .map(<[u8]>::to_vec)
            .ok_or(ArtifactError::Invalid)
    }

    fn segment_count(&self) -> u16 {
        self.segments.len().min(usize::from(u16::MAX)) as u16
    }
}

/// The segmented content store of a composed artifact, verified on read.
pub struct SegmentSource<'a, S: SegmentReader> {
    table: &'a BlobTable,
    segments: &'a S,
}

impl<'a, S: SegmentReader> SegmentSource<'a, S> {
    pub fn new(table: &'a BlobTable, segments: &'a S) -> Self {
        Self { table, segments }
    }

    /// Read one blob, decompressing and verifying it against its table entry.
    pub fn blob(&self, entry: &BlobEntry) -> Result<Vec<u8>, ArtifactError> {
        let compressed =
            self.segments
                .read_range(entry.segment, entry.offset, entry.compressed_size)?;
        let mut decoder = zstd::stream::read::Decoder::new(Cursor::new(compressed))?;
        let mut decoded = Vec::new();
        decoded
            .try_reserve_exact(usize::try_from(entry.size).map_err(|_| ArtifactError::Invalid)?)
            .map_err(|_| ArtifactError::Allocation { size: entry.size })?;
        decoder
            .by_ref()
            .take(entry.size + 1)
            .read_to_end(&mut decoded)?;
        if decoded.len() as u64 != entry.size
            || Sha256Digest::from_bytes(Sha256::digest(&decoded).into()) != entry.digest
        {
            return Err(ArtifactError::DigestMismatch {
                media_type: MediaType::BLOB.label(),
                digest: entry.digest.to_hex(),
            });
        }
        Ok(decoded)
    }

    /// Verify every blob the table declares.
    pub fn verify_all(&self) -> Result<(), ArtifactError> {
        for entry in self.table.entries() {
            self.blob(entry)?;
        }
        Ok(())
    }
}

/// The small documents a container carries beside the store.
#[derive(Debug, Clone, Default)]
pub struct MetadataSet {
    documents: BTreeMap<(&'static str, Sha256Digest), Vec<u8>>,
}

impl MetadataSet {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a document, verifying it against the descriptor that names it.
    pub fn insert(&mut self, descriptor: &Descriptor, bytes: Vec<u8>) -> Result<(), ArtifactError> {
        descriptor.verify(&bytes)?;
        self.documents
            .insert((descriptor.media_type.as_str(), descriptor.digest), bytes);
        Ok(())
    }

    pub fn get(&self, descriptor: &Descriptor) -> Option<&[u8]> {
        self.documents
            .get(&(descriptor.media_type.as_str(), descriptor.digest))
            .map(Vec::as_slice)
    }

    pub fn len(&self) -> usize {
        self.documents.len()
    }

    pub fn is_empty(&self) -> bool {
        self.documents.is_empty()
    }
}

/// A parsed artifact: an index, a blob table, and the content behind them.
///
/// This is the one parser. The dispatcher, the runtime, and `zup artifact
/// inspect` all read an artifact through it, so a format rule can never be
/// implemented twice and drift.
pub struct ArtifactView<S: SegmentReader> {
    index: ArtifactIndex,
    index_bytes: Vec<u8>,
    table: BlobTable,
    table_bytes: Vec<u8>,
    metadata: MetadataSet,
    segments: S,
}

impl<S: SegmentReader> ArtifactView<S> {
    /// Parse an artifact from the bytes its container carries.
    pub fn open(
        index_bytes: &[u8],
        table_bytes: &[u8],
        metadata: MetadataSet,
        segments: S,
    ) -> Result<Self, ArtifactError> {
        let index = ArtifactIndex::parse(index_bytes)?;
        let table = BlobTable::parse(table_bytes)?;
        index.tables.blobs.verify(table_bytes)?;
        if table.segments != 0 && table.segments > segments.segment_count() {
            return Err(ArtifactError::Incomplete {
                id: index.artifact.id.clone(),
                detail: format!(
                    "the table declares {} segments but the artifact carries {}",
                    table.segments,
                    segments.segment_count()
                ),
            });
        }
        Ok(Self {
            index,
            index_bytes: index_bytes.to_vec(),
            table,
            table_bytes: table_bytes.to_vec(),
            metadata,
            segments,
        })
    }

    pub fn index(&self) -> &ArtifactIndex {
        &self.index
    }

    pub fn table(&self) -> &BlobTable {
        &self.table
    }

    pub fn segments(&self) -> &S {
        &self.segments
    }

    /// The content store, verified on every read.
    pub fn store(&self) -> SegmentSource<'_, S> {
        SegmentSource::new(&self.table, &self.segments)
    }

    /// Read one variant manifest and verify it against the index descriptor.
    pub fn variant_manifest(&self, id: &str) -> Result<VariantManifest, ArtifactError> {
        let variant = self
            .index
            .variant(id)
            .ok_or_else(|| ArtifactError::UnknownVariant { id: id.to_owned() })?;
        let bytes = self.read(&variant.manifest)?;
        VariantManifest::parse(&bytes)
    }

    /// Read one variant's native runtime image, if it carries one.
    pub fn variant_runtime(&self, id: &str) -> Result<Option<Vec<u8>>, ArtifactError> {
        let variant = self
            .index
            .variant(id)
            .ok_or_else(|| ArtifactError::UnknownVariant { id: id.to_owned() })?;
        let Some(runtime) = variant.runtime else {
            return Ok(None);
        };
        self.read(&runtime).map(Some)
    }

    /// Verify that every blob a variant needs is present before anything is
    /// materialized from it, so a selected runtime never sees another
    /// architecture's content and never starts with a half-present store.
    pub fn verify_variant(&self, id: &str) -> Result<VariantManifest, ArtifactError> {
        let variant = self
            .index
            .variant(id)
            .ok_or_else(|| ArtifactError::UnknownVariant { id: id.to_owned() })?;
        let manifest = self.variant_manifest(id)?;
        let needed = manifest.content_digests();
        let missing = self.table.missing(&needed);
        if !missing.is_empty() {
            return Err(ArtifactError::Incomplete {
                id: self.index.artifact.id.clone(),
                detail: format!(
                    "{} blobs the selected variant needs are absent, starting with {}",
                    missing.len(),
                    missing[0]
                ),
            });
        }
        if self.index.artifact.mode.carries_content() && variant.runtime.is_none() {
            return Err(ArtifactError::Incomplete {
                id: self.index.artifact.id.clone(),
                detail: format!("variant `{id}` carries no native runtime to execute"),
            });
        }
        Ok(manifest)
    }
}

impl<S: SegmentReader> std::fmt::Debug for ArtifactView<S> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ArtifactView")
            .field("index", &self.index)
            .field("segments", &self.segments.segment_count())
            .finish()
    }
}

impl<S: SegmentReader> ContentSource for ArtifactView<S> {
    fn read(&self, descriptor: &Descriptor) -> Result<Vec<u8>, ArtifactError> {
        let bytes = match descriptor.media_type {
            MediaType::INDEX => self.index_bytes.clone(),
            MediaType::BLOB_TABLE => self.table_bytes.clone(),
            MediaType::VARIANT_MANIFEST | MediaType::RUNTIME => self
                .metadata
                .get(descriptor)
                .ok_or_else(|| ArtifactError::Missing {
                    media_type: descriptor.label(),
                    digest: descriptor.digest.to_hex(),
                })?
                .to_vec(),
            MediaType::BLOB => {
                let entry =
                    self.table
                        .entry(&descriptor.digest)
                        .ok_or_else(|| ArtifactError::Missing {
                            media_type: descriptor.label(),
                            digest: descriptor.digest.to_hex(),
                        })?;
                self.store().blob(entry)?
            }
        };
        descriptor.verify(&bytes)?;
        Ok(bytes)
    }

    fn contains(&self, descriptor: &Descriptor) -> bool {
        match descriptor.media_type {
            MediaType::INDEX | MediaType::BLOB_TABLE => true,
            MediaType::VARIANT_MANIFEST | MediaType::RUNTIME => {
                self.metadata.get(descriptor).is_some()
            }
            MediaType::BLOB => self.table.entry(&descriptor.digest).is_some(),
        }
    }
}

/// The size bound a descriptor of each media type is checked against.
pub fn limit_for(media_type: MediaType) -> u64 {
    media_type.limit()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn descriptor(bytes: &[u8]) -> Descriptor {
        Descriptor::of(MediaType::BLOB, bytes)
    }

    /// A source that publishes bytes it has not matched against the digest they
    /// are addressed by is a source that can serve anything at any address.
    #[test]
    fn a_memory_source_refuses_content_that_does_not_match_its_descriptor() {
        let descriptor = descriptor(b"content");
        let mut source = MemorySource::new();
        assert!(source.insert(&descriptor, b"other".to_vec()).is_err());
        source.insert(&descriptor, b"content".to_vec()).unwrap();
        assert_eq!(source.read(&descriptor).unwrap(), b"content");
        assert!(source.contains(&descriptor));
    }

    /// The spooled form is the one that reaches the filesystem, so a short write
    /// must leave no file at all: a truncated blob at a valid path is a blob a
    /// later reader would trust.
    #[test]
    fn a_spool_source_publishes_only_verified_content() {
        let root = tempfile::tempdir().unwrap();
        let mut source = SpoolSource::new(root.path());
        let digest = Sha256Digest::from_bytes(Sha256::digest(b"payload").into());
        assert!(
            source
                .write_from(&digest, Cursor::new(b"pay".as_slice()))
                .is_err()
        );
        assert!(!source.path(&digest).exists());
        let size = source
            .write_from(&digest, Cursor::new(b"payload".as_slice()))
            .unwrap();
        assert_eq!(size, 7);
        assert!(source.path(&digest).exists());
        source.verify_all().unwrap();
        let read = source.read(&descriptor(b"payload")).unwrap();
        assert_eq!(read, b"payload");
    }

    /// A digest nothing was ever written for is reported as missing, not as an
    /// empty read. A zero-length blob is a valid blob, so "no bytes" and "these
    /// bytes" must stay distinguishable all the way to the caller.
    #[test]
    fn a_spool_source_reports_a_missing_digest_rather_than_a_short_read() {
        let root = tempfile::tempdir().unwrap();
        let source = SpoolSource::new(root.path());
        let error = source.read(&descriptor(b"absent")).unwrap_err();
        assert!(matches!(error, ArtifactError::Missing { .. }));
        assert!(!source.contains(&descriptor(b"absent")));
    }

    /// The spool is a directory other processes can reach, so the file behind a
    /// digest can be replaced between the write and the read. A reader that trusted
    /// the path rather than the bytes would serve whatever is there now.
    #[test]
    fn a_spooled_file_replaced_behind_the_source_is_detected() {
        let root = tempfile::tempdir().unwrap();
        let mut source = SpoolSource::new(root.path());
        let digest = Sha256Digest::from_bytes(Sha256::digest(b"payload").into());
        source.write(&digest, b"payload").unwrap();
        std::fs::write(source.path(&digest), b"tampered").unwrap();
        assert!(matches!(
            source.read(&descriptor(b"payload")),
            Err(ArtifactError::SizeMismatch { .. })
        ));
    }

    /// A blob entry addresses a byte range inside a segment. Reading one past the end
    /// would splice the next blob's bytes into this one, and every digest check
    /// downstream would then be checking a blob that never existed.
    #[test]
    fn segments_read_exact_ranges_and_refuse_to_run_past_their_end() {
        let mut segments = MemorySegments::new();
        segments.insert(0, b"abcdef".to_vec());
        assert_eq!(segments.read_range(0, 2, 3).unwrap(), b"cde".to_vec());
        assert!(segments.read_range(0, 4, 3).is_err());
        assert!(segments.read_range(1, 0, 1).is_err());
    }
}
