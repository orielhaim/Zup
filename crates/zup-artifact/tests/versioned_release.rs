//! Immutable release references survive channel movement and same-version builds.
#[allow(dead_code)]
mod common;

use common::{FixtureTarget, X64, build_target};
use std::path::Path;
use zup_acquire::{ReleaseDescriptor, document_path};
use zup_artifact::{ArtifactComposer, ArtifactRequest, WebExport, export_web_tree};

fn publish(
    source: &Path,
    web: &Path,
    target: &FixtureTarget,
) -> (zup_artifact::ArtifactGraph, Vec<u8>) {
    let variant = build_target(source, target);
    let request = ArtifactRequest::universal_offline("acme-windows", &common::app(), "Setup.exe");
    let graph = ArtifactComposer::new(request, &[&variant])
        .unwrap()
        .compose(&[&variant])
        .unwrap();
    export_web_tree(&graph, &WebExport::new("stable").unwrap(), web).unwrap();
    let bytes = std::fs::read(web.join("releases/stable.json")).unwrap();
    (graph, bytes)
}

#[test]
fn a_channel_can_advance_without_changing_a_retained_build_with_the_same_version() {
    let root = tempfile::tempdir().unwrap();
    let web = root.path().join("web");
    let (_, first) = publish(&root.path().join("first"), &web, &X64);
    let first_release = ReleaseDescriptor::parse(&first).unwrap();
    let mut retained = vec![(zup_core::hash_bytes(&first), first.clone())];
    for digest in std::iter::once(first_release.catalog.digest).chain(
        first_release
            .variants
            .iter()
            .map(|variant| variant.manifest.digest),
    ) {
        retained.push((
            digest,
            std::fs::read(web.join(document_path(&digest).to_string())).unwrap(),
        ));
    }
    let changed = FixtureTarget {
        exclusive: &[(
            "bin/Acme.exe",
            "a different build with the same display version",
        )],
        ..X64
    };
    let (_, second) = publish(&root.path().join("second"), &web, &changed);
    let second_release = ReleaseDescriptor::parse(&second).unwrap();
    assert_eq!(first_release.version, second_release.version);
    assert_ne!(first_release.release_digest, second_release.release_digest);
    assert_ne!(zup_core::hash_bytes(&first), zup_core::hash_bytes(&second));
    for (digest, expected) in retained {
        let name = document_path(&digest).to_string();
        assert_eq!(std::fs::read(web.join(&name)).unwrap(), expected);
        assert_eq!(
            std::fs::read(web.join("tuf-input").join(name)).unwrap(),
            expected
        );
    }
}

#[test]
fn publication_refuses_a_corrupt_existing_metadata_document() {
    let root = tempfile::tempdir().unwrap();
    let web = root.path().join("web");
    let (graph, bytes) = publish(&root.path().join("source"), &web, &X64);
    let release = ReleaseDescriptor::parse(&bytes).unwrap();
    let path = web.join(document_path(&release.catalog.digest).to_string());
    let size = std::fs::metadata(&path).unwrap().len();
    std::fs::write(&path, vec![b'x'; size as usize]).unwrap();
    assert!(export_web_tree(&graph, &WebExport::new("stable").unwrap(), &web).is_err());
    assert_eq!(std::fs::read(path).unwrap(), vec![b'x'; size as usize]);
}
