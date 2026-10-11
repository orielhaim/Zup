use std::fs;
use std::process::Command;

use tempfile::TempDir;

fn zup() -> Command {
    Command::new(env!("CARGO_BIN_EXE_zup"))
}

/// The target fixtures check on this host, and a frontend its backend ships.
///
/// The diagnostics under test are host-independent; the target carrying them
/// has to lower far enough to reach the compile errors they describe.
#[cfg(windows)]
const TARGET: &str = "x86_64-pc-windows-msvc";
#[cfg(not(windows))]
const TARGET: &str = "x86_64-unknown-linux-gnu";

#[cfg(windows)]
const FRONTEND: &str = "gui";
#[cfg(not(windows))]
const FRONTEND: &str = "console";

#[test]
fn init_creates_a_small_editor_ready_manifest() {
    let root = TempDir::new().unwrap();
    let manifest = root.path().join("zup.toml");
    let output = zup()
        .args([
            "init",
            "--manifest",
            manifest.to_str().unwrap(),
            "--non-interactive",
            "--name",
            "Acme",
            "--app-id",
            "com.example.acme",
            "--version",
            "1.4.0",
            "--source",
            "dist",
            "--scope",
            "user",
            "--main",
            "Acme.exe",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let source = fs::read_to_string(&manifest).unwrap();
    assert!(source.starts_with("#:schema https://zup.dev/schema/zup.toml.json"));
    assert!(source.contains("allow_directory_override = true"));
    assert!(root.path().join("dist").is_dir());
    let checked = zup()
        .args(["check", "--manifest", manifest.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        checked.status.success(),
        "{}",
        String::from_utf8_lossy(&checked.stderr)
    );
}

#[test]
#[cfg(windows)]
fn plan_reports_the_resolved_installation_as_machine_output() {
    let root = TempDir::new().unwrap();
    let manifest = root.path().join("zup.toml");
    fs::write(
        &manifest,
        r#"#:schema https://zup.dev/schema/zup.toml.json
schema = 1
[app]
id = "com.example.plan"
name = "Plan"
version = "1.0.0"
[build]

[build.targets.default]
target = "x86_64-pc-windows-msvc"
source = { directory = "dist" }
[install]
scope = "user"
[install.directory]
user = "${location.user_data}/Plan"
"#,
    )
    .unwrap();
    fs::create_dir_all(root.path().join("dist")).unwrap();
    let plan = zup()
        .args([
            "plan",
            "--manifest",
            manifest.to_str().unwrap(),
            "--state-root",
            root.path().join("state").to_str().unwrap(),
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(
        plan.status.success(),
        "{}",
        String::from_utf8_lossy(&plan.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&plan.stdout).unwrap();
    assert_eq!(value["operation"], "plan");
    assert_eq!(value["application"]["name"], "Plan");
    // The template is resolved, not echoed: a consumer reads this to install.
    assert!(
        !value["details"]["install_directory"]
            .as_str()
            .unwrap()
            .contains("${"),
        "an unresolved template reached the machine output: {value}"
    );
    assert_eq!(value["details"]["scope"], "user");
}

#[test]
fn fmt_preserves_author_comments_and_check_detects_clean_manifests() {
    let root = TempDir::new().unwrap();
    let manifest = root.path().join("zup.toml");
    fs::write(
        &manifest,
        "# keep this comment\nschema=1\n[app]\nid=\"com.example.fmt\"\nname=\"Fmt\"\nversion=\"1.0.0\"\n[build]\n\n[build.targets.default]\ntarget=\"x86_64-pc-windows-msvc\"\nsource={directory=\"dist\"}\n[install]\nscope=\"user\"\n[install.directory]\nuser=\"${location.user_data}/Fmt\"\n",
    )
    .unwrap();
    fs::create_dir_all(root.path().join("dist")).unwrap();
    let formatted = zup()
        .args(["fmt", "--manifest", manifest.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        formatted.status.success(),
        "{}",
        String::from_utf8_lossy(&formatted.stderr)
    );
    let source = fs::read_to_string(&manifest).unwrap();
    assert!(source.contains("# keep this comment"));
    assert!(source.contains("schema"));
    let checked = zup()
        .args(["fmt", "--manifest", manifest.to_str().unwrap(), "--check"])
        .output()
        .unwrap();
    assert!(checked.status.success());
}

#[test]
fn check_renders_source_aware_semantic_diagnostics() {
    let root = TempDir::new().unwrap();
    let manifest = root.path().join("zup.toml");
    fs::write(
        &manifest,
        format!(
            r#"schema = 1
frontend = "{FRONTEND}"
[app]
id = "com.example.bad"
name = "Bad"
version = "1.0.0"
[build]

[build.targets.default]
target = "{TARGET}"
source = {{ directory = "dist" }}
[install]
scope = "user"
[install.directory]
user = "${{location.user_data}}/Bad"
[[files]]
source = "x"
destination = "${{install}}/x"
component = "missing"
"#,
        ),
    )
    .unwrap();
    let output = zup()
        .args(["check", "--manifest", manifest.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unknown component `missing`"));
    assert!(stderr.contains("component = \"missing\""));
}
