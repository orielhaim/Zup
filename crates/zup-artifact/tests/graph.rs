//! The artifact graph, end to end.
//!
//! These tests are about the properties the milestone exists for: one copy of
//! shared content, deterministic bytes, a reader that fails closed, and
//! composition that refuses what it cannot compose.

mod common;

use std::collections::BTreeSet;

use common::{ARM64, X64, build_target};
use sha2::Digest as _;
use tempfile::TempDir;
use zup_artifact::{
    ArtifactComposer, ArtifactError, ArtifactIndex, ArtifactRequest, ArtifactView, BlobEntry,
    BlobTable, Compatibility, CompatibilityDimension, DistributionVariant, FileSegments,
    HostArchitecture, HostExecution, HostVersion, MediaType, MemorySegments, MetadataSet, Platform,
    PlatformOs, ReleaseManifest, SegmentSource, VariantRequirements, select_from_index,
};
use zup_core::{InstallScope, Sha256Digest, TargetTriple};

fn request(id: &str) -> ArtifactRequest {
    ArtifactRequest::universal_offline(id, &common::app(), "Acme-Windows-Setup.exe")
}

fn compose(variants: &[DistributionVariant]) -> zup_artifact::ArtifactGraph {
    ArtifactComposer::new(request("windows"), &refs(variants))
        .expect("variants compose")
        .compose(&refs(variants))
        .expect("graph composes")
}

fn refs(variants: &[DistributionVariant]) -> Vec<&DistributionVariant> {
    variants.iter().collect()
}

fn fixture() -> (TempDir, Vec<DistributionVariant>) {
    let root = TempDir::new().unwrap();
    let variants = vec![
        build_target(root.path().join(X64.profile), &X64),
        build_target(root.path().join(ARM64.profile), &ARM64),
    ];
    (root, variants)
}

/// Read a composed graph back through the same parser the dispatcher and the
/// inspector use, over the same segment files a container would carry.
fn open(graph: &zup_artifact::ArtifactGraph) -> ArtifactView<FileSegments> {
    let index_bytes = graph.index_bytes().unwrap();
    let table_bytes = graph.table_bytes().unwrap();
    let mut metadata = MetadataSet::new();
    for (id, bytes) in graph.manifest_bytes().unwrap() {
        let descriptor = graph
            .index()
            .variant(&id)
            .map(|variant| variant.manifest)
            .expect("index names the manifest");
        metadata.insert(&descriptor, bytes).unwrap();
    }
    for (id, bytes) in graph.runtime_bytes().unwrap() {
        let descriptor = graph
            .index()
            .variant(&id)
            .and_then(|variant| variant.runtime)
            .expect("index names the runtime");
        metadata.insert(&descriptor, bytes).unwrap();
    }
    ArtifactView::open(
        &index_bytes,
        &table_bytes,
        metadata,
        graph.segments().unwrap(),
    )
    .unwrap()
}

#[test]
fn a_graph_round_trips_through_the_runtime_parser() {
    let (_root, variants) = fixture();
    let graph = compose(&variants);
    let view = open(&graph);
    assert_eq!(view.index().artifact.id, "windows");
    assert_eq!(view.index().artifact.output, "Acme-Windows-Setup.exe");
    assert_eq!(view.index().variants.len(), 2);
    for id in view.index().variant_ids() {
        let manifest = view.verify_variant(id).unwrap();
        assert_eq!(
            manifest.target.as_str(),
            view.index().variant(id).unwrap().target.as_str()
        );
        assert!(view.variant_runtime(id).unwrap().is_some());
    }
    view.store().verify_all().unwrap();
}

#[test]
fn shared_content_is_stored_once_and_referenced_by_both_variants() {
    let (_root, variants) = fixture();
    let graph = compose(&variants);
    let savings = graph.savings();

    let stored: BTreeSet<Sha256Digest> =
        graph.table().entries().map(|entry| entry.digest).collect();
    let referenced: BTreeSet<Sha256Digest> = variants
        .iter()
        .flat_map(|variant| variant.content_digests())
        .collect();
    assert_eq!(
        stored, referenced,
        "the store holds exactly the referenced set"
    );
    assert_eq!(stored.len() as u64, savings.unique_blob_count);
    assert!(
        stored.len()
            < variants
                .iter()
                .map(|v| v.content_digests().len())
                .sum::<usize>(),
        "the two target trees really do overlap, so deduplication has something to do"
    );
    assert_eq!(
        savings.unique_variant_size,
        savings.shared_size + savings.exclusive_size,
        "every distinct byte is either shared or exclusive"
    );
    assert!(savings.shared_size > 0);
    assert!(savings.exclusive_size > 0);
    assert!(
        savings.standalone_size > savings.unique_variant_size,
        "separate artifacts would pay for the shared bytes once per variant"
    );
    assert!(savings.deduplicated() > 0);
}

#[test]
fn composition_is_deterministic() {
    let (_root, variants) = fixture();
    let first = compose(&variants);
    let second = compose(&variants);
    assert_eq!(first.index_bytes().unwrap(), second.index_bytes().unwrap());
    assert_eq!(first.table_bytes().unwrap(), second.table_bytes().unwrap());
    assert_eq!(
        first.manifest_bytes().unwrap(),
        second.manifest_bytes().unwrap()
    );
    assert_eq!(first.table(), second.table());
    assert_eq!(first.savings(), second.savings());
}

#[test]
fn variant_descriptors_are_deterministic_regardless_of_input_order() {
    let (_root, mut variants) = fixture();
    let forward = compose(&variants);
    variants.reverse();
    let reverse = compose(&variants);
    assert_eq!(
        forward.index_bytes().unwrap(),
        reverse.index_bytes().unwrap()
    );
    assert_eq!(
        forward.table_bytes().unwrap(),
        reverse.table_bytes().unwrap()
    );
    assert_eq!(
        forward
            .index()
            .variants
            .iter()
            .map(|variant| variant.id.as_str())
            .collect::<Vec<_>>(),
        vec!["windows-arm64", "windows-x64"],
        "variants are ordered by id, not by the order they were composed in"
    );
}

#[test]
fn a_frontend_mismatch_is_refused_with_a_named_dimension() {
    let root = TempDir::new().unwrap();
    let headless = build_target(
        root.path().join("arm-console"),
        &common::FixtureTarget {
            profile: "windows-arm64",
            target: "aarch64-pc-windows-msvc",
            frontend: zup_core::Frontend::Headless,
            exclusive: common::ARM64.exclusive,
            component: None,
            plugin: None,
        },
    );
    let variants = [build_target(root.path().join("x64"), &X64), headless];
    let error = ArtifactComposer::new(request("windows"), &refs(&variants)).unwrap_err();
    assert!(
        matches!(&error, ArtifactError::Incompatible(incompatible)
            if incompatible.reason.dimension() == CompatibilityDimension::LauncherSubsystem),
        "{error}"
    );
    assert!(error.to_string().contains("launcher subsystem"), "{error}");
}

#[test]
fn console_and_headless_share_a_subsystem_and_compose() {
    let root = TempDir::new().unwrap();
    let x64 = build_target(
        root.path().join("x64"),
        &common::FixtureTarget {
            profile: "windows-x64",
            target: "x86_64-pc-windows-msvc",
            frontend: zup_core::Frontend::Console,
            exclusive: common::X64.exclusive,
            component: None,
            plugin: None,
        },
    );
    let arm = build_target(
        root.path().join("arm"),
        &common::FixtureTarget {
            profile: "windows-arm64",
            target: "aarch64-pc-windows-msvc",
            frontend: zup_core::Frontend::Headless,
            exclusive: common::ARM64.exclusive,
            component: None,
            plugin: None,
        },
    );
    let graph = compose(&[x64, arm]);
    assert_eq!(
        graph.index().artifact.subsystem,
        zup_artifact::LauncherSubsystem::Console
    );
}

#[test]
fn an_install_scope_mismatch_is_refused() {
    let root = TempDir::new().unwrap();
    let mut install = common::install();
    let x64 = build_target(root.path().join("x64"), &X64).with_install(install.clone());
    install.scope = InstallScope::Machine;
    let arm = build_target(root.path().join("arm"), &ARM64).with_install(install);
    let variants = [x64, arm];
    let error = ArtifactComposer::new(request("windows"), &refs(&variants)).unwrap_err();
    assert!(
        matches!(&error, ArtifactError::Incompatible(incompatible)
            if incompatible.reason.dimension() == CompatibilityDimension::InstallerSemantics),
        "{error}"
    );
}

#[test]
fn an_application_version_mismatch_is_refused() {
    let root = TempDir::new().unwrap();
    let x64 = build_target(root.path().join("x64"), &X64);
    let arm = build_target(root.path().join("arm"), &ARM64).with_version("1.5.0");
    let variants = [x64, arm];
    let error = ArtifactComposer::new(request("windows"), &refs(&variants)).unwrap_err();
    assert!(
        matches!(&error, ArtifactError::Incompatible(incompatible)
            if incompatible.reason.dimension() == CompatibilityDimension::ApplicationIdentity),
        "{error}"
    );
}

#[test]
fn a_different_operating_system_is_refused() {
    let root = TempDir::new().unwrap();
    let x64 = build_target(root.path().join("x64"), &X64);
    let linux = build_target(
        root.path().join("linux"),
        &common::FixtureTarget {
            profile: "linux-x64",
            target: "x86_64-unknown-linux-gnu",
            frontend: zup_core::Frontend::Gui,
            exclusive: common::X64.exclusive,
            component: None,
            plugin: None,
        },
    );
    let variants = [x64, linux];
    let error = ArtifactComposer::new(request("windows"), &refs(&variants)).unwrap_err();
    assert!(
        matches!(&error, ArtifactError::Incompatible(incompatible)
            if incompatible.reason.dimension() == CompatibilityDimension::Platform),
        "{error}"
    );
}

#[test]
fn an_offline_artifact_without_a_runtime_is_incomplete() {
    let root = TempDir::new().unwrap();
    let x64 = build_target(root.path().join("x64"), &X64);
    let without_runtime = DistributionVariant::resolve(
        &x64_resolved(&x64, root.path()),
        &zup_build::TargetBuildPlan {
            installer: x64.plan().installer.clone(),
            prerequisites: Vec::new(),
            plugins: Vec::new(),
            files: Vec::new(),
            total_size: 0,
            prerequisite_size: 0,
        },
        &[],
        None,
    )
    .unwrap();
    let error = ArtifactComposer::new(
        request("windows"),
        &refs(std::slice::from_ref(&without_runtime)),
    )
    .unwrap()
    .compose(&refs(std::slice::from_ref(&without_runtime)))
    .unwrap_err();
    assert!(matches!(error, ArtifactError::Incomplete { .. }), "{error}");
    assert!(error.to_string().contains("native runtime"), "{error}");
}

fn x64_resolved(
    variant: &DistributionVariant,
    root: &std::path::Path,
) -> zup_core::ResolvedTargetConfig {
    zup_core::ResolvedTargetConfig {
        profile: variant.profile().clone(),
        target: variant.target().clone(),
        source: zup_core::Source::new(root.to_path_buf()).unwrap(),
        frontend: variant.frontend(),
        install: variant.install().clone(),
    }
}

#[test]
fn a_corrupt_artifact_index_is_refused() {
    let (_root, variants) = fixture();
    let graph = compose(&variants);

    // Truncated: the shape a container that lost its tail produces.
    let bytes = graph.index_bytes().unwrap();
    let error = ArtifactIndex::parse(&bytes[..bytes.len() / 2]).unwrap_err();
    assert!(matches!(error, ArtifactError::Json(_)), "{error}");

    // A platform that disagrees with the target it was derived from: the kind of
    // drift a hand-edited or repackaged index introduces.
    let mut index = graph.index().clone();
    index.variants[1].platform.architecture = "aarch64".to_owned();
    let error = ArtifactIndex::parse(&index.encode().unwrap()).unwrap_err();
    assert!(matches!(error, ArtifactError::Invalid), "{error}");

    // Variants out of order: the file order of the graph must not be able to
    // change what a selector sees.
    let mut index = graph.index().clone();
    index.variants.swap(0, 1);
    let error = ArtifactIndex::parse(&index.encode().unwrap()).unwrap_err();
    assert!(matches!(error, ArtifactError::Invalid), "{error}");
}

#[test]
fn an_index_with_a_wrong_schema_is_refused() {
    let (_root, variants) = fixture();
    let graph = compose(&variants);
    let mut index = graph.index().clone();
    index.schema = 2;
    assert!(matches!(
        ArtifactIndex::parse(&index.encode().unwrap()),
        Err(ArtifactError::Invalid)
    ));
}

#[test]
fn an_index_naming_an_unknown_feature_fails_closed() {
    let (_root, variants) = fixture();
    let graph = compose(&variants);
    let mut index = graph.index().clone();
    index.required_features |= 1 << 40;
    let error = ArtifactIndex::parse(&index.encode().unwrap()).unwrap_err();
    assert!(
        matches!(error, ArtifactError::UnsupportedFeatures { unknown, .. } if unknown == 1 << 40),
        "{error}"
    );
}

#[test]
fn a_blob_whose_bytes_are_not_its_digest_is_never_served() {
    let (_root, variants) = fixture();
    let graph = compose(&variants);
    let entry = graph.table().entries().next().copied().unwrap();

    // A segment holding a valid frame of entirely different content, described
    // by an entry that still claims the original digest.
    let wrong = b"not the blob";
    let compressed = zstd::stream::encode_all(std::io::Cursor::new(wrong.as_slice()), 9).unwrap();
    let mut segments = MemorySegments::new();
    segments.insert(entry.segment, compressed.clone());
    let forged = BlobEntry {
        digest: entry.digest,
        segment: 0,
        offset: 0,
        compressed_size: compressed.len() as u64,
        size: wrong.len() as u64,
    };
    let table = BlobTable::pack(vec![forged]).unwrap();
    table.validate().unwrap();
    let source = SegmentSource::new(&table, &segments);
    assert!(
        matches!(
            source.blob(&forged),
            Err(ArtifactError::DigestMismatch { .. })
        ),
        "a blob must be refused when its content is not its digest"
    );
}

#[test]
fn a_corrupt_segment_is_never_served() {
    let (_root, variants) = fixture();
    let graph = compose(&variants);
    let view = open(&graph);
    view.store().verify_all().unwrap();

    let segments = graph.segments().unwrap();
    let entry = graph.table().entries().next().copied().unwrap();
    let path = segments.path(entry.segment);
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[entry.offset as usize] ^= 0xff;
    std::fs::write(&path, &bytes).unwrap();

    assert!(
        view.store().blob(&entry).is_err(),
        "a flipped byte in a segment must never yield a blob"
    );
}

#[test]
fn a_missing_segment_is_reported_rather_than_served_as_empty() {
    let (_root, variants) = fixture();
    let graph = compose(&variants);
    let view = open(&graph);
    let segments = graph.segments().unwrap();
    let entry = graph.table().entries().next().copied().unwrap();
    std::fs::remove_file(segments.path(entry.segment)).unwrap();
    assert!(view.store().blob(&entry).is_err());
}

#[test]
fn a_tampered_blob_table_is_refused_before_anything_is_read() {
    let (_root, variants) = fixture();
    let graph = compose(&variants);
    let mut table = graph.table().clone();
    table.blobs[0].size += 1;
    let error = ArtifactView::open(
        &graph.index_bytes().unwrap(),
        &table.encode().unwrap(),
        MetadataSet::new(),
        MemorySegments::new(),
    )
    .unwrap_err();
    assert!(
        matches!(error, ArtifactError::DigestMismatch { .. }),
        "{error}"
    );
}

#[test]
fn a_view_whose_store_is_too_small_is_refused() {
    let (_root, variants) = fixture();
    let graph = compose(&variants);
    let error = ArtifactView::open(
        &graph.index_bytes().unwrap(),
        &graph.table_bytes().unwrap(),
        MetadataSet::new(),
        MemorySegments::new(),
    )
    .unwrap_err();
    assert!(matches!(error, ArtifactError::Incomplete { .. }), "{error}");
}

#[test]
fn a_x64_host_selects_x64_and_an_arm64_host_selects_arm64() {
    let (_root, variants) = fixture();
    let index = compose(&variants);
    let index = index.index();
    let x64_host = HostExecution {
        os: PlatformOs::Windows,
        native: HostArchitecture::X86_64,
        emulated: vec![HostArchitecture::X86],
        version: Some(HostVersion::new(10, 0, 22621)),
    };
    let arm_host = HostExecution {
        os: PlatformOs::Windows,
        native: HostArchitecture::Arm64,
        emulated: vec![HostArchitecture::X86_64, HostArchitecture::X86],
        version: Some(HostVersion::new(11, 0, 0)),
    };
    assert_eq!(
        select_from_index(&x64_host, index).unwrap().candidate.id,
        "windows-x64"
    );
    assert_eq!(
        select_from_index(&arm_host, index).unwrap().candidate.id,
        "windows-arm64"
    );
}

#[test]
fn an_arm64_host_prefers_native_arm64_over_emulated_x64() {
    let (_root, variants) = fixture();
    let graph = compose(&variants);
    let host = HostExecution {
        os: PlatformOs::Windows,
        native: HostArchitecture::Arm64,
        emulated: vec![HostArchitecture::X86_64],
        version: Some(HostVersion::new(11, 0, 0)),
    };
    let selection = select_from_index(&host, graph.index()).unwrap();
    assert_eq!(selection.candidate.id, "windows-arm64");
    assert_eq!(selection.compatibility, Compatibility::Native);
}

#[test]
fn an_unsupported_host_gets_a_useful_error() {
    let (_root, variants) = fixture();
    let graph = compose(&variants);
    let host = HostExecution::native_only(PlatformOs::Linux, HostArchitecture::X86_64);
    let error = select_from_index(&host, graph.index()).unwrap_err();
    assert!(error.is_unsupported_host());
    assert!(error.to_string().contains("windows"), "{error}");
}

#[test]
fn a_variant_with_a_machine_component_is_not_selected_through_emulation() {
    let root = TempDir::new().unwrap();
    let arm =
        build_target(root.path().join("arm"), &ARM64).with_requirements(VariantRequirements {
            native_execution: true,
            capabilities: vec![zup_artifact::PlatformCapability::MachineComponents],
            minimum_host: None,
        });
    let graph = compose(&[arm]);
    let host = HostExecution {
        os: PlatformOs::Windows,
        native: HostArchitecture::X86_64,
        emulated: vec![HostArchitecture::Arm64],
        version: Some(HostVersion::new(11, 0, 0)),
    };
    assert!(
        select_from_index(&host, graph.index())
            .unwrap_err()
            .is_unsupported_host()
    );
}

#[test]
fn a_plugin_is_compiled_for_one_target_only_and_selected_through_the_manifest() {
    let (_root, variants) = fixture();
    let graph = compose(&variants);
    let view = open(&graph);
    let x64 = view.variant_manifest("windows-x64").unwrap();
    let arm = view.variant_manifest("windows-arm64").unwrap();
    assert!(
        x64.plan.plugins.is_empty(),
        "the x64 target declares no plugin"
    );
    assert_eq!(arm.plan.plugins.len(), 1);
    let plugin = &arm.plan.plugins[0];
    assert_eq!(
        plugin.target,
        TargetTriple::parse("aarch64-pc-windows-msvc").unwrap(),
        "a plugin is never selected by filename; it is bound to the variant target"
    );
    // The plugin's ahead-of-time content is in the shared store, referenced by
    // digest through the manifest.
    let entry = graph.table().entry(&plugin.blob).expect("stored");
    assert_eq!(entry.size, plugin.aot_size);
    assert_eq!(
        view.store().blob(entry).unwrap().len() as u64,
        plugin.aot_size
    );
    assert!(
        !x64.plan
            .entries
            .iter()
            .any(|entry| entry.blob == plugin.blob),
        "the x64 variant does not reference the ARM64 plugin"
    );
}

#[test]
fn the_release_description_is_deterministic_and_free_of_build_paths() {
    let (_root, variants) = fixture();
    let graph = compose(&variants);
    let mut release = ReleaseManifest::new(&common::app());
    release
        .add_artifact(
            graph.index(),
            "dist/Acme-Windows-Setup.exe",
            measured(&graph, 1234),
        )
        .unwrap();
    let bytes = release.encode().unwrap();
    let text = String::from_utf8(bytes.clone()).unwrap();
    assert!(!text.contains(":\\"), "no absolute path: {text}");
    assert_eq!(ReleaseManifest::parse(&bytes).unwrap(), release);
    assert_eq!(release.artifacts[0].path, "dist/Acme-Windows-Setup.exe");
    assert_eq!(release.variants.len(), 2);
    assert!(
        release
            .variants
            .iter()
            .all(|variant| variant.artifacts == vec!["windows".to_owned()])
    );
    // A build-machine path is refused outright rather than normalized.
    let mut other = ReleaseManifest::new(&common::app());
    assert!(
        other
            .add_artifact(graph.index(), r"C:\build\out\Acme.exe", measured(&graph, 1),)
            .is_err()
    );
}

/// What one output file measured, read off the graph it was composed from.
fn measured(graph: &zup_artifact::ArtifactGraph, size: u64) -> zup_artifact::Measured {
    let savings = graph.savings();
    zup_artifact::Measured::composed(
        Sha256Digest::from_bytes([1; 32]),
        size,
        graph.table().stored_size(),
        graph.table().logical_size(),
        savings.unique_blob_count,
        savings.standalone_size,
        savings.shared_size,
    )
}

#[test]
fn an_oci_layout_exports_the_same_digests_the_installer_verifies() {
    let (_root, variants) = fixture();
    let graph = compose(&variants);
    let root = TempDir::new().unwrap();
    let layout = root.path().join("oci");
    zup_artifact::oci::export_oci_layout(&graph, &layout).unwrap();
    let index = std::fs::read(layout.join("index.json")).unwrap();
    let parsed: oci_spec::image::ImageIndex = serde_json::from_slice(&index).unwrap();
    assert_eq!(parsed.manifests().len(), 2);
    assert!(layout.join("oci-layout").is_file());
    for descriptor in parsed.manifests() {
        let hex = descriptor.digest().to_string();
        let (_, value) = hex.split_once(':').unwrap();
        assert!(
            layout.join("blobs").join("sha256").join(value).is_file(),
            "missing {hex}"
        );
        let platform = descriptor.platform().as_ref().expect("a platform");
        assert!(matches!(platform.os(), oci_spec::image::Os::Windows));
    }
}

#[test]
fn a_platform_derived_from_a_triple_round_trips() {
    for text in ["x86_64-pc-windows-msvc", "aarch64-pc-windows-msvc"] {
        let target = TargetTriple::parse(text).unwrap();
        let platform = Platform::from_triple(&target);
        assert_eq!(platform.triple().unwrap(), target);
        assert_eq!(platform.os, "windows");
    }
}

#[test]
fn a_single_target_artifact_is_still_one_artifact() {
    let root = TempDir::new().unwrap();
    let x64 = build_target(root.path().join("x64"), &X64);
    let graph = ArtifactComposer::new(request("windows-x64"), &refs(std::slice::from_ref(&x64)))
        .unwrap()
        .compose(&refs(std::slice::from_ref(&x64)))
        .unwrap();
    assert_eq!(graph.index().variants.len(), 1);
    assert_eq!(
        graph.savings().shared_size,
        0,
        "one variant shares nothing with itself"
    );
}

#[test]
fn an_in_memory_composition_reports_that_it_has_no_segments() {
    let root = TempDir::new().unwrap();
    let x64 = build_target(root.path().join("x64"), &X64);
    let graph = ArtifactComposer::new(request("windows"), &refs(std::slice::from_ref(&x64)))
        .unwrap()
        .with_storage(zup_artifact::CompositionStorage::Memory)
        .compose(&refs(std::slice::from_ref(&x64)))
        .unwrap();
    assert!(graph.segments().is_err());
    assert_eq!(graph.table().segments, 1);
}

#[test]
fn a_blob_table_round_trips_canonically() {
    let (_root, variants) = fixture();
    let graph = compose(&variants);
    let bytes = graph.table_bytes().unwrap();
    assert_eq!(bytes, BlobTable::parse(&bytes).unwrap().encode().unwrap());
    assert!(graph.index().tables.blobs.verify(&bytes).is_ok());
}

#[test]
fn a_descriptor_is_verified_against_exactly_its_own_content() {
    let digest = Sha256Digest::from_bytes(sha2::Sha256::digest(b"payload").into());
    let entry = BlobTable::pack(vec![BlobEntry {
        digest,
        segment: 0,
        offset: 0,
        compressed_size: 4,
        size: 7,
    }])
    .unwrap();
    let descriptor = zup_artifact::Descriptor {
        media_type: MediaType::BLOB,
        digest,
        size: 7,
    };
    assert!(
        descriptor.verify(b"payload").is_ok(),
        "content matching its own descriptor verifies"
    );
    assert!(descriptor.verify(b"other!!!").is_err());
    assert!(entry.entry(&digest).is_some());
}
