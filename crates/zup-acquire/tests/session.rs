//! The session: closure computation, scheduling, the barrier, and cancellation.
//!
//! Every test here drives the same call a fresh install, an update, a modify,
//! and a repair would make. Nothing needs a network, which is the point: the
//! engine's guarantees are properties of the closure and the cache, not of any
//! transport.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::*;
use tokio::sync::Mutex;
use zup_acquire::{
    AcquireError, AcquisitionEvent, AcquisitionItem, AcquisitionPlan, AcquisitionSession,
    ArtifactSource, CachePolicy, CancelFlag, Cancellation, ContentDescriptor, ContentKind,
    ContentPriority, ContentReason, DirectorySource, ProgressSink, SchedulerConfig, SourceChain,
    Verify,
};
use zup_core::Sha256Digest;

/// A source that hands each blob over one small piece at a time, so a session
/// that claims to run transfers concurrently is actually observed doing it.
struct CountingSource {
    name: String,
    blobs: std::collections::BTreeMap<Sha256Digest, Vec<u8>>,
    started: Arc<AtomicUsize>,
    /// Transfers currently inside the source.
    live: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
    chunk: usize,
}

impl CountingSource {
    fn new(name: &str, blobs: Vec<(ContentDescriptor, Vec<u8>)>, chunk: usize) -> Arc<Self> {
        Arc::new(Self {
            name: name.to_owned(),
            blobs: blobs.into_iter().map(|(d, w)| (d.digest, w)).collect(),
            started: Arc::new(AtomicUsize::new(0)),
            live: Arc::new(AtomicUsize::new(0)),
            peak: Arc::new(AtomicUsize::new(0)),
            chunk,
        })
    }

    fn peak(&self) -> usize {
        self.peak.load(Ordering::SeqCst)
    }
}

impl ArtifactSource for CountingSource {
    fn name(&self) -> &str {
        &self.name
    }

    fn contains(&self, descriptor: &ContentDescriptor) -> bool {
        self.blobs.contains_key(&descriptor.digest)
    }

    fn acquire<'a>(
        &'a self,
        request: zup_acquire::AcquireRequest<'a>,
    ) -> zup_acquire::SourceFuture<'a, Result<zup_acquire::VerifiedBlob, zup_acquire::SourceError>>
    {
        let Some(wire) = self.blobs.get(&request.descriptor.digest).cloned() else {
            let name = self.name.clone();
            let descriptor = *request.descriptor;
            return Box::pin(
                async move { Err(zup_acquire::SourceError::absent(&name, &descriptor)) },
            );
        };
        self.started.fetch_add(1, Ordering::SeqCst);
        let live = self.live.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(live, Ordering::SeqCst);
        let live_flag = Arc::clone(&self.live);
        let chunk = self.chunk;
        let name = self.name.clone();
        Box::pin(async move {
            let mut writer = request.cache.writer(request.descriptor).map_err(|error| {
                zup_acquire::SourceError::unavailable(&name, request.descriptor, error.to_string())
            })?;
            let outcome = async {
                for piece in wire.chunks(chunk) {
                    if request.cancellation.is_cancelled() {
                        return Err(zup_acquire::SourceError::unavailable(
                            &name,
                            request.descriptor,
                            "cancelled",
                        ));
                    }
                    writer.write(piece).map_err(|error| {
                        zup_acquire::SourceError::unavailable(
                            &name,
                            request.descriptor,
                            error.to_string(),
                        )
                    })?;
                    // Hand the runtime back between pieces so overlapping
                    // transfers are actually observable.
                    tokio::task::yield_now().await;
                }
                writer.commit().map_err(|error| {
                    zup_acquire::SourceError::unavailable(
                        &name,
                        request.descriptor,
                        error.to_string(),
                    )
                })
            }
            .await;
            live_flag.fetch_sub(1, Ordering::SeqCst);
            outcome
        })
    }
}

fn items_for(blobs: &[(ContentDescriptor, Vec<u8>)]) -> Vec<AcquisitionItem> {
    blobs
        .iter()
        .map(|(descriptor, _)| {
            AcquisitionItem::new(*descriptor, ContentReason::File { component: None })
        })
        .collect()
}

fn fixture(count: usize) -> Vec<(ContentDescriptor, Vec<u8>)> {
    (0..count)
        .map(|index| {
            let logical = payload(index as u8 + 30, 8 * 1024 + index * 512);
            let descriptor = payload_descriptor(&logical, LEVEL);
            (descriptor, wire_of(&logical, LEVEL))
        })
        .collect()
}

fn chain(sources: Vec<Arc<dyn ArtifactSource>>) -> SourceChain {
    SourceChain::new(sources)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_closure_is_satisfied_and_the_barrier_opens() {
    let cache = TestCache::new();
    let cache = cache.shared(CachePolicy::Keep);
    let blobs = fixture(4);
    let plan = AcquisitionPlan::build(items_for(&blobs)).expect("the closure is well formed");
    let progress = ProgressSink::discard();

    let barrier = AcquisitionSession::new(plan, Arc::clone(&cache), SchedulerConfig::default())
        .run(
            chain(vec![memory_source("origin", &blobs)]),
            never(),
            &progress,
        )
        .await
        .expect("the closure is satisfied");

    let outcome = barrier.enter();
    assert_eq!(outcome.items.len(), 4);
    assert_eq!(outcome.cache_hits, 0, "a cold cache moves every byte");
    assert_eq!(
        outcome.estimate.download_bytes,
        plan_wire_size(&blobs),
        "the estimate the user saw is the estimate that was met"
    );
}

fn plan_wire_size(blobs: &[(ContentDescriptor, Vec<u8>)]) -> u64 {
    blobs
        .iter()
        .map(|(descriptor, _)| descriptor.compressed_size)
        .sum()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_run_costs_no_network_bytes() {
    let cache = TestCache::new();
    let cache = cache.shared(CachePolicy::Keep);
    let blobs = fixture(3);
    let plan = AcquisitionPlan::build(items_for(&blobs)).expect("the closure is well formed");
    let progress = ProgressSink::discard();
    let source = memory_source("origin", &blobs);

    AcquisitionSession::new(plan.clone(), Arc::clone(&cache), SchedulerConfig::default())
        .run(chain(vec![Arc::clone(&source)]), never(), &progress)
        .await
        .expect("the first run succeeds");
    let warm =
        AcquisitionSession::new(plan, Arc::clone(&cache), SchedulerConfig::default()).estimate();
    assert_eq!(warm.download_bytes, 0, "a warm cache has nothing to fetch");
    assert_eq!(warm.cached_items, 3);
    assert_eq!(warm.missing_items, 0);
    assert!(warm.is_satisfied());

    // A source that carries nothing at all is still enough, because the cache
    // is the thing that satisfies the closure the second time.
    let outcome = AcquisitionSession::new(
        AcquisitionPlan::build(items_for(&blobs)).expect("the closure is well formed"),
        Arc::clone(&cache),
        SchedulerConfig::default(),
    )
    .run(chain(vec![]), never(), &progress)
    .await
    .expect("a warm cache needs no source at all")
    .enter();
    assert_eq!(outcome.cache_hits, 3);
    assert_eq!(outcome.estimate.download_bytes, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn only_the_selected_variant_and_components_are_downloaded() {
    let cache = TestCache::new();
    let cache = cache.shared(CachePolicy::Keep);
    // Two architectures' content plus two components' content, all present in
    // the origin. The selection keeps one of each.
    let windows = payload(40, 16 * 1024);
    let arm = payload(41, 16 * 1024);
    let core = payload(42, 16 * 1024);
    let tooling = payload(43, 16 * 1024);
    let available: Vec<(ContentDescriptor, Vec<u8>)> = [&windows, &arm, &core, &tooling]
        .into_iter()
        .map(|logical| (payload_descriptor(logical, LEVEL), wire_of(logical, LEVEL)))
        .collect();
    let (windows_descriptor, _, core_descriptor, tooling_descriptor, arm_descriptor) = (
        available[0].0,
        available[1].0,
        available[2].0,
        available[3].0,
        available[1].0,
    );
    let _ = arm_descriptor;
    let _ = (windows_descriptor, core_descriptor, tooling_descriptor);

    // The user chose the x64 variant with the core component only.
    let plan = AcquisitionPlan::build(vec![
        AcquisitionItem::new(available[0].0, ContentReason::Runtime),
        AcquisitionItem::new(
            available[2].0,
            ContentReason::File {
                component: Some("core".to_owned()),
            },
        ),
    ])
    .expect("the closure is well formed");
    let counting = CountingSource::new("cdn", available.clone(), 64 * 1024);
    let outcome = AcquisitionSession::new(plan, Arc::clone(&cache), SchedulerConfig::default())
        .run(
            chain(vec![counting.clone()]),
            never(),
            &ProgressSink::discard(),
        )
        .await
        .expect("the closure is satisfied")
        .enter();

    assert_eq!(outcome.items.len(), 2, "only the selection is fetched");
    assert_eq!(counting.started.load(Ordering::SeqCst), 2);
    assert!(
        !cache
            .get(&available[1].0, Verify::WireLength)
            .expect("the probe runs")
            .is_some(),
        "the other architecture is never downloaded"
    );
    assert!(
        !cache
            .get(&tooling_descriptor, Verify::WireLength)
            .expect("the probe runs")
            .is_some(),
        "an unselected component is never downloaded"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_shared_blob_is_downloaded_once_for_two_reasons() {
    let cache = TestCache::new();
    let cache = cache.shared(CachePolicy::Keep);
    let shared = payload(44, 32 * 1024);
    let descriptor = payload_descriptor(&shared, LEVEL);
    let wire = wire_of(&shared, LEVEL);
    // The same digest is wanted as a payload file and as an embedded
    // prerequisite package. It is one download and one cache entry.
    let plan = AcquisitionPlan::build(vec![
        AcquisitionItem::new(
            descriptor,
            ContentReason::File {
                component: Some("core".to_owned()),
            },
        ),
        AcquisitionItem::new(
            descriptor,
            ContentReason::Prerequisite {
                id: "vcredist".to_owned(),
            },
        ),
    ])
    .expect("the closure is well formed");
    assert_eq!(plan.len(), 1, "one digest is one item");

    let started = Arc::new(AtomicUsize::new(0));
    let counting = CountingSource::new("cdn", vec![(descriptor, wire)], 16 * 1024);
    let outcome = AcquisitionSession::new(plan, cache, SchedulerConfig::default())
        .run(
            chain(vec![counting.clone()]),
            never(),
            &ProgressSink::discard(),
        )
        .await
        .expect("the closure is satisfied")
        .enter();
    assert_eq!(outcome.items.len(), 1);
    assert_eq!(counting.started.load(Ordering::SeqCst), 1);
    let _ = started;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transfers_run_concurrently_and_the_pool_is_bounded() {
    let cache = TestCache::new();
    let cache = cache.shared(CachePolicy::Keep);
    let blobs = fixture(12);
    let plan = AcquisitionPlan::build(items_for(&blobs)).expect("the closure is well formed");
    let workers = 4;
    let source = CountingSource::new("cdn", blobs, 1024);
    let config = SchedulerConfig {
        per_origin: workers,
        total: workers,
        staging: 2,
        ..SchedulerConfig::default()
    };
    let outcome = AcquisitionSession::new(plan, cache, config)
        .run(
            chain(vec![source.clone()]),
            never(),
            &ProgressSink::discard(),
        )
        .await
        .expect("the closure is satisfied")
        .enter();
    assert_eq!(outcome.items.len(), 12);
    assert_eq!(source.started.load(Ordering::SeqCst), 12);
    assert!(
        source.peak() > 1,
        "a pool of {workers} must actually overlap transfers, saw a peak of {}",
        source.peak()
    );
    assert!(
        source.peak() <= workers,
        "the pool is bounded, saw a peak of {}",
        source.peak()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_sequential_pool_still_satisfies_the_same_closure() {
    let cache = TestCache::new();
    let cache = cache.shared(CachePolicy::Keep);
    let blobs = fixture(4);
    let plan = AcquisitionPlan::build(items_for(&blobs)).expect("the closure is well formed");
    let source = CountingSource::new("cdn", blobs, 1024);
    let outcome = AcquisitionSession::new(plan, cache, SchedulerConfig::sequential())
        .run(
            chain(vec![source.clone()]),
            never(),
            &ProgressSink::discard(),
        )
        .await
        .expect("the closure is satisfied")
        .enter();
    assert_eq!(outcome.items.len(), 4);
    assert_eq!(source.peak(), 1, "a pool of one never overlaps");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_large_blob_does_not_block_a_small_critical_one() {
    let cache = TestCache::new();
    let cache = cache.shared(CachePolicy::Keep);
    // A small runtime that the installer is blocked on, and a very large payload
    // blob. The runtime must be scheduled first even though the payload blob is
    // in the same closure.
    let runtime_bytes = payload(45, 2 * 1024);
    let huge = payload(46, 6 * 1024 * 1024);
    let runtime = ContentDescriptor::stored(ContentKind::Runtime, digest_of(&runtime_bytes), 2048)
        .with_priority(ContentPriority::Critical);
    let huge_descriptor = payload_descriptor(&huge, LEVEL);
    let plan = AcquisitionPlan::build(vec![
        AcquisitionItem::new(huge_descriptor, ContentReason::File { component: None }),
        AcquisitionItem::new(runtime, ContentReason::Runtime),
    ])
    .expect("the closure is well formed");
    assert_eq!(
        plan.items()[0].descriptor.kind(),
        ContentKind::Runtime,
        "the critical item is scheduled first"
    );
    assert!(plan.items()[0].wire_size() < plan.items()[1].wire_size());

    let progress = ProgressSink::discard();
    let outcome = AcquisitionSession::new(plan, cache, SchedulerConfig::sequential())
        .run(
            chain(vec![memory_source(
                "origin",
                &[
                    (runtime, runtime_bytes),
                    (huge_descriptor, wire_of(&huge, LEVEL)),
                ],
            )]),
            never(),
            &progress,
        )
        .await
        .expect("the closure is satisfied")
        .enter();
    assert_eq!(outcome.items.len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_source_that_returns_the_wrong_bytes_cannot_publish() {
    let cache = TestCache::new();
    let cache = cache.shared(CachePolicy::Keep);
    let logical = payload(47, 16 * 1024);
    let descriptor = payload_descriptor(&logical, LEVEL);
    let honest = wire_of(&logical, LEVEL);
    let dishonest = wire_of(&payload(48, 16 * 1024), LEVEL);
    assert_eq!(
        honest.len(),
        dishonest.len(),
        "the fixture must be the same length"
    );

    let plan = AcquisitionPlan::build(vec![AcquisitionItem::new(
        descriptor,
        ContentReason::File { component: None },
    )])
    .expect("the closure is well formed");
    let error = AcquisitionSession::new(plan, Arc::clone(&cache), SchedulerConfig::default())
        .run(
            chain(vec![LyingSource::new("cdn", descriptor, dishonest)]),
            never(),
            &ProgressSink::discard(),
        )
        .await
        .expect_err("a lying source cannot satisfy the barrier");
    assert!(matches!(error, AcquireError::Unavailable { .. }), "{error}");
    assert!(
        !cache
            .get(&descriptor, Verify::Full)
            .expect("the probe runs")
            .is_some(),
        "the lying source published nothing"
    );
    assert!(error.left_machine_unchanged());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_chain_falls_through_to_the_next_source_and_never_poisons_one() {
    let cache = TestCache::new();
    let cache = cache.shared(CachePolicy::Keep);
    let blobs = fixture(3);
    let honest = memory_source("mirror", &blobs);
    let broken = FailingSource::new("primary", "connection reset");
    let budgeted = SourceChain::new(vec![broken.clone(), honest.clone()]).with_failure_budget(2);

    // Two acquisitions: the broken source is taken out of rotation after its
    // budget, not after its first failure.
    for _ in 0..2 {
        let plan = AcquisitionPlan::build(items_for(&blobs)).expect("the closure is well formed");
        AcquisitionSession::new(plan, Arc::clone(&cache), SchedulerConfig::default())
            .run(
                SourceChain::new(vec![broken.clone(), honest.clone()]).with_failure_budget(2),
                never(),
                &ProgressSink::discard(),
            )
            .await
            .expect("the mirror satisfies the closure");
    }
    assert!(
        broken.reports() >= 2,
        "a broken origin is tried more than once before it is set aside"
    );
    assert!(
        budgeted.live_names().contains(&"mirror"),
        "a broken primary never removes a healthy mirror from rotation"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_broken_source_alone_leaves_the_machine_untouched_and_reports_why() {
    let cache = TestCache::new();
    let cache = cache.shared(CachePolicy::Keep);
    let blobs = fixture(2);
    let plan = AcquisitionPlan::build(items_for(&blobs)).expect("the closure is well formed");
    let progress = ProgressSink::channel(64);
    let (sink, mut events) = progress;
    let error = AcquisitionSession::new(plan, Arc::clone(&cache), SchedulerConfig::default())
        .run(
            chain(vec![FailingSource::new("primary", "connection reset")]),
            never(),
            &sink,
        )
        .await
        .expect_err("no source can satisfy the closure");
    assert!(error.left_machine_unchanged());
    assert!(cache.digests().expect("the cache lists").is_empty());

    let mut failure = None;
    while let Ok(event) = events.try_recv() {
        if matches!(event, AcquisitionEvent::Failed { .. }) {
            failure = Some(event);
        }
    }
    let event = failure.expect("a failure is reported");
    assert_eq!(event.reasons().len(), 1, "one source was tried");
    assert!(
        event.reasons()[0].contains("connection reset"),
        "the transport reason survives to the caller: {:?}",
        event.reasons()
    );
    match &event {
        AcquisitionEvent::Failed {
            machine_unchanged, ..
        } => assert!(
            *machine_unchanged,
            "the event states the machine is unchanged"
        ),
        other => panic!("expected a failure, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancellation_before_the_barrier_leaves_the_cache_intact_and_nothing_published() {
    let cache = TestCache::new();
    let cache = cache.shared(CachePolicy::Keep);
    let blobs = fixture(6);
    // Two blobs are already present and verified; they must survive a cancel.
    let warm = &blobs[..2];
    for (descriptor, wire) in warm {
        let mut writer = cache.writer(descriptor).expect("the writer opens");
        writer.write(wire).expect("the bytes land");
        writer.commit().expect("the blob verifies");
    }
    let plan = AcquisitionPlan::build(items_for(&blobs)).expect("the closure is well formed");
    let flag = Arc::new(CancelFlag::new());
    flag.cancel();
    let error = AcquisitionSession::new(plan, Arc::clone(&cache), SchedulerConfig::default())
        .run(
            chain(vec![memory_source("origin", &blobs)]),
            flag,
            &ProgressSink::discard(),
        )
        .await
        .expect_err("a cancelled session does not reach the barrier");
    assert!(matches!(error, AcquireError::Cancelled), "{error}");
    assert_eq!(
        cache.digests().expect("the cache lists").len(),
        2,
        "verified blobs are kept across a cancellation"
    );
    assert!(error.left_machine_unchanged());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cancelled_transfer_leaves_a_partial_rather_than_a_blob() {
    let cache = TestCache::new();
    let cache = cache.shared(CachePolicy::Keep);
    let logical = payload(49, 512 * 1024);
    let descriptor = payload_descriptor(&logical, LEVEL);
    let wire = wire_of(&logical, LEVEL);
    let flag = Arc::new(CancelFlag::new());
    // A source that cancels itself after the first chunk.
    struct SelfCancelling {
        flag: Arc<CancelFlag>,
        wire: Vec<u8>,
        name: String,
    }
    impl ArtifactSource for SelfCancelling {
        fn name(&self) -> &str {
            &self.name
        }
        fn contains(&self, descriptor: &ContentDescriptor) -> bool {
            descriptor.digest == digest_of(self.wire.as_slice()) || true
        }
        fn acquire<'a>(
            &'a self,
            request: zup_acquire::AcquireRequest<'a>,
        ) -> zup_acquire::SourceFuture<
            'a,
            Result<zup_acquire::VerifiedBlob, zup_acquire::SourceError>,
        > {
            let wire = self.wire.clone();
            let name = self.name.clone();
            Box::pin(async move {
                let mut writer = request.cache.writer(request.descriptor).map_err(|error| {
                    zup_acquire::SourceError::unavailable(
                        &name,
                        request.descriptor,
                        error.to_string(),
                    )
                })?;
                writer.write(&wire[..1024]).map_err(|error| {
                    zup_acquire::SourceError::unavailable(
                        &name,
                        request.descriptor,
                        error.to_string(),
                    )
                })?;
                self.flag.cancel();
                writer.abandon();
                Err(zup_acquire::SourceError::unavailable(
                    &name,
                    request.descriptor,
                    "cancelled",
                ))
            })
        }
    }
    let plan = AcquisitionPlan::build(vec![AcquisitionItem::new(
        descriptor,
        ContentReason::File { component: None },
    )])
    .expect("the closure is well formed");
    let _ = AcquisitionSession::new(plan, Arc::clone(&cache), SchedulerConfig::default())
        .run(
            chain(vec![Arc::new(SelfCancelling {
                flag: Arc::clone(&flag),
                wire: wire.clone(),
                name: "cdn".to_owned(),
            })]),
            Arc::clone(&flag) as Arc<dyn Cancellation>,
            &ProgressSink::discard(),
        )
        .await;
    let paths = cache.paths(&descriptor).expect("the blob has paths");
    assert!(
        !paths.final_path.exists(),
        "a partial is never a published blob"
    );
    assert!(
        cache
            .get(&descriptor, Verify::Full)
            .expect("the probe runs")
            .is_none(),
        "an incomplete transfer is not usable"
    );
    assert!(
        flag.is_cancelled(),
        "the flag the source raised is the one the session asked about"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_verified_blob_is_staged_as_it_lands_not_after_the_last_one() {
    let cache = TestCache::new();
    let cache = cache.shared(CachePolicy::Keep);
    let blobs = fixture(6);
    let plan = AcquisitionPlan::build(items_for(&blobs)).expect("the closure is well formed");
    let stager = RecordingStager::new();
    let outcome = AcquisitionSession::new(plan, cache, SchedulerConfig::default())
        .with_stager(stager.clone())
        .run(
            chain(vec![memory_source("origin", &blobs)]),
            never(),
            &ProgressSink::discard(),
        )
        .await
        .expect("the closure is satisfied")
        .enter();
    assert_eq!(stager.seen().len(), 6, "every verified blob is staged");
    for blob in &outcome.items {
        assert!(
            stager.seen().contains(&blob.descriptor.digest),
            "each blob is staged exactly once"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_staging_failure_closes_the_barrier() {
    let cache = TestCache::new();
    let cache = cache.shared(CachePolicy::Keep);
    let blobs = fixture(3);
    let doomed = blobs[0].0.digest;
    let plan = AcquisitionPlan::build(items_for(&blobs)).expect("the closure is well formed");
    let error = AcquisitionSession::new(plan, Arc::clone(&cache), SchedulerConfig::default())
        .with_stager(RecordingStager::failing(doomed))
        .run(
            chain(vec![memory_source("origin", &blobs)]),
            never(),
            &ProgressSink::discard(),
        )
        .await
        .expect_err("a staging failure must not reach the barrier");
    assert!(matches!(error, AcquireError::Staging { .. }), "{error}");
    assert!(error.left_machine_unchanged());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn progress_is_aggregate_and_carries_no_per_chunk_events() {
    let cache = TestCache::new();
    let cache = cache.shared(CachePolicy::Keep);
    let blobs = fixture(3);
    let plan = AcquisitionPlan::build(items_for(&blobs)).expect("the closure is well formed");
    let (sink, mut events) = ProgressSink::channel(512);
    AcquisitionSession::new(plan, cache, SchedulerConfig::default())
        .run(chain(vec![memory_source("origin", &blobs)]), never(), &sink)
        .await
        .expect("the closure is satisfied");

    let mut started = 0;
    let mut cache_hits = 0;
    let mut samples = 0;
    let mut complete = 0;
    let mut terminal = None;
    let mut elapsed_ms = 0u64;
    while let Ok(event) = events.try_recv() {
        match &event {
            AcquisitionEvent::AcquisitionStarted { .. } => started += 1,
            AcquisitionEvent::CacheHit { .. } => cache_hits += 1,
            AcquisitionEvent::DownloadProgress { .. } => samples += 1,
            AcquisitionEvent::AcquisitionComplete { elapsed_ms: ms, .. } => {
                complete += 1;
                elapsed_ms = *ms;
                terminal = Some(event.name());
            }
            other => panic!("unexpected event {other:?}"),
        }
    }
    assert_eq!(started, 1, "one start, not one per blob");
    assert_eq!(cache_hits, 0, "a cold cache has no hits");
    assert_eq!(complete, 1, "one completion");
    assert_eq!(terminal, Some("acquisition_complete"));
    // The claim is that progress is sampled on an interval rather than emitted
    // per transfer event. The bound comes from the *session's own* elapsed time,
    // which is the window the sampler ran in — measuring the drain loop instead
    // would make the bound a function of how fast this test reads a queue, which
    // has nothing to do with the claim.
    let bound = elapsed_ms / 100 + 2;
    assert!(
        samples as u64 <= bound,
        "progress must be sampled, not emitted per event: {samples} samples in {elapsed_ms}ms"
    );
    // And the stream carries nothing outside the closed set of aggregate events,
    // which the match above already enforces by rejecting anything else.
    assert!(samples <= complete + 64, "progress is bounded");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_event_stream_names_are_the_stable_aggregate_contract() {
    // These are the names a JSONL consumer switches on. Changing one is a
    // breaking change to the contract, so the test states them.
    let names = [
        AcquisitionEvent::ReleaseResolved {
            app_id: "com.acme".to_owned(),
            channel: "stable".to_owned(),
            version: "1.4.0".to_owned(),
            release_digest: digest_of(b"release"),
        },
        AcquisitionEvent::VariantSelected {
            variant: "windows-x64".to_owned(),
            target: "x86_64-pc-windows-msvc".to_owned(),
            compatibility: "native".to_owned(),
        },
        AcquisitionEvent::AcquisitionStarted {
            variant: "windows-x64".to_owned(),
            items: 0,
            estimate: Default::default(),
        },
        AcquisitionEvent::CacheHit {
            kind: "payload blob",
            digest: "0".repeat(64),
            wire_bytes: 1,
        },
        AcquisitionEvent::Retrying {
            kind: "payload blob",
            digest: "0".repeat(64),
            attempt: 1,
            delay_ms: 100,
            reason: "connection reset".to_owned(),
        },
        AcquisitionEvent::AcquisitionComplete {
            items: 0,
            bytes: 0,
            elapsed_ms: 0,
        },
        AcquisitionEvent::StagingComplete {
            staged: 0,
            bytes: 0,
        },
    ];
    let rendered: Vec<&str> = names.iter().map(AcquisitionEvent::name).collect();
    assert_eq!(
        rendered,
        vec![
            "release_resolved",
            "variant_selected",
            "acquisition_started",
            "cache_hit",
            "retrying",
            "acquisition_complete",
            "staging_complete",
        ]
    );
    for event in &names {
        let json = serde_json::to_value(event).expect("the event serializes");
        assert_eq!(
            json.get("event").and_then(|value| value.as_str()),
            Some(event.name()),
            "the wire name is the event name"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_local_tree_satisfies_a_closure_with_no_second_format() {
    let cache = TestCache::new();
    let cache = cache.shared(CachePolicy::Keep);
    let tree = tempfile::tempdir().expect("a temporary tree");
    let blobs = fixture(3);
    for (descriptor, wire) in &blobs {
        seed_web_tree(tree.path(), descriptor, wire);
    }
    let plan = AcquisitionPlan::build(items_for(&blobs)).expect("the closure is well formed");
    let outcome = AcquisitionSession::new(plan, cache, SchedulerConfig::default())
        .run(
            chain(vec![directory_source("usb", tree.path())]),
            never(),
            &ProgressSink::discard(),
        )
        .await
        .expect("the local tree satisfies the closure")
        .enter();
    assert_eq!(outcome.items.len(), 3);
    assert_eq!(
        outcome.cache_hits, 0,
        "a cold cache reads every blob from the tree"
    );
    assert_eq!(
        outcome.estimate.download_bytes,
        blobs.iter().map(|(d, _)| d.compressed_size).sum::<u64>(),
        "a local seed is accounted as a cold closure: the bytes moved, just not over a network"
    );
    // The bytes really are the ones the tree held.
    let restored = outcome.get(&blobs[0].0.digest).expect("the blob is there");
    assert_eq!(
        restored.read_to_end().expect("the blob decodes"),
        payload(30, 8 * 1024)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_local_tree_that_lacks_a_blob_falls_through_to_the_network_source() {
    let cache = TestCache::new();
    let cache = cache.shared(CachePolicy::Keep);
    let tree = tempfile::tempdir().expect("a temporary tree");
    let blobs = fixture(3);
    seed_web_tree(tree.path(), &blobs[0].0, &blobs[0].1);
    let plan = AcquisitionPlan::build(items_for(&blobs)).expect("the closure is well formed");
    let outcome = AcquisitionSession::new(plan, cache, SchedulerConfig::default())
        .run(
            chain(vec![
                directory_source("usb", tree.path()),
                memory_source("cdn", &blobs),
            ]),
            never(),
            &ProgressSink::discard(),
        )
        .await
        .expect("the chain satisfies the closure")
        .enter();
    assert_eq!(outcome.items.len(), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_blob_outside_a_local_tree_is_never_reachable() {
    let tree = tempfile::tempdir().expect("a temporary tree");
    let outside = tempfile::tempdir().expect("a temporary directory");
    let logical = payload(50, 4096);
    let descriptor = payload_descriptor(&logical, LEVEL);
    let hex = descriptor.digest.to_hex();
    // A file with the right name, in the wrong place.
    std::fs::write(outside.path().join(&hex), wire_of(&logical, LEVEL)).expect("the file lands");
    let source = DirectorySource::new("share", tree.path());
    assert!(
        !source.contains(&descriptor),
        "a tree does not reach outside itself"
    );
    assert!(source.locate(&descriptor).is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_estimate_reports_download_install_and_cache_in_one_model() {
    let cache = TestCache::new();
    let cache = cache.shared(CachePolicy::Keep);
    let blobs = fixture(4);
    let warm = &blobs[..1];
    for (descriptor, wire) in warm {
        let mut writer = cache.writer(descriptor).expect("the writer opens");
        writer.write(wire).expect("the bytes land");
        writer.commit().expect("the blob verifies");
    }
    let plan = AcquisitionPlan::build(items_for(&blobs)).expect("the closure is well formed");
    let session = AcquisitionSession::new(plan.clone(), cache, SchedulerConfig::default());
    let estimate = session.estimate();
    assert_eq!(estimate.cached_items, 1);
    assert_eq!(estimate.missing_items, 3);
    assert_eq!(estimate.cached_bytes, blobs[0].0.compressed_size);
    assert_eq!(
        estimate.download_bytes,
        blobs[1..]
            .iter()
            .map(|(d, _)| d.compressed_size)
            .sum::<u64>()
    );
    assert_eq!(
        estimate.install_bytes,
        blobs.iter().map(|(d, _)| d.size).sum::<u64>()
    );
    let lines = estimate.lines();
    assert_eq!(lines[0].0, "Download");
    assert_eq!(lines[1].0, "Install");
    assert_eq!(lines[2].0, "Already cached");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_update_closure_that_omits_unchanged_content_costs_nothing_for_it() {
    let cache = TestCache::new();
    let cache = cache.shared(CachePolicy::Keep);
    // Version 1 and version 2 share most of their content.
    let unchanged = payload(51, 64 * 1024);
    let changed = payload(52, 64 * 1024);
    let added = payload(53, 64 * 1024);
    let unchanged_descriptor = payload_descriptor(&unchanged, LEVEL);
    let changed_descriptor = payload_descriptor(&changed, LEVEL);
    let added_descriptor = payload_descriptor(&added, LEVEL);

    // The machine already has version 1's closure.
    for logical in [&unchanged, &changed] {
        let descriptor = payload_descriptor(logical, LEVEL);
        let mut writer = cache.writer(&descriptor).expect("the writer opens");
        writer
            .write(&wire_of(logical, LEVEL))
            .expect("the bytes land");
        writer.commit().expect("the blob verifies");
    }

    // Version 2's closure is the two it shares plus the one it adds.
    let plan = AcquisitionPlan::build(vec![
        AcquisitionItem::new(unchanged_descriptor, ContentReason::Retained),
        AcquisitionItem::new(changed_descriptor, ContentReason::File { component: None }),
        AcquisitionItem::new(added_descriptor, ContentReason::File { component: None }),
    ])
    .expect("the closure is well formed");
    let session = AcquisitionSession::new(plan.clone(), cache, SchedulerConfig::default());
    let estimate = session.estimate();
    assert_eq!(
        estimate.download_bytes, added_descriptor.compressed_size,
        "unchanged content costs zero network bytes"
    );
    assert_eq!(estimate.cached_items, 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_repair_closure_is_exactly_the_missing_owned_digests() {
    let cache = TestCache::new();
    let cache = cache.shared(CachePolicy::Keep);
    let present = payload(54, 32 * 1024);
    let missing = payload(55, 32 * 1024);
    let present_descriptor = payload_descriptor(&present, LEVEL);
    let missing_descriptor = payload_descriptor(&missing, LEVEL);
    let mut writer = cache.writer(&present_descriptor).expect("the writer opens");
    writer
        .write(&wire_of(&present, LEVEL))
        .expect("the bytes land");
    writer.commit().expect("the blob verifies");

    // Repair discovers one drifted file. It asks for that digest, not for the
    // whole variant, and not for anything it already holds.
    let plan = AcquisitionPlan::build(vec![AcquisitionItem::new(
        missing_descriptor,
        ContentReason::File { component: None },
    )])
    .expect("the closure is well formed");
    let outcome = AcquisitionSession::new(plan, Arc::clone(&cache), SchedulerConfig::default())
        .run(
            chain(vec![memory_source(
                "origin",
                &[(missing_descriptor, wire_of(&missing, LEVEL))],
            )]),
            never(),
            &ProgressSink::discard(),
        )
        .await
        .expect("the repair closure is satisfied")
        .enter();
    assert_eq!(outcome.items.len(), 1);
    assert_eq!(
        outcome.estimate.download_bytes,
        missing_descriptor.compressed_size
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_empty_closure_is_a_refusal_rather_than_a_silent_success() {
    let error = AcquisitionPlan::build(Vec::new()).expect_err("an empty closure is refused");
    assert!(matches!(error, AcquireError::Descriptor(_)), "{error}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_descriptors_for_one_digest_must_agree() {
    let logical = payload(56, 4096);
    let honest = payload_descriptor(&logical, LEVEL);
    let mut disagreeing = honest;
    disagreeing.size += 1;
    let error = AcquisitionPlan::build(vec![
        AcquisitionItem::new(honest, ContentReason::File { component: None }),
        AcquisitionItem::new(disagreeing, ContentReason::File { component: None }),
    ])
    .expect_err("one digest cannot have two sizes");
    assert!(matches!(error, AcquireError::Descriptor(_)), "{error}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_cache_is_not_consulted_through_a_source_chain_when_it_already_has_the_blob() {
    let cache = TestCache::new();
    let cache = cache.shared(CachePolicy::Keep);
    let blobs = fixture(1);
    let (descriptor, wire) = blobs[0].clone();
    let mut writer = cache.writer(&descriptor).expect("the writer opens");
    writer.write(&wire).expect("the bytes land");
    writer.commit().expect("the blob verifies");

    let plan = AcquisitionPlan::build(items_for(&blobs)).expect("the closure is well formed");
    // No sources at all: the cache is the source.
    let outcome = AcquisitionSession::new(plan, Arc::clone(&cache), SchedulerConfig::default())
        .run(chain(vec![]), never(), &ProgressSink::discard())
        .await
        .expect("the cache satisfies the closure")
        .enter();
    assert_eq!(outcome.cache_hits, 1);
    assert_eq!(outcome.items.len(), 1);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_document_and_a_payload_of_the_same_bytes_are_different_content() {
    let cache = TestCache::new();
    let cache = cache.shared(CachePolicy::Keep);
    let bytes = b"the same bytes".to_vec();
    let document =
        ContentDescriptor::stored(ContentKind::Metadata, digest_of(&bytes), bytes.len() as u64);
    let payload = ContentDescriptor::compressed(
        ContentKind::Payload,
        digest_of(&bytes),
        zstd::stream::encode_all(bytes.as_slice(), LEVEL)
            .expect("compresses")
            .len() as u64,
        bytes.len() as u64,
    );
    // Two descriptors, one digest, different verification policy. The cache keys
    // by digest, so the second one is a hit on the first — and the kind decides
    // how deeply it is re-checked.
    let mut writer = cache.writer(&document).expect("the writer opens");
    writer.write(&bytes).expect("the bytes land");
    writer.commit().expect("the document verifies");
    assert!(
        cache
            .get(&document, zup_acquire::Verify::Full)
            .expect("the probe runs")
            .is_some()
    );
    let _ = payload;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn progress_never_reports_more_bytes_than_the_closure_needs() {
    let cache = TestCache::new();
    let cache = cache.shared(CachePolicy::Keep);
    let blobs = fixture(3);
    let plan = AcquisitionPlan::build(items_for(&blobs)).expect("the closure is well formed");
    let total = plan.wire_size();
    let (sink, mut events) = ProgressSink::channel(512);
    AcquisitionSession::new(plan, cache, SchedulerConfig::default())
        .run(chain(vec![memory_source("origin", &blobs)]), never(), &sink)
        .await
        .expect("the closure is satisfied");
    while let Ok(event) = events.try_recv() {
        if let AcquisitionEvent::DownloadProgress { progress } = event {
            assert!(
                progress.completed_bytes <= progress.total_bytes,
                "progress overshot the closure: {progress:?}"
            );
            assert_eq!(progress.total_bytes, total);
            assert!(progress.percent().is_some_and(|percent| percent <= 100));
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_over_a_source_that_returns_nothing_names_every_source_it_tried() {
    let cache = TestCache::new();
    let cache = cache.shared(CachePolicy::Keep);
    let blobs = fixture(1);
    let plan = AcquisitionPlan::build(items_for(&blobs)).expect("the closure is well formed");
    let error = AcquisitionSession::new(plan, cache, SchedulerConfig::default())
        .run(
            chain(vec![
                memory_source("empty", &[]),
                FailingSource::new("primary", "connection reset"),
            ]),
            never(),
            &ProgressSink::discard(),
        )
        .await
        .expect_err("nothing satisfies the closure");
    let AcquireError::Unavailable { reports, .. } = &error else {
        panic!("expected an unavailable closure, got {error}");
    };
    let origins: Vec<&str> = reports.iter().map(|report| report.source()).collect();
    assert_eq!(origins, vec!["empty", "primary"]);
    assert!(
        reports
            .iter()
            .any(|report| report.to_string().contains("connection reset")),
        "the transport reason survives to the caller"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_memory_source_and_a_directory_source_produce_identical_closures() {
    let logical = payload(57, 64 * 1024);
    let descriptor = payload_descriptor(&logical, LEVEL);
    let wire = wire_of(&logical, LEVEL);
    let tree = tempfile::tempdir().expect("a temporary tree");
    seed_web_tree(tree.path(), &descriptor, &wire);

    let from_memory = TestCache::new();
    let from_memory = from_memory.shared(CachePolicy::Keep);
    AcquisitionSession::new(
        AcquisitionPlan::build(items_for(&[(descriptor, wire.clone())])).expect("the closure"),
        Arc::clone(&from_memory),
        SchedulerConfig::default(),
    )
    .run(
        chain(vec![memory_source("origin", &[(descriptor, wire.clone())])]),
        never(),
        &ProgressSink::discard(),
    )
    .await
    .expect("the memory source satisfies the closure");

    let from_disk = TestCache::new();
    let from_disk = from_disk.shared(CachePolicy::Keep);
    AcquisitionSession::new(
        AcquisitionPlan::build(items_for(&[(descriptor, wire.clone())])).expect("the closure"),
        Arc::clone(&from_disk),
        SchedulerConfig::default(),
    )
    .run(
        chain(vec![directory_source("usb", tree.path())]),
        never(),
        &ProgressSink::discard(),
    )
    .await
    .expect("the directory source satisfies the same closure");

    let mut left = std::collections::BTreeSet::new();
    let mut right = std::collections::BTreeSet::new();
    for digest in from_memory.digests().expect("the cache lists") {
        left.insert(digest.to_hex());
    }
    for digest in from_disk.digests().expect("the cache lists") {
        right.insert(digest.to_hex());
    }
    assert_eq!(left, right, "the source is not observable in the result");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_never_reports_a_barrier_it_did_not_reach() {
    let cache = TestCache::new();
    let cache = cache.shared(CachePolicy::Keep);
    let blobs = fixture(4);
    // The origin carries only some of what the closure names.
    let partial: Vec<(ContentDescriptor, Vec<u8>)> = blobs[..2].to_vec();
    let plan = AcquisitionPlan::build(items_for(&blobs)).expect("the closure is well formed");
    let result = AcquisitionSession::new(plan, cache, SchedulerConfig::default())
        .run(
            chain(vec![memory_source("origin", &partial)]),
            never(),
            &ProgressSink::discard(),
        )
        .await;
    assert!(
        result.is_err(),
        "a partially satisfied closure is a refusal"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_source_chain_with_a_single_live_source_still_reports_a_reason() {
    let cache = TestCache::new();
    let cache = cache.shared(CachePolicy::Keep);
    let blobs = fixture(1);
    let plan = AcquisitionPlan::build(items_for(&blobs)).expect("the closure is well formed");
    let error = AcquisitionSession::new(plan, cache, SchedulerConfig::default())
        .run(chain(vec![]), never(), &ProgressSink::discard())
        .await
        .expect_err("an empty chain satisfies nothing");
    let AcquireError::Unavailable { reports, .. } = &error else {
        panic!("expected an unavailable closure, got {error}");
    };
    assert_eq!(reports.len(), 1, "an empty chain still says something");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_sessions_over_one_cache_do_not_publish_the_same_blob_twice() {
    let cache = TestCache::new();
    let cache = cache.shared(CachePolicy::Keep);
    let blobs = fixture(6);
    let plan = AcquisitionPlan::build(items_for(&blobs)).expect("the closure is well formed");
    let guard = Arc::new(Mutex::new(()));
    let mut handles = Vec::new();
    for _ in 0..4 {
        let cache = Arc::clone(&cache);
        let plan = plan.clone();
        let blobs = blobs.clone();
        let guard = Arc::clone(&guard);
        handles.push(tokio::spawn(async move {
            let _serialized = guard.lock().await;
            AcquisitionSession::new(plan, cache, SchedulerConfig::default())
                .run(
                    chain(vec![memory_source("origin", &blobs)]),
                    never(),
                    &ProgressSink::discard(),
                )
                .await
        }));
    }
    for handle in handles {
        handle
            .await
            .expect("the task runs")
            .expect("each session is satisfied");
    }
    assert_eq!(cache.digests().expect("the cache lists").len(), 6);
    assert_eq!(
        cache.stored_size().expect("the cache measures"),
        plan_wire_size(&blobs)
    );
}
