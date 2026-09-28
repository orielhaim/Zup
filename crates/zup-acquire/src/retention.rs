//! What the cache keeps after a transaction commits, and why.
//!
//! This is a policy, and the whole point of writing it down is that "the cache
//! happens to have whatever was downloaded" is not one. There are exactly two
//! things a machine can want from its content cache afterwards:
//!
//! - **repair is online.** The installed files are the copy that matters. A
//!   damaged file is re-fetched from the release graph, which costs one object
//!   and needs no network policy at install time.
//! - **repair is offline.** The machine may have to restore itself with no
//!   network, so the closure that produced the installation is retained.
//!
//! Everything else is a cache. The default is online repair, because retaining
//! the closure means roughly doubling an application's disk usage to solve a
//! problem the graph already solves for a few hundred kilobytes.
//!
//! # What is never retained
//!
//! Retained objects are **not copied**. A blob marked for retention is the same
//! object in the same place that the acquisition already published it; retention
//! only decides whether a sweep may delete it. A policy that "kept" a blob by
//! duplicating it would cost disk and gain nothing, because the identity is the
//! path.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use zup_core::Sha256Digest;

use crate::cache::{CachePolicy, ContentCache};
use crate::error::CacheError;

/// Why the cache holds what it holds.
///
/// This is the stateful form of [`CachePolicy`]: the policy says what should be
/// kept, and this records what was, so a sweep can tell "old but required" from
/// "old and required by nothing".
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetentionState {
    pub schema: u32,
    /// The policy the installation was created under.
    pub policy: String,
    /// The release that produced the installation, so a sweep can name it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release: Option<Sha256Digest>,
    /// Digests the installation is still using.
    ///
    /// This is the closure, not a superset: a blob no longer referenced by the
    /// installed release is not retained, because the release that contains it
    /// is not the one this machine runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned: Option<BTreeSet<Sha256Digest>>,
}

/// Current retention state schema.
pub const RETENTION_SCHEMA: u32 = 1;

/// A file name of the retention record, inside the cache root.
pub const RETENTION_FILE: &str = "retention.json";

/// The largest retention record this build will read.
pub const MAX_RETENTION_BYTES: u64 = 64 * 1024 * 1024;

impl RetentionState {
    /// Record the closure an installation uses.
    pub fn record(
        policy: CachePolicy,
        release: Sha256Digest,
        pinned: BTreeSet<Sha256Digest>,
    ) -> Self {
        Self {
            schema: RETENTION_SCHEMA,
            policy: policy.as_str().to_owned(),
            release: Some(release),
            pinned: Some(pinned),
        }
    }

    /// Whether this state retains payload.
    pub fn retains_payload(&self) -> bool {
        CachePolicy::from_name(&self.policy)
            .map(|policy| policy.retains_payload())
            .unwrap_or(false)
    }

    /// Whether the policy is offline repair, which is the only reason a sweep
    /// may keep a large object forever.
    pub fn is_offline_repair(&self) -> bool {
        matches!(
            CachePolicy::from_name(&self.policy),
            Some(CachePolicy::Keep)
        )
    }

    /// The digests a sweep must not delete.
    pub fn pinned(&self) -> BTreeSet<Sha256Digest> {
        self.pinned.clone().unwrap_or_default()
    }

    /// How long the policy exempts the objects it retains.
    ///
    /// `None` for a policy that retains nothing, and for a name this build does
    /// not recognize - which `validate` already refuses, so the fallback only
    /// matters for a record read from somewhere hostile.
    pub fn window(&self) -> Option<Duration> {
        CachePolicy::from_name(&self.policy).and_then(CachePolicy::auto_retention)
    }

    /// Reject a record a hostile directory could use to make a sweep walk
    /// somewhere it should not.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.schema != RETENTION_SCHEMA {
            return Err("unsupported retention state schema");
        }
        if CachePolicy::from_name(&self.policy).is_none() {
            return Err("unknown cache policy");
        }
        if let Some(pinned) = &self.pinned
            && pinned.len() > crate::MAX_CLOSURE_ITEMS
        {
            return Err("a retention record names more objects than this build accepts");
        }
        Ok(())
    }
}

/// What a sweep would do, and what it did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetentionReport {
    /// Objects examined.
    pub examined: u64,
    /// Objects kept because the installation references them.
    pub pinned: u64,
    /// Objects kept because they are inside the retention window.
    pub fresh: u64,
    /// Objects deleted.
    pub removed: u64,
    /// Wire bytes deleted.
    pub freed: u64,
}

impl RetentionReport {
    /// Whether the sweep changed anything.
    pub const fn is_empty(&self) -> bool {
        self.removed == 0
    }
}

/// A cache's retention record.
pub fn retention_path(cache_root: &Path) -> PathBuf {
    cache_root.join(RETENTION_FILE)
}

/// Read a cache's retention record, or a default when it has none.
///
/// A missing or unreadable record is not an error: a machine that never
/// recorded one is a machine under the default policy, and refusing to sweep
/// because of that would leak disk forever.
pub fn read_retention(cache_root: &Path) -> RetentionState {
    let fallback = RetentionState {
        schema: RETENTION_SCHEMA,
        policy: CachePolicy::default().as_str().to_owned(),
        release: None,
        pinned: None,
    };
    let Ok(bytes) = std::fs::read(retention_path(cache_root)) else {
        return fallback;
    };
    if bytes.len() as u64 > MAX_RETENTION_BYTES {
        return fallback;
    }
    serde_json::from_slice::<RetentionState>(&bytes)
        .ok()
        .filter(|state| state.validate().is_ok())
        .unwrap_or(fallback)
}

/// Write a cache's retention record durably enough to survive a crash.
///
/// A retention record that fails to write is a performance outcome, never a
/// correctness one: the worst case is a sweep that keeps more than it should.
pub fn write_retention(cache_root: &Path, state: &RetentionState) -> Result<(), CacheError> {
    state
        .validate()
        .map_err(|detail| CacheError::invalid(retention_path(cache_root), detail))?;
    let bytes = serde_json::to_vec(state)
        .map_err(|error| CacheError::invalid(retention_path(cache_root), error.to_string()))?;
    let path = retention_path(cache_root);
    let temporary = path.with_extension("json.partial");
    std::fs::create_dir_all(cache_root).map_err(|error| CacheError::io(&path, error))?;
    {
        use std::io::Write;
        let mut file =
            std::fs::File::create(&temporary).map_err(|error| CacheError::io(&path, error))?;
        file.write_all(&bytes)
            .and_then(|()| file.sync_all())
            .map_err(|error| CacheError::io(&path, error))?;
    }
    std::fs::rename(&temporary, &path).map_err(|error| CacheError::io(&path, error))?;
    Ok(())
}

/// Apply a policy to what the machine holds.
///
/// Two windows decide whether an object survives, and they answer different
/// questions.
///
/// `window` is the policy's own: under `auto` an object is exempt for seven days
/// and under `keep` forever, which is how "retain what this installation uses"
/// becomes a bounded promise rather than an open-ended one. `grace` is the
/// operational one: nothing is collectable inside it, because a second operation
/// running right now may hold a reference no sweep can see. So an object is
/// exempt when the policy retains it *and* it is inside the window, or when it is
/// merely inside the grace.
pub fn sweep(cache: &ContentCache, state: &RetentionState, grace: Duration) -> RetentionReport {
    let mut report = RetentionReport::default();
    let pinned = state.pinned();
    let window = state.window().unwrap_or(Duration::ZERO);
    let now = SystemTime::now();
    for object in cache.objects() {
        report.examined += 1;
        let age = object
            .modified
            .and_then(|modified| now.duration_since(modified).ok());
        let fresh = age.is_some_and(|age| age < grace);
        if fresh {
            report.fresh += 1;
            continue;
        }
        if state.retains_payload()
            && pinned.contains(&object.digest)
            && age.is_some_and(|age| age < window)
        {
            report.pinned += 1;
            continue;
        }
        if let Ok(freed) = cache.remove(&object.digest) {
            report.removed += 1;
            report.freed = report.freed.saturating_add(freed);
        }
    }
    report
}

/// Apply a policy to a cache root without holding the cache open.
///
/// Used by maintenance, which runs between operations and should not have to
/// construct a session to make a decision about disk.
pub fn sweep_root(cache_root: &Path, grace: Duration) -> Result<RetentionReport, CacheError> {
    let cache = ContentCache::open(cache_root, CachePolicy::Auto)?;
    Ok(sweep(&cache, &read_retention(cache_root), grace))
}

/// How long an unreferenced object is kept under the default grace.
///
/// A day is long enough that an operator running a second operation ten minutes
/// after an install never sees content disappear, and short enough that the
/// previous release's payload does not sit around for a month.
pub const DEFAULT_GRACE: Duration = Duration::from_secs(24 * 60 * 60);
