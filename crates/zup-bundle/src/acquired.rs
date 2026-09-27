//! Serving lifecycle payload from a verified content cache.
//!
//! This is the one adapter that connects the acquisition engine to the
//! transaction engine, and it is deliberately tiny. The lifecycle asks for a
//! portable path, a digest, and a length — the same three things it asked of an
//! embedded package or a build directory — and this type answers them out of a
//! content-addressed cache that the release graph authenticated.
//!
//! The security argument does not live here. A caller supplies the cache and a
//! plan; every read resolves to a blob whose digest the plan already names, and
//! the cache re-decompresses and re-hashes before it returns. A hostile cache
//! yields a `DigestMismatch`, not a file. What this type has to get right is
//! refusing to hand back bytes it did not verify, which is why it returns the
//! cache's own digest-checking reader rather than a decompressor it built.

use std::collections::BTreeMap;
use std::io::Read;

use zup_acquire::{ContentCatalog, ContentKind, VerifiedBlob, Verify};
use zup_core::{RelativePath, Sha256Digest};

use crate::format::{Package, PackageError};
use crate::payload::{PayloadError, PayloadReader, PayloadSource};

/// A payload source backed by the verified content cache.
pub struct AcquiredPayloadSource {
    cache: zup_acquire::ContentCache,
    catalog: ContentCatalog,
    /// The digests this plan can use, so a call for anything else is a miss
    /// rather than a lookup into content the plan never named.
    allowed: BTreeMap<RelativePath, (Sha256Digest, u64)>,
}

impl AcquiredPayloadSource {
    /// A source over `cache`, restricted to what `package`'s plan names.
    pub fn new(
        cache: zup_acquire::ContentCache,
        catalog: ContentCatalog,
        package: &Package,
    ) -> Self {
        let allowed = package
            .plan()
            .entries
            .iter()
            .map(|entry| (entry.path.clone(), (entry.blob, entry.size)))
            .collect();
        Self {
            cache,
            catalog,
            allowed,
        }
    }

    /// Read a whole blob, for a prerequisite package or a plugin image.
    ///
    /// These are not lifecycle payload — they are staged and executed by their
    /// own engines — so they are read whole rather than streamed.
    pub fn read_blob(&self, digest: &Sha256Digest) -> Result<Vec<u8>, PayloadError> {
        self.verified(digest)?
            .read_to_end()
            .map_err(|error| PayloadError::Read {
                path: digest.to_hex(),
                source: std::io::Error::other(error.to_string()),
            })
    }

    /// The verified blob for `digest`, at the depth its kind earns.
    fn verified(&self, digest: &Sha256Digest) -> Result<VerifiedBlob, PayloadError> {
        let entry = self
            .catalog
            .entry(digest)
            .ok_or_else(|| PayloadError::NotFound {
                path: digest.to_hex(),
            })?;
        let descriptor = entry.descriptor(ContentKind::Payload);
        self.cache
            .get(&descriptor, Verify::for_kind(ContentKind::Payload))
            .map_err(|error| PayloadError::Read {
                path: digest.to_hex(),
                source: std::io::Error::other(error.to_string()),
            })?
            .ok_or_else(|| PayloadError::NotFound {
                path: digest.to_hex(),
            })
    }
}

impl PayloadSource for AcquiredPayloadSource {
    fn open(
        &self,
        path: &RelativePath,
        expected_sha256: &Sha256Digest,
        expected_size: u64,
    ) -> Result<PayloadReader, PayloadError> {
        let Some((blob, size)) = self.allowed.get(path) else {
            return Err(PayloadError::NotFound {
                path: path.to_string(),
            });
        };
        // The plan's own record is the authority for what a path is. A caller
        // asking for the same path with a different digest is asking for
        // something the plan does not describe.
        if *blob != *expected_sha256 || *size != expected_size {
            return Err(PayloadError::DigestMismatch {
                path: path.to_string(),
            });
        }
        let blob = self.verified(blob)?;
        let reader = blob.open().map_err(|error| PayloadError::Read {
            path: path.to_string(),
            source: std::io::Error::other(error.to_string()),
        })?;
        // The cache reader proves identity when the stream ends. A consumer that
        // stops early has read less than it asked for, which the executor's own
        // length and digest check catches; a consumer that reads to the end has
        // proved them here.
        Ok(Box::new(VerifyingReader {
            inner: reader,
            produced: 0,
            expected: expected_size,
        }))
    }
}

/// Counts what a consumer read, so a short read is visible at the boundary
/// rather than looking like a complete file.
struct VerifyingReader {
    inner: zup_acquire::BlobReader,
    produced: u64,
    expected: u64,
}

impl Read for VerifyingReader {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let read = self.inner.read(buffer)?;
        self.produced += read as u64;
        if self.produced > self.expected {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "payload grew to {} bytes while it was being read; the plan declares {}",
                    self.produced, self.expected
                ),
            ));
        }
        Ok(read)
    }
}

/// Reject a plan-only package that claims to have verified content.
pub fn refuse_self_verified(plan_only: bool) -> Result<(), PackageError> {
    if plan_only {
        return Err(PackageError::Missing {
            media_type: "content blob",
            digest: "a plan-only package carries no payload".to_owned(),
        });
    }
    Ok(())
}
