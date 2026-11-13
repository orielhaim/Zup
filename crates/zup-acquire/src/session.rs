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
//! Transfers are bounded globally, with a separate limit on staging. Work is
//! taken from a queue in scheduler order, priority first, then smallest first,
//! so a 2 KiB document the installer is blocked on is not
//! sitting behind a 4 GiB payload blob.
//!
//! # Shutdown
//!
//! One owner schedules and joins all tasks. The first observed error or
//! cancellation stops dispatch and wakes pending transfers and staging permit
//! waits. Ready task results win ties with cancellation in the owner select.
//! Active staging is drained to completion, including on peer failure; stagers
//! must settle their own writes and child work before returning or unwinding.
//! Synchronous cache writes and publication are never interrupted between steps.
//! A terminal event follows settlement, and only success yields a barrier.
//! Callers must await `run` after requesting cancellation, rather than abort it.
//! Terminal delivery applies backpressure: keep reading the event receiver or
//! drop it. Intermediate progress samples are lossy.

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tokio::task::JoinSet;
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
            total: 1,
            staging: 1,
            progress_interval: Duration::from_millis(100),
        }
    }

    /// Refuse a configuration that could not make progress.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.total == 0 {
            return Err("a scheduler needs at least one transfer in flight");
        }
        if self.staging == 0 {
            return Err("a scheduler needs at least one staging operation in flight");
        }
        if self.progress_interval.is_zero() {
            return Err("progress sampling needs a nonzero interval");
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
/// The session owns this handle until all work has settled.
pub type SharedCancellation = Arc<dyn Cancellation>;

/// A boxed future, so the stager trait stays object safe.
pub type StagerFuture<'a, T> = Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// Stages a verified blob onto the volume the transaction will install from.
///
/// The session calls this as soon as a blob is verified, which is what lets
/// decompression and disk writes overlap with the remaining downloads. It must
/// not touch anything the application owns: staging writes to a work root, and
/// publishing is the transaction's job.
///
/// Once started, preparation and staging are awaited to completion even during
/// cancellation. Implementations must settle writes and child work before returning
/// or unwinding. A failed operation may leave private work files, never deployed
/// content. Callers must await `run` through cancellation rather than abort it.
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
/// Workers update counters; the session owner samples them.
#[derive(Debug, Default)]
struct Counters {
    acquired_bytes: AtomicU64,
    acquired_items: AtomicU64,
    completed_bytes: AtomicU64,
    completed_items: AtomicU64,
    active: AtomicU64,
}

enum WorkCompletion {
    Acquired(Result<VerifiedBlob, AcquireError>),
    Staged(Result<VerifiedBlob, AcquireError>),
}

/// Runs one closure to a verified barrier.
pub struct AcquisitionSession {
    plan: AcquisitionPlan,
    cache: Arc<ContentCache>,
    config: SchedulerConfig,
    stager: Arc<dyn BlobStager>,
    counters: Arc<Counters>,
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
            counters: Arc::default(),
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
        let result = self.run_inner(chain, cancellation, progress).await;
        let event = match &result {
            Ok(barrier) => AcquisitionEvent::AcquisitionComplete {
                items: barrier.outcome.items.len() as u64,
                bytes: barrier.outcome.total_bytes,
                elapsed_ms: barrier.outcome.elapsed.as_millis() as u64,
            },
            Err(AcquireError::Cancelled) => AcquisitionEvent::Cancelled {
                completed_items: self.counters.completed_items.load(Ordering::Relaxed),
                total_items: self.plan.len() as u64,
            },
            Err(error) => AcquisitionEvent::Failed {
                kind: failure_kind(error),
                digest: failure_digest(error),
                message: error.to_string(),
                reasons: failure_reasons(error),
            },
        };
        progress.terminal(event).await;
        result
    }

    async fn run_inner(
        &self,
        chain: SourceChain,
        cancellation: SharedCancellation,
        progress: &ProgressSink,
    ) -> Result<Barrier, AcquireError> {
        self.config.validate().map_err(AcquireError::Descriptor)?;
        if cancellation.is_cancelled() {
            return Err(AcquireError::Cancelled);
        }
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

        let mut items = Vec::new();
        let cache_hits = estimate.cached_items;
        self.fetch_all(
            self.plan.items().to_vec(),
            chain,
            &mut items,
            Arc::clone(&cancellation),
            progress,
            &estimate,
        )
        .await?;

        if cancellation.is_cancelled() {
            return Err(AcquireError::Cancelled);
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
        Ok(Barrier { outcome })
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
        let mut queue = VecDeque::from(work);
        let mut ready = VecDeque::new();
        let mut tasks = JoinSet::new();
        let stop = Arc::new(crate::CancelFlag::new());
        let mut acquiring = 0;
        let mut staging = 0;
        let mut failure = None;
        let mut interval = tokio::time::interval(self.config.progress_interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let cancelled = cancellation.cancelled();
        tokio::pin!(cancelled);
        loop {
            while failure.is_none()
                && !cancellation.is_cancelled()
                && !stop.is_cancelled()
                && staging < self.config.staging
            {
                let Some(blob): Option<VerifiedBlob> = ready.pop_front() else {
                    break;
                };
                let stager = self.stager.clone();
                let stop = stop.clone();
                staging += 1;
                tasks.spawn(async move {
                    let result = stager
                        .stage(blob.clone())
                        .await
                        .map_err(|detail| AcquireError::Staging {
                            kind: blob.descriptor.kind().as_str(),
                            digest: blob.descriptor.digest.to_hex(),
                            detail,
                        })
                        .map(|()| blob);
                    if result.is_err() {
                        stop.cancel();
                    }
                    WorkCompletion::Staged(result)
                });
            }
            while failure.is_none()
                && !cancellation.is_cancelled()
                && !stop.is_cancelled()
                && acquiring < self.config.total
                && ready.len() < self.config.total
            {
                let Some(item) = queue.pop_front() else { break };
                let chain = chain.clone();
                let cache = self.cache.clone();
                let stop = stop.clone();
                let counters = self.counters.clone();
                acquiring += 1;
                tasks.spawn(async move {
                    let result = async {
                        if stop.is_cancelled() { return Err(AcquireError::Cancelled); }
                        if let Some(blob) = cache.get(&item.descriptor, Verify::for_kind(item.descriptor.kind()))? {
                            return Ok(blob);
                        }
                        let _active = ActiveTransfer::new(&counters.active);
                        tokio::select! {
                            biased;
                            _ = stop.cancelled() => Err(AcquireError::Cancelled),
                            result = chain.acquire(&item.descriptor, &cache, stop.as_ref()) => result,
                        }
                    }.await;
                    WorkCompletion::Acquired(result)
                });
            }
            if tasks.is_empty() {
                break;
            }
            let error = tokio::select! {
                biased;
                joined = tasks.join_next() => match joined.expect("task set nonempty") {
                    Ok(WorkCompletion::Acquired(result)) => {
                        acquiring -= 1;
                        match result { Ok(blob) => { self.counters.acquired_bytes.fetch_add(blob.wire_size, Ordering::Relaxed); self.counters.acquired_items.fetch_add(1, Ordering::Relaxed); ready.push_back(blob); None }, Err(error) => Some(error) }
                    }
                    Ok(WorkCompletion::Staged(result)) => {
                        staging -= 1;
                        match result {
                            Ok(blob) => {
                                self.counters.completed_bytes.fetch_add(blob.wire_size, Ordering::Relaxed);
                                self.counters.completed_items.fetch_add(1, Ordering::Relaxed);
                                items.push(blob);
                                None
                            }
                            Err(error) => Some(error),
                        }
                    }
                    Err(error) => Some(AcquireError::Worker(error.to_string())),
                },
                _ = &mut cancelled, if failure.is_none() => Some(AcquireError::Cancelled),
                _ = interval.tick() => { self.sample_progress(progress, total_bytes, total_items, estimate); None }
            };
            if failure.is_none()
                && let Some(error) = error
                && !(matches!(error, AcquireError::Cancelled)
                    && stop.is_cancelled()
                    && !cancellation.is_cancelled())
            {
                let (retries, delay_ms, reason) = retry_bookkeeping(&error);
                if retries > 0 {
                    progress.sample(AcquisitionEvent::Retrying {
                        kind: "blob",
                        digest: failure_digest(&error).unwrap_or_default(),
                        attempt: retries,
                        delay_ms,
                        reason,
                    });
                }
                failure = Some(error);
                stop.cancel();
            }
        }
        self.sample_progress(progress, total_bytes, total_items, estimate);
        match failure {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn sample_progress(
        &self,
        progress: &ProgressSink,
        total_bytes: u64,
        total_items: u64,
        estimate: &AcquisitionEstimate,
    ) {
        let mut sample = AcquisitionProgress {
            phase: if self.counters.active.load(Ordering::Relaxed) == 0 {
                AcquisitionPhase::Staging
            } else {
                AcquisitionPhase::Downloading
            },
            acquired_bytes: self.counters.acquired_bytes.load(Ordering::Relaxed),
            acquired_items: self.counters.acquired_items.load(Ordering::Relaxed),
            completed_bytes: self.counters.completed_bytes.load(Ordering::Relaxed),
            total_bytes,
            cached_bytes: estimate.cached_bytes,
            cached_items: estimate.cached_items as u64,
            active_transfers: self.counters.active.load(Ordering::Relaxed),
            completed_items: self.counters.completed_items.load(Ordering::Relaxed),
            total_items,
            elapsed: self.start.elapsed(),
            throughput: 0,
            eta: None,
            retries: 0,
        };
        sample.recompute();
        sample.throughput = 0;
        sample.eta = None;
        progress.sample(AcquisitionEvent::DownloadProgress {
            progress: Box::new(sample),
        });
    }
}

struct ActiveTransfer<'a>(&'a AtomicU64);

impl<'a> ActiveTransfer<'a> {
    fn new(active: &'a AtomicU64) -> Self {
        active.fetch_add(1, Ordering::Relaxed);
        Self(active)
    }
}

impl Drop for ActiveTransfer<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

fn failure_digest(error: &AcquireError) -> Option<String> {
    match error {
        AcquireError::Unavailable { digest, .. }
        | AcquireError::Missing { digest, .. }
        | AcquireError::Staging { digest, .. } => Some(digest.clone()),
        _ => None,
    }
}

fn failure_kind(error: &AcquireError) -> &'static str {
    match error {
        AcquireError::Cancelled => "cancelled",
        AcquireError::Worker(_) => "worker",
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

fn retry_bookkeeping(error: &AcquireError) -> (u32, u64, String) {
    if let AcquireError::Unavailable { reports, .. } = error
        && let Some(retry) = reports.iter().find_map(retry_marker)
    {
        return retry;
    }
    (0, 0, String::new())
}
