use std::{fs, process::Command};

use tempfile::TempDir;

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
        .arg(env!("CARGO_BIN_EXE_zup-setup"))
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
        .arg(env!("CARGO_BIN_EXE_zup-setup"))
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
        .arg(env!("CARGO_BIN_EXE_zup-setup"))
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
            .arg(env!("CARGO_BIN_EXE_zup-setup"))
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
