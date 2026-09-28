//! What a GitHub distribution cost.
//!
//! The numbers that decide whether a range-addressable package is worth having
//! are not assumptions. They are requests and bytes, counted at the source, so
//! that a report can say "GitHub served 1.2 MiB across 8 requests" instead of
//! "GitHub-only distribution is efficient".
//!
//! Counted separately, because they are the two different answers:
//!
//! - **ranged** transfers, where the host honoured a `Range` and the source got
//!   only the frame it needed.
//! - **fallbacks**, where the host sent the whole piece anyway, or refused a
//!   range it had already started answering.
//!
//! A fallback is not a failure. It is the documented behaviour of a host that
//! does not do ranges, and it is correct - it is only slower, and the point of
//! counting it is that "slower" should be a number rather than an impression.

use std::sync::atomic::{AtomicU64, Ordering};

/// The counters.
#[derive(Debug, Default)]
pub struct Metrics {
    /// Transfers that used a byte range.
    pub ranged_transfers: AtomicU64,
    /// Transfers that fell back to a whole piece.
    pub fallback_transfers: AtomicU64,
    /// Transfers refused because a `Content-Range` did not line up.
    pub refused_transfers: AtomicU64,
    /// Transfers that continued a partial left by an interrupted attempt.
    pub resumed_transfers: AtomicU64,
    /// HTTP requests issued.
    pub requests: AtomicU64,
    /// Bytes received over the wire.
    pub wire_bytes: AtomicU64,
    /// Bytes the release actually needed, had everything been ranged.
    pub needed_bytes: AtomicU64,
}

impl Metrics {
    /// All counters at zero.
    pub fn new() -> Self {
        Self::default()
    }

    /// Count a transfer that used a range.
    pub fn ranged(&self, _piece: usize) {
        self.ranged_transfers.fetch_add(1, Ordering::Relaxed);
    }

    /// Count bytes the release actually needed, had everything been ranged.
    ///
    /// Separate from [`Self::request`] because on the fallback path the two
    /// differ by the whole piece, and a report that showed only the wire cost
    /// would not say what the optimisation would have been worth.
    pub fn needed(&self, bytes: u64) {
        self.needed_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Count a transfer that continued rather than restarted.
    pub fn resumed(&self) {
        self.resumed_transfers.fetch_add(1, Ordering::Relaxed);
    }

    /// Count a transfer that had to read the whole piece.
    pub fn fallback(&self) {
        self.fallback_transfers.fetch_add(1, Ordering::Relaxed);
    }

    /// Count a transfer whose `Content-Range` did not agree with the request.
    pub fn refused(&self, _declared: String) {
        self.refused_transfers.fetch_add(1, Ordering::Relaxed);
    }

    /// Count a request and its bytes.
    pub fn request(&self, bytes: u64) {
        self.requests.fetch_add(1, Ordering::Relaxed);
        self.wire_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    /// A snapshot, for a report.
    pub fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            ranged_transfers: self.ranged_transfers.load(Ordering::Relaxed),
            fallback_transfers: self.fallback_transfers.load(Ordering::Relaxed),
            refused_transfers: self.refused_transfers.load(Ordering::Relaxed),
            resumed_transfers: self.resumed_transfers.load(Ordering::Relaxed),
            requests: self.requests.load(Ordering::Relaxed),
            wire_bytes: self.wire_bytes.load(Ordering::Relaxed),
            needed_bytes: self.needed_bytes.load(Ordering::Relaxed),
        }
    }
}

/// A point-in-time reading of the counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub struct MetricsSnapshot {
    pub ranged_transfers: u64,
    pub fallback_transfers: u64,
    pub refused_transfers: u64,
    pub resumed_transfers: u64,
    pub requests: u64,
    pub wire_bytes: u64,
    pub needed_bytes: u64,
}

impl MetricsSnapshot {
    /// How many bytes the range optimisation saved.
    ///
    /// Zero when the host does not do ranges, which is the honest answer rather
    /// than a rounding of "no saving".
    pub fn saved(&self) -> u64 {
        self.wire_bytes.saturating_sub(self.needed_bytes)
    }

    /// Whether every transfer avoided reading a whole piece.
    pub fn is_fully_ranged(&self) -> bool {
        self.fallback_transfers == 0 && self.refused_transfers == 0
    }
}
