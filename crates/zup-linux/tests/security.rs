#![cfg(target_os = "linux")]
#![cfg(feature = "test-support")]

use std::path::{Path, PathBuf};

use zup_core::{SelectedScope, TargetTriple};
use zup_linux::{Carrier, CarrierError, ExecError, LinuxAction, LinuxRunRequest, PathError, run};

use zup_linux::test_support::{
    IsolatedUser, compose_installer, genuine_template, package_bytes, package_bytes_for, v1_files,
};

fn genuine_installer(scratch: &Path, name: &str) -> (PathBuf, Vec<u8>) {
    let package = package_bytes(scratch, "1.0.0", &v1_files());
    let output = scratch.join(name);
    compose_installer(&genuine_template("console"), &output, &package);
    let bytes = std::fs::read(&output).expect("the installer reads");
    (output, bytes)
}

const VERSION_AT: usize = 17;
const OFFSET_AT: usize = 25;
const LENGTH_AT: usize = 33;
const DIGEST_AT: usize = 41;

fn poke_u64(bytes: &mut [u8], field: usize, value: u64) {
    let start = bytes.len() - 81 + field;
    bytes[start..start + 8].copy_from_slice(&value.to_le_bytes());
}

fn poke_u32(bytes: &mut [u8], field: usize, value: u32) {
    let start = bytes.len() - 81 + field;
    bytes[start..start + 4].copy_from_slice(&value.to_le_bytes());
}

fn write_copy(scratch: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = scratch.join(name);
    std::fs::write(&path, bytes).expect("the mutated copy writes");
    path
}

#[test]
fn a_footer_digest_that_disagrees_is_refused() {
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let (_, mut bytes) = genuine_installer(scratch.path(), "setup");
    let digest_at = bytes.len() - 81 + DIGEST_AT;
    bytes[digest_at] ^= 0x01;
    let path = write_copy(scratch.path(), "bad-digest", &bytes);

    match Carrier::open(&path) {
        Err(CarrierError::PackageDigestMismatch { .. }) => {}
        other => panic!("a bad footer digest is refused as mismatch: {other:?}"),
    }
}

#[test]
fn an_overflowing_package_length_is_refused_without_allocation() {
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let (_, mut bytes) = genuine_installer(scratch.path(), "setup");
    poke_u64(&mut bytes, LENGTH_AT, u64::MAX);
    let path = write_copy(scratch.path(), "huge-length", &bytes);

    match Carrier::open(&path) {
        Err(CarrierError::PackageOutsideFile { .. } | CarrierError::ImplausibleLength { .. }) => {}
        other => panic!("an overflowing length is refused before allocation: {other:?}"),
    }
}

#[test]
fn an_offset_outside_the_file_is_refused() {
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let (_, mut bytes) = genuine_installer(scratch.path(), "setup");
    let far = bytes.len() as u64 + 4096;
    poke_u64(&mut bytes, OFFSET_AT, far);
    let path = write_copy(scratch.path(), "far-offset", &bytes);

    match Carrier::open(&path) {
        Err(CarrierError::PackageOutsideFile { .. }) => {}
        other => panic!("an outside offset is refused: {other:?}"),
    }
}

#[test]
fn an_unknown_carrier_version_is_refused() {
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let (_, mut bytes) = genuine_installer(scratch.path(), "setup");
    poke_u32(&mut bytes, VERSION_AT, 0xFFFF);
    let path = write_copy(scratch.path(), "future-version", &bytes);

    match Carrier::open(&path) {
        Err(CarrierError::UnsupportedVersion { found: 0xFFFF, .. }) => {}
        other => panic!("a future version is refused explicitly: {other:?}"),
    }
}

#[test]
fn a_windows_package_in_a_linux_runtime_is_refused() {
    let user = IsolatedUser::isolate();
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let windows = TargetTriple::parse("x86_64-pc-windows-msvc").expect("a Windows target");
    let package = package_bytes_for(scratch.path(), &windows, "1.0.0", &v1_files());
    let output = scratch.path().join("mismatched");
    compose_installer(&genuine_template("console"), &output, &package);

    match Carrier::open(&output) {
        Err(CarrierError::TargetMismatch { .. }) => {}
        other => panic!("a target mismatch is refused: {other:?}"),
    }

    match run(&LinuxRunRequest {
        installer: output,
        scope: SelectedScope::User,
        state_root: Some(user.state.clone()),
        action: LinuxAction::Apply,
        install_dir_override: None,
    }) {
        Err(ExecError::Carrier(CarrierError::TargetMismatch { .. })) => {}
        other => panic!("a mismatched run is refused: {other:?}"),
    }
    assert!(
        !user.programs().join("tool").exists(),
        "refusal happens before any mutation"
    );
}

#[test]
fn a_redirected_state_hierarchy_is_refused() {
    let user = IsolatedUser::isolate();
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let (installer, _) = genuine_installer(scratch.path(), "setup");

    let elsewhere = tempfile::tempdir().expect("an unrelated tree");
    let redirected = user.state.join("transactions");
    std::fs::create_dir_all(&user.state).expect("a state root");
    std::os::unix::fs::symlink(elsewhere.path(), &redirected).expect("a planted redirect");
    std::fs::write(elsewhere.path().join("owned"), b"attacker content").expect("write");

    match run(&LinuxRunRequest {
        installer,
        scope: SelectedScope::User,
        state_root: Some(user.state.clone()),
        action: LinuxAction::Apply,
        install_dir_override: None,
    }) {
        Err(ExecError::Path(PathError::Refused { .. })) => {}
        other => panic!("a redirected state hierarchy is refused: {other:?}"),
    }
    assert!(
        !user.programs().join("tool").exists(),
        "refusal happens before any mutation"
    );
    assert_eq!(
        std::fs::read(elsewhere.path().join("owned")).expect("read"),
        b"attacker content",
        "nothing was written through the link"
    );
}

#[test]
fn a_redirected_install_destination_is_refused() {
    let user = IsolatedUser::isolate();
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let (installer, _) = genuine_installer(scratch.path(), "setup");

    let programs = user.programs();
    std::fs::create_dir_all(&programs).expect("a programs root");
    let elsewhere = tempfile::tempdir().expect("an unrelated tree");
    std::fs::remove_dir(&programs).expect("remove the real root");
    std::os::unix::fs::symlink(elsewhere.path(), &programs).expect("a planted redirect");

    match run(&LinuxRunRequest {
        installer,
        scope: SelectedScope::User,
        state_root: Some(user.state.clone()),
        action: LinuxAction::Apply,
        install_dir_override: None,
    }) {
        Err(ExecError::Path(PathError::Refused { .. })) => {}
        other => panic!("a redirected destination is refused: {other:?}"),
    }
    let mut planted: Vec<_> = std::fs::read_dir(elsewhere.path())
        .expect("read")
        .filter_map(|entry| entry.ok().map(|entry| entry.file_name()))
        .collect();
    planted.sort();
    assert!(
        planted.is_empty(),
        "nothing was installed through the link: {planted:?}"
    );
}
