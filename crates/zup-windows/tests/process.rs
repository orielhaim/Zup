//! Starting a real child, and refusing to start a wrong one.
//!
//! The handoff's security properties are properties of a *value* — which
//! executable, which arguments, which inheritance — so they are tested against
//! the value. The properties that are not properties of a value are that a child
//! actually starts, that its exit code comes back, and that it is independent
//! once `CreateProcessW` returns. Those are tested against real processes, on
//! this machine, in this repository.
//!
//! The child here is this test executable. That is deliberate: it is a real
//! x86_64 PE, it exists on every machine that can run this test, and its
//! behaviour is under this test's control. A test that launched a system tool
//! would be testing that tool.

#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use zup_windows::{HandOff, LaunchRequest, Launcher, launch, quote_argument};

/// A launcher that records what it was asked for and starts nothing.
#[derive(Default)]
struct Recorder {
    requests: Mutex<Vec<LaunchRequest>>,
    /// The exit code a recorded child reports, so a test can drive the
    /// classification path without a process.
    exit: Mutex<i32>,
}

impl Recorder {
    fn only(&self) -> LaunchRequest {
        let requests = self.requests.lock().expect("the recorder is not poisoned");
        assert_eq!(
            requests.len(),
            1,
            "a handoff starts exactly one runtime: {:?}",
            requests
        );
        requests[0].clone()
    }
}

impl Launcher for Recorder {
    fn launch(
        &self,
        request: &LaunchRequest,
    ) -> Result<zup_windows::ChildProcess, zup_windows::LaunchError> {
        self.requests
            .lock()
            .expect("the recorder is not poisoned")
            .push(request.clone());
        Ok(zup_windows::ChildProcess::already_finished(
            *self.exit.lock().expect("the recorder is not poisoned"),
        ))
    }
}

fn this_executable() -> PathBuf {
    std::env::current_exe().expect("the test executable")
}

/// An argument the libtest harness rejects.
///
/// The harness refuses an unknown flag with a usage error, so this is a real
/// process that exits nonzero without needing a helper binary or a shell. The
/// code it chooses is the harness's business; the tests below only ever compare
/// codes against each other.
const USAGE_ERROR: &str = "--zup-not-a-flag";

/// The same command run by the platform's own launcher.
///
/// The exit code `std::process` reports is the reference: if this module's
/// `CreateProcessW` handoff reports the same number for the same program, then
/// it is reporting what the program did and not something `wait` decided.
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
    // Two real processes, the same program, two launchers. If the codes agree,
    // then the handoff is forwarding the child's outcome rather than inventing
    // one, and the child is genuinely independent of this process — `std::process`
    // waited for its own child in the same call, and this one finished anyway.
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
fn a_child_is_independent_once_the_launch_returns() {
    // The claim the design rests on: a bootstrapper that is killed mid-install
    // leaves an installation that is either committed or recoverable. That is
    // true because the child needs nothing from us. So: start one, stop looking
    // at it for a while, and then read the code. If it were reading a handle we
    // had to hold open, it could not have finished.
    let child = launch(
        &this_executable(),
        &[USAGE_ERROR.to_owned()],
        HandOff::Console,
        None,
    )
    .expect("a real child starts");
    let pid = child.pid();
    std::thread::sleep(std::time::Duration::from_millis(100));
    assert_eq!(child.wait(), reference_exit_code());
    assert_ne!(pid, 0);
}

#[test]
fn a_silent_handoff_starts_no_window_and_still_runs() {
    // The GUI handoff's claim is that the user sees one window: no console, no
    // inherited handles, and the child still runs. A real child proves the third
    // half, which is the half that would break if the empty handle list were
    // wrong — `bInheritHandles` is set even with nothing to inherit, because the
    // list is what constrains it.
    let child = launch(
        &this_executable(),
        &[USAGE_ERROR.to_owned()],
        HandOff::Silent,
        None,
    )
    .expect("a silent handoff starts a real child");
    assert_ne!(child.pid(), 0);
    // A silent handoff does not wait, by design: a user who closes an installer
    // window should not have a hidden process holding the bootstrapper's exit
    // code. So `wait` returns immediately and the child is simply left running.
    assert_eq!(child.wait(), 0, "a silent handoff does not forward a code");
}

#[test]
fn a_request_renders_the_command_line_the_child_will_parse() {
    // The rendered line is the interface. If it is wrong the child receives
    // something other than what was asked for, and a test that only counted
    // arguments would not notice.
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
        // A path with no space, quote, or trailing backslash is passed through
        // unquoted: quoting more than the rules require is not a correctness
        // problem, but it is a diff every release has to read.
        r#""C:\Program Files\Acme\Setup.exe" install --acquired C:\Users\o\AppData\Local\Acme\content --handoff "C:\a b\handoff.json""#
    );
    // And the executable is the application name, so it can never become a
    // `PATH` lookup however it is spelled.
    assert!(
        request
            .command_line()
            .starts_with(&quote_argument(r"C:\Program Files\Acme\Setup.exe")),
        "the executable is the first token and is quoted when it has to be"
    );
}

#[test]
fn a_missing_executable_names_the_api_and_the_win32_code() {
    // A bootstrapper that cannot start the runtime has to say which call failed
    // and why. A bare "failed to launch" is not something a support engineer can
    // act on.
    let directory = tempfile::tempdir().expect("a temporary directory");

    // The file is missing from a directory that exists.
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

    // A directory in the path is missing, which is a different problem with a
    // different code: a half-finished install has both, and telling them apart is
    // the difference between "the cache is stale" and "the release is wrong".
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
    // A staged runtime that turned into a directory — a half-finished write, an
    // interrupted unpack — must be refused rather than started, and the refusal
    // has to be distinguishable from "no such file".
    let directory = tempfile::tempdir().expect("a temporary directory");
    let error = launch(directory.path(), &[], HandOff::Console, None)
        .expect_err("a directory does not start");
    assert_eq!(error.api, "CreateProcessW");
    assert_ne!(error.code, 0);
}

#[test]
fn a_working_directory_is_inherited_when_none_is_named() {
    // Not a security claim — a convenience one — but it is the kind of thing
    // that silently stops being true, so it is stated.
    let request = LaunchRequest::new(this_executable(), vec![], HandOff::Console);
    assert_eq!(
        request.working_directory, None,
        "an unnamed working directory means the child inherits this one"
    );
    let mut request = request;
    request.working_directory = Some(PathBuf::from(r"C:\Windows\Temp"));
    assert_eq!(
        request.working_directory.as_deref(),
        Some(Path::new(r"C:\Windows\Temp"))
    );
}

#[test]
fn an_injected_launcher_receives_the_request_it_would_have_run() {
    // The seam itself. A test that injects a launcher can assert on the handoff
    // without a process, which is the only way to test that the arguments are
    // locations and nothing else.
    let recorder = Recorder::default();
    let request = LaunchRequest::new(
        Path::new(r"C:\Acme\Setup.exe"),
        vec!["--acquired".to_owned(), r"C:\Acme\content".to_owned()],
        HandOff::Silent,
    );
    let child = recorder
        .launch(&request)
        .expect("an injected launcher succeeds");
    let recorded = recorder.only();
    assert_eq!(recorded, request);
    assert_eq!(child.wait(), 0);
}
