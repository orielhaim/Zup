#![cfg(target_os = "linux")]
#![cfg(feature = "test-support")]

use std::path::PathBuf;

use zup_core::{AppId, SelectedScope};
use zup_linux::test_support::{
    IsolatedUser, genuine_template, gui_package_bytes, mode_of, run_gui_installer_process,
    run_tool, v1_files,
};
use zup_linux::{LinuxLedgerStore, LinuxSessionHost};
use zup_session::SessionHost;

fn driver_binary(name: &str) -> Vec<u8> {
    let exe = std::env::current_exe().expect("a test executable");
    let profile = exe
        .parent()
        .and_then(|deps| deps.parent())
        .expect("a profile directory");
    let candidate = profile.join(name);
    assert!(
        candidate.is_file(),
        "no `{name}` beside the test profile at {}. These drivers build with the test \
         suite, so a green build stages them before running it.",
        profile.display()
    );
    std::fs::read(candidate).expect("a preset driver binary")
}

fn gui_installer(scratch: &std::path::Path, name: &str, preset: &[u8]) -> PathBuf {
    let package = gui_package_bytes(scratch, "1.0.0", &v1_files());
    let output = scratch.join(name);
    zup_compose::compose_linux(&genuine_template("gui"), &output, &package, Some(preset))
        .expect("compose");
    output
}

fn gui_state_root() -> PathBuf {
    zup_linux::state_root(SelectedScope::User).expect("an isolated state root")
}

#[test]
fn a_graphical_run_without_a_display_is_refused_with_a_diagnostic() {
    let user = IsolatedUser::isolate();
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let installer = gui_installer(scratch.path(), "Acme-Setup", b"not a preset");
    let output = run_gui_installer_process(&installer, &user, &[], false);
    assert!(
        !output.status.success(),
        "no display means no window, not a headless run"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Wayland") && stderr.contains("X11"),
        "the refusal names what is missing: {stderr}"
    );
    assert!(
        stderr.contains("console"),
        "the refusal names the way forward: {stderr}"
    );
    let ledger = LinuxLedgerStore::new(gui_state_root())
        .load(
            &AppId::new("com.example.tool").expect("an id"),
            SelectedScope::User,
        )
        .expect("the ledger reads");
    assert!(ledger.is_none(), "the refusal happens before any mutation");
}

#[test]
fn materialize_stages_a_private_executable_preset() {
    let user = IsolatedUser::isolate();
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let preset = driver_binary("test-gui-install-preset");
    let installer = gui_installer(scratch.path(), "Acme-Setup", &preset);
    let mut host = LinuxSessionHost::open(installer.clone());
    let discovery = host
        .discover(
            &installer,
            &zup_session::SessionLaunch {
                scope: Some(SelectedScope::User),
                state_root: Some(user.state.clone()),
                ..Default::default()
            },
        )
        .expect("a GUI carrier discovers");
    assert!(discovery.installer().preset.is_some());
    let scope = zup_session::select_scope(
        discovery.installer().install.scope,
        &zup_session::SessionLaunch {
            scope: Some(SelectedScope::User),
            state_root: Some(user.state.clone()),
            interactive: true,
            ..Default::default()
        },
        &discovery.installations,
    )
    .expect("user scope");
    let context = discovery.context(scope).expect("context");
    let launched = host
        .materialize_preset(&discovery, &context)
        .expect("the preset materializes");
    assert_ne!(
        launched.executable.parent(),
        installer.parent(),
        "the preset is not staged beside the installer"
    );
    assert_eq!(
        std::fs::read(&launched.executable).expect("the staged preset reads"),
        preset,
        "the staged executable is the carrier's binary, byte for byte"
    );
    assert_ne!(
        mode_of(&launched.executable) & 0o111,
        0,
        "the staged preset is runnable as produced"
    );
    assert!(
        launched.configuration.settings.is_object(),
        "the host hands over the configured settings"
    );
}

#[test]
fn a_scripted_preset_installs_through_the_shared_session() {
    let user = IsolatedUser::isolate();
    let state_root = gui_state_root();
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let preset = driver_binary("test-gui-install-preset");
    let installer = gui_installer(scratch.path(), "Acme-Setup", &preset);
    let output = run_gui_installer_process(&installer, &user, &[], true);
    assert!(
        output.status.success(),
        "exit {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let install = user.programs().join("tool");
    assert_eq!(
        run_tool(&install.join("tool"), &["--version"]).trim(),
        "tool 1.0.0",
        "the GUI install commits a working application"
    );
    let ledger = LinuxLedgerStore::new(&state_root)
        .load(
            &AppId::new("com.example.tool").expect("an id"),
            SelectedScope::User,
        )
        .expect("the ledger reads")
        .expect("an installation is recorded");
    assert!(
        ledger.preset().is_some(),
        "the window is retained for maintenance"
    );
    let maintenance = zup_transaction::maintenance_runtime_path(
        &state_root,
        &AppId::new("com.example.tool").expect("an id"),
        SelectedScope::User,
        &semver::Version::parse("1.0.0").expect("a version"),
        "",
    );
    assert!(maintenance.exists(), "a maintenance copy is persisted");
}

#[test]
fn a_scripted_close_ends_the_session_without_installing() {
    let user = IsolatedUser::isolate();
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let preset = driver_binary("test-gui-close-preset");
    let installer = gui_installer(scratch.path(), "Acme-Setup", &preset);
    let output = run_gui_installer_process(&installer, &user, &[], true);
    assert!(
        output.status.success(),
        "a close ends the session cleanly: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !user.programs().join("tool").exists(),
        "closing installs nothing"
    );
}
