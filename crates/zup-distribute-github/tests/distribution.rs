//! Reading a release back out of GitHub, end to end.
//!
//! Everything a GitHub-hosted thin installer does: pick a package, fetch it, put
//! verified objects in the cache - and, for every way a host can answer, still do
//! all three correctly.

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;

use common::{Blob, Origin, blobs, catalog_entry, descriptor as blob_descriptor};
use zup_acquire::{
    ArtifactSource, CachePolicy, ContentCache, ContentDescriptor, NeverCancelled,
    RelativeContentPath, SourceChain,
};
use zup_distribute_github::{
    Document, FileRef, PackageDescriptor, PackageHeader, ReleaseLayout, ShardRef, Writer,
};

const VARIANT: &str = "win-x64";
const TARGET: &str = "x86_64-pc-windows-msvc";
const APPLICATION: &str = "com.acme.app";
const ASSET: &str = "Acme-Windows-x64.zup";

/// One variant's package, packed and ready to serve.
struct Packed {
    /// The pieces, in order. Concatenated, they are the logical package.
    pieces: Vec<Vec<u8>>,
    metadata: zup_distribute_github::Metadata,
}

impl Packed {
    /// The whole logical package.
    fn bytes(&self) -> Vec<u8> {
        self.pieces.concat()
    }
}

/// Pack `blobs` into a package, with a shard ceiling of `shard_bytes`.
fn pack(blobs: &[Blob], shard_bytes: u64) -> Packed {
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
    let metadata = writer
        .write(
            PackageHeader::new(
                VARIANT,
                TARGET,
                APPLICATION,
                document(VARIANT),
                document("catalog"),
            ),
            shard_bytes,
            |index, bytes| {
                pieces.insert(index, bytes.to_vec());
                Ok(())
            },
        )
        .expect("a fixture package is written");
    Packed {
        pieces: pieces.into_values().collect(),
        metadata,
    }
}

fn document(text: &str) -> Document {
    Document {
        digest: zup_core::hash_bytes(text.as_bytes()),
        size: text.len() as u64,
    }
}

/// The descriptor a release would publish for a packed package.
fn package_descriptor(packed: &Packed) -> PackageDescriptor {
    let sharded = packed.pieces.len() > 1;
    let shards: Vec<ShardRef> = packed
        .metadata
        .shards
        .iter()
        .map(|shard| {
            let index = shard.index as usize;
            let piece = &packed.pieces[index];
            ShardRef {
                index: shard.index,
                name: if sharded {
                    zup_publish::shard_name(ASSET, index)
                } else {
                    ASSET.to_owned()
                },
                digest: zup_core::hash_bytes(piece),
                size: shard.size,
                start: shard.start,
            }
        })
        .collect();
    let descriptor = PackageDescriptor {
        schema: zup_distribute_github::DESCRIPTOR_SCHEMA,
        variant: VARIANT.to_owned(),
        target: TARGET.to_owned(),
        application: APPLICATION.to_owned(),
        package: FileRef {
            name: ASSET.to_owned(),
            digest: zup_core::hash_bytes(&packed.bytes()),
            size: packed.bytes().len() as u64,
        },
        shards,
        blob_count: packed.metadata.blobs.len() as u64,
        logical_size: packed
            .metadata
            .blobs
            .iter()
            .fold(0u64, |sum, frame| sum + frame.size),
    };
    descriptor
        .validate()
        .expect("a fixture descriptor is valid");
    descriptor
}

/// A package and its descriptor, served at a layout.
struct Release {
    layout: ReleaseLayout,
    descriptor: PackageDescriptor,
    packed: Packed,
}

/// Pack, describe, and serve one variant's content.
fn release(origin: &Origin, layout: ReleaseLayout, blobs: &[Blob], shard_bytes: u64) -> Release {
    let packed = pack(blobs, shard_bytes);
    let descriptor = package_descriptor(&packed);
    for shard in &descriptor.shards {
        origin.put(&shard.name, &packed.pieces[shard.index as usize]);
    }
    Release {
        layout,
        descriptor,
        packed,
    }
}

/// Open the release's package over HTTP and build a source over it.
///
/// Every test goes through the real open path rather than handing the source a
/// pre-built index, so the one request a thin installer makes to find out where
/// the bytes are is part of what is under test.
///
/// The source is shared by `Arc` because a scheduler holds one and every worker
/// borrows it, and because the range observation and the counters live behind the
/// same handles - which is what lets a test watch a transfer it did not own.
async fn open_source(
    release: &Release,
    blobs: &[Blob],
) -> (
    Arc<zup_distribute_github::GithubContentSource>,
    Vec<ContentDescriptor>,
) {
    let client = zup_distribute_github::client().expect("a client");
    let package = zup_distribute_github::open(&release.layout, &release.descriptor, &client)
        .await
        .expect("the package opens");
    let catalog = zup_acquire::ContentCatalog::new(blobs.iter().map(catalog_entry).collect())
        .expect("a fixture catalog");
    let source = zup_distribute_github::GithubContentSource::new(
        "github-release",
        &release.layout,
        package,
        catalog,
    )
    .expect("a source over a package");
    (
        Arc::new(source),
        blobs.iter().map(blob_descriptor).collect(),
    )
}

/// A one-source chain over a shared source.
fn chain(source: &Arc<zup_distribute_github::GithubContentSource>) -> SourceChain {
    SourceChain::new(vec![Arc::clone(source) as Arc<dyn ArtifactSource>])
}

/// A cache in a directory that outlives this call.
///
/// The temporary directory is the cache's whole lifetime: a `VerifiedBlob` names a
/// path inside it, so a test that reads acquired bytes has to still be holding
/// the cache that produced them.
struct Cache {
    _root: tempfile::TempDir,
    cache: ContentCache,
}

impl Cache {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("a temporary directory");
        let cache = ContentCache::open(root.path(), CachePolicy::Keep).expect("a cache");
        Self { _root: root, cache }
    }
}

/// Acquire every descriptor through a one-source chain, into a fresh cache.
///
/// The cache is returned rather than dropped: the blobs it holds name paths
/// inside it, so the caller has to keep it alive to read them.
async fn acquire_all(
    source: &Arc<zup_distribute_github::GithubContentSource>,
    descriptors: &[ContentDescriptor],
) -> (Vec<zup_acquire::VerifiedBlob>, Cache) {
    let cache = Cache::new();
    let chain = chain(source);
    let mut acquired = Vec::new();
    for descriptor in descriptors {
        acquired.push(
            chain
                .acquire(descriptor, &cache.cache, &NeverCancelled)
                .await
                .expect("a blob is acquired"),
        );
    }
    (acquired, cache)
}

/// Read every acquired blob and check it against its fixture.
fn assert_bytes(acquired: &[zup_acquire::VerifiedBlob], fixtures: &[Blob]) {
    assert_eq!(acquired.len(), fixtures.len());
    for (blob, fixture) in acquired.iter().zip(fixtures) {
        assert_eq!(
            blob.read_to_end().expect("a verified blob reads"),
            fixture.bytes
        );
    }
}

fn origin_base(origin: &Origin) -> String {
    format!("http://{}/acme/acme", origin.address())
}

#[tokio::test]
async fn a_version_pinned_release_imports_every_blob_into_the_cache() {
    let blobs = blobs(6, 32 * 1024);
    let origin = Origin::start(BTreeMap::new());
    let release = release(&origin, origin.pinned(), &blobs, u64::MAX);

    assert!(
        release.layout.release.is_pinned(),
        "a tag address is an identity"
    );
    assert_eq!(
        release.layout.asset_url(ASSET).expect("a url").as_str(),
        format!("{}/releases/download/v1.4.0/{ASSET}", origin_base(&origin)),
        "a pinned installer addresses the release by tag, so the bytes are the same forever"
    );

    let (source, descriptors) = open_source(&release, &blobs).await;
    assert_eq!(source.variant(), VARIANT);

    let (acquired, cache) = acquire_all(&source, &descriptors).await;
    assert_eq!(acquired.len(), blobs.len());
    // Identity is preserved: the digest the release authenticated is the path the
    // cache published the bytes at, byte for byte.
    for (blob, fixture) in acquired.iter().zip(&blobs) {
        assert_eq!(blob.descriptor.digest, fixture.digest);
        assert_eq!(
            blob.read_to_end().expect("a verified blob reads"),
            fixture.bytes
        );
    }
    let objects = cache.cache.objects();
    assert_eq!(objects.len(), blobs.len());
    for fixture in &blobs {
        assert!(
            objects.iter().any(|object| object.digest == fixture.digest),
            "sha256:{} is not in the cache",
            fixture.digest.to_hex()
        );
    }
}

#[tokio::test]
async fn a_second_architecture_is_never_downloaded() {
    let mine = blobs(4, 16 * 1024);
    // A different architecture's content, which shares no blob with this one.
    let theirs = common::blobs_from(4, 16 * 1024, 200);
    let origin = Origin::start(BTreeMap::new());
    let release = release(&origin, origin.pinned(), &mine, u64::MAX);

    let (source, descriptors) = open_source(&release, &mine).await;
    let foreign: Vec<ContentDescriptor> = theirs.iter().map(blob_descriptor).collect();
    // The source answers structurally, from an index it already holds, so the
    // question "could this host have it" costs no request at all.
    for descriptor in &foreign {
        assert!(
            !source.contains(descriptor),
            "a foreign blob is not claimed"
        );
    }
    let (acquired, _cache) = acquire_all(&source, &descriptors).await;
    assert_bytes(&acquired, &mine);
    // The wire cost is exactly this variant's package plus its frames: the open
    // read that finds the index, and one range per blob. Not one byte of anything
    // else, which is the whole claim of a per-variant package.
    let package = release.packed.bytes().len() as u64;
    let frames: u64 = mine.iter().map(|blob| blob.compressed.len() as u64).sum();
    assert_eq!(
        origin.served(),
        package + frames,
        "exactly this variant's package and its frames, and nothing else"
    );
    assert_eq!(
        origin.requests(),
        1 + mine.len() as u64,
        "one open, one per blob"
    );
}

/// Every way a host can answer a `Range`, and the same verified bytes.
///
/// Only the first is an optimisation; the other two are the shapes a client
/// has to survive, and the accounting has to say which one happened.
#[tokio::test]
async fn a_host_that_answers_a_range_any_way_still_produces_correct_content() {
    for (honour, wrong, support, ranged, refused) in [
        (
            true,
            false,
            Some(zup_distribute_github::RangeSupport::Supported),
            5u64,
            0u64,
        ),
        (
            false,
            false,
            Some(zup_distribute_github::RangeSupport::Unsupported),
            0,
            0,
        ),
        // A `206` whose `Content-Range` names a different range than the one
        // asked for. Reading those bytes would be reading the wrong object.
        (true, true, None, 0, 1),
    ] {
        let blobs = blobs(5, 24 * 1024);
        let origin = Origin::with_options(BTreeMap::new(), honour, wrong);
        let release = release(&origin, origin.pinned(), &blobs, u64::MAX);
        let (source, descriptors) = open_source(&release, &blobs).await;
        let metrics = source.metrics();
        assert_eq!(
            source.range_support(),
            zup_distribute_github::RangeSupport::Unknown,
            "the open is a whole read"
        );

        let (acquired, _cache) = acquire_all(&source, &descriptors).await;
        assert_bytes(&acquired, &blobs);
        if let Some(support) = support {
            assert_eq!(source.range_support(), support, "honour={honour}");
        }
        assert_bytes(&acquired, &blobs);
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.ranged_transfers, ranged, "honour={honour}");
        assert_eq!(snapshot.refused_transfers, refused, "honour={honour}");
        let package = release.packed.bytes().len() as u64;
        if ranged > 0 {
            let frames: u64 = blobs.iter().map(|blob| blob.compressed.len() as u64).sum();
            assert!(snapshot.is_fully_ranged(), "{snapshot:?}");
            assert_eq!(snapshot.needed_bytes, frames, "only the frames");
            assert_eq!(snapshot.wire_bytes, frames);
            assert!(
                snapshot.wire_bytes < package,
                "a ranged read costs {snapshot:?} against a {package} byte package"
            );
        } else {
            // Slower, and honestly so: the bytes are the package, read once
            // per blob.
            assert!(snapshot.fallback_transfers > 0, "{snapshot:?}");
            assert!(!snapshot.is_fully_ranged());
            assert!(snapshot.wire_bytes > snapshot.needed_bytes, "{snapshot:?}");
        }
    }
}
#[tokio::test]
async fn a_corrupt_package_frame_is_rejected_rather_than_published() {
    let blobs = blobs(3, 16 * 1024);
    let origin = Origin::start(BTreeMap::new());
    let mut release = release(&origin, origin.pinned(), &blobs, u64::MAX);
    // Corrupt the bytes of the *last* frame's interior. The header and metadata
    // still verify, so the index opens; only the frame's own digest catches it,
    // which is the point of checking every frame.
    let last = *release.packed.metadata.blobs.last().expect("a frame");
    let offset = last.offset as usize + (last.compressed_size as usize / 2);
    release.packed.pieces[0][offset] ^= 0xff;
    let piece = &release.packed.pieces[0];
    origin.put(ASSET, piece);

    let (source, descriptors) = open_source(&release, &blobs).await;
    let cache = Cache::new();
    let chain = chain(&source);
    let target = descriptors
        .iter()
        .find(|descriptor| descriptor.digest == last.digest)
        .expect("the corrupt frame's descriptor");
    let error = chain
        .acquire(target, &cache.cache, &NeverCancelled)
        .await
        .expect_err("a corrupt frame is refused");
    let reported = format!("{error:?}");
    // The refusal names both digests: the one the release authenticated and the
    // one the bytes actually have. A mismatch is the whole diagnosis.
    assert!(reported.contains(&last.digest.to_hex()), "{reported}");
    assert!(reported.contains("hashed to"), "{reported}");
    assert!(
        !cache
            .cache
            .objects()
            .iter()
            .any(|object| object.digest == last.digest),
        "a corrupt blob never reaches the cache"
    );
}

#[tokio::test]
async fn a_blob_the_authenticated_catalog_does_not_name_is_refused() {
    let blobs = blobs(3, 16 * 1024);
    let origin = Origin::start(BTreeMap::new());
    let release = release(&origin, origin.pinned(), &blobs, u64::MAX);
    // A catalog that names only half the package: the package carries a blob no
    // authenticated release ever claimed, and importing it would put content in
    // the cache that nothing vouches for.
    let catalog = zup_acquire::ContentCatalog::new(blobs[..2].iter().map(catalog_entry).collect())
        .expect("a fixture catalog");
    let client = zup_distribute_github::client().expect("a client");
    let package = zup_distribute_github::open(&release.layout, &release.descriptor, &client)
        .await
        .expect("the package opens");
    let source = Arc::new(
        zup_distribute_github::GithubContentSource::new(
            "github-release",
            &release.layout,
            package,
            catalog,
        )
        .expect("a source over a package"),
    );
    let stranger = &blobs[2];
    let descriptor = blob_descriptor(stranger);
    assert!(
        source.contains(&descriptor),
        "the index alone would say yes"
    );
    let cache = Cache::new();
    let chain = chain(&source);
    let error = chain
        .acquire(&descriptor, &cache.cache, &NeverCancelled)
        .await
        .expect_err("a blob the catalog does not name is refused");
    let reported = format!("{error:?}");
    assert!(
        reported.contains("catalog"),
        "the refusal names the reason: {reported}"
    );
    assert!(cache.cache.objects().is_empty(), "nothing was published");
}

#[tokio::test]
async fn a_sharded_package_reads_as_one_logical_source() {
    let blobs = blobs(6, 24 * 1024);
    let origin = Origin::start(BTreeMap::new());
    // A ceiling well under the package, so the writer has to split.
    let release = release(&origin, origin.pinned(), &blobs, 12 * 1024);
    assert!(
        release.descriptor.shards.len() > 1,
        "the fixture is sharded: {:?}",
        release.descriptor.shards
    );
    for shard in &release.descriptor.shards {
        assert!(
            zup_publish::check_asset_name(&shard.name).is_ok(),
            "`{}` is not a safe asset name",
            shard.name
        );
        assert!(
            shard.name.starts_with(ASSET),
            "`{}` keeps the package's name, so the pieces are recognisable on a release page",
            shard.name
        );
    }

    let (source, descriptors) = open_source(&release, &blobs).await;
    assert_eq!(
        source.variant(),
        VARIANT,
        "six assets are one logical source"
    );
    let (acquired, _cache) = acquire_all(&source, &descriptors).await;
    assert_bytes(&acquired, &blobs);
}
#[tokio::test]
async fn a_blob_whose_shard_is_missing_cannot_be_acquired() {
    let blobs = blobs(6, 24 * 1024);
    let origin = Origin::start(BTreeMap::new());
    let release = release(&origin, origin.pinned(), &blobs, 12 * 1024);
    let last = release
        .descriptor
        .shards
        .last()
        .expect("a last piece")
        .name
        .clone();
    origin.remove(&last);

    let (source, descriptors) = open_source(&release, &blobs).await;
    let cache = Cache::new();
    let chain = chain(&source);
    let mut failed = 0;
    for descriptor in &descriptors {
        if chain
            .acquire(descriptor, &cache.cache, &NeverCancelled)
            .await
            .is_err()
        {
            failed += 1;
        }
    }
    assert!(
        failed > 0,
        "a blob that lived only in the missing piece cannot be acquired"
    );
}
/// The stable alias is a channel, not an identity, and says so.
#[tokio::test]
async fn the_latest_alias_is_a_channel_and_never_a_pin() {
    let blobs = blobs(2, 8 * 1024);
    let origin = Origin::start(BTreeMap::new());
    let stable = origin.latest();
    let release = release(&origin, stable.clone(), &blobs, u64::MAX);

    assert!(!stable.release.is_pinned(), "the stable alias is not a pin");
    assert_eq!(stable.release.as_str(), "latest");
    assert_eq!(
        stable.asset_url(ASSET).expect("a url").as_str(),
        format!("{}/releases/latest/download/{ASSET}", origin_base(&origin))
    );
    let pinned = origin.pinned();
    assert!(pinned.release.is_pinned());
    assert_ne!(
        pinned.asset_url(ASSET).expect("a url"),
        stable.asset_url(ASSET).expect("a url"),
        "the two addresses are different shapes, and neither is derived from the other"
    );
    // A stable reference resolves today and is a different file tomorrow, which
    // is exactly why a version-pinned installer must not use it.
    let (source, descriptors) = open_source(&release, &blobs).await;
    let (acquired, _cache) = acquire_all(&source, &descriptors).await;
    assert_bytes(&acquired, &blobs);
    assert_eq!(
        release.layout.release_url(),
        None,
        "latest has no tag address"
    );
}

#[test]
fn a_written_package_is_its_pieces_in_order_and_nothing_is_left_out() {
    // The container's one structural promise: concatenating the pieces in index
    // order reproduces the package exactly, and the pieces tile it with no gap.
    // A boundary that started after the header would put those bytes in no piece
    // at all, and every reader would be short by sixty.
    for shard_bytes in [u64::MAX, 64 * 1024, 12 * 1024, 4 * 1024] {
        let blobs = blobs(6, 24 * 1024);
        let packed = pack(&blobs, shard_bytes);
        let mut cursor = 0u64;
        for (index, shard) in packed.metadata.shards.iter().enumerate() {
            assert_eq!(usize::try_from(shard.index).unwrap(), index);
            assert_eq!(
                shard.start, cursor,
                "piece {index} starts where the last ended"
            );
            assert_eq!(shard.size, packed.pieces[index].len() as u64);
            cursor += shard.size;
        }
        assert_eq!(
            cursor,
            packed.bytes().len() as u64,
            "the pieces cover everything"
        );

        // The reassembled package opens and its index agrees with the writer.
        let index = zup_distribute_github::Index::read(&packed.bytes()).expect("it opens");
        assert_eq!(index.metadata.shards, packed.metadata.shards);
        assert_eq!(index.total, packed.bytes().len() as u64);
        for frame in &packed.metadata.blobs {
            let found = index
                .metadata
                .frame(&frame.digest)
                .expect("a frame is found");
            assert_eq!(found, frame);
        }
    }
}

#[test]
fn a_package_refuses_metadata_that_has_been_tampered_with() {
    let blobs = blobs(2, 8 * 1024);
    let packed = pack(&blobs, u64::MAX);
    let mut bytes = packed.bytes();
    // The metadata is the region between the 60-byte header and the first frame.
    let start = zup_distribute_github::HEADER_LEN;
    let end = start + 8;
    for byte in &mut bytes[start..end] {
        *byte ^= 0xff;
    }
    let error =
        zup_distribute_github::Index::read(&bytes).expect_err("a tampered index is refused");
    assert!(
        matches!(
            error,
            zup_distribute_github::PackageError::MetadataDigest { .. }
        ),
        "{error}"
    );
}

#[test]
fn a_package_is_not_a_zup_bundle_and_says_so() {
    let error =
        zup_distribute_github::Index::read(b"ZUPBNDL\0").expect_err("a bundle is not a package");
    assert!(
        matches!(error, zup_distribute_github::PackageError::Short { .. }),
        "{error}"
    );
}
#[test]
fn a_release_root_path_never_becomes_an_asset_name() {
    // A blob path is content, not a document, and a publisher that flattened one
    // would produce a release with ten thousand assets.
    let path = RelativeContentPath::parse("blobs/sha256/ab/abcdef").expect("a valid content path");
    let error = zup_publish::asset_name(&path).expect_err("a blob is not publishable");
    assert!(error.contains("one asset per object"), "{error}");
}

#[test]
fn a_package_over_the_per_asset_limit_is_refused_before_anything_is_written() {
    // The point of `ProductClass` is that the same limit is a refusal for one
    // product and a sharding decision for another.
    let limits = zup_publish_github::LIMITS;
    let base = |class: zup_publish::ProductClass| {
        let mut plan = zup_publish::ReleasePlan::new(
            zup_publish::Application::new(
                zup_core::AppId::new(APPLICATION).expect("an id"),
                "Acme",
                "1.0.0",
            ),
            zup_publish::TagIntent::required("v1.0.0"),
        );
        plan.push(zup_publish::ReleaseProduct::new(
            ASSET,
            zup_publish::ProductRole::Install,
            class,
            zup_core::hash_bytes(b"x"),
            zup_publish_github::MAX_ASSET_BYTES,
        ));
        plan
    };
    let user = base(zup_publish::ProductClass::UserFacing)
        .preflight(&limits)
        .expect_err("an installer at the limit is a refusal");
    assert!(
        user.to_string().contains("the installer that was signed"),
        "{user}"
    );
    let transport = base(zup_publish::ProductClass::Transport)
        .preflight(&limits)
        .expect_err("a package at the limit is a refusal too, and says it may be sharded");
    assert!(transport.to_string().contains("shards"), "{transport}");
    // And the ceiling a publisher actually packs to leaves room under the host's
    // limit rather than sitting on it. Both are constants, so this is a fact about
    // the build rather than a runtime check.
    const { assert!(zup_publish_github::PACKAGE_SHARD_BYTES < zup_publish_github::MAX_ASSET_BYTES) };
    assert!(zup_publish_github::accepts(
        zup_publish_github::MAX_ASSET_BYTES - 1
    ));
    assert!(!zup_publish_github::accepts(
        zup_publish_github::MAX_ASSET_BYTES
    ));
}
