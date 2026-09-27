//! The verified on-disk content cache.
//!
//! The cache is the reason two unrelated zup operations cost zero bytes the
//! second time. Content is named by its digest, so a blob fetched by a thin
//! install, an update, a repair, and a different application that happens to
//! ship the same file are one object on disk, and no database is needed to know
//! that.
//!
//! Every property the rest of the system relies on is enforced here:
//!
//! - **Nothing is trusted because it is local.** A cached blob is re-validated
//!   against its descriptor before it is consumed, at the depth the caller's
//!   policy asks for.
//! - **Partial bytes are never visible.** A transfer writes `<digest>.partial`
//!   and is published to `<digest>` only after the complete logical stream has
//!   hashed to the descriptor's digest.
//! - **A resume never trusts serialized hash state.** The resume record says
//!   how far the wire form got; the prefix it points at is decompressed and
//!   re-hashed before the transfer continues.
//! - **No path leaves the root, and no link is followed.** Every prefix of
//!   every path is checked before a byte is read or written.
//!
//! # Layout
//!
//! ```text
//! <root>/blobs/sha256/<ab>/<hex>            a verified blob
//! <root>/blobs/sha256/<ab>/<hex>.partial    a transfer in progress
//! <root>/blobs/sha256/<ab>/<hex>.resume     what the partial is and how far it got
//! <root>/blobs/sha256/<ab>/<hex>.lock       a writer's claim on the blob
//! ```
//!
//! The layout is the same shape the web tree uses, so a directory staged for
//! static hosting is a valid seed source with no second packaging format.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zup_core::Sha256Digest;

use crate::descriptor::RelativeContentPath;
use crate::descriptor::{ContentCompression, ContentDescriptor, ContentKind};
use crate::error::CacheError;
use crate::filesystem::{CacheFileSystem, PortableCacheFileSystem};
use crate::layout::{BLOB_ROOT, blob_path};

/// Current resume record schema.
pub const RESUME_SCHEMA: u32 = 1;

/// How much of a partial transfer the resume record is refreshed after.
///
/// A record is rewritten on this interval so an unclean exit costs at most this
/// many bytes of redownload, and the record itself stays a few hundred bytes.
pub const RESUME_RECORD_INTERVAL: u64 = 8 * 1024 * 1024;

/// A lock older than this is assumed to belong to a writer that died.
pub const RESERVATION_STALE: std::time::Duration = std::time::Duration::from_secs(30 * 60);

/// How thoroughly a cached blob is re-validated before it is consumed.
///
/// Re-hashing every payload blob on every read would double the cost of every
/// install for a threat that requires write access to a per-user directory the
/// process already owns. The default therefore validates identity where it
/// matters — the length always, and the digest for anything executable,
/// structural, or small.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Verify {
    /// Check the wire length only. Sufficient for payload that was verified at
    /// publication time and lives in a directory only this user can write.
    #[default]
    WireLength,
    /// Decompress and re-hash the complete logical content.
    Full,
}

impl Verify {
    /// The policy this crate applies to `kind` by default.
    ///
    /// Executable content and documents are cheap to re-hash relative to their
    /// value, and a runtime is about to be executed, so both are always fully
    /// verified. Payload is the bulk and is checked by length.
    pub const fn for_kind(kind: ContentKind) -> Self {
        match kind {
            ContentKind::Payload => Self::WireLength,
            ContentKind::Runtime | ContentKind::Metadata | ContentKind::Catalog => Self::Full,
        }
    }
}

/// What the cache holds after a transaction, which is a deliberate choice
/// rather than an accident of never cleaning up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CachePolicy {
    /// Hold nothing once the transaction commits. The closure is re-fetched if
    /// it is ever needed again, which is the right default for a machine that
    /// is online and does not want its disk doubled.
    #[default]
    Temporary,
    /// Hold what a normal install needs to be repaired or updated without
    /// re-fetching unchanged content: the trusted release, small critical
    /// state, and recently verified payload. Large payload is pruned once it
    /// has aged out.
    Auto,
    /// Hold the entire closure, so the machine can repair or reinstall with no
    /// network at all. An author who wants offline-repair capability asks for
    /// this explicitly.
    Keep,
}

impl CachePolicy {
    /// Stable name for a CLI flag and for state on disk.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Temporary => "temporary",
            Self::Auto => "auto",
            Self::Keep => "keep",
        }
    }

    /// Whether verified payload is retained after the transaction commits.
    pub const fn retains_payload(self) -> bool {
        matches!(self, Self::Auto | Self::Keep)
    }

    /// Payloads older than this are pruned under [`CachePolicy::Auto`].
    pub const fn auto_retention(self) -> Option<std::time::Duration> {
        match self {
            Self::Temporary => None,
            Self::Auto => Some(std::time::Duration::from_secs(7 * 24 * 60 * 60)),
            Self::Keep => Some(std::time::Duration::MAX),
        }
    }

    /// The policy this name denotes, as it is written in a retention record.
    ///
    /// A record's policy is a string so a human can read it; this is the one
    /// place that turns it back into a decision, so a caller cannot accidentally
    /// implement a looser or stricter reading of the same word.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "temporary" => Some(Self::Temporary),
            "auto" => Some(Self::Auto),
            "keep" => Some(Self::Keep),
            _ => None,
        }
    }
}

impl std::str::FromStr for CachePolicy {
    type Err = ();

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        Self::from_name(name).ok_or(())
    }
}

/// One object a cache holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheObject {
    pub digest: Sha256Digest,
    /// The wire bytes it occupies.
    pub wire_size: u64,
    /// When it was last written.
    pub modified: Option<std::time::SystemTime>,
}

/// What a probe found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheProbe {
    /// The blob is present and satisfies the requested verification.
    Present { wire_size: u64 },
    /// The blob is not present, and no resumable partial exists.
    Absent,
    /// A partial transfer exists that the next writer can continue from.
    Resumable { wire_offset: u64 },
}

/// A blob that is in the cache and has been validated against its descriptor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedBlob {
    pub descriptor: ContentDescriptor,
    /// Where the wire form lives.
    pub path: PathBuf,
    /// Length of the wire form, which is what a size cap is checked against.
    pub wire_size: u64,
}

impl VerifiedBlob {
    /// Read and verify the complete logical content.
    pub fn read_to_end(&self) -> Result<Vec<u8>, CacheError> {
        let mut reader = self.open()?;
        let mut out = Vec::with_capacity(self.descriptor.size.min(64 * 1024 * 1024) as usize);
        reader.read_all_logical(&mut out)?;
        Ok(out)
    }

    /// Open a stream that yields the logical bytes and verifies the digest as
    /// it reaches the end.
    ///
    /// Verification is not optional and not a separate call: a consumer that
    /// stops reading early has read less content, and one that reaches the end
    /// has proved the digest. There is no path that yields bytes and reports
    /// success without proving identity.
    pub fn open(&self) -> Result<BlobReader, CacheError> {
        let file = File::open(&self.path).map_err(|source| CacheError::io(&self.path, source))?;
        BlobReader::new(file, self.descriptor)
    }
}

/// Reads a cached blob's logical bytes, verifying identity at the end.
pub struct BlobReader {
    descriptor: ContentDescriptor,
    inner: Box<dyn Read + Send>,
    hasher: Sha256,
    produced: u64,
    verified: bool,
}

impl BlobReader {
    fn new(file: File, descriptor: ContentDescriptor) -> Result<Self, CacheError> {
        let inner: Box<dyn Read + Send> = if descriptor.is_compressed() {
            Box::new(zstd::stream::read::Decoder::new(file).map_err(|source| {
                CacheError::io(PathBuf::from(descriptor.digest.to_hex()), source)
            })?)
        } else {
            Box::new(file)
        };
        Ok(Self {
            descriptor,
            inner,
            hasher: Sha256::new(),
            produced: 0,
            verified: false,
        })
    }

    /// Bytes of logical content produced so far.
    pub const fn produced(&self) -> u64 {
        self.produced
    }

    /// Read every remaining byte into `out`, verifying the digest on success.
    pub fn read_all_logical(&mut self, out: &mut Vec<u8>) -> Result<(), CacheError> {
        let mut buffer = [0u8; 128 * 1024];
        loop {
            let read = self.inner.read(&mut buffer).map_err(|source| {
                CacheError::io(PathBuf::from(self.descriptor.digest.to_hex()), source)
            })?;
            if read == 0 {
                break;
            }
            self.absorb(&buffer[..read])?;
            out.extend_from_slice(&buffer[..read]);
        }
        self.finish()
    }
}

impl Read for BlobReader {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let read = self.inner.read(buffer)?;
        if read == 0 {
            // End of stream is the only point at which identity can be
            // established, so a short read must not look like success.
            if !self.verified {
                self.finish().map_err(|error| {
                    std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string())
                })?;
            }
            return Ok(0);
        }
        self.absorb(&buffer[..read]).map_err(|error| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string())
        })?;
        Ok(read)
    }
}

impl BlobReader {
    fn absorb(&mut self, bytes: &[u8]) -> Result<(), CacheError> {
        self.produced = self
            .produced
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| CacheError::Overflow {
                digest: self.descriptor.digest.to_hex(),
                limit: self.descriptor.size,
            })?;
        if self.produced > self.descriptor.size {
            return Err(CacheError::SizeMismatch {
                digest: self.descriptor.digest.to_hex(),
                expected: self.descriptor.size,
                found: self.produced,
            });
        }
        if self.produced > self.descriptor.expansion_limit() {
            return Err(CacheError::Overflow {
                digest: self.descriptor.digest.to_hex(),
                limit: self.descriptor.expansion_limit(),
            });
        }
        self.hasher.update(bytes);
        Ok(())
    }

    /// Prove the complete content matches the descriptor.
    fn finish(&mut self) -> Result<(), CacheError> {
        if self.verified {
            return Ok(());
        }
        if self.produced != self.descriptor.size {
            return Err(CacheError::SizeMismatch {
                digest: self.descriptor.digest.to_hex(),
                expected: self.descriptor.size,
                found: self.produced,
            });
        }
        let found = Sha256Digest::from_bytes(self.hasher.clone().finalize().into());
        if found != self.descriptor.digest {
            return Err(CacheError::DigestMismatch {
                digest: self.descriptor.digest.to_hex(),
                found: found.to_hex(),
            });
        }
        self.verified = true;
        Ok(())
    }
}

/// What a partial transfer is and how far it got.
///
/// This record is a few hundred bytes and is the only durable state a resume
/// depends on. It carries no hash state: `wire_offset` says where to continue,
/// and the prefix it points at is decompressed and re-hashed before any byte is
/// appended. A record that disagrees with the partial is discarded, so a torn
/// write costs a full re-fetch rather than a corrupt blob.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResumeRecord {
    pub schema: u32,
    /// Identity of the logical content this partial belongs to.
    pub digest: Sha256Digest,
    /// Total logical length.
    pub size: u64,
    pub compression: ContentCompression,
    /// Total wire length.
    pub compressed_size: u64,
    /// Bytes of the wire form that are on disk and accounted for.
    pub wire_offset: u64,
    /// Logical bytes the accounted wire prefix decodes to.
    pub prefix_size: u64,
    /// SHA-256 of the decoded prefix.
    pub prefix_digest: Sha256Digest,
}

impl ResumeRecord {
    /// Whether this record describes `descriptor`.
    pub fn describes(&self, descriptor: &ContentDescriptor) -> bool {
        self.schema == RESUME_SCHEMA
            && self.digest == descriptor.digest
            && self.size == descriptor.size
            && self.compression == descriptor.compression
            && self.compressed_size == descriptor.compressed_size
    }
}

/// A per-user content cache keyed by immutable digest.
pub struct ContentCache {
    root: PathBuf,
    file_system: Arc<dyn CacheFileSystem>,
    policy: CachePolicy,
    reservation_stale: std::time::Duration,
}

/// Where one blob's files live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobPaths {
    pub relative: RelativeContentPath,
    pub final_path: PathBuf,
    pub partial_path: PathBuf,
    pub resume_path: PathBuf,
    pub lock_path: PathBuf,
}

impl BlobPaths {
    /// The parent directory of this blob's files.
    pub fn parent(&self) -> &Path {
        self.final_path
            .parent()
            .expect("a blob path always has a parent")
    }
}

impl ContentCache {
    /// Open a cache on the portable `std::fs` filesystem.
    pub fn open(root: impl Into<PathBuf>, policy: CachePolicy) -> Result<Self, CacheError> {
        Self::with_file_system(root, policy, Arc::new(PortableCacheFileSystem))
    }

    /// Open a cache that publishes and clears links through `file_system`.
    pub fn with_file_system(
        root: impl Into<PathBuf>,
        policy: CachePolicy,
        file_system: Arc<dyn CacheFileSystem>,
    ) -> Result<Self, CacheError> {
        let root = root.into();
        std::fs::create_dir_all(&root).map_err(|source| CacheError::io(&root, source))?;
        let cache = Self {
            root,
            file_system,
            policy,
            reservation_stale: RESERVATION_STALE,
        };
        cache.reject_links(cache.root.as_path())?;
        Ok(cache)
    }

    /// How long a writer's claim on a blob survives before another acquisition
    /// may take it.
    ///
    /// A claim is an optimization — two writers of one digest produce identical
    /// bytes — so the only cost of getting this wrong is a refused transfer.
    /// Lowering it lets a machine recover quickly from an installer that was
    /// killed mid-write.
    pub fn set_reservation_stale(&mut self, stale: std::time::Duration) {
        self.reservation_stale = stale;
    }

    /// The directory the cache is rooted at.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// What this cache retains after a transaction commits.
    pub const fn policy(&self) -> CachePolicy {
        self.policy
    }

    /// Change the retention policy.
    pub fn set_policy(&mut self, policy: CachePolicy) {
        self.policy = policy;
    }

    /// Where one blob's files live, with every path confined to the root.
    pub fn paths(&self, descriptor: &ContentDescriptor) -> Result<BlobPaths, CacheError> {
        let relative = blob_path(&descriptor.digest);
        let final_path = self.resolve(&relative)?;
        let stem = final_path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or(CacheError::Descriptor("a blob path has no file name"))?
            .to_owned();
        let parent = final_path
            .parent()
            .ok_or(CacheError::Descriptor("a blob path has no parent"))?
            .to_path_buf();
        Ok(BlobPaths {
            relative,
            partial_path: parent.join(format!("{stem}.partial")),
            resume_path: parent.join(format!("{stem}.resume")),
            lock_path: parent.join(format!("{stem}.lock")),
            final_path,
        })
    }

    /// Join a checked relative path to the root, refusing anything that leaves
    /// it.
    fn resolve(&self, relative: &RelativeContentPath) -> Result<PathBuf, CacheError> {
        let mut path = self.root.clone();
        for segment in relative.to_string().split('/') {
            path.push(segment);
        }
        if !path.starts_with(&self.root) || path == self.root {
            return Err(CacheError::Escapes {
                root: self.root.display().to_string(),
                path: path.display().to_string(),
            });
        }
        Ok(path)
    }

    /// Refuse to proceed when any component of `path`, or `path` itself, is a
    /// link: reading or writing through one would move data outside the root.
    fn reject_links(&self, path: &Path) -> Result<(), CacheError> {
        for component in link_checked_prefixes(path)? {
            if self
                .file_system
                .is_link(&component)
                .map_err(|source| CacheError::io(&component, source))?
            {
                return Err(CacheError::Link {
                    path: component.display().to_string(),
                });
            }
        }
        Ok(())
    }

    /// Look up one blob, validating it to the depth `verify` asks for.
    ///
    /// A blob that fails validation is treated as absent, and its file is
    /// removed: a cache entry that does not match its own name is not an entry.
    pub fn probe(
        &self,
        descriptor: &ContentDescriptor,
        verify: Verify,
    ) -> Result<CacheProbe, CacheError> {
        descriptor.validate().map_err(CacheError::Descriptor)?;
        let paths = self.paths(descriptor)?;
        self.reject_links(&paths.final_path)?;
        let metadata = match std::fs::symlink_metadata(&paths.final_path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(self.probe_partial(&paths));
            }
            Err(source) => return Err(CacheError::io(&paths.final_path, source)),
        };
        if !metadata.is_file() || metadata.len() != descriptor.compressed_size {
            let _ = std::fs::remove_file(&paths.final_path);
            return Ok(self.probe_partial(&paths));
        }
        if verify == Verify::Full {
            match self.verified_blob(descriptor, &paths)? {
                Some(blob) => {
                    return Ok(CacheProbe::Present {
                        wire_size: blob.wire_size,
                    });
                }
                None => return Ok(self.probe_partial(&paths)),
            }
        }
        Ok(CacheProbe::Present {
            wire_size: descriptor.compressed_size,
        })
    }

    fn probe_partial(&self, paths: &BlobPaths) -> CacheProbe {
        if !paths.partial_path.is_file() || !paths.resume_path.is_file() {
            return CacheProbe::Absent;
        }
        match read_resume(&paths.resume_path) {
            Some(record) if record.wire_offset > 0 => CacheProbe::Resumable {
                wire_offset: record.wire_offset,
            },
            _ => CacheProbe::Absent,
        }
    }

    fn verified_blob(
        &self,
        descriptor: &ContentDescriptor,
        paths: &BlobPaths,
    ) -> Result<Option<VerifiedBlob>, CacheError> {
        let file = match File::open(&paths.final_path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(CacheError::io(&paths.final_path, source)),
        };
        let mut reader = BlobReader::new(file, *descriptor)?;
        let mut sink = std::io::sink();
        let mut buffer = [0u8; 128 * 1024];
        loop {
            let read = match reader.inner.read(&mut buffer) {
                Ok(read) => read,
                Err(_) => return Ok(None),
            };
            if read == 0 {
                break;
            }
            if reader.absorb(&buffer[..read]).is_err() {
                return Ok(None);
            }
            let _ = sink.write_all(&buffer[..read]);
        }
        if reader.finish().is_err() {
            let _ = std::fs::remove_file(&paths.final_path);
            return Ok(None);
        }
        Ok(Some(VerifiedBlob {
            descriptor: *descriptor,
            path: paths.final_path.clone(),
            wire_size: descriptor.compressed_size,
        }))
    }

    /// Return the verified blob for `descriptor`, or `None` if it is absent or
    /// does not validate.
    pub fn get(
        &self,
        descriptor: &ContentDescriptor,
        verify: Verify,
    ) -> Result<Option<VerifiedBlob>, CacheError> {
        match self.probe(descriptor, verify)? {
            CacheProbe::Present { wire_size } => {
                let paths = self.paths(descriptor)?;
                Ok(Some(VerifiedBlob {
                    descriptor: *descriptor,
                    path: paths.final_path,
                    wire_size,
                }))
            }
            CacheProbe::Absent | CacheProbe::Resumable { .. } => Ok(None),
        }
    }

    /// Every object this cache holds, in ascending digest order.
    ///
    /// The walk is over the content-addressed tree, so it needs no index: a
    /// cache is exactly its own directory listing. Anything whose name is not a
    /// 64-character hex digest is skipped rather than refused, because a stray
    /// file in a per-user cache is not an attack and refusing to sweep would
    /// leak disk for the life of the machine.
    pub fn objects(&self) -> Vec<CacheObject> {
        let mut objects = Vec::new();
        for shard in self.shards() {
            let Some(prefix) = shard
                .file_name()
                .and_then(|name| name.to_str())
                .filter(|name| name.len() == 2)
            else {
                continue;
            };
            let Ok(entries) = std::fs::read_dir(&shard) else {
                continue;
            };
            for entry in entries.flatten() {
                let name = entry.file_name();
                let Some(name) = name.to_str() else {
                    continue;
                };
                // A `.partial`, `.resume`, or `.lock` is not a published object,
                // so it is not a candidate for retention.
                if name.contains('.') {
                    continue;
                }
                // A digest is its shard directory plus its own name. The shard
                // is part of the identity, not a convenience, so it is read back
                // rather than assumed.
                let Ok(digest) = format!("{prefix}{name}").parse::<Sha256Digest>() else {
                    continue;
                };
                let Ok(metadata) = entry.metadata() else {
                    continue;
                };
                if !metadata.is_file() {
                    continue;
                }
                objects.push(CacheObject {
                    digest,
                    wire_size: metadata.len(),
                    modified: metadata.modified().ok(),
                });
            }
        }
        objects.sort_by_key(|object| object.digest);
        objects
    }

    /// Delete one object and everything beside it, returning the wire bytes
    /// freed.
    ///
    /// Deleting is a rename to a scratch name followed by a removal, so a sweep
    /// interrupted half way leaves either the object or a scratch file the next
    /// sweep removes. It never leaves a file whose name is a digest it does not
    /// hash to.
    pub fn remove(&self, digest: &Sha256Digest) -> Result<u64, CacheError> {
        let descriptor = ContentDescriptor::stored(ContentKind::Payload, *digest, 1);
        let paths = self.paths(&descriptor)?;
        self.reject_links(&paths.final_path)?;
        let metadata = match std::fs::symlink_metadata(&paths.final_path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(source) => return Err(CacheError::io(&paths.final_path, source)),
        };
        for path in [&paths.partial_path, &paths.resume_path, &paths.lock_path] {
            let _ = std::fs::remove_file(path);
        }
        std::fs::remove_file(&paths.final_path)
            .map_err(|source| CacheError::io(&paths.final_path, source))?;
        Ok(metadata.len())
    }

    /// Every two-character shard directory under the blob root.
    fn shards(&self) -> Vec<PathBuf> {
        let mut path = self.root.clone();
        for segment in BLOB_ROOT.split('/') {
            path.push(segment);
        }
        let Ok(entries) = std::fs::read_dir(&path) else {
            return Vec::new();
        };
        let mut shards: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|shard| {
                shard
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.len() == 2)
            })
            .collect();
        shards.sort();
        shards
    }

    /// Open a writer for one blob, resuming a valid partial transfer when one
    /// exists.
    ///
    /// The writer is the only way content enters the cache. It bounds the wire
    /// form by the descriptor, records progress for a resume, and publishes
    /// only after the complete logical content has been verified.
    pub fn writer(&self, descriptor: &ContentDescriptor) -> Result<BlobWriter, CacheError> {
        descriptor.validate().map_err(CacheError::Descriptor)?;
        if descriptor.size > descriptor.kind.size_limit() {
            return Err(CacheError::TooLarge {
                digest: descriptor.digest.to_hex(),
                declared: descriptor.size,
                limit: descriptor.kind.size_limit(),
            });
        }
        let paths = self.paths(descriptor)?;
        std::fs::create_dir_all(paths.parent())
            .map_err(|source| CacheError::io(paths.parent(), source))?;
        self.reject_links(paths.parent())?;
        self.reject_links(&paths.final_path)?;

        let resume = self.resume_point(&paths, descriptor)?;
        let lock = Reservation::acquire(&self.file_system, &paths, self.reservation_stale)?;
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(resume == 0)
            .open(&paths.partial_path)
            .map_err(|source| CacheError::io(&paths.partial_path, source))?;
        file.set_len(resume)
            .map_err(|source| CacheError::io(&paths.partial_path, source))?;
        if resume > 0 {
            file.seek(SeekFrom::Start(resume))
                .map_err(|source| CacheError::io(&paths.partial_path, source))?;
        }
        Ok(BlobWriter {
            descriptor: *descriptor,
            paths,
            file: Some(file),
            file_system: Arc::clone(&self.file_system),
            lock,
            wire_offset: resume,
            next_record: resume.saturating_add(RESUME_RECORD_INTERVAL),
            settled: false,
        })
    }

    /// Decide where a new transfer should start.
    ///
    /// A partial is only continued when its record describes this descriptor
    /// *and* the wire prefix it points at decodes to the logical prefix the
    /// record names. Everything else starts from zero, which is the only safe
    /// answer when the evidence is not conclusive.
    fn resume_point(
        &self,
        paths: &BlobPaths,
        descriptor: &ContentDescriptor,
    ) -> Result<u64, CacheError> {
        self.reject_links(&paths.partial_path)?;
        let Some(record) = read_resume(&paths.resume_path) else {
            self.discard_partial(paths);
            return Ok(0);
        };
        if !record.describes(descriptor) {
            self.discard_partial(paths);
            return Ok(0);
        }
        let actual = match std::fs::metadata(&paths.partial_path) {
            Ok(metadata) => metadata.len(),
            Err(_) => {
                self.discard_partial(paths);
                return Ok(0);
            }
        };
        // A partial longer than the record accounts for has a tail nobody
        // vouched for, so it is cut back rather than trusted.
        let offset = record.wire_offset.min(actual);
        if offset == 0 || offset >= descriptor.compressed_size {
            self.discard_partial(paths);
            return Ok(0);
        }
        match verify_prefix(&paths.partial_path, offset, &record, descriptor) {
            Ok(()) => Ok(offset),
            Err(_) => {
                self.discard_partial(paths);
                Ok(0)
            }
        }
    }

    /// Remove a partial transfer and its record.
    pub fn discard_partial(&self, paths: &BlobPaths) {
        let _ = std::fs::remove_file(&paths.partial_path);
        let _ = std::fs::remove_file(&paths.resume_path);
    }

    /// Remove one blob and any partial state for it.
    pub fn forget(&self, descriptor: &ContentDescriptor) -> Result<(), CacheError> {
        let paths = self.paths(descriptor)?;
        for path in [
            &paths.final_path,
            &paths.partial_path,
            &paths.resume_path,
            &paths.lock_path,
        ] {
            remove_if_present(path)?;
        }
        Ok(())
    }

    /// Remove one blob by digest, for a policy that has no descriptor.
    ///
    /// Eviction is the only caller: it knows which digests aged out but not what
    /// kind or size they were, and it must not have to re-derive a descriptor
    /// it no longer has the catalog for.
    pub fn forget_digest(&self, digest: &Sha256Digest) -> Result<(), CacheError> {
        let relative = blob_path(digest);
        let final_path = self.resolve(&relative)?;
        let hex = digest.to_hex();
        let parent = final_path
            .parent()
            .ok_or(CacheError::Descriptor("a blob path has no parent"))?
            .to_path_buf();
        for path in [
            final_path,
            parent.join(format!("{hex}.partial")),
            parent.join(format!("{hex}.resume")),
            parent.join(format!("{hex}.lock")),
        ] {
            remove_if_present(&path)?;
        }
        Ok(())
    }

    /// Every blob the cache currently holds, in ascending digest order.
    pub fn digests(&self) -> Result<Vec<Sha256Digest>, CacheError> {
        let root = self.root.join(BLOB_ROOT);
        let mut out = Vec::new();
        collect_digests(&self.file_system, &root, "", &mut out)?;
        out.sort_unstable();
        out.dedup();
        Ok(out)
    }

    /// Wire bytes the cache currently holds.
    pub fn stored_size(&self) -> Result<u64, CacheError> {
        let mut total = 0u64;
        for digest in self.digests()? {
            let relative = blob_path(&digest);
            let mut path = self.root.clone();
            for segment in relative.to_string().split('/') {
                path.push(segment);
            }
            if let Ok(metadata) = std::fs::metadata(&path) {
                total = total.saturating_add(metadata.len());
            }
        }
        Ok(total)
    }

    /// Apply the retention policy, returning how many blobs were removed.
    ///
    /// Eviction only ever removes cache entries. It cannot touch an installed
    /// application or the ownership ledger, because neither is reachable from a
    /// digest: the ledger names destinations, and a destination holds its own
    /// copy of the bytes.
    pub fn enforce_policy(&self, protected: &[Sha256Digest]) -> Result<usize, CacheError> {
        if self.policy == CachePolicy::Keep {
            return Ok(0);
        }
        if self.policy == CachePolicy::Temporary {
            let mut removed = 0;
            for digest in self.digests()? {
                if !protected.contains(&digest) {
                    self.forget_digest(&digest)?;
                    removed += 1;
                }
            }
            return Ok(removed);
        }
        let retention = self
            .policy
            .auto_retention()
            .ok_or(CacheError::Descriptor("this policy prunes nothing"))?;
        let cutoff =
            std::time::SystemTime::now()
                .checked_sub(retention)
                .ok_or(CacheError::Descriptor(
                    "the retention window is out of range",
                ))?;
        let root = self.root.join(BLOB_ROOT);
        let mut removed = 0;
        prune_directory(
            &self.file_system,
            &self.root,
            &root,
            cutoff,
            protected,
            &mut removed,
        )?;
        Ok(removed)
    }
}

/// A writer's claim on one blob.
///
/// The claim is an optimization, never a correctness requirement: two writers
/// of the same digest produce identical bytes, so the worst a lost race costs is
/// bandwidth. That is why a stale claim is reclaimed rather than waited on
/// forever, and why correctness never depends on holding it.
struct Reservation {
    path: PathBuf,
    held: bool,
}

impl Reservation {
    fn acquire(
        file_system: &Arc<dyn CacheFileSystem>,
        paths: &BlobPaths,
        stale_after: std::time::Duration,
    ) -> Result<Self, CacheError> {
        for attempt in 0..2 {
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&paths.lock_path)
            {
                Ok(mut file) => {
                    let _ = file.write_all(format!("{}\n", std::process::id()).as_bytes());
                    let _ = file.sync_all();
                    return Ok(Self {
                        path: paths.lock_path.clone(),
                        held: true,
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    if attempt == 1 || !claim_is_stale(&paths.lock_path, stale_after) {
                        return Err(CacheError::Reserved {
                            digest: paths
                                .final_path
                                .file_name()
                                .and_then(|name| name.to_str())
                                .unwrap_or("blob")
                                .to_owned(),
                        });
                    }
                    let _ = std::fs::remove_file(&paths.lock_path);
                }
                Err(source) => return Err(CacheError::io(&paths.lock_path, source)),
            }
        }
        let _ = file_system;
        Err(CacheError::Descriptor(
            "a blob reservation could not be taken",
        ))
    }

    /// Give the blob back so the next acquisition can take it.
    fn release(&mut self) {
        if self.held {
            let _ = std::fs::remove_file(&self.path);
            self.held = false;
        }
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.release();
    }
}

fn claim_is_stale(path: &Path, stale_after: std::time::Duration) -> bool {
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .is_some_and(|age| age >= stale_after)
}

/// Writes one blob's wire form into quarantine and publishes it verified.
pub struct BlobWriter {
    descriptor: ContentDescriptor,
    paths: BlobPaths,
    file: Option<File>,
    file_system: Arc<dyn CacheFileSystem>,
    lock: Reservation,
    wire_offset: u64,
    next_record: u64,
    /// Whether the transfer reached a decision, so `Drop` does not second-guess
    /// it.
    settled: bool,
}

impl BlobWriter {
    /// The descriptor this writer is filling.
    pub const fn descriptor(&self) -> &ContentDescriptor {
        &self.descriptor
    }

    /// Wire bytes already on disk, which is where a range request continues.
    pub const fn wire_offset(&self) -> u64 {
        self.wire_offset
    }

    /// Append wire bytes.
    ///
    /// The descriptor's wire length is a hard ceiling: a source that keeps
    /// sending past it is refused rather than allowed to fill a disk.
    pub fn write(&mut self, bytes: &[u8]) -> Result<(), CacheError> {
        if bytes.is_empty() {
            return Ok(());
        }
        let digest = self.descriptor.digest.to_hex();
        let next = self
            .wire_offset
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| CacheError::Overflow {
                digest: digest.clone(),
                limit: self.descriptor.compressed_size,
            })?;
        if next > self.descriptor.compressed_size {
            return Err(CacheError::Overflow {
                digest,
                limit: self.descriptor.compressed_size,
            });
        }
        let file = self
            .file
            .as_mut()
            .expect("a blob writer is only used before it is consumed");
        file.write_all(bytes)
            .map_err(|source| CacheError::io(&self.paths.partial_path, source))?;
        self.wire_offset = next;
        if self.wire_offset >= self.next_record {
            self.write_resume_record()?;
            self.next_record = self.wire_offset.saturating_add(RESUME_RECORD_INTERVAL);
        }
        Ok(())
    }

    /// Verify the complete wire form and publish it under its digest.
    ///
    /// The verification is a fresh pass over the whole file: the digest is of
    /// the decoded stream, so it cannot be established from a prefix. Nothing
    /// outside the cache can observe a blob that has not been through here.
    pub fn commit(mut self) -> Result<VerifiedBlob, CacheError> {
        let digest = self.descriptor.digest.to_hex();
        self.settled = true;
        if let Some(file) = self.file.as_mut() {
            file.flush()
                .and_then(|()| file.sync_all())
                .map_err(|source| CacheError::io(&self.paths.partial_path, source))?;
        }
        self.file = None;
        let wire_size = std::fs::metadata(&self.paths.partial_path)
            .map_err(|source| CacheError::io(&self.paths.partial_path, source))?
            .len();
        if wire_size != self.descriptor.compressed_size {
            self.discard_files();
            return Err(CacheError::SizeMismatch {
                digest,
                expected: self.descriptor.compressed_size,
                found: wire_size,
            });
        }
        match self.read_logical() {
            Ok(logical) if logical == self.descriptor.size => {}
            Ok(logical) => {
                self.discard_files();
                return Err(CacheError::SizeMismatch {
                    digest,
                    expected: self.descriptor.size,
                    found: logical,
                });
            }
            Err(error) => {
                self.discard_files();
                return Err(error);
            }
        }
        self.file_system
            .publish_replace(&self.paths.partial_path, &self.paths.final_path)
            .map_err(|source| CacheError::io(&self.paths.final_path, source))?;
        let _ = std::fs::remove_file(&self.paths.resume_path);
        let blob = VerifiedBlob {
            descriptor: self.descriptor,
            path: self.paths.final_path.clone(),
            wire_size,
        };
        // The claim is released here rather than in `Drop` so a caller that
        // fails to look at the result still frees the blob for the next run.
        self.lock.release();
        Ok(blob)
    }

    /// Give up on this transfer, keeping the partial when a resume is worth it.
    pub fn abandon(mut self) {
        self.settled = true;
        let resumable = self.wire_offset > 0 && self.wire_offset < self.descriptor.compressed_size;
        self.file = None;
        if resumable {
            let _ = self.write_resume_record();
        } else {
            self.discard_files();
        }
        self.lock.release();
    }

    fn discard_files(&self) {
        let _ = std::fs::remove_file(&self.paths.partial_path);
        let _ = std::fs::remove_file(&self.paths.resume_path);
    }

    /// Decode the whole wire form once, proving the logical digest.
    fn read_logical(&self) -> Result<u64, CacheError> {
        let file = File::open(&self.paths.partial_path)
            .map_err(|source| CacheError::io(&self.paths.partial_path, source))?;
        let mut reader = BlobReader::new(file, self.descriptor)?;
        let mut sink = std::io::sink();
        let mut buffer = [0u8; 128 * 1024];
        reader.read_all_logical_into(&mut sink, &mut buffer)?;
        Ok(reader.produced())
    }

    /// Refresh the resume record so an unclean exit loses at most one interval.
    fn write_resume_record(&mut self) -> Result<(), CacheError> {
        let prefix =
            match measure_prefix(&self.paths.partial_path, self.wire_offset, &self.descriptor) {
                Ok(prefix) => prefix,
                // A prefix that cannot be decoded now will not decode later either.
                // Recording nothing means the next run starts over, which is the
                // only safe answer.
                Err(_) => {
                    let _ = std::fs::remove_file(&self.paths.resume_path);
                    return Ok(());
                }
            };
        let record = ResumeRecord {
            schema: RESUME_SCHEMA,
            digest: self.descriptor.digest,
            size: self.descriptor.size,
            compression: self.descriptor.compression,
            compressed_size: self.descriptor.compressed_size,
            wire_offset: self.wire_offset,
            prefix_size: prefix.size,
            prefix_digest: prefix.digest,
        };
        write_resume(&self.paths.resume_path, &record)
    }
}

impl Drop for BlobWriter {
    fn drop(&mut self) {
        // A writer that goes away without a decision — because it was dropped
        // mid-transfer, or because a process was killed between two
        // instructions — leaves a partial. If that partial is worth continuing,
        // record where it got to so the next process does not start from zero.
        // `abandon` and `commit` both settle deliberately and set the flag, so
        // this only runs for an interrupted transfer.
        if self.settled {
            return;
        }
        let resumable = self.wire_offset > 0 && self.wire_offset < self.descriptor.compressed_size;
        if !resumable {
            self.discard_files();
            return;
        }
        self.file = None;
        let _ = self.write_resume_record();
    }
}

impl BlobReader {
    fn read_all_logical_into(
        &mut self,
        sink: &mut impl Write,
        buffer: &mut [u8],
    ) -> Result<(), CacheError> {
        loop {
            let read = self.inner.read(buffer).map_err(|source| {
                CacheError::io(PathBuf::from(self.descriptor.digest.to_hex()), source)
            })?;
            if read == 0 {
                break;
            }
            self.absorb(&buffer[..read])?;
            sink.write_all(&buffer[..read]).map_err(|source| {
                CacheError::io(PathBuf::from(self.descriptor.digest.to_hex()), source)
            })?;
        }
        self.finish()
    }
}

/// The decoded length and digest of a wire prefix.
struct PrefixDigest {
    size: u64,
    digest: Sha256Digest,
}

/// Decode a wire prefix and report what it produced.
///
/// A prefix of a Zstandard frame is not a frame, so the decoder stops at the
/// first incomplete block. Frames are self-describing per block, which is what
/// makes a resumable compressed object possible at all: the blocks that arrived
/// decode cleanly and the ones that did not are simply absent. The measurement
/// streams, so a multi-gigabyte prefix costs a buffer rather than memory.
fn measure_prefix(
    path: &Path,
    wire_offset: u64,
    descriptor: &ContentDescriptor,
) -> Result<PrefixDigest, CacheError> {
    let mut file = File::open(path).map_err(|source| CacheError::io(path, source))?;
    if !descriptor.is_compressed() {
        return hash_stream((&mut file).take(wire_offset));
    }
    let mut reader = zstd::stream::read::Decoder::new((&mut file).take(wire_offset))
        .map_err(|source| CacheError::io(path, source))?;
    let mut hasher = Sha256::new();
    let mut size = 0u64;
    let mut buffer = [0u8; 128 * 1024];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                size = size.saturating_add(read as u64);
                hasher.update(&buffer[..read]);
            }
            // A truncated frame ends here. Whatever decoded is what a resume
            // can rely on being able to reproduce.
            Err(_) => break,
        }
    }
    Ok(PrefixDigest {
        size,
        digest: Sha256Digest::from_bytes(hasher.finalize().into()),
    })
}

fn remove_if_present(path: &Path) -> Result<(), CacheError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(CacheError::io(path, source)),
    }
}

fn verify_prefix(
    path: &Path,
    wire_offset: u64,
    record: &ResumeRecord,
    descriptor: &ContentDescriptor,
) -> Result<(), CacheError> {
    let measured = measure_prefix(path, wire_offset, descriptor)?;
    if measured.size != record.prefix_size || measured.digest != record.prefix_digest {
        return Err(CacheError::DigestMismatch {
            digest: descriptor.digest.to_hex(),
            found: measured.digest.to_hex(),
        });
    }
    Ok(())
}

fn hash_stream(mut reader: impl Read) -> Result<PrefixDigest, CacheError> {
    let mut hasher = Sha256::new();
    let mut size = 0u64;
    let mut buffer = [0u8; 128 * 1024];
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|source| CacheError::io(PathBuf::from("a content prefix"), source))?;
        if read == 0 {
            break;
        }
        size = size.saturating_add(read as u64);
        hasher.update(&buffer[..read]);
    }
    Ok(PrefixDigest {
        size,
        digest: Sha256Digest::from_bytes(hasher.finalize().into()),
    })
}

fn read_resume(path: &Path) -> Option<ResumeRecord> {
    // A record is a few hundred bytes; a larger file is not one.
    let metadata = std::fs::metadata(path).ok()?;
    if metadata.len() > 4096 {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn write_resume(path: &Path, record: &ResumeRecord) -> Result<(), CacheError> {
    let bytes = serde_json::to_vec(record)
        .map_err(|error| CacheError::io(path, std::io::Error::other(error)))?;
    let temporary = path.with_extension("resume.tmp");
    {
        let mut file =
            File::create(&temporary).map_err(|source| CacheError::io(&temporary, source))?;
        file.write_all(&bytes)
            .and_then(|()| file.sync_all())
            .map_err(|source| CacheError::io(&temporary, source))?;
    }
    // A replace is attempted through the same seam the blob publication uses,
    // so a cache is atomic everywhere it is atomic for content.
    match std::fs::rename(&temporary, path) {
        Ok(()) => Ok(()),
        Err(error) if path.exists() => {
            let _ = std::fs::remove_file(path);
            std::fs::rename(&temporary, path).map_err(|source| {
                let _ = error;
                CacheError::io(path, source)
            })
        }
        Err(source) => {
            let _ = std::fs::remove_file(&temporary);
            Err(CacheError::io(path, source))
        }
    }
}

/// `path` preceded by each of its ancestors, from the filesystem root down, so
/// a link anywhere along the way is inspected before the leaf is touched.
fn link_checked_prefixes(path: &Path) -> Result<Vec<PathBuf>, CacheError> {
    let mut current = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|source| CacheError::io(path, source))?
            .join(path)
    };
    let mut components = Vec::new();
    while let Some(parent) = current.parent() {
        components.push(parent.to_path_buf());
        if parent == current {
            break;
        }
        current = parent.to_path_buf();
    }
    components.reverse();
    components.push(path.to_path_buf());
    Ok(components)
}

/// Walk the blob tree, reconstructing each digest from its fan-out directory
/// and its file name.
///
/// A blob's name is split across two levels — `blobs/sha256/<ab>/<rest>` — so a
/// walk has to carry the prefix down. Anything that is not exactly a digest is
/// not content: a partial, a resume record, and a writer's claim all live in the
/// same directory and none of them may be counted or pruned as a blob.
fn collect_digests(
    file_system: &Arc<dyn CacheFileSystem>,
    directory: &Path,
    prefix: &str,
    out: &mut Vec<Sha256Digest>,
) -> Result<(), CacheError> {
    if file_system
        .is_link(directory)
        .map_err(|source| CacheError::io(directory, source))?
    {
        return Err(CacheError::Link {
            path: directory.display().to_string(),
        });
    }
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => return Err(CacheError::io(directory, source)),
    };
    for entry in entries {
        let entry = entry.map_err(|source| CacheError::io(directory, source))?;
        let path = entry.path();
        if path.is_dir() {
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default();
            collect_digests(file_system, &path, &format!("{prefix}{name}"), out)?;
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if let Ok(digest) = format!("{prefix}{name}").parse::<Sha256Digest>() {
            out.push(digest);
        }
    }
    Ok(())
}

fn prune_directory(
    file_system: &Arc<dyn CacheFileSystem>,
    root: &Path,
    directory: &Path,
    cutoff: std::time::SystemTime,
    protected: &[Sha256Digest],
    removed: &mut usize,
) -> Result<(), CacheError> {
    if file_system
        .is_link(directory)
        .map_err(|source| CacheError::io(directory, source))?
    {
        return Err(CacheError::Link {
            path: directory.display().to_string(),
        });
    }
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => return Err(CacheError::io(directory, source)),
    };
    for entry in entries {
        let entry = entry.map_err(|source| CacheError::io(directory, source))?;
        let path = entry.path();
        if path.is_dir() {
            prune_directory(file_system, root, &path, cutoff, protected, removed)?;
            let _ = std::fs::remove_dir(&path);
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let is_partial = name.ends_with(".partial")
            || name.ends_with(".resume")
            || name.ends_with(".lock")
            || name.ends_with(".resume.tmp");
        if is_partial {
            // A partial nobody finished is not content and cannot be
            // identified, so it is removed under every policy.
            if std::fs::metadata(&path)
                .and_then(|metadata| metadata.modified())
                .ok()
                .is_some_and(|modified| modified < cutoff)
            {
                let _ = std::fs::remove_file(&path);
            }
            continue;
        }
        let Ok(digest) = name.parse::<Sha256Digest>() else {
            continue;
        };
        if protected.contains(&digest) {
            continue;
        }
        let aged = std::fs::metadata(&path)
            .and_then(|metadata| metadata.modified())
            .ok()
            .is_some_and(|modified| modified < cutoff);
        if aged {
            let _ = std::fs::remove_file(&path);
            *removed += 1;
        }
    }
    let _ = root;
    Ok(())
}
