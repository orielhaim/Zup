use std::fs;
use std::process::Command;

use tempfile::TempDir;

fn zup() -> Command {
    Command::new(env!("CARGO_BIN_EXE_zup"))
}

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
fn schema_and_plan_have_stable_machine_output() {
    let schema = zup().args(["schema"]).output().unwrap();
    assert!(schema.status.success());
    let value: serde_json::Value = serde_json::from_slice(&schema.stdout).unwrap();
    assert_eq!(value["$id"], "https://zup.dev/schema/zup.toml.json");
    assert!(value["properties"]["install"].is_object());

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
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        plan.status.success(),
        "{}",
        String::from_utf8_lossy(&plan.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&plan.stdout).unwrap();
    assert_eq!(value["preview"]["application"], "Plan");
    assert!(value["transaction"]["install_directory"]["path"].is_string());
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
        r#"schema = 1
[app]
id = "com.example.bad"
name = "Bad"
version = "1.0.0"
[build]

[build.targets.default]
target = "x86_64-pc-windows-msvc"
source = { directory = "dist" }
[install]
scope = "user"
[install.directory]
user = "${location.user_data}/Bad"
[[files]]
source = "x"
destination = "${install}/x"
component = "missing"
"#,
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

#[test]
fn completions_include_authoring_commands() {
    let output = zup().args(["completions", "powershell"]).output().unwrap();
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("init"));
    assert!(text.contains("check"));
    assert!(text.contains("plan"));
}
