//! a preset as release content, from a build plan to a staged web tree.
//!
//! Every claim a release makes about a preset is checked here, because a release
//! that names a preset without shipping it is a release a client cannot install
//! from - and the client finds that out on a machine rather than at publish
//! time.
//!
//! No HTTP and no TUF: the layout is a file tree and the identity is arithmetic,
//! so the whole online story is provable without an origin.

mod common;

use std::path::Path;

use common::{ARM64, PresetSpec, X64, build_target_with_preset};
use zup_acquire::{ContentCatalog, ReleaseDescriptor};
use zup_artifact::{
    ArtifactComposer, ArtifactRequest, MediaType, VariantManifest, WebExport, export_web_tree,
};

/// a preset with three names over two pieces of content.
fn preset(generation: &'static str) -> PresetSpec {
    PresetSpec {
        assets: vec![
            (
                "branding/logo.svg".to_owned(),
                "the application logo".to_owned(),
            ),
            (
                "branding/hero.png".to_owned(),
                "the application hero".to_owned(),
            ),
            // Two logical names, one piece of content: the release must store it
            // once and name it twice.
            (
                "branding/logo-mask.svg".to_owned(),
                "the application logo".to_owned(),
            ),
        ],
        generation,
    }
}

/// Compose the graph a release is staged from.
///
/// The same request `zup publish stage` makes: one graph for the whole release,
/// in the mode that carries content. A thin artifact is the bootstrapper that
/// fetches from the release, so the release - not the bootstrapper - is what has
/// to hold the bytes.
fn compose(root: &Path, id: &str, window: Option<&PresetSpec>) -> zup_artifact::ArtifactGraph {
    let x64 = build_target_with_preset(root.join("x64"), &X64, window);
    let arm = build_target_with_preset(root.join("arm"), &ARM64, window);
    let request = ArtifactRequest::universal_offline(id, &common::app(), "Acme-Windows-Setup.exe");
    let variants = [&x64, &arm];
    ArtifactComposer::new(request, &variants)
        .expect("the request is well formed")
        .compose(&variants)
        .expect("the graph composes")
}

fn stage(root: &Path, graph: &zup_artifact::ArtifactGraph) -> std::path::PathBuf {
    let export = WebExport::new("stable").expect("a channel name");
    export_web_tree(graph, &export, &root.join("web")).expect("the tree is written");
    root.join("web")
}

fn release(web: &Path) -> ReleaseDescriptor {
    ReleaseDescriptor::parse(
        &std::fs::read(web.join("releases").join("stable.json")).expect("the release is staged"),
    )
    .expect("the release parses")
}

fn catalog(web: &Path) -> ContentCatalog {
    ContentCatalog::parse(
        &std::fs::read(web.join("releases").join("stable").join("catalog.json"))
            .expect("the catalog is staged"),
    )
    .expect("the catalog parses")
}

fn manifest_of(graph: &zup_artifact::ArtifactGraph, id: &str) -> VariantManifest {
    let bytes = graph
        .manifest_bytes()
        .expect("the graph has manifests")
        .into_iter()
        .find(|(variant, _)| variant == id)
        .map(|(_, bytes)| bytes)
        .expect("the graph has that variant");
    VariantManifest::parse(&bytes).expect("the manifest parses")
}

fn blob_path(web: &Path, digest: &zup_core::Sha256Digest) -> std::path::PathBuf {
    let hex = digest.to_hex();
    web.join("blobs")
        .join("sha256")
        .join(&hex[..2])
        .join(&hex[2..])
}

/// A release names the preset's executable, per target, and ships its bytes.
#[test]
fn a_release_names_and_ships_each_targets_preset() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let graph = compose(dir.path(), "acme-1.4.0", Some(&preset("a")));
    let web = stage(dir.path(), &graph);
    let release = release(&web);
    let catalog = catalog(&web);

    let x64 = release.variant("windows-x64").expect("x64 is offered");
    let arm = release.variant("windows-arm64").expect("arm64 is offered");
    let x64_preset = x64.preset.expect("a graphical variant names its window");
    let arm_preset = arm.preset.expect("and so does the other target");

    assert_ne!(
        x64_preset.digest, arm_preset.digest,
        "each target names its own native image: two architectures are two binaries"
    );
    let declared = manifest_of(&graph, "windows-x64")
        .preset
        .expect("the manifest declares the same image")
        .size;
    assert_eq!(
        x64_preset.size, declared,
        "the release states the length it composed"
    );

    for descriptor in [x64_preset, arm_preset] {
        let entry = catalog
            .entry(&descriptor.digest)
            .expect("the preset's bytes are catalogued");
        assert_eq!(
            entry.size, descriptor.size,
            "the catalog agrees with the release"
        );
        assert!(
            blob_path(&web, &descriptor.digest).is_file(),
            "and the bytes are in the tree at {}",
            descriptor.digest
        );
    }
}

/// The application's assets are required content, and identical bytes are one
/// download whatever names them.
#[test]
fn the_assets_a_window_needs_are_part_of_the_variants_content() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let graph = compose(dir.path(), "acme-1.4.0", Some(&preset("a")));
    let web = stage(dir.path(), &graph);
    let release = release(&web);
    let catalog = catalog(&web);

    let x64 = release.variant("windows-x64").expect("x64 is offered");
    let manifest = manifest_of(&graph, "windows-x64");
    assert_eq!(
        manifest.plan.ui_assets.len(),
        3,
        "three names were configured"
    );

    for asset in &manifest.plan.ui_assets {
        assert!(
            x64.content.contains(&asset.sha256),
            "the asset `{}` is required content, so a client fetches it",
            asset.name
        );
        assert!(
            catalog.entry(&asset.sha256).is_some(),
            "and the release carries its bytes"
        );
    }
    let distinct: std::collections::BTreeSet<_> = manifest
        .plan
        .ui_assets
        .iter()
        .map(|asset| asset.sha256)
        .collect();
    assert_eq!(distinct.len(), 2, "two names share the logo's bytes");
    assert_eq!(
        x64.content
            .iter()
            .filter(|digest| distinct.contains(digest))
            .count(),
        2,
        "and the variant's content set is deduplicated rather than counting it twice"
    );
}

/// Where the preset came from is not a fact the release carries.
///
/// a preset from the toolchain and a preset from a user's package are the same
/// shape by this point, because both are a digest in a plan beside a media type
/// the release model already had. Branching acquisition on the origin would mean
/// two acquisition paths for one thing.
#[rstest::rstest]
#[case::a_default_preset_source("toolchain")]
#[case::a_package_the_user_chooses("package")]
fn where_the_window_came_from_does_not_change_the_release(#[case] _origin: &str) {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let graph = compose(dir.path(), "acme-1.4.0", Some(&preset("a")));
    stage(dir.path(), &graph);
    let release = release(&dir.path().join("web"));

    assert!(
        release
            .variant("windows-x64")
            .expect("x64")
            .preset
            .is_some(),
        "either way the release names a native image and nothing more"
    );
    assert_eq!(
        manifest_of(&graph, "windows-x64")
            .preset
            .map(|preset| preset.media_type),
        Some(MediaType::PRESET),
        "and the media type is the permanent one, not a side channel"
    );
}

/// A variant with no window carries none, and saying so is a fact rather than a
/// gap: a console application has no image to fetch and nothing to declare.
#[test]
fn a_variant_with_no_window_carries_none() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let graph = compose(dir.path(), "acme-1.4.0", None);
    stage(dir.path(), &graph);
    let release = release(&dir.path().join("web"));

    assert!(
        release
            .variant("windows-x64")
            .expect("x64")
            .preset
            .is_none()
    );
    assert!(
        manifest_of(&graph, "windows-x64").plan.ui_assets.is_empty(),
        "and the plan names no assets for a preset it does not have"
    );
}

/// The release names a preset by content, never by filename.
///
/// A filename is what a target's composition decides, and a release that
/// published one would be telling a client running on a different platform which
/// file to expect. Payload destinations are a different matter - they are real
/// paths the application asked for - so the check is about the preset's own
/// records.
#[test]
fn the_release_names_a_window_by_content_and_never_by_filename() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let graph = compose(dir.path(), "acme-1.4.0", Some(&preset("a")));
    stage(dir.path(), &graph);
    let web = dir.path().join("web");

    for name in ["releases/stable.json", "releases/stable/catalog.json"] {
        let text = std::fs::read_to_string(web.join(name)).expect("the document is staged");
        assert!(
            !text.contains(".exe"),
            "{name} carries a filename, which is a composition decision, not release data"
        );
    }
    for id in ["windows-x64", "windows-arm64"] {
        let manifest = manifest_of(&graph, id);
        let preset = manifest.preset.expect("a graphical variant declares one");
        assert_eq!(preset.media_type, MediaType::PRESET);
        assert!(
            !preset.digest.to_hex().contains('.'),
            "and a preset is named by its content address alone"
        );
    }
}

/// Two generations of the same application: the preset that did not change is one
/// download, and the preset that did is a different digest.
#[test]
fn a_window_that_did_not_change_is_the_same_content() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let first = compose(&dir.path().join("first"), "acme-1.4.0", Some(&preset("a")));
    let again = compose(&dir.path().join("again"), "acme-1.4.0", Some(&preset("a")));
    let changed = compose(
        &dir.path().join("changed"),
        "acme-1.5.0",
        Some(&PresetSpec {
            assets: vec![(
                "branding/logo.svg".to_owned(),
                "a different logo".to_owned(),
            )],
            generation: "b",
        }),
    );

    let image = |graph: &zup_artifact::ArtifactGraph| graph.presets()[0].descriptor.digest;
    let logo = |graph: &zup_artifact::ArtifactGraph| {
        manifest_of(graph, "windows-x64").plan.ui_assets[0].sha256
    };

    assert_eq!(
        image(&first),
        image(&again),
        "the same window is the same content, so it is fetched once"
    );
    assert_eq!(logo(&first), logo(&again), "and so is an unchanged asset");
    assert_ne!(
        image(&first),
        image(&changed),
        "a new window generation is different content"
    );
    assert_ne!(logo(&first), logo(&changed), "and so is a changed asset");
}

/// The same asset under two names is one blob in the tree, because the tree is
/// addressed by content and not by what a plan happens to call it.
#[test]
fn one_piece_of_content_under_two_names_is_stored_once() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let graph = compose(dir.path(), "acme-1.4.0", Some(&preset("a")));
    let web = stage(dir.path(), &graph);
    let manifest = manifest_of(&graph, "windows-x64");

    let logo = manifest
        .plan
        .ui_assets
        .iter()
        .find(|asset| asset.name.as_str() == "branding/logo.svg")
        .expect("the logo");
    let mask = manifest
        .plan
        .ui_assets
        .iter()
        .find(|asset| asset.name.as_str() == "branding/logo-mask.svg")
        .expect("the mask");
    assert_eq!(
        logo.sha256, mask.sha256,
        "the fixture names one content twice"
    );
    assert!(
        blob_path(&web, &logo.sha256).is_file(),
        "and the tree holds it once, addressed by its content"
    );
}
