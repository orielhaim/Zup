//! Machine-scope root proofs: ownership, modes, write resistance, tamper.
//!
//! These tests need uid 0: they prove what the isolated loopback suite
//! cannot - that root-owned state is really root-owned, really private
//! where it must be, and really unwritable by anyone else. Anywhere else
//! they skip. Requires the `test-support` feature. Nothing here touches the
//! host's `/opt` or `/var/lib/zup`: every root lives under isolated
//! directories, and the privilege under test is the test's own uid, not an
//! escalation.
//!
//! CI runs these under `sudo` on the Ubuntu job; see the workflow.
#![cfg(target_os = "linux")]
#![cfg(feature = "test-support")]

#[path = "support.rs"]
mod support;

use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::Path;

use zup_core::{AppId, SelectedScope};
use zup_linux::test_support::{MachineTestRoots, run_machine_isolated_in};
use zup_linux::{LinuxAction, LinuxOutcome};

use support::{machine_fixture, machine_maintenance_path, machine_v1_files};

fn root_only() -> bool {
    if rustix::process::geteuid().as_raw() != 0 {
        eprintln!("skipping root proof: not running as uid 0");
        return false;
    }
    true
}

fn app_id() -> AppId {
    AppId::new("com.example.tool").expect("an id")
}

/// An isolated machine tree held alive for one test.
fn isolated() -> (tempfile::TempDir, MachineTestRoots) {
    let base = tempfile::tempdir().expect("an isolated base");
    let roots = MachineTestRoots::isolate_in(base.path());
    (base, roots)
}

fn mode_of(path: &Path) -> u32 {
    std::fs::symlink_metadata(path)
        .expect("stat")
        .permissions()
        .mode()
        & 0o7777
}

fn uid_of(path: &Path) -> u32 {
    std::fs::symlink_metadata(path).expect("stat").uid()
}

/// Run a shell probe as an unprivileged user, for write-resistance proofs.
///
/// `setpriv` ships with util-linux; where it is absent the sub-proof
/// skips rather than failing the suite for the harness's sake.
fn probe_as_nobody(script: &str) -> Option<bool> {
    let output = std::process::Command::new("setpriv")
        .args(["--reuid=65534", "--regid=65534", "--clear-groups"])
        .args(["/bin/sh", "-c", script])
        .output()
        .ok()?;
    output.status.code().map(|code| code == 0)
}

fn assert_unwritable(path: &Path) {
    let probe = format!("test ! -w {}", path.display());
    match probe_as_nobody(&probe) {
        Some(true) => {}
        Some(false) => panic!("{} is writable by an unprivileged user", path.display()),
        None => eprintln!("skipping write proof for {}: no setpriv", path.display()),
    }
    // And the mode says the same thing without executing anyone.
    assert_eq!(
        mode_of(path) & 0o022,
        0,
        "{} is never group- or world-writable",
        path.display()
    );
}

/// After a machine install as root: every privileged path is root-owned,
/// private where it must be, public where it legitimately is, and
/// unwritable by anyone else - proven by attempt, not only by mode bits.
#[test]
fn root_machine_state_ownership_and_modes() {
    if !root_only() {
        return;
    }
    let (_base, roots) = isolated();
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let installer = machine_fixture(scratch.path(), "v1", "1.0.0", &machine_v1_files());
    let outcome = run_machine_isolated_in(
        &installer,
        &roots.roots,
        &roots.state,
        LinuxAction::Install,
        None,
    );
    assert!(
        matches!(outcome, Ok(LinuxOutcome::Committed { .. })),
        "install commits: {outcome:?}"
    );

    // State root and public containers: root-owned, traversable, not writable.
    for directory in [
        roots.state.clone(),
        roots.state.join("installations"),
        roots.state.join("generated"),
    ] {
        if directory.exists() {
            assert_eq!(uid_of(&directory), 0, "{}", directory.display());
            assert_unwritable(&directory);
        }
    }
    // Private transaction state: root-owned and closed.
    let transactions = roots.state.join("transactions");
    if transactions.exists() {
        assert_eq!(uid_of(&transactions), 0);
        assert_eq!(mode_of(&transactions) & 0o777, 0o700);
        assert_unwritable(&transactions);
    }
    // The ledger: root-owned public metadata, readable for planning.
    let ledger =
        zup_linux::LinuxLedgerStore::new(&roots.state).path_for(&app_id(), SelectedScope::Machine);
    assert!(ledger.is_file());
    assert_eq!(uid_of(&ledger), 0);
    assert_eq!(mode_of(&ledger) & 0o777, 0o644);
    assert_unwritable(&ledger);
    // The lock marker: root-owned, never writable below root.
    let lock = roots
        .state
        .join("zup-install-com_example_tool-machine.lock");
    if lock.exists() {
        assert_eq!(uid_of(&lock), 0);
        assert_unwritable(&lock);
    }
    // The maintenance generation: root-owned, runnable, never writable.
    let maintenance = machine_maintenance_path(&roots.state, "1.0.0");
    assert!(maintenance.is_file());
    assert_eq!(uid_of(&maintenance), 0);
    assert_unwritable(&maintenance);
    assert_ne!(mode_of(&maintenance) & 0o111, 0, "still runnable");
    // The installed executable: root-owned, runnable by every account.
    let tool = roots.roots.programs.join("tool").join("tool");
    assert!(tool.is_file());
    assert_eq!(uid_of(&tool), 0);
    assert_eq!(mode_of(&tool) & 0o777, 0o755);
}

/// A maintenance generation an unprivileged user could write is not
/// trusted: the worker refuses it instead of elevating through it.
#[test]
fn root_refuses_user_writable_maintenance() {
    if !root_only() {
        return;
    }
    let (_base, roots) = isolated();
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let installer = machine_fixture(scratch.path(), "v1", "1.0.0", &machine_v1_files());
    let outcome = run_machine_isolated_in(
        &installer,
        &roots.roots,
        &roots.state,
        LinuxAction::Install,
        None,
    );
    assert!(
        matches!(outcome, Ok(LinuxOutcome::Committed { .. })),
        "install commits: {outcome:?}"
    );

    // Loosen the maintenance copy as an attacker with a chmod would: the
    // next privileged operation must refuse the generation, not serve it.
    let maintenance = machine_maintenance_path(&roots.state, "1.0.0");
    let mut permissions = std::fs::metadata(&maintenance).expect("stat").permissions();
    permissions.set_mode(0o666);
    std::fs::set_permissions(&maintenance, permissions).expect("chmod");
    let outcome = run_machine_isolated_in(
        &maintenance,
        &roots.roots,
        &roots.state,
        LinuxAction::Repair { force_files: true },
        None,
    );
    assert!(
        outcome.is_err(),
        "a user-writable maintenance generation is refused: {outcome:?}"
    );

    // Restore the trusted mode and prove the generation serves again.
    let mut permissions = std::fs::metadata(&maintenance).expect("stat").permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&maintenance, permissions).expect("chmod");
    std::fs::remove_file(roots.roots.programs.join("tool").join("keep.dat")).expect("delete");
    let outcome = run_machine_isolated_in(
        &maintenance,
        &roots.roots,
        &roots.state,
        LinuxAction::Repair { force_files: false },
        None,
    );
    assert!(
        matches!(outcome, Ok(LinuxOutcome::Committed { .. })),
        "trusted maintenance serves again: {outcome:?}"
    );
}

/// A ledger an unprivileged user could write is not planned from: both the
/// worker and the unprivileged planner refuse it.
#[test]
fn root_refuses_user_writable_ledger() {
    if !root_only() {
        return;
    }
    let (_base, roots) = isolated();
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let installer = machine_fixture(scratch.path(), "v1", "1.0.0", &machine_v1_files());
    let outcome = run_machine_isolated_in(
        &installer,
        &roots.roots,
        &roots.state,
        LinuxAction::Install,
        None,
    );
    assert!(
        matches!(outcome, Ok(LinuxOutcome::Committed { .. })),
        "install commits: {outcome:?}"
    );

    let ledger =
        zup_linux::LinuxLedgerStore::new(&roots.state).path_for(&app_id(), SelectedScope::Machine);
    let mut permissions = std::fs::metadata(&ledger).expect("stat").permissions();
    permissions.set_mode(0o666);
    std::fs::set_permissions(&ledger, permissions).expect("chmod");
    let outcome = run_machine_isolated_in(
        &installer,
        &roots.roots,
        &roots.state,
        LinuxAction::Repair { force_files: true },
        None,
    );
    assert!(
        outcome.is_err(),
        "a user-writable ledger is refused: {outcome:?}"
    );
}

/// Already root takes no weaker shortcut: the public run dispatches machine
/// scope through the same worker path, with the same validation.
#[test]
fn root_public_run_dispatches_machine_scope() {
    if !root_only() {
        return;
    }
    let (_base, roots) = isolated();
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let installer = machine_fixture(scratch.path(), "v1", "1.0.0", &machine_v1_files());
    let outcome = zup_linux::run(&zup_linux::LinuxRunRequest {
        installer,
        scope: SelectedScope::Machine,
        state_root: Some(roots.state.clone()),
        action: LinuxAction::Install,
        install_dir_override: None,
    });
    assert!(
        matches!(outcome, Ok(LinuxOutcome::Committed { .. })),
        "the public machine run commits without pkexec: {outcome:?}"
    );
    let tool = roots.roots.programs.join("tool").join("tool");
    assert!(
        tool.is_file(),
        "the payload installed through the public path"
    );
    assert_eq!(uid_of(&tool), 0);
}

/// Real `pkexec` is proven by hand, not by CI: this test documents the
/// boundary instead of faking it.
///
/// CI cannot answer an interactive polkit prompt, so the harnessed suite
/// proves the client/worker protocol, the root worker behavior, and the
/// launcher result mapping. A human proves the remaining step - an
/// unprivileged installer, a real `pkexec` authorization, a machine
/// install - on a native machine and records it in the release notes.
/// This test passes trivially so the requirement is visible in the suite
/// rather than in a comment somewhere.
#[test]
fn real_pkexec_proof_is_manual() {
    eprintln!(
        "manual proof required: unprivileged installer -> real pkexec -> worker -> machine install"
    );
}
