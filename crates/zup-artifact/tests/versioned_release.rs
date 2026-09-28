//! The version-addressed release document, and the promise it makes.
//!
//! A channel document moves. That is the whole point of a channel: it is where
//! "the current release" is published. It is also why a version-labelled
//! installer cannot use one - the installer would install whatever the channel
//! said on the day it ran, which is not what "version 1.4.0" means to anybody.
//!
//! So the same body is published under both names. The version-addressed one
//! cannot move, because nothing rewrites it, and its digest is the same either
//! way so a pinned and a channel client that land on one version can prove they
//! would install identical bytes.

mod common;

use std::path::Path;

use common::{ARM64, X64, build_target};
use zup_acquire::{OnlineTrust, ReleaseDescriptor, ReleasePin};
use zup_artifact::{ArtifactComposer, ArtifactRequest, WebExport, export_web_tree};

fn stage(root: &Path) -> std::path::PathBuf {
    let x64 = build_target(root.join("x64"), &X64);
    let arm = build_target(root.join("arm64"), &ARM64);
    let request = ArtifactRequest::universal_offline(
        "acme-windows",
        &common::app(),
        "Acme-Windows-Setup.exe",
    );
    let graph = ArtifactComposer::new(request, &[&x64, &arm])
        .expect("the request is well formed")
        .compose(&[&x64, &arm])
        .expect("the graph composes");
    let export = WebExport::new("stable").expect("a channel name");
    export_web_tree(&graph, &export, &root.join("web")).expect("the tree is written");
    root.join("web")
}

#[test]
fn a_channel_and_a_version_address_the_same_release() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let web = stage(dir.path());

    let channel =
        std::fs::read(web.join("releases").join("stable.json")).expect("the channel is staged");
    let versioned = std::fs::read(
        web.join("releases")
            .join("stable")
            .join("versions")
            .join("1.4.0.json"),
    )
    .expect("the version-addressed release is staged");

    // Identical bytes, so the release digest is the same either way. That is
    // what lets a pinned client and a channel client prove they agree.
    assert_eq!(channel, versioned);
    let release = ReleaseDescriptor::parse(&channel).expect("the release parses");
    release.validate().expect("the release is well formed");
    assert_eq!(release.version, "1.4.0");

    // And both names are TUF targets, so both are authenticated by the same
    // signature - a version-addressed name nobody signed would be worthless.
    let tuf_input = web.join("tuf-input");
    assert!(
        tuf_input
            .join("releases/stable/versions/1.4.0.json")
            .is_file(),
        "the version-addressed release is in the TUF input tree"
    );
    assert_eq!(
        std::fs::read(tuf_input.join("releases/stable/versions/1.4.0.json")).expect("readable"),
        versioned,
        "the TUF input tree carries the same bytes the origin serves"
    );
}

#[test]
fn the_two_thin_artifacts_authenticate_two_different_documents() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let web = stage(dir.path());
    let trusted_root = br#"{"_type":"root","signed":{"version":1}}"#;
    let repository = "https://updates.example.com/acme";
    let app_id = common::app().id.clone();

    let address = |pin: ReleasePin| {
        let trust = OnlineTrust::new(app_id.clone(), "stable", repository, trusted_root, pin);
        trust.release_document().expect("addressable")
    };

    let pinned = address(ReleasePin::Version {
        version: "1.4.0".to_owned(),
    });
    let channel = address(ReleasePin::Channel {
        channel: "stable".to_owned(),
    });
    assert_eq!(pinned.to_string(), "releases/stable/versions/1.4.0.json");
    assert_eq!(channel.to_string(), "releases/stable.json");
    assert_ne!(pinned, channel, "two files, two promises");

    // Both names exist in the tree a single publish produced, which is the point:
    // one publication serves both artifacts, and the difference is entirely in
    // which name each one authenticates.
    for document in [&pinned, &channel] {
        assert!(
            web.join(document.to_string()).is_file(),
            "{} is not in the staged tree",
            document
        );
    }

    // A version the channel has moved past is still addressable, because nothing
    // removes the version-addressed document. That is what a pinned installer
    // depends on: it asks for `1.3.0` and must get `1.3.0`, not whatever the
    // channel says today.
    let older = address(ReleasePin::Version {
        version: "1.3.0".to_owned(),
    });
    assert_eq!(older.to_string(), "releases/stable/versions/1.3.0.json");
    assert_ne!(
        older, channel,
        "a version pin is a different document from the channel even when the channel names it"
    );
}
