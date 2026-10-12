//! The exact content one transaction needs, and what it will cost.
//!
//! A closure is computed, not declared. The variant manifest names every digest
//! a variant *could* need and says which component each file belongs to; the
//! user's selection says which of those are actually wanted. The result is the
//! set of blobs that must exist before the machine may be touched, and it is
//! the same set for a fresh install, an update, a modify, and a repair.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use zup_core::Sha256Digest;

use crate::descriptor::{ContentDescriptor, ContentPriority, ContentReason, schedule_order};
use crate::error::AcquireError;
use crate::layout::{CatalogEntry, ContentCatalog};

/// One blob in a closure, with the reason it is there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcquisitionItem {
    pub descriptor: ContentDescriptor,
    pub reason: ContentReason,
}

impl AcquisitionItem {
    /// Pair a descriptor with the reason it is wanted.
    pub fn new(descriptor: ContentDescriptor, reason: ContentReason) -> Self {
        Self { descriptor, reason }
    }

    /// The digest of this item.
    pub fn digest(&self) -> &Sha256Digest {
        self.descriptor.digest()
    }

    /// The scheduler's priority for this item.
    pub const fn priority(&self) -> ContentPriority {
        self.descriptor.priority
    }

    /// Wire bytes this item costs when nothing is cached.
    pub const fn wire_size(&self) -> u64 {
        self.descriptor.compressed_size
    }

    /// Logical bytes this item occupies once installed.
    pub const fn install_size(&self) -> u64 {
        self.descriptor.size
    }

    /// The group a frontend shows instead of the per-item detail.
    pub fn group(&self) -> &'static str {
        self.reason.group()
    }
}

/// A closure, ordered for the scheduler and accounted for the user.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AcquisitionPlan {
    items: Vec<AcquisitionItem>,
}

impl AcquisitionPlan {
    /// An empty plan.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Build a plan from items, deduplicating by digest and ordering for the
    /// scheduler.
    ///
    /// A digest that appears twice keeps its highest priority and its first
    /// reason, because a blob shared by a component and a prerequisite is one
    /// download and the user should be told about it once.
    pub fn build(items: Vec<AcquisitionItem>) -> Result<Self, AcquireError> {
        if items.is_empty() {
            return Err(AcquireError::Descriptor("a closure names no content"));
        }
        if items.len() > crate::MAX_CLOSURE_ITEMS {
            return Err(AcquireError::TooManyItems {
                count: items.len(),
                limit: crate::MAX_CLOSURE_ITEMS,
            });
        }
        let mut by_digest: BTreeMap<Sha256Digest, AcquisitionItem> = BTreeMap::new();
        for item in items {
            item.descriptor
                .validate()
                .map_err(AcquireError::Descriptor)?;
            match by_digest.get_mut(&item.descriptor.digest) {
                Some(existing) => {
                    if item.descriptor.compressed_size != existing.descriptor.compressed_size
                        || item.descriptor.size != existing.descriptor.size
                    {
                        return Err(AcquireError::Descriptor(
                            "two descriptors claim different sizes for one digest",
                        ));
                    }
                    if item.priority() > existing.priority() {
                        existing.descriptor.priority = item.priority();
                    }
                }
                None => {
                    by_digest.insert(item.descriptor.digest, item);
                }
            }
        }
        let mut items: Vec<AcquisitionItem> = by_digest.into_values().collect();
        items.sort_by(|left, right| {
            schedule_order(&left.descriptor, &right.descriptor)
                .then_with(|| left.reason.cmp(&right.reason))
        });
        Ok(Self { items })
    }

    /// Build a plan for one variant's full content set, before any component
    /// selection narrows it.
    pub fn for_variant(
        catalog: &ContentCatalog,
        digests: &[Sha256Digest],
        kind: crate::ContentKind,
    ) -> Result<Self, AcquireError> {
        let mut items = Vec::with_capacity(digests.len());
        for digest in digests {
            let entry: &CatalogEntry =
                catalog.entry(digest).ok_or_else(|| AcquireError::Missing {
                    kind: kind.as_str(),
                    digest: digest.to_hex(),
                })?;
            items.push(AcquisitionItem::new(
                entry.descriptor(kind),
                ContentReason::File { component: None },
            ));
        }
        Self::build(items)
    }

    /// The items, in scheduler order.
    pub fn items(&self) -> &[AcquisitionItem] {
        &self.items
    }

    /// How many distinct blobs the closure names.
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether the closure is empty.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Every digest in the closure, in scheduler order.
    pub fn digests(&self) -> Vec<Sha256Digest> {
        self.items.iter().map(|item| *item.digest()).collect()
    }

    /// Wire bytes for the whole closure, which is what a cold install costs.
    pub fn wire_size(&self) -> u64 {
        self.items
            .iter()
            .fold(0u64, |total, item| total.saturating_add(item.wire_size()))
    }

    /// Logical bytes for the whole closure, which is what it occupies on disk.
    pub fn install_size(&self) -> u64 {
        self.items.iter().fold(0u64, |total, item| {
            total.saturating_add(item.install_size())
        })
    }

    /// Wire bytes per reason group, which is what an estimate shows.
    pub fn wire_size_by_group(&self) -> BTreeMap<&'static str, u64> {
        group_totals(&self.items, AcquisitionItem::wire_size)
    }

    /// Logical bytes per reason group.
    pub fn install_size_by_group(&self) -> BTreeMap<&'static str, u64> {
        group_totals(&self.items, AcquisitionItem::install_size)
    }

    /// Narrow the closure to `keep`.
    ///
    /// Items the selection drops are not fetched, which is the whole point:
    /// another architecture's content, a component the user turned off, and a
    /// prerequisite this machine already satisfies all cost zero bytes.
    pub fn retain(&mut self, keep: &dyn Fn(&AcquisitionItem) -> bool) {
        self.items.retain(keep);
    }

    /// Add content to an existing closure, keeping the strongest priority.
    ///
    /// Used to fold an installed release's content into an update's closure so
    /// a blob both versions need is not fetched twice.
    pub fn extend(&mut self, items: Vec<AcquisitionItem>) -> Result<(), AcquireError> {
        let mut combined = self.items.clone();
        combined.extend(items);
        *self = Self::build(combined)?;
        Ok(())
    }

    /// Describe the closure for a log or a report.
    pub fn summary(&self) -> String {
        let mut groups: Vec<(&'static str, u64, usize)> = self
            .items
            .iter()
            .fold(
                BTreeMap::<&'static str, (u64, usize)>::new(),
                |mut totals, item| {
                    let entry = totals.entry(item.group()).or_insert((0, 0));
                    entry.0 = entry.0.saturating_add(item.wire_size());
                    entry.1 += 1;
                    totals
                },
            )
            .into_iter()
            .map(|(group, (bytes, count))| (group, bytes, count))
            .collect();
        groups.sort_by(|left, right| right.1.cmp(&left.1).then(left.0.cmp(right.0)));
        let rendered: Vec<String> = groups
            .iter()
            .map(|(group, bytes, count)| {
                format!("{group} {} in {count} blobs", format_bytes(*bytes))
            })
            .collect();
        format!(
            "{} in {} blobs ({})",
            format_bytes(self.wire_size()),
            self.items.len(),
            rendered.join(", ")
        )
    }
}

fn group_totals(
    items: &[AcquisitionItem],
    size: fn(&AcquisitionItem) -> u64,
) -> BTreeMap<&'static str, u64> {
    let mut totals: BTreeMap<&'static str, u64> = BTreeMap::new();
    for item in items {
        *totals.entry(item.group()).or_default() += size(item);
    }
    totals
}

/// What an acquisition will cost, before it starts.
///
/// The three numbers a person actually wants are: what has to arrive, what it
/// will occupy, and how much of it the machine already has. The same struct
/// backs the GUI line, the console table, and the JSON event, so the three
/// frontends cannot disagree.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcquisitionEstimate {
    /// Wire bytes that must arrive.
    pub download_bytes: u64,
    /// Logical bytes the closure occupies once installed.
    pub install_bytes: u64,
    /// Wire bytes the machine already has verified.
    pub cached_bytes: u64,
    /// Items already present and valid.
    pub cached_items: usize,
    /// Items the machine must fetch.
    pub missing_items: usize,
}

impl AcquisitionEstimate {
    /// Wire bytes of `items` that `present` reports as already held.
    pub fn measure(
        items: &[AcquisitionItem],
        mut present: impl FnMut(&AcquisitionItem) -> bool,
    ) -> Self {
        let mut estimate = Self {
            install_bytes: 0,
            ..Self::default()
        };
        for item in items {
            estimate.install_bytes = estimate.install_bytes.saturating_add(item.install_size());
            if present(item) {
                estimate.cached_bytes = estimate.cached_bytes.saturating_add(item.wire_size());
                estimate.cached_items += 1;
            } else {
                estimate.download_bytes = estimate.download_bytes.saturating_add(item.wire_size());
                estimate.missing_items += 1;
            }
        }
        estimate
    }

    /// Whether every item is already held.
    pub const fn is_satisfied(&self) -> bool {
        self.missing_items == 0
    }

    /// A three-line estimate a frontend can render unchanged.
    pub fn lines(&self) -> Vec<(String, u64)> {
        vec![
            ("Download".to_owned(), self.download_bytes),
            ("Install".to_owned(), self.install_bytes),
            ("Already cached".to_owned(), self.cached_bytes),
        ]
    }
}

impl fmt::Display for AcquisitionEstimate {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "download {} · install {} · cached {}",
            format_bytes(self.download_bytes),
            format_bytes(self.install_bytes),
            format_bytes(self.cached_bytes)
        )
    }
}

/// Render a byte count the way every zup frontend renders one.
pub use zup_core::format_bytes;
