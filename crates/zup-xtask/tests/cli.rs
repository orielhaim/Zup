//! The exit codes and messages the command line promises.
//!
//! The two checks that gate a repository - the boundary check and the pin check -
//! are covered here end to end, because a gate that cannot fail is a gate nobody
//! reads. The usage errors are pinned because a silently-ignored argument is how a
//! typo in a `--matrix` name builds the wrong thing.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use rstest::rstest;
use tempfile::TempDir;
use zup_xtask::matrix;

fn xtask(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_xtask"))
        .args(args)
        .output()
        .expect("xtask runs")
}

fn stderr(args: &[&str]) -> String {
    let output = xtask(args);
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn code(output: &Output) -> i32 {
    output.status.code().expect("xtask exits normally")
}

/// A workspace holding exactly the packages the matrices name, so the
/// matrix-membership rule is satisfied and one package can be dirtied.
fn complete_workspace() -> TempDir {
    let root = TempDir::new().expect("temp workspace");
    for package in matrix::all() {
        let directory = root.path().join("crates").join(package);
        fs::create_dir_all(&directory).expect("package dir");
        fs::write(
            directory.join("Cargo.toml"),
            format!("[package]\nname = \"{package}\"\nversion = \"0.0.1\"\nedition = \"2024\"\n"),
        )
        .expect("package manifest");
    }
    fs::write(
        root.path().join("Cargo.toml"),
        "[workspace]\nresolver = \"3\"\nmembers = [\"crates/*\"]\n",
    )
    .expect("root manifest");
    root
}

/// An argument the parser does not recognise is a usage error, never a silent
/// no-op: a typo in a `--matrix` name must not quietly build the wrong thing.
#[rstest]
#[case::an_unknown_command(&["verify-everything"], "verify-everything")]
#[case::an_unknown_matrix(&["emit-portable-matrix", "--matrix", "nope"], "nope")]
#[case::an_option_from_another_command(
    &["verify-portable-boundaries", "--format", "text"],
    "--format"
)]
fn a_rejected_argument_is_a_usage_error(#[case] args: &[&str], #[case] named: &str) {
    let output = xtask(args);
    assert_eq!(code(&output), 2, "{args:?}");
    assert!(
        stderr(args).contains(named),
        "{args:?} should name `{named}`"
    );
}

#[test]
fn the_pin_check_reports_a_broken_lock_rather_than_passing() {
    // A `check` that cannot fail is a `check` nobody reads. The fixture has a
    // workflow whose `uses:` does not match the lock, which is the failure this
    // command exists to catch.
    let fixture = tempfile::tempdir().expect("a temporary workspace");
    let workflows = fixture.path().join(".github").join("workflows");
    std::fs::create_dir_all(&workflows).expect("a directory");
    std::fs::write(
        workflows.join("release.yml"),
        "jobs:\n  build:\n    steps:\n      - uses: actions/checkout@v5\n",
    )
    .expect("a workflow");

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_xtask"))
        .args(["github-action-pins", "check", "--root"])
        .arg(fixture.path())
        .output()
        .expect("xtask runs");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("actions/checkout"), "{stderr}");
    assert!(stderr.contains("does not match the lock"), "{stderr}");
}

/// The negative control for the pin check: a repository that has not generated a
/// workflow yet is not a failure. Without it, a `check` that reported nothing at
/// all would be indistinguishable from a `check` that passed.
#[test]
fn the_pin_check_passes_on_a_workspace_with_no_workflows() {
    let fixture = tempfile::tempdir().expect("a temporary workspace");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_xtask"))
        .args(["github-action-pins", "check", "--root"])
        .arg(fixture.path())
        .output()
        .expect("xtask runs");
    assert_eq!(output.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&output.stdout).contains("tracked"));
}

#[test]
fn the_boundary_check_reports_a_fixture_violation_and_exits_nonzero() {
    let root = complete_workspace();
    let source = root.path().join("crates").join("zup-core").join("src");
    fs::create_dir_all(&source).expect("source dir");
    fs::write(
        source.join("lib.rs"),
        "use std::os::windows::fs::MetadataExt;\n",
    )
    .expect("source");

    let output = xtask(&[
        "verify-portable-boundaries",
        "--root",
        root.path().to_str().unwrap(),
    ]);
    assert_eq!(code(&output), 1);
    assert_eq!(String::from_utf8_lossy(&output.stdout), "");
    let message = String::from_utf8_lossy(&output.stderr);
    assert!(
        message.contains("1 portable boundary violation"),
        "{message}"
    );
    assert!(message.contains("os-windows-import"), "{message}");
    assert!(message.contains("zup-core/src/lib.rs:1"), "{message}");
}

#[test]
fn a_clean_workspace_boundary_check_is_silent() {
    let root = complete_workspace();
    let output = xtask(&[
        "verify-portable-boundaries",
        "--root",
        root.path().to_str().unwrap(),
    ]);
    assert_eq!(
        code(&output),
        0,
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout), "");
    assert_eq!(String::from_utf8_lossy(&output.stderr), "");
}

/// A CI job runs the check with no arguments, so the default root has to be the
/// repository rather than whatever the working directory happens to be. Both
/// invocations must reach the same tree, which is what an identical exit code and
/// an identical report show.
#[test]
fn the_repository_root_is_the_default_workspace() {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let explicit = xtask(&[
        "verify-portable-boundaries",
        "--root",
        repository.to_str().unwrap(),
    ]);
    let implicit = xtask(&["verify-portable-boundaries"]);
    assert_eq!(code(&implicit), code(&explicit));
    assert_eq!(
        String::from_utf8_lossy(&implicit.stderr),
        String::from_utf8_lossy(&explicit.stderr)
    );
}
