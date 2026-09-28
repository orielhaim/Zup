//! The small authenticated document that names a transport package.
//!
//! # A locator, not a trust anchor
//!
//! This document says where a variant's content is: the package's name, its
//! digest, its length, and the pieces it was split into. It does **not** decide
//! what is installed.
//!
//! That distinction is the reason a GitHub-hosted release can be used for
//! content without a second trust mechanism bolted on. Every blob inside the
//! package is verified against a digest from the release's own content catalog
//! before it is published into the cache, so a package descriptor that has been
//! tampered with can cause a wrong-blob attempt and a wasted download - both
//! caught - and cannot cause unverified content to be installed.
//!
//! Publishing it as its own document is still worth doing, for one reason: when
//! the project *does* sign its release (through TUF), the descriptor is one more
//! small, named thing to put in the signed targets, and sharding cannot work
//! without somewhere authenticated to record the shard map.

use serde::{Deserialize, Serialize};
use zup_core::Sha256Digest;
use zup_publish::DocumentKind;

use crate::error::DistributeError;

/// Version of the package descriptor shape.
pub const DESCRIPTOR_SCHEMA: u32 = 1;

/// A file's identity, as a descriptor records it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileRef {
    pub name: String,
    pub digest: Sha256Digest,
    pub size: u64,
}

/// One piece of a package.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShardRef {
    /// The piece's index, in order.
    pub index: u32,
    /// The asset name.
    pub name: String,
    /// The piece's own digest.
    pub digest: Sha256Digest,
    /// The piece's own length.
    pub size: u64,
    /// Where this piece starts within the logical package.
    pub start: u64,
}

/// What a release says about one variant's transport package.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackageDescriptor {
    pub schema: u32,
    /// The variant this package carries.
    pub variant: String,
    /// That variant's canonical target triple.
    pub target: String,
    /// The application the release is for.
    pub application: String,
    /// The whole logical package.
    pub package: FileRef,
    /// Its pieces, in order. One entry means it is not sharded.
    pub shards: Vec<ShardRef>,
    /// How many blobs the package holds.
    pub blob_count: u64,
    /// The sum of the blobs' logical sizes.
    pub logical_size: u64,
}

impl PackageDescriptor {
    /// Whether the package was split.
    pub fn is_sharded(&self) -> bool {
        self.shards.len() > 1
    }

    /// The release-asset name of this descriptor.
    pub fn document_name(&self) -> String {
        zup_publish::document_name(&DocumentKind::Package {
            variant: &self.variant,
        })
    }

    /// Read a descriptor, bounded, and check its own claims.
    pub fn parse(bytes: &[u8], expected: Option<Sha256Digest>) -> Result<Self, DistributeError> {
        if let Some(expected) = expected {
            let found = zup_core::hash_bytes(bytes);
            if found != expected {
                return Err(DistributeError::Untrusted {
                    name: "package descriptor".to_owned(),
                    reason: format!(
                        "the release named sha256:{} and the host served sha256:{}",
                        expected.to_hex(),
                        found.to_hex()
                    ),
                });
            }
        }
        let descriptor: Self =
            serde_json::from_slice(bytes).map_err(|error| DistributeError::Untrusted {
                name: "package descriptor".to_owned(),
                reason: error.to_string(),
            })?;
        descriptor.validate()?;
        Ok(descriptor)
    }

    /// The descriptor as JSON.
    pub fn encode(&self) -> Result<Vec<u8>, DistributeError> {
        serde_json::to_vec_pretty(self).map_err(|error| DistributeError::Untrusted {
            name: "package descriptor".to_owned(),
            reason: error.to_string(),
        })
    }

    /// Check that the descriptor is internally consistent.
    ///
    /// The pieces have to tile the whole package with no gap and no overlap: a
    /// hole means a blob the descriptor claims exists is at a byte nobody
    /// serves, and an overlap means two pieces claim the same bytes, which is
    /// the shape a "second one wins" bug takes.
    pub fn validate(&self) -> Result<(), DistributeError> {
        if self.schema != DESCRIPTOR_SCHEMA {
            return Err(DistributeError::Untrusted {
                name: "package descriptor".to_owned(),
                reason: format!(
                    "it is schema {}; this build reads schema {DESCRIPTOR_SCHEMA}",
                    self.schema
                ),
            });
        }
        if self.variant.is_empty() || self.target.is_empty() {
            return Err(DistributeError::Untrusted {
                name: "package descriptor".to_owned(),
                reason: "it names no variant".to_owned(),
            });
        }
        if self.shards.is_empty() {
            return Err(DistributeError::Untrusted {
                name: "package descriptor".to_owned(),
                reason: "it names no pieces".to_owned(),
            });
        }
        let mut cursor = 0u64;
        for (index, shard) in self.shards.iter().enumerate() {
            if usize::try_from(shard.index).ok() != Some(index) {
                return Err(DistributeError::Untrusted {
                    name: "package descriptor".to_owned(),
                    reason: format!("piece {index} is numbered {}", shard.index),
                });
            }
            if shard.start != cursor {
                return Err(DistributeError::Untrusted {
                    name: "package descriptor".to_owned(),
                    reason: format!(
                        "piece {} starts at {} but piece {index} ends at {cursor}",
                        shard.index, shard.start
                    ),
                });
            }
            zup_publish::check_asset_name(&shard.name).map_err(|reason| {
                DistributeError::Untrusted {
                    name: "package descriptor".to_owned(),
                    reason,
                }
            })?;
            cursor = cursor.saturating_add(shard.size);
        }
        if cursor != self.package.size {
            return Err(DistributeError::Untrusted {
                name: "package descriptor".to_owned(),
                reason: format!(
                    "its pieces total {cursor} bytes but the package is {} bytes",
                    self.package.size
                ),
            });
        }
        if self.package.name.is_empty() {
            return Err(DistributeError::Untrusted {
                name: "package descriptor".to_owned(),
                reason: "it names no package".to_owned(),
            });
        }
        Ok(())
    }

    /// The bytes the pieces add up to, and the whole package's digest has to
    /// be the digest of exactly those bytes in that order.
    pub fn logical_digest_claim(&self) -> Option<&Sha256Digest> {
        Some(&self.package.digest)
    }
}
