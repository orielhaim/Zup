#![cfg(target_os = "linux")]
#![cfg(feature = "test-support")]

use zup_compose::CARRIER_FOOTER_LEN as FOOTER_LEN;
use zup_core::{AppId, SelectedScope};
use zup_linux::{LinuxInstallationStore, LinuxOutcome, LinuxRunRequest, run};

use zup_linux::test_support::{
    IsolatedUser, compose_fixture, mode_of, run_installer, run_tool, tool_script, v1_files,
    v2_files,
};
use zup_runtime::Operation;

#[test]
fn fresh_install_runs_the_application() {
    let user = IsolatedUser::isolate();
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let installer = compose_fixture(scratch.path(), "Acme-Setup", "1.0.0", &v1_files());

    let outcome = run_installer(&installer, &user.state, Operation::Apply);
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

    let ledger = LinuxInstallationStore::new(&user.state)
        .load(
            &AppId::new("com.example.tool").expect("an id"),
            SelectedScope::User,
        )
        .expect("the ledger reads")
        .expect("an installation is recorded");
    assert_eq!(ledger.current.version.to_string(), "1.0.0");
    assert!(
        !ledger.current.resources.is_empty(),
        "ownership is recorded"
    );

    let maintenance =
        zup_linux::test_support::maintenance_path(&user.state, SelectedScope::User, "1.0.0");
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

#[test]
fn upgrade_moves_the_installation_to_v2() {
    let user = IsolatedUser::isolate();
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let v1 = compose_fixture(scratch.path(), "v1", "1.0.0", &v1_files());
    let outcome = run_installer(&v1, &user.state, Operation::Apply);
    assert!(matches!(outcome, LinuxOutcome::Committed { .. }));

    let install = user.programs().join("tool");
    std::fs::write(install.join("user-notes.txt"), b"the user's").expect("a user file");

    let v2 = compose_fixture(scratch.path(), "v2", "1.1.0", &v2_files());
    let outcome = run_installer(&v2, &user.state, Operation::Apply);
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
    let ledger = LinuxInstallationStore::new(&user.state)
        .load(
            &AppId::new("com.example.tool").expect("an id"),
            SelectedScope::User,
        )
        .expect("the ledger reads")
        .expect("an installation is recorded");
    assert_eq!(
        ledger.current.version.to_string(),
        "1.1.0",
        "the ledger records v2"
    );

    let generation = |version: &str| {
        zup_linux::test_support::maintenance_path(&user.state, SelectedScope::User, version)
            .parent()
            .unwrap()
            .to_owned()
    };
    assert!(
        generation("1.1.0").exists(),
        "the new generation is current"
    );
    assert!(
        generation("1.0.0").exists(),
        "the prior maintenance generation remains available for recovery"
    );
}

#[test]
fn repair_restores_owned_files_and_ignores_the_rest() {
    let user = IsolatedUser::isolate();
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let v1 = compose_fixture(scratch.path(), "v1", "1.0.0", &v1_files());
    assert!(matches!(
        run_installer(&v1, &user.state, Operation::Apply),
        LinuxOutcome::Committed { .. }
    ));
    let install = user.programs().join("tool");

    std::fs::remove_file(install.join("keep.dat")).expect("delete");
    std::fs::write(install.join("unrelated.txt"), b"not owned").expect("an unrelated file");
    let outcome = run_installer(&v1, &user.state, Operation::Repair { force: false });
    assert!(
        matches!(outcome, LinuxOutcome::Committed { .. }),
        "a repair commits: {outcome:?}"
    );
    assert_eq!(
        std::fs::read(install.join("keep.dat")).expect("keep.dat"),
        b"keep-v1",
        "the deleted file is restored"
    );

    std::fs::write(install.join("tool"), b"damaged").expect("damage");
    let outcome = run(&LinuxRunRequest {
        installer: v1.clone(),
        scope: SelectedScope::User,
        state_root: Some(user.state.clone()),
        action: Operation::Repair { force: false },
        selection: Default::default(),
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

    let outcome = run_installer(&v1, &user.state, Operation::Repair { force: true });
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

#[test]
fn uninstall_uses_ownership_after_source_components_are_removed() {
    let user = IsolatedUser::isolate();
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let bytes = zup_linux::test_support::package_bytes(scratch.path(), "1.0.0", &v1_files());
    let package = zup_bundle::Package::from_bytes(bytes).unwrap();
    let mut build = package.build_plan().unwrap().targets.remove(0);
    let component = zup_core::ComponentId::new("retired").unwrap();
    build.installer.components.push(zup_core::Component {
        id: component.clone(),
        name: zup_core::NonEmptyString::new("Retired").unwrap(),
        description: None,
        required: true,
        default: true,
        requires: vec![],
        group: None,
    });
    for file in &mut build.files {
        file.source = scratch
            .path()
            .join("payload")
            .join(file.source_relative.as_str());
        file.component = Some(component.clone());
    }
    for mapping in &mut build.installer.files {
        mapping.component = Some(component.clone());
    }
    let v1 = scratch.path().join("v1");
    zup_linux::test_support::compose_installer(
        std::path::Path::new(zup_linux::test_support::inert_template()),
        &v1,
        &zup_bundle::BundleWriter::encode(&build, &[]).unwrap(),
    );
    assert!(matches!(
        run_installer(&v1, &user.state, Operation::Apply),
        LinuxOutcome::Committed { .. }
    ));
    let install = user.programs().join("tool");
    std::fs::write(install.join("unrelated.txt"), b"not owned").expect("an unrelated file");

    let v2 = compose_fixture(scratch.path(), "v2", "2.0.0", &v2_files());
    let outcome = run_installer(&v2, &user.state, Operation::Uninstall);
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
    let ledger = LinuxInstallationStore::new(&user.state)
        .load(
            &AppId::new("com.example.tool").expect("an id"),
            SelectedScope::User,
        )
        .expect("the ledger reads");
    assert!(
        ledger.is_some_and(|record| !record.is_installed() && record.current.resources.is_empty()),
        "the tombstone holds no active ownership"
    );
    let maintenance =
        zup_linux::test_support::maintenance_path(&user.state, SelectedScope::User, "1.0.0");
    assert!(
        maintenance.exists(),
        "retained maintenance content remains available until safe reclamation is established"
    );
}

#[test]
fn a_tampered_package_is_refused_before_mutation() {
    let user = IsolatedUser::isolate();
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let installer = compose_fixture(scratch.path(), "v1", "1.0.0", &v1_files());

    let mut bytes = std::fs::read(&installer).expect("the installer reads");

    let template_len = std::fs::metadata("/bin/true").expect("a template").len() as usize;
    let tamper_at = template_len + 100;
    assert!(
        tamper_at < bytes.len() - FOOTER_LEN as usize,
        "the fixture package is larger than the tamper offset"
    );
    bytes[tamper_at] ^= 0x40;
    let tampered = scratch.path().join("tampered");
    std::fs::write(&tampered, &bytes).expect("the tampered copy writes");

    let outcome = run(&LinuxRunRequest {
        installer: tampered,
        scope: SelectedScope::User,
        state_root: Some(user.state.clone()),
        action: Operation::Apply,
        selection: Default::default(),
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
        action: Operation::Apply,
        selection: Default::default(),
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

#[test]
fn repair_runs_from_the_maintenance_copy_without_the_download() {
    let user = IsolatedUser::isolate();
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let installer = compose_fixture(scratch.path(), "v1", "1.0.0", &v1_files());
    assert!(matches!(
        run_installer(&installer, &user.state, Operation::Apply),
        LinuxOutcome::Committed { .. }
    ));

    std::fs::remove_file(&installer).expect("the download is deleted");

    let maintenance =
        zup_linux::test_support::maintenance_path(&user.state, SelectedScope::User, "1.0.0");
    let install = user.programs().join("tool");
    std::fs::remove_file(install.join("keep.dat")).expect("delete");

    let outcome = run_installer(
        &maintenance,
        &user.state,
        Operation::Repair { force: false },
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
