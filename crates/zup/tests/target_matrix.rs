#![cfg(feature = "build")]

use std::{fs, path::PathBuf, process::Command};

use tempfile::TempDir;
#[cfg(windows)]
use zup_core::Frontend;
#[cfg(windows)]
use zup_windows::EmbeddedBundle;

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

/// The installer frontend this test binary was built with.
fn selected_frontend() -> &'static str {
    #[cfg(feature = "gui")]
    {
        "gui"
    }
    #[cfg(all(not(feature = "gui"), feature = "console"))]
    {
        "console"
    }
    #[cfg(all(not(feature = "gui"), not(feature = "console"), feature = "headless"))]
    {
        "headless"
    }
    #[cfg(not(any(feature = "gui", feature = "console", feature = "headless")))]
    {
        "headless"
    }
}

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
    let repeated = zup()
        .args(["check", "--manifest"])
        .arg(&manifest)
        .args(["--target", "alpha", "--target", HOST_TARGET])
        .output()
        .unwrap();
    assert!(
        repeated.status.success(),
        "{}",
        String::from_utf8_lossy(&repeated.stderr)
    );
    let text = String::from_utf8_lossy(&repeated.stdout);
    assert!(text.contains("alpha"), "{text}");

    let raw = zup()
        .args(["check", "--manifest"])
        .arg(&manifest)
        .args(["--target", OTHER_TARGET])
        .output()
        .unwrap();
    assert!(
        raw.status.success(),
        "{}",
        String::from_utf8_lossy(&raw.stderr)
    );
    assert!(String::from_utf8_lossy(&raw.stdout).contains("beta"));

    let all = zup()
        .args(["check", "--manifest"])
        .arg(&manifest)
        .output()
        .unwrap();
    assert!(
        all.status.success(),
        "{}",
        String::from_utf8_lossy(&all.stderr)
    );
    let text = String::from_utf8_lossy(&all.stdout);
    assert!(text.contains("alpha") && text.contains("beta"), "{text}");
    assert_eq!(
        fs::read(root.path().join("dist/alpha/app.bin")).unwrap(),
        b"alpha"
    );
}

#[test]
fn multi_target_runtime_and_output_cardinality_fail_before_writes() {
    let (root, manifest) = write_matrix_project();
    let runtime = PathBuf::from(env!("CARGO_BIN_EXE_zup-setup"));
    let first_output = root.path().join("first.exe");

    let missing_runtime = zup()
        .args(["build", "--manifest"])
        .arg(&manifest)
        .arg("--output")
        .arg(&first_output)
        .output()
        .unwrap();
    assert!(!missing_runtime.status.success());
    assert!(String::from_utf8_lossy(&missing_runtime.stderr).contains("multiple targets"));
    assert!(!first_output.exists());

    let second_runtime = root.path().join("runtime-copy.exe");
    fs::copy(&runtime, &second_runtime).unwrap();
    let mismatched_output = zup()
        .args(["build", "--manifest"])
        .arg(&manifest)
        .arg("--runtime")
        .arg(&runtime)
        .arg("--runtime")
        .arg(&second_runtime)
        .arg("--output")
        .arg(&first_output)
        .output()
        .unwrap();
    assert!(!mismatched_output.status.success());
    assert!(String::from_utf8_lossy(&mismatched_output.stderr).contains("outputs"));
    assert!(!first_output.exists());
}

#[cfg(windows)]
#[test]
fn frontend_override_is_resolved_before_compilation() {
    let root = TempDir::new().unwrap();
    fs::create_dir_all(root.path().join("dist")).unwrap();
    fs::write(root.path().join("dist/app.bin"), b"payload").unwrap();
    let manifest = root.path().join("zup.toml");
    fs::write(
        &manifest,
        format!(
            r#"schema = 1
[app]
id = "com.example.frontend-override"
name = "Frontend Override"
version = "1.0.0"
[build]
[build.targets.default]
target = "{HOST_TARGET}"
source = {{ directory = "dist" }}
frontend = "gui"
[install]
scope = "user"
[install.directory]
user = "${{location.user_data}}/FrontendOverride"
"#
        ),
    )
    .unwrap();
    let output = root.path().join("Setup.exe");
    let result = zup()
        .args(["build", "--manifest"])
        .arg(&manifest)
        .arg("--runtime")
        .arg(PathBuf::from(env!("CARGO_BIN_EXE_zup-setup-console")))
        .args(["--frontend", "console", "--output"])
        .arg(&output)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let bundle = EmbeddedBundle::open(&output).unwrap();
    assert_eq!(bundle.frontend(), Frontend::Console);
    assert_eq!(bundle.plan().installer.frontend, Frontend::Console);
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
#[test]
fn repeatable_resolution_flags_must_line_up_with_the_selection() {
    let (_root, manifest) = write_matrix_project();
    for command in ["build", "check", "doctor"] {
        for (flag, value, noun) in [
            ("--source", "dist/alpha", "sources"),
            ("--install-directory", "C:/Acme", "install directories"),
        ] {
            let output = zup()
                .args([command, "--manifest"])
                .arg(&manifest)
                .arg(flag)
                .arg(value)
                .output()
                .unwrap();
            assert!(
                !output.status.success(),
                "{command} {flag} accepted one value for two targets"
            );
            let stderr = flat(&output.stderr);
            assert!(
                stderr.contains(&format!(
                    "selected 2 targets but received 1 {noun}; provide one {flag} per target"
                )),
                "{command} {flag}: {stderr}"
            );
        }
    }

    // One value per target is accepted by the command that only resolves.
    let aligned = zup()
        .args(["check", "--manifest"])
        .arg(&manifest)
        .arg("--source")
        .arg("dist/alpha")
        .arg("--source")
        .arg("dist/beta")
        .output()
        .unwrap();
    #[cfg(windows)]
    {
        assert!(
            aligned.status.success(),
            "{}",
            String::from_utf8_lossy(&aligned.stderr)
        );
        let stdout = String::from_utf8_lossy(&aligned.stdout);
        assert!(
            stdout.contains("dist/alpha") && stdout.contains("dist/beta"),
            "{stdout}"
        );
    }
    #[cfg(not(windows))]
    drop(aligned);
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
    assert_eq!(
        resolved,
        vec![
            "  Install     user · C:\\Apps\\Alpha",
            "  Install     user · C:\\Apps\\Beta",
        ],
        "{stdout}"
    );
    assert!(
        !stdout.contains("${location.user_data}/Shared"),
        "the common install directory is replaced per profile: {stdout}"
    );
}

/// `--force` is the only way to write over an existing output.
#[cfg(all(windows, target_arch = "x86_64"))]
#[test]
fn force_overwrites_an_existing_output_and_its_absence_refuses() {
    let root = TempDir::new().unwrap();
    fs::create_dir_all(root.path().join("dist")).unwrap();
    fs::write(root.path().join("dist/app.bin"), b"payload").unwrap();
    let manifest = root.path().join("zup.toml");
    fs::write(
        &manifest,
        format!(
            r#"schema = 1
[app]
id = "com.example.force-overwrite"
name = "Force Overwrite"
version = "1.0.0"
[build]
[build.targets.default]
target = "{HOST_TARGET}"
source = {{ directory = "dist" }}
frontend = "{frontend}"
[install]
scope = "user"
[install.directory]
user = "${{location.user_data}}/ForceOverwrite"
[[files]]
source = "**/*"
destination = "${{install}}"
"#,
            frontend = selected_frontend(),
        ),
    )
    .unwrap();
    let output = root.path().join("Setup.exe");
    let build = |extra: &[&str]| {
        zup()
            .args(["build", "--manifest"])
            .arg(&manifest)
            .arg("--runtime")
            .arg(PathBuf::from(env!("CARGO_BIN_EXE_zup-setup")))
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
    assert!(output.is_file(), "the first build wrote its output");

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

/// A build writes into an output directory the caller named but has not created.
#[test]
fn a_build_creates_the_output_directory_it_names() {
    let root = TempDir::new().unwrap();
    fs::create_dir_all(root.path().join("dist")).unwrap();
    fs::write(root.path().join("dist/app.bin"), b"payload").unwrap();
    let manifest = root.path().join("zup.toml");
    fs::write(
        &manifest,
        format!(
            r#"schema = 1
[app]
id = "com.example.output-parent"
name = "Output Parent"
version = "1.0.0"
[build]
[build.targets.default]
target = "{HOST_TARGET}"
source = {{ directory = "dist" }}
frontend = "{frontend}"
[install]
scope = "user"
[install.directory]
user = "${{location.user_data}}/OutputParent"
[[files]]
source = "**/*"
destination = "${{install}}"
"#,
            frontend = selected_frontend(),
        ),
    )
    .unwrap();
    let output = root.path().join("nested/output/Setup.exe");
    assert!(
        !output.parent().unwrap().exists(),
        "the parent does not exist yet"
    );

    let built = zup()
        .args(["build", "--manifest"])
        .arg(&manifest)
        .arg("--runtime")
        .arg(PathBuf::from(env!("CARGO_BIN_EXE_zup-setup")))
        .arg("--output")
        .arg(&output)
        .output()
        .unwrap();
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    assert!(
        EmbeddedBundle::open(&output).is_ok(),
        "the installer is written into the directory the build created"
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

#[test]
fn init_and_single_target_commands_use_schema_1_and_require_explicit_selection() {
    let root = TempDir::new().unwrap();
    let initialized = root.path().join("init.toml");
    let init = zup()
        .args(["init", "--manifest"])
        .arg(&initialized)
        .args([
            "--non-interactive",
            "--name",
            "Acme",
            "--app-id",
            "com.example.init-matrix",
            "--version",
            "1.0.0",
            "--source",
            "dist",
            "--scope",
            "user",
        ])
        .output()
        .unwrap();
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );
    let source = fs::read_to_string(&initialized).unwrap();
    assert!(source.contains("schema = 1"));
    assert!(source.contains("[build.targets.default]"));
    assert!(source.contains(&format!("target = \"{HOST_TARGET}\"")));
    assert!(source.contains("source = { directory = \"dist\" }"));
    let checked = zup()
        .args(["check", "--manifest"])
        .arg(&initialized)
        .output()
        .unwrap();
    assert!(
        checked.status.success(),
        "{}",
        String::from_utf8_lossy(&checked.stderr)
    );

    let (_matrix, manifest) = write_matrix_project();
    let ambiguous = zup()
        .args(["plan", "--manifest"])
        .arg(&manifest)
        .arg("--json")
        .output()
        .unwrap();
    assert!(!ambiguous.status.success());
    assert!(String::from_utf8_lossy(&ambiguous.stderr).contains("exactly one target"));

    let explicit = zup()
        .args(["plan", "--manifest"])
        .arg(&manifest)
        .args(["--target", "alpha", "--json"])
        .output()
        .unwrap();
    assert!(
        explicit.status.success(),
        "{}",
        String::from_utf8_lossy(&explicit.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&explicit.stdout).unwrap();
    assert_eq!(value["target"]["target"], HOST_TARGET);

    let source_ambiguous = zup()
        .args(["install", "--manifest"])
        .arg(&manifest)
        .args(["--non-interactive", "--yes", "--output", "json"])
        .output()
        .unwrap();
    assert!(!source_ambiguous.status.success());
    assert!(String::from_utf8_lossy(&source_ambiguous.stderr).contains("exactly one target"));
}

#[test]
fn source_manifest_explicit_target_skips_unselected_source() {
    let (root, manifest) = write_matrix_project();
    fs::remove_dir_all(root.path().join("dist/beta")).unwrap();
    let state = root.path().join("state");
    let result = zup()
        .args(["repair", "--manifest"])
        .arg(&manifest)
        .args(["--target", "alpha", "--state-root"])
        .arg(&state)
        .args(["--non-interactive", "--yes"])
        .output()
        .unwrap();
    assert!(!result.status.success());
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(stderr.contains("installation not found"), "{stderr}");
    assert!(!stderr.contains("source directory"), "{stderr}");
}
