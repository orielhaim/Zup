mod common;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use common::*;
use rstest::rstest;
use tokio::sync::{Semaphore, mpsc};
use zup_acquire::*;
use zup_core::Sha256Digest;

#[derive(Clone, Copy, Debug)]
enum Fault {
    Source,
    Panic,
    Stage,
    None,
}

struct Live<'a>(&'a AtomicUsize, &'a Semaphore);

impl<'a> Live<'a> {
    fn enter(active: &'a AtomicUsize, settled: &'a Semaphore) -> Self {
        active.fetch_add(1, Ordering::SeqCst);
        Self(active, settled)
    }
}

impl Drop for Live<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
        self.1.add_permits(1);
    }
}

struct Transfer {
    bytes: Vec<u8>,
    release: Semaphore,
}

struct ControlledSource {
    transfers: BTreeMap<Sha256Digest, Transfer>,
    entered: mpsc::Sender<Sha256Digest>,
    active: AtomicUsize,
    settled: Semaphore,
    fault: Fault,
    failing: Sha256Digest,
}

impl ControlledSource {
    fn new(
        blobs: &[(ContentDescriptor, Vec<u8>)],
        fault: Fault,
    ) -> (Arc<Self>, mpsc::Receiver<Sha256Digest>) {
        let (entered, receiver) = mpsc::channel(blobs.len());
        (
            Arc::new(Self {
                transfers: blobs
                    .iter()
                    .map(|(d, bytes)| {
                        (
                            d.digest,
                            Transfer {
                                bytes: bytes.clone(),
                                release: Semaphore::new(0),
                            },
                        )
                    })
                    .collect(),
                entered,
                active: AtomicUsize::new(0),
                settled: Semaphore::new(0),
                fault,
                failing: blobs[0].0.digest,
            }),
            receiver,
        )
    }

    fn release(&self, digest: Sha256Digest) {
        self.transfers[&digest].release.add_permits(1);
    }
}

impl ArtifactSource for ControlledSource {
    fn name(&self) -> &str {
        "controlled"
    }
    fn contains(&self, descriptor: &ContentDescriptor) -> bool {
        self.transfers.contains_key(&descriptor.digest)
    }
    fn acquire<'a>(
        &'a self,
        request: AcquireRequest<'a>,
    ) -> SourceFuture<'a, Result<VerifiedBlob, SourceError>> {
        Box::pin(async move {
            let _live = Live::enter(&self.active, &self.settled);
            let transfer = &self.transfers[&request.descriptor.digest];
            let mut writer = request.cache.writer(request.descriptor).unwrap();
            let prefix = transfer.bytes.len() / 2;
            let offset = writer.wire_offset() as usize;
            if offset < prefix {
                writer.write(&transfer.bytes[offset..prefix]).unwrap();
            }
            self.entered.try_send(request.descriptor.digest).unwrap();
            let _release = transfer.release.acquire().await.unwrap();
            if request.descriptor.digest == self.failing {
                match self.fault {
                    Fault::Source => {
                        return Err(SourceError::unavailable(
                            self.name(),
                            request.descriptor,
                            retry_detail(2, Duration::from_millis(17), "fixture source failed"),
                        ));
                    }
                    Fault::Panic => panic!("fixture worker panic"),
                    _ => {}
                }
            }
            let offset = writer.wire_offset() as usize;
            writer.write(&transfer.bytes[offset..]).unwrap();
            Ok(writer.commit().unwrap())
        })
    }
}

struct ControlledStager {
    entered: mpsc::Sender<Sha256Digest>,
    release: Semaphore,
    active: AtomicUsize,
    settled: Semaphore,
    seen: std::sync::Mutex<Vec<Sha256Digest>>,
}

impl ControlledStager {
    fn new(count: usize) -> (Arc<Self>, mpsc::Receiver<Sha256Digest>) {
        let (entered, receiver) = mpsc::channel(count);
        (
            Arc::new(Self {
                entered,
                release: Semaphore::new(0),
                active: AtomicUsize::new(0),
                settled: Semaphore::new(0),
                seen: std::sync::Mutex::new(Vec::new()),
            }),
            receiver,
        )
    }
}

impl BlobStager for ControlledStager {
    fn prepare(&self) -> StagerFuture<'_, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
    fn stage(&self, blob: VerifiedBlob) -> StagerFuture<'_, Result<(), String>> {
        Box::pin(async move {
            let _live = Live::enter(&self.active, &self.settled);
            // Reading logical bytes here also checks the handoff is verified.
            blob.read_to_end().unwrap();
            self.entered.try_send(blob.descriptor.digest).unwrap();
            self.release.acquire().await.unwrap().forget();
            self.seen.lock().unwrap().push(blob.descriptor.digest);
            Ok(())
        })
    }
}

fn blobs(count: usize) -> Vec<(ContentDescriptor, Vec<u8>)> {
    (0..count)
        .map(|n| {
            let bytes = payload(n as u8 + 70, 2048);
            (document_descriptor(ContentKind::Runtime, &bytes), bytes)
        })
        .collect()
}

fn plan(blobs: &[(ContentDescriptor, Vec<u8>)]) -> AcquisitionPlan {
    if blobs.is_empty() {
        return AcquisitionPlan::empty();
    }
    AcquisitionPlan::build(
        blobs
            .iter()
            .map(|(d, _)| AcquisitionItem::new(*d, ContentReason::Runtime))
            .collect(),
    )
    .unwrap()
}

fn events(receiver: &mut mpsc::Receiver<AcquisitionEvent>) -> Vec<AcquisitionEvent> {
    let mut events = Vec::new();
    while let Ok(event) = receiver.try_recv() {
        events.push(event);
    }
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(
                e,
                AcquisitionEvent::Failed { .. }
                    | AcquisitionEvent::Cancelled { .. }
                    | AcquisitionEvent::AcquisitionComplete { .. }
            ))
            .count(),
        1
    );
    assert!(matches!(
        events.first(),
        Some(AcquisitionEvent::AcquisitionStarted { .. })
    ));
    events
}

fn assert_settled(events: &[AcquisitionEvent]) {
    let sample = events
        .iter()
        .rev()
        .find_map(|e| match e {
            AcquisitionEvent::DownloadProgress { progress } => Some(progress),
            _ => None,
        })
        .unwrap();
    assert_eq!(sample.active_transfers, 0);
}

async fn bounded<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(3), future)
        .await
        .expect("session must settle")
}

#[rstest]
#[case(Fault::Source)]
#[case(Fault::Panic)]
#[case(Fault::Stage)]
#[tokio::test]
async fn failure_settles_peers_and_releases_partial_writers(#[case] fault: Fault) {
    bounded(async {
        let root = TestCache::new();
        let cache = root.shared(CachePolicy::Keep);
        let blobs = blobs(8);
        let (source, mut entered) = ControlledSource::new(&blobs, fault);
        let (sink, mut receiver) = ProgressSink::channel(64);
        let session = AcquisitionSession::new(plan(&blobs), cache.clone(), SchedulerConfig { total: 4, ..SchedulerConfig::default() })
            .with_stager(RecordingStager::failing(blobs[0].0.digest));
        let handle = tokio::spawn({ let source = source.clone(); async move {
            session.run(SourceChain::new(vec![source]), never(), &sink).await
        }});
        let mut started = Vec::new();
        for _ in 0..4 { started.push(entered.recv().await.unwrap()); }
        assert!(started.contains(&blobs[0].0.digest));
        source.release(blobs[0].0.digest);
        let error = handle.await.unwrap().unwrap_err();
        match fault {
            Fault::Source => assert!(matches!(error, AcquireError::Unavailable { .. })),
            Fault::Panic => assert!(matches!(error, AcquireError::Worker(_))),
            Fault::Stage => assert!(matches!(error, AcquireError::Staging { .. })),
            Fault::None => unreachable!(),
        }
        assert_eq!(source.active.load(Ordering::SeqCst), 0);
        assert!(entered.try_recv().is_err(), "shutdown must stop dispatching queued items");
        let events = events(&mut receiver);
        assert_settled(&events);
        assert!(matches!(events.last(), Some(AcquisitionEvent::Failed { .. })));
        if matches!(fault, Fault::Source) {
            assert!(events.iter().any(|e| matches!(e, AcquisitionEvent::Retrying { attempt: 2, delay_ms: 17, reason, .. } if reason == "fixture source failed")));
            assert!(matches!(events.last(), Some(AcquisitionEvent::Failed { reasons, .. }) if reasons.iter().any(|r| r.contains("fixture source failed"))));
        }
        for digest in started.into_iter().filter(|d| *d != blobs[0].0.digest) {
            let descriptor = &blobs.iter().find(|(d, _)| d.digest == digest).unwrap().0;
            assert!(cache.get(descriptor, Verify::Full).unwrap().is_none());
            assert_eq!(cache.writer(descriptor).unwrap().wire_offset(), 1024);
        }
        // A fresh acquisition on the same cache must reuse partials and acquire
        // locks immediately, without relying on the previous runtime shutting down.
        let (retry, _retry_events) = ControlledSource::new(&blobs, Fault::None);
        for (descriptor, _) in &blobs { retry.release(descriptor.digest); }
        let outcome = AcquisitionSession::new(plan(&blobs), cache, SchedulerConfig::default())
            .run(SourceChain::new(vec![retry.clone()]), never(), &ProgressSink::discard()).await.unwrap().enter();
        assert_eq!(outcome.len(), blobs.len());
        assert_eq!(retry.active.load(Ordering::SeqCst), 0);
        for (descriptor, bytes) in &blobs { assert_eq!(outcome.get(&descriptor.digest).unwrap().read_to_end().unwrap(), *bytes); }
    }).await;
}

#[rstest]
#[case(true)]
#[case(false)]
#[tokio::test]
async fn cancellation_interrupts_pending_transfers_and_preserves_partials(#[case] wakeable: bool) {
    bounded(async {
        let root = TestCache::new();
        let cache = root.shared(CachePolicy::Keep);
        let blobs = blobs(4);
        let (source, mut entered) = ControlledSource::new(&blobs, Fault::None);
        let flag = Arc::new(CancelFlag::new());
        let cancellation: Arc<dyn Cancellation> = if wakeable {
            flag.clone()
        } else {
            let flag = flag.clone();
            Arc::new(move || flag.is_cancelled())
        };
        let (sink, mut receiver) = ProgressSink::channel(64);
        let handle = tokio::spawn({
            let source = source.clone();
            let cache = cache.clone();
            let plan = plan(&blobs);
            async move {
                AcquisitionSession::new(plan, cache, SchedulerConfig::default())
                    .run(SourceChain::new(vec![source]), cancellation, &sink)
                    .await
            }
        });
        for _ in 0..4 {
            entered.recv().await.unwrap();
        }
        flag.cancel();
        assert!(matches!(
            handle.await.unwrap(),
            Err(AcquireError::Cancelled)
        ));
        assert_eq!(source.active.load(Ordering::SeqCst), 0);
        for (descriptor, _) in &blobs {
            assert!(cache.get(descriptor, Verify::Full).unwrap().is_none());
            assert_eq!(cache.writer(descriptor).unwrap().wire_offset(), 1024);
        }
        let events = events(&mut receiver);
        assert_settled(&events);
        assert!(matches!(
            events.last(),
            Some(AcquisitionEvent::Cancelled {
                completed_items: 0,
                total_items: 4
            })
        ));
    })
    .await;
}

#[rstest]
#[case(true)]
#[case(false)]
#[tokio::test]
async fn staging_settles_before_terminal_and_does_not_gate_network(#[case] cancel: bool) {
    bounded(async {
        let root = TestCache::new();
        let cache = root.shared(CachePolicy::Keep);
        let blobs = blobs(3);
        let (source, mut transfers) = ControlledSource::new(&blobs, Fault::None);
        let (stager, mut stages) = ControlledStager::new(blobs.len());
        let flag = Arc::new(CancelFlag::new());
        let (sink, mut receiver) = ProgressSink::channel(64);
        let handle = tokio::spawn({
            let source = source.clone();
            let stager = stager.clone();
            let flag = flag.clone();
            let plan = plan(&blobs);
            let cache = cache.clone();
            async move {
                AcquisitionSession::new(
                    plan,
                    cache,
                    SchedulerConfig {
                        total: 3,
                        staging: 1,
                        ..SchedulerConfig::default()
                    },
                )
                .with_stager(stager)
                .run(SourceChain::new(vec![source]), flag, &sink)
                .await
            }
        });
        for _ in 0..3 {
            source.release(transfers.recv().await.unwrap());
        }
        // Every transfer must finish even with the single staging permit held.
        source.settled.acquire_many(3).await.unwrap().forget();
        stages.recv().await.unwrap();
        assert_eq!(stager.active.load(Ordering::SeqCst), 1);
        assert!(stages.try_recv().is_err());
        if cancel {
            flag.cancel();
        }
        assert!(
            !handle.is_finished(),
            "a running stager owns unfinished effects"
        );
        if cancel {
            stager.release.add_permits(1);
        } else {
            stager.release.add_permits(3);
        }
        let result = handle.await.unwrap();
        let seen = stager.seen.lock().unwrap().clone();
        assert_eq!(stager.active.load(Ordering::SeqCst), 0);
        assert_eq!(source.active.load(Ordering::SeqCst), 0);
        for (descriptor, bytes) in &blobs {
            assert_eq!(
                cache
                    .get(descriptor, Verify::Full)
                    .unwrap()
                    .unwrap()
                    .read_to_end()
                    .unwrap(),
                *bytes
            );
        }
        let events = events(&mut receiver);
        assert_settled(&events);
        if cancel {
            assert!(matches!(result, Err(AcquireError::Cancelled)));
            assert_eq!(seen.len(), 1);
            assert!(matches!(
                events.last(),
                Some(AcquisitionEvent::Cancelled {
                    completed_items: 1,
                    total_items: 3
                })
            ));
        } else {
            let outcome = result.unwrap().enter();
            assert_eq!(outcome.len(), 3);
            let unique: std::collections::BTreeSet<_> = seen.iter().collect();
            assert_eq!(unique.len(), 3);
            assert_eq!(seen.len(), 3);
            assert!(matches!(
                events.last(),
                Some(AcquisitionEvent::AcquisitionComplete {
                    items: 3,
                    bytes: 6144,
                    ..
                })
            ));
        }
    })
    .await;
}

#[rstest]
#[case(0)]
#[case(3)]
#[tokio::test]
async fn cached_closures_with_no_downloads_have_one_terminal_event(#[case] count: usize) {
    let root = TestCache::new();
    let cache = root.shared(CachePolicy::Keep);
    let blobs = blobs(count);
    for (descriptor, bytes) in &blobs {
        let mut writer = cache.writer(descriptor).unwrap();
        writer.write(bytes).unwrap();
        writer.commit().unwrap();
    }
    let (sink, mut receiver) = ProgressSink::channel(64);
    let outcome = bounded(
        AcquisitionSession::new(plan(&blobs), cache, SchedulerConfig::default()).run(
            SourceChain::new(vec![]),
            never(),
            &sink,
        ),
    )
    .await
    .unwrap()
    .enter();
    assert_eq!(outcome.cache_hits, count);
    assert!(
        matches!(events(&mut receiver).last(), Some(AcquisitionEvent::AcquisitionComplete { items, .. }) if *items == count as u64)
    );
}

#[rstest]
#[case(false)]
#[case(true)]
#[tokio::test]
async fn shutdown_latches_the_first_outcome_and_settles_active_staging(#[case] cancel_first: bool) {
    bounded(async {
        let root = TestCache::new();
        let blobs = blobs(3);
        let (source, mut transfers) = ControlledSource::new(&blobs, Fault::Source);
        let (stager, mut stages) = ControlledStager::new(3);
        let flag = Arc::new(CancelFlag::new());
        let (sink, mut receiver) = ProgressSink::channel(64);
        let session = AcquisitionSession::new(plan(&blobs), root.shared(CachePolicy::Keep), SchedulerConfig::default()).with_stager(stager.clone());
        let handle = tokio::spawn({ let source = source.clone(); let flag = flag.clone(); async move {
            session.run(SourceChain::new(vec![source]), flag, &sink).await
        }});
        for _ in 0..3 { transfers.recv().await.unwrap(); }
        source.release(blobs[1].0.digest);
        assert_eq!(stages.recv().await.unwrap(), blobs[1].0.digest);
        if cancel_first { flag.cancel(); } else { source.release(blobs[0].0.digest); }
        // The untouched third transfer can settle only after the owner has
        // observed the initiating outcome and signalled shutdown.
        source.settled.acquire_many(3).await.unwrap().forget();
        assert!(!handle.is_finished());
        flag.cancel();
        stager.release.add_permits(1);
        let result = handle.await.unwrap();
        assert_eq!(stager.seen.lock().unwrap().as_slice(), &[blobs[1].0.digest]);
        assert_eq!(stager.active.load(Ordering::SeqCst), 0);
        let events = events(&mut receiver);
        assert_settled(&events);
        if cancel_first {
            assert!(matches!(result, Err(AcquireError::Cancelled)));
            assert!(matches!(events.last(), Some(AcquisitionEvent::Cancelled { completed_items: 1, .. })));
        } else {
            assert!(matches!(result, Err(AcquireError::Unavailable { .. })));
            assert!(matches!(events.last(), Some(AcquisitionEvent::Failed { kind: "unavailable", digest: Some(digest), .. }) if *digest == blobs[0].0.digest.to_hex()));
        }
    }).await;
}

#[tokio::test]
async fn ready_failures_and_a_full_observer_channel_do_not_strand_workers() {
    bounded(async {
        let root = TestCache::new();
        let (sink, mut receiver) = ProgressSink::channel(1);
        let session = AcquisitionSession::new(
            plan(&blobs(48)),
            root.shared(CachePolicy::Keep),
            SchedulerConfig {
                total: 12,
                ..SchedulerConfig::default()
            },
        );
        let handle = tokio::spawn(async move {
            session
                .run(
                    SourceChain::new(vec![FailingSource::new("offline", "unreachable")]),
                    never(),
                    &sink,
                )
                .await
        });
        assert!(matches!(
            receiver.recv().await,
            Some(AcquisitionEvent::AcquisitionStarted { .. })
        ));
        let mut terminals = 0;
        while let Some(event) = receiver.recv().await {
            match event {
                AcquisitionEvent::Failed { .. } => terminals += 1,
                AcquisitionEvent::AcquisitionComplete { .. }
                | AcquisitionEvent::Cancelled { .. } => panic!("incorrect terminal event"),
                _ => {}
            }
        }
        assert!(matches!(
            handle.await.unwrap(),
            Err(AcquireError::Unavailable { .. })
        ));
        assert_eq!(terminals, 1);
    })
    .await;
}

#[tokio::test]
async fn invalid_sampling_configuration_reports_failure_without_starting_work() {
    let root = TestCache::new();
    let (sink, mut receiver) = ProgressSink::channel(4);
    let result = AcquisitionSession::new(
        AcquisitionPlan::empty(),
        root.shared(CachePolicy::Keep),
        SchedulerConfig {
            progress_interval: Duration::ZERO,
            ..SchedulerConfig::default()
        },
    )
    .run(SourceChain::new(vec![]), never(), &sink)
    .await;
    assert!(matches!(result, Err(AcquireError::Descriptor(_))));
    assert!(matches!(
        receiver.try_recv(),
        Ok(AcquisitionEvent::Failed {
            kind: "descriptor",
            ..
        })
    ));
    assert!(receiver.try_recv().is_err());
}

#[tokio::test]
async fn cancellation_waits_for_started_blocking_staging_writes() {
    struct BlockingStager {
        path: std::path::PathBuf,
        started: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
        release: Arc<std::sync::Mutex<std::sync::mpsc::Receiver<()>>>,
    }
    impl BlobStager for BlockingStager {
        fn prepare(&self) -> StagerFuture<'_, Result<(), String>> {
            Box::pin(async { Ok(()) })
        }
        fn stage(&self, blob: VerifiedBlob) -> StagerFuture<'_, Result<(), String>> {
            Box::pin(async move {
                let started = self.started.lock().unwrap().take().unwrap();
                let release = self.release.clone();
                let path = self.path.clone();
                tokio::task::spawn_blocking(move || {
                    started.send(()).unwrap();
                    release.lock().unwrap().recv().unwrap();
                    std::fs::write(path, blob.read_to_end().unwrap()).unwrap();
                })
                .await
                .map_err(|e| e.to_string())
            })
        }
    }
    bounded(async {
        let root = TestCache::new();
        let output = tempfile::tempdir().unwrap();
        let path = output.path().join("staged");
        let blobs = blobs(1);
        let (release, released) = std::sync::mpsc::channel();
        let (started, ready) = tokio::sync::oneshot::channel();
        let stager = Arc::new(BlockingStager {
            path: path.clone(),
            started: std::sync::Mutex::new(Some(started)),
            release: Arc::new(std::sync::Mutex::new(released)),
        });
        let session = AcquisitionSession::new(
            plan(&blobs),
            root.shared(CachePolicy::Keep),
            SchedulerConfig::sequential(),
        )
        .with_stager(stager);
        let flag = Arc::new(CancelFlag::new());
        let (sink, mut receiver) = ProgressSink::channel(64);
        let source = memory_source("memory", &blobs);
        let handle = tokio::spawn({
            let flag = flag.clone();
            async move {
                session
                    .run(SourceChain::new(vec![source]), flag, &sink)
                    .await
            }
        });
        ready.await.unwrap();
        flag.cancel();
        assert!(!handle.is_finished());
        assert!(!path.exists());
        release.send(()).unwrap();
        assert!(matches!(
            handle.await.unwrap(),
            Err(AcquireError::Cancelled)
        ));
        assert_eq!(std::fs::read(path).unwrap(), blobs[0].1);
        let events = events(&mut receiver);
        assert_settled(&events);
        assert!(matches!(
            events.last(),
            Some(AcquisitionEvent::Cancelled {
                completed_items: 1,
                ..
            })
        ));
    })
    .await;
}
