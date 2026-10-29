use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use serde_json::{Value, json};
use zup_core::{Frontend, Target, TargetBuildPlan};

struct Fixture {
    root: tempfile::TempDir,
    setup: PathBuf,
    #[cfg(all(target_os = "linux", feature = "online"))]
    plan: TargetBuildPlan,
}

impl Fixture {
    fn new(frontend: Frontend) -> Self {
        let root = tempfile::tempdir().unwrap();
        let target = Target::host().unwrap();
        let directory = root
            .path()
            .join("application")
            .to_string_lossy()
            .replace('\\', "/");
        let mut plan: TargetBuildPlan = serde_json::from_value(json!({
            "installer": {
                "app": {"id": format!("com.zup.terminal-{}", uuid::Uuid::now_v7().simple()), "name": "Terminal installer", "version": "1.0.0"},
                "target": target, "frontend": frontend, "updates": null,
                "install": {"scope": "either", "directory": {"user": directory, "machine": directory}},
                "components": [{"id": "core", "name": "Application", "required": true}, {"id": "docs", "name": "Documentation", "default": false}],
                "plugins": [], "files": [], "launchers": [], "path": [], "services": [], "protocols": [], "file_associations": []
            },
            "files": [], "plugins": [], "prerequisites": [], "total_size": 0, "prerequisite_size": 0
        })).unwrap();
        for component in ["core", "docs"] {
            let source = root.path().join(format!("{component}.txt"));
            std::fs::write(&source, component).unwrap();
            plan.files.push(zup_core::ResolvedFile {
                source,
                source_relative: zup_core::RelativePath::new(format!("{component}.txt")).unwrap(),
                destination: zup_core::Template::parse(&format!("${{install}}/{component}.txt"))
                    .unwrap(),
                size: component.len() as u64,
                sha256: zup_core::hash_bytes(component.as_bytes()),
                component: Some(zup_core::ComponentId::new(component).unwrap()),
                condition: None,
                executable: false,
            });
        }
        plan.total_size = plan.files.iter().map(|file| file.size).sum();
        #[cfg(all(target_os = "linux", feature = "online"))]
        let preset = linux_updates::preset(frontend);
        #[cfg(not(all(target_os = "linux", feature = "online")))]
        let preset: Option<Vec<u8>> = None;
        if preset.is_some() {
            plan.installer.preset = Some(zup_core::PresetRuntime {
                name: zup_core::NonEmptyString::new("test-preset").unwrap(),
                version: "1.0.0".parse().unwrap(),
                protocol: zup_preset_protocol::PRESET_PROTOCOL_VERSION,
                required_capabilities: Vec::new(),
                settings: json!({}),
                assets: Vec::new(),
            });
        }
        let package = zup_bundle::BundleWriter::encode(&plan, &[]).unwrap();
        let runtime = match frontend {
            Frontend::Console => env!("CARGO_BIN_EXE_zup-setup-console"),
            Frontend::Headless => env!("CARGO_BIN_EXE_zup-setup-headless"),
            #[cfg(all(target_os = "linux", feature = "online", feature = "gui"))]
            Frontend::Gui => env!("CARGO_BIN_EXE_zup-setup-gui"),
            #[cfg(not(all(target_os = "linux", feature = "online", feature = "gui")))]
            _ => unreachable!(),
        };
        let setup = root
            .path()
            .join(format!("setup{}", target.executable_suffix()));
        zup_compose::compose(
            &target,
            Path::new(runtime),
            &setup,
            &package,
            preset.as_deref(),
        )
        .unwrap();
        Self {
            root,
            setup,
            #[cfg(all(target_os = "linux", feature = "online"))]
            plan,
        }
    }
    fn command(&self) -> Command {
        let mut command = Command::new(&self.setup);
        command
            .arg("--state-root")
            .arg(self.root.path().join("zup"))
            .stdin(Stdio::null());
        command
    }
    fn terminal(&self, args: &[&str], exchanges: &[(&str, &str)], width: u16, term: &str) -> u32 {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: width,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut command = CommandBuilder::new(&self.setup);
        command.args(args);
        command.arg("--state-root");
        command.arg(self.root.path().join("zup"));
        command.env("TERM", term);
        command.env("NO_COLOR", "1");
        let mut reader = pair.master.try_clone_reader().unwrap();
        let writer = std::sync::Arc::new(std::sync::Mutex::new(pair.master.take_writer().unwrap()));
        let reply = std::sync::Arc::clone(&writer);
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut buffer = [0; 2048];
            let mut pending = Vec::new();
            while let Ok(count) = reader.read(&mut buffer) {
                if count == 0 {
                    break;
                }
                pending.extend_from_slice(&buffer[..count]);
                // ConPTY asks its terminal emulator for the inherited cursor position.
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
        let deadline = Instant::now() + Duration::from_secs(90);
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
                assert_eq!(next, exchanges.len(), "missing prompt: {transcript}");
                return status.exit_code();
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                let _ = child.wait();
                panic!("terminal stalled after exchange {next}: {transcript}");
            }
        }
    }
}

#[test]
fn automation_is_one_document_or_a_framed_stream_and_never_prompts() {
    for frontend in [Frontend::Headless, Frontend::Console] {
        let fixture = Fixture::new(frontend);
        let missing = fixture
            .command()
            .args(["install", "--output", "json"])
            .output()
            .unwrap();
        assert_eq!(missing.status.code(), Some(3));
        let failure: Value = serde_json::from_slice(&missing.stdout).unwrap();
        assert_ne!(failure["outcome"], "success");
        assert!(!fixture.root.path().join("application").exists());
        for (verb, format) in [
            ("install", "json"),
            ("modify", "jsonl"),
            ("repair", "json"),
            ("uninstall", "jsonl"),
        ] {
            let result = fixture
                .command()
                .args([verb, "--scope", "user", "--output", format])
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            let documents = String::from_utf8(result.stdout)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str::<Value>(line).unwrap())
                .collect::<Vec<_>>();
            assert_eq!(
                documents[0]["protocol_version"],
                zup_presentation::INSTALLER_PROTOCOL_VERSION
            );
            if format == "json" {
                assert_eq!(documents.len(), 1);
            } else {
                assert_eq!(documents[0]["type"], "started");
                assert_eq!(documents.last().unwrap()["type"], "completed");
                assert_eq!(
                    documents
                        .iter()
                        .filter(|event| event["type"] == "completed" || event["type"] == "failed")
                        .count(),
                    1
                );
            }
            assert_eq!(documents.last().unwrap()["outcome"], "success");
        }
    }
}

#[test]
fn real_terminal_selects_components_confirms_and_cancels_without_mutation() {
    let fixture = Fixture::new(Frontend::Console);
    assert_eq!(
        fixture.terminal(&["install"], &[("Install for", "q\r")], 36, "dumb"),
        2
    );
    assert!(!fixture.root.path().join("application").exists());
    assert_eq!(
        fixture.terminal(
            &["install"],
            &[
                ("Install for", "\r"),
                ("Components", " \r"),
                ("Proceed with these changes?", "y")
            ],
            80,
            "xterm-256color"
        ),
        0
    );
    assert!(fixture.root.path().join("application/docs.txt").exists());
    assert_eq!(
        fixture.terminal(
            &["--scope", "user"],
            &[
                ("Maintenance", "\r"),
                ("Components", "\r"),
                ("Proceed with these changes?", "y")
            ],
            80,
            "xterm-256color"
        ),
        0
    );

    assert_eq!(
        fixture.terminal(
            &["uninstall", "--scope", "user"],
            &[("Proceed with these changes?", "n")],
            80,
            "xterm-256color"
        ),
        2
    );
    assert!(fixture.root.path().join("application/core.txt").exists());
    assert_eq!(
        fixture.terminal(
            &["uninstall", "--scope", "user", "--plain"],
            &[("Proceed with these changes?", "y\r")],
            36,
            "dumb"
        ),
        0
    );
    assert!(!fixture.root.path().join("application/core.txt").exists());
}

#[cfg(all(target_os = "linux", feature = "online"))]
#[path = "support/linux_updates.rs"]
mod linux_updates;
