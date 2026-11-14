//! The immutable web layout: one shape for staging and for reading.
//!
//! A static origin is enough because every object is named by its identity. The
//! layout is deliberately dull:
//!
//! ```text
//! metadata/                              TUF metadata
//! releases/<channel>.json                authenticated release descriptor
//! documents/sha256/<ab>/<digest>         immutable JSON documents
//! blobs/sha256/<ab>/<abcdef…>            one compressed blob
//! ```
//!
//! Nothing under `blobs/` ever changes: the name is the digest, so an origin
//! may cache it forever, a client may resume into it, and a mirror may hold it
//! without being trusted. The server understands none of it - it is a static
//! file tree.

use serde::{Deserialize, Serialize};
use zup_core::Sha256Digest;

use crate::descriptor::{ContentCompression, RelativeContentPath};

/// Root of the content-addressed blob tree, relative to a repository.
pub const BLOB_ROOT: &str = "blobs/sha256";

/// Directory holding the authenticated release descriptors.
pub const RELEASE_ROOT: &str = "releases";

/// Directory holding TUF metadata.
pub const METADATA_ROOT: &str = "metadata";

/// Path of the release descriptor for `channel`.
pub fn release_path(channel: &str) -> Result<RelativeContentPath, &'static str> {
    check_channel(channel)?;
    RelativeContentPath::parse(&format!("{RELEASE_ROOT}/{channel}.json"))
}

/// Path of an immutable metadata document, hashed over its stored bytes.
pub fn document_path(digest: &Sha256Digest) -> RelativeContentPath {
    let hex = digest.to_hex();
    RelativeContentPath::parse(&format!("documents/sha256/{}/{}", &hex[..2], &hex[2..]))
        .expect("a SHA-256 digest is a valid document path")
}

/// Path of one blob within the content-addressed tree.
///
/// The fan-out is two hex characters, which keeps any one directory small
/// enough for a plain static host to serve without listing a million entries.
pub fn blob_path(digest: &Sha256Digest) -> RelativeContentPath {
    let hex = digest.to_hex();
    // The hex form is produced by this crate's own digest type and is always
    // 64 lowercase characters, so the split is total.
    RelativeContentPath::parse(&format!("{BLOB_ROOT}/{}/{}", &hex[..2], &hex[2..]))
        .expect("a SHA-256 hex digest is a valid content path")
}

/// Refuse a channel name that could escape the release directory.
///
/// The channel reaches a filesystem path and a URL, so it is bounded to the
/// same shape the manifest validator already requires: lowercase ASCII
/// letters, digits, and hyphens.
pub fn check_channel(channel: &str) -> Result<(), &'static str> {
    if channel.is_empty() {
        return Err("a channel name cannot be empty");
    }
    if channel.len() > 32 {
        return Err("a channel name cannot exceed 32 bytes");
    }
    if !channel
        .bytes()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err("a channel name must be lowercase ASCII letters, digits, and hyphens");
    }
    Ok(())
}

/// Refuse a variant name that could escape the variant directory.
pub fn check_segment(name: &str) -> Result<(), &'static str> {
    if name.is_empty() {
        return Err("a variant name cannot be empty");
    }
    if name.len() > 64 {
        return Err("a variant name cannot exceed 64 bytes");
    }
    if !name
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_' || byte == b'.')
    {
        return Err("a variant name must be ASCII letters, digits, `-`, `_`, or `.`");
    }
    if name.starts_with('.') {
        return Err("a variant name cannot start with `.`");
    }
    Ok(())
}

/// One entry of a content catalog: what a digest costs to move.
///
/// The catalog is the portable successor of a store table. It exists so an
/// online client knows every blob's wire size before it fetches one byte,
/// which is what makes a pre-download estimate exact rather than a guess, and
/// what makes a size cap enforceable from authenticated metadata instead of
/// from a response header.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogEntry {
    pub digest: Sha256Digest,
    /// How the wire form reaches the logical form.
    pub compression: ContentCompression,
    /// Length of the wire form.
    pub compressed_size: u64,
    /// Length of the logical form.
    pub size: u64,
}

impl CatalogEntry {
    /// Describe one catalogued blob carried without compression.
    pub fn stored(digest: Sha256Digest, size: u64) -> Self {
        Self {
            digest,
            compression: ContentCompression::None,
            compressed_size: size,
            size,
        }
    }

    /// Describe one catalogued blob carried as a Zstandard frame.
    pub fn compressed(digest: Sha256Digest, compressed_size: u64, size: u64) -> Self {
        Self {
            digest,
            compression: ContentCompression::Zstandard,
            compressed_size,
            size,
        }
    }

    /// The descriptor an acquisition session would schedule for this entry.
    pub fn descriptor(&self, kind: crate::ContentKind) -> crate::ContentDescriptor {
        crate::ContentDescriptor::new(
            kind,
            self.digest,
            self.compression,
            self.compressed_size,
            self.size,
        )
    }
}

/// Every unique blob a release can serve, with both of its sizes.
///
/// Entries are sorted by digest with no duplicates, so the document is a
/// function of the content set and a reader can binary-search it. It is
/// authenticated by the release descriptor, which names it by digest and
/// length, so a client never parses a catalog it cannot tie to a release.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContentCatalog {
    pub schema: u32,
    pub blobs: Vec<CatalogEntry>,
}

/// Current content catalog schema.
pub const CATALOG_SCHEMA: u32 = 1;

impl ContentCatalog {
    /// Build a catalog from entries, sorting by digest and removing repeats.
    ///
    /// A digest that appears twice is one object on an origin, so it is one
    /// entry here; a reader that counted it twice would plan two transfers and
    /// take one.
    pub fn new(mut entries: Vec<CatalogEntry>) -> Result<Self, crate::CacheError> {
        entries.sort_unstable_by_key(|entry| entry.digest);
        entries.dedup_by_key(|entry| entry.digest);
        let catalog = Self {
            schema: CATALOG_SCHEMA,
            blobs: entries,
        };
        catalog.validate().map_err(crate::CacheError::Descriptor)?;
        Ok(catalog)
    }

    /// Encode the catalog as canonical JSON.
    pub fn encode(&self) -> Result<Vec<u8>, crate::AcquireError> {
        let bytes = serde_json::to_vec(self)?;
        if bytes.len() as u64 > crate::MAX_CATALOG_BYTES {
            return Err(crate::AcquireError::TooLarge {
                kind: crate::ContentKind::Catalog.as_str(),
                size: bytes.len() as u64,
                limit: crate::MAX_CATALOG_BYTES,
            });
        }
        Ok(bytes)
    }

    /// Parse a catalog, enforcing its bound before deserializing.
    pub fn parse(bytes: &[u8]) -> Result<Self, crate::AcquireError> {
        if bytes.len() as u64 > crate::MAX_CATALOG_BYTES {
            return Err(crate::AcquireError::TooLarge {
                kind: crate::ContentKind::Catalog.as_str(),
                size: bytes.len() as u64,
                limit: crate::MAX_CATALOG_BYTES,
            });
        }
        let catalog: Self = serde_json::from_slice(bytes)?;
        catalog
            .validate()
            .map_err(crate::AcquireError::Descriptor)?;
        Ok(catalog)
    }

    /// Reject a catalog that could make a reader allocate or that is not in
    /// canonical order.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.schema != CATALOG_SCHEMA {
            return Err("unsupported content catalog schema");
        }
        if self.blobs.is_empty() {
            return Err("a content catalog names no blobs");
        }
        if self.blobs.len() > crate::MAX_CLOSURE_ITEMS {
            return Err("a content catalog names more blobs than this build accepts");
        }
        let mut previous: Option<Sha256Digest> = None;
        for entry in &self.blobs {
            if entry.size == 0 || entry.compressed_size == 0 {
                return Err("a catalog entry declares zero bytes");
            }
            if entry.size > crate::MAX_PAYLOAD_BYTES || entry.size > crate::MAX_RUNTIME_BYTES {
                return Err("a catalog entry exceeds the size limit for content");
            }
            // A wire form that claims to expand past its kind's ratio is a
            // compression bomb wearing a descriptor. The absolute size bound
            // already caps the damage; this refuses the shape outright.
            if entry.size
                > entry
                    .compressed_size
                    .saturating_mul(crate::ContentKind::Payload.max_expansion_ratio())
            {
                return Err("a catalog entry declares an expansion beyond its limit");
            }
            if let Some(previous) = previous
                && entry.digest <= previous
            {
                return Err("a content catalog is not sorted by strictly ascending digest");
            }
            previous = Some(entry.digest);
        }
        Ok(())
    }

    /// Look up one blob's sizes.
    pub fn entry(&self, digest: &Sha256Digest) -> Option<&CatalogEntry> {
        self.blobs
            .binary_search_by(|entry| entry.digest.cmp(digest))
            .ok()
            .map(|index| &self.blobs[index])
    }

    /// Every digest this catalog describes, in ascending order.
    pub fn digests(&self) -> impl Iterator<Item = &Sha256Digest> {
        self.blobs.iter().map(|entry| &entry.digest)
    }

    /// Wire bytes of every catalogued blob.
    pub fn compressed_size(&self) -> u64 {
        self.blobs.iter().fold(0u64, |total, entry| {
            total.saturating_add(entry.compressed_size)
        })
    }
}
