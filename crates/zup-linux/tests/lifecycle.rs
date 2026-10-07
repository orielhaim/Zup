#![cfg(target_os = "linux")]

//! The Linux lifecycle, end to end through the real engine.
//!
//! Each test composes a genuine installer - a real ELF template with a real
//! package appended - and runs it through [`run`]: install, upgrade, repair,
//! uninstall. Nothing here calls the executor directly; the primary path is
//! the same one a downloaded installer takes, from carrier open to ledger
//! publish.
//!
//! The environment is isolated: every path the installer can resolve lands
//! under temporary directories, and the real user profile is never touched.
//! (Process execution of the produced installer is `process.rs`, with a
//! genuine runtime template.)

#[path = "support.rs"]
mod support;

use zup_core::{AppId, SelectedScope};
use zup_linux::{LinuxAction, LinuxLedgerStore, LinuxOutcome, LinuxRunRequest, run};

use support::{
    IsolatedUser, compose_fixture, mode_of, run_installer, run_tool, tool_script, v1_files,
    v2_files,
};

/// A fresh install through the real engine: the installer exits committed, the
/// payload exists with the modes the intents asked for, the ledger exists, a
/// maintenance generation exists, and the installed executable actually runs.
#[test]
fn fresh_install_runs_the_application() {
    let user = IsolatedUser::isolate();
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let installer = compose_fixture(scratch.path(), "Acme-Setup", "1.0.0", &v1_files());

    let outcome = run_installer(&installer, &user.state, LinuxAction::Apply);
    assert!(
        matches!(outcome, LinuxOutcome::Committed { .. }),
        "a fresh install commits: {outcome:?}"
    );

    let install = user.programs().join("tool");
    assert_eq!(
        std::fs::read(install.join("tool")).expect("the tool installs"),
        tool_script("1.0.0")
    );
    assert_eq!(mode_of(&install.join("tool")), 0o744, "declared runnable");
    assert_eq!(mode_of(&install.join("keep.dat")), 0o644, "data stays data");
    assert_eq!(
        run_tool(&install.join("tool"), &["--version"]).trim(),
        "tool 1.0.0",
        "the installed executable actually runs"
    );
    assert_eq!(
        run_tool(&install.join("tool"), &["read-payload"]).trim(),
        "keep-v1"
    );

    // The ledger exists and names this installation.
    let ledger = LinuxLedgerStore::new(&user.state)
        .load(
            &AppId::new("com.example.tool").expect("an id"),
            SelectedScope::User,
        )
        .expect("the ledger reads")
        .expect("an installation is recorded");
    assert_eq!(ledger.version.to_string(), "1.0.0");
    assert!(!ledger.resources.is_empty(), "ownership is recorded");

    // A maintenance generation exists, is executable, and is the installer.
    let maintenance = zup_transaction::maintenance_runtime_path(
        &user.state,
        &AppId::new("com.example.tool").expect("an id"),
        SelectedScope::User,
        &semver::Version::parse("1.0.0").expect("a version"),
        "",
    );
    assert!(
        maintenance.exists(),
        "a maintenance generation is persisted"
    );
    assert_ne!(
        mode_of(&maintenance) & 0o111,
        0,
        "the maintenance copy runs"
    );
}

/// An upgrade runs the v2 installer over v1 through lifecycle deltas: the app
/// reports v2, shared content is untouched, the retired file is gone, the new
/// file is present, the ledger records v2, and the old maintenance generation
/// is retired.
#[test]
fn upgrade_moves_the_installation_to_v2() {
    let user = IsolatedUser::isolate();
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let v1 = compose_fixture(scratch.path(), "v1", "1.0.0", &v1_files());
    let outcome = run_installer(&v1, &user.state, LinuxAction::Apply);
    assert!(matches!(outcome, LinuxOutcome::Committed { .. }));

    // The user's own file beside the payload: upgrade must not touch it.
    let install = user.programs().join("tool");
    std::fs::write(install.join("user-notes.txt"), b"the user's").expect("a user file");

    let v2 = compose_fixture(scratch.path(), "v2", "1.1.0", &v2_files());
    let outcome = run_installer(&v2, &user.state, LinuxAction::Apply);
    assert!(
        matches!(outcome, LinuxOutcome::Committed { .. }),
        "an upgrade commits: {outcome:?}"
    );

    assert_eq!(
        run_tool(&install.join("tool"), &["--version"]).trim(),
        "tool 1.1.0",
        "the app reports v2"
    );
    assert_eq!(
        std::fs::read(install.join("keep.dat")).expect("keep.dat"),
        b"keep-v1",
        "shared content is untouched"
    );
    assert!(
        !install.join("removed-in-v2.dat").exists(),
        "the retired file is gone"
    );
    assert!(
        install.join("added-in-v2.dat").exists(),
        "the new file is present"
    );
    assert_eq!(
        std::fs::read(install.join("user-notes.txt")).expect("user file"),
        b"the user's",
        "the upgrade does not touch what it does not own"
    );
    let ledger = LinuxLedgerStore::new(&user.state)
        .load(
            &AppId::new("com.example.tool").expect("an id"),
            SelectedScope::User,
        )
        .expect("the ledger reads")
        .expect("an installation is recorded");
    assert_eq!(ledger.version.to_string(), "1.1.0", "the ledger records v2");

    // The new maintenance generation is current; the old one is retired.
    let generation = |version: &str| {
        zup_transaction::maintenance_directory(
            &user.state,
            &AppId::new("com.example.tool").expect("an id"),
            SelectedScope::User,
            &semver::Version::parse(version).expect("a version"),
        )
    };
    assert!(
        generation("1.1.0").exists(),
        "the new generation is current"
    );
    assert!(
        !generation("1.0.0").exists(),
        "the old generation is retired by policy"
    );
}

/// Repair restores owned files: deleted ones always come back, damaged ones
/// with force, and an unrelated file nearby is not treated as owned merely
/// because of where it sits. Repair without force on a damaged file fails
/// rather than overwriting what may be a user edit.
#[test]
fn repair_restores_owned_files_and_ignores_the_rest() {
    let user = IsolatedUser::isolate();
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let v1 = compose_fixture(scratch.path(), "v1", "1.0.0", &v1_files());
    assert!(matches!(
        run_installer(&v1, &user.state, LinuxAction::Apply),
        LinuxOutcome::Committed { .. }
    ));
    let install = user.programs().join("tool");

    // A deleted file comes back on a plain repair.
    std::fs::remove_file(install.join("keep.dat")).expect("delete");
    std::fs::write(install.join("unrelated.txt"), b"not owned").expect("an unrelated file");
    let outcome = run_installer(&v1, &user.state, LinuxAction::Repair { force_files: false });
    assert!(
        matches!(outcome, LinuxOutcome::Committed { .. }),
        "a repair commits: {outcome:?}"
    );
    assert_eq!(
        std::fs::read(install.join("keep.dat")).expect("keep.dat"),
        b"keep-v1",
        "the deleted file is restored"
    );

    // A damaged file is refused without force: it may be damage or a user
    // edit, and the installer does not decide which silently.
    std::fs::write(install.join("tool"), b"damaged").expect("damage");
    let outcome = run(&LinuxRunRequest {
        installer: v1.clone(),
        scope: SelectedScope::User,
        state_root: Some(user.state.clone()),
        action: LinuxAction::Repair { force_files: false },
        install_dir_override: None,
    });
    assert!(
        outcome.is_err(),
        "repair without force refuses a damaged file: {outcome:?}"
    );
    assert_eq!(
        std::fs::read(install.join("tool")).expect("the tool"),
        b"damaged",
        "the refusal leaves the bytes alone"
    );

    // With force, exact original bytes are restored and the executable runs.
    let outcome = run_installer(&v1, &user.state, LinuxAction::Repair { force_files: true });
    assert!(
        matches!(outcome, LinuxOutcome::Committed { .. }),
        "a forced repair commits: {outcome:?}"
    );
    assert_eq!(
        std::fs::read(install.join("tool")).expect("the tool"),
        tool_script("1.0.0"),
        "exact original bytes are restored"
    );
    assert_eq!(
        std::fs::read(install.join("unrelated.txt")).expect("unrelated"),
        b"not owned",
        "the ledger stays authoritative about ownership"
    );
    assert_eq!(
        run_tool(&install.join("tool"), &["--version"]).trim(),
        "tool 1.0.0",
        "the repaired executable runs"
    );
}

/// Uninstall retires only owned resources: the payload, ledger, maintenance,
/// and transaction state go; an unrelated file survives; Zup-created
/// directories leave only while empty; no lock marker is left behind.
#[test]
fn uninstall_removes_what_zup_owns_and_nothing_else() {
    let user = IsolatedUser::isolate();
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let v1 = compose_fixture(scratch.path(), "v1", "1.0.0", &v1_files());
    assert!(matches!(
        run_installer(&v1, &user.state, LinuxAction::Apply),
        LinuxOutcome::Committed { .. }
    ));
    let install = user.programs().join("tool");
    std::fs::write(install.join("unrelated.txt"), b"not owned").expect("an unrelated file");

    let outcome = run_installer(&v1, &user.state, LinuxAction::Uninstall);
    assert!(
        matches!(outcome, LinuxOutcome::Committed { .. }),
        "an uninstall commits: {outcome:?}"
    );

    assert!(!install.join("tool").exists(), "the payload is gone");
    assert!(!install.join("keep.dat").exists(), "owned data is gone");
    assert_eq!(
        std::fs::read(install.join("unrelated.txt")).expect("unrelated"),
        b"not owned",
        "an unrelated file survives the uninstall"
    );
    assert!(
        install.exists(),
        "a directory with content of its own stays"
    );
    let ledger = LinuxLedgerStore::new(&user.state)
        .load(
            &AppId::new("com.example.tool").expect("an id"),
            SelectedScope::User,
        )
        .expect("the ledger reads");
    assert!(ledger.is_none(), "the ledger is removed");
    let maintenance = zup_transaction::maintenance_root(
        &user.state,
        &AppId::new("com.example.tool").expect("an id"),
        SelectedScope::User,
    );
    assert!(
        !maintenance.exists()
            || maintenance
                .read_dir()
                .map(|mut entries| entries.next().is_none())
                .unwrap_or(true),
        "maintenance content is removed"
    );
}

/// A tampered installer is refused before the machine is mutated: flipping
/// bytes in the embedded package breaks the footer digest, and the carrier
/// never gets as far as planning.
#[test]
fn a_tampered_package_is_refused_before_mutation() {
    let user = IsolatedUser::isolate();
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let installer = compose_fixture(scratch.path(), "v1", "1.0.0", &v1_files());

    let mut bytes = std::fs::read(&installer).expect("the installer reads");
    // Inside the package, not the template: the template occupies the file's
    // head, so the midpoint of the whole image may still be template bytes,
    // which the carrier deliberately does not cover. The package starts where
    // the template ends.
    let template_len = std::fs::metadata("/bin/true").expect("a template").len() as usize;
    let tamper_at = template_len + 100;
    assert!(
        tamper_at < bytes.len() - 81,
        "the fixture package is larger than the tamper offset"
    );
    bytes[tamper_at] ^= 0x40;
    let tampered = scratch.path().join("tampered");
    std::fs::write(&tampered, &bytes).expect("the tampered copy writes");

    let outcome = run(&LinuxRunRequest {
        installer: tampered,
        scope: SelectedScope::User,
        state_root: Some(user.state.clone()),
        action: LinuxAction::Apply,
        install_dir_override: None,
    });
    assert!(
        outcome.is_err(),
        "a tampered package is refused: {outcome:?}"
    );

    assert!(
        !user.programs().join("tool").exists(),
        "refusal happens before any mutation"
    );
}

/// Truncation is a refusal, not a panic: cutting the footer off the image
/// leaves a file the carrier will not parse, let alone install from.
#[test]
fn a_truncated_installer_is_refused_without_panicking() {
    let user = IsolatedUser::isolate();
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let installer = compose_fixture(scratch.path(), "v1", "1.0.0", &v1_files());

    let bytes = std::fs::read(&installer).expect("the installer reads");
    let truncated = scratch.path().join("truncated");
    std::fs::write(&truncated, &bytes[..bytes.len() - 20]).expect("the truncated copy writes");

    let outcome = run(&LinuxRunRequest {
        installer: truncated,
        scope: SelectedScope::User,
        state_root: Some(user.state.clone()),
        action: LinuxAction::Apply,
        install_dir_override: None,
    });
    assert!(
        outcome.is_err(),
        "a truncated image is refused: {outcome:?}"
    );
    assert!(
        !user.programs().join("tool").exists(),
        "refusal happens before any mutation"
    );
}

/// Repair from the persisted maintenance copy, not from the download.
///
/// This is the requirement that makes the maintenance generation load-bearing:
/// the original installer is deleted first, and the repair runs out of the
/// copy the installation owns. A repair that needed the download would make
/// the maintenance copy decorative.
#[test]
fn repair_runs_from_the_maintenance_copy_without_the_download() {
    let user = IsolatedUser::isolate();
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let installer = compose_fixture(scratch.path(), "v1", "1.0.0", &v1_files());
    assert!(matches!(
        run_installer(&installer, &user.state, LinuxAction::Apply),
        LinuxOutcome::Committed { .. }
    ));
    // The download is gone. What remains is what the installation owns.
    std::fs::remove_file(&installer).expect("the download is deleted");

    // The maintenance copy is a genuine installer, not a marker: it opens its
    // own embedded package and repairs from it. It is already runnable - the
    // transaction installed it with executable intent, so no manual chmod is
    // needed, exactly as for every installer this build produces.
    let maintenance = zup_transaction::maintenance_runtime_path(
        &user.state,
        &AppId::new("com.example.tool").expect("an id"),
        SelectedScope::User,
        &semver::Version::parse("1.0.0").expect("a version"),
        "",
    );
    let install = user.programs().join("tool");
    std::fs::remove_file(install.join("keep.dat")).expect("delete");

    let outcome = run_installer(
        &maintenance,
        &user.state,
        LinuxAction::Repair { force_files: false },
    );
    assert!(
        matches!(outcome, LinuxOutcome::Committed { .. }),
        "repair from maintenance commits: {outcome:?}"
    );
    assert_eq!(
        std::fs::read(install.join("keep.dat")).expect("keep.dat"),
        b"keep-v1"
    );
}
