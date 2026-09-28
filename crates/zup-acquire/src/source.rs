//! Where content comes from.
//!
//! An [`ArtifactSource`] is one place a verified blob might already be. The
//! acquisition session does not know which places those are, does not care how
//! bytes travel, and does not learn where a blob ended up coming from - the
//! cache already proved it.
//!
//! This is the whole reason an offline artifact, a network, a USB stick, and a
//! warm cache can satisfy the same closure. They are four implementations of one
//! trait, and the transaction engine asks for a closure rather than for a
//! transport.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::cache::ContentCache;
use crate::cancellation::Cancellation;
use crate::descriptor::ContentDescriptor;
use crate::error::SourceError;

/// A boxed future, so the trait stays object safe.
pub type SourceFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// What a source is asked to do with one blob.
///
/// The source is handed the cache rather than a byte sink, so every source
/// gets the same resume, bounds, hashing, and atomic publication for free. A
/// source that wants to write somewhere else is not implementing this trait.
pub struct AcquireRequest<'a> {
    pub descriptor: &'a ContentDescriptor,
    pub cache: &'a ContentCache,
    pub cancellation: &'a dyn crate::Cancellation,
}

impl<'a> AcquireRequest<'a> {
    /// Build a request for one blob.
    pub fn new(
        descriptor: &'a ContentDescriptor,
        cache: &'a ContentCache,
        cancellation: &'a dyn crate::Cancellation,
    ) -> Self {
        Self {
            descriptor,
            cache,
            cancellation,
        }
    }
}

/// One place verified content can come from.
pub trait ArtifactSource: Send + Sync {
    /// A stable name for diagnostics, origin accounting, and event payloads.
    ///
    /// This is never logged with a URL, so a source that fronts an authenticated
    /// endpoint cannot leak a credential through a diagnostic.
    fn name(&self) -> &str;

    /// Whether this source could serve the blob at all.
    ///
    /// This is a cheap structural answer - a local file that exists, an
    /// in-memory map that has the digest - and is allowed to say yes
    /// optimistically. It is what lets a session skip a source entirely rather
    /// than opening a connection to be told nothing.
    fn contains(&self, descriptor: &ContentDescriptor) -> bool;

    /// Acquire the blob into the cache, verified by digest.
    ///
    /// Implementations must publish through [`ContentCache::writer`], which is
    /// what keeps partial bytes unobservable and the digest authoritative. A
    /// source that returns without publishing has not acquired anything.
    fn acquire<'a>(
        &'a self,
        request: AcquireRequest<'a>,
    ) -> SourceFuture<'a, Result<crate::VerifiedBlob, SourceError>>;
}

/// An ordered set of sources tried in turn.
///
/// Order is a preference, never a trust grant. A source that is first is not
/// more believed than a source that is third: every blob is verified by digest
/// on arrival, so a hostile or broken origin can only cost time, never content.
///
/// A source is dropped from consideration for the rest of a session only after
/// it has failed repeatedly, and a single failure is never enough: a transient
/// network error must not take a healthy CDN out of rotation for one install.
///
/// The chain is shared across the session's workers, so the lock is held only
/// long enough to read the live set and to record a failure. A transfer runs with
/// no lock held at all, which is what lets the scheduler's concurrency bound mean
/// something.
#[derive(Clone)]
pub struct SourceChain {
    sources: Arc<Vec<Arc<dyn ArtifactSource>>>,
    /// How many times a source may fail before it is set aside for this
    /// session.
    failure_budget: usize,
    /// Consecutive failures per source, in source order.
    failures: Arc<Vec<AtomicUsize>>,
}

impl SourceChain {
    /// Build a chain over `sources`, tried in order.
    pub fn new(sources: Vec<Arc<dyn ArtifactSource>>) -> Self {
        let failures = (0..sources.len())
            .map(|_| AtomicUsize::new(0))
            .collect::<Vec<_>>();
        Self {
            sources: Arc::new(sources),
            failure_budget: 3,
            failures: Arc::new(failures),
        }
    }

    /// Build a chain that gives up on a source after `budget` failures.
    pub fn with_failure_budget(mut self, budget: usize) -> Self {
        self.failure_budget = budget.max(1);
        self
    }

    /// The sources in this chain, in order.
    pub fn sources(&self) -> &[Arc<dyn ArtifactSource>] {
        &self.sources
    }

    /// Whether any live source claims to carry `descriptor`.
    pub fn any_contains(&self, descriptor: &ContentDescriptor) -> bool {
        self.live()
            .iter()
            .any(|(index, _)| self.sources[*index].contains(descriptor))
    }

    /// The names of the sources still in rotation.
    pub fn live_names(&self) -> Vec<&str> {
        self.live()
            .iter()
            .map(|(index, _)| self.sources[*index].name())
            .collect()
    }

    /// The indices of the sources still in rotation, in preference order.
    fn live(&self) -> Vec<(usize, Arc<dyn ArtifactSource>)> {
        self.sources
            .iter()
            .enumerate()
            .filter(|(index, _)| {
                self.failures[*index].load(Ordering::Relaxed) < self.failure_budget
            })
            .map(|(index, source)| (index, Arc::clone(source)))
            .collect()
    }

    fn is_live(&self, index: usize) -> bool {
        self.failures[index].load(Ordering::Relaxed) < self.failure_budget
    }

    /// Try every live source in order, returning the first verified blob.
    ///
    /// A source that reports it has nothing is not a failure and does not count
    /// against its budget; that is the difference between "this mirror does not
    /// carry that blob" and "this mirror is broken".
    pub async fn acquire(
        &self,
        descriptor: &ContentDescriptor,
        cache: &ContentCache,
        cancellation: &dyn Cancellation,
    ) -> Result<crate::VerifiedBlob, crate::AcquireError> {
        let mut reports = Vec::new();
        // The live set is snapshotted before the first await, so a transfer never
        // runs while a worker is holding the chain.
        let candidates = self.live();
        for (index, source) in candidates {
            if cancellation.is_cancelled() {
                return Err(crate::AcquireError::Cancelled);
            }
            if !source.contains(descriptor) {
                reports.push(SourceError::absent(source.name(), descriptor));
                continue;
            }
            let request = AcquireRequest::new(descriptor, cache, cancellation);
            match source.acquire(request).await {
                Ok(blob) => {
                    self.failures[index].store(0, Ordering::Relaxed);
                    return Ok(blob);
                }
                Err(error) => {
                    if matches!(error, SourceError::Absent { .. }) {
                        reports.push(error);
                        continue;
                    }
                    self.failures[index].fetch_add(1, Ordering::Relaxed);
                    reports.push(error);
                }
            }
        }
        if reports.is_empty() {
            reports.push(SourceError::absent("no live source", descriptor));
        }
        Err(crate::AcquireError::Unavailable {
            kind: descriptor.kind().as_str(),
            digest: descriptor.digest.to_hex(),
            reports,
        })
    }

    /// Whether a source is still in rotation, for a caller that wants to know
    /// why an origin was set aside.
    pub fn is_in_rotation(&self, name: &str) -> bool {
        self.sources
            .iter()
            .enumerate()
            .any(|(index, source)| source.name() == name && self.is_live(index))
    }
}
