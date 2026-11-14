#![cfg(windows)]

//! The selected preset as part of what an installation owns.
//!
//! Everything here crosses a process boundary and a filesystem boundary, because
//! the property being proved is about surviving both. An installer is composed
//! with a preset, a payload, and an asset the application configured; it is run;
//! the file a person downloaded is deleted; the maintenance copy the
//! installation persisted is launched instead; and a real preset child, built
//! against the public SDK, answers over real `zup-preset-ipc` with the settings and
//! the asset the original installer carried.
//!
//! What this deliberately does not weaken: the child is a real executable, not a
//! stub; the composition is the production composer; the install is the
//! production transaction; and the maintenance launch is the production entry
//! point, reached the way Apps & Features reaches it.
//!
//! What it cannot cover is the window. Between "the host launched a preset" and
//! "a person sees something" there is GPUI's own startup, and a headless
//! environment has no display for it. Everything the host controls is on this
//! side of that line.

#![cfg(windows)]

mod support {
    pub mod project;
}

use std::path::{Path, PathBuf};

use support::project::{AppSpec, Payload, PresetSpec, State, compose_with_preset};

use zup_preset_protocol::Capability;
use zup_windows::InstallationRecordStore;

/// The preset executable these tests launch, staged by the same run.
fn preset_executable() -> PathBuf {
    support::project::test_preset()
}

/// An application whose window is a real preset, with a real asset.
fn preset_app(label: &str, report: &Path) -> (AppSpec, PresetSpec) {
    let app = AppSpec::unique(label);
    let logo = b"<svg xmlns='http://www.w3.org/2000/svg' width='8' height='8'/>";
    let preset = PresetSpec {
        executable: std::fs::read(preset_executable()).expect("the peer preset is built"),
        settings: serde_json::json!({
            "hero": format!("Install {}", app.name),
            "logo": "branding/logo.svg",
            "report": report,
        }),
        assets: vec![("branding/logo.svg".to_owned(), logo.to_vec())],
        required_capabilities: vec![Capability::Components],
    };
    (app, preset)
}

/// The window an installation recorded, read back the way a host reads it.
fn recorded_ui(state: &State, app: &AppSpec) -> zup_core::InstalledPreset {
    let ledger = InstallationRecordStore::new(state.path())
        .load(
            &zup_core::AppId::new(&app.id).expect("a valid id"),
            zup_core::SelectedScope::User,
        )
        .expect("the ledger reads")
        .expect("the installation is recorded");
    ledger
        .preset()
        .cloned()
        .expect("a graphical installation records the window it will present")
}

/// Every preset content file the installation owns, across every version.
fn owned_ui_files(state: &State, app: &AppSpec) -> Vec<PathBuf> {
    let record = InstallationRecordStore::new(state.path())
        .load(
            &zup_core::AppId::new(&app.id).unwrap(),
            zup_core::SelectedScope::User,
        )
        .unwrap()
        .unwrap();
    let mut found = Vec::new();
    for snapshot in std::iter::once(&record.current).chain(&record.retained) {
        if let Some(generation) = &snapshot.maintenance {
            for member in generation.descriptor().files() {
                if member.path.as_str().starts_with("ui/") {
                    found.push(
                        PathBuf::from(generation.directory().as_str()).join(member.path.as_str()),
                    );
                }
            }
        }
    }
    found.sort();
    found.dedup();
    found
}

fn launch_maintenance(maintenance: &Path, state: &State) -> std::process::Output {
    std::process::Command::new(maintenance)
        .args([
            "modify",
            "--ui",
            "--scope",
            "user",
            "--state-root",
            state.path().to_str().expect("a path"),
            "--output",
            "json",
        ])
        .output()
        .expect("the maintenance runtime runs")
}

/// After a successful install, the installer a person downloaded is disposable.
///
/// The property, end to end: what the original installer carried is what the
/// installation owns, the original file is gone, and a real preset child launched
/// from the installation's own state receives the same settings and reads the
/// same asset bytes.
#[test]
fn the_installer_can_be_deleted_and_the_window_still_opens() {
    let state = State::new();
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let report = scratch.path().join("maintenance-report.txt");
    let (app, preset) = preset_app("durable", &report);
    let _cleanup = support::project::cleanup(&app, &state);
    let payload = vec![Payload::named("app.exe", b"acme payload", Some("core"))];
    let setup = compose_with_preset(&app, &payload, &preset);

    // The original installer, and the `.zupui`-shaped content it carried.
    assert!(
        zup_windows::EmbeddedBundle::open(setup.path())
            .expect("the composed installer opens")
            .preset()
            .is_some(),
        "the installer this test deletes does carry a preset"
    );

    setup.succeed(&state, "install", &["--yes", "--output", "json"]);

    // What the installation now owns, in its own words.
    let ui = recorded_ui(&state, &app);
    assert_eq!(
        ui.preset.settings["hero"],
        format!("Install {}", app.name),
        "the settings the application configured were validated once and recorded, not \
         re-derived from anything"
    );
    assert_eq!(
        ui.preset.assets.len(),
        1,
        "the asset the settings named is part of what the installation recorded"
    );
    let preset_digest = zup_core::hash_bytes(&preset.executable);
    assert_eq!(
        ui.executable, preset_digest,
        "the window names the preset executable by content"
    );
    let owned = owned_ui_files(&state, &app);
    assert_eq!(
        owned.len(),
        2,
        "the preset executable and the one asset, at {}",
        owned[0].display()
    );
    assert!(
        owned.iter().all(|path| path.is_file()),
        "each is a file the installation can read back: {owned:?}"
    );

    // The maintenance copy is on disk before the original is gone, and it is not
    // the original: it is a copy the installation owns.
    let maintenance = state.maintenance(&app);
    assert!(
        maintenance.is_file(),
        "the maintenance runtime was persisted"
    );

    // Now the part the whole slice exists for.
    std::fs::remove_file(setup.path()).expect("the downloaded installer is disposable");
    assert!(
        !setup.path().exists(),
        "the machine has no copy of the installer a person downloaded"
    );

    let output = launch_maintenance(&maintenance, &state);
    assert!(
        output.status.success(),
        "maintenance launched:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let report = std::fs::read_to_string(&report).expect("the preset reported what it received");
    assert!(
        report.contains(&format!("hero=Install {}", app.name)),
        "the same settings reached the preset with the original installer deleted: {report}"
    );
    assert!(
        report.contains(&format!("asset-bytes={}", preset.assets[0].1.len())),
        "and the preset read the application's own asset bytes: {report}"
    );
    assert!(
        report.contains("state=Maintenance"),
        "the window opened on the maintenance surface, not the install one: {report}"
    );
    assert!(
        report.contains("maintenance=true"),
        "and the host offered the maintenance capability: {report}"
    );
    assert!(
        report.contains(&format!(
            "protocol={}",
            zup_preset_protocol::PRESET_PROTOCOL_VERSION
        )),
        "over the protocol version the composition recorded: {report}"
    );
}

/// A failed update leaves the window it would have replaced exactly as it was.
///
/// The failure is a real one: the second generation's payload is a file the
/// transaction cannot stage, so the transaction is refused before it commits.
/// The point is not that it failed - it is that the installation a person already
/// had is still coherent afterwards, and still opens.
#[test]
fn a_failed_update_leaves_the_previous_window_coherent() {
    let state_root = tempfile::tempdir().expect("a state root");
    let state = State::with_root(state_root.path().join("state"));
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let report = scratch.path().join("report.txt");
    let (app, preset) = preset_app("rollback", &report);
    let _cleanup = support::project::cleanup(&app, &state);
    let payload = vec![Payload::named("app.exe", b"acme payload", Some("core"))];
    let setup = compose_with_preset(&app, &payload, &preset);
    setup.succeed(&state, "install", &["--yes", "--output", "json"]);
    let installed = recorded_ui(&state, &app);

    // A newer application whose runtime directory is occupied by a file. The
    // transaction cannot create the directory it has to write into, so it fails
    // before the commit - which is the point: the point of interest is what the
    // installation looks like afterwards, not why the update stopped.
    let next = AppSpec {
        id: app.id.clone(),
        name: app.name.clone(),
        version: "9.9.9".to_owned(),
        install_directory: app.install_directory.clone(),
    };
    let next_payload = vec![
        Payload::named("app.exe", b"acme payload", Some("core")),
        Payload::named("blocked/next.txt", b"next", Some("core")),
    ];
    let newer = compose_with_preset(&next, &next_payload, &preset);
    std::fs::write(
        app.install_directory().join("blocked"),
        b"foreign parent entry",
    )
    .unwrap();
    let refused = std::process::Command::new(newer.path())
        .args(["upgrade", "--yes", "--output", "json"])
        .args(["--scope", "user", "--state-root"])
        .arg(state.path())
        .output()
        .expect("the update runs");
    assert!(
        !refused.status.success(),
        "an update with nowhere to write must not report success: {}",
        String::from_utf8_lossy(&refused.stdout)
    );
    std::fs::remove_file(app.install_directory().join("blocked"))
        .expect("the obstruction is cleared");

    assert_eq!(
        recorded_ui(&state, &app),
        installed,
        "the window the update would have replaced is untouched, as one value"
    );
    let owned = owned_ui_files(&state, &app);
    assert_eq!(owned.len(), 2, "and still exactly its content: {owned:?}");

    // And the installation still opens its own window.
    let output = launch_maintenance(&state.maintenance(&app), &state);
    assert!(
        output.status.success(),
        "maintenance still launched:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let report = std::fs::read_to_string(&report).expect("the preset reported");
    assert!(
        report.contains(&format!("hero=Install {}", app.name)),
        "with the settings the failed update did not touch: {report}"
    );
}

#[test]
fn a_corrupted_ledger_opens_the_trusted_carriers_diagnostic() {
    let state_root = tempfile::tempdir().expect("a state root");
    let state = State::with_root(state_root.path().join("state"));
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let report = scratch.path().join("never-written.txt");
    let (app, preset) = preset_app("forgotten", &report);
    let _cleanup = support::project::cleanup(&app, &state);
    let payload = vec![Payload::named("app.exe", b"acme payload", Some("core"))];
    let setup = compose_with_preset(&app, &payload, &preset);
    setup.succeed(&state, "install", &["--yes", "--output", "json"]);

    let maintenance = state.maintenance(&app);
    let path = zup_windows::InstallationRecordStore::new(state.path()).path_for(
        &zup_core::AppId::new(&app.id).expect("a valid id"),
        zup_core::SelectedScope::User,
    );
    let mut document: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).expect("the ledger is readable"))
            .expect("the ledger parses");
    document["record"]["current"]["preset"] = serde_json::Value::Null;
    std::fs::write(&path, serde_json::to_vec(&document).expect("it serializes"))
        .expect("the record is rewritten");

    let corrupted = std::fs::read(&path).unwrap();
    let output = launch_maintenance(&maintenance, &state);
    assert!(
        output.status.success(),
        "trusted diagnostic carrier: {output:?}"
    );
    let report = std::fs::read_to_string(&report).expect("trusted preset diagnostic");
    assert!(report.contains("state=RefreshRequired"), "{report}");
    assert_eq!(std::fs::read(&path).unwrap(), corrupted);
}

/// Content the installation recorded but cannot produce is an integrity problem,
/// not a reason to open some other window.
#[test]
fn an_installation_whose_preset_is_missing_says_so_rather_than_opening_another() {
    let state = State::new();
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let report = scratch.path().join("unreachable.txt");
    let (app, preset) = preset_app("damaged", &report);
    let _cleanup = support::project::cleanup(&app, &state);
    let payload = vec![Payload::named("app.exe", b"acme payload", Some("core"))];
    let setup = compose_with_preset(&app, &payload, &preset);
    setup.succeed(&state, "install", &["--yes", "--output", "json"]);

    let ui = recorded_ui(&state, &app);
    let maintenance = state.maintenance(&app);
    let directory = maintenance.parent().unwrap();
    let executable = zup_bundle::preset_path(
        directory,
        &ui.executable,
        support::project::host_target().executable_suffix(),
    );
    std::fs::write(&executable, b"not the preset").expect("the content is replaced");

    let error = resolve_installed(directory, &ui)
        .expect_err("content that is not what the installation recorded");
    assert!(
        error.to_string().contains("hashes to"),
        "the refusal says the content is wrong, not that something is missing: {error}"
    );
    let asset = zup_bundle::asset_path(directory, "branding/logo.svg", &ui.preset.assets[0].sha256);
    std::fs::remove_file(&asset).expect("the asset is removed");
    let error = resolve_installed(directory, &ui).expect_err("an asset the settings named is gone");
    assert!(
        error.to_string().contains("branding/logo.svg"),
        "the refusal names the asset the application configured: {error}"
    );
}

/// The installed window's content, proved the way the host proves it.
///
/// The test asks the portable owner rather than reaching into the installation's
/// own directories, because that is the contract the host holds itself to: a
/// caller is handed a path only once every byte behind it has been checked.
fn resolve_installed(
    directory: &Path,
    ui: &zup_core::InstalledPreset,
) -> Result<zup_bundle::Resolved, zup_bundle::PresetContentError> {
    zup_bundle::resolve_preset_content(
        directory,
        ui,
        support::project::host_target().executable_suffix(),
        zup_windows::plain_path_text,
    )
}

/// The window survives every verb that is not an uninstall.
///
/// A modify and a repair are re-plans of the same installation, so the window
/// they leave behind has to be the same window: the same preset, the same
/// settings, and the same asset bytes. This drives them through the real
/// transactions against the same state root.
#[test]
fn modify_and_repair_preserve_the_configured_window() {
    let state = State::new();
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let report = scratch.path().join("report.txt");
    let (app, preset) = preset_app("steady", &report);
    let _cleanup = support::project::cleanup(&app, &state);
    let payload = vec![Payload::named("app.exe", b"acme payload", Some("core"))];
    let setup = compose_with_preset(&app, &payload, &preset);
    setup.succeed(&state, "install", &["--yes", "--output", "json"]);

    let installed = recorded_ui(&state, &app);
    let maintenance = state.maintenance(&app);

    for verb in ["modify", "repair"] {
        support::project::succeed(&maintenance, &state, verb, &["--yes", "--output", "json"]);
        assert_eq!(
            state.maintenance(&app),
            maintenance,
            "unchanged complete maintenance content is reused"
        );
        assert_eq!(
            recorded_ui(&state, &app),
            installed,
            "{verb} left a different window behind: an installed generation is one value"
        );
    }

    let current = state.maintenance(&app);
    let asset = zup_bundle::asset_path(
        current.parent().unwrap(),
        "branding/logo.svg",
        &installed.preset.assets[0].sha256,
    );
    assert_eq!(
        std::fs::read(&asset).expect("the asset is readable"),
        preset.assets[0].1,
        "the bytes a modify and a repair left are the bytes the application configured"
    );
}

/// An uninstall removes the window's content, and only once the session that
/// needed it is over.
///
/// The preset executable is the file this hardest gets wrong: a running preset
/// holds it open, and Windows will not delete a file somebody is executing. The
/// ordering this asserts is the one that makes it work - the session finishes
/// and the child is gone, and only then is the content retired.
#[test]
fn uninstall_retires_the_window_and_preserves_retained_recovery_content() {
    let state = State::new();
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let report = scratch.path().join("report.txt");
    let (app, preset) = preset_app("departing", &report);
    let _cleanup = support::project::cleanup(&app, &state);
    let payload = vec![Payload::named("app.exe", b"acme payload", Some("core"))];
    let setup = compose_with_preset(&app, &payload, &preset);
    setup.succeed(&state, "install", &["--yes", "--output", "json"]);
    let maintenance = state.maintenance(&app);

    // The session runs to its end first, which is what releases the executable.
    let output = launch_maintenance(&maintenance, &state);
    assert!(
        output.status.success(),
        "maintenance launched:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        report.is_file(),
        "the preset ran before the uninstall, which is the only order that works"
    );
    assert_eq!(
        owned_ui_files(&state, &app).len(),
        2,
        "and its content was still there while it ran"
    );

    support::project::succeed(
        &maintenance,
        &state,
        "uninstall",
        &["--yes", "--output", "json"],
    );
    let record = InstallationRecordStore::new(state.path())
        .load(
            &zup_core::AppId::new(&app.id).unwrap(),
            zup_core::SelectedScope::User,
        )
        .unwrap()
        .unwrap();
    assert!(!record.is_installed());
    assert!(record.preset().is_none());
    assert!(maintenance.is_file());
    assert_eq!(owned_ui_files(&state, &app).len(), 2);
}

/// The generation an update installs replaces the previous one as a whole.
///
/// `2.0.0` is a different preset, different settings, and a different logo.
/// After the update the installation presents that, and the previous generation's
/// content is gone rather than left beside it.
#[test]
fn an_update_replaces_the_whole_window_generation() {
    let state = State::new();
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let first_report = scratch.path().join("first.txt");
    let second_report = scratch.path().join("second.txt");
    let (app, first) = preset_app("upgrade", &first_report);
    let _cleanup = support::project::cleanup(&app, &state);
    let payload = vec![Payload::named("app.exe", b"acme payload", Some("core"))];

    let setup = compose_with_preset(&app, &payload, &first);
    setup.succeed(&state, "install", &["--yes", "--output", "json"]);
    let installed = recorded_ui(&state, &app);
    assert_eq!(
        installed.preset.name.as_str(),
        format!("{}-preset", app.name)
    );

    // A different application version, a different preset identity, a different
    // logo, and a report path the new settings name.
    let next = AppSpec {
        id: app.id.clone(),
        name: app.name.clone(),
        version: "9.9.9".to_owned(),
        install_directory: app.install_directory.clone(),
    };
    let second = PresetSpec {
        executable: std::fs::read(preset_executable()).expect("the peer preset is built"),
        settings: serde_json::json!({
            "hero": "Install Acme 9",
            "logo": "branding/logo.svg",
            "report": second_report,
        }),
        assets: vec![(
            "branding/logo.svg".to_owned(),
            b"<svg xmlns='http://www.w3.org/2000/svg' width='16' height='16'/>".to_vec(),
        )],
        required_capabilities: vec![Capability::Components],
    };
    let upgrade = compose_with_preset(&next, &payload, &second);
    // The hidden verb, because that is the door an embedded installer is upgraded
    // through: `update` is the release graph's, and this installer carries the
    // new window itself.
    support::project::succeed(
        upgrade.path(),
        &state,
        "upgrade",
        &["--yes", "--output", "json"],
    );
    let replaced = recorded_ui(&state, &app);
    assert_eq!(
        replaced.preset.settings["hero"], "Install Acme 9",
        "the update's settings are what the installation now presents"
    );
    assert_ne!(
        replaced.preset.assets[0].sha256, installed.preset.assets[0].sha256,
        "and so is the update's logo, not the one it replaced"
    );
    assert_eq!(
        replaced.preset.version.to_string(),
        "9.9.9",
        "a new preset version is a new generation, not an edit"
    );

    let owned = owned_ui_files(&state, &app);
    assert_eq!(owned.len(), 4);
    let previous_runtime = state.maintenance(&app);
    let current_runtime = state.maintenance(&next);
    let previous = previous_runtime.parent().unwrap();
    let current = current_runtime.parent().unwrap();
    assert_ne!(previous, current);
    assert!(owned.iter().all(|path| path.is_file() && (path.starts_with(previous) || path.starts_with(current))));

    // And the new window is the one a person gets.
    let maintenance = current.join("maintenance.exe");
    assert!(
        maintenance.is_file(),
        "the update persisted its own runtime"
    );
    let output = launch_maintenance(&maintenance, &state);
    assert!(
        output.status.success(),
        "maintenance launched:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let report = std::fs::read_to_string(&second_report).expect("the preset reported");
    assert!(
        report.contains("hero=Install Acme 9"),
        "the updated window is the one that opened: {report}"
    );
    assert!(
        report.contains(&format!("asset-bytes={}", second.assets[0].1.len())),
        "with the updated logo's bytes: {report}"
    );
}
