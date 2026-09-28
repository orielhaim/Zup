//! The content-addressed store table.
//!
//! The table locates every unique blob by digest. It is a separate
//! content-addressed document rather than part of the index, because it grows
//! with the number of distinct blobs while the index stays small enough to read
//! and validate before anything else is trusted.
//!
//! Blobs are packed into a small number of fixed-capacity *segments* rather than
//! addressed one by one. A platform container writes a whole file per opaque
//! region, so segments are what make a multi-gigabyte store addressable at all;
//! a descriptor still names exactly one blob, and the table only says where that
//! blob's compressed bytes live.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use zup_core::Sha256Digest;

use crate::error::ArtifactError;
use crate::media_type::{MAX_BLOBS, MAX_SEGMENTS};

/// Largest compressed size of one segment.
///
/// A segment is written as one container region, so its size is bounded by what
/// a 32-bit length can address. Blobs larger than this cannot be segmented and
/// are refused rather than silently split.
pub const MAX_SEGMENT_BYTES: u64 = 3 * 1024 * 1024 * 1024;

/// Where one blob's compressed bytes live, and how large they are.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlobEntry {
    pub digest: Sha256Digest,
    /// Index of the segment holding the compressed bytes.
    pub segment: u16,
    /// Offset of the compressed bytes within that segment.
    pub offset: u64,
    pub compressed_size: u64,
    /// Uncompressed size, which is the value a selection plan budgets with.
    pub size: u64,
}

/// The canonical locator for every unique blob in an artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlobTable {
    pub schema: u32,
    /// Number of segments the store uses. A thin artifact carries the table
    /// with no bytes behind it, so this is `0`.
    pub segments: u16,
    /// Entries sorted by digest, with no duplicates.
    pub blobs: Vec<BlobEntry>,
}

/// Current blob table schema.
pub const BLOB_TABLE_SCHEMA: u32 = 1;

impl BlobTable {
    /// Build a table from unsorted entries, packing them into segments.
    ///
    /// Packing is deterministic: entries are visited in ascending digest order
    /// and a new segment starts only when the next blob would not fit, so the
    /// same content always produces the same table and the same byte layout.
    ///
    /// Repeats are collapsed, because a table names every *unique* blob and a
    /// duplicate is one object stored twice — and because `validate` refuses a
    /// table that names a digest twice, so a `pack` that kept them would hand
    /// back something its own `parse` rejects. Two entries for one digest that
    /// disagree about size are a different mistake and are refused rather than
    /// silently resolved: which of the two a reader should fetch is not a question
    /// this function can answer.
    pub fn pack(mut entries: Vec<BlobEntry>) -> Result<Self, ArtifactError> {
        entries.sort_by_key(|entry| entry.digest);
        entries.dedup_by(|left, right| {
            left.digest == right.digest
                && left.size == right.size
                && left.compressed_size == right.compressed_size
        });
        for pair in entries.windows(2) {
            if pair[0].digest == pair[1].digest {
                return Err(ArtifactError::Invalid);
            }
        }
        if entries.len() > MAX_BLOBS {
            return Err(ArtifactError::TooManyBlobs {
                count: entries.len(),
                limit: MAX_BLOBS,
            });
        }
        let mut segment = 0u16;
        let mut cursor = 0u64;
        for (index, entry) in entries.iter_mut().enumerate() {
            if entry.compressed_size == 0 {
                return Err(ArtifactError::Invalid);
            }
            if entry.compressed_size > MAX_SEGMENT_BYTES
                || (index > 0 && cursor + entry.compressed_size > MAX_SEGMENT_BYTES)
            {
                segment = segment.checked_add(1).ok_or(ArtifactError::Invalid)?;
                if segment >= MAX_SEGMENTS {
                    return Err(ArtifactError::TooManyBlobs {
                        count: MAX_BLOBS,
                        limit: MAX_BLOBS,
                    });
                }
                cursor = 0;
            }
            entry.segment = segment;
            entry.offset = cursor;
            cursor += entry.compressed_size;
        }
        let segments = match entries.first() {
            Some(_) => segment + 1,
            None => 0,
        };
        Ok(Self {
            schema: BLOB_TABLE_SCHEMA,
            segments,
            blobs: entries,
        })
    }

    /// A table with descriptors but no bytes, for a thin artifact.
    pub fn detached(entries: Vec<BlobEntry>) -> Result<Self, ArtifactError> {
        let mut table = Self::pack(entries)?;
        table.segments = 0;
        Ok(table)
    }

    /// Serialize canonically.
    pub fn encode(&self) -> Result<Vec<u8>, ArtifactError> {
        crate::descriptor::to_canonical_json(self)
    }

    /// Parse and validate a table, including the sortedness and contiguity a
    /// reader needs before it can seek anywhere.
    pub fn parse(bytes: &[u8]) -> Result<Self, ArtifactError> {
        let table: Self = crate::descriptor::from_bounded_json(
            bytes,
            crate::media_type::MAX_BLOB_TABLE_BYTES,
            "blob table",
        )?;
        table.validate()?;
        Ok(table)
    }

    /// Reject a table whose layout a reader could not trust.
    ///
    /// A detached table names descriptors but no bytes, so it has no segments to
    /// be contiguous within; everything else about it is still checked.
    pub fn validate(&self) -> Result<(), ArtifactError> {
        if self.schema != BLOB_TABLE_SCHEMA
            || self.blobs.len() > MAX_BLOBS
            || self.segments > MAX_SEGMENTS
            || (self.blobs.is_empty() && self.segments != 0)
        {
            return Err(ArtifactError::Invalid);
        }
        let mut cursors = vec![None::<u64>; usize::from(self.segments)];
        for (index, entry) in self.blobs.iter().enumerate() {
            if entry.compressed_size == 0
                || entry.compressed_size > MAX_SEGMENT_BYTES
                || (index > 0 && self.blobs[index - 1].digest >= entry.digest)
            {
                return Err(ArtifactError::Invalid);
            }
            if self.segments == 0 {
                continue;
            }
            let Some(cursor) = cursors.get_mut(usize::from(entry.segment)) else {
                return Err(ArtifactError::Invalid);
            };
            if entry.offset != cursor.unwrap_or(0) {
                return Err(ArtifactError::Invalid);
            }
            *cursor = Some(entry.offset + entry.compressed_size);
        }
        if cursors.iter().any(Option::is_none) {
            return Err(ArtifactError::Invalid);
        }
        Ok(())
    }

    /// The entry for `digest`.
    pub fn entry(&self, digest: &Sha256Digest) -> Option<&BlobEntry> {
        self.blobs
            .binary_search_by(|entry| entry.digest.cmp(digest))
            .ok()
            .map(|index| &self.blobs[index])
    }

    /// Every entry, in ascending digest order.
    pub fn entries(&self) -> std::slice::Iter<'_, BlobEntry> {
        self.blobs.iter()
    }

    /// Total compressed bytes across every segment.
    pub fn stored_size(&self) -> u64 {
        self.blobs
            .iter()
            .try_fold(0u64, |sum, entry| sum.checked_add(entry.compressed_size))
            .unwrap_or(u64::MAX)
    }

    /// The compressed size of one segment, which is how long it is on disk.
    pub fn segment_size(&self, segment: u16) -> u64 {
        self.blobs
            .iter()
            .filter(|entry| entry.segment == segment)
            .try_fold(0u64, |sum, entry| sum.checked_add(entry.compressed_size))
            .unwrap_or(0)
    }

    /// Total uncompressed bytes of every unique blob.
    pub fn logical_size(&self) -> u64 {
        self.blobs
            .iter()
            .try_fold(0u64, |sum, entry| sum.checked_add(entry.size))
            .unwrap_or(u64::MAX)
    }

    /// The digests in `digests` that this table does not carry bytes for.
    pub fn missing(&self, digests: &[Sha256Digest]) -> Vec<Sha256Digest> {
        digests
            .iter()
            .copied()
            .filter(|digest| self.entry(digest).is_none())
            .collect()
    }

    /// The entries for `digests`, keyed by digest, rejecting any digest the
    /// table does not carry.
    pub fn select(
        &self,
        digests: &[Sha256Digest],
    ) -> Result<BTreeMap<Sha256Digest, BlobEntry>, ArtifactError> {
        let mut selected = BTreeMap::new();
        for digest in digests {
            let entry = self.entry(digest).ok_or_else(|| ArtifactError::Missing {
                media_type: crate::media_type::MediaType::BLOB.label(),
                digest: digest.to_hex(),
            })?;
            selected.insert(*digest, *entry);
        }
        Ok(selected)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(seed: u8, size: u64) -> BlobEntry {
        BlobEntry {
            digest: Sha256Digest::from_bytes([seed; 32]),
            segment: 0,
            offset: 0,
            compressed_size: size,
            size: size * 2,
        }
    }

    #[test]
    fn packing_is_deterministic_and_input_order_independent() {
        let entries = vec![entry(3, 10), entry(1, 20), entry(2, 5)];
        let forward = BlobTable::pack(entries.clone()).unwrap();
        let reverse = BlobTable::pack(entries.into_iter().rev().collect()).unwrap();
        assert_eq!(forward, reverse);
        assert_eq!(
            forward
                .blobs
                .iter()
                .map(|entry| (entry.digest.as_bytes()[0], entry.offset))
                .collect::<Vec<_>>(),
            vec![(1, 0), (2, 20), (3, 25)]
        );
        forward.validate().unwrap();
    }

    #[test]
    fn validation_rejects_unsorted_and_gapped_tables() {
        let mut table = BlobTable::pack(vec![entry(1, 4), entry(2, 4)]).unwrap();
        table.blobs.swap(0, 1);
        assert!(matches!(table.validate(), Err(ArtifactError::Invalid)));

        let mut table = BlobTable::pack(vec![entry(1, 4), entry(2, 4)]).unwrap();
        table.blobs[1].offset = 9;
        assert!(matches!(table.validate(), Err(ArtifactError::Invalid)));

        let mut table = BlobTable::pack(vec![entry(1, 4), entry(2, 4)]).unwrap();
        table.segments = 3;
        assert!(matches!(table.validate(), Err(ArtifactError::Invalid)));
    }

    #[test]
    fn a_detached_table_validates_without_bytes() {
        let table = BlobTable::detached(vec![entry(1, 4), entry(2, 4)]).unwrap();
        assert_eq!(table.segments, 0);
        table.validate().unwrap();
        assert_eq!(table.stored_size(), 8);
    }

    /// A payload with two identical files produces two plan entries with one
    /// digest, and `pack` used to keep both — handing back a table its own
    /// `parse` refuses. A pack that cannot be re-read is not a pack.
    #[test]
    fn one_digest_stored_twice_is_one_entry() {
        let table = BlobTable::pack(vec![entry(1, 4), entry(1, 4), entry(2, 4)]).unwrap();
        assert_eq!(table.blobs.len(), 2);
        table.validate().unwrap();
        let encoded = table.encode().unwrap();
        assert_eq!(BlobTable::parse(&encoded).unwrap(), table);
    }

    /// Two entries for one digest that disagree are a different mistake, and
    /// picking one of them would be guessing which bytes a reader should fetch.
    #[test]
    fn one_digest_described_two_ways_is_refused() {
        let mut disagreeing = entry(1, 4);
        disagreeing.size = 99;
        assert!(matches!(
            BlobTable::pack(vec![entry(1, 4), disagreeing]),
            Err(ArtifactError::Invalid)
        ));
    }

    #[test]
    fn select_reports_every_missing_digest() {
        let table = BlobTable::pack(vec![entry(1, 4)]).unwrap();
        let wanted = [
            Sha256Digest::from_bytes([1; 32]),
            Sha256Digest::from_bytes([9; 32]),
        ];
        assert_eq!(
            table.missing(&wanted),
            vec![Sha256Digest::from_bytes([9; 32])]
        );
        assert!(table.select(&wanted).is_err());
        assert!(table.select(&wanted[..1]).is_ok());
    }
}
