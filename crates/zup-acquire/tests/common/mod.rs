//! Shared fixtures for the acquisition tests.
//!
//! The point of these fixtures is that nothing here needs a network or an HTTP
//! server. A blob is some bytes, a descriptor names them, and a source hands
//! them over. That is the whole contract, and every property the engine claims
//! has to hold without anything else in the picture.

// Each integration binary is its own crate and uses a different subset of these
// helpers, so one that goes unused in a given binary is expected.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use sha2::{Digest, Sha256};
use zup_acquire::{
    CachePolicy, Cancellation, ContentCache, ContentCatalog, ContentDescriptor, ContentKind,
    DirectorySource, MemorySource,
};
use zup_core::Sha256Digest;

/// Deterministic pseudo-random bytes, so a fixture is reproducible and a
/// compressibility claim in a test is a fact rather than a hope.
pub fn payload(seed: u8, length: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(length);
    let mut state = u64::from(seed).wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    while out.len() < length {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        out.extend_from_slice(&state.to_le_bytes());
    }
    out.truncate(length);
    out
}

/// Compressible bytes, for the paths that exercise the resume machinery.
pub fn compressible(seed: u8, length: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(length);
    let mut state = seed as u64 | 1;
    while out.len() < length {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let byte = (state >> 33) as u8 & 0x1f;
        out.push(byte.wrapping_add(seed));
    }
    out.truncate(length);
    out
}

pub fn digest_of(bytes: &[u8]) -> Sha256Digest {
    Sha256Digest::from_bytes(Sha256::digest(bytes).into())
}

/// A payload descriptor: the digest is of the *decompressed* bytes and the wire
/// form is a Zstandard frame, which is how a blob actually travels.
pub fn payload_descriptor(bytes: &[u8], level: i32) -> ContentDescriptor {
    let wire = zstd::stream::encode_all(bytes, level).expect("fixture compresses");
    ContentDescriptor::compressed(
        ContentKind::Payload,
        digest_of(bytes),
        wire.len() as u64,
        bytes.len() as u64,
    )
}

/// The wire form a payload descriptor expects on the wire.
pub fn wire_of(bytes: &[u8], level: i32) -> Vec<u8> {
    zstd::stream::encode_all(bytes, level).expect("fixture compresses")
}

/// A document descriptor, carried exactly as given.
pub fn document_descriptor(kind: ContentKind, bytes: &[u8]) -> ContentDescriptor {
    ContentDescriptor::stored(kind, digest_of(bytes), bytes.len() as u64)
}

/// A content catalog over `(logical bytes, level)` pairs, keyed by digest.
pub fn catalog(
    blobs: &[(&[u8], i32)],
) -> (ContentCatalog, BTreeMap<Sha256Digest, ContentDescriptor>) {
    let mut entries = Vec::new();
    let mut descriptors = BTreeMap::new();
    for (bytes, level) in blobs {
        let descriptor = payload_descriptor(bytes, *level);
        entries.push(zup_acquire::CatalogEntry::compressed(
            descriptor.digest,
            descriptor.compressed_size,
            descriptor.size,
        ));
        descriptors.insert(descriptor.digest, descriptor);
    }
    (
        ContentCatalog::new(entries).expect("the fixture catalog is well formed"),
        descriptors,
    )
}

/// A cache in a directory that is removed when the test ends.
pub struct TestCache {
    path: PathBuf,
    _dir: tempfile::TempDir,
}

impl TestCache {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = dir.path().join("cache");
        Self { path, _dir: dir }
    }

    pub fn root(&self) -> &Path {
        &self.path
    }

    pub fn open(&self, policy: CachePolicy) -> ContentCache {
        ContentCache::open(&self.path, policy).expect("the fixture cache opens")
    }

    pub fn shared(&self, policy: CachePolicy) -> Arc<ContentCache> {
        Arc::new(self.open(policy))
    }
}

/// Write a blob into a directory in the immutable web layout, so a
/// `DirectorySource` can find it exactly as a CDN would serve it.
pub fn seed_web_tree(root: &Path, descriptor: &ContentDescriptor, wire: &[u8]) {
    let relative = zup_acquire::blob_path(&descriptor.digest).to_string();
    let path = root.join(relative.replace('/', std::path::MAIN_SEPARATOR_STR));
    std::fs::create_dir_all(path.parent().expect("a blob path has a parent"))
        .expect("the tree is created");
    std::fs::write(&path, wire).expect("the fixture blob is written");
}

/// A memory source over `(descriptor, wire)` pairs.
pub fn memory_source(
    name: &str,
    blobs: &[(ContentDescriptor, Vec<u8>)],
) -> Arc<dyn zup_acquire::ArtifactSource> {
    let mut map: BTreeMap<Sha256Digest, Vec<u8>> = BTreeMap::new();
    for (descriptor, wire) in blobs {
        map.insert(descriptor.digest, wire.clone());
    }
    Arc::new(MemorySource::new(name, map))
}

/// A directory source over a staged web tree.
pub fn directory_source(name: &str, root: &Path) -> Arc<dyn zup_acquire::ArtifactSource> {
    Arc::new(DirectorySource::new(name, root))
}

/// A cancellation flag that is never raised.
pub fn never() -> Arc<dyn Cancellation> {
    Arc::new(zup_acquire::NeverCancelled)
}

/// A cancellation flag that is already raised.
pub fn cancelled() -> Arc<dyn Cancellation> {
    let flag = Arc::new(zup_acquire::CancelFlag::new());
    flag.cancel();
    flag
}

/// A stager that records the digests it was handed, in order.
#[derive(Default)]
pub struct RecordingStager {
    seen: std::sync::Mutex<Vec<Sha256Digest>>,
    fail_on: Option<Sha256Digest>,
}

impl RecordingStager {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn failing(digest: Sha256Digest) -> Arc<Self> {
        Arc::new(Self {
            seen: std::sync::Mutex::new(Vec::new()),
            fail_on: Some(digest),
        })
    }

    pub fn seen(&self) -> Vec<Sha256Digest> {
        self.seen
            .lock()
            .expect("the recorder is not poisoned")
            .clone()
    }
}

impl zup_acquire::BlobStager for RecordingStager {
    fn prepare(&self) -> zup_acquire::StagerFuture<'_, Result<(), String>> {
        Box::pin(std::future::ready(Ok(())))
    }

    fn stage(
        &self,
        blob: zup_acquire::VerifiedBlob,
    ) -> zup_acquire::StagerFuture<'_, Result<(), String>> {
        let digest = blob.descriptor.digest;
        if self.fail_on == Some(digest) {
            return Box::pin(std::future::ready(Err(format!(
                "the fixture refuses {digest}"
            ))));
        }
        self.seen
            .lock()
            .expect("the recorder is not poisoned")
            .push(digest);
        Box::pin(std::future::ready(Ok(())))
    }
}

/// A source that reports a fixed failure, for chain and retry tests.
pub struct FailingSource {
    name: String,
    reports: std::sync::atomic::AtomicUsize,
    detail: String,
}

impl FailingSource {
    pub fn new(name: &str, detail: &str) -> Arc<Self> {
        Arc::new(Self {
            name: name.to_owned(),
            reports: std::sync::atomic::AtomicUsize::new(0),
            detail: detail.to_owned(),
        })
    }

    pub fn reports(&self) -> usize {
        self.reports.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl zup_acquire::ArtifactSource for FailingSource {
    fn name(&self) -> &str {
        &self.name
    }

    fn contains(&self, _descriptor: &ContentDescriptor) -> bool {
        true
    }

    fn acquire<'a>(
        &'a self,
        request: zup_acquire::AcquireRequest<'a>,
    ) -> zup_acquire::SourceFuture<'a, Result<zup_acquire::VerifiedBlob, zup_acquire::SourceError>>
    {
        self.reports
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let detail = self.detail.clone();
        Box::pin(async move {
            Err(zup_acquire::SourceError::unavailable(
                &self.name,
                request.descriptor,
                detail,
            ))
        })
    }
}

/// A source that claims to carry a blob and then serves the wrong bytes, which
/// is what a hostile origin looks like from the engine's side.
pub struct LyingSource {
    name: String,
    descriptor: ContentDescriptor,
    wire: Vec<u8>,
}

impl LyingSource {
    pub fn new(name: &str, descriptor: ContentDescriptor, wire: Vec<u8>) -> Arc<Self> {
        Arc::new(Self {
            name: name.to_owned(),
            descriptor,
            wire,
        })
    }
}

impl zup_acquire::ArtifactSource for LyingSource {
    fn name(&self) -> &str {
        &self.name
    }

    fn contains(&self, descriptor: &ContentDescriptor) -> bool {
        descriptor.digest == self.descriptor.digest
    }

    fn acquire<'a>(
        &'a self,
        _request: zup_acquire::AcquireRequest<'a>,
    ) -> zup_acquire::SourceFuture<'a, Result<zup_acquire::VerifiedBlob, zup_acquire::SourceError>>
    {
        let name = self.name.clone();
        let descriptor = self.descriptor;
        let wire = self.wire.clone();
        Box::pin(async move {
            // The bytes are written through the cache exactly as honest bytes
            // would be. The cache is what refuses them, which is the property
            // under test: a source cannot launder content by being believed.
            let dir = tempfile::tempdir().map_err(|error| {
                zup_acquire::SourceError::unavailable(&name, &descriptor, error.to_string())
            })?;
            let cache = ContentCache::open(dir.path(), CachePolicy::Keep).map_err(|error| {
                zup_acquire::SourceError::unavailable(&name, &descriptor, error.to_string())
            })?;
            let mut writer = cache.writer(&descriptor).map_err(|error| {
                zup_acquire::SourceError::unavailable(&name, &descriptor, error.to_string())
            })?;
            writer.write(&wire).map_err(|error| {
                zup_acquire::SourceError::unavailable(&name, &descriptor, error.to_string())
            })?;
            writer.commit().map_err(|error| {
                zup_acquire::SourceError::unavailable(&name, &descriptor, error.to_string())
            })
        })
    }
}

/// The compression level the fixtures use.
pub const LEVEL: i32 = 3;
