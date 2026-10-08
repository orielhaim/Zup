#![cfg(target_os = "linux")]
#![cfg(feature = "test-support")]

use zup_core::{AppId, SelectedScope};
use zup_linux::LinuxLedgerStore;

use zup_linux::test_support::{
    IsolatedUser, compose_installer, genuine_template, mode_of, package_bytes,
    run_installer_process, run_tool, v1_files,
};

#[test]
fn the_produced_installer_installs_as_a_process() {
    let user = IsolatedUser::isolate();
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let package = package_bytes(scratch.path(), "1.0.0", &v1_files());
    let installer = scratch.path().join("Acme-Setup");
    compose_installer(&genuine_template("console"), &installer, &package);

    assert_eq!(
        installer.extension(),
        None,
        "a Linux installer carries no extension"
    );
    assert_ne!(
        mode_of(&installer) & 0o111,
        0,
        "the composed installer is runnable as produced"
    );

    let output = run_installer_process(&installer, &user, &[]);
    assert!(
        output.status.success(),
        "exit {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("committed"),
        "the installer reports its outcome: {}",
        String::from_utf8_lossy(&output.stdout)
    );

    let install = user.programs().join("tool");
    assert_eq!(
        run_tool(&install.join("tool"), &["--version"]).trim(),
        "tool 1.0.0",
        "the installed executable runs"
    );
    let ledger = LinuxLedgerStore::new(&user.state)
        .load(
            &AppId::new("com.example.tool").expect("an id"),
            SelectedScope::User,
        )
        .expect("the ledger reads")
        .expect("an installation is recorded");
    assert_eq!(ledger.version.to_string(), "1.0.0");
}

#[test]
fn the_headless_installer_installs_with_piped_stdio() {
    let user = IsolatedUser::isolate();
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let package = package_bytes(scratch.path(), "1.0.0", &v1_files());
    let installer = scratch.path().join("Acme-Setup");
    compose_installer(&genuine_template("headless"), &installer, &package);

    let output = run_installer_process(&installer, &user, &["install"]);
    assert!(
        output.status.success(),
        "exit {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    let install = user.programs().join("tool");
    assert_eq!(
        run_tool(&install.join("tool"), &["--version"]).trim(),
        "tool 1.0.0"
    );
}

#[test]
fn the_produced_installer_uninstalls_as_a_process() {
    let user = IsolatedUser::isolate();
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let package = package_bytes(scratch.path(), "1.0.0", &v1_files());
    let installer = scratch.path().join("Acme-Setup");
    compose_installer(&genuine_template("console"), &installer, &package);

    let output = run_installer_process(&installer, &user, &[]);
    assert!(output.status.success(), "install: {}", output.status);
    let install = user.programs().join("tool");
    std::fs::write(install.join("unrelated.txt"), b"not owned").expect("an unrelated file");

    let output = run_installer_process(&installer, &user, &["uninstall"]);
    assert!(
        output.status.success(),
        "uninstall exit {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!install.join("tool").exists(), "the payload is gone");
    assert!(
        install.join("unrelated.txt").exists(),
        "an unrelated file survives"
    );
    let ledger = LinuxLedgerStore::new(&user.state)
        .load(
            &AppId::new("com.example.tool").expect("an id"),
            SelectedScope::User,
        )
        .expect("the ledger reads");
    assert!(ledger.is_none(), "the ledger is removed");
}

#[test]
fn concurrent_installers_serialize_on_the_installation_lock() {
    use std::process::Stdio;

    let user = IsolatedUser::isolate();
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let package = package_bytes(scratch.path(), "1.0.0", &v1_files());
    let installer = scratch.path().join("Acme-Setup");
    compose_installer(&genuine_template("headless"), &installer, &package);

    let app_id = AppId::new("com.example.tool").expect("an id");
    let key = zup_transaction::InstallationLock::lock_key(app_id.as_str(), "user");
    let _held = zup_transaction::InstallationLock::try_acquire(&user.state, &key)
        .expect("the lock acquires")
        .expect("nothing else holds it");

    let mut command = std::process::Command::new(&installer);
    command.arg("--state-root").arg(&user.state);
    for (name, value) in user.child_env() {
        command.env(name, value);
    }
    command.env("PATH", "/usr/bin:/bin");
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let output = command.output().expect("the installer process runs");
    assert!(
        !output.status.success(),
        "an installer must not commit while the installation is locked"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("another maintenance operation"),
        "the refusal names the lock, not a crash: {stderr}"
    );
    assert!(
        !user.programs().join("tool").exists(),
        "nothing is installed while locked out"
    );

    drop(_held);
    let output = run_installer_process(&installer, &user, &[]);
    assert!(output.status.success(), "after release: {}", output.status);
    let install = user.programs().join("tool");
    assert_eq!(
        run_tool(&install.join("tool"), &["--version"]).trim(),
        "tool 1.0.0",
        "the serialized installation runs"
    );
}
