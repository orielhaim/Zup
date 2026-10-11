use std::path::{Path, PathBuf};

use zup_artifact::{ArtifactComposer, ArtifactRequest};
use zup_bundle::Package;
use zup_core::Sha256Digest;
use zup_windows::{UniversalArtifact, UniversalError, compose_universal_executable, stage_variant};

mod fixture;

use fixture::{Fixture, app as app_identity};

fn dispatcher_template() -> PathBuf {
    template(zup_xtask::dispatcher::CONSOLE)
}

fn gui_dispatcher_template() -> PathBuf {
    template(zup_xtask::dispatcher::GUI)
}

fn template(name: &str) -> PathBuf {
    zup_xtask::dispatcher::beside_test_executable(
        &std::env::current_exe().expect("test executable path"),
        name,
    )
    .unwrap_or_else(|error| panic!("{error}"))
}

fn compose(root: &Path, fixture: &Fixture) -> (PathBuf, zup_artifact::ArtifactGraph) {
    let request =
        ArtifactRequest::universal_offline("windows", &app_identity(), "Acme-Windows-Setup.exe");
    let composed = fixture
        .variants
        .iter()
        .collect::<Vec<&zup_artifact::DistributionVariant>>();
    let graph = ArtifactComposer::new(request, &composed)
        .expect("composable")
        .compose(&composed)
        .expect("composed");
    let output = root.join("Acme-Windows-Setup.exe");
    compose_universal_executable(&dispatcher_template(), &output, &graph).expect("composed");
    (output, graph)
}

#[test]
fn a_universal_artifact_round_trips_through_the_runtime_parser() {
    let fixture = Fixture::new();
    let root = tempfile::tempdir().unwrap();
    let (output, _graph) = compose(root.path(), &fixture);
    let artifact = UniversalArtifact::open(&output).expect("the artifact opens");
    assert_eq!(artifact.index().artifact.id, "windows");
    assert_eq!(artifact.index().variants.len(), fixture.variants.len());
    for id in artifact.index().variant_ids() {
        let manifest = artifact
            .view()
            .verify_variant(id)
            .expect("every variant is complete");
        assert!(artifact.view().variant_runtime(id).unwrap().is_some());
        assert_eq!(manifest.plan.installer.target.as_str(), {
            artifact.index().variant(id).unwrap().target.as_str()
        });
    }
}

#[test]
fn a_composed_artifact_verifies_every_blob_it_advertises() {
    let fixture = Fixture::new();
    let root = tempfile::tempdir().unwrap();
    let (output, _graph) = compose(root.path(), &fixture);
    let artifact = UniversalArtifact::open(&output).expect("the artifact opens");
    artifact
        .view()
        .store()
        .verify_all()
        .expect("every advertised blob verifies");
}

#[test]
fn a_staged_variant_is_a_package_the_native_runtime_already_reads() {
    let fixture = Fixture::new();
    let root = tempfile::tempdir().unwrap();
    let (output, _graph) = compose(root.path(), &fixture);
    let artifact = UniversalArtifact::open(&output).expect("the artifact opens");
    let id = "windows-x64";
    let store = root.path().join("store");
    let staged = stage_variant(&artifact, id, &store).expect("staged");

    assert!(staged.runtime.is_file());
    let variant = artifact.index().variant("windows-x64").unwrap();
    let runtime = variant.runtime.expect("the variant carries a runtime");
    let staged_bytes = std::fs::read(&staged.runtime).unwrap();
    assert_eq!(staged_bytes.len() as u64, runtime.size);
    let (_, digest) = zup_core::hash_reader(staged_bytes.as_slice()).unwrap();
    assert_eq!(digest, runtime.digest);

    let package = Package::open(&staged.package).expect("the staged package opens");
    assert_eq!(package.plan().installer.target.as_str(), {
        artifact.index().variant(id).unwrap().target.as_str()
    });
    assert!(
        package.plan().plugins.is_empty(),
        "the x64 variant declares no plugin"
    );
}

#[test]
fn a_staged_variant_carries_no_other_architecture_content() {
    let fixture = Fixture::new();
    let root = tempfile::tempdir().unwrap();
    let (output, _graph) = compose(root.path(), &fixture);
    let artifact = UniversalArtifact::open(&output).expect("the artifact opens");
    let store = root.path().join("store-x64");
    let staged = stage_variant(&artifact, "windows-x64", &store).expect("staged");
    let package = Package::open(&staged.package).expect("the staged package opens");

    let arm = artifact
        .view()
        .variant_manifest("windows-arm64")
        .expect("the arm64 manifest reads");
    let x64 = artifact
        .view()
        .variant_manifest("windows-x64")
        .expect("the x64 manifest reads");
    let arm_only: Vec<Sha256Digest> = arm
        .content_digests()
        .into_iter()
        .filter(|digest| !x64.content_digests().contains(digest))
        .collect();
    assert!(!arm_only.is_empty(), "the fixture has ARM64-only content");
    for entry in &package.plan().entries {
        assert!(
            !arm_only.contains(&entry.blob),
            "{} is ARM64-only content",
            entry.path
        );
    }
    for artifact in &package.plan().plugins {
        assert!(
            !arm_only.contains(&artifact.blob),
            "an ahead-of-time plugin is never another architecture's"
        );
    }
}

#[test]
fn this_host_selects_exactly_one_variant_and_it_is_its_own() {
    let fixture = Fixture::new();
    let root = tempfile::tempdir().unwrap();
    let (output, _graph) = compose(root.path(), &fixture);
    let artifact = UniversalArtifact::open(&output).expect("the artifact opens");
    let selection = artifact.select().expect("a variant is selectable");
    let native = zup_windows::native_machine()
        .expect("the host reports its machine")
        .architecture
        .as_str();
    let expected = if native == "aarch64" {
        "windows-arm64"
    } else {
        "windows-x64"
    };
    assert_eq!(selection.id, expected);
    assert!(!selection.emulated, "a supported host selects natively");
}

#[test]
fn a_dispatcher_that_cannot_launch_the_artifact_is_refused() {
    let fixture = Fixture::new();
    let root = tempfile::tempdir().unwrap();
    let (_output, graph) = compose(root.path(), &fixture);

    let error = compose_universal_executable(
        &gui_dispatcher_template(),
        &root.path().join("mismatch.exe"),
        &graph,
    )
    .expect_err("a windowed template cannot produce a console artifact");
    assert!(
        matches!(
            error,
            UniversalError::DispatcherTemplate {
                expected: zup_artifact::LauncherSubsystem::Console,
                found: zup_artifact::LauncherSubsystem::Gui,
            }
        ),
        "{error}"
    );

    let dispatcher = dispatcher_template();
    assert_eq!(
        zup_binary::Executable::read(&dispatcher)
            .expect("the dispatcher is an image")
            .architecture(),
        Some(zup_binary::BinaryArchitecture::X86_32),
        "the dispatcher is built for the narrowest machine Windows runs everywhere"
    );
    let widened = root.path().join("widened-dispatcher.exe");
    let mut widened_bytes = std::fs::read(&dispatcher).unwrap();
    let machine = pe_machine_offset(&widened_bytes);
    widened_bytes[machine..machine + 2].copy_from_slice(&0x8664u16.to_le_bytes());
    std::fs::write(&widened, &widened_bytes).unwrap();

    let too_wide = root.path().join("too-wide.exe");
    let error = compose_universal_executable(&widened, &too_wide, &graph)
        .expect_err("a 64-bit dispatcher cannot start a 32-bit Windows host");
    assert!(
        matches!(
            error,
            UniversalError::DispatcherTooWide {
                found: zup_binary::BinaryArchitecture::X86_64,
                ..
            }
        ),
        "{error}"
    );
    assert!(!too_wide.exists(), "a refused composition writes nothing");
}

fn pe_machine_offset(bytes: &[u8]) -> usize {
    let header = u32::from_le_bytes(bytes[0x3c..0x40].try_into().unwrap()) as usize;
    assert_eq!(&bytes[header..header + 4], b"PE\0\0");
    header + 4
}

#[test]
fn a_missing_resource_is_reported_rather_than_read_as_empty() {
    let fixture = Fixture::new();
    let root = tempfile::tempdir().unwrap();
    let (_output, _graph) = compose(root.path(), &fixture);

    let plain = root.path().join("plain.exe");
    std::fs::write(&plain, b"MZ not really an image").unwrap();
    assert!(UniversalArtifact::open(&plain).is_err());
}
