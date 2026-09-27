//! What an acquisition reports while it runs.
//!
//! A downloader that narrates every chunk is unusable in a window and
//! unusable in a log. Everything here is an aggregate: bytes, counts, and
//! state, sampled on an interval. A frontend can render it as a bar, a table,
//! or a JSONL stream and none of the three sees a per-chunk event.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use zup_core::Sha256Digest;

use crate::plan::AcquisitionEstimate;

/// Where an acquisition is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcquisitionPhase {
    /// Working out which blobs are missing.
    Planning,
    /// Moving bytes.
    Downloading,
    /// Everything required is present and verified; staging may begin.
    Verified,
    /// The transaction is staging verified content.
    Staging,
    /// The closure is satisfied and staged. The barrier has been crossed and
    /// the machine may now be mutated.
    Complete,
    /// Stopped by the caller.
    Cancelled,
    /// Stopped by a failure. Nothing was mutated.
    Failed,
}

impl AcquisitionPhase {
    /// Stable name for a `phase` event.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Planning => "planning",
            Self::Downloading => "downloading",
            Self::Verified => "verified",
            Self::Staging => "staging",
            Self::Complete => "complete",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
        }
    }
}

/// One aggregate sample of an acquisition in flight.
///
/// This is the only shape progress is reported in. It is deliberately countable
/// rather than descriptive: everything on it is a number a person can watch
/// move, and nothing on it describes an individual transfer.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct AcquisitionProgress {
    pub phase: AcquisitionPhase,
    /// Wire bytes present and verified.
    pub completed_bytes: u64,
    /// Wire bytes the closure needs.
    pub total_bytes: u64,
    /// Wire bytes that were already held when the session started.
    pub cached_bytes: u64,
    /// Items already held when the session started.
    pub cached_items: u64,
    /// Transfers running right now.
    pub active_transfers: u64,
    /// Items finished, from any source.
    pub completed_items: u64,
    /// Items the closure names.
    pub total_items: u64,
    /// Wall-clock time spent transferring.
    pub elapsed: Duration,
    /// Bytes per second over the whole session, or zero before the first byte.
    pub throughput: u64,
    /// Seconds remaining at the current throughput, or `None` when it cannot be
    /// known yet.
    pub eta: Option<Duration>,
    /// How many retry attempts are outstanding across all transfers.
    pub retries: u64,
}

impl AcquisitionProgress {
    /// Completion as a percentage of the closure's wire bytes.
    pub fn percent(&self) -> Option<u32> {
        if self.total_bytes == 0 {
            return None;
        }
        Some((self.completed_bytes.saturating_mul(100) / self.total_bytes).min(100) as u32)
    }

    /// Whether every byte the closure names is present.
    pub const fn is_complete(&self) -> bool {
        self.total_bytes != 0 && self.completed_bytes >= self.total_bytes
    }

    /// The line a GUI or a console renders under a progress bar.
    pub fn detail(&self) -> String {
        let mut line = format!(
            "{} of {} · {}",
            crate::plan::format_bytes(self.completed_bytes),
            crate::plan::format_bytes(self.total_bytes),
            format_rate(self.throughput)
        );
        if let Some(eta) = self.eta {
            line.push_str(" · ");
            line.push_str(&format_eta(eta));
        }
        line
    }

    /// Recompute the derived fields from the counters that changed.
    pub fn recompute(&mut self) {
        self.throughput = if self.elapsed.as_secs_f64() > 0.05 {
            (self.completed_bytes as f64 / self.elapsed.as_secs_f64()) as u64
        } else {
            0
        };
        self.eta = match (self.total_bytes, self.throughput) {
            (total, rate) if total > self.completed_bytes && rate > 0 => {
                Some(Duration::from_secs((total - self.completed_bytes) / rate))
            }
            _ => None,
        };
    }
}

/// Something worth telling the caller about, as a closed set.
///
/// The set is closed on purpose: a frontend switches on it exhaustively, so
/// adding an event is a visible change rather than something a UI silently
/// ignores.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "event")]
pub enum AcquisitionEvent {
    /// A release graph was authenticated and is now the one being installed.
    ReleaseResolved {
        app_id: String,
        channel: String,
        version: String,
        release_digest: Sha256Digest,
    },
    /// Exactly one variant was chosen for this host.
    VariantSelected {
        variant: String,
        target: String,
        compatibility: String,
    },
    /// The closure is known and the estimate is fixed.
    AcquisitionStarted {
        variant: String,
        items: u64,
        estimate: AcquisitionEstimate,
    },
    /// A sampled progress point.
    DownloadProgress { progress: Box<AcquisitionProgress> },
    /// A blob was already present and valid, so it cost nothing.
    CacheHit {
        kind: &'static str,
        digest: String,
        wire_bytes: u64,
    },
    /// A transfer is being retried after a recoverable failure.
    Retrying {
        kind: &'static str,
        digest: String,
        attempt: u32,
        delay_ms: u64,
        reason: String,
    },
    /// Every required resource is present and verified. The barrier is now
    /// open: application-owned mutation may begin.
    AcquisitionComplete {
        items: u64,
        bytes: u64,
        elapsed_ms: u64,
    },
    /// Verified content is staged and the transaction may proceed.
    StagingComplete { staged: u64, bytes: u64 },
    /// The caller cancelled before the barrier.
    Cancelled {
        completed_items: u64,
        total_items: u64,
    },
    /// Acquisition failed before the barrier. The machine is unchanged.
    Failed {
        kind: &'static str,
        digest: Option<String>,
        message: String,
        /// One line per source that was tried and why it did not work.
        ///
        /// The top-level message says what could not be produced; these say what
        /// to do about it. "no source could acquire X" is not actionable, and
        /// "the CDN reset the connection twice, then the mirror does not carry
        /// it" is.
        reasons: Vec<String>,
        /// Always true. Present so a consumer can assert the guarantee rather
        /// than infer it.
        machine_unchanged: bool,
    },
}

impl AcquisitionEvent {
    /// The stable event name, which is the JSONL `type`.
    pub const fn name(&self) -> &'static str {
        match self {
            Self::ReleaseResolved { .. } => "release_resolved",
            Self::VariantSelected { .. } => "variant_selected",
            Self::AcquisitionStarted { .. } => "acquisition_started",
            Self::DownloadProgress { .. } => "download_progress",
            Self::CacheHit { .. } => "cache_hit",
            Self::Retrying { .. } => "retrying",
            Self::AcquisitionComplete { .. } => "acquisition_complete",
            Self::StagingComplete { .. } => "staging_complete",
            Self::Cancelled { .. } => "cancelled",
            Self::Failed { .. } => "failed",
        }
    }

    /// Every reason a failure carries, rendered for a person.
    ///
    /// A transport detail never contains a URL with credentials: a source
    /// reports the origin by name and the reason by class, and that is all that
    /// reaches a log.
    pub fn reasons(&self) -> &[String] {
        match self {
            Self::Failed { reasons, .. } => reasons,
            _ => &[],
        }
    }
}

fn format_rate(bytes_per_second: u64) -> String {
    if bytes_per_second == 0 {
        return "starting…".to_owned();
    }
    format!("{}/s", crate::plan::format_bytes(bytes_per_second))
}

fn format_eta(eta: Duration) -> String {
    let seconds = eta.as_secs();
    if seconds < 60 {
        return format!("{seconds}s left");
    }
    let minutes = seconds / 60;
    if minutes < 60 {
        return format!("{minutes}m left");
    }
    format!("{}h left", minutes / 60)
}
