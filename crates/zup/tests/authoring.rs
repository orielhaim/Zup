use std::fs;
use std::path::Path;
use std::process::Command;

use tempfile::TempDir;

#[path = "support/toolchain_fixture.rs"]
mod toolchain_fixture;

fn zup() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_zup"));
    command.env_remove("ZUP_TOOLCHAIN");
    command
}

fn zup_with_toolchain(root: &Path) -> Command {
    toolchain_fixture::zup(&toolchain_fixture::host_toolchain(root))
}

const TARGET: &str = zup_plugin_contract::HOST_TARGET;

#[cfg(windows)]
const FRONTEND: &str = "gui";
#[cfg(not(windows))]
const FRONTEND: &str = "console";

#[test]
fn plan_reports_the_resolved_installation_as_machine_output() {
    let root = TempDir::new().unwrap();
    let manifest = root.path().join("zup.toml");
    fs::write(
        &manifest,
        r#"#:schema https://zup.dev/schema/zup.toml.json
schema = 1
frontend = "headless"
[app]
id = "com.example.plan"
name = "Plan"
version = "1.0.0"
[build]

[build.targets.default]
target = "windows-x64"
source = { directory = "dist" }
[install]
scope = "user"
[install.directory]
user = "${location.user_data}/Plan"
[[plugins]]
id = "configure"
source = "configure.wasm"
"#
        .replace("windows-x64", TARGET),
    )
    .unwrap();
    fs::create_dir_all(root.path().join("dist")).unwrap();
    fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/plugins/configure/dist/configure.wasm"),
        root.path().join("configure.wasm"),
    )
    .unwrap();
    let plan = zup()
        .args([
            "plan",
            "--manifest",
            manifest.to_str().unwrap(),
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
    assert!(
        !value["details"]["install_directory"]
            .as_str()
            .unwrap()
            .contains("${"),
        "an unresolved template reached the machine output: {value}"
    );
    assert_eq!(value["details"]["scope"], "user");
    let resources = value["details"]["resources"].as_array().unwrap();
    assert!(resources.iter().any(|resource| {
        resource["kind"] == "file"
            && resource["generated"] == true
            && resource["destination"]
                .as_str()
                .unwrap()
                .ends_with("configure.txt")
    }));
}

#[test]
fn fmt_preserves_author_comments_and_check_detects_clean_manifests() {
    let root = TempDir::new().unwrap();
    let manifest = root.path().join("zup.toml");
    fs::write(
        &manifest,
        "# keep this comment\nschema=1\n[app]\nid=\"com.example.fmt\"\nname=\"Fmt\"\nversion=\"1.0.0\"\n[build]\n\n[build.targets.default]\ntarget=\"windows-x64\"\nsource={directory=\"dist\"}\n[install]\nscope=\"user\"\n[install.directory]\nuser=\"${location.user_data}/Fmt\"\n",
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
    let output = zup_with_toolchain(root.path())
        .args(["check", "--manifest", manifest.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unknown component `missing`"));
    assert!(stderr.contains("component = \"missing\""));
}

#[test]
fn an_operation_requires_a_concrete_scope() {
    let output = zup().args(["plan", "--scope", "either"]).output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("either"));
}

mod initialization {
    use super::*;
    use std::io::{Read, Write};
    use std::sync::{Arc, Mutex, mpsc};
    use std::time::{Duration, Instant};

    use portable_pty::{CommandBuilder, PtySize, native_pty_system};
    use rstest::rstest;
    use zup_core::{Frontend, InstallScope, Target, TargetOverrides};

    fn manifest(root: &Path) -> zup_manifest::Manifest {
        let source = fs::read_to_string(root.join("zup.toml")).unwrap();
        let manifest = zup_manifest::parse(&source).unwrap();
        let overrides = TargetOverrides::default();
        for target in zup_manifest::select_targets(&manifest, &[], &overrides).unwrap() {
            zup_manifest::compile(&manifest, &target, &overrides).unwrap();
        }
        manifest
    }

    fn init(root: &Path, args: &[&str]) -> std::process::Output {
        zup()
            .current_dir(root)
            .arg("init")
            .args(args)
            .output()
            .unwrap()
    }

    fn succeeds(output: std::process::Output) {
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[rstest]
    #[case("-y", false)]
    #[case("--yes", true)]
    fn unattended_initialization_needs_no_identity_flags(
        #[case] flag: &str,
        #[case] terminal: bool,
    ) {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("Acme Desktop");
        fs::create_dir(&root).unwrap();
        if terminal {
            wizard(&root, &[flag], &[]);
        } else {
            succeeds(init(&root, &[flag]));
        }
        let manifest = manifest(&root);
        assert_eq!(manifest.app.name.as_str(), "Acme Desktop");
        assert_eq!(manifest.app.id.as_str(), "com.example.acme-desktop");
        assert_eq!(manifest.app.version.to_string(), "0.1.0");
        assert!(manifest.app.main.is_none());
        assert_eq!(manifest.install.scope, InstallScope::User);
        assert_eq!(manifest.frontend, Frontend::Gui);
        let profiles = manifest.build.targets;
        assert_eq!(profiles.len(), 1);
        assert!(profiles.contains_key(Target::host().unwrap().as_str()));
        assert!(root.join("dist").is_dir());
    }

    #[test]
    fn explicit_overrides_produce_a_valid_multi_target_project() {
        let root = TempDir::new().unwrap();
        fs::create_dir_all(root.path().join("payload/bin")).unwrap();
        fs::write(
            root.path().join("payload/bin/launch"),
            "an existing entry point",
        )
        .unwrap();
        succeeds(init(
            root.path(),
            &[
                "-y",
                "--name",
                "Acme \"Desktop\"",
                "--app-id",
                "com.acme.desktop",
                "--version",
                "2.3.4",
                "--target",
                "windows-x64,linux-arm64",
                "--target",
                "windows-x64",
                "--source",
                "payload",
                "--main",
                "bin/launch",
                "--scope",
                "either",
                "--frontend",
                "console",
            ],
        ));
        let manifest = manifest(root.path());
        assert_eq!(manifest.app.name.as_str(), "Acme \"Desktop\"");
        assert_eq!(manifest.app.id.as_str(), "com.acme.desktop");
        assert_eq!(manifest.app.version.to_string(), "2.3.4");
        assert_eq!(manifest.app.main.unwrap().as_literal(), Some("bin/launch"));
        assert_eq!(manifest.install.scope, InstallScope::Either);
        assert_eq!(manifest.frontend, Frontend::Console);
        assert_eq!(
            manifest
                .build
                .targets
                .keys()
                .map(|key| key.as_str())
                .collect::<Vec<_>>(),
            ["linux-arm64", "windows-x64"]
        );
        for profile in manifest.build.targets.values() {
            assert_eq!(profile.source.directory, Path::new("payload"));
        }
    }

    #[test]
    fn a_main_override_must_name_an_existing_file() {
        let root = TempDir::new().unwrap();
        let output = init(root.path(), &["-y", "--main", "missing.exe"]);
        assert!(!output.status.success());
        assert!(!root.path().join("zup.toml").exists());
        assert!(!root.path().join("dist").exists());
    }

    #[test]
    fn overwriting_requires_force_and_valid_answers() {
        let root = TempDir::new().unwrap();
        succeeds(init(root.path(), &["-y", "--name", "Original"]));
        let original = fs::read(root.path().join("zup.toml")).unwrap();
        assert!(
            !init(root.path(), &["-y", "--name", "Changed"])
                .status
                .success()
        );
        assert!(
            !init(root.path(), &["-y", "--force", "--version", "invalid"])
                .status
                .success()
        );
        assert_eq!(fs::read(root.path().join("zup.toml")).unwrap(), original);
        succeeds(init(root.path(), &["-y", "--force", "--name", "Changed"]));
        assert_eq!(manifest(root.path()).app.name.as_str(), "Changed");
    }

    #[test]
    fn discovery_uses_real_binaries_and_leaves_ambiguous_entry_points_unset() {
        let root = TempDir::new().unwrap();
        let source = root.path().join("out");
        fs::create_dir(&source).unwrap();
        let name = if cfg!(windows) { "Acme.EXE" } else { "Acme" };
        fs::copy(env!("CARGO_BIN_EXE_zup"), source.join(name)).unwrap();
        succeeds(init(root.path(), &["-y"]));
        let first = manifest(root.path());
        assert_eq!(first.app.main.unwrap().as_literal(), Some(name));
        assert_eq!(
            first
                .build
                .targets
                .values()
                .next()
                .unwrap()
                .source
                .directory,
            Path::new("out")
        );
        fs::copy(
            source.join(name),
            source.join(if cfg!(windows) {
                "helper.exe"
            } else {
                "helper"
            }),
        )
        .unwrap();
        succeeds(init(root.path(), &["-y", "--force"]));
        assert!(manifest(root.path()).app.main.is_none());
        succeeds(init(
            root.path(),
            &["-y", "--force", "--target", "windows-x64,linux-x64"],
        ));
        assert!(manifest(root.path()).app.main.is_none());
    }

    #[test]
    fn unattended_mode_is_explicit_and_the_removed_flag_is_rejected() {
        let root = TempDir::new().unwrap();
        for args in [&[][..], &["--yes", "--non-interactive"][..]] {
            assert!(!init(root.path(), args).status.success());
            assert!(!root.path().join("zup.toml").exists());
        }
    }

    #[rstest]
    #[case(false)]
    #[case(true)]
    fn the_wizard_uses_the_same_defaults_and_accepts_multiple_targets(#[case] edit: bool) {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("Acme Desktop");
        fs::create_dir(&root).unwrap();
        succeeds(init(&root, &["--yes"]));
        let defaults = manifest(&root);
        fs::remove_file(root.join("zup.toml")).unwrap();
        let targets = if edit {
            "\x1b[D\x1b[H \x1b[B\x1b[B\x1b[B\x1b[B \r"
        } else {
            "\r"
        };
        wizard(
            &root,
            &[],
            &[
                ("Application name", if edit { " Pro\r" } else { "\r" }),
                ("Application ID", "\r"),
                ("Space to select", targets),
                ("Source directory", "\r"),
            ],
        );
        let actual = manifest(&root);
        if edit {
            assert_eq!(actual.app.name.as_str(), "Acme Desktop Pro");
            assert_eq!(actual.app.id.as_str(), "com.example.acme-desktop-pro");
            assert_eq!(
                actual
                    .build
                    .targets
                    .keys()
                    .map(|key| key.as_str())
                    .collect::<Vec<_>>(),
                ["linux-arm64", "windows-x64"]
            );
        } else {
            assert_eq!(actual, defaults);
        }
    }

    fn wizard(root: &Path, args: &[&str], exchanges: &[(&str, &str)]) {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 30,
                cols: 100,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_zup"));
        command.cwd(root);
        command.arg("init");
        command.args(args);
        command.env("TERM", "xterm-256color");
        command.env("NO_COLOR", "1");
        let mut reader = pair.master.try_clone_reader().unwrap();
        let writer = Arc::new(Mutex::new(pair.master.take_writer().unwrap()));
        let reply = writer.clone();
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let mut buffer = [0; 2048];
            let mut pending = Vec::new();
            while let Ok(count) = reader.read(&mut buffer) {
                if count == 0 {
                    break;
                }
                pending.extend_from_slice(&buffer[..count]);
                // ConPTY requests its terminal emulator's cursor position.
                if pending.windows(4).any(|bytes| bytes == b"\x1b[6n") {
                    let mut reply = reply.lock().unwrap();
                    if reply
                        .write_all(b"\x1b[1;1R")
                        .and_then(|_| reply.flush())
                        .is_err()
                    {
                        break;
                    }
                }
                if pending.len() > 3 {
                    pending.drain(..pending.len() - 3);
                }
                if sender.send(buffer[..count].to_vec()).is_err() {
                    break;
                }
            }
        });
        let mut child = pair.slave.spawn_command(command).unwrap();
        drop(pair.slave);
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut transcript = String::new();
        let mut next = 0;
        loop {
            if let Ok(bytes) = receiver.recv_timeout(Duration::from_millis(20)) {
                transcript.push_str(&String::from_utf8_lossy(&bytes));
                if let Some((question, answer)) = exchanges.get(next)
                    && transcript.contains(question)
                {
                    let mut writer = writer.lock().unwrap();
                    writer.write_all(answer.as_bytes()).unwrap();
                    writer.flush().unwrap();
                    transcript.clear();
                    next += 1;
                }
            }
            if let Some(status) = child.try_wait().unwrap() {
                assert_eq!(next, exchanges.len(), "missing question: {transcript}");
                assert!(status.success(), "{transcript}");
                return;
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                let _ = child.wait();
                panic!("wizard stalled after question {next}: {transcript}");
            }
        }
    }
}
