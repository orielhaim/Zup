//! A window's content, served from verified acquired content.
//!
//! The property this file exists for: once a preset is release content, the
//! bytes a transition installs arrive by digest from the same verified cache that
//! every other byte arrives from. Nothing reads a `.zupui`, nothing reads a
//! project directory, and nothing hashes an executable to discover what it is.
//!
//! Every case runs offline against a seeded cache, which is the same seam a real
//! download lands in: the acquisition engine writes each blob through
//! [`ContentCache::writer`] and re-proves it on the way out, so a source that
//! supplies the wrong bytes produces a refusal rather than a file.

use std::path::Path;

use tempfile::TempDir;
use zup_acquire::{CachePolicy, CatalogEntry, ContentCache, ContentCatalog};
use zup_bundle::{AcquiredPayloadSource, BundleWriter, Package, PayloadError};
use zup_core::hash_bytes;

/// The window this release carries: an executable, and two assets over two
/// pieces of content.
const PRESET: &[u8] = b"the preset executable";
const LOGO: &[u8] = b"<svg/>";
const HERO: &[u8] = b"\x89PNG\r\n";

fn asset(name: &str, content: &[u8]) -> zup_core::UiAsset {
    zup_core::UiAsset {
        name: zup_core::NonEmptyString::new(name).expect("a name"),
        size: content.len() as u64,
        sha256: hash_bytes(content),
    }
}

/// A plan-only package naming a window, and a cache holding exactly the bytes
/// that window needs.
///
/// `lying` replaces every blob with same-length different bytes, which is the
/// case a length check cannot catch. It surfaces as a refusal rather than a
/// panic, because the refusal is the behaviour under test.
fn release(
    root: &Path,
    lying: bool,
) -> Result<(Package, AcquiredPayloadSource), zup_acquire::CacheError> {
    let assets = vec![
        asset("branding/logo.svg", LOGO),
        asset("branding/hero.png", HERO),
    ];
    let mut installer = zup_core::Installer {
        app: zup_core::App {
            id: zup_core::AppId::new("com.example.window").expect("a valid id"),
            name: zup_core::NonEmptyString::new("Window").expect("a name"),
            version: "1.0.0".parse().expect("a version"),
            publisher: None,
            main: None,
            description: None,
        },
        target: zup_plugin_contract::HOST_TARGET
            .parse()
            .expect("the host target"),
        frontend: zup_core::Frontend::Gui,
        preset: Some(zup_core::UiPreset {
            name: zup_core::NonEmptyString::new("aurora").expect("a name"),
            version: "1.4.2".parse().expect("a version"),
            protocol: 1,
            required_capabilities: vec!["components".to_owned()],
            settings: serde_json::json!({
                "hero": "Install Acme",
                "logo": "branding/logo.svg",
            }),
            assets: assets.clone(),
        }),
        updates: None,
        install: zup_core::Install {
            scope: zup_core::InstallScope::User,
            directory: zup_core::InstallDirectory {
                user: Some(zup_core::Template::parse("${install}").expect("a dir")),
                machine: None,
            },
            allow_directory_override: false,
        },
        prerequisites: Vec::new(),
        components: Vec::new(),
        plugins: Vec::new(),
        files: Vec::new(),
        launchers: Vec::new(),
        path: Vec::new(),
        services: Vec::new(),
        protocols: Vec::new(),
        file_associations: Vec::new(),
    };
    installer.files = Vec::new();
    let plan = zup_bundle::PortableBuildPlan {
        installer,
        entries: Vec::new(),
        prerequisite_artifacts: Vec::new(),
        ui_assets: assets,
        plugins: Vec::new(),
        total_size: 0,
    };
    let package = Package::parse(
        BundleWriter::encode_plan_only_plan(&plan, &[]).expect("a plan-only package encodes"),
    )
    .expect("a plan-only package parses");

    // Every blob the window names goes through the cache the way a download
    // would, so what comes back out has been proved twice.
    let cache = ContentCache::open(root.join("cache"), CachePolicy::Auto).unwrap();
    let mut entries = Vec::new();
    for content in [PRESET, LOGO, HERO] {
        let mut served = content.to_vec();
        if lying {
            served[0] = b'!';
        }
        let descriptor = zup_acquire::ContentDescriptor::stored(
            zup_acquire::ContentKind::Payload,
            hash_bytes(content),
            served.len() as u64,
        );
        let mut writer = cache.writer(&descriptor).unwrap();
        writer.write(&served).unwrap();
        writer.commit()?;
        entries.push(CatalogEntry::stored(
            hash_bytes(content),
            served.len() as u64,
        ));
    }
    entries.sort_by_key(|entry| entry.digest);
    let source = AcquiredPayloadSource::new(cache, ContentCatalog::new(entries).unwrap(), &package);
    Ok((package, source))
}

/// A window's bytes come out of verified content, by digest and by the name the
/// settings used.
#[test]
fn a_windows_bytes_arrive_from_verified_content() {
    let root = TempDir::new().unwrap();
    let (_package, source) = release(root.path(), false).expect("the release is served");

    assert_eq!(
        source.read_blob(&hash_bytes(PRESET)).unwrap(),
        PRESET,
        "the executable arrives by the digest the release named, with nothing hashed to \
         discover it"
    );
    assert_eq!(
        source.ui_asset("branding/logo.svg").unwrap(),
        LOGO,
        "an asset arrives under the name the settings used"
    );
    assert_eq!(source.ui_asset("branding/hero.png").unwrap(), HERO);
    assert_eq!(
        source.ui_asset_names(),
        ["branding/hero.png", "branding/logo.svg"],
        "and the source can say which ones it has, so a plan can be checked against it"
    );
}

/// A name the release does not describe is a miss, not a lookup into whatever
/// happens to be in the cache.
#[test]
fn an_asset_the_release_does_not_describe_is_a_miss() {
    let root = TempDir::new().unwrap();
    let (_package, source) = release(root.path(), false).expect("the release is served");
    let error = source
        .ui_asset("branding/never-configured.svg")
        .expect_err("this release never described that asset");
    assert!(matches!(error, PayloadError::NotFound { .. }), "{error}");
}

/// Content that is not what the catalog described is refused before it is ever
/// published.
///
/// The refusal is at the acquisition boundary rather than at the read, which is
/// the stronger of the two: a blob that would not hash to its digest never
/// becomes a file, so there is no window in which content a network supplied is
/// merely *present* and could be executed. Every blob here is the right length
/// and the wrong bytes, so nothing but the digest could have caught it.
#[test]
fn content_that_is_not_what_the_catalog_described_is_refused_before_publication() {
    let root = TempDir::new().unwrap();
    let error = release(root.path(), true)
        .err()
        .expect("a blob that does not hash to its digest is not content");
    assert!(
        format!("{error}").contains("hashed to"),
        "and the refusal is about the digest, not the length: {error}"
    );
    let published = root.path().join("cache").join("blobs").join("sha256");
    let mut stored = 0usize;
    if let Ok(shards) = std::fs::read_dir(&published) {
        for shard in shards.flatten() {
            let Ok(blobs) = std::fs::read_dir(shard.path()) else {
                continue;
            };
            stored += blobs
                .flatten()
                .filter(|blob| !blob.file_name().to_string_lossy().contains('.'))
                .count();
        }
    }
    assert_eq!(
        stored, 0,
        "and nothing was published, so the wrong bytes were never a file"
    );
}

/// A blob that is not in the cache at all is absent, not an empty file.
#[test]
fn a_window_image_the_cache_does_not_hold_is_absent() {
    let root = TempDir::new().unwrap();
    let (_package, source) = release(root.path(), false).expect("the release is served");
    let error = source
        .read_blob(&hash_bytes(b"never acquired"))
        .expect_err("this machine does not hold that content");
    assert!(matches!(error, PayloadError::NotFound { .. }), "{error}");
}

/// The package's required content is its store, and a window's assets are in it.
///
/// The window's *executable* is not, and that is the shape rather than an
/// omission: an asset is a payload-shaped blob the plan names by logical name, so
/// it travels in the store beside everything else. The executable is a native
/// image, and a native image is addressed as one - beside the runtime, on the
/// variant manifest, and in the release descriptor - rather than as a blob the
/// package happens to carry. A query that conflated the two would be answering a
/// question nothing asks.
#[test]
fn a_plans_required_content_covers_the_assets_and_not_the_native_image() {
    let root = TempDir::new().unwrap();
    let (package, _source) = release(root.path(), false).expect("the release is served");
    let required = package.required_digests();

    for content in [LOGO, HERO] {
        assert!(
            required.contains(&hash_bytes(content)),
            "an asset is store content, so the package requires {content:?}"
        );
    }
    for asset in &package.plan().ui_assets {
        assert!(
            required.contains(&asset.sha256),
            "and each asset's own digest, so the plan and the closure agree about `{}`",
            asset.name
        );
    }
    assert_eq!(
        required.len(),
        2,
        "the two assets, and no payload this plan has"
    );
    assert!(
        !required.contains(&hash_bytes(PRESET)),
        "the executable is a named native image rather than store content, so naming it here \
         would be a second answer to a question the release already answers"
    );
}
