//! Sources that are already on the machine.
//!
//! Two of them exist because "download it" is the wrong answer too often: a USB
//! stick or a network share that holds the same tree a CDN would, and the
//! verified bytes already sitting in a warm cache. Both are untrusted in exactly
//! the same way, and both are checked the same way: descriptor, then size, then
//! digest, and only then use.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use zup_core::Sha256Digest;

use crate::cache::ContentCache;
use crate::descriptor::{ContentDescriptor, RelativeContentPath};
use crate::error::SourceError;
use crate::filesystem::{CacheFileSystem, PortableCacheFileSystem};
use crate::layout::blob_path;
use crate::source::{AcquireRequest, ArtifactSource, SourceFuture};

/// A local directory that carries the immutable web tree.
///
/// This is the offline fallback: point it at a tree staged by
/// `zup publish stage`, a USB stick, an enterprise share, or a CI image that
/// was warmed earlier, and the same closure is satisfied without a second
/// packaging format. The layout is the one the CDN serves, so nothing has to be
/// repackaged to seed an install.
///
/// The tree is untrusted. Every read is confined to the root, no link is
/// followed, the wire length is bounded by the descriptor before a byte is
/// read, and the digest is proved by the cache on publication.
pub struct DirectorySource {
    name: String,
    root: PathBuf,
    file_system: Arc<dyn CacheFileSystem>,
}

impl DirectorySource {
    /// A source over `root` on the portable filesystem.
    pub fn new(name: impl Into<String>, root: impl Into<PathBuf>) -> Self {
        Self {
            name: name.into(),
            root: root.into(),
            file_system: Arc::new(PortableCacheFileSystem),
        }
    }

    /// A source that refuses links through `file_system`.
    pub fn with_file_system(
        name: impl Into<String>,
        root: impl Into<PathBuf>,
        file_system: Arc<dyn CacheFileSystem>,
    ) -> Self {
        Self {
            name: name.into(),
            root: root.into(),
            file_system,
        }
    }

    /// The directory this source reads.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Where one blob lives in the tree, if the tree carries it.
    ///
    /// Two shapes are recognized, which is what makes a staged web tree and a
    /// bare blob directory both work:
    ///
    /// - the full immutable layout, `blobs/sha256/<ab>/<hex>`
    /// - the flat form, `<ab>/<hex>`, which is what a directory of digests
    ///   looks like when it was produced by copying blobs out of a cache
    pub fn locate(&self, descriptor: &ContentDescriptor) -> Option<PathBuf> {
        let full = self.root.join(blob_path(&descriptor.digest).to_string());
        if full.is_file() && !self.is_link(&full).unwrap_or(true) {
            return Some(full);
        }
        let hex = descriptor.digest.to_hex();
        let flat = self.root.join(&hex[..2]).join(&hex[2..]);
        if flat.is_file() && !self.is_link(&flat).unwrap_or(true) {
            return Some(flat);
        }
        None
    }

    /// Read a named document out of the tree, for a release or a catalog that
    /// has not been authenticated yet.
    ///
    /// This exists for bootstrapping only: a document read this way is
    /// untrusted until its digest matches a claim in an authenticated release.
    pub fn read_document(&self, relative: &RelativeContentPath) -> std::io::Result<Vec<u8>> {
        let path = self.resolve(relative)?;
        if self.is_link(&path)? {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "a content path is a link",
            ));
        }
        let metadata = std::fs::symlink_metadata(&path)?;
        if !metadata.is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "a content path is not a regular file",
            ));
        }
        let mut out = Vec::with_capacity(metadata.len().min(1 << 20) as usize);
        FileLimitReader::new(&path, relative)?.read_to_end(&mut out)?;
        Ok(out)
    }

    fn resolve(&self, relative: &RelativeContentPath) -> std::io::Result<PathBuf> {
        let mut path = self.root.clone();
        for segment in relative.to_string().split('/') {
            path.push(segment);
        }
        if !path.starts_with(&self.root) || path == self.root {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "a content path escapes its root",
            ));
        }
        for component in link_checked_prefixes(&path)? {
            if self.is_link(&component)? {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "a content path traverses a link",
                ));
            }
        }
        Ok(path)
    }

    fn is_link(&self, path: &Path) -> std::io::Result<bool> {
        self.file_system.is_link(path)
    }
}

impl ArtifactSource for DirectorySource {
    fn name(&self) -> &str {
        &self.name
    }

    fn contains(&self, descriptor: &ContentDescriptor) -> bool {
        self.locate(descriptor).is_some()
    }

    fn acquire<'a>(
        &'a self,
        request: AcquireRequest<'a>,
    ) -> SourceFuture<'a, Result<crate::VerifiedBlob, SourceError>> {
        Box::pin(async move {
            let descriptor = *request.descriptor;
            let Some(path) = self.locate(&descriptor) else {
                return Err(SourceError::absent(self.name(), &descriptor));
            };
            let relative = blob_path(&descriptor.digest);
            let mut writer = request.cache.writer(&descriptor).map_err(|error| {
                SourceError::unavailable(self.name(), &descriptor, error.to_string())
            })?;
            let mut reader = FileLimitReader::new(&path, &relative).map_err(|error| {
                SourceError::unavailable(self.name(), &descriptor, error.to_string())
            })?;
            let mut buffer = [0u8; 128 * 1024];
            loop {
                if request.cancellation.is_cancelled() {
                    writer.abandon();
                    return Err(SourceError::unavailable(
                        self.name(),
                        &descriptor,
                        "cancelled",
                    ));
                }
                let read = reader.read(&mut buffer).map_err(|error| {
                    SourceError::unavailable(self.name(), &descriptor, error.to_string())
                })?;
                if read == 0 {
                    break;
                }
                if let Err(error) = writer.write(&buffer[..read]) {
                    writer.abandon();
                    return Err(SourceError::unavailable(
                        self.name(),
                        &descriptor,
                        error.to_string(),
                    ));
                }
            }
            // A local source resumes by seeking, which is the one thing an
            // untrusted tree is genuinely better at than a network. A partial
            // whose prefix did not verify is not resumed from, so a seeded
            // partial cannot smuggle bytes past the digest.
            writer.commit().map_err(|error| {
                SourceError::unavailable(self.name(), &descriptor, error.to_string())
            })
        })
    }
}

/// Reads a file for a local source.
///
/// The descriptor's wire length is the real ceiling, and it is enforced by the
/// cache writer every byte passes through, so this type is just the reader.
struct FileLimitReader {
    file: std::fs::File,
}

impl FileLimitReader {
    fn new(path: &Path, _relative: &RelativeContentPath) -> std::io::Result<Self> {
        Ok(Self {
            file: std::fs::File::open(path)?,
        })
    }
}

impl Read for FileLimitReader {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.file.read(buffer)
    }
}

/// Content that is already in memory, keyed by digest.
///
/// This is what an embedded offline artifact and a thin artifact's metadata
/// region are, and what tests use to prove the engine has no transport
/// dependency at all.
pub struct MemorySource {
    name: String,
    blobs: BTreeMap<Sha256Digest, Vec<u8>>,
}

impl MemorySource {
    /// A source over `blobs`.
    pub fn new(name: impl Into<String>, blobs: BTreeMap<Sha256Digest, Vec<u8>>) -> Self {
        Self {
            name: name.into(),
            blobs,
        }
    }

    /// A source over one blob.
    pub fn single(name: impl Into<String>, digest: Sha256Digest, bytes: Vec<u8>) -> Self {
        let mut blobs = BTreeMap::new();
        blobs.insert(digest, bytes);
        Self::new(name, blobs)
    }

    /// How many blobs this source carries.
    pub fn len(&self) -> usize {
        self.blobs.len()
    }

    /// Whether this source carries nothing.
    pub fn is_empty(&self) -> bool {
        self.blobs.is_empty()
    }
}

impl ArtifactSource for MemorySource {
    fn name(&self) -> &str {
        &self.name
    }

    fn contains(&self, descriptor: &ContentDescriptor) -> bool {
        self.blobs.contains_key(&descriptor.digest)
    }

    fn acquire<'a>(
        &'a self,
        request: AcquireRequest<'a>,
    ) -> SourceFuture<'a, Result<crate::VerifiedBlob, SourceError>> {
        Box::pin(async move {
            let descriptor = *request.descriptor;
            let Some(bytes) = self.blobs.get(&descriptor.digest) else {
                return Err(SourceError::absent(self.name(), &descriptor));
            };
            if request.cancellation.is_cancelled() {
                return Err(SourceError::unavailable(
                    self.name(),
                    &descriptor,
                    "cancelled",
                ));
            }
            let mut writer = request.cache.writer(&descriptor).map_err(|error| {
                SourceError::unavailable(self.name(), &descriptor, error.to_string())
            })?;
            writer.write(bytes).map_err(|error| {
                SourceError::unavailable(self.name(), &descriptor, error.to_string())
            })?;
            writer.commit().map_err(|error| {
                SourceError::unavailable(self.name(), &descriptor, error.to_string())
            })
        })
    }
}

/// A source that is already satisfied.
///
/// A cache is a source in its own right, and making it one means a caller that
/// wants a chain has exactly one way to express "check the cache first" rather
/// than two: probe the cache in the session, and use this when a caller needs a
/// chain with no network at all.
pub struct SatisfiedSource {
    cache: Arc<ContentCache>,
}

impl SatisfiedSource {
    /// A source over `cache`, verifying each blob at the policy its kind earns.
    pub fn new(cache: Arc<ContentCache>) -> Self {
        Self { cache }
    }
}

impl ArtifactSource for SatisfiedSource {
    fn name(&self) -> &str {
        "cache"
    }

    fn contains(&self, descriptor: &ContentDescriptor) -> bool {
        self.cache
            .probe(descriptor, crate::Verify::for_kind(descriptor.kind()))
            .map(|probe| probe == crate::CacheProbe::Absent)
            .map(|absent| !absent)
            .unwrap_or(false)
    }

    fn acquire<'a>(
        &'a self,
        request: AcquireRequest<'a>,
    ) -> SourceFuture<'a, Result<crate::VerifiedBlob, SourceError>> {
        Box::pin(async move {
            let descriptor = *request.descriptor;
            self.cache
                .get(&descriptor, crate::Verify::for_kind(descriptor.kind()))
                .map_err(|error| {
                    SourceError::unavailable(self.name(), &descriptor, error.to_string())
                })?
                .ok_or_else(|| SourceError::absent(self.name(), &descriptor))
        })
    }
}

fn link_checked_prefixes(path: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut current = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
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
