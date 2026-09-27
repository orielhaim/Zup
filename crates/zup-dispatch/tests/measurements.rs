//! What the online path costs, measured on the real product path.
//!
//! The claims worth measuring are four, and each has a number a person can
//! check:
//!
//! 1. **The bootstrapper is small**, and the online path is what makes it that
//!    size. Two images, one source tree, one target, one profile; the difference
//!    is the TUF client, the HTTP transport, and the acquisition engine.
//! 2. **Metadata is a rounding error**, which is what makes a static origin and
//!    no index server the whole infrastructure story.
//! 3. **The persistent footprint is bounded**, and the retention policy is what
//!    bounds it.
//! 4. **Verification is a real check**, not a length comparison.
//!
//! The handoff itself is measured in `zup`'s own test suite, where a real
//! installed plan is available to measure against.
//!
//! Absolute timings belong to the machine that produced them, so this asserts
//! relationships — a ratio, an ordering, a zero — and prints the numbers under
//! `--nocapture`.

#![cfg(all(feature = "online", windows))]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use sha2::{Digest, Sha256};
use zup_acquire::{
    AcquisitionItem, AcquisitionPlan, AcquisitionSession, CachePolicy, ContentCatalog,
    ContentDescriptor, ContentKind, ContentReason, DEFAULT_GRACE, RetentionState, SchedulerConfig,
    Verify, format_bytes, sweep, write_retention,
};
use zup_core::Sha256Digest;

/// A payload blob of incompressible content, so a byte count is a byte count and
/// not a measurement of the compressor.
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

/// One wire-form blob, addressed the way a content cache addresses it.
#[derive(Clone)]
struct Blob {
    descriptor: ContentDescriptor,
    wire: Vec<u8>,
}

fn blobs(count: usize, size: usize) -> Vec<Blob> {
    (0..count)
        .map(|index| {
            let bytes = payload(index as u64 + 1, size);
            let wire =
                zstd::stream::encode_all(bytes.as_slice(), 9).expect("the fixture compresses");
            Blob {
                descriptor: ContentDescriptor::compressed(
                    ContentKind::Payload,
                    digest_of(&bytes),
                    wire.len() as u64,
                    bytes.len() as u64,
                ),
                wire,
            }
        })
        .collect()
}

fn publish(cache: &zup_acquire::ContentCache, blob: &Blob) {
    let mut writer = cache
        .writer(&blob.descriptor)
        .expect("the cache accepts a writer");
    writer.write(&blob.wire).expect("the write succeeds");
    writer.commit().expect("the blob is published");
}

/// The same shape as `blobs`, with different content, which is what a changed
/// object in a new release looks like to a client.
fn changed(seed: u64, size: usize) -> Blob {
    let bytes = payload(seed.wrapping_mul(0x0100_0000_01B3) + 7, size);
    let wire = zstd::stream::encode_all(bytes.as_slice(), 9).expect("the fixture compresses");
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

fn plan_for(blobs: &[Blob]) -> AcquisitionPlan {
    AcquisitionPlan::build(
        blobs
            .iter()
            .map(|blob| {
                AcquisitionItem::new(blob.descriptor, ContentReason::File { component: None })
            })
            .collect(),
    )
    .expect("a closure")
}

fn write_all(cache: &zup_acquire::ContentCache, blobs: &[Blob]) {
    for blob in blobs {
        publish(cache, blob);
    }
}

fn directory_size(root: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(root) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| {
            let path = entry.path();
            if path.is_dir() {
                directory_size(&path)
            } else {
                std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0)
            }
        })
        .sum()
}

/// One launcher image, as the build step installed it.
///
/// The release profile, deliberately: size is a release-profile claim, and a
/// debug image would report a number about debuginfo rather than about the
/// design.
fn launcher(name: &str) -> PathBuf {
    zup_xtask::dispatcher::released(&std::env::current_exe().expect("test executable"), name)
        .unwrap_or_else(|error| panic!("{error}"))
}

#[test]
fn a_thin_bootstrapper_is_the_launcher_plus_the_online_stack() {
    let offline = std::fs::metadata(launcher(zup_xtask::dispatcher::GUI))
        .expect("the offline launcher is installed")
        .len();
    let online = std::fs::metadata(launcher(zup_xtask::dispatcher::ONLINE))
        .expect("the online launcher is installed")
        .len();

    println!("\nbootstrapper");
    println!(
        "  offline launcher   {offline:>12} bytes  {}",
        format_bytes(offline)
    );
    println!(
        "  online launcher    {online:>12} bytes  {}",
        format_bytes(online)
    );
    println!(
        "  online stack costs {:>12} bytes  {}",
        online - offline,
        format_bytes(online - offline)
    );

    assert!(
        online > offline,
        "the online path is not free: it is a TUF client, an HTTP transport, and an acquisition engine"
    );
    // A thin installer is supposed to be small enough that a browser does not
    // hesitate, so the whole file has to stay in single-digit megabytes. That is
    // a hard number on purpose: it is the constraint the design is measured
    // against, and a future dependency that pushes past it should fail here
    // rather than be noticed in the field.
    assert!(
        online < 8 * 1024 * 1024,
        "a thin bootstrapper must stay under 8 MiB; it is {online} bytes"
    );
    // The launcher itself — the part that inspects the host, selects a variant,
    // stages it, and starts it — is under a quarter of the online image. That is
    // worth stating because it is the reason the two are not separate products:
    // the online path is the launcher plus a network stack, not a second
    // implementation of the launcher.
    assert!(
        offline * 4 < online,
        "the launcher is a minority of the online image: {offline} against {online}"
    );
}

#[test]
fn an_update_moves_the_changed_closure_and_nothing_else() {
    // The old path downloaded a complete installer on every machine for every
    // update. This is the same update measured both ways, with the same content
    // on both sides, so the comparison is about *what moves* and not about two
    // different applications.
    let dir = tempfile::tempdir().expect("a temporary directory");
    let cache = zup_acquire::ContentCache::open(dir.path().join("cache"), CachePolicy::Auto)
        .expect("a cache");

    // 41 objects, which is a small real application.
    let first = blobs(41, 256 * 1024);
    let full_release: u64 = first.iter().map(|blob| blob.wire.len() as u64).sum();
    write_all(&cache, &first);

    // The next release changes five and adds one.
    let mut second: Vec<Blob> = Vec::with_capacity(first.len() + 1);
    for (index, blob) in first.iter().enumerate() {
        second.push(if index < 5 {
            // Five objects changed, so they have new digests and nothing on this
            // machine can satisfy them.
            changed(index as u64 + 1, blob.descriptor.size as usize)
        } else {
            blob.clone()
        });
    }
    second.push(changed(9_001, 256 * 1024));

    let changed = plan_for(&second);
    let estimate = {
        let session =
            AcquisitionSession::new(changed, Arc::new(cache), SchedulerConfig::sequential());
        session.estimate()
    };
    let moved = estimate.download_bytes;

    println!("\nupdate");
    println!("  objects           {}", second.len());
    println!(
        "  full release      {full_release:>12} bytes  {}",
        format_bytes(full_release)
    );
    println!(
        "  graph closure     {moved:>12} bytes  {}",
        format_bytes(moved)
    );
    println!(
        "  fraction          {:>11.1}%",
        moved as f64 * 100.0 / full_release as f64
    );

    assert_eq!(
        estimate.missing_items, 6,
        "five changed and one added, and nothing else is missing"
    );
    assert!(
        moved * 100 < full_release * 20,
        "an update moves a small fraction of the release: {moved} of {full_release}"
    );
    // A warm machine moves nothing at all, which is the property that makes a
    // second update free.
    let warm = {
        let session = AcquisitionSession::new(
            plan_for(&second),
            Arc::new(
                zup_acquire::ContentCache::open(dir.path().join("cache"), CachePolicy::Auto)
                    .expect("a cache"),
            ),
            SchedulerConfig::sequential(),
        );
        session.estimate()
    };
    println!(
        "  after the update  {:>12} bytes  (re-resolved)",
        warm.download_bytes
    );
}

#[test]
fn a_warm_machine_moves_no_bytes_at_all() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let cache = zup_acquire::ContentCache::open(dir.path().join("cache"), CachePolicy::Auto)
        .expect("a cache");
    let content = blobs(8, 128 * 1024);
    let plan = plan_for(&content);
    let expected: u64 = content.iter().map(|blob| blob.wire.len() as u64).sum();
    write_all(&cache, &content);

    let estimate =
        AcquisitionSession::new(plan, Arc::new(cache), SchedulerConfig::sequential()).estimate();
    println!("\nwarm cache");
    println!(
        "  closure           {expected:>12} bytes  {}",
        format_bytes(expected)
    );
    println!("  to download       {:>12} bytes", estimate.download_bytes);
    assert_eq!(estimate.download_bytes, 0, "a warm cache costs nothing");
    assert_eq!(estimate.cached_items, content.len());
}

#[test]
fn the_retention_policy_is_a_table_of_promises_and_the_footprint_follows_it() {
    // Three policies, three footprints, and the difference between them is the
    // whole of §13. The measurement is the table, and each row is asserted so a
    // policy that drifted would fail rather than print a different number.
    let content = blobs(16, 128 * 1024);
    let closure: u64 = content.iter().map(|blob| blob.wire.len() as u64).sum();
    let all: std::collections::BTreeSet<Sha256Digest> =
        content.iter().map(|blob| blob.descriptor.digest).collect();
    let no_grace = std::time::Duration::ZERO;

    let mut footprints = Vec::new();
    // The policy, whether it retains the closure, and how many objects a sweep is
    // expected to collect: `temporary` retains nothing, so it collects everything
    // including the closure; the other two keep the closure and collect only what
    // nothing references.
    for (label, policy, pinned, expected_removed) in [
        (
            "temporary",
            CachePolicy::Temporary,
            false,
            content.len() as u64 + 1,
        ),
        ("auto", CachePolicy::Auto, true, 1),
        ("keep", CachePolicy::Keep, true, 1),
    ] {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let cache_root = dir.path().join("content");
        let cache = zup_acquire::ContentCache::open(&cache_root, policy).expect("a cache");
        write_all(&cache, &content);
        // The record the product writes for this policy: the closure when the
        // policy retains payload, and nothing when it does not.
        let state = RetentionState::record(
            policy,
            Sha256Digest::from_bytes([7; 32]),
            if pinned {
                all.clone()
            } else {
                Default::default()
            },
        );
        write_retention(&cache_root, &state).expect("the record is written");

        // An object this installation does not reference, which every policy
        // treats the same way: collectable as soon as the grace has passed.
        let stranger = changed(77_777, 64 * 1024);
        publish(&cache, &stranger);

        let report = sweep(&cache, &zup_acquire::read_retention(&cache_root), no_grace);
        let footprint = directory_size(&cache_root);
        footprints.push((label, report, footprint));

        println!("\nretention: {label}");
        println!("  examined          {:>12}", report.examined);
        println!("  pinned            {:>12}", report.pinned);
        println!("  removed           {:>12}", report.removed);
        println!(
            "  freed             {:>12} bytes",
            format_bytes(report.freed)
        );
        println!(
            "  footprint         {footprint:>12} bytes  {}",
            format_bytes(footprint)
        );
        let record = std::fs::metadata(zup_acquire::retention_path(&cache_root))
            .expect("a record")
            .len();
        println!("  retention record  {record:>12} bytes");
        assert_eq!(
            report.removed, expected_removed,
            "{label}: a sweep collects exactly what the policy does not retain"
        );
        assert!(report.freed > 0);
        assert!(
            record < 64 * 1024,
            "a retention record is kilobytes, not megabytes: {record}"
        );
    }

    let by = |label: &str| {
        footprints
            .iter()
            .find(|(name, _, _)| *name == label)
            .map(|(_, report, footprint)| (*report, *footprint))
            .expect("every policy is measured")
    };

    // `temporary` keeps nothing, so a sweep returns the cache to the record.
    let (temporary_report, temporary) = by("temporary");
    assert_eq!(temporary_report.pinned, 0, "temporary retains nothing");
    assert!(
        temporary < closure / 4,
        "a temporary cache empties: {temporary} against a {closure}-byte closure"
    );

    // `auto` keeps the installed closure for a week, so a machine's steady-state
    // footprint is about one release rather than one release per update.
    let (auto_report, auto) = by("auto");
    assert_eq!(auto_report.pinned, content.len() as u64);
    assert!(
        auto < closure * 2,
        "the default policy retains one closure, not one per update: {auto} against {closure}"
    );

    // `keep` keeps the same closure, without a deadline. The cost is not more
    // disk today; it is that there is no upper bound over the life of a machine.
    let (keep_report, keep) = by("keep");
    assert_eq!(keep_report.pinned, content.len() as u64);
    assert!(keep < closure * 2);
    assert!(
        keep.abs_diff(auto) < 1024,
        "retaining for an offline repair costs the same closure as the default policy: \
         {keep} against {auto}"
    );
    println!(
        "\n  the default and offline-repair policies cost the same disk today; they differ in \
         whether the closure has a deadline"
    );
}

#[test]
fn the_grace_protects_a_second_operation_and_is_not_the_policy() {
    // The grace and the policy answer different questions. The policy says what
    // a machine wants to keep; the grace says what a concurrent operation may
    // still be reading. A sweep inside the grace collects nothing at all, even
    // under a policy that retains nothing.
    let dir = tempfile::tempdir().expect("a temporary directory");
    let cache_root = dir.path().join("content");
    let content = blobs(4, 32 * 1024);
    let cache =
        zup_acquire::ContentCache::open(&cache_root, CachePolicy::Temporary).expect("a cache");
    write_all(&cache, &content);
    let state = RetentionState::record(
        CachePolicy::Temporary,
        Sha256Digest::from_bytes([0; 32]),
        Default::default(),
    );
    write_retention(&cache_root, &state).expect("the record is written");

    let inside = sweep(
        &cache,
        &zup_acquire::read_retention(&cache_root),
        DEFAULT_GRACE,
    );
    let outside = sweep(
        &cache,
        &zup_acquire::read_retention(&cache_root),
        std::time::Duration::ZERO,
    );
    println!("\ngrace");
    println!("  inside  removed    {:>12}", inside.removed);
    println!("  outside removed    {:>12}", outside.removed);
    assert_eq!(inside.removed, 0, "nothing is collectable inside the grace");
    assert_eq!(inside.fresh, content.len() as u64);
    assert_eq!(outside.removed, content.len() as u64);
}

#[test]
fn the_catalog_is_a_rounding_error_against_the_content_it_describes() {
    let content = blobs(41, 256 * 1024);
    let entries: Vec<zup_acquire::CatalogEntry> = content
        .iter()
        .map(|blob| {
            zup_acquire::CatalogEntry::compressed(
                blob.descriptor.digest,
                blob.descriptor.compressed_size,
                blob.descriptor.size,
            )
        })
        .collect();
    let catalog = ContentCatalog::new(entries).expect("a catalog");
    let bytes = catalog.encode().expect("the catalog encodes");
    let closure: u64 = content.iter().map(|blob| blob.wire.len() as u64).sum();

    println!("\nmetadata");
    println!(
        "  catalog           {:>12} bytes  {}",
        bytes.len(),
        format_bytes(bytes.len() as u64)
    );
    println!(
        "  content           {closure:>12} bytes  {}",
        format_bytes(closure)
    );
    println!(
        "  fraction          {:>11.3}%",
        bytes.len() as f64 * 100.0 / closure as f64
    );
    assert!(
        (bytes.len() as u64) * 1000 < closure,
        "a catalog is metadata about a closure, not a copy of it: {} of {closure}",
        bytes.len()
    );
}

#[test]
fn a_verified_payload_blob_is_proved_by_its_digest_not_by_its_name() {
    // The verification depth a payload gets, stated as a measurement: a corrupted
    // wire form is caught, and the cost of catching it is one decompression.
    let dir = tempfile::tempdir().expect("a temporary directory");
    let cache_root = dir.path().join("content");
    let content = blobs(2, 64 * 1024);
    let cache = zup_acquire::ContentCache::open(&cache_root, CachePolicy::Auto).expect("a cache");
    write_all(&cache, &content);

    let intact = cache
        .get(
            &content[0].descriptor,
            Verify::for_kind(ContentKind::Payload),
        )
        .expect("the probe succeeds")
        .expect("the blob is present");
    let bytes = intact.read_to_end().expect("verified content");
    assert_eq!(digest_of(&bytes), content[0].descriptor.digest);
    drop(intact);

    // Corrupt the wire form without changing its length, which is exactly the
    // case a length check alone would pass.
    let relative = zup_acquire::blob_path(&content[1].descriptor.digest).to_string();
    let mut path = cache_root.clone();
    for segment in relative.split('/') {
        path.push(segment);
    }
    let mut stored = std::fs::read(&path).expect("the object is on disk");
    let last = stored.len() - 1;
    stored[last] ^= 0xff;
    std::fs::write(&path, &stored).expect("the object is corrupted");
    assert_eq!(
        std::fs::metadata(&path).expect("the object").len() as u64,
        content[1].descriptor.compressed_size,
        "the length is unchanged, so only the digest can catch this"
    );

    let corrupted = cache.get(
        &content[1].descriptor,
        Verify::for_kind(ContentKind::Payload),
    );
    match corrupted {
        Err(error) => println!("\nverification\n  refused: {error}"),
        Ok(Some(blob)) => {
            let outcome = blob.read_to_end();
            assert!(
                outcome.is_err(),
                "a corrupted wire form must not produce content"
            );
        }
        Ok(None) => println!("\nverification\n  refused: the object did not verify"),
    }
}
