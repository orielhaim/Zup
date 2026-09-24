#![cfg(feature = "build")]

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use tempfile::TempDir;
use wit_component::{ComponentEncoder, StringEncoding, dummy_module, embed_component_metadata};
use wit_parser::{ManglingAndAbi, Resolve};
#[cfg(any(feature = "console", feature = "headless"))]
use zup_bundle::EmbeddedBundle;
#[cfg(feature = "headless")]
use zup_bundle::read_pe_frontend;
#[cfg(feature = "console")]
use zup_bundle::{PeSubsystem, read_pe_subsystem};
#[cfg(any(feature = "console", feature = "headless"))]
use zup_core::Frontend;
use zup_plugin_contract::HOST_TARGET;

const PLUGIN_WIT: &str = include_str!("../../../wit/zup-plugin.wit");

fn plugin_component() -> Vec<u8> {
    let mut resolve = Resolve::default();
    let package = resolve.push_str("zup-plugin.wit", PLUGIN_WIT).unwrap();
    let world = resolve.select_world(&[package], Some("plugin")).unwrap();
    let mut module = dummy_module(&resolve, world, ManglingAndAbi::Standard32);
    embed_component_metadata(&mut module, &resolve, world, StringEncoding::UTF8).unwrap();
    ComponentEncoder::default()
        .module(&module)
        .unwrap()
        .validate(true)
        .encode()
        .unwrap()
}

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

fn setup_runtime() -> PathBuf {
    #[cfg(feature = "gui")]
    {
        PathBuf::from(env!("CARGO_BIN_EXE_zup-setup"))
    }
    #[cfg(all(not(feature = "gui"), feature = "console"))]
    {
        PathBuf::from(env!("CARGO_BIN_EXE_zup-setup-console"))
    }
    #[cfg(all(not(feature = "gui"), not(feature = "console"), feature = "headless"))]
    {
        PathBuf::from(env!("CARGO_BIN_EXE_zup-setup-headless"))
    }
    #[cfg(not(any(feature = "gui", feature = "console", feature = "headless")))]
    {
        PathBuf::from(env!("CARGO_BIN_EXE_zup-setup"))
    }
}

fn write_pluginless_project(root: &Path) {
    fs::create_dir_all(root.join("dist")).unwrap();
    fs::write(root.join("dist/app.bin"), b"payload").unwrap();
    let manifest = r#"
schema = 1
[app]
id = "com.example.runtime-target"
name = "Runtime Target"
version = "1.0.0"
[source]
directory = "dist"
[install]
scope = "user"
[install.directory]
user = "${known.local_app_data}/RuntimeTarget"
[[files]]
source = "**/*"
destination = "${install}"
"#
    .replace(
        "schema = 1",
        &format!("schema = 1\nfrontend = \"{}\"", selected_frontend()),
    );
    fs::write(root.join("zup.toml"), manifest).unwrap();
}

fn run_pluginless_build(project: &Path, runtime: &Path, target: &str) -> Output {
    let output = project.join("Setup.exe");
    Command::new(env!("CARGO_BIN_EXE_zup"))
        .args(["build", "--manifest"])
        .arg(project.join("zup.toml"))
        .arg("--runtime")
        .arg(runtime)
        .arg("--target")
        .arg(target)
        .arg("--output")
        .arg(&output)
        .output()
        .unwrap()
}

#[cfg(any(feature = "console", feature = "headless"))]
fn run_pluginless_build_with_frontend(
    project: &Path,
    runtime: &Path,
    target: &str,
    frontend: &str,
) -> Output {
    let output = project.join("Setup.exe");
    Command::new(env!("CARGO_BIN_EXE_zup"))
        .args(["build", "--manifest"])
        .arg(project.join("zup.toml"))
        .arg("--runtime")
        .arg(runtime)
        .arg("--frontend")
        .arg(frontend)
        .arg("--target")
        .arg(target)
        .arg("--output")
        .arg(&output)
        .output()
        .unwrap()
}

#[cfg(all(windows, target_arch = "x86_64"))]
#[test]
fn build_accepts_matching_x64_runtime_target_without_plugins() {
    let project = TempDir::new().unwrap();
    write_pluginless_project(project.path());
    let result = run_pluginless_build(project.path(), &setup_runtime(), "x86_64-pc-windows-msvc");
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}

#[cfg(all(feature = "console", windows, target_arch = "x86_64"))]
#[test]
fn build_frontend_override_selects_the_console_runtime() {
    let project = TempDir::new().unwrap();
    write_pluginless_project(project.path());
    let runtime = Path::new(env!("CARGO_BIN_EXE_zup-setup-console"));
    let result = run_pluginless_build_with_frontend(
        project.path(),
        runtime,
        "x86_64-pc-windows-msvc",
        "console",
    );
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let output = project.path().join("Setup.exe");
    let bundle = EmbeddedBundle::open(&output).unwrap();
    assert_eq!(bundle.frontend(), Frontend::Console);
    assert_eq!(read_pe_subsystem(&output).unwrap(), PeSubsystem::Console);
}

#[cfg(all(feature = "headless", windows, target_arch = "x86_64"))]
#[test]
fn manifest_frontend_selects_the_headless_runtime() {
    let project = TempDir::new().unwrap();
    write_pluginless_project(project.path());
    let manifest_path = project.path().join("zup.toml");
    let source = fs::read_to_string(&manifest_path).unwrap();
    fs::write(
        &manifest_path,
        source.replacen(
            &format!("frontend = \"{}\"", selected_frontend()),
            "frontend = \"headless\"",
            1,
        ),
    )
    .unwrap();
    let runtime = Path::new(env!("CARGO_BIN_EXE_zup-setup-headless"));
    let result = run_pluginless_build(project.path(), runtime, "x86_64-pc-windows-msvc");
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let output = project.path().join("Setup.exe");
    let bundle = EmbeddedBundle::open(&output).unwrap();
    assert_eq!(bundle.frontend(), Frontend::Headless);
    assert_eq!(read_pe_frontend(&output).unwrap(), Frontend::Console);
}

#[cfg(all(feature = "headless", windows, target_arch = "x86_64"))]
#[test]
fn build_rejects_a_headless_template_for_console_selection() {
    let project = TempDir::new().unwrap();
    write_pluginless_project(project.path());
    let result = run_pluginless_build_with_frontend(
        project.path(),
        Path::new(env!("CARGO_BIN_EXE_zup-setup-headless")),
        "x86_64-pc-windows-msvc",
        "console",
    );
    assert!(!result.status.success());
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(stderr.contains("not the console template"), "{stderr}");
}

#[cfg(all(feature = "headless", windows, target_arch = "x86_64"))]
#[test]
fn headless_noninteractive_install_and_uninstall_emit_json() {
    let project = TempDir::new().unwrap();
    write_pluginless_project(project.path());
    let manifest_path = project.path().join("zup.toml");
    let source = fs::read_to_string(&manifest_path).unwrap();
    fs::write(
        &manifest_path,
        source.replace("com.example.runtime-target", "com.example.headless-runtime"),
    )
    .unwrap();
    let runtime = Path::new(env!("CARGO_BIN_EXE_zup-setup-headless"));
    let build = run_pluginless_build_with_frontend(
        project.path(),
        runtime,
        "x86_64-pc-windows-msvc",
        "headless",
    );
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8_lossy(&build.stderr)
    );
    let setup = project.path().join("Setup.exe");
    let state = project.path().join("state");
    let install = Command::new(&setup)
        .args(["install", "--yes", "--output", "json", "--state-root"])
        .arg(&state)
        .output()
        .unwrap();
    assert!(
        install.status.success(),
        "{}",
        String::from_utf8_lossy(&install.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&install.stdout).unwrap();
    assert_eq!(value["outcome"], "success");
    let uninstall = Command::new(&setup)
        .args(["uninstall", "--yes", "--output", "json", "--state-root"])
        .arg(&state)
        .output()
        .unwrap();
    assert!(
        uninstall.status.success(),
        "{}",
        String::from_utf8_lossy(&uninstall.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&uninstall.stdout).unwrap();
    assert_eq!(value["outcome"], "success");

    let install = Command::new(&setup)
        .args(["install", "--yes", "--output", "jsonl", "--state-root"])
        .arg(&state)
        .output()
        .unwrap();
    assert!(install.status.success());
    let lines = String::from_utf8(install.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(lines.first().unwrap()["type"], "started");
    assert_eq!(lines.first().unwrap()["protocol_version"], 1);
    assert_eq!(lines.last().unwrap()["type"], "completed");
    assert_eq!(lines.last().unwrap()["outcome"], "success");

    let uninstall = Command::new(&setup)
        .args(["uninstall", "--yes", "--output", "jsonl", "--state-root"])
        .arg(&state)
        .output()
        .unwrap();
    assert!(uninstall.status.success());
    let lines = String::from_utf8(uninstall.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(lines.first().unwrap()["type"], "started");
    assert_eq!(lines.first().unwrap()["protocol_version"], 1);
    assert_eq!(lines.last().unwrap()["type"], "completed");
    assert_eq!(lines.last().unwrap()["outcome"], "success");
}

#[cfg(all(feature = "console", windows, target_arch = "x86_64"))]
#[test]
fn console_redirected_install_does_not_prompt() {
    let project = TempDir::new().unwrap();
    write_pluginless_project(project.path());
    let manifest_path = project.path().join("zup.toml");
    let suffix = uuid::Uuid::now_v7().simple().to_string();
    let app_id = format!("com.example.console-{suffix}");
    let install_name = format!("ConsoleRuntime-{suffix}");
    let source = fs::read_to_string(&manifest_path).unwrap();
    fs::write(
        &manifest_path,
        source
            .replace("com.example.runtime-target", &app_id)
            .replace("RuntimeTarget", &install_name),
    )
    .unwrap();
    let runtime = Path::new(env!("CARGO_BIN_EXE_zup-setup-console"));
    let build = run_pluginless_build_with_frontend(
        project.path(),
        runtime,
        "x86_64-pc-windows-msvc",
        "console",
    );
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8_lossy(&build.stderr)
    );
    let setup = project.path().join("Setup.exe");
    let state = project.path().join("state");
    let install = Command::new(&setup)
        .args(["install", "--output", "json", "--state-root"])
        .arg(&state)
        .output()
        .unwrap();
    assert!(
        install.status.success(),
        "{}",
        String::from_utf8_lossy(&install.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&install.stdout).unwrap();
    assert_eq!(value["outcome"], "success");
    let cleanup = Command::new(&setup)
        .args(["uninstall", "--yes", "--state-root"])
        .arg(&state)
        .output()
        .unwrap();
    assert!(cleanup.status.success());
}

#[cfg(all(windows, target_arch = "x86_64"))]
#[test]
fn build_rejects_arm64_target_for_x64_runtime_without_plugins() {
    let project = TempDir::new().unwrap();
    write_pluginless_project(project.path());
    let result = run_pluginless_build(project.path(), &setup_runtime(), "aarch64-pc-windows-msvc");
    assert!(!result.status.success());
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(
        stderr.contains("does not match requested target"),
        "{stderr}"
    );
    assert!(!project.path().join("Setup.exe").exists());
}

#[test]
fn build_rejects_non_pe_runtime() {
    let project = TempDir::new().unwrap();
    write_pluginless_project(project.path());
    let runtime = project.path().join("runtime.exe");
    fs::write(&runtime, b"not a PE").unwrap();
    let result = run_pluginless_build(project.path(), &runtime, "x86_64-pc-windows-msvc");
    assert!(!result.status.success());
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(stderr.contains("runtime target"), "{stderr}");
    assert!(!project.path().join("Setup.exe").exists());
}

#[test]
fn build_embeds_a_verified_package_and_runs_outside_project_directory() {
    let project = TempDir::new().unwrap();
    let elsewhere = TempDir::new().unwrap();
    fs::create_dir_all(project.path().join("dist")).unwrap();
    fs::write(project.path().join("dist/app.exe"), b"payload bytes").unwrap();
    fs::write(
        project.path().join("zup.toml"),
        r#"
schema = 1
[app]
id = "com.example.portable"
name = "Portable App"
version = "1.2.3"
[source]
directory = "dist"
[install]
scope = "user"
[install.directory]
user = "${known.local_app_data}/PortableApp"
[[files]]
source = "**/*"
destination = "${install}"
"#,
    )
    .unwrap();
    let output = project.path().join("Portable-Setup.exe");
    let result = Command::new(env!("CARGO_BIN_EXE_zup"))
        .current_dir(elsewhere.path())
        .arg("build")
        .arg("--manifest")
        .arg(project.path().join("zup.toml"))
        .arg("--runtime")
        .arg(setup_runtime())
        .arg("--frontend")
        .arg(selected_frontend())
        .arg("--output")
        .arg(&output)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let package = zup_bundle::EmbeddedBundle::open(output).unwrap();
    assert_eq!(
        package.plan().installer.app.id.as_str(),
        "com.example.portable"
    );
    assert_eq!(package.plan().entries.len(), 1);
}

#[test]
fn build_compiles_and_embeds_declared_plugins_for_the_explicit_target() {
    let project = TempDir::new().unwrap();
    let elsewhere = TempDir::new().unwrap();
    fs::create_dir_all(project.path().join("dist")).unwrap();
    fs::create_dir_all(project.path().join("plugins")).unwrap();
    fs::write(project.path().join("dist/app.exe"), b"payload bytes").unwrap();
    fs::write(
        project.path().join("plugins/helper.wasm"),
        plugin_component(),
    )
    .unwrap();
    fs::write(
        project.path().join("zup.toml"),
        r#"
schema = 1
[app]
id = "com.example.plugin-cli"
name = "Plugin CLI"
version = "1.0.0"
[source]
directory = "dist"
[install]
scope = "user"
[install.directory]
user = "${known.local_app_data}/PluginCLI"
[[files]]
source = "**/*"
destination = "${install}"
[[plugins]]
id = "helper"
source = "plugins/helper.wasm"
"#,
    )
    .unwrap();
    let output = project.path().join("Plugin-Setup.exe");
    let result = Command::new(env!("CARGO_BIN_EXE_zup"))
        .current_dir(elsewhere.path())
        .args(["build", "--manifest"])
        .arg(project.path().join("zup.toml"))
        .arg("--runtime")
        .arg(setup_runtime())
        .arg("--frontend")
        .arg(selected_frontend())
        .arg("--target")
        .arg(HOST_TARGET)
        .arg("--output")
        .arg(&output)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let package = zup_bundle::EmbeddedBundle::open(&output).unwrap();
    let id = zup_core::PluginId::new("helper").unwrap();
    let metadata = package.plugin_artifact(&id).unwrap();
    assert_eq!(metadata.target, HOST_TARGET);
    assert!(!package.plugin_aot(&id).unwrap().is_empty());
    assert!(package.build_plan().unwrap().plugins.is_empty());
    assert_eq!(package.plan().entries.len(), 1);
}

#[test]
fn manifest_directory_mode_rejects_active_source_plugins_without_jit() {
    let project = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    fs::create_dir_all(project.path().join("dist")).unwrap();
    fs::create_dir_all(project.path().join("plugins")).unwrap();
    fs::write(project.path().join("dist/app.exe"), b"payload").unwrap();
    fs::write(project.path().join("plugins/helper.wasm"), b"not component").unwrap();
    fs::write(
        project.path().join("zup.toml"),
        r#"
schema = 1
[app]
id = "com.example.source-plugin"
name = "Source Plugin"
version = "1.0.0"
[source]
directory = "dist"
[install]
scope = "user"
[install.directory]
user = "${known.local_app_data}/SourcePlugin"
[[files]]
source = "**/*"
destination = "${install}"
[[plugins]]
id = "helper"
source = "plugins/helper.wasm"
"#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_zup"))
        .current_dir(project.path())
        .args(["install", "--manifest", "zup.toml", "--state-root"])
        .arg(state.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("active plugin `helper`"), "{stderr}");
    assert!(stderr.contains("source plugin JIT"), "{stderr}");
    assert!(stderr.contains("is disabled"), "{stderr}");
}

#[cfg(windows)]
#[test]
fn embedded_install_survives_source_deletion_and_supports_repair_modify_uninstall() {
    let project = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    fs::create_dir_all(project.path().join("dist")).unwrap();
    fs::write(project.path().join("dist/app.exe"), b"app payload").unwrap();
    fs::write(project.path().join("dist/readme.txt"), b"optional payload").unwrap();
    let app_id = format!("com.zup.e2e{}", uuid::Uuid::now_v7().simple());
    let install_name = format!("ZupE2E-{}", uuid::Uuid::now_v7().simple());
    let manifest = format!(
        r#"
schema = 1
[app]
id = "{app_id}"
name = "Zup E2E"
version = "1.0.0"
[source]
directory = "dist"
[install]
scope = "user"
[install.directory]
user = "${{known.local_app_data}}/Programs/{install_name}"
[[components]]
id = "core"
name = "Core"
required = true
[[components]]
id = "docs"
name = "Documentation"
default = true
[[files]]
source = "app.exe"
destination = "${{install}}"
component = "core"
[[files]]
source = "readme.txt"
destination = "${{install}}"
component = "docs"
"#
    );
    fs::write(project.path().join("zup.toml"), manifest).unwrap();
    let setup = project.path().join("Setup.exe");
    let build = Command::new(env!("CARGO_BIN_EXE_zup"))
        .current_dir(outside.path())
        .args(["build", "--manifest"])
        .arg(project.path().join("zup.toml"))
        .arg("--runtime")
        .arg(setup_runtime())
        .arg("--frontend")
        .arg(selected_frontend())
        .arg("--output")
        .arg(&setup)
        .output()
        .unwrap();
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8_lossy(&build.stderr)
    );

    let run = |exe: &std::path::Path, command: &str, extra: &[&str]| {
        let output = Command::new(exe)
            .current_dir(outside.path())
            .arg(command)
            .arg("--scope")
            .arg("user")
            .arg("--state-root")
            .arg(state.path())
            .args(extra)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{command}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    run(&setup, "install", &[]);
    let maintenance = state
        .path()
        .join("maintenance")
        .join(&app_id)
        .join("user")
        .join("1.0.0")
        .join("Setup.exe");
    assert!(maintenance.is_file());
    let registration =
        zup_windows::inspect_uninstall_registration(zup_core::SelectedScope::User, &app_id)
            .unwrap()
            .unwrap();
    let values = &registration.values;
    assert_eq!(
        values["DisplayName"],
        zup_exec::UninstallEntryValue::String("Zup E2E".into())
    );
    assert_eq!(
        values["DisplayVersion"],
        zup_exec::UninstallEntryValue::String("1.0.0".into())
    );
    assert!(
        matches!(values["EstimatedSize"], zup_exec::UninstallEntryValue::Dword(size) if size > 0)
    );
    let zup_exec::UninstallEntryValue::String(uninstall) = &values["UninstallString"] else {
        panic!("UninstallString is not REG_SZ")
    };
    let zup_exec::UninstallEntryValue::String(modify) = &values["ModifyPath"] else {
        panic!("ModifyPath is not REG_SZ")
    };
    assert!(uninstall.contains(&maintenance.to_string_lossy().to_string()));
    assert!(modify.contains(&maintenance.to_string_lossy().to_string()));
    fs::remove_file(&setup).unwrap();
    fs::remove_dir_all(project.path()).unwrap();

    let local = std::env::var_os("LOCALAPPDATA").unwrap();
    let install = std::path::PathBuf::from(local)
        .join("Programs")
        .join(install_name);
    fs::remove_file(install.join("app.exe")).unwrap();
    run(&maintenance, "repair", &[]);
    assert_eq!(fs::read(install.join("app.exe")).unwrap(), b"app payload");
    run(&maintenance, "modify", &["--disable", "docs"]);
    assert!(!install.join("readme.txt").exists());
    run(&maintenance, "uninstall", &[]);
    assert!(!install.join("app.exe").exists());
    assert!(
        zup_windows::inspect_uninstall_registration(zup_core::SelectedScope::User, &app_id)
            .unwrap()
            .is_none()
    );
    assert!(!maintenance.exists());
    assert!(!state.path().join("maintenance").join(&app_id).exists());
    assert!(
        zup_windows::InstallLedgerStore::new(state.path())
            .load(
                &zup_core::AppId::new(&app_id).unwrap(),
                zup_core::SelectedScope::User
            )
            .unwrap()
            .is_none()
    );
    assert!(!state.path().join("transactions").exists());
    assert!(!state.path().join("work").exists());
    assert!(!state.path().join("installations").exists());
    let lock_key = zup_windows::InstallationLock::lock_key(&app_id, "user");
    assert!(!state.path().join(format!("{lock_key}.lock")).exists());
}

#[cfg(windows)]
#[test]
fn uninstall_preserves_a_drifted_apps_and_features_entry() {
    let project = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    fs::create_dir_all(project.path().join("dist")).unwrap();
    fs::write(project.path().join("dist/app.exe"), b"app payload").unwrap();
    let app_id = format!("com.zup.arpdrift{}", uuid::Uuid::now_v7().simple());
    let install_name = format!("ZupArpDrift-{}", uuid::Uuid::now_v7().simple());
    fs::write(
        project.path().join("zup.toml"),
        format!(
            "schema = 1\n[app]\nid = \"{app_id}\"\nname = \"ARP Drift\"\nversion = \"1.0.0\"\n[source]\ndirectory = \"dist\"\n[install]\nscope = \"user\"\n[install.directory]\nuser = \"${{known.local_app_data}}/Programs/{install_name}\"\n[[files]]\nsource = \"**/*\"\ndestination = \"${{install}}\"\n"
        ),
    )
    .unwrap();
    let setup = project.path().join("Setup.exe");
    let build = Command::new(env!("CARGO_BIN_EXE_zup"))
        .current_dir(outside.path())
        .args(["build", "--manifest"])
        .arg(project.path().join("zup.toml"))
        .arg("--runtime")
        .arg(setup_runtime())
        .arg("--frontend")
        .arg(selected_frontend())
        .arg("--output")
        .arg(&setup)
        .output()
        .unwrap();
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8_lossy(&build.stderr)
    );
    let run = |exe: &std::path::Path, command: &str| {
        Command::new(exe)
            .current_dir(outside.path())
            .args([command, "--scope", "user", "--state-root"])
            .arg(state.path())
            .output()
            .unwrap()
    };
    assert!(run(&setup, "install").status.success());
    let maintenance = state
        .path()
        .join("maintenance")
        .join(&app_id)
        .join("user/1.0.0/Setup.exe");
    let key_path = format!("Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\{app_id}");
    windows_registry::CURRENT_USER
        .options()
        .read()
        .write()
        .open(&key_path)
        .unwrap()
        .set_string("DisplayName", "Changed outside zup")
        .unwrap();
    let uninstall = run(&maintenance, "uninstall");
    assert!(
        uninstall.status.success(),
        "{}",
        String::from_utf8_lossy(&uninstall.stderr)
    );
    let remaining =
        zup_windows::inspect_uninstall_registration(zup_core::SelectedScope::User, &app_id)
            .unwrap()
            .unwrap();
    assert_eq!(
        remaining.values["DisplayName"],
        zup_exec::UninstallEntryValue::String("Changed outside zup".into())
    );
    assert!(!maintenance.exists());
    assert!(
        zup_windows::InstallLedgerStore::new(state.path())
            .load(
                &zup_core::AppId::new(&app_id).unwrap(),
                zup_core::SelectedScope::User
            )
            .unwrap()
            .is_none()
    );
    windows_registry::CURRENT_USER
        .remove_tree(&key_path)
        .unwrap();
}

#[cfg(windows)]
#[test]
fn failed_embedded_upgrade_keeps_previous_committed_maintenance_copy() {
    let project = TempDir::new().unwrap();
    let next = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let app_id = format!("com.zup.upgrade{}", uuid::Uuid::now_v7().simple());
    let install_name = format!("ZupUpgrade-{}", uuid::Uuid::now_v7().simple());
    let write_project = |root: &std::path::Path, version: &str, with_blocker: bool| {
        fs::create_dir_all(root.join("dist")).unwrap();
        fs::write(root.join("dist/app.exe"), format!("app {version}")).unwrap();
        if with_blocker {
            fs::write(root.join("dist/block.dat"), b"blocked").unwrap();
        }
        let extra = if with_blocker {
            "[[files]]\nsource = \"block.dat\"\ndestination = \"${install}\"\n"
        } else {
            ""
        };
        fs::write(
            root.join("zup.toml"),
            format!(
                "schema = 1\n[app]\nid = \"{app_id}\"\nname = \"Upgrade E2E\"\nversion = \"{version}\"\n[source]\ndirectory = \"dist\"\n[install]\nscope = \"user\"\n[install.directory]\nuser = \"${{known.local_app_data}}/Programs/{install_name}\"\n[[files]]\nsource = \"app.exe\"\ndestination = \"${{install}}\"\n{extra}"
            ),
        )
        .unwrap();
    };
    let build = |root: &std::path::Path, output: &std::path::Path| {
        let result = Command::new(env!("CARGO_BIN_EXE_zup"))
            .current_dir(outside.path())
            .args(["build", "--manifest"])
            .arg(root.join("zup.toml"))
            .arg("--runtime")
            .arg(setup_runtime())
            .arg("--frontend")
            .arg(selected_frontend())
            .arg("--output")
            .arg(output)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    };
    let run = |exe: &std::path::Path, command: &str| {
        Command::new(exe)
            .current_dir(outside.path())
            .arg(command)
            .args(["--scope", "user", "--state-root"])
            .arg(state.path())
            .output()
            .unwrap()
    };

    write_project(project.path(), "1.0.0", false);
    let setup_v1 = project.path().join("Setup.exe");
    build(project.path(), &setup_v1);
    let install = std::path::PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap())
        .join("Programs")
        .join(&install_name);
    assert!(run(&setup_v1, "install").status.success());
    let maintenance_v1 = state
        .path()
        .join("maintenance")
        .join(&app_id)
        .join("user/1.0.0/Setup.exe");
    assert!(maintenance_v1.is_file());
    fs::remove_file(&setup_v1).unwrap();

    write_project(next.path(), "2.0.0", true);
    let setup_v2 = next.path().join("Setup.exe");
    build(next.path(), &setup_v2);
    fs::create_dir(install.join("block.dat")).unwrap();
    let failed = run(&setup_v2, "upgrade");
    assert!(!failed.status.success());
    let ledger = zup_windows::InstallLedgerStore::new(state.path())
        .load(
            &zup_core::AppId::new(&app_id).unwrap(),
            zup_core::SelectedScope::User,
        )
        .unwrap()
        .unwrap();
    assert_eq!(ledger.version.to_string(), "1.0.0");
    assert!(maintenance_v1.is_file());
    let registration =
        zup_windows::inspect_uninstall_registration(zup_core::SelectedScope::User, &app_id)
            .unwrap()
            .unwrap();
    assert_eq!(
        registration.values["DisplayVersion"],
        zup_exec::UninstallEntryValue::String("1.0.0".into())
    );
    fs::remove_dir(install.join("block.dat")).unwrap();
    assert!(run(&maintenance_v1, "repair").status.success());
    assert!(run(&setup_v2, "upgrade").status.success());
    let maintenance_v2 = state
        .path()
        .join("maintenance")
        .join(&app_id)
        .join("user/2.0.0/Setup.exe");
    assert!(maintenance_v2.is_file());
    let upgraded = zup_windows::InstallLedgerStore::new(state.path())
        .load(
            &zup_core::AppId::new(&app_id).unwrap(),
            zup_core::SelectedScope::User,
        )
        .unwrap()
        .unwrap();
    assert_eq!(upgraded.version.to_string(), "2.0.0");
    assert!(!maintenance_v1.exists());
    let registration =
        zup_windows::inspect_uninstall_registration(zup_core::SelectedScope::User, &app_id)
            .unwrap()
            .unwrap();
    assert_eq!(
        registration.values["DisplayVersion"],
        zup_exec::UninstallEntryValue::String("2.0.0".into())
    );
}

#[cfg(all(feature = "build", windows))]
#[test]
fn plain_cli_without_bundle_uses_manifest_dispatch() {
    let project = TempDir::new().unwrap();
    write_pluginless_project(project.path());
    fs::write(
        project.path().join("zup.toml"),
        r#"
schema = 1
[app]
id = "com.example.plain-cli"
name = "Plain CLI"
version = "1.0.0"
[source]
directory = "dist"
[install]
scope = "user"
[install.directory]
user = "${known.local_app_data}/PlainCLI"
[[plugins]]
id = "Helper"
source = "plugins/one.wasm"
[[plugins]]
id = "helper"
source = "plugins/two.wasm"
"#,
    )
    .unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_zup"))
        .current_dir(project.path())
        .args(["install", "--manifest", "zup.toml"])
        .output()
        .unwrap();
    assert!(!result.status.success());
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(stderr.contains("duplicate plugin"), "{stderr}");
    assert!(!stderr.contains("installer package"), "{stderr}");
}

#[cfg(all(feature = "build", windows))]
#[test]
fn corrupt_embedded_setup_does_not_fall_back_to_local_manifest() {
    let project = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    write_pluginless_project(project.path());
    let setup = project.path().join("Setup.exe");
    let manifest_path = project.path().join("zup.toml");
    let source = fs::read_to_string(&manifest_path).unwrap();
    let parsed = zup_manifest::parse(&source).unwrap();
    let installer = zup_manifest::parse_and_compile(&source).unwrap();
    let build = zup_build::materialize(&manifest_path, &parsed, installer).unwrap();
    let mut package = zup_bundle::BundleWriter::encode(&build, &[]).unwrap();
    let metadata_len = u64::from_le_bytes(package[20..28].try_into().unwrap()) as usize;
    package[60 + metadata_len + 10] ^= 0x40;
    let package_path = project.path().join("corrupt.zupbundle");
    fs::write(&package_path, package).unwrap();
    zup_bundle::embed_bundle_file(&setup_runtime(), &setup, &package_path).unwrap();
    assert!(zup_bundle::EmbeddedBundle::open(&setup).is_err());

    fs::write(
        project.path().join("zup.toml"),
        r#"
schema = 1
[app]
id = "com.example.local-manifest"
name = "Local Manifest"
version = "1.0.0"
[source]
directory = "dist"
[install]
scope = "user"
[install.directory]
user = "${known.local_app_data}/LocalManifest"
"#,
    )
    .unwrap();
    let result = Command::new(&setup)
        .current_dir(project.path())
        .args(["install", "--manifest", "zup.toml", "--state-root"])
        .arg(state.path())
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(!state.path().join("transactions").exists());
}

#[cfg(all(feature = "build", windows, target_arch = "x86_64"))]
mod generated_file_lifecycle {
    use super::setup_runtime;

    use std::{
        fs,
        path::{Path, PathBuf},
        process::{Command, Output},
        thread,
        time::Duration,
    };

    use base64::Engine as _;
    use semver::Version;
    use tempfile::TempDir;
    use zup_build::{BuildPlan, ResolvedPlugin};
    use zup_bundle::{BundleWriter, CompiledPluginArtifact, PluginArtifact};
    use zup_core::{
        App, AppId, Component, ComponentId, Frontend, Install, InstallDirectory, InstallScope,
        Installer, NonEmptyString, PluginBinding, PluginId, RelativePath, ResourceKey,
        SelectedScope, Sha256Digest, Template, hash_reader,
    };
    use zup_exec::OwnedResource;
    use zup_plugin_contract::{
        AOT_FORMAT_VERSION, HOST_TARGET, PLUGIN_API_VERSION, PluginEngine, WASMTIME_VERSION,
        wit_package_digest,
    };
    use zup_windows::{InstallLedgerStore, PAYLOAD_OVERLAY_DIRECTORY};

    const PLUGIN_ID: &str = "configure";
    const SOURCE_BYTES: &[u8] = b"configure component source is build-time only";
    const CONFIGURE_AOT: &str =
        include_str!("../../zup-plugin-runtime/tests/fixtures/configure-plugin.aot.b64");

    fn decoded_fixture() -> Vec<u8> {
        base64::engine::general_purpose::STANDARD
            .decode(CONFIGURE_AOT.trim())
            .unwrap()
    }

    fn make_plan(
        version: &str,
        app_id: &AppId,
        install_name: &str,
        source_path: &Path,
    ) -> BuildPlan {
        let (source_size, source_sha256) = hash_reader(SOURCE_BYTES).unwrap();
        BuildPlan {
            installer: Installer {
                ui: None,
                frontend: Frontend::Gui,
                app: App {
                    id: app_id.clone(),
                    name: NonEmptyString::new("Configure Lifecycle").unwrap(),
                    version: Version::parse(version).unwrap(),
                    publisher: None,
                    main: None,
                    description: None,
                },
                updates: None,
                install: Install {
                    scope: InstallScope::User,
                    directory: InstallDirectory {
                        user: Some(
                            Template::parse(&format!("${{known.local_app_data}}/{install_name}"))
                                .unwrap(),
                        ),
                        machine: None,
                    },
                    allow_directory_override: false,
                },
                components: vec![Component {
                    id: ComponentId::new("core").unwrap(),
                    name: NonEmptyString::new("Core").unwrap(),
                    description: None,
                    required: true,
                    default: true,
                    requires: Vec::new(),
                }],
                plugins: vec![PluginBinding {
                    id: PluginId::new(PLUGIN_ID).unwrap(),
                    component: None,
                    when: None,
                }],
                files: Vec::new(),
                shortcuts: Vec::new(),
                path: Vec::new(),
                services: Vec::new(),
                protocols: Vec::new(),
                file_types: Vec::new(),
            },
            plugins: vec![ResolvedPlugin {
                id: PluginId::new(PLUGIN_ID).unwrap(),
                source: source_path.to_path_buf(),
                source_relative: RelativePath::new("configure.component.wasm").unwrap(),
                size: source_size,
                sha256: source_sha256,
            }],
            files: Vec::new(),
            total_size: 0,
        }
    }

    fn artifact() -> CompiledPluginArtifact {
        let bytes = decoded_fixture();
        let engine = PluginEngine::new(HOST_TARGET).unwrap();
        engine.verify_precompiled(&bytes).unwrap();
        let (source_size, source_sha256) = hash_reader(SOURCE_BYTES).unwrap();
        let (aot_size, aot_sha256) = hash_reader(bytes.as_slice()).unwrap();
        CompiledPluginArtifact::new(
            PluginArtifact {
                plugin_id: PluginId::new(PLUGIN_ID).unwrap(),
                source_size,
                source_sha256,
                target: HOST_TARGET.to_owned(),
                wasmtime_version: WASMTIME_VERSION.to_owned(),
                aot_format_version: AOT_FORMAT_VERSION,
                plugin_api_version: PLUGIN_API_VERSION.to_owned(),
                wit_digest: Sha256Digest::from_bytes(wit_package_digest()),
                engine_fingerprint: Sha256Digest::from_bytes(*engine.fingerprint().as_bytes()),
                aot_size,
                aot_sha256,
                blob: aot_sha256,
            },
            bytes,
        )
        .unwrap()
    }

    fn write_setup(
        root: &Path,
        name: &str,
        plan: &BuildPlan,
        artifact: &CompiledPluginArtifact,
    ) -> PathBuf {
        let package = BundleWriter::encode(plan, std::slice::from_ref(artifact)).unwrap();
        assert_eq!(u32::from_le_bytes(package[8..12].try_into().unwrap()), 3);
        let package_path = root.join(format!("{name}.zupbundle"));
        fs::write(&package_path, package).unwrap();
        let output = root.join(format!("{name}.exe"));
        zup_bundle::embed_bundle_file(&setup_runtime(), &output, &package_path).unwrap();
        let bundle = zup_bundle::EmbeddedBundle::open(&output).unwrap();
        let plugin_id = PluginId::new(PLUGIN_ID).unwrap();
        let metadata = bundle.plugin_artifact(&plugin_id).unwrap();
        assert_eq!(metadata.target, HOST_TARGET);
        assert_eq!(metadata.wasmtime_version, WASMTIME_VERSION);
        assert_eq!(bundle.plan().plugins.len(), 1);
        assert!(bundle.build_plan().unwrap().plugins.is_empty());
        output
    }

    fn invoke(exe: &Path, cwd: &Path, state: &Path, command: &str, extra: &[&str]) -> Output {
        Command::new(exe)
            .current_dir(cwd)
            .arg(command)
            .args(["--scope", "user", "--state-root"])
            .arg(state)
            .args(extra)
            .output()
            .unwrap()
    }

    fn run(exe: &Path, cwd: &Path, state: &Path, command: &str, extra: &[&str]) -> Output {
        let output = invoke(exe, cwd, state, command, extra);
        assert!(
            output.status.success(),
            "{command}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn wait_for_uninstall(state: &Path, app_id: &AppId, generated: &Path) {
        for _ in 0..100 {
            let ledger_gone = InstallLedgerStore::new(state)
                .load(app_id, SelectedScope::User)
                .ok()
                .flatten()
                .is_none();
            if ledger_gone && !generated.exists() {
                return;
            }
            thread::sleep(Duration::from_millis(100));
        }
        panic!("uninstall did not finish");
    }

    #[test]
    fn generated_file_survives_the_full_embedded_lifecycle_without_source_dependencies() {
        let root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        let source_root = TempDir::new().unwrap();
        let source_path = source_root.path().join("configure.component.wasm");
        fs::write(&source_path, SOURCE_BYTES).unwrap();

        let token = uuid::Uuid::now_v7().simple().to_string();
        let app_id_text = format!("com.zup.plugin-lifecycle-{token}");
        let install_name = format!("ZupPluginLifecycle-{token}");
        let app_id = AppId::new(&app_id_text).unwrap();
        let plan_v1 = make_plan("1.0.0", &app_id, &install_name, &source_path);
        let plan_v2 = make_plan("1.1.0", &app_id, &install_name, &source_path);
        let artifact = artifact();
        let setup_v1 = write_setup(root.path(), "Setup-v1", &plan_v1, &artifact);
        let setup_v2 = write_setup(root.path(), "Setup-v2", &plan_v2, &artifact);

        fs::remove_file(&source_path).unwrap();
        assert!(!source_path.exists());
        assert!(
            zup_bundle::EmbeddedBundle::open(&setup_v1)
                .unwrap()
                .build_plan()
                .unwrap()
                .plugins
                .is_empty()
        );
        let local_app_data = PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap());
        let install = local_app_data.join(&install_name);
        let generated = install.join("plugin-config.txt");
        let expected = format!(
            "app id: {app_id_text}\ninstall directory: ${{known.local_app_data}}/{install_name}\nselected components: core\n"
        );
        let maintenance_v1 = state
            .path()
            .join("maintenance")
            .join(&app_id_text)
            .join("user/1.0.0/Setup.exe");
        let maintenance_v2 = state
            .path()
            .join("maintenance")
            .join(&app_id_text)
            .join("user/1.1.0/Setup.exe");

        run(&setup_v1, outside.path(), state.path(), "install", &[]);
        assert_eq!(fs::read(&generated).unwrap(), expected.as_bytes());
        assert!(maintenance_v1.is_file());
        assert!(!state.path().join(PAYLOAD_OVERLAY_DIRECTORY).exists());

        let ledger = InstallLedgerStore::new(state.path())
            .load(&app_id, SelectedScope::User)
            .unwrap()
            .unwrap();
        assert_eq!(ledger.version.to_string(), "1.0.0");
        let generated_key = ResourceKey::File {
            destination: generated.to_string_lossy().into_owned(),
        };
        let generated_owned = ledger.resources.get(&generated_key).unwrap();
        let OwnedResource::File {
            source_relative,
            sha256,
            size,
            ..
        } = generated_owned
        else {
            panic!("generated file is not owned as a file");
        };
        assert!(zup_windows::is_plugin_payload_path(source_relative));
        assert_eq!(*sha256, hash_reader(expected.as_bytes()).unwrap().1);
        assert_eq!(*size, expected.len() as u64);
        assert_eq!(
            ledger
                .resources
                .values()
                .filter(|resource| {
                    matches!(
                        resource,
                        OwnedResource::File { source_relative, .. }
                            if zup_windows::is_plugin_payload_path(source_relative)
                    )
                })
                .count(),
            1
        );

        fs::remove_file(&setup_v1).unwrap();
        run(&setup_v2, outside.path(), state.path(), "upgrade", &[]);
        assert_eq!(fs::read(&generated).unwrap(), expected.as_bytes());
        assert!(maintenance_v2.is_file());
        assert!(!maintenance_v1.exists());
        assert!(!state.path().join(PAYLOAD_OVERLAY_DIRECTORY).exists());
        let upgraded = InstallLedgerStore::new(state.path())
            .load(&app_id, SelectedScope::User)
            .unwrap()
            .unwrap();
        assert_eq!(upgraded.version.to_string(), "1.1.0");
        fs::remove_file(&setup_v2).unwrap();

        fs::remove_file(&generated).unwrap();
        run(&maintenance_v2, outside.path(), state.path(), "repair", &[]);
        assert_eq!(fs::read(&generated).unwrap(), expected.as_bytes());
        assert!(!state.path().join(PAYLOAD_OVERLAY_DIRECTORY).exists());

        fs::write(&generated, b"corrupt").unwrap();
        run(
            &maintenance_v2,
            outside.path(),
            state.path(),
            "repair",
            &["--force-files"],
        );
        assert_eq!(fs::read(&generated).unwrap(), expected.as_bytes());
        assert!(!state.path().join(PAYLOAD_OVERLAY_DIRECTORY).exists());

        run(
            &maintenance_v2,
            outside.path(),
            state.path(),
            "uninstall",
            &[],
        );
        wait_for_uninstall(state.path(), &app_id, &generated);
        assert!(!generated.exists());
        assert!(!maintenance_v2.exists());
        assert!(!state.path().join("maintenance").join(&app_id_text).exists());
        assert!(!state.path().join(PAYLOAD_OVERLAY_DIRECTORY).exists());
        assert!(!state.path().join("transactions").exists());
        assert!(!state.path().join("work").exists());
        assert!(!state.path().join("installations").exists());
        let lock_key = zup_windows::InstallationLock::lock_key(&app_id_text, "user");
        assert!(!state.path().join(format!("{lock_key}.lock")).exists());
    }
}
