//! Real systemd coverage: the manager answers, units validate, and (as
//! root, opt-in) a disposable unit round-trips policy without ever
//! starting anything.
//!
//! Read-only tests run wherever the system bus answers and skip honestly
//! where it does not (containers without systemd, cross hosts). Privileged
//! tests additionally need uid 0 and an explicit opt-in
//! (`ZUP_TEST_REAL_SYSTEMD=1`): they install uniquely-named units, never
//! start them, and always disable, unmask, remove, and reload in a cleanup
//! guard. `systemd-analyze verify` serves as an additional oracle where
//! available; the renderer and runtime verification stay authoritative.

#![cfg(target_os = "linux")]

use zup_core::ServiceStart;
use zup_linux::{RealSystemd, SystemdManager as _, probe_systemd};

/// Whether the systemd system manager answers on this machine.
fn bus_available() -> bool {
    probe_systemd().is_ok()
}

/// Whether a privileged round-trip may run here: root, bus, and explicit
/// opt-in. Production architecture stays the pkexec worker; `sudo` in CI
/// only stages this isolated test environment.
fn privileged_available() -> bool {
    rustix::process::geteuid().as_raw() == 0
        && bus_available()
        && std::env::var("ZUP_TEST_REAL_SYSTEMD").is_ok()
}

/// `systemd-analyze verify` as a test oracle, when the host ships it.
fn analyze_available() -> bool {
    std::process::Command::new("systemd-analyze")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

#[test]
fn the_system_manager_answers_where_it_runs() {
    if !bus_available() {
        eprintln!("skipped: no systemd system bus on this machine");
        return;
    }
    // Read-only: an unknown unit reports a state (or a clean refusal),
    // proving the manager answers without mutating anything.
    let mut manager = RealSystemd::connect().expect("the manager connects");
    let state = manager.unit_file_state("zup-definitely-not-installed.service");
    eprintln!("unknown unit state: {state:?}");
}

/// Every rendered start policy verifies under `systemd-analyze` where the
/// host ships it: the renderer output is structurally valid systemd.
#[test]
fn rendered_units_verify() {
    if !analyze_available() {
        eprintln!("skipped: no systemd-analyze on this machine");
        return;
    }
    // `systemd-analyze verify` executes nothing but insists the service
    // binary exists and is runnable: a host-owned true binary serves.
    let binary = ["/bin/true", "/usr/bin/true"]
        .into_iter()
        .find(|path| std::fs::metadata(path).is_ok_and(|metadata| metadata.is_file()))
        .expect("a true binary exists on a systemd host");
    for start in [
        ServiceStart::Automatic,
        ServiceStart::Manual,
        ServiceStart::Disabled,
    ] {
        let target = zup_core::TargetTriple::parse("x86_64-unknown-linux-gnu").expect("a target");
        let executable = zup_platform::TargetPath::new(target.clone(), binary).expect("a path");
        let service = zup_platform::TargetService {
            key: zup_core::ResourceKey::Service {
                id: zup_core::ServiceId::new("verify").expect("an id"),
            },
            id: zup_core::ServiceId::new("verify").expect("an id"),
            name: zup_core::NonEmptyString::new("Verify").expect("a name"),
            display_name: None,
            command: zup_platform::CommandSpec::new(
                executable,
                vec!["--serve".to_owned(), "$HOME".to_owned(), "%u".to_owned()],
            ),
            start,
            privilege: zup_core::Privilege::System,
        };
        let desired = zup_linux::DesiredService::derive(&service).expect("a service derives");
        let dir = tempfile::tempdir().expect("a directory");
        let path = dir.path().join(&desired.unit);
        std::fs::write(&path, &desired.bytes).expect("a unit writes");
        let output = std::process::Command::new("systemd-analyze")
            .arg("verify")
            .arg(&path)
            .output()
            .expect("systemd-analyze runs");
        assert!(
            output.status.success(),
            "unit for {start:?} verifies: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

/// A disposable privileged round-trip: unique unit, real manager, real
/// unit directory, cleanup guard. Proves source recognition, reload,
/// enable/disable/mask transitions, and that install never starts the
/// service - without ever calling StartUnit.
#[test]
fn a_disposable_unit_round_trips_policy() {
    if !privileged_available() {
        eprintln!("skipped: needs root, a system bus, and ZUP_TEST_REAL_SYSTEMD=1");
        return;
    }
    let tag = uuid::Uuid::now_v7().simple().to_string();
    // A tiny real executable under a disposable program tree.
    let program = std::path::PathBuf::from(format!("/opt/zup-test-{tag}"));
    std::fs::create_dir_all(&program).expect("a program tree");
    let binary = program.join("svc");
    std::fs::write(&binary, b"#!/bin/sh\nexit 0\n").expect("a service binary");
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).expect("chmod");

    let target = zup_core::TargetTriple::parse("x86_64-unknown-linux-gnu").expect("a target");
    let executable =
        zup_platform::TargetPath::new(target.clone(), binary.to_string_lossy().as_ref())
            .expect("a path");
    let service = zup_platform::TargetService {
        key: zup_core::ResourceKey::Service {
            id: zup_core::ServiceId::new(format!("zup-test-{tag}")).expect("an id"),
        },
        id: zup_core::ServiceId::new(format!("zup-test-{tag}")).expect("an id"),
        name: zup_core::NonEmptyString::new("Zup Test").expect("a name"),
        display_name: None,
        command: zup_platform::CommandSpec::new(executable, vec!["--serve".into()]),
        start: ServiceStart::Automatic,
        privilege: zup_core::Privilege::System,
    };
    // The unit name is the deterministic derivation of the stable
    // identity: unique here because the service id carries the tag, and
    // stable across display-name changes by construction.
    let desired = zup_linux::DesiredService::derive(&service).expect("a service derives");
    let unit = desired.unit.clone();
    let path = std::path::Path::new("/usr/local/lib/systemd/system").join(&unit);
    struct Cleanup {
        unit: String,
        path: std::path::PathBuf,
        program: std::path::PathBuf,
    }
    impl Drop for Cleanup {
        fn drop(&mut self) {
            if let Ok(mut manager) = RealSystemd::connect() {
                let _ = manager.unmask(&self.unit);
                let _ = manager.disable(&self.unit);
                std::fs::remove_file(&self.path).ok();
                let _ = manager.reload();
            } else {
                std::fs::remove_file(&self.path).ok();
            }
            std::fs::remove_dir_all(&self.program).ok();
        }
    }
    let _cleanup = Cleanup {
        unit: unit.clone(),
        path: path.clone(),
        program: program.clone(),
    };
    // Preflight: a stale identity from a crashed earlier run is cleaned
    // first (unique tags make this nearly impossible); a foreign unit
    // with this name would have failed derivation uniqueness instead.
    // (Fresh tags never collide: the check is the cleanup, not a gate.)
    std::fs::remove_file(&path).ok();
    std::fs::write(&path, &desired.bytes).expect("the source installs");
    use std::os::unix::fs::MetadataExt as _;
    let metadata = std::fs::symlink_metadata(&path).expect("the source stats");
    assert!(metadata.is_file() && !metadata.file_type().is_symlink());
    assert_eq!(metadata.uid(), 0);
    assert_eq!(metadata.permissions().mode() & 0o777, 0o644);
    let mut manager = RealSystemd::connect().expect("the manager connects");
    manager.reload().expect("a reload works");
    // Automatic becomes persistently enabled...
    let changes = manager.enable(&unit).expect("enable works");
    assert!(!changes.is_empty(), "enablement reports its changes");
    assert_eq!(
        manager.unit_file_state(&unit).expect("a state reads"),
        "enabled"
    );
    let info = manager.load_unit(&unit).expect("the unit loads");
    assert_eq!(info.fragment_path, path.to_string_lossy());
    // ...Manual becomes disabled and unmasked...
    let _ = manager.disable(&unit).expect("disable works");
    assert_eq!(
        manager.unit_file_state(&unit).expect("a state reads"),
        "disabled"
    );
    // ...Disabled becomes persistently masked with the source intact...
    let _ = manager.mask(&unit).expect("mask works");
    assert_eq!(
        manager.unit_file_state(&unit).expect("a state reads"),
        "masked"
    );
    assert!(path.is_file(), "a mask keeps the canonical source intact");
    // ...and back to Automatic removes only the owned mask.
    let _ = manager.unmask(&unit).expect("unmask works");
    let _ = manager.enable(&unit).expect("re-enable works");
    assert_eq!(
        manager.unit_file_state(&unit).expect("a state reads"),
        "enabled"
    );
    // The service was never started by any of this: running state is not
    // Zup's to own, and no start API ran.
    let active = std::process::Command::new("systemctl")
        .args(["is-active", &unit])
        .output()
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .unwrap_or_default();
    assert_ne!(
        active, "active",
        "install registers policy, it does not start"
    );
    // `systemd-analyze verify` as an oracle where available.
    if std::process::Command::new("systemd-analyze")
        .arg("verify")
        .arg(&path)
        .output()
        .is_ok_and(|output| output.status.success())
    {
        eprintln!("systemd-analyze accepts the installed source");
    }
}
