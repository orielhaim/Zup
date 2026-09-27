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

const ALL_MATRICES: &str = "\
portable-core: crates that build and test on a non-Windows host
  zup-core
  zup-manifest
  zup-build
  zup-plan
  zup-platform
  zup-exec
  zup-transaction
  zup-bootstrap
  zup-bundle
  zup-acquire
  zup-acquire-http
  zup-artifact
  zup-protocol
  zup-runtime
  zup-presentation
  zup-update
  zup-plugin-contract
  zup-plugin-build
  zup-plugin-runtime
portable-tests: portable crates that verify the stack instead of shipping in an installer
  zup-xtask
windows-only: crates that require a Windows build host
  zup-pe
  zup-windows
  zup-dispatch
  zup
  zup-ui
";

#[test]
fn no_arguments_prints_usage() {
    let output = xtask(&[]);
    assert_eq!(code(&output), 2);
    assert_eq!(String::from_utf8_lossy(&output.stdout), "");
    assert!(stderr(&[]).contains("emit-portable-matrix"));
}

#[test]
fn help_lists_both_commands_and_the_exit_codes() {
    let help = stdout(&["help"]);
    assert!(help.contains("emit-portable-matrix"), "{help}");
    assert!(help.contains("verify-portable-boundaries"), "{help}");
    assert!(help.contains("0  clean"), "{help}");
    assert!(help.contains("1  boundary violations"), "{help}");
    assert!(help.contains("2  usage or unreadable workspace"), "{help}");
}

#[test]
fn the_matrix_view_lists_every_package_in_declaration_order() {
    assert_eq!(stdout(&["emit-portable-matrix"]), ALL_MATRICES);
}

#[test]
fn one_matrix_can_be_selected() {
    assert_eq!(
        stdout(&["emit-portable-matrix", "--matrix", "windows-only"]),
        "windows-only: crates that require a Windows build host\n  zup-pe\n  zup-windows\n  zup-dispatch\n  zup\n  zup-ui\n"
    );
    assert_eq!(
        stdout(&["emit-portable-matrix", "--matrix", "portable-tests"]),
        "portable-tests: portable crates that verify the stack instead of shipping in an installer\n  zup-xtask\n"
    );
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
        "portable-tests: portable crates that verify the stack instead of shipping in an installer\n  zup-xtask\n\
         portable-core: crates that build and test on a non-Windows host\n  zup-core\n  zup-manifest\n  \
         zup-build\n  zup-plan\n  zup-platform\n  zup-exec\n  zup-transaction\n  zup-bootstrap\n  \
         zup-bundle\n  zup-acquire\n  zup-acquire-http\n  zup-artifact\n  zup-protocol\n  zup-runtime\n  \
         zup-presentation\n  zup-update\n  \
         zup-plugin-contract\n  zup-plugin-build\n  zup-plugin-runtime\n"
    );
}

#[test]
fn cargo_args_format_emits_package_flags_for_one_matrix() {
    assert_eq!(
        stdout(&[
            "emit-portable-matrix",
            "--matrix",
            "portable-core",
            "--format",
            "cargo-args"
        ]),
        "-p zup-core -p zup-manifest -p zup-build -p zup-plan -p zup-platform -p zup-exec \
         -p zup-transaction -p zup-bootstrap -p zup-bundle -p zup-acquire -p zup-acquire-http \
         -p zup-artifact -p zup-protocol \
         -p zup-runtime -p zup-presentation -p zup-update -p zup-plugin-contract \
         -p zup-plugin-build -p zup-plugin-runtime\n"
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
    for known in ["portable-core", "portable-tests", "windows-only"] {
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
