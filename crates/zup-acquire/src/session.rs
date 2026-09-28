//! The scheduler and the acquisition barrier.
//!
//! One session takes a closure and a chain of sources and returns the same
//! closure, verified, in the cache. It knows nothing about HTTP, TUF, or
//! components, which is what lets a fresh install, an update, a modify, and a
//! repair all be the same call.
//!
//! # The barrier
//!
//! Everything a session does happens **before** the machine changes. Once
//! [`Barrier`] is returned, every byte the transaction needs is present and
//! verified, and only then may files, registry entries, services, or PATH be
//! touched. That is what makes it safe for a network to be in the loop at all: a
//! download that fails cannot leave a half-installed application, because a
//! download that fails never reaches the part of the system that installs.
//!
//! Overlap is still available *inside* the barrier. A verified blob is handed to
//! a stager as soon as it lands, so decompression, logical re-verification, and
//! writing to the destination volume run concurrently with the remaining
//! downloads. The stages overlap; the mutation does not begin early.
//!
//! # Concurrency
//!
//! Bounded, and bounded per origin rather than as one global number, because the
//! thing that actually limits a transfer is the connection pool of the thing
//! serving it. Work is taken from a queue in scheduler order - priority first,
//! then smallest first - so a 2 KiB document the installer is blocked on is not
//! sitting behind a 4 GiB payload blob.

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::{Mutex, mpsc};
use zup_core::Sha256Digest;

use crate::cache::{CacheProbe, ContentCache, VerifiedBlob, Verify};
use crate::cancellation::Cancellation;
use crate::error::{AcquireError, SourceError};
use crate::plan::{AcquisitionEstimate, AcquisitionItem, AcquisitionPlan};
use crate::progress::{AcquisitionEvent, AcquisitionPhase, AcquisitionProgress};
use crate::source::SourceChain;

/// How many transfers one session runs at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SchedulerConfig {
    /// Transfers in flight against a single origin.
    ///
    /// HTTP/2 multiplexes many requests over one connection, so this is a
    /// useful number rather than a formality; it is also the number a
    /// conventional CDN's edge will serve happily.
    pub per_origin: usize,
    /// Transfers in flight across every origin.
    pub total: usize,
    /// How many verified blobs may be decompressed and staged at once.
    pub staging: usize,
    /// How often a progress sample is emitted.
    pub progress_interval: Duration,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            per_origin: 6,
            total: 12,
            staging: 2,
            progress_interval: Duration::from_millis(100),
        }
    }
}

impl SchedulerConfig {
    /// Sequential: one transfer, one staging operation. The honest baseline a
    /// benchmark compares against.
    pub const fn sequential() -> Self {
        Self {
            per_origin: 1,
            total: 1,
            staging: 1,
            progress_interval: Duration::from_millis(100),
        }
    }

    /// Refuse a configuration that could not make progress.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.per_origin == 0 || self.total == 0 {
            return Err("a scheduler needs at least one transfer in flight");
        }
        if self.staging == 0 {
            return Err("a scheduler needs at least one staging operation in flight");
        }
        Ok(())
    }
}

/// Receives aggregate progress as a session runs.
///
/// Samples are delivered with a non-blocking send, so a consumer that is not
/// keeping up loses intermediate samples rather than stalling the transfers.
/// Terminal events are delivered with a blocking send after every worker has
/// settled, so a consumer that is still reading always sees the outcome.
#[derive(Debug, Clone)]
pub struct ProgressSink(mpsc::Sender<AcquisitionEvent>);

impl ProgressSink {
    /// A sink that discards events, for a caller that only wants the outcome.
    pub fn discard() -> Self {
        let (sender, receiver) = mpsc::channel(1);
        drop(receiver);
        Self(sender)
    }

    /// A sink that delivers every event it can.
    pub fn channel(capacity: usize) -> (Self, mpsc::Receiver<AcquisitionEvent>) {
        let (sender, receiver) = mpsc::channel(capacity.max(1));
        (Self(sender), receiver)
    }

    /// Offer a sample. A full channel drops it, which is the intended
    /// behaviour: the next sample supersedes it.
    fn sample(&self, event: AcquisitionEvent) {
        let _ = self.0.try_send(event);
    }

    /// Offer an event from outside a session.
    ///
    /// Release resolution happens before a session exists and is still part of
    /// one logical operation, so its events travel the same channel on the same
    /// terms: a consumer that is not keeping up loses a sample rather than
    /// stalling a network fetch.
    pub fn offer(&self, event: AcquisitionEvent) {
        self.sample(event);
    }

    /// Deliver an event that must not be dropped.
    async fn terminal(&self, event: AcquisitionEvent) {
        let _ = self.0.send(event).await;
    }
}

/// A cancellation question a worker can hold on to.
///
/// The session's workers are spawned tasks, so they need an owned handle rather
/// than a borrow. `Arc<dyn Cancellation>` is that handle: a flag is a single
/// atomic, so sharing one is free, and every worker sees the same answer.
pub type SharedCancellation = Arc<dyn Cancellation>;

/// A boxed future, so the stager trait stays object safe.
pub type StagerFuture<'a, T> = Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// Stages a verified blob onto the volume the transaction will install from.
///
/// The session calls this as soon as a blob is verified, which is what lets
/// decompression and disk writes overlap with the remaining downloads. It must
/// not touch anything the application owns: staging writes to a work root, and
/// publishing is the transaction's job.
pub trait BlobStager: Send + Sync {
    /// Prepare whatever the stager needs before any blob arrives.
    fn prepare(&self) -> StagerFuture<'_, Result<(), String>>;

    /// Take one verified blob. Called at most `SchedulerConfig::staging` times
    /// concurrently.
    fn stage(&self, blob: VerifiedBlob) -> StagerFuture<'_, Result<(), String>>;
}

/// A stager that does nothing, for a caller that only needs the cache.
pub struct NoStaging;

impl BlobStager for NoStaging {
    fn prepare(&self) -> StagerFuture<'_, Result<(), String>> {
        Box::pin(std::future::ready(Ok(())))
    }

    fn stage(&self, _blob: VerifiedBlob) -> StagerFuture<'_, Result<(), String>> {
        Box::pin(std::future::ready(Ok(())))
    }
}

/// What a completed session produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcquisitionOutcome {
    /// Every item in the closure, verified and present.
    pub items: Vec<VerifiedBlob>,
    pub estimate: AcquisitionEstimate,
    pub elapsed: Duration,
    /// Items the machine already had.
    pub cache_hits: usize,
    /// The total wire bytes the closure names.
    pub total_bytes: u64,
}

impl AcquisitionOutcome {
    /// Look up one verified blob by digest.
    pub fn get(&self, digest: &Sha256Digest) -> Option<&VerifiedBlob> {
        self.items
            .iter()
            .find(|blob| &blob.descriptor.digest == digest)
    }

    /// How many blobs are in the closure.
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether the closure is empty.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

/// The barrier. Holding this is the statement that the machine may now change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Barrier {
    outcome: AcquisitionOutcome,
}

impl Barrier {
    /// Take the barrier, yielding the verified closure.
    pub fn enter(self) -> AcquisitionOutcome {
        self.outcome
    }

    /// Look at the closure without entering.
    pub fn outcome(&self) -> &AcquisitionOutcome {
        &self.outcome
    }
}

/// Shared counters, so progress is a read of state rather than a message per
/// chunk.
///
/// Each counter is independently shareable so the progress sampler can be a
/// detached task that reads the same state the transfers write, without holding
/// a borrow on the session.
#[derive(Debug, Default, Clone)]
struct Counters {
    completed_bytes: Arc<AtomicU64>,
    completed_items: Arc<AtomicU64>,
    active: Arc<AtomicU64>,
}

/// One transfer's result, or the reason it failed.
type TransferResult = Result<VerifiedBlob, TransferFailure>;

/// A transfer failure, tagged with the retry bookkeeping a source performed so
/// the session can report it without the source and the scheduler sharing
/// mutable state.
#[derive(Debug)]
struct TransferFailure {
    error: AcquireError,
    retries: u32,
    last_delay_ms: u64,
    last_reason: String,
}

impl TransferFailure {
    fn new(error: AcquireError) -> Self {
        let (retries, last_delay_ms, last_reason) = retry_bookkeeping(&error);
        Self {
            error,
            retries,
            last_delay_ms,
            last_reason,
        }
    }
}

/// Recover a retry count from a source's report.
///
/// A source records its attempts in the detail of an unavailable report, so the
/// session can surface them without every source growing a side channel. The
/// format is fixed and owned by this crate: `<n> attempts, last delay <ms>ms, <reason>`.
fn retry_bookkeeping(error: &AcquireError) -> (u32, u64, String) {
    if let AcquireError::Unavailable { reports, .. } = error
        && let Some(retry) = reports.iter().find_map(retry_marker)
    {
        return retry;
    }
    (0, 0, String::new())
}

/// Runs one closure to a verified barrier.
pub struct AcquisitionSession {
    plan: AcquisitionPlan,
    cache: Arc<ContentCache>,
    config: SchedulerConfig,
    stager: Arc<dyn BlobStager>,
    counters: Counters,
    start: Instant,
}

impl AcquisitionSession {
    /// Build a session over `plan`, writing into `cache`.
    pub fn new(plan: AcquisitionPlan, cache: Arc<ContentCache>, config: SchedulerConfig) -> Self {
        Self {
            plan,
            cache,
            config,
            stager: Arc::new(NoStaging),
            counters: Counters::default(),
            start: Instant::now(),
        }
    }

    /// Stage each verified blob through `stager` as it lands.
    pub fn with_stager(mut self, stager: Arc<dyn BlobStager>) -> Self {
        self.stager = stager;
        self
    }

    /// What the closure will cost given what the cache already holds.
    pub fn estimate(&self) -> AcquisitionEstimate {
        AcquisitionEstimate::measure(self.plan.items(), |item| self.is_present(item))
    }

    fn is_present(&self, item: &AcquisitionItem) -> bool {
        self.cache
            .probe(&item.descriptor, Verify::for_kind(item.descriptor.kind()))
            .map(|probe| matches!(probe, CacheProbe::Present { .. }))
            .unwrap_or(false)
    }

    /// Run the session to the barrier.
    ///
    /// On success the returned [`Barrier`] is the only thing that authorizes a
    /// machine mutation. On any failure, nothing has been mutated.
    pub async fn run(
        self,
        chain: SourceChain,
        cancellation: SharedCancellation,
        progress: &ProgressSink,
    ) -> Result<Barrier, AcquireError> {
        self.config.validate().map_err(AcquireError::Descriptor)?;
        self.stager
            .prepare()
            .await
            .map_err(|detail| AcquireError::Staging {
                kind: "staging root",
                digest: String::new(),
                detail,
            })?;

        let estimate = self.estimate();
        progress.sample(AcquisitionEvent::AcquisitionStarted {
            variant: String::new(),
            items: self.plan.len() as u64,
            estimate,
        });

        let (mut items, missing, cache_hits) = self.take_cached(progress, &cancellation).await?;
        if !missing.is_empty() {
            self.fetch_all(
                missing,
                chain,
                &mut items,
                cancellation,
                progress,
                &estimate,
            )
            .await?;
        }

        // The barrier check. Every item the closure named must be present and
        // verified; anything else is a refusal, not a partial success.
        for item in self.plan.items() {
            if !items
                .iter()
                .any(|blob| blob.descriptor.digest == item.descriptor.digest)
            {
                let error = AcquireError::Missing {
                    kind: item.descriptor.kind().as_str(),
                    digest: item.descriptor.digest.to_hex(),
                };
                progress
                    .terminal(AcquisitionEvent::Failed {
                        kind: "incomplete_closure",
                        digest: Some(item.descriptor.digest.to_hex()),
                        message: error.to_string(),
                        reasons: Vec::new(),
                        machine_unchanged: true,
                    })
                    .await;
                return Err(error);
            }
        }

        let outcome = AcquisitionOutcome {
            items,
            estimate,
            elapsed: self.start.elapsed(),
            cache_hits,
            total_bytes: estimate
                .download_bytes
                .saturating_add(estimate.cached_bytes),
        };
        progress
            .terminal(AcquisitionEvent::AcquisitionComplete {
                items: outcome.items.len() as u64,
                bytes: outcome.total_bytes,
                elapsed_ms: outcome.elapsed.as_millis() as u64,
            })
            .await;
        Ok(Barrier { outcome })
    }

    /// Split the closure into what the cache already holds and what must move.
    async fn take_cached(
        &self,
        progress: &ProgressSink,
        cancellation: &SharedCancellation,
    ) -> Result<(Vec<VerifiedBlob>, Vec<AcquisitionItem>, usize), AcquireError> {
        let mut present = Vec::new();
        let mut missing = Vec::new();
        let mut hits = 0usize;
        for item in self.plan.items() {
            if cancellation.is_cancelled() {
                progress
                    .terminal(AcquisitionEvent::Cancelled {
                        completed_items: hits as u64,
                        total_items: self.plan.len() as u64,
                    })
                    .await;
                return Err(AcquireError::Cancelled);
            }
            let descriptor = item.descriptor;
            let verify = Verify::for_kind(descriptor.kind());
            match self.cache.probe(&descriptor, verify)? {
                CacheProbe::Present { wire_size } => {
                    let Some(blob) = self.cache.get(&descriptor, verify)? else {
                        missing.push(item.clone());
                        continue;
                    };
                    hits += 1;
                    self.counters
                        .completed_bytes
                        .fetch_add(wire_size, Ordering::Relaxed);
                    self.counters
                        .completed_items
                        .fetch_add(1, Ordering::Relaxed);
                    if hits <= MAX_REPORTED_CACHE_HITS {
                        progress.sample(AcquisitionEvent::CacheHit {
                            kind: descriptor.kind().as_str(),
                            digest: descriptor.digest.to_hex(),
                            wire_bytes: wire_size,
                        });
                    }
                    present.push(blob);
                }
                CacheProbe::Absent | CacheProbe::Resumable { .. } => missing.push(item.clone()),
            }
        }
        Ok((present, missing, hits))
    }

    /// Dispatch the missing work across a bounded pool and keep the progress
    /// sample current while it runs.
    async fn fetch_all(
        &self,
        work: Vec<AcquisitionItem>,
        chain: SourceChain,
        items: &mut Vec<VerifiedBlob>,
        cancellation: SharedCancellation,
        progress: &ProgressSink,
        estimate: &AcquisitionEstimate,
    ) -> Result<(), AcquireError> {
        let total_items = self.plan.len() as u64;
        let total_bytes = estimate
            .download_bytes
            .saturating_add(estimate.cached_bytes);
        let workers = self.config.total.min(work.len()).max(1);
        let queue = Arc::new(Mutex::new(work.into_iter().collect::<VecDeque<_>>()));
        let cache = Arc::clone(&self.cache);
        let stager = Arc::clone(&self.stager);
        let counters = self.counters.clone();
        // The staging bound is on decompression and disk writes only. Applying
        // it to the transfers would cap concurrency at the staging depth and
        // make a slow disk throttle the network, which is backwards.
        let staging = Arc::new(tokio::sync::Semaphore::new(self.config.staging));

        let (results, mut receiver) = mpsc::channel::<(TransferResult, AcquisitionItem)>(workers);
        let mut handles = Vec::with_capacity(workers);
        for _ in 0..workers {
            let queue = Arc::clone(&queue);
            let chain = chain.clone();
            let cache = Arc::clone(&cache);
            let stager = Arc::clone(&stager);
            let results = results.clone();
            let staging = Arc::clone(&staging);
            let counters = counters.clone();
            let cancellation = Arc::clone(&cancellation);
            handles.push(tokio::spawn(async move {
                loop {
                    let next = { queue.lock().await.pop_front() };
                    let Some(item) = next else { break };
                    if cancellation.is_cancelled() {
                        let _ = results
                            .send((Err(TransferFailure::new(AcquireError::Cancelled)), item))
                            .await;
                        break;
                    }
                    counters.active.fetch_add(1, Ordering::Relaxed);
                    let acquired = chain
                        .acquire(&item.descriptor, &cache, cancellation.as_ref())
                        .await;
                    counters.active.fetch_sub(1, Ordering::Relaxed);
                    let outcome = match acquired {
                        Ok(blob) => {
                            let _permit = staging.acquire().await;
                            match stager.stage(blob.clone()).await {
                                Ok(()) => {
                                    counters
                                        .completed_bytes
                                        .fetch_add(blob.wire_size, Ordering::Relaxed);
                                    counters.completed_items.fetch_add(1, Ordering::Relaxed);
                                    Ok(blob)
                                }
                                Err(detail) => Err(AcquireError::Staging {
                                    kind: item.descriptor.kind().as_str(),
                                    digest: item.descriptor.digest.to_hex(),
                                    detail,
                                }),
                            }
                        }
                        Err(error) => Err(error),
                    };
                    let outcome = outcome.map_err(TransferFailure::new);
                    if results.send((outcome, item)).await.is_err() {
                        break;
                    }
                }
            }));
        }
        drop(results);

        let sampler = self.spawn_sampler(progress, total_bytes, total_items, estimate);

        let mut failure: Option<(TransferFailure, AcquisitionItem)> = None;
        let mut workers_done = 0usize;
        while let Some((outcome, item)) = receiver.recv().await {
            match outcome {
                Ok(blob) => items.push(blob),
                Err(error) => {
                    if error.retries > 0 {
                        progress.sample(AcquisitionEvent::Retrying {
                            kind: item.descriptor.kind().as_str(),
                            digest: item.descriptor.digest.to_hex(),
                            attempt: error.retries,
                            delay_ms: error.last_delay_ms,
                            reason: error.last_reason.clone(),
                        });
                    }
                    if failure.is_none() {
                        failure = Some((error, item));
                    }
                }
            }
            workers_done += 1;
            if failure.is_some() {
                break;
            }
        }
        for handle in handles {
            let _ = handle.await;
        }
        let _ = workers_done;
        sampler.abort();

        if let Some((failure, item)) = failure {
            let error = failure.error;
            if matches!(error, AcquireError::Cancelled) {
                progress
                    .terminal(AcquisitionEvent::Cancelled {
                        completed_items: self.counters.completed_items.load(Ordering::Relaxed),
                        total_items,
                    })
                    .await;
                return Err(error);
            }
            progress
                .terminal(AcquisitionEvent::Failed {
                    kind: failure_kind(&error),
                    digest: Some(item.descriptor.digest.to_hex()),
                    message: error.to_string(),
                    reasons: failure_reasons(&error),
                    machine_unchanged: true,
                })
                .await;
            return Err(error);
        }
        Ok(())
    }

    /// Sample progress on an interval until the last byte lands.
    ///
    /// The sample is a read of shared counters, so sampling costs nothing on the
    /// transfer path and cannot slow a download down.
    fn spawn_sampler(
        &self,
        progress: &ProgressSink,
        total_bytes: u64,
        total_items: u64,
        estimate: &AcquisitionEstimate,
    ) -> tokio::task::JoinHandle<()> {
        let progress = progress.clone();
        let interval = self.config.progress_interval;
        let start = self.start;
        let cached_bytes = estimate.cached_bytes;
        let cached_items = estimate.cached_items as u64;
        let counters = self.counters.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(interval).await;
                let mut sample = AcquisitionProgress {
                    phase: AcquisitionPhase::Downloading,
                    completed_bytes: counters.completed_bytes.load(Ordering::Relaxed),
                    total_bytes,
                    cached_bytes,
                    cached_items,
                    active_transfers: counters.active.load(Ordering::Relaxed),
                    completed_items: counters.completed_items.load(Ordering::Relaxed),
                    total_items,
                    elapsed: start.elapsed(),
                    throughput: 0,
                    eta: None,
                    retries: 0,
                };
                sample.recompute();
                let complete = sample.is_complete();
                progress.sample(AcquisitionEvent::DownloadProgress {
                    progress: Box::new(sample),
                });
                if complete {
                    return;
                }
            }
        })
    }
}

fn failure_kind(error: &AcquireError) -> &'static str {
    match error {
        AcquireError::Cancelled => "cancelled",
        AcquireError::Unavailable { .. } => "unavailable",
        AcquireError::Missing { .. } => "incomplete_closure",
        AcquireError::Staging { .. } => "staging",
        AcquireError::Cache(_) => "cache",
        AcquireError::TooLarge { .. } => "too_large",
        AcquireError::TooManyItems { .. } => "too_many_items",
        AcquireError::Descriptor(_) => "descriptor",
        AcquireError::Source(_) => "source",
        AcquireError::Escapes { .. } | AcquireError::Link { .. } => "path",
        AcquireError::Io { .. } | AcquireError::Json(_) => "io",
    }
}

/// One line per source that was tried, for a failure a person has to act on.
///
/// The top-level message says what could not be produced. This says why each
/// place could not provide it, which is the difference between a report nobody
/// can act on and one that names the thing to fix.
fn failure_reasons(error: &AcquireError) -> Vec<String> {
    match error {
        AcquireError::Unavailable { reports, .. } => {
            reports.iter().map(|report| report.to_string()).collect()
        }
        _ => Vec::new(),
    }
}

/// How many cache hits are reported individually before the rest are counted.
///
/// A closure can be tens of thousands of blobs and nobody wants a stream of one
/// line per blob. The total is on the progress sample and in the outcome, so
/// nothing is lost; only the per-blob narration is capped.
const MAX_REPORTED_CACHE_HITS: usize = 16;

/// Where a source's retry bookkeeping is read back out of.
///
/// A source records its attempts in the detail of an unavailable report, so the
/// session can report a retry without every source growing a side channel. The
/// format is owned by this module so a source and the session cannot disagree
/// about it: `retry <n> attempts after <ms>ms: <reason>`.
pub const RETRY_MARKER: &str = "retry ";

/// Build the detail string a source uses to report its retry bookkeeping.
pub fn retry_detail(attempts: u32, delay: Duration, reason: &str) -> String {
    format!(
        "{RETRY_MARKER}{attempts} attempts after {}ms: {reason}",
        delay.as_millis()
    )
}

fn retry_marker(report: &SourceError) -> Option<(u32, u64, String)> {
    let SourceError::Unavailable { detail, .. } = report else {
        return None;
    };
    let rest = detail.split_once(RETRY_MARKER)?.1;
    let (attempts, rest) = rest.split_once(" attempts after ")?;
    let (delay, reason) = rest.split_once("ms: ")?;
    Some((
        attempts.trim().parse().ok()?,
        delay.trim().parse().ok()?,
        reason.trim().to_owned(),
    ))
}
