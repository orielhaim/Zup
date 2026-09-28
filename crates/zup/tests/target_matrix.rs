use std::{fs, path::PathBuf, process::Command};

use tempfile::TempDir;
#[cfg(windows)]
use zup_windows::EmbeddedBundle;

#[path = "support/toolchain_fixture.rs"]
mod toolchain_fixture;

#[cfg(target_arch = "aarch64")]
const X64: &str = "x86_64-pc-windows-msvc";
#[cfg(not(target_arch = "aarch64"))]
const ARM64: &str = "aarch64-pc-windows-msvc";
const HOST_TARGET: &str = zup_plugin_contract::HOST_TARGET;
#[cfg(target_arch = "aarch64")]
const OTHER_TARGET: &str = X64;
#[cfg(not(target_arch = "aarch64"))]
const OTHER_TARGET: &str = ARM64;

/// A target no backend implements on this build host.
#[cfg(windows)]
const UNSUPPORTED_TARGET: &str = "aarch64-unknown-linux-gnu";
#[cfg(not(windows))]
const UNSUPPORTED_TARGET: &str = "x86_64-pc-windows-msvc";

/// The frontend every fixture project declares.
///
/// One frontend, named here rather than selected at compile time: the developer
/// CLI has no presentation features, so there is nothing for a test binary's own
/// build to have selected. `zup build` finds the matching runtime template through
/// the toolchain resolver, exactly as it does for a person who installed `zup`.
const FRONTEND: &str = "gui";

fn zup() -> Command {
    Command::new(env!("CARGO_BIN_EXE_zup"))
}

/// A diagnostic as one string. miette draws a box and wraps messages to the
/// terminal width, so the drawing characters and whitespace runs are removed
/// before a message is matched.
fn flat(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr)
        .chars()
        .filter(|character| !"│╭╰─×•⚠".contains(*character))
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn write_matrix_project() -> (TempDir, PathBuf) {
    let root = TempDir::new().unwrap();
    fs::create_dir_all(root.path().join("dist/alpha")).unwrap();
    fs::create_dir_all(root.path().join("dist/beta")).unwrap();
    fs::write(root.path().join("dist/alpha/app.bin"), b"alpha").unwrap();
    fs::write(root.path().join("dist/beta/app.bin"), b"beta").unwrap();
    let manifest = format!(
        r#"schema = 1
[app]
id = "com.example.cli-matrix"
name = "CLI Matrix"
version = "1.0.0"
[build]
[build.targets.alpha]
target = "{HOST_TARGET}"
source = {{ directory = "dist/alpha" }}
[build.targets.beta]
target = "{OTHER_TARGET}"
source = {{ directory = "dist/beta" }}
[install]
scope = "user"
allow_directory_override = true
[install.directory]
user = "${{location.user_data}}/CliMatrix"
"#
    );
    let path = root.path().join("zup.toml");
    fs::write(&path, manifest).unwrap();
    (root, path)
}

#[test]
fn repeated_profile_and_raw_triple_selection_are_checked() {
    let (root, manifest) = write_matrix_project();
    for selection in [
        vec!["--target", "alpha", "--target", HOST_TARGET],
        vec!["--target", OTHER_TARGET],
        vec![],
    ] {
        let output = zup()
            .args(["check", "--manifest"])
            .arg(&manifest)
            .args(&selection)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{selection:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    // A check reads the source tree and resolves; it must not write to it.
    assert_eq!(
        fs::read(root.path().join("dist/alpha/app.bin")).unwrap(),
        b"alpha"
    );
}

/// A manifest whose profiles disagree with the command line, so precedence is
/// observable: the CLI must win for source, install directory, and frontend.
#[cfg(windows)]
fn write_precedence_project() -> (TempDir, PathBuf) {
    let root = TempDir::new().unwrap();
    for name in ["declared", "cli"] {
        fs::create_dir_all(root.path().join(format!("out/{name}"))).unwrap();
        fs::write(
            root.path().join(format!("out/{name}/app.bin")),
            name.as_bytes(),
        )
        .unwrap();
    }
    let manifest = root.path().join("zup.toml");
    fs::write(
        &manifest,
        format!(
            r#"schema = 1
[app]
id = "com.example.cli-precedence"
name = "Cli Precedence"
version = "1.0.0"
[build]
[build.targets.default]
target = "{HOST_TARGET}"
source = {{ directory = "out/declared" }}
frontend = "gui"
[install]
scope = "user"
[install.directory]
user = "${{location.user_data}}/CliPrecedence"
"#
        ),
    )
    .unwrap();
    (root, manifest)
}

/// `zup check` reports what it resolved, so precedence is visible without a build.
#[cfg(windows)]
fn check_resolved(manifest: &std::path::Path, extra: &[&str]) -> String {
    let output = zup()
        .args(["check", "--manifest"])
        .arg(manifest)
        .args(extra)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[cfg(windows)]
#[test]
fn cli_overrides_beat_profile_source_and_install_directory() {
    let (_root, manifest) = write_precedence_project();

    let declared = check_resolved(&manifest, &[]);
    assert!(declared.contains("out/declared"), "{declared}");
    assert!(
        declared.contains("${location.user_data}/CliPrecedence"),
        "{declared}"
    );

    let overridden = check_resolved(
        &manifest,
        &[
            "--source",
            "out/cli",
            "--install-directory",
            r"D:\Apps\CliPrecedence",
        ],
    );
    assert!(overridden.contains("out/cli"), "{overridden}");
    assert!(
        !overridden.contains("out/declared"),
        "the profile source is fully replaced: {overridden}"
    );
    assert!(
        overridden.contains(r"D:\Apps\CliPrecedence"),
        "{overridden}"
    );
    assert!(
        !overridden.contains("${location.user_data}/CliPrecedence"),
        "the common install directory is fully replaced: {overridden}"
    );
}

/// A per-target flag is one value per selected target, on every command.
///
/// `build_inputs::every_repeatable_per_target_flag_uses_one_alignment_message`
/// pins the message; this pins that the CLI actually routes every command and
/// every flag through it, which a unit test on the formatter cannot see.
#[test]
fn repeatable_resolution_flags_must_line_up_with_the_selection() {
    let (root, manifest) = write_matrix_project();
    let output_path = root.path().join("misaligned-Setup.exe");
    // `build` resolves a runtime before it reaches the output, so the two
    // templates have to be individually correct or the output problem would never
    // be the one reported.
    let alpha = toolchain_fixture::runtime(HOST_TARGET, zup_core::Frontend::Gui).write(root.path());
    let beta = toolchain_fixture::runtime(OTHER_TARGET, zup_core::Frontend::Gui).write(root.path());
    for command in ["build", "check", "doctor"] {
        for (flag, value, noun) in [
            ("--source", "dist/alpha", "sources"),
            ("--install-directory", "C:/Acme", "install directories"),
            // One output for two targets is the same rule, and a refused build
            // must leave the output path untouched. `check` and `doctor` do not
            // take `--output` at all; the commands that compose are the ones that
            // need the rule.
            ("--output", output_path.to_str().unwrap(), "outputs"),
        ] {
            if flag == "--output" && command != "build" {
                continue;
            }
            let mut command_line = zup();
            command_line.args([command, "--manifest"]).arg(&manifest);
            if command == "build" {
                command_line
                    .arg("--runtime")
                    .arg(&alpha)
                    .arg("--runtime")
                    .arg(&beta);
            }
            let output = command_line.arg(flag).arg(value).output().unwrap();
            assert!(
                !output.status.success(),
                "{command} {flag} accepted one value for two targets"
            );
            let stderr = flat(&output.stderr);
            assert!(
                stderr.contains(&format!(
                    "selected 2 targets (alpha, beta) but received 1 {noun}; provide one {flag} per target, in that order"
                )),
                "{command} {flag}: {stderr}"
            );
        }
    }
    assert!(!output_path.exists(), "a refused build writes nothing");
}

#[cfg(windows)]
#[test]
fn one_value_per_target_install_directory_is_aligned_per_profile() {
    let root = TempDir::new().unwrap();
    for name in ["alpha", "beta"] {
        fs::create_dir_all(root.path().join(format!("dist/{name}"))).unwrap();
        fs::write(
            root.path().join(format!("dist/{name}/app.bin")),
            name.as_bytes(),
        )
        .unwrap();
    }
    let manifest = root.path().join("zup.toml");
    fs::write(
        &manifest,
        format!(
            r#"schema = 1
[app]
id = "com.example.cli-install-matrix"
name = "Cli Install Matrix"
version = "1.0.0"
[build]
[build.targets.alpha]
target = "{HOST_TARGET}"
source = {{ directory = "dist/alpha" }}
[build.targets.beta]
target = "{OTHER_TARGET}"
source = {{ directory = "dist/beta" }}
[install]
scope = "user"
[install.directory]
user = "${{location.user_data}}/Shared"
"#
        ),
    )
    .unwrap();
    let result = zup()
        .args(["check", "--manifest"])
        .arg(&manifest)
        .arg("--install-directory")
        .arg(r"C:\Apps\Alpha")
        .arg("--install-directory")
        .arg(r"C:\Apps\Beta")
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let stdout = String::from_utf8_lossy(&result.stdout);
    // Two profiles, two values, in the order the profiles are selected.
    let resolved = stdout
        .lines()
        .filter(|line| line.trim_start().starts_with("Install"))
        .collect::<Vec<_>>();
    assert_eq!(resolved.len(), 2, "{stdout}");
    assert!(resolved[0].contains(r"C:\Apps\Alpha"), "{stdout}");
    assert!(resolved[1].contains(r"C:\Apps\Beta"), "{stdout}");
    assert!(
        !stdout.contains("${location.user_data}/Shared"),
        "the common install directory is replaced per profile: {stdout}"
    );
}

/// A build writes into an output directory the caller named but has not created,
/// and `--force` is the only way to write over an output that is already there.
#[cfg(all(windows, target_arch = "x86_64"))]
#[test]
fn a_build_creates_its_output_directory_and_force_is_the_only_overwrite() {
    let root = TempDir::new().unwrap();
    fs::create_dir_all(root.path().join("dist")).unwrap();
    fs::write(root.path().join("dist/app.bin"), b"payload").unwrap();
    let manifest = root.path().join("zup.toml");
    fs::write(
        &manifest,
        format!(
            r#"schema = 1
[app]
id = "com.example.output-lifecycle"
name = "Output Lifecycle"
version = "1.0.0"
[build]
[build.targets.default]
target = "{HOST_TARGET}"
source = {{ directory = "dist" }}
frontend = "{frontend}"
[install]
scope = "user"
[install.directory]
user = "${{location.user_data}}/OutputLifecycle"
[[files]]
source = "**/*"
destination = "${{install}}"
"#,
            frontend = FRONTEND,
        ),
    )
    .unwrap();
    let output = root.path().join("nested/output/Setup.exe");
    assert!(
        !output.parent().unwrap().exists(),
        "the parent does not exist yet"
    );
    let build = |extra: &[&str]| {
        zup()
            .args(["build", "--manifest"])
            .arg(&manifest)
            .arg("--output")
            .arg(&output)
            .args(extra)
            .output()
            .unwrap()
    };

    let first = build(&[]);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(
        EmbeddedBundle::open(&output).is_ok(),
        "the installer is written into the directory the build created"
    );

    let refused = build(&[]);
    assert!(!refused.status.success(), "a second build must be refused");
    let stderr = flat(&refused.stderr);
    assert!(stderr.contains("already exists"), "{stderr}");
    assert!(
        stderr.contains("--force"),
        "the refusal names the escape hatch: {stderr}"
    );

    let forced = build(&["--force"]);
    assert!(
        forced.status.success(),
        "{}",
        String::from_utf8_lossy(&forced.stderr)
    );
    assert!(
        EmbeddedBundle::open(&output).is_ok(),
        "--force leaves a usable installer behind"
    );
}

/// An unsupported target is refused before the source tree is read at all.
#[test]
fn an_unsupported_target_is_refused_before_any_source_is_read() {
    let root = TempDir::new().unwrap();
    let manifest = root.path().join("zup.toml");
    // The declared source directory does not exist, so a filesystem-first build
    // would report a missing source instead of the backend boundary.
    fs::write(
        &manifest,
        format!(
            r#"schema = 1
[app]
id = "com.example.backend-boundary"
name = "Backend Boundary"
version = "1.0.0"
[build]
[build.targets.default]
target = "{UNSUPPORTED_TARGET}"
source = {{ directory = "dist/never-created" }}
[install]
scope = "user"
[install.directory]
user = "${{location.user_data}}/BackendBoundary"
"#
        ),
    )
    .unwrap();

    for command in ["build", "check"] {
        let output = zup()
            .args([command, "--manifest"])
            .arg(&manifest)
            .output()
            .unwrap();
        assert!(!output.status.success(), "{command} accepted the target");
        let stderr = flat(&output.stderr);
        assert!(
            stderr.contains("unsupported backend for target"),
            "{command} reported {stderr}"
        );
        for filesystem in ["dist/never-created", "does not exist", "source directory"] {
            assert!(
                !stderr.contains(filesystem),
                "{command} touched the filesystem before the backend boundary: {stderr}"
            );
        }
    }
    assert!(
        !root.path().join("dist").exists(),
        "no source directory is created for a refused target"
    );
}

/// A project with several profiles never plans an arbitrary one of them. The
/// per-target flags are ambiguous for the same reason, which is why
/// `repeatable_resolution_flags_must_line_up_with_the_selection` exists.
#[test]
fn a_multi_target_project_refuses_to_plan_without_an_explicit_selection() {
    let (_matrix, manifest) = write_matrix_project();
    let ambiguous = zup()
        .args(["plan", "--manifest"])
        .arg(&manifest)
        .arg("--format")
        .arg("json")
        .output()
        .unwrap();
    assert!(!ambiguous.status.success());
    // The refusal is still a protocol document, not a crash: a consumer gets a
    // parseable reason rather than an empty stdout.
    let refusal: serde_json::Value = serde_json::from_slice(&ambiguous.stdout).unwrap();
    assert_eq!(refusal["operation"], "plan");
    assert_eq!(refusal["status"], "failure");
    assert!(
        String::from_utf8_lossy(&ambiguous.stderr).contains("exactly one target"),
        "{}",
        String::from_utf8_lossy(&ambiguous.stderr)
    );

    let explicit = zup()
        .args(["plan", "--manifest"])
        .arg(&manifest)
        .args(["--target", "alpha", "--format", "json"])
        .output()
        .unwrap();
    assert!(
        explicit.status.success(),
        "{}",
        String::from_utf8_lossy(&explicit.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&explicit.stdout).unwrap();
    assert_eq!(value["targets"][0]["target"], HOST_TARGET);
}
