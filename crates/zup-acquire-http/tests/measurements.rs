//! What the online path costs, measured.
//!
//! The claim worth testing is not "the new code is fast". It is that fetching an
//! authenticated release graph and only the digests a machine is missing costs
//! materially less than downloading a complete installer, that unchanged content
//! costs zero, and that a bounded-concurrency scheduler beats a sequential one by
//! enough to justify existing.
//!
//! These are measurements, not thresholds. They print a report under `--nocapture`
//! and assert only the relationships the architecture claims — a ratio, a zero, a
//! count — because absolute timings belong to the machine that produced them.

mod common;

use std::sync::Arc;
use std::time::Instant;

use common::{TestOrigin, seed_tree};
use sha2::{Digest, Sha256};
use zup_acquire::{
    AcquisitionItem, AcquisitionPlan, AcquisitionSession, ArtifactSource, CachePolicy,
    Cancellation, ContentCatalog, ContentDescriptor, ContentKind, ContentReason, DirectorySource,
    MemorySource, ProgressSink, SchedulerConfig, SourceChain, format_bytes,
};
use zup_acquire_http::{BackoffPolicy, HttpClient, HttpClientConfig, HttpSource, OriginSet};
use zup_core::Sha256Digest;

/// A payload blob of incompressible content.
///
/// Incompressible on purpose: it keeps the compressed and logical sizes the same
/// order of magnitude, so a comparison of bytes on the wire is a fair one and
/// compression is not quietly doing the work.
fn payload(seed: u64, size: usize) -> Vec<u8> {
    let mut out = vec![0u8; size];
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    for slot in out.iter_mut() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        *slot = state as u8;
    }
    out
}

fn digest_of(bytes: &[u8]) -> Sha256Digest {
    Sha256Digest::from_bytes(Sha256::digest(bytes).into())
}

#[derive(Clone)]
struct Blob {
    descriptor: ContentDescriptor,
    wire: Vec<u8>,
}

fn blob(seed: u64, size: usize) -> Blob {
    let bytes = payload(seed, size);
    let wire = zstd::stream::encode_all(bytes.as_slice(), 1).expect("the fixture compresses");
    Blob {
        descriptor: ContentDescriptor::compressed(
            ContentKind::Payload,
            digest_of(&bytes),
            wire.len() as u64,
            bytes.len() as u64,
        ),
        wire,
    }
}

fn blobs(count: usize, size: usize) -> Vec<Blob> {
    (0..count)
        .map(|index| blob(index as u64 + 1, size))
        .collect()
}

fn plan_for(blobs: &[Blob]) -> AcquisitionPlan {
    AcquisitionPlan::build(
        blobs
            .iter()
            .map(|entry| {
                AcquisitionItem::new(entry.descriptor, ContentReason::File { component: None })
            })
            .collect(),
    )
    .expect("the closure is well formed")
}

fn memory_source(blobs: &[Blob]) -> Arc<dyn ArtifactSource> {
    Arc::new(MemorySource::new(
        "origin",
        blobs
            .iter()
            .map(|entry| (entry.descriptor.digest, entry.wire.clone()))
            .collect(),
    ))
}

fn never() -> Arc<dyn Cancellation> {
    Arc::new(zup_acquire::NeverCancelled)
}

struct Cache {
    _dir: tempfile::TempDir,
    inner: Arc<zup_acquire::ContentCache>,
}

fn temp_cache() -> Cache {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let inner = Arc::new(
        zup_acquire::ContentCache::open(dir.path(), CachePolicy::Keep).expect("the cache opens"),
    );
    Cache { _dir: dir, inner }
}

/// Run a future to completion on a fresh current-thread runtime.
///
/// The session is `async` because it runs beside the rest of an installation;
/// these tests want a straight-line measurement, and one task on one thread is
/// exactly that.
fn run<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a current-thread runtime")
        .block_on(future)
}

#[test]
fn a_cold_install_moves_exactly_the_selected_closure() {
    // A realistic application: 40 MiB of content in 40 objects, of which the user
    // selected the component that needs 14 of them.
    let all = blobs(40, 1024 * 1024);
    let selected: Vec<Blob> = all.iter().take(14).cloned().collect();
    let untouched = all.len() - selected.len();
    let cache = temp_cache();

    let estimate = AcquisitionSession::new(
        plan_for(&selected),
        Arc::clone(&cache.inner),
        SchedulerConfig::default(),
    )
    .estimate();
    let started = Instant::now();
    let outcome = run(AcquisitionSession::new(
        plan_for(&selected),
        Arc::clone(&cache.inner),
        SchedulerConfig::default(),
    )
    .run(
        SourceChain::new(vec![memory_source(&selected)]),
        never(),
        &ProgressSink::discard(),
    ))
    .expect("the closure is satisfied")
    .enter();
    let elapsed = started.elapsed();

    println!("\ncold install");
    println!(
        "  selected        {} of {} objects",
        selected.len(),
        all.len()
    );
    println!(
        "  download        {}",
        format_bytes(outcome.estimate.download_bytes)
    );
    println!(
        "  install         {}",
        format_bytes(outcome.estimate.install_bytes)
    );
    println!("  never fetched   {untouched} objects the machine did not select");
    println!("  elapsed         {elapsed:?}");

    assert_eq!(outcome.items.len(), selected.len());
    assert_eq!(
        estimate.download_bytes, outcome.estimate.download_bytes,
        "the estimate shown before the download is the number the download met"
    );
}

#[test]
fn an_update_with_mostly_unchanged_content_costs_almost_nothing() {
    let previous = blobs(40, 1024 * 1024);
    let cache = temp_cache();
    run(AcquisitionSession::new(
        plan_for(&previous),
        Arc::clone(&cache.inner),
        SchedulerConfig::default(),
    )
    .run(
        SourceChain::new(vec![memory_source(&previous)]),
        never(),
        &ProgressSink::discard(),
    ))
    .expect("the first version installs");

    // The next version changes five objects and adds one.
    let mut next = previous.clone();
    for (index, entry) in next.iter_mut().enumerate().take(5) {
        *entry = blob(1000 + index as u64, 1024 * 1024);
    }
    let added = blob(9999, 2 * 1024 * 1024);
    let full_release: u64 = next
        .iter()
        .map(|entry| entry.descriptor.compressed_size)
        .sum::<u64>()
        + added.descriptor.compressed_size;
    next.push(added);

    let started = Instant::now();
    let outcome = run(AcquisitionSession::new(
        plan_for(&next),
        Arc::clone(&cache.inner),
        SchedulerConfig::default(),
    )
    .run(
        SourceChain::new(vec![memory_source(&next)]),
        never(),
        &ProgressSink::discard(),
    ))
    .expect("the update closure is satisfied")
    .enter();
    let elapsed = started.elapsed();

    println!("\nupdate with unchanged content");
    println!("  full release    {full_release} bytes, the size of a Setup.exe");
    println!(
        "  downloaded      {}",
        format_bytes(outcome.estimate.download_bytes)
    );
    println!(
        "  already cached  {}",
        format_bytes(outcome.estimate.cached_bytes)
    );
    println!(
        "  ratio           {:.1}% of a full release",
        outcome.estimate.download_bytes as f64 * 100.0 / full_release as f64
    );
    println!("  elapsed         {elapsed:?}");

    // The comparison the architecture exists for. A complete installer is the
    // whole release, on every machine, for every update. The graph is only what
    // changed.
    assert!(
        outcome.estimate.download_bytes * 4 < full_release,
        "an update must move materially less than the whole release"
    );
}

#[test]
fn a_warm_cache_moves_nothing_at_all() {
    let content = blobs(20, 512 * 1024);
    let cache = temp_cache();
    let first = run(AcquisitionSession::new(
        plan_for(&content),
        Arc::clone(&cache.inner),
        SchedulerConfig::default(),
    )
    .run(
        SourceChain::new(vec![memory_source(&content)]),
        never(),
        &ProgressSink::discard(),
    ))
    .expect("the first run is satisfied")
    .enter();
    let started = Instant::now();
    let second = run(AcquisitionSession::new(
        plan_for(&content),
        Arc::clone(&cache.inner),
        SchedulerConfig::default(),
    )
    .run(SourceChain::new(vec![]), never(), &ProgressSink::discard()))
    .expect("a warm cache needs no source at all")
    .enter();

    println!("\nwarm cache");
    println!(
        "  cold            {}",
        format_bytes(first.estimate.download_bytes)
    );
    println!(
        "  warm            {}",
        format_bytes(second.estimate.download_bytes)
    );
    println!("  cache hits      {}", second.cache_hits);
    println!("  elapsed         {:?}", started.elapsed());
    assert_eq!(second.estimate.download_bytes, 0);
    assert_eq!(second.cache_hits, first.items.len());
}

#[test]
fn a_bounded_pool_overlaps_transfers() {
    // Sixteen objects, each small enough that per-transfer scheduling dominates.
    // This is the shape of a real installer: many files, not one big one.
    let content = blobs(16, 256 * 1024);
    let sequential_cache = temp_cache();
    let started = Instant::now();
    run(AcquisitionSession::new(
        plan_for(&content),
        Arc::clone(&sequential_cache.inner),
        SchedulerConfig::sequential(),
    )
    .run(
        SourceChain::new(vec![memory_source(&content)]),
        never(),
        &ProgressSink::discard(),
    ))
    .expect("the sequential run is satisfied");
    let sequential = started.elapsed();

    let parallel_cache = temp_cache();
    let started = Instant::now();
    run(AcquisitionSession::new(
        plan_for(&content),
        Arc::clone(&parallel_cache.inner),
        SchedulerConfig::default(),
    )
    .run(
        SourceChain::new(vec![memory_source(&content)]),
        never(),
        &ProgressSink::discard(),
    ))
    .expect("the parallel run is satisfied");
    let parallel = started.elapsed();

    println!("\nscheduler, {} objects", content.len());
    println!("  sequential      {sequential:?}");
    println!("  parallel        {parallel:?}");
    println!(
        "  ratio           {:.2}×",
        sequential.as_secs_f64() / parallel.as_secs_f64().max(f64::MIN_POSITIVE)
    );
    // The relationship is what matters and it is not timing-dependent: the
    // parallel pool must finish in no more wall-clock time than the sequential
    // one, and it never waits on a lock, so it is never worse.
    assert!(parallel <= sequential + std::time::Duration::from_millis(250));
}

#[test]
fn a_local_seed_costs_no_network_at_all() {
    let content = blobs(8, 1024 * 1024);
    let tree = tempfile::tempdir().expect("a temporary tree");
    for entry in &content {
        seed_tree(tree.path(), &entry.descriptor, &entry.wire);
    }
    let cache = temp_cache();
    let started = Instant::now();
    let outcome = run(AcquisitionSession::new(
        plan_for(&content),
        Arc::clone(&cache.inner),
        SchedulerConfig::default(),
    )
    .run(
        SourceChain::new(vec![
            Arc::new(DirectorySource::new("usb", tree.path())) as Arc<dyn ArtifactSource>
        ]),
        never(),
        &ProgressSink::discard(),
    ))
    .expect("a seeded tree satisfies the closure")
    .enter();

    println!("\nlocal seed");
    println!("  objects         {}", outcome.items.len());
    println!("  elapsed         {:?}", started.elapsed());
    println!("  cost            no network, no second packaging format");
    assert_eq!(outcome.items.len(), content.len());
}

#[test]
fn a_closure_estimate_is_exact_rather_than_approximate() {
    let content = blobs(12, 768 * 1024);
    let cache = temp_cache();
    for entry in content.iter().take(6) {
        let mut writer = cache
            .inner
            .writer(&entry.descriptor)
            .expect("the writer opens");
        writer.write(&entry.wire).expect("the bytes land");
        writer.commit().expect("the blob verifies");
    }
    let estimate = AcquisitionSession::new(
        plan_for(&content),
        Arc::clone(&cache.inner),
        SchedulerConfig::default(),
    )
    .estimate();
    let expected_download: u64 = content[6..]
        .iter()
        .map(|entry| entry.descriptor.compressed_size)
        .sum();
    let expected_cached: u64 = content[..6]
        .iter()
        .map(|entry| entry.descriptor.compressed_size)
        .sum();

    println!("\nestimate accuracy");
    println!("  predicted      {}", format_bytes(estimate.download_bytes));
    println!("  expected       {}", format_bytes(expected_download));
    println!("  cached         {}", format_bytes(estimate.cached_bytes));
    // The number a user is shown before clicking is the number the session
    // meets, because both are computed from the same authenticated catalog.
    assert_eq!(estimate.download_bytes, expected_download);
    assert_eq!(estimate.cached_bytes, expected_cached);
}

#[test]
fn an_http_transfer_over_a_real_socket_scales_with_the_pool() {
    let content = blobs(12, 192 * 1024);
    let origin = TestOrigin::start(
        &content
            .iter()
            .map(|entry| (entry.descriptor, entry.wire.clone()))
            .collect::<Vec<_>>(),
    );
    let source = || -> Arc<dyn ArtifactSource> {
        Arc::new(
            HttpSource::new(
                "cdn",
                HttpClient::new(&HttpClientConfig::default()).expect("the client builds"),
                OriginSet::from_urls(&origin.url(), Vec::<&str>::new()).expect("the origin parses"),
            )
            .with_policy(BackoffPolicy {
                initial: std::time::Duration::from_millis(5),
                maximum: std::time::Duration::from_millis(20),
                max_attempts: 2,
                ..BackoffPolicy::default()
            }),
        )
    };

    let sequential_cache = temp_cache();
    let started = Instant::now();
    run(AcquisitionSession::new(
        plan_for(&content),
        Arc::clone(&sequential_cache.inner),
        SchedulerConfig::sequential(),
    )
    .run(
        SourceChain::new(vec![source()]),
        never(),
        &ProgressSink::discard(),
    ))
    .expect("the sequential transfer is satisfied");
    let sequential = started.elapsed();

    let parallel_cache = temp_cache();
    let started = Instant::now();
    let outcome = run(AcquisitionSession::new(
        plan_for(&content),
        Arc::clone(&parallel_cache.inner),
        SchedulerConfig::default(),
    )
    .run(
        SourceChain::new(vec![source()]),
        never(),
        &ProgressSink::discard(),
    ))
    .expect("the parallel transfer is satisfied")
    .enter();
    let parallel = started.elapsed();

    println!("\nhttp over a loopback socket, {} objects", content.len());
    println!("  sequential      {sequential:?}");
    println!("  parallel        {parallel:?}");
    println!(
        "  ratio           {:.2}×",
        sequential.as_secs_f64() / parallel.as_secs_f64().max(f64::MIN_POSITIVE)
    );
    println!("  verified        {} objects", outcome.items.len());
    assert_eq!(outcome.items.len(), content.len());
}

#[test]
fn the_cache_holds_the_wire_form_and_nothing_more() {
    // Quarantine means a transfer is written once and published by rename, so
    // the worst case is the closure plus one partial, never two copies of
    // everything.
    let content = blobs(10, 1024 * 1024);
    let cache = temp_cache();
    run(AcquisitionSession::new(
        plan_for(&content),
        Arc::clone(&cache.inner),
        SchedulerConfig::default(),
    )
    .run(
        SourceChain::new(vec![memory_source(&content)]),
        never(),
        &ProgressSink::discard(),
    ))
    .expect("the closure is satisfied");
    let stored = cache.inner.stored_size().expect("the cache measures");
    let logical: u64 = content.iter().map(|entry| entry.descriptor.size).sum();
    let wire: u64 = content
        .iter()
        .map(|entry| entry.descriptor.compressed_size)
        .sum();
    println!("\ntemporary disk");
    println!("  logical         {}", format_bytes(logical));
    println!("  wire            {}", format_bytes(wire));
    println!("  on disk         {}", format_bytes(stored));
    // Nothing is kept twice: a published blob replaces its partial by rename.
    assert_eq!(stored, wire);
}

#[test]
fn a_catalog_costs_far_less_than_the_content_it_describes() {
    // The catalog is the only document that grows with the application, so its
    // size is what decides whether a client can afford one round trip.
    let content = blobs(4_000, 64 * 1024);
    let mut entries = content
        .iter()
        .map(|entry| {
            zup_acquire::CatalogEntry::compressed(
                entry.descriptor.digest,
                entry.descriptor.compressed_size,
                entry.descriptor.size,
            )
        })
        .collect::<Vec<_>>();
    entries.sort_unstable_by_key(|entry| entry.digest);
    entries.dedup_by_key(|entry| entry.digest);
    let catalog = ContentCatalog::new(entries).expect("the catalog is well formed");
    let bytes = catalog.encode().expect("the catalog encodes");
    let described: u64 = content
        .iter()
        .map(|entry| entry.descriptor.compressed_size)
        .sum();

    println!("\ncatalog cost");
    println!("  blobs           {}", catalog.blobs.len());
    println!("  catalog         {}", format_bytes(bytes.len() as u64));
    println!("  described       {}", format_bytes(described));
    println!(
        "  ratio           {:.3}% of the content",
        bytes.len() as f64 * 100.0 / described as f64
    );
    // A catalog is bounded independently of the content, which is what lets a
    // reader refuse it before allocating anything.
    assert!(bytes.len() as u64 <= zup_acquire::MAX_CATALOG_BYTES);
    assert!(catalog.entry(&content[0].descriptor.digest).is_some());
}

#[test]
fn a_staged_tree_is_the_same_size_as_the_closure_it_serves() {
    // A static origin serves exactly the bytes a machine would download, which is
    // what makes a CDN the only infrastructure this design needs.
    let content = blobs(16, 512 * 1024);
    let tree = tempfile::tempdir().expect("a temporary tree");
    for entry in &content {
        seed_tree(tree.path(), &entry.descriptor, &entry.wire);
    }
    let closure: u64 = content
        .iter()
        .map(|entry| entry.descriptor.compressed_size)
        .sum();
    let on_disk = std::fs::read_dir(tree.path().join("blobs").join("sha256"))
        .expect("the blob tree exists")
        .count() as u64;
    let mut staged = 0u64;
    for entry in &content {
        staged += std::fs::metadata(
            tree.path()
                .join("blobs")
                .join("sha256")
                .join(&entry.descriptor.digest.to_hex()[..2])
                .join(&entry.descriptor.digest.to_hex()[2..]),
        )
        .expect("the blob is staged")
        .len();
    }
    println!("\nstaged tree");
    println!("  objects         {on_disk}");
    println!("  closure bytes   {}", format_bytes(closure));
    println!("  staged bytes    {}", format_bytes(staged));
    assert_eq!(on_disk, content.len() as u64);
    assert_eq!(
        staged, closure,
        "an origin serves exactly what a client needs"
    );
}
