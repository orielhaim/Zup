#![cfg(target_os = "linux")]
#![cfg(feature = "test-support")]

use std::path::{Path, PathBuf};

use zup_compose::{CARRIER_FOOTER_LEN as FOOTER_LEN, ComposeError};
use zup_core::{SelectedScope, Target};
use zup_linux::{Carrier, ExecError, LinuxRunRequest, PathError, run};

use rstest::rstest;
use zup_linux::test_support::{
    IsolatedUser, compose_installer, inert_template, package_bytes, package_bytes_for, v1_files,
};
use zup_runtime::Operation;

fn installer(scratch: &Path, name: &str) -> (PathBuf, Vec<u8>) {
    let package = package_bytes(scratch, "1.0.0", &v1_files());
    let output = scratch.join(name);
    compose_installer(Path::new(inert_template()), &output, &package);
    let bytes = std::fs::read(&output).expect("the installer reads");
    (output, bytes)
}

#[derive(Clone, Copy)]
enum FooterFault {
    Digest,
    Length,
    Offset,
    Version,
    Flags,
}

const VERSION_AT: usize = 17;
const OFFSET_AT: usize = 25;
const LENGTH_AT: usize = 33;
const DIGEST_AT: usize = 41;

fn poke_u64(bytes: &mut [u8], field: usize, value: u64) {
    let start = bytes.len() - FOOTER_LEN as usize + field;
    bytes[start..start + 8].copy_from_slice(&value.to_le_bytes());
}

fn poke_u32(bytes: &mut [u8], field: usize, value: u32) {
    let start = bytes.len() - FOOTER_LEN as usize + field;
    bytes[start..start + 4].copy_from_slice(&value.to_le_bytes());
}

fn write_copy(scratch: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = scratch.join(name);
    std::fs::write(&path, bytes).expect("the mutated copy writes");
    path
}

#[rstest]
#[case::digest(FooterFault::Digest)]
#[case::length(FooterFault::Length)]
#[case::offset(FooterFault::Offset)]
#[case::version(FooterFault::Version)]
#[case::flags(FooterFault::Flags)]
fn a_corrupt_footer_is_refused_before_any_install(#[case] fault: FooterFault) {
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let (_, mut bytes) = installer(scratch.path(), "setup");
    match fault {
        FooterFault::Digest => {
            let digest_at = bytes.len() - FOOTER_LEN as usize + DIGEST_AT;
            bytes[digest_at] ^= 0x01;
        }
        FooterFault::Length => poke_u64(&mut bytes, LENGTH_AT, u64::MAX),
        FooterFault::Offset => {
            let far = bytes.len() as u64 + 4096;
            poke_u64(&mut bytes, OFFSET_AT, far);
        }
        FooterFault::Version => poke_u32(&mut bytes, VERSION_AT, 0xFFFF),
        FooterFault::Flags => poke_u32(&mut bytes, 21, 1),
    }
    let path = write_copy(scratch.path(), "corrupt", &bytes);

    match (fault, Carrier::open(&path)) {
        (FooterFault::Digest, Err(ComposeError::PackageDigestMismatch { .. }))
        | (
            FooterFault::Length,
            Err(ComposeError::ContentOutsideFile { .. } | ComposeError::ImplausibleLength { .. }),
        )
        | (FooterFault::Offset, Err(ComposeError::ContentOutsideFile { .. }))
        | (FooterFault::Version, Err(ComposeError::UnsupportedVersion { found: 0xFFFF, .. })) => {}
        (FooterFault::Flags, Err(ComposeError::UnsupportedFlags { .. })) => {}
        (_, other) => panic!("a corrupt footer is refused: {other:?}"),
    }
}

#[test]
fn a_windows_package_in_a_linux_runtime_is_refused() {
    let user = IsolatedUser::isolate();
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let windows = Target::parse("windows-x64").expect("a Windows target");
    let package = package_bytes_for(scratch.path(), &windows, "1.0.0", &v1_files());
    let output = scratch.path().join("mismatched");
    compose_installer(Path::new(inert_template()), &output, &package);

    match Carrier::open(&output) {
        Err(ComposeError::Target(_) | ComposeError::FormatMismatch { .. }) => {}
        other => panic!("a target mismatch is refused: {other:?}"),
    }

    match run(&LinuxRunRequest {
        installer: output,
        scope: SelectedScope::User,
        state_root: Some(user.state.clone()),
        action: Operation::Apply,
        install_dir_override: None,
        selection: Default::default(),
    }) {
        Err(ExecError::Carrier(ComposeError::Target(_) | ComposeError::FormatMismatch { .. })) => {}
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
    let (installer, _) = installer(scratch.path(), "setup");

    let elsewhere = tempfile::tempdir().expect("an unrelated tree");
    let redirected = user.state.join("transactions");
    std::fs::create_dir_all(&user.state).expect("a state root");
    std::os::unix::fs::symlink(elsewhere.path(), &redirected).expect("a planted redirect");
    std::fs::write(elsewhere.path().join("owned"), b"attacker content").expect("write");

    match run(&LinuxRunRequest {
        installer,
        scope: SelectedScope::User,
        state_root: Some(user.state.clone()),
        action: Operation::Apply,
        install_dir_override: None,
        selection: Default::default(),
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
    let (installer, _) = installer(scratch.path(), "setup");

    let programs = user.programs();
    std::fs::create_dir_all(&programs).expect("a programs root");
    let elsewhere = tempfile::tempdir().expect("an unrelated tree");
    std::fs::remove_dir(&programs).expect("remove the real root");
    std::os::unix::fs::symlink(elsewhere.path(), &programs).expect("a planted redirect");

    match run(&LinuxRunRequest {
        installer,
        scope: SelectedScope::User,
        state_root: Some(user.state.clone()),
        action: Operation::Apply,
        install_dir_override: None,
        selection: Default::default(),
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
