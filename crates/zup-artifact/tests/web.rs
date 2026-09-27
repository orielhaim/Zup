//! From a real build to a static web tree to a verified content cache.
//!
//! This is the end-to-end claim the whole online story rests on: one
//! `zup build` produces a graph, `zup publish stage` turns that graph into a
//! directory a static host can serve, and a client reads the same closure back
//! out of it — with nothing but a digest as proof that the bytes it received
//! are the bytes that were built.
//!
//! No HTTP, no TUF, and no platform API is involved. That is the point: the
//! layout is a file tree, and the identity is arithmetic.

mod common;

use std::sync::Arc;

use common::{ARM64, X64, build_target};
use zup_acquire::{
    AcquireError, AcquisitionItem, AcquisitionPlan, AcquisitionSession, ArtifactSource,
    CachePolicy, ContentCatalog, ContentDescriptor, ContentKind, ContentReason, DirectorySource,
    ProgressSink, ReleaseDescriptor, SchedulerConfig, SourceChain, Verify,
};
use zup_artifact::{ArtifactComposer, ArtifactRequest, WebExport, export_web_tree};

fn compose(root: &std::path::Path) -> zup_artifact::ArtifactGraph {
    let x64 = build_target(root.join("x64"), &X64);
    let arm = build_target(root.join("arm64"), &ARM64);
    let request = ArtifactRequest::universal_offline(
        "acme-windows",
        &common::app(),
        "Acme-Windows-Setup.exe",
    );
    ArtifactComposer::new(request, &[&x64, &arm])
        .expect("the request is well formed")
        .compose(&[&x64, &arm])
        .expect("the graph composes")
}

fn stage(root: &std::path::Path, graph: &zup_artifact::ArtifactGraph) -> zup_artifact::WebTree {
    let export = WebExport::new("stable").expect("a channel name");
    export_web_tree(graph, &export, &root.join("web")).expect("the tree is written")
}

#[test]
fn a_build_stages_a_complete_immutable_web_tree() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let graph = compose(dir.path());
    let tree = stage(dir.path(), &graph);
    let web = dir.path().join("web");

    // Every blob in the composed store is present, named by its digest, plus one
    // native runtime per variant.
    assert_eq!(
        tree.blob_count,
        graph.table().entries().count() as u64 + graph.index().variants.len() as u64,
        "every unique blob plus every native runtime is an object"
    );
    assert!(tree.blob_bytes > 0);
    for entry in graph.table().entries() {
        let hex = entry.digest.to_hex();
        let path = web
            .join("blobs")
            .join("sha256")
            .join(&hex[..2])
            .join(&hex[2..]);
        assert!(path.is_file(), "{} is missing", path.display());
        assert_eq!(
            std::fs::metadata(&path)
                .expect("the blob is readable")
                .len(),
            entry.compressed_size,
            "a blob's wire length is the length the graph composed"
        );
    }

    // The authenticated documents are present and parse.
    assert_eq!(tree.variant_count, 2);
    let release_bytes =
        std::fs::read(web.join("releases").join("stable.json")).expect("the release is staged");
    let release = ReleaseDescriptor::parse(&release_bytes).expect("the release parses");
    release.validate().expect("the release is well formed");
    assert_eq!(release.release_digest, tree.release_digest);
    assert_eq!(release.channel, "stable");
    assert_eq!(release.app_id.as_str(), "com.acme.desktop");
    assert_eq!(release.version, "1.4.0");
    assert_eq!(release.variant_ids(), vec!["windows-arm64", "windows-x64"]);

    // The catalog describes every object the origin can serve: the composed
    // blobs plus one native runtime per variant, so a client knows every wire
    // length before it fetches a byte.
    let catalog_bytes = std::fs::read(web.join("releases").join("stable").join("catalog.json"))
        .expect("the catalog is staged");
    let catalog = ContentCatalog::parse(&catalog_bytes).expect("the catalog parses");
    assert_eq!(
        catalog.blobs.len(),
        graph.table().entries().count() + graph.index().variants.len()
    );
    for entry in graph.table().entries() {
        let catalogued = catalog.entry(&entry.digest).expect("catalogued");
        assert_eq!(
            catalogued.compressed_size, entry.compressed_size,
            "the catalog states the wire length the graph composed"
        );
        assert_eq!(catalogued.size, entry.size);
    }

    // A TUF input tree exists, so signing is a `tuftool` invocation away and no
    // key ever passes through zup.
    assert!(!tree.tuf_targets.is_empty());
    for target in &tree.tuf_targets {
        assert!(
            web.join("tuf-input").join(target).is_file(),
            "the TUF input tree is missing {target}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_staged_tree_is_a_valid_local_source_for_a_client() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let graph = compose(dir.path());
    stage(dir.path(), &graph);
    let web = dir.path().join("web");

    // A client reads the release, picks one variant, and asks for exactly the
    // content that variant names. Nothing else in the tree is touched.
    let release = ReleaseDescriptor::parse(
        &std::fs::read(web.join("releases").join("stable.json")).expect("the release is staged"),
    )
    .expect("the release parses");
    let catalog = ContentCatalog::parse(
        &std::fs::read(web.join("releases").join("stable").join("catalog.json"))
            .expect("the catalog is staged"),
    )
    .expect("the catalog parses");
    let variant = release
        .variant("windows-x64")
        .expect("the x64 variant is in the release");

    let mut items = Vec::new();
    for digest in &variant.content {
        let entry = catalog.entry(digest).expect("the catalog describes it");
        items.push(AcquisitionItem::new(
            entry.descriptor(ContentKind::Payload),
            ContentReason::File { component: None },
        ));
    }
    let runtime = release
        .runtime_descriptor(variant)
        .expect("an online release names the runtime");
    assert_eq!(runtime.kind(), ContentKind::Runtime);
    let plan = AcquisitionPlan::build(items).expect("the closure is well formed");

    let cache_dir = tempfile::tempdir().expect("a temporary cache");
    let cache = Arc::new(
        zup_acquire::ContentCache::open(cache_dir.path(), CachePolicy::Keep)
            .expect("the cache opens"),
    );
    let outcome = AcquisitionSession::new(plan, Arc::clone(&cache), SchedulerConfig::default())
        .run(
            SourceChain::new(vec![
                Arc::new(DirectorySource::new("origin", &web)) as Arc<dyn ArtifactSource>
            ]),
            Arc::new(zup_acquire::NeverCancelled),
            &ProgressSink::discard(),
        )
        .await
        .expect("the staged tree satisfies the closure")
        .enter();

    // Only the selected variant's content arrived. The other architecture's
    // blobs are still just files on a disk nobody read.
    assert_eq!(outcome.items.len(), variant.content.len());
    let arm = release
        .variant("windows-arm64")
        .expect("the arm64 variant is in the release");
    let arm_only = arm
        .content
        .iter()
        .find(|digest| !variant.content.contains(digest))
        .expect("the two variants differ");
    assert!(
        cache
            .get(
                &ContentDescriptor::compressed(
                    ContentKind::Payload,
                    *arm_only,
                    catalog.entry(arm_only).expect("catalogued").compressed_size,
                    catalog.entry(arm_only).expect("catalogued").size,
                ),
                Verify::WireLength,
            )
            .expect("the cache probes")
            .is_none(),
        "the other architecture's content was never fetched"
    );

    // Every blob the closure asked for decodes back to its content.
    for blob in &outcome.items {
        assert!(
            !blob.read_to_end().expect("the blob decodes").is_empty(),
            "every blob the closure named decodes to real content"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_client_with_a_warm_cache_moves_no_bytes() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let graph = compose(dir.path());
    stage(dir.path(), &graph);
    let web = dir.path().join("web");
    let release = ReleaseDescriptor::parse(
        &std::fs::read(web.join("releases").join("stable.json")).expect("the release is staged"),
    )
    .expect("the release parses");
    let variant = release.variant("windows-x64").expect("a variant");
    let plan = AcquisitionPlan::for_variant(
        &ContentCatalog::parse(
            &std::fs::read(web.join("releases").join("stable").join("catalog.json"))
                .expect("the catalog is staged"),
        )
        .expect("the catalog parses"),
        &variant.content,
        ContentKind::Payload,
    )
    .expect("the closure is well formed");

    let cache_dir = tempfile::tempdir().expect("a temporary cache");
    let cache = Arc::new(
        zup_acquire::ContentCache::open(cache_dir.path(), CachePolicy::Keep)
            .expect("the cache opens"),
    );
    let source = || -> Arc<dyn ArtifactSource> { Arc::new(DirectorySource::new("origin", &web)) };
    let cancellation =
        || Arc::new(zup_acquire::NeverCancelled) as Arc<dyn zup_acquire::Cancellation>;

    let first =
        AcquisitionSession::new(plan.clone(), Arc::clone(&cache), SchedulerConfig::default())
            .run(
                SourceChain::new(vec![source()]),
                cancellation(),
                &ProgressSink::discard(),
            )
            .await
            .expect("the first run is satisfied")
            .enter();
    assert_eq!(first.cache_hits, 0);

    // A second machine, or the same one after a repair, needs nothing: the cache
    // is keyed by content, so the tree is not consulted at all.
    let second = AcquisitionSession::new(plan, Arc::clone(&cache), SchedulerConfig::default())
        .run(
            SourceChain::new(vec![]),
            cancellation(),
            &ProgressSink::discard(),
        )
        .await
        .expect("a warm cache needs no source")
        .enter();
    assert_eq!(second.cache_hits, first.items.len());
    assert_eq!(second.estimate.download_bytes, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_corrupt_blob_in_the_staged_tree_is_refused_rather_than_installed() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let graph = compose(dir.path());
    stage(dir.path(), &graph);
    let web = dir.path().join("web");
    let release = ReleaseDescriptor::parse(
        &std::fs::read(web.join("releases").join("stable.json")).expect("the release is staged"),
    )
    .expect("the release parses");
    let catalog = ContentCatalog::parse(
        &std::fs::read(web.join("releases").join("stable").join("catalog.json"))
            .expect("the catalog is staged"),
    )
    .expect("the catalog parses");
    let variant = release.variant("windows-x64").expect("a variant");
    let victim = variant.content[0];
    let entry = catalog.entry(&victim).expect("catalogued");
    let hex = victim.to_hex();
    let path = web
        .join("blobs")
        .join("sha256")
        .join(&hex[..2])
        .join(&hex[2..]);

    // Replace one blob with different bytes of exactly the same length, which is
    // the only substitution a length check alone would miss.
    let original = std::fs::read(&path).expect("the blob is readable");
    let mut corrupt = original.clone();
    corrupt[0] ^= 0xff;
    std::fs::write(&path, &corrupt).expect("the corruption lands");

    let plan = AcquisitionPlan::build(vec![AcquisitionItem::new(
        entry.descriptor(ContentKind::Payload),
        ContentReason::File { component: None },
    )])
    .expect("the closure is well formed");
    let cache_dir = tempfile::tempdir().expect("a temporary cache");
    let cache = Arc::new(
        zup_acquire::ContentCache::open(cache_dir.path(), CachePolicy::Keep)
            .expect("the cache opens"),
    );
    let error = AcquisitionSession::new(plan, Arc::clone(&cache), SchedulerConfig::default())
        .run(
            SourceChain::new(vec![Arc::new(DirectorySource::new("origin", &web))]),
            Arc::new(zup_acquire::NeverCancelled),
            &ProgressSink::discard(),
        )
        .await
        .expect_err("a corrupt blob cannot satisfy the closure");
    assert!(matches!(error, AcquireError::Unavailable { .. }), "{error}");
    assert!(error.left_machine_unchanged());
    assert!(
        cache
            .get(&entry.descriptor(ContentKind::Payload), Verify::Full)
            .is_ok(),
        "the cache was asked, and it refused"
    );
    assert!(
        cache
            .get(&entry.descriptor(ContentKind::Payload), Verify::Full)
            .expect("the cache probes")
            .is_none(),
        "nothing corrupt is ever published"
    );
}

#[test]
fn shared_content_is_staged_once_and_served_to_both_variants() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let graph = compose(dir.path());
    let _tree = stage(dir.path(), &graph);
    let release = ReleaseDescriptor::parse(
        &std::fs::read(dir.path().join("web").join("releases").join("stable.json"))
            .expect("the release is staged"),
    )
    .expect("the release parses");

    let x64 = release.variant("windows-x64").expect("a variant");
    let arm = release.variant("windows-arm64").expect("a variant");
    let shared: Vec<_> = x64
        .content
        .iter()
        .filter(|digest| arm.content.contains(digest))
        .collect();
    assert!(
        !shared.is_empty(),
        "the fixture shares content between architectures, which is the whole point"
    );
    // The catalog describes the shared content once.
    let catalog = ContentCatalog::parse(
        &std::fs::read(
            dir.path()
                .join("web")
                .join("releases")
                .join("stable")
                .join("catalog.json"),
        )
        .expect("the catalog is staged"),
    )
    .expect("the catalog parses");
    for digest in &shared {
        assert!(
            catalog.entry(digest).is_some(),
            "a shared digest is catalogued once and served to both variants"
        );
    }
}

#[test]
fn a_channel_name_that_could_escape_the_release_directory_is_refused() {
    assert!(WebExport::new("stable").is_ok());
    assert!(WebExport::new("lts-1").is_ok());
    for hostile in ["../evil", "a/b", "", "Stable", "with space"] {
        assert!(
            WebExport::new(hostile).is_err(),
            "`{hostile}` must not be a channel"
        );
    }
}

#[test]
fn a_staged_runtime_is_addressable_content_a_client_can_verify_before_running() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let graph = compose(dir.path());
    stage(dir.path(), &graph);
    let web = dir.path().join("web");
    let release = ReleaseDescriptor::parse(
        &std::fs::read(web.join("releases").join("stable.json")).expect("the release is staged"),
    )
    .expect("the release parses");

    for variant in &release.variants {
        let runtime = release
            .runtime_descriptor(variant)
            .expect("an online release names a runtime");
        let hex = runtime.digest.to_hex();
        let path = web
            .join("blobs")
            .join("sha256")
            .join(&hex[..2])
            .join(&hex[2..]);
        assert!(
            path.is_file(),
            "the runtime for {} is staged as an ordinary immutable object",
            variant.id
        );
        assert_eq!(
            std::fs::metadata(&path)
                .expect("the runtime is readable")
                .len(),
            runtime.size
        );
        // It is content, so it goes through the same verification as anything
        // else — which is the only reason a bootstrapper may execute it.
        assert!(runtime.kind().is_executable());
    }
}
