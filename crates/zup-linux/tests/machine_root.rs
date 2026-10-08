#![cfg(target_os = "linux")]
#![cfg(feature = "test-support")]

use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use zup_core::{AppId, SelectedScope};
use zup_linux::{LinuxAction, LinuxOutcome, SystemPkexec};

use zup_linux::test_support::{
    MachineTestRoots, machine_fixture, machine_maintenance_path, machine_v1_files,
    run_machine_isolated,
};

fn root_only() -> bool {
    if rustix::process::geteuid().as_raw() != 0 {
        eprintln!("skipping root proof: not running as uid 0");
        return false;
    }
    true
}

const TMPFS_MAGIC: i64 = 0x0102_1994;

static MOUNT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct ProductionMountIsolation {
    _lock: std::sync::MutexGuard<'static, ()>,
    mounts: Vec<(PathBuf, bool)>,
}

impl ProductionMountIsolation {
    fn isolate() -> Self {
        let lock = MOUNT_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // SAFETY: NEWNS only swaps this process's mount table; no file
        // descriptor tables diverge and no memory is shared differently.
        // The process-wide lock serializes isolated tests so mounts never
        // interleave; other tests never touch production machine paths.
        unsafe { rustix::thread::unshare_unsafe(rustix::thread::UnshareFlags::NEWNS) }
            .expect("a private mount namespace needs privilege to unshare");
        rustix::mount::mount_change(
            "/",
            rustix::mount::MountPropagationFlags::PRIVATE
                | rustix::mount::MountPropagationFlags::REC,
        )
        .expect("the private namespace propagates nothing to the host");
        let roots = zup_linux::MachineRoots::production();
        let mut mounts = Vec::new();
        for target in [&roots.programs, &roots.state, &roots.shared_data] {
            let created = if std::fs::symlink_metadata(target).is_err() {
                std::fs::create_dir_all(target).expect(
                    "isolation stages a missing production machine path inside its own namespace",
                );
                true
            } else {
                false
            };
            let metadata = std::fs::symlink_metadata(target).expect(
                "isolation covers a production machine path that exists as a real directory",
            );
            assert!(
                metadata.is_dir() && !metadata.file_type().is_symlink(),
                "isolation covers {target}, a real directory",
                target = target.display()
            );
            rustix::mount::mount(
                c"tmpfs",
                target,
                c"tmpfs",
                rustix::mount::MountFlags::NOSUID | rustix::mount::MountFlags::NODEV,
                None::<&std::ffi::CStr>,
            )
            .expect("a disposable mount covers the production path");
            let fstype = rustix::fs::statfs(target)
                .expect("the covered path stats")
                .f_type;
            assert_eq!(
                fstype,
                TMPFS_MAGIC,
                "isolation covers {target} with tmpfs",
                target = target.display()
            );
            mounts.push((target.clone(), created));
        }
        Self {
            _lock: lock,
            mounts,
        }
    }
}

impl Drop for ProductionMountIsolation {
    fn drop(&mut self) {
        for (target, created) in self.mounts.iter().rev() {
            let _ = rustix::mount::unmount(target, rustix::mount::UnmountFlags::empty());
            if *created {
                let _ = std::fs::remove_dir(target);
            }
        }
    }
}

fn app_id() -> AppId {
    AppId::new("com.example.tool").expect("an id")
}
fn commit(
    roots: &MachineTestRoots,
    installer: &Path,
    action: LinuxAction,
    install_dir_override: Option<PathBuf>,
    what: &str,
) {
    let outcome = run_machine_isolated(installer, roots, action, install_dir_override);
    assert!(
        matches!(outcome, Ok(LinuxOutcome::Committed { .. })),
        "{what}: {outcome:?}"
    );
}

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

    assert_eq!(
        mode_of(path) & 0o022,
        0,
        "{} is never group- or world-writable",
        path.display()
    );
}

#[test]
fn root_machine_state_ownership_and_modes() {
    if !root_only() {
        return;
    }
    let (_base, roots) = isolated();
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let installer = machine_fixture(scratch.path(), "v1", "1.0.0", &machine_v1_files());
    commit(
        &roots,
        &installer,
        LinuxAction::Install,
        None,
        "install commits: {outcome:?}",
    );

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

    let transactions = roots.state.join("transactions");
    if transactions.exists() {
        assert_eq!(uid_of(&transactions), 0);
        assert_eq!(mode_of(&transactions) & 0o777, 0o700);
        assert_unwritable(&transactions);
    }

    let ledger =
        zup_linux::LinuxLedgerStore::new(&roots.state).path_for(&app_id(), SelectedScope::Machine);
    assert!(ledger.is_file());
    assert_eq!(uid_of(&ledger), 0);
    assert_eq!(mode_of(&ledger) & 0o777, 0o644);
    assert_unwritable(&ledger);

    let lock = roots
        .state
        .join("zup-install-com_example_tool-machine.lock");
    if lock.exists() {
        assert_eq!(uid_of(&lock), 0);
        assert_unwritable(&lock);
    }

    let maintenance = machine_maintenance_path(&roots.state, "1.0.0");
    assert!(maintenance.is_file());
    assert_eq!(uid_of(&maintenance), 0);
    assert_unwritable(&maintenance);
    assert_ne!(mode_of(&maintenance) & 0o111, 0, "still runnable");

    let tool = roots.roots.programs.join("tool").join("tool");
    assert!(tool.is_file());
    assert_eq!(uid_of(&tool), 0);
    assert_eq!(mode_of(&tool) & 0o777, 0o755);
}

#[test]
fn root_refuses_user_writable_maintenance() {
    if !root_only() {
        return;
    }
    let (_base, roots) = isolated();
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let installer = machine_fixture(scratch.path(), "v1", "1.0.0", &machine_v1_files());
    commit(
        &roots,
        &installer,
        LinuxAction::Install,
        None,
        "install commits: {outcome:?}",
    );

    let maintenance = machine_maintenance_path(&roots.state, "1.0.0");
    let mut permissions = std::fs::metadata(&maintenance).expect("stat").permissions();
    permissions.set_mode(0o666);
    std::fs::set_permissions(&maintenance, permissions).expect("chmod");
    let outcome = run_machine_isolated(
        &maintenance,
        &roots,
        LinuxAction::Repair { force_files: true },
        None,
    );
    assert!(
        outcome.is_err(),
        "a user-writable maintenance generation is refused: {outcome:?}"
    );

    let mut permissions = std::fs::metadata(&maintenance).expect("stat").permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&maintenance, permissions).expect("chmod");
    std::fs::remove_file(roots.roots.programs.join("tool").join("keep.dat")).expect("delete");
    commit(
        &roots,
        &maintenance,
        LinuxAction::Repair { force_files: false },
        None,
        "trusted maintenance serves again: {outcome:?}",
    );
}

#[test]
fn root_refuses_user_writable_ledger() {
    if !root_only() {
        return;
    }
    let (_base, roots) = isolated();
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let installer = machine_fixture(scratch.path(), "v1", "1.0.0", &machine_v1_files());
    commit(
        &roots,
        &installer,
        LinuxAction::Install,
        None,
        "install commits: {outcome:?}",
    );

    let ledger =
        zup_linux::LinuxLedgerStore::new(&roots.state).path_for(&app_id(), SelectedScope::Machine);
    let mut permissions = std::fs::metadata(&ledger).expect("stat").permissions();
    permissions.set_mode(0o666);
    std::fs::set_permissions(&ledger, permissions).expect("chmod");
    let outcome = run_machine_isolated(
        &installer,
        &roots,
        LinuxAction::Repair { force_files: true },
        None,
    );
    assert!(
        outcome.is_err(),
        "a user-writable ledger is refused: {outcome:?}"
    );
}

#[test]
fn root_public_run_dispatches_machine_scope() {
    if !root_only() {
        return;
    }
    let _isolation = ProductionMountIsolation::isolate();
    let (_base, roots) = isolated();
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let installer = machine_fixture(scratch.path(), "v1", "1.0.0", &machine_v1_files());
    let request = |action| zup_linux::LinuxRunRequest {
        installer: installer.clone(),
        scope: SelectedScope::Machine,
        state_root: Some(roots.state.clone()),
        action,
        install_dir_override: None,
    };
    let tool = PathBuf::from("/opt/tool/tool");
    let outcome = zup_linux::run(&request(LinuxAction::Install));
    assert!(
        matches!(outcome, Ok(LinuxOutcome::Committed { .. })),
        "the public machine run commits without pkexec: {outcome:?}"
    );
    assert!(
        tool.is_file(),
        "the payload installed through the public path"
    );
    assert_eq!(uid_of(&tool), 0);
    let outcome = zup_linux::run(&request(LinuxAction::Uninstall));
    assert!(
        matches!(outcome, Ok(LinuxOutcome::Committed { .. })),
        "the public machine run uninstalls: {outcome:?}"
    );
    assert!(!tool.exists(), "uninstall removes the payload");
}

#[test]
fn unprivileged_pkexec_proves_elevation_to_root() {
    use zup_linux::{LaunchOutcome, PkexecLauncher, WorkerChild};

    if rustix::process::geteuid().as_raw() == 0 {
        eprintln!("skipping pkexec elevation proof: elevation is vacuous for uid 0");
        return;
    }
    let launcher = match SystemPkexec::resolve() {
        Ok(launcher) => launcher,
        Err(error) => {
            eprintln!("skipping pkexec elevation proof: {error}");
            return;
        }
    };
    let shell = ["/bin/sh", "/usr/bin/sh"]
        .into_iter()
        .map(Path::new)
        .find(|shell| shell.is_file());
    let Some(shell) = shell else {
        eprintln!("skipping pkexec elevation proof: no system shell to elevate");
        return;
    };
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let sentinel = scratch.path().join("uid");
    let child = launcher
        .spawn(
            shell,
            &["-c".to_owned(), format!("id -u > {}", sentinel.display())],
        )
        .expect("pkexec spawns");
    let outcome: LaunchOutcome = child.wait().expect("pkexec reports");
    match outcome.code {
        Some(0) => {}
        Some(126 | 127) => {
            eprintln!(
                "skipping pkexec elevation proof: pkexec refused without an authorizing agent: {}",
                outcome.stderr.trim()
            );
            return;
        }
        other => panic!(
            "pkexec failed unexpectedly ({other:?}): {}",
            outcome.stderr.trim()
        ),
    }
    assert_eq!(
        std::fs::read_to_string(&sentinel)
            .expect("the elevated child records its uid")
            .trim(),
        "0",
        "a process pkexec launched for an unprivileged parent runs as root"
    );
}
