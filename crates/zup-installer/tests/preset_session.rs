//! The production chain, from a host that launches a preset to a real child
//! process answering over the real transport.
//!
//! Everything here crosses a process boundary: a host that creates the endpoint,
//! a child executable that collects it, the `UiHello` and `HostHello` a shipped
//! preset exchanges, the first snapshot, an action the child sends, and the state
//! machine validating it. The peer is a real binary built against the public
//! `zup-ui-sdk`, not an in-process fake, because a fake can only prove that the
//! code agrees with itself.
//!
//! What this cannot cover is the window itself. Between "the host launched a
//! preset" and "a person sees something" there is GPUI's own startup, and a
//! headless environment has no display for it. Everything the host controls is
//! on this side of that line, and the child is a real process for all of it.

#![cfg(windows)]

mod support {
    pub mod project;
}

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use zup_core::{
    AppId, Component, ComponentId, Install, InstallDirectory, InstallScope, Installer,
    NonEmptyString, SelectedScope, TargetTriple, Template, UiAsset, UiPreset,
};
use zup_installer::host::{HostDecision, HostState, preset};
use zup_ui_protocol::{HostOffers, UI_PROTOCOL_VERSION, UiAction, UiCapabilities, UiCapability};

/// The child this test launches.
///
/// `zup-preset-test` is a package of its own, because a preset is a project of
/// its own and because the runtime's feature set is exactly its three
/// presentations - a fourth feature that only added a test binary would have
/// broken that. It is staged by `cargo xtask toolchain build` and read from
/// there, so a test run is one build and two tests cannot race for the peer.
fn child() -> PathBuf {
    support::project::test_preset()
}

/// An application with one optional component, so the child's first action names
/// something the snapshot actually published.
fn installer() -> Installer {
    Installer {
        preset: None,
        app: zup_core::App {
            id: AppId::new("com.acme.e2e").expect("id"),
            name: NonEmptyString::new("Acme").expect("name"),
            version: semver::Version::parse("2.1.0").expect("version"),
            publisher: None,
            main: None,
            description: None,
        },
        target: TargetTriple::parse(zup_plugin_contract::HOST_TARGET).expect("the host's target"),
        frontend: zup_core::Frontend::Gui,
        updates: None,
        install: Install {
            scope: InstallScope::User,
            directory: InstallDirectory {
                user: Some(Template::parse("${location.programs}/Acme").expect("a template")),
                machine: None,
            },
            allow_directory_override: true,
        },
        prerequisites: Vec::new(),
        components: vec![
            Component {
                id: ComponentId::new("core").expect("id"),
                name: NonEmptyString::new("Core").expect("name"),
                description: None,
                required: true,
                default: true,
                requires: Vec::new(),
                group: None,
            },
            Component {
                id: ComponentId::new("docs").expect("id"),
                name: NonEmptyString::new("Documentation").expect("name"),
                description: None,
                required: false,
                default: false,
                requires: Vec::new(),
                group: None,
            },
        ],
        component_groups: Vec::new(),
        plugins: Vec::new(),
        files: Vec::new(),
        launchers: Vec::new(),
        path: Vec::new(),
        services: Vec::new(),
        protocols: Vec::new(),
        file_associations: Vec::new(),
    }
}

/// The host state a fresh install opens on.
fn install_state(installer: &Installer) -> HostState {
    HostState::install(
        zup_installer::host::product(installer),
        zup_installer::host::install_options(installer, SelectedScope::User, None, None, None),
        zup_installer::host::capabilities(installer, false),
    )
}

/// What the child is told, and where it reports having been told.
///
/// The report path is one of the preset's own settings, so the child writes it
/// because its configuration says to - not because the test arranged for it.
fn configuration(
    report: &Path,
    assets: BTreeMap<String, String>,
) -> zup_ui_protocol::UiConfiguration {
    zup_ui_protocol::UiConfiguration {
        settings: serde_json::json!({
            "hero": "Install Acme",
            "logo": "branding/logo.svg",
            "report": report.to_string_lossy(),
        }),
        assets,
    }
}

/// One real session, and everything the host observed while it ran.
struct Session {
    decisions: Vec<String>,
    /// Whether the child was still alive when the host stopped reading.
    running_after: bool,
}

/// Run the production loop against one real child.
fn run_session(state: &mut HostState, configuration: &zup_ui_protocol::UiConfiguration) -> Session {
    let mut process = preset::launch(
        &child(),
        state.capabilities().clone(),
        state.snapshot().product.clone(),
    )
    .expect("the host creates the endpoint and the real child collects it");

    process
        .publish(configuration.clone(), Box::new(state.snapshot().clone()))
        .expect("the first snapshot and the settings cross the process boundary");

    let reader = process.take_reader();
    let mut decisions = Vec::new();
    while let Some(action) = reader.next() {
        decisions.push(describe(state.accept(action.clone())));
        if action == UiAction::Close {
            break;
        }
    }
    Session {
        decisions,
        running_after: process.is_running(),
    }
}

/// A one-word account of a decision, so a failure names which step was refused.
fn describe(decision: HostDecision) -> String {
    match decision {
        HostDecision::Run { .. } => "run".into(),
        HostDecision::Preview(_) => "preview".into(),
        HostDecision::Update => "update".into(),
        HostDecision::Cancel => "cancel".into(),
        HostDecision::OpenLog => "open-log".into(),
        HostDecision::CopyDiagnostics => "diagnostics".into(),
        HostDecision::Acknowledged => "acknowledged".into(),
        HostDecision::Refused(_) => "refused".into(),
    }
}

/// A real child completes the handshake, receives the settings and the snapshot,
/// and has its action validated by the state machine.
///
/// The peer is the `zup-installer-test-preset` binary, built from the public SDK,
/// spawned by the production `launch`. Nothing here mocks either side.
#[test]
fn a_real_child_completes_the_handshake_and_its_action_is_validated() {
    let directory = tempfile::tempdir().expect("a directory");
    let report = directory.path().join("report.txt");
    let installer = installer();
    let mut state = install_state(&installer);
    assert_eq!(
        state.snapshot().surface.components().len(),
        2,
        "the host published components for the child to act on"
    );

    let session = run_session(&mut state, &configuration(&report, BTreeMap::new()));

    let report = std::fs::read_to_string(&report).expect("the child reported what it received");
    assert_eq!(
        session.decisions,
        ["acknowledged", "run", "acknowledged"],
        "the child chose a component, asked to install, and closed; it said: {report}"
    );
    assert!(
        session.running_after,
        "the child ended the session itself; the host did not have to kill it"
    );
    assert!(
        report.contains("hero=Install Acme"),
        "the settings crossed the boundary and were deserialized into the child's own type: \
         {report}"
    );
    assert!(
        report.contains("host=Acme"),
        "the host named itself over the protocol: {report}"
    );
    assert!(
        report.contains("components=2"),
        "the first snapshot carried the surface: {report}"
    );
}

/// The application-provided asset reaches the child as a logical identity backed
/// by the host's materialized bytes, and the child never learns a project path.
#[test]
fn an_application_asset_reaches_the_child_as_verified_bytes() {
    let directory = tempfile::tempdir().expect("a directory");
    let report = directory.path().join("report.txt");
    // A path the child is given, standing in for what the host materialized
    // beside itself. What matters is that the child reads a file and that the
    // name it asked for is the name it got.
    let materialized = directory.path().join("logo.svg");
    let bytes = b"<svg xmlns='http://www.w3.org/2000/svg' width='8' height='8'/>";
    std::fs::write(&materialized, bytes).expect("the asset");

    let mut assets = BTreeMap::new();
    assets.insert(
        "branding/logo.svg".to_owned(),
        materialized.to_string_lossy().into_owned(),
    );

    let installer = installer();
    let mut state = install_state(&installer);
    run_session(&mut state, &configuration(&report, assets));

    let report = std::fs::read_to_string(&report).expect("the child reported");
    assert!(
        report.contains("asset=branding/logo.svg"),
        "the name the settings used is the name the child resolved: {report}"
    );
    assert!(
        report.contains(&format!("asset-bytes={}", bytes.len())),
        "the child read the application's own bytes through the SDK: {report}"
    );
}

/// A child may only ask for a component the host actually published. A preset is
/// native code and is not sandboxed, but a component nobody offered is a dead
/// control rather than an install, and the host says so instead of guessing.
#[test]
fn a_component_the_host_never_published_is_refused() {
    let installer = installer();
    let mut state = install_state(&installer);
    let decision = state.accept(UiAction::SetComponent {
        component: zup_ui_protocol::ComponentId::new("not-published").expect("an id"),
        selected: true,
    });
    assert!(
        matches!(decision, HostDecision::Refused(_)),
        "a component outside the snapshot is refused, not carried out"
    );
}

/// The host refuses a preset whose requirements it cannot meet, before launching
/// anything. A window that never opens, with a reason, is better than one that
/// opens and shows a control that does nothing.
#[test]
fn a_preset_this_host_cannot_present_is_refused_before_it_is_launched() {
    let installer = installer();
    let preset = UiPreset {
        name: NonEmptyString::new("needy").expect("name"),
        version: semver::Version::parse("1.0.0").expect("version"),
        protocol: UI_PROTOCOL_VERSION,
        required_capabilities: vec![UiCapability::Updates.to_string()],
        settings: serde_json::json!({}),
        assets: Vec::new(),
    };
    let offered = zup_artifact::ui::offers_for(&installer, false);
    assert!(
        !offered.contains(UiCapability::Updates),
        "this application configures no updates, so it cannot offer that capability"
    );

    let error = preset::materialize(
        &preset::Source::Installed {
            directory: Path::new("nowhere"),
            runtime: &zup_core::UiRuntime {
                executable: zup_core::hash_bytes(b"a preset"),
                preset,
            },
        },
        &offered,
    )
    .expect_err("a preset needing updates cannot be presented here");
    assert!(
        error.to_string().contains("updates"),
        "the refusal names what was missing: {error}"
    );
}

/// A preset from another protocol generation is its own refusal, distinct from a
/// missing capability: adding a capability would not have helped.
#[test]
fn a_preset_from_another_protocol_generation_is_its_own_refusal() {
    let host = HostOffers::new(UiCapabilities::new(UiCapability::ALL.iter().copied()));
    let error = host
        .check(UI_PROTOCOL_VERSION + 1, &UiCapabilities::default())
        .expect_err("a preset built against another generation");
    assert!(
        matches!(error, zup_ui_protocol::Incompatible::Protocol { .. }),
        "{error}"
    );
}

/// A preset whose assets the host cannot materialize is refused rather than
/// launched with a logo that is not there.
#[test]
fn a_preset_whose_assets_are_absent_is_refused() {
    let installer = installer();
    let offered = zup_artifact::ui::offers_for(&installer, false);
    let preset = UiPreset {
        name: NonEmptyString::new("needy").expect("name"),
        version: semver::Version::parse("1.0.0").expect("version"),
        protocol: UI_PROTOCOL_VERSION,
        required_capabilities: Vec::new(),
        settings: serde_json::json!({}),
        assets: vec![UiAsset {
            name: NonEmptyString::new("branding/logo.svg").expect("a name"),
            size: 6,
            sha256: zup_core::hash_reader(b"<svg/>".as_slice())
                .expect("a hash of six bytes")
                .1,
        }],
    };
    let error = preset::materialize(
        &preset::Source::Installed {
            directory: Path::new("nowhere"),
            runtime: &zup_core::UiRuntime {
                executable: zup_core::hash_bytes(b"a preset"),
                preset,
            },
        },
        &offered,
    )
    .expect_err("there is no content to materialize the asset from");
    assert!(
        error.to_string().contains("branding/logo.svg"),
        "the refusal names the asset: {error}"
    );
}

/// The whole chain, from a published package to a host launching what it carried.
///
/// The property this exists to prove: the bytes a build selected out of a
/// `.zupui` are the bytes the host executes. Everything between is real - a
/// package written by the publisher's own writer, an installer composed by the
/// production composer around this package's own runtime template, a bundle read
/// back out of that executable, and a child launched from what the host
/// materialized from it.
#[test]
fn a_composed_installer_launches_the_preset_its_package_carried() {
    let directory = tempfile::tempdir().expect("a directory");
    let child_bytes = std::fs::read(child()).expect("the child preset is built");

    // A real package, written the way `zup ui pack` writes one.
    let mut writer = zup_artifact::ui::PresetPackageWriter::new(
        zup_ui_protocol::PresetDescription::new("e2e", "1.0.0", settings_schema())
            .with_capabilities(UiCapabilities::new([UiCapability::Components])),
    )
    .expect("a valid description");
    writer
        .add_binary(
            TargetTriple::parse(zup_plugin_contract::HOST_TARGET).expect("a target"),
            child_bytes.clone(),
        )
        .expect("one binary for the host");
    let package = writer.finish().expect("a verified package");
    let view = zup_artifact::ui::PresetPackageView::open(package).expect("opens");
    view.verify().expect("verifies");
    // What a build selects out of it: the target's native binary, not the package.
    let selected = view
        .binary_for(&TargetTriple::parse(zup_plugin_contract::HOST_TARGET).expect("a target"))
        .expect("the package has this target");
    assert_eq!(
        selected, child_bytes,
        "the selection is the preset executable"
    );

    // The application's asset, read and hashed the way a build reads it.
    let logo = directory.path().join("logo.svg");
    let logo_bytes = b"<svg xmlns='http://www.w3.org/2000/svg'/>";
    std::fs::write(&logo, logo_bytes).expect("the asset");
    let (size, digest) =
        zup_core::hash_reader(std::fs::File::open(&logo).expect("open")).expect("the asset hashes");

    let mut installer = installer();
    installer.preset = Some(UiPreset {
        name: NonEmptyString::new("e2e").expect("name"),
        version: semver::Version::parse("1.0.0").expect("version"),
        protocol: UI_PROTOCOL_VERSION,
        required_capabilities: vec![UiCapability::Components.to_string()],
        settings: serde_json::json!({
            "hero": "Install Acme",
            "logo": "branding/logo.svg",
            "report": directory.path().join("report.txt").to_string_lossy(),
        }),
        assets: vec![UiAsset {
            name: NonEmptyString::new("logo").expect("name"),
            size,
            sha256: digest,
        }],
    });
    let plan = zup_core::TargetBuildPlan {
        installer: installer.clone(),
        prerequisites: Vec::new(),
        plugins: Vec::new(),
        files: Vec::new(),
        ui_assets: vec![zup_core::ResolvedAsset {
            name: NonEmptyString::new("logo").expect("name"),
            source: Some(logo.clone()),
            source_relative: None,
            size,
            sha256: digest,
        }],
        total_size: 0,
        prerequisite_size: 0,
    };

    // A real installer, composed around this package's own runtime template.
    let output = directory.path().join("Setup.exe");
    zup_windows::build_self_contained_executable(
        Path::new(env!("CARGO_BIN_EXE_zup-setup-gui")),
        &output,
        &plan,
        &[],
        Some(&selected),
    )
    .expect("the installer composes");

    // The composed executable carries exactly the package's target binary.
    let bundle = zup_windows::EmbeddedBundle::open(&output).expect("the installer opens");
    assert_eq!(
        bundle.preset().expect("the installer carries a preset"),
        child_bytes,
        "the bytes composed are the bytes the package carried for this target"
    );
    assert_eq!(
        view.binary_for(&plan.installer.target)
            .expect("the package has it"),
        selected,
        "and the package is what the publisher wrote"
    );

    // The host reads the composed installer and materializes from it.
    let offered = zup_artifact::ui::offers_for(&installer, false);
    let composed = preset::materialize(
        &preset::Source::Composed {
            executable: &output,
            preset: installer.preset.as_ref().expect("a preset"),
            bundle: &bundle,
        },
        &offered,
    )
    .expect("the host materializes the preset and its assets");
    assert_eq!(
        std::fs::read(&composed.executable).expect("the preset is on disk"),
        child_bytes,
        "the executable the host will launch is the package's binary, byte for byte"
    );
    assert_eq!(
        composed.configuration.settings["hero"], "Install Acme",
        "the application's settings are what the host will hand over"
    );

    // And the real session runs against it, with the host launching what it
    // materialized rather than the build tree's copy.
    let mut state = HostState::install(
        zup_installer::host::product(&installer),
        zup_installer::host::install_options(&installer, SelectedScope::User, None, None, None),
        offered.clone(),
    );
    let mut process = preset::launch(
        &composed.executable,
        state.capabilities().clone(),
        state.snapshot().product.clone(),
    )
    .expect("the preset the installer carried collects the endpoint");
    process
        .publish(
            composed.configuration.clone(),
            Box::new(state.snapshot().clone()),
        )
        .expect("the configuration crosses the process boundary");
    let reader = process.take_reader();
    let mut decisions = Vec::new();
    while let Some(action) = reader.next() {
        decisions.push(match state.accept(action.clone()) {
            HostDecision::Run { .. } => "run",
            HostDecision::Acknowledged => "acknowledged",
            other => Box::leak(format!("{other:?}").into_boxed_str()),
        });
        if action == UiAction::Close {
            break;
        }
    }
    assert_eq!(
        decisions,
        ["acknowledged", "run", "acknowledged"],
        "the preset the installer carried drove a real session"
    );

    let report =
        std::fs::read_to_string(directory.path().join("report.txt")).expect("the preset reported");
    assert!(
        report.contains("hero=Install Acme"),
        "the settings the build composed reached the preset the installer carries: {report}"
    );
    assert!(
        report.contains(&format!("asset-bytes={}", logo_bytes.len())),
        "and so did the application's asset, read from what the host materialized: {report}"
    );
}

/// A schema with the shape a real preset generates.
fn settings_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "hero": { "type": ["string", "null"] },
            "logo": {
                "anyOf": [
                    { "$ref": "#/$defs/AssetRef" },
                    { "type": "null" }
                ]
            }
        },
        "$defs": {
            "AssetRef": { "type": "string", "title": "Asset", "x-zup-asset": true }
        }
    })
}
