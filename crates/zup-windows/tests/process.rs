#![cfg(windows)]

use std::path::{Path, PathBuf};

use zup_windows::{HandOff, LaunchRequest, launch, quote_argument};

fn this_executable() -> PathBuf {
    std::env::current_exe().expect("the test executable")
}

const USAGE_ERROR: &str = "--zup-not-a-flag";

fn reference_exit_code() -> i32 {
    std::process::Command::new(this_executable())
        .arg(USAGE_ERROR)
        .output()
        .expect("the platform launcher runs a real child")
        .status
        .code()
        .expect("a real child that exits normally has a code")
}

#[test]
fn a_real_child_starts_and_reports_the_platforms_own_exit_code() {
    let expected = reference_exit_code();
    assert_ne!(expected, 0, "the probe must fail, or it proves nothing");

    let child = launch(
        &this_executable(),
        &[USAGE_ERROR.to_owned()],
        HandOff::Console,
        None,
    )
    .expect("a real child starts");
    assert_ne!(child.pid(), 0, "a started process has a process id");
    assert_eq!(
        child.wait(),
        expected,
        "the child's own exit code comes back unchanged"
    );
}

#[test]
fn a_silent_handoff_starts_no_window_and_still_runs() {
    let child = launch(
        &this_executable(),
        &[USAGE_ERROR.to_owned()],
        HandOff::Silent,
        None,
    )
    .expect("a silent handoff starts a real child");
    assert_ne!(child.pid(), 0);

    assert_eq!(child.wait(), 0, "a silent handoff does not forward a code");
}

#[test]
fn a_request_renders_the_command_line_the_child_will_parse() {
    let request = LaunchRequest::new(
        Path::new(r"C:\Program Files\Acme\Setup.exe"),
        vec![
            "install".to_owned(),
            "--acquired".to_owned(),
            r"C:\Users\o\AppData\Local\Acme\content".to_owned(),
            "--handoff".to_owned(),
            r"C:\a b\handoff.json".to_owned(),
        ],
        HandOff::Console,
    );
    assert_eq!(
        request.command_line(),
        r#""C:\Program Files\Acme\Setup.exe" install --acquired C:\Users\o\AppData\Local\Acme\content --handoff "C:\a b\handoff.json""#
    );

    assert!(
        request
            .command_line()
            .starts_with(&quote_argument(r"C:\Program Files\Acme\Setup.exe")),
        "the executable is the first token and is quoted when it has to be"
    );
}

#[test]
fn a_missing_executable_names_the_api_and_the_win32_code() {
    let directory = tempfile::tempdir().expect("a temporary directory");

    let error = launch(
        &directory.path().join("Setup.exe"),
        &[],
        HandOff::Console,
        None,
    )
    .expect_err("a missing executable does not start");
    assert_eq!(error.api, "CreateProcessW");
    assert_eq!(error.code, 2, "ERROR_FILE_NOT_FOUND");
    assert!(error.to_string().contains("CreateProcessW"), "{error}");

    let error = launch(
        &directory.path().join("nope").join("Setup.exe"),
        &[],
        HandOff::Console,
        None,
    )
    .expect_err("a missing directory does not start");
    assert_eq!(error.api, "CreateProcessW");
    assert_eq!(error.code, 3, "ERROR_PATH_NOT_FOUND");
}

#[test]
fn a_directory_is_not_an_executable() {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let error = launch(directory.path(), &[], HandOff::Console, None)
        .expect_err("a directory does not start");
    assert_eq!(error.api, "CreateProcessW");
    assert_ne!(error.code, 0);
}
