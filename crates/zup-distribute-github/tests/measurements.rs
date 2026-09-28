//! What GitHub distribution actually costs, measured rather than asserted in
//! prose.
//!
//! Every other test in this crate asks "is it correct". This one asks "how much",
//! because the answers to these are what decide whether GitHub-hosted
//! distribution is a good idea for a project or a compromise:
//!
//! | question                | why it is the question                                       |
//! |-------------------------|--------------------------------------------------------------|
//! | how many assets         | the host allows 1000 and the UI allows about thirty            |
//! | how many bytes, cold    | what a user on a slow connection waits for                     |
//! | how many bytes, warm    | what a second install costs, and whether it costs anything     |
//! | how many requests       | what the host sees, and what a rate limit would be spent on     |
//! | ranged vs whole         | whether the range optimisation is real or a hope               |
//! | what a resume saves     | what a dropped connection costs a user on a large blob         |
//!
//! The numbers are computed from a fixture that shares content the way a real
//! multi-architecture release does - one runtime, many shared assets - because a
//! fixture with no sharing would make per-variant packages look like a flat copy
//! of the project.

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;

use common::{Origin, blobs_from, catalog_entry, descriptor as blob_descriptor};
use zup_acquire::{ArtifactSource, CachePolicy, ContentCache, NeverCancelled, SourceChain};
use zup_distribute_github::{
    Document, FileRef, PackageDescriptor, PackageHeader, ShardRef, Writer,
};

const APPLICATION: &str = "com.acme.app";

/// One variant's package and the descriptor that names it.
struct Packed {
    pieces: Vec<Vec<u8>>,
    descriptor: PackageDescriptor,
}

impl Packed {
    fn size(&self) -> u64 {
        self.pieces.iter().map(|piece| piece.len() as u64).sum()
    }
}

/// Pack `blobs` for `variant`, the way `zup publish stage` does.
fn pack(variant: &str, target: &str, blobs: &[common::Blob], shard_bytes: u64) -> Packed {
    let asset = zup_publish::package_name("Acme", variant);
    let mut writer = Writer::new();
    for blob in blobs {
        writer
            .insert(
                blob.digest,
                blob.compressed.clone(),
                blob.bytes.len() as u64,
            )
            .expect("a fixture blob is inserted");
    }
    let mut pieces: BTreeMap<usize, Vec<u8>> = BTreeMap::new();
    let document = |text: &str| Document {
        digest: zup_core::hash_bytes(text.as_bytes()),
        size: text.len() as u64,
    };
    let metadata = writer
        .write(
            PackageHeader::new(
                variant,
                target,
                APPLICATION,
                document(variant),
                document("catalog"),
            ),
            shard_bytes,
            |index, bytes| {
                pieces.insert(index, bytes.to_vec());
                Ok(())
            },
        )
        .expect("a fixture package is written");
    let sharded = pieces.len() > 1;
    let ordered: Vec<Vec<u8>> = pieces.into_values().collect();
    let shards: Vec<ShardRef> = metadata
        .shards
        .iter()
        .map(|shard| {
            let index = shard.index as usize;
            ShardRef {
                index: shard.index,
                name: if sharded {
                    zup_publish::shard_name(&asset, index)
                } else {
                    asset.clone()
                },
                digest: zup_core::hash_bytes(&ordered[index]),
                size: shard.size,
                start: shard.start,
            }
        })
        .collect();
    let size: u64 = shards.iter().map(|shard| shard.size).sum();
    let descriptor = PackageDescriptor {
        schema: zup_distribute_github::DESCRIPTOR_SCHEMA,
        variant: variant.to_owned(),
        target: target.to_owned(),
        application: APPLICATION.to_owned(),
        package: FileRef {
            name: asset.clone(),
            digest: zup_core::hash_bytes(&ordered.concat()),
            size,
        },
        shards,
        blob_count: metadata.blobs.len() as u64,
        logical_size: metadata
            .blobs
            .iter()
            .fold(0u64, |sum, frame| sum + frame.size),
    };
    descriptor
        .validate()
        .expect("a fixture descriptor is valid");
    Packed {
        pieces: ordered,
        descriptor,
    }
}

/// The variants of a release that shares its runtime, as a real one does.
///
/// `win-x64` and `win-arm64` differ in a handful of blobs and agree about
/// everything else, which is the shape that decides whether a per-variant package
/// is a copy or a set.
fn variants() -> Vec<(&'static str, &'static str, Vec<common::Blob>)> {
    let shared: Vec<common::Blob> = blobs_from(24, 48 * 1024, 1);
    let x64_only = blobs_from(4, 32 * 1024, 90);
    let arm_only = blobs_from(4, 32 * 1024, 140);
    let mut win_x64 = shared.clone();
    win_x64.extend(x64_only);
    let mut win_arm64 = shared.clone();
    win_arm64.extend(arm_only);
    vec![
        ("win-x64", "x86_64-pc-windows-msvc", win_x64),
        ("win-arm64", "aarch64-pc-windows-msvc", win_arm64),
    ]
}

/// Serve every variant's pieces and return the layout.
fn serve(origin: &Origin, packed: &[Packed]) -> zup_distribute_github::ReleaseLayout {
    let layout = origin.pinned();
    for package in packed {
        for shard in &package.descriptor.shards {
            origin.put(&shard.name, &package.pieces[shard.index as usize]);
        }
    }
    layout
}

/// Acquire every blob of one variant, returning what the host sent.
async fn install(
    layout: &zup_distribute_github::ReleaseLayout,
    descriptor: &PackageDescriptor,
    blobs: &[common::Blob],
    cache_root: &std::path::Path,
) -> u64 {
    let packed = Packed {
        pieces: Vec::new(),
        descriptor: descriptor.clone(),
    };
    let source = build_source(layout, &packed, blobs).await;
    install_with(&source, blobs, cache_root).await;
    source.metrics().snapshot().wire_bytes
}

#[tokio::test]
async fn a_two_architecture_release_is_a_handful_of_assets_and_the_union_of_its_content() {
    let origin = Origin::start(BTreeMap::new());
    let packed: Vec<Packed> = variants()
        .iter()
        .map(|(variant, target, blobs)| pack(variant, target, blobs, u64::MAX))
        .collect();
    let layout = serve(&origin, &packed);

    // The headline number: assets per release is a function of the *variants*, not
    // of the content. A project with a thousand blobs has the same release page as
    // one with ten.
    let assets: usize = packed.iter().map(|package| package.pieces.len()).sum();
    let documents = packed.len(); // one package descriptor per variant
    assert_eq!(
        assets + documents,
        4,
        "two variants is two packages and two descriptors, and the content is inside them"
    );
    assert!(
        assets + documents < zup_publish_github::MAX_ASSETS,
        "and nowhere near the host's per-release limit"
    );

    // The other headline number, and the one worth being precise about: bytes do
    // not deduplicate across variants. Two variants that share most of their
    // content still publish both copies, because each package is a separate asset
    // on a host with no cross-asset storage. What deduplicates is the *asset
    // count* - the alternative is one asset per content object, which is the thing
    // that breaks.
    //
    // So the honest numbers are: the release holds each variant's content once,
    // plus a per-variant header and index, and it costs `variants × assets`-many
    // files rather than `blobs`-many.
    let sum: u64 = variants()
        .iter()
        .map(|(_, _, blobs)| {
            blobs
                .iter()
                .map(|blob| blob.compressed.len() as u64)
                .sum::<u64>()
        })
        .sum();
    let published: u64 = packed.iter().map(Packed::size).sum();
    let objects: usize = variants().iter().map(|(_, _, blobs)| blobs.len()).sum();
    assert!(
        published >= sum,
        "every variant's content is published: {published} against {sum} of frames"
    );
    let overhead = published - sum;
    assert!(
        overhead < published / 16,
        "and the per-variant header and index is the only overhead: {overhead} bytes of {published}"
    );
    assert!(
        assets + documents <= objects / 4,
        "while the asset count is a small fraction of the object count: {} assets for {objects} objects",
        assets + documents
    );

    // A cold install on one architecture costs its frames, plus one read to find
    // where they are - not the whole release, and not the other architecture.
    let root = tempfile::tempdir().expect("a temporary directory");
    let (_, _, x64) = variants().into_iter().next().expect("a variant");
    let served = install(&layout, &packed[0].descriptor, &x64, root.path()).await;
    let frames: u64 = x64.iter().map(|blob| blob.compressed.len() as u64).sum();
    let package = packed[0].size();
    assert!(
        served <= package + frames,
        "a ranged install costs {served} against a {package}-byte package and {frames} bytes of frames"
    );
    assert!(
        served < packed[0].descriptor.logical_size,
        "and less than the content it reconstructs, which is the point of transport compression"
    );
}

#[tokio::test]
async fn a_warm_cache_transfers_nothing_and_still_costs_one_read_to_open_the_package() {
    let origin = Origin::start(BTreeMap::new());
    let packed: Vec<Packed> = variants()
        .iter()
        .map(|(variant, target, blobs)| pack(variant, target, blobs, u64::MAX))
        .collect();
    let layout = serve(&origin, &packed);
    let (_, _, blobs) = variants().into_iter().next().expect("a variant");

    let root = tempfile::tempdir().expect("a temporary directory");
    let cold = install(&layout, &packed[0].descriptor, &blobs, root.path()).await;
    assert!(cold > 0, "a cold cache does transfer something");
    let requests_after_cold = origin.requests();
    let served_after_cold = origin.served();

    // Same cache, same blobs. No content crosses the wire the second time, which
    // is what makes an update check cheap - but opening the package is not free,
    // because the source has to learn where the frames are and the index lives in
    // the package. One small read, and no more.
    let warm = install(&layout, &packed[0].descriptor, &blobs, root.path()).await;
    assert_eq!(
        warm, 0,
        "a warm cache transfers no content; the open is counted by the caller"
    );
    assert_eq!(
        origin.requests() - requests_after_cold,
        1,
        "and the only request a warm run makes is the one that opens the package"
    );
    let open_read = origin.served() - served_after_cold;
    assert_eq!(
        open_read,
        packed[0].size(),
        "which is one read of the first piece: the header, the index, and whatever frames share it"
    );
}

#[tokio::test]
async fn the_range_optimisation_is_worth_what_it_costs_or_it_is_not_worth_anything() {
    // The same fixture against a host that honours ranges and a host that does
    // not, so "GitHub supports range requests" is a number rather than a belief.
    let (_, target, blobs) = variants().into_iter().next().expect("a variant");

    let ranged = install_cost(true, target, &blobs).await;
    let whole = install_cost(false, target, &blobs).await;

    assert!(
        ranged.wire < whole.wire,
        "a ranged install costs less: {ranged:?} against {whole:?}"
    );
    assert_eq!(
        ranged.needed, whole.needed,
        "and both deliver the same content"
    );

    // And the shape of the fallback, which is the number a project needs to know
    // before choosing this distribution mode. A host that ignores `Range` sends
    // the whole piece for every blob, so the cost is *blobs × piece* - correct,
    // and potentially an order of magnitude more than the content. The counters
    // exist so this is something a project measures rather than discovers on a
    // user's connection.
    assert!(
        whole.wire > whole.piece * (blobs.len() as u64 / 2),
        "without range support the host re-sends the piece per blob: {} bytes of wire for {} blobs in a {}-byte piece",
        whole.wire,
        blobs.len(),
        whole.piece
    );

    // And the same numbers again through the metrics, because a report is how a
    // project would find any of this out.
    let origin = Origin::with_options(BTreeMap::new(), false, false);
    let packed = pack("win-x64", "x86_64-pc-windows-msvc", &blobs, u64::MAX);
    let layout = serve(&origin, std::slice::from_ref(&packed));
    let root = tempfile::tempdir().expect("a temporary directory");
    let source = build_source(&layout, &packed, &blobs).await;
    install_with(&source, &blobs, root.path()).await;
    let snapshot = source.metrics().snapshot();
    assert_eq!(snapshot.ranged_transfers, 0);
    assert_eq!(snapshot.fallback_transfers, blobs.len() as u64);
    assert_eq!(
        snapshot.wire_bytes, whole.wire,
        "the wire cost is what it says"
    );
    assert_eq!(
        snapshot.needed_bytes, whole.needed,
        "and the saving the optimisation would have been is separate from it"
    );
    assert!(
        !snapshot.is_fully_ranged(),
        "and a report says so, rather than leaving a project to assume the optimisation is there"
    );
}

/// What one install cost.
#[derive(Debug, Clone, Copy)]
struct Cost {
    /// Bytes the host sent for content.
    wire: u64,
    /// Bytes the release actually needed.
    needed: u64,
    /// The one piece every blob lives in.
    piece: u64,
}

/// Serve one variant's package on a host with the given range behaviour, and
/// install it.
async fn install_cost(honour: bool, target: &str, blobs: &[common::Blob]) -> Cost {
    let origin = Origin::with_options(BTreeMap::new(), honour, false);
    let packed = pack("win-x64", target, blobs, u64::MAX);
    let piece = packed.size();
    let layout = serve(&origin, std::slice::from_ref(&packed));
    let root = tempfile::tempdir().expect("a temporary directory");
    let source = build_source(&layout, &packed, blobs).await;
    install_with(&source, blobs, root.path()).await;
    let snapshot = source.metrics().snapshot();
    Cost {
        wire: snapshot.wire_bytes,
        needed: snapshot.needed_bytes,
        piece,
    }
}

/// Open a package and wrap it in a source, for the tests that need both.
async fn build_source(
    layout: &zup_distribute_github::ReleaseLayout,
    packed: &Packed,
    blobs: &[common::Blob],
) -> Arc<zup_distribute_github::GithubContentSource> {
    let client = zup_distribute_github::client().expect("a client");
    let package = zup_distribute_github::open(layout, &packed.descriptor, &client)
        .await
        .expect("the package opens");
    let catalog = zup_acquire::ContentCatalog::new(blobs.iter().map(catalog_entry).collect())
        .expect("a catalog");
    Arc::new(
        zup_distribute_github::GithubContentSource::new("github-release", layout, package, catalog)
            .expect("a source"),
    )
}

/// Acquire every blob of `blobs` through `source`.
async fn install_with(
    source: &Arc<zup_distribute_github::GithubContentSource>,
    blobs: &[common::Blob],
    cache_root: &std::path::Path,
) {
    let cache = ContentCache::open(cache_root, CachePolicy::Keep).expect("a cache");
    let chain = SourceChain::new(vec![Arc::clone(source) as Arc<dyn ArtifactSource>]);
    for blob in blobs {
        chain
            .acquire(&blob_descriptor(blob), &cache, &NeverCancelled)
            .await
            .unwrap_or_else(|error| panic!("a blob is acquired: {error}"));
    }
}

#[tokio::test]
async fn a_dropped_connection_costs_the_rest_of_the_blob_and_not_the_whole_blob_again() {
    // The one measurement a user feels directly. A ninety-megabyte blob on a
    // dropped connection is either a nine-megabyte retry or a ninety-megabyte one,
    // and the difference is the host's fault, which is why the client has to
    // handle it rather than assume a good connection.
    let blobs = blobs_from(2, 96 * 1024, 7);
    let origin = Origin::start(BTreeMap::new());
    let packed = pack("win-x64", "x86_64-pc-windows-msvc", &blobs, u64::MAX);
    let layout = serve(&origin, std::slice::from_ref(&packed));

    let source = build_source(&layout, &packed, &blobs).await;
    let root = tempfile::tempdir().expect("a temporary directory");
    let cache = ContentCache::open(root.path(), CachePolicy::Keep).expect("a cache");
    let chain = SourceChain::new(vec![Arc::clone(&source) as Arc<dyn ArtifactSource>]);
    let target = blob_descriptor(&blobs[0]);
    // Opening the package is one whole read; the transfer is measured from there.
    let before_transfer = origin.served();
    let frame = blobs[0].compressed.len() as u64;

    // The first attempt is cut short.
    origin.truncate_bodies();
    chain
        .acquire(&target, &cache, &NeverCancelled)
        .await
        .expect_err("a connection that drops mid-body is a failure");
    let after_first = origin.served() - before_transfer;
    assert!(
        after_first > 0 && after_first < frame,
        "only part of the blob arrived: {after_first} of {frame}"
    );
    assert!(
        !cache
            .objects()
            .iter()
            .any(|object| object.digest == blobs[0].digest),
        "and nothing was published from a transfer that did not finish"
    );

    // The second attempt finishes it, and the ranges the host was asked for prove
    // it started where the first stopped rather than at zero.
    let acquired = chain
        .acquire(&target, &cache, &NeverCancelled)
        .await
        .expect("the retry completes the blob");
    assert_eq!(acquired.read_to_end().expect("reads"), blobs[0].bytes);
    assert_eq!(
        origin.served() - before_transfer,
        frame,
        "so the blob crossed the wire once in total, not twice"
    );
    // The proof that it resumed rather than restarted: the last range the host was
    // asked for starts where the truncated body ended, counted in the same piece
    // the first one started in.
    let asked: Vec<String> = origin.ranges().into_iter().flatten().collect();
    let start_of = |range: &str| -> u64 {
        range
            .strip_prefix("bytes=")
            .and_then(|value| value.split_once('-'))
            .and_then(|(start, _)| start.trim().parse().ok())
            .unwrap_or_else(|| panic!("a byte range, not `{range}`"))
    };
    let first = &asked[asked.len() - 2];
    let last = asked.last().expect("the resumed request");
    assert_eq!(
        start_of(last) - start_of(first),
        after_first,
        "the retry began {after_first} bytes past where the first attempt started, which is exactly what arrived"
    );
    assert_eq!(
        source.metrics().snapshot().resumed_transfers,
        1,
        "and the source says so, so a report can show that resumes are happening"
    );
}
