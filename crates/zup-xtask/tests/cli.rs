//! Command-line output fixtures.
//!
//! The expected text is the documented matrix view, so a change in the emitted
//! matrix is a deliberate change to this file.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use tempfile::TempDir;
use zup_xtask::matrix;

fn xtask(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_xtask"))
        .args(args)
        .output()
        .expect("xtask runs")
}

fn stdout(args: &[&str]) -> String {
    let output = xtask(args);
    assert!(
        output.status.success(),
        "{args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("stdout is utf-8")
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

/// The rendered view of one matrix, as the library renders it.
///
/// Derived rather than pasted, because a pasted copy of the matrix is a second
/// place to update every time a package is added — and the two copies are what
/// this test would then be asserting against each other. What is under test here
/// is the command line: selection, ordering, and format. The matrix's *contents*
/// are pinned by the boundary rules, which fail on a member no matrix names.
fn view(name: &str) -> String {
    matrix::render(&[matrix::matrix(name).expect("a known matrix")])
}

#[test]
fn no_arguments_prints_usage() {
    let output = xtask(&[]);
    assert_eq!(code(&output), 2);
    assert_eq!(String::from_utf8_lossy(&output.stdout), "");
    assert!(stderr(&[]).contains("emit-portable-matrix"));
}

#[test]
fn help_lists_every_command_and_the_exit_codes() {
    let help = stdout(&["help"]);
    assert!(help.contains("emit-portable-matrix"), "{help}");
    assert!(help.contains("verify-portable-boundaries"), "{help}");
    assert!(help.contains("github-action-pins check"), "{help}");
    assert!(help.contains("github-action-pins refresh"), "{help}");
    assert!(help.contains("0  clean"), "{help}");
    assert!(help.contains("1  problems found"), "{help}");
    assert!(help.contains("2  usage or unreadable workspace"), "{help}");
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

#[test]
fn the_pin_check_passes_on_a_workspace_with_no_workflows() {
    // A repository that has not generated a workflow yet is not a failure.
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
fn the_matrix_view_lists_every_matrix_in_declaration_order() {
    assert_eq!(
        stdout(&["emit-portable-matrix"]),
        matrix::render(&matrix::MATRICES.iter().collect::<Vec<_>>())
    );
}

#[test]
fn one_matrix_can_be_selected() {
    for name in matrix::names() {
        assert_eq!(
            stdout(&["emit-portable-matrix", "--matrix", name]),
            view(name),
            "{name}"
        );
    }
}

#[test]
fn a_repeated_matrix_selector_keeps_the_requested_order() {
    assert_eq!(
        stdout(&[
            "emit-portable-matrix",
            "--matrix",
            "portable-tests",
            "--matrix",
            "portable-core"
        ]),
        format!("{}{}", view("portable-tests"), view("portable-core"))
    );
}

#[test]
fn cargo_args_format_emits_package_flags_for_one_matrix() {
    let core = matrix::matrix("portable-core").expect("a known matrix");
    assert_eq!(
        stdout(&[
            "emit-portable-matrix",
            "--matrix",
            "portable-core",
            "--format",
            "cargo-args"
        ]),
        format!("{}\n", matrix::render_cargo_args(core))
    );
}

#[test]
fn cargo_args_format_needs_exactly_one_matrix() {
    for args in [
        vec!["emit-portable-matrix", "--format", "cargo-args"],
        vec![
            "emit-portable-matrix",
            "--matrix",
            "portable-core",
            "--matrix",
            "portable-tests",
            "--format",
            "cargo-args",
        ],
    ] {
        let output = xtask(&args);
        assert_eq!(code(&output), 2, "{args:?}");
        assert!(stderr(&args).contains("exactly one --matrix"), "{args:?}");
    }
}

#[test]
fn an_unknown_matrix_is_a_usage_error_naming_the_known_matrices() {
    let output = xtask(&["emit-portable-matrix", "--matrix", "portable-everything"]);
    assert_eq!(code(&output), 2);
    let message = stderr(&["emit-portable-matrix", "--matrix", "portable-everything"]);
    assert!(message.contains("portable-everything"), "{message}");
    for known in [
        "portable-core",
        "portable-file-format",
        "portable-tests",
        "windows-only",
    ] {
        assert!(message.contains(known), "{message}");
    }
}

#[test]
fn an_unknown_format_is_a_usage_error() {
    let output = xtask(&["emit-portable-matrix", "--format", "json"]);
    assert_eq!(code(&output), 2);
    assert!(stderr(&["emit-portable-matrix", "--format", "json"]).contains("cargo-args"));
}

#[test]
fn an_option_from_another_command_is_a_usage_error() {
    let output = xtask(&["verify-portable-boundaries", "--format", "text"]);
    assert_eq!(code(&output), 2);
    let message = stderr(&["verify-portable-boundaries", "--format", "text"]);
    assert!(message.contains("--format"), "{message}");
    assert!(message.contains("verify-portable-boundaries"), "{message}");
}

#[test]
fn an_unknown_command_is_a_usage_error() {
    let output = xtask(&["verify-everything"]);
    assert_eq!(code(&output), 2);
    assert!(stderr(&["verify-everything"]).contains("verify-everything"));
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
