#![cfg(target_os = "linux")]

//! Attacks against a real ELF carrier, and the refusals they must produce.
//!
//! The unit tests in `carrier.rs` prove the parser against synthetic bytes;
//! these prove the whole open path against a genuine composed installer: a
//! real ELF template with a real package appended. Every test mutates the
//! image and then runs it, asserting refusal *before* machine mutation - no
//! payload installed, no ledger written, no panic.
//!
//! Footer layout (offsets from the footer's own start, 81 bytes total):
//! magic[0..17], version[17..21], flags[21..25], offset[25..33],
//! length[33..41], digest[41..73], footer-length[73..81]. All integers are
//! little-endian, matching the composer.

#[path = "support.rs"]
mod support;

use std::path::{Path, PathBuf};

use zup_core::{SelectedScope, TargetTriple};
use zup_linux::{Carrier, CarrierError, LinuxAction, LinuxRunError, LinuxRunRequest, run};

use support::{IsolatedUser, compose_installer, genuine_template, package_bytes_for, v1_files};

/// Compose a genuine installer and return its bytes plus path.
fn genuine_installer(scratch: &Path, name: &str) -> (PathBuf, Vec<u8>) {
    let package = support::package_bytes(scratch, "1.0.0", &v1_files());
    let output = scratch.join(name);
    compose_installer(&genuine_template("console"), &output, &package);
    let bytes = std::fs::read(&output).expect("the installer reads");
    (output, bytes)
}

/// Footer-relative field offsets, matching the carrier format documentation.
const VERSION_AT: usize = 17;
const OFFSET_AT: usize = 25;
const LENGTH_AT: usize = 33;
const DIGEST_AT: usize = 41;

/// Overwrite an 81-byte footer's u64 field, little-endian.
fn poke_u64(bytes: &mut [u8], field: usize, value: u64) {
    let start = bytes.len() - 81 + field;
    bytes[start..start + 8].copy_from_slice(&value.to_le_bytes());
}

/// Overwrite an 81-byte footer's u32 field, little-endian.
fn poke_u32(bytes: &mut [u8], field: usize, value: u32) {
    let start = bytes.len() - 81 + field;
    bytes[start..start + 4].copy_from_slice(&value.to_le_bytes());
}

fn write_copy(scratch: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = scratch.join(name);
    std::fs::write(&path, bytes).expect("the mutated copy writes");
    path
}

/// A footer digest that disagrees with the package bytes is refused as the
/// wrong package, not parsed as a malformed one.
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

/// A package length of `u64::MAX` must not allocate or wrap: checked
/// arithmetic refuses it before any read.
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

/// An offset past the end of the file addresses nothing: refused, not read.
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

/// A carrier format the future invented is refused explicitly, not parsed
/// with this build's field offsets.
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

/// A genuine Linux runtime carrying a Windows package is refused before any
/// mutation: the image and the package disagree about the machine.
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

    // And the full run refuses the same way, with nothing installed.
    match run(&LinuxRunRequest {
        installer: output,
        scope: SelectedScope::User,
        state_root: Some(user.state.clone()),
        action: LinuxAction::Apply,
    }) {
        Err(LinuxRunError::Carrier(CarrierError::TargetMismatch { .. })) => {}
        other => panic!("a mismatched run is refused: {other:?}"),
    }
    assert!(
        !user.programs().join("tool").exists(),
        "refusal happens before any mutation"
    );
}

/// A state hierarchy reached through a symbolic link is refused before the
/// journal, the ledger, or the lock is touched.
///
/// The planted redirect points at an attacker tree: a run that followed it
/// would read and write ownership records wherever the link points.
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
    }) {
        Err(LinuxRunError::RefusedPath { .. }) => {}
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

/// An install destination reached through a symbolic link is refused.
///
/// The programs root itself is swapped for a link between composition and
/// execution - exactly the window an attacker has - and the run must refuse
/// rather than install the payload into the linked tree.
#[test]
fn a_redirected_install_destination_is_refused() {
    let user = IsolatedUser::isolate();
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let (installer, _) = genuine_installer(scratch.path(), "setup");

    // Resolve where the payload would go, then replace a parent with a link.
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
    }) {
        Err(LinuxRunError::RefusedPath { .. }) => {}
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
