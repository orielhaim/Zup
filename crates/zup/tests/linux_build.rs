//! The public Linux build workflow, end to end through the real `zup` binary.
//!
//! Every test here runs the actual `zup` executable against a fixture project
//! exactly as an application author would: `check`, then `build`, then the
//! produced installer, then the installed application. The deep
//! upgrade/repair/recovery semantics live below this layer in `zup-linux`;
//! what is proven here is that the public path reaches that lifecycle with a
//! genuine installer.
//!
//! The environment is isolated: every path the installer resolves lands under
//! temporary directories, and the real user profile is never touched.

#![cfg(target_os = "linux")]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;
use zup_core::{AppId, SelectedScope};

const LINUX_TARGET: &str = "x86_64-unknown-linux-gnu";

const MANIFEST: &str = r#"
schema = 1

[app]
id = "com.example.tool"
name = "Tool"
version = "1.0.0"

[build]

[build.targets.linux]
target = "x86_64-unknown-linux-gnu"
frontend = "console"
source = { directory = "dist" }

[install]
scope = "user"

[install.directory]
user = "${location.programs}/tool"

[[files]]
source = "tool"
destination = "${install}"
executable = true

[[files]]
source = "keep.dat"
destination = "${install}"
"#;

const TOOL: &str = "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo \"tool 1.0.0\"; exit 0; fi\necho \"tool: unknown command $1\" >&2\nexit 1\n";

/// A fixture project as an application author would write it.
struct Project {
    root: TempDir,
}

impl Project {
    fn new() -> Self {
        Self::with_manifest(MANIFEST)
    }

    fn with_manifest(manifest: &str) -> Self {
        let root = TempDir::new().expect("a project directory");
        let dist = root.path().join("dist");
        fs::create_dir_all(&dist).expect("a source directory");
        fs::write(dist.join("tool"), TOOL).expect("the tool source");
        fs::write(dist.join("keep.dat"), b"keep-v1").expect("the data source");
        fs::write(root.path().join("zup.toml"), manifest).expect("a manifest");
        Self { root }
    }

    fn manifest(&self) -> PathBuf {
        self.root.path().join("zup.toml")
    }

    fn zup(&self, command: &str, extra: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_zup"))
            .arg(command)
            .arg("--manifest")
            .arg(self.manifest())
            .args(extra)
            .output()
            .expect("zup runs")
    }
}

/// An isolated Linux user for the installer child processes: temporary home
/// and XDG directories, passed explicitly so nothing leaks in from the
/// parent's environment.
struct IsolatedUser {
    _root: TempDir,
    home: PathBuf,
    run: PathBuf,
    state: PathBuf,
}

impl IsolatedUser {
    fn isolate() -> Self {
        let root = TempDir::new().expect("a user directory");
        let home = root.path().join("home");
        for directory in ["", ".local/share", ".local/state", ".config", ".cache"] {
            fs::create_dir_all(home.join(directory)).expect("an isolated directory");
        }
        let run = root.path().join("run");
        fs::create_dir_all(&run).expect("a runtime directory");
        let state = root.path().join("state-root");
        Self {
            _root: root,
            home,
            run,
            state,
        }
    }

    fn installer(&self, installer: &Path, args: &[&str]) -> Output {
        let mut command = Command::new(installer);
        command.args(args);
        command.arg("--state-root").arg(&self.state);
        command.env_clear();
        command.env("HOME", &self.home);
        command.env("XDG_DATA_HOME", self.home.join(".local/share"));
        command.env("XDG_STATE_HOME", self.home.join(".local/state"));
        command.env("XDG_CONFIG_HOME", self.home.join(".config"));
        command.env("XDG_CACHE_HOME", self.home.join(".cache"));
        command.env("XDG_RUNTIME_DIR", &self.run);
        command.env("PATH", "/usr/bin:/bin");
        command.output().expect("the installer process runs")
    }

    fn programs(&self) -> PathBuf {
        self.home.join(".local/lib/zup/apps")
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// `zup check` accepts the fixture project for its Linux target.
#[test]
fn check_accepts_a_linux_console_project() {
    let project = Project::new();
    let result = project.zup("check", &["--target", "linux"]);
    assert!(result.status.success(), "{}", stderr(&result));
    assert!(stdout(&result).contains("is valid"), "{}", stdout(&result));
}

/// `zup doctor` reports the Linux target ready to build.
#[test]
fn doctor_reports_the_linux_target_ready() {
    let project = Project::new();
    let result = project.zup("doctor", &["--target", "linux"]);
    assert!(result.status.success(), "{}", stderr(&result));
    assert!(stdout(&result).contains("ready"), "{}", stdout(&result));
}

/// `zup build` produces a real, runnable Linux installer with release metadata.
#[test]
fn build_produces_a_real_linux_installer() {
    let project = Project::new();
    let result = project.zup("build", &["--target", "linux"]);
    assert!(result.status.success(), "{}", stderr(&result));

    // Extensionless, beside the manifest, like every installer this build
    // produces - and runnable without a manual chmod, because composition
    // keeps the template's mode.
    let installer = project.root.path().join("Tool-Setup");
    assert!(installer.is_file(), "{}", stdout(&result));
    assert_eq!(installer.extension(), None);
    assert_ne!(
        std::os::unix::fs::PermissionsExt::mode(
            &fs::metadata(&installer).expect("stat").permissions()
        ) & 0o111,
        0,
        "the produced installer runs as produced"
    );

    // A valid ELF whose carrier opens: footer, package digest, package, and
    // image/target pairing all verified, the way its own runtime opens it.
    let image = zup_binary::Executable::read(&installer).expect("the image reads");
    assert_eq!(image.format(), zup_binary::BinaryFormat::Elf);
    let carrier = zup_linux::Carrier::open(&installer).expect("the carrier opens");
    assert_eq!(carrier.target().as_str(), LINUX_TARGET);
    let plan = carrier.package().build_plan().expect("the package decodes");
    assert_eq!(plan.targets.len(), 1);
    assert_eq!(plan.targets[0].files.len(), 2);
    assert!(
        plan.targets[0]
            .files
            .iter()
            .find(|file| file.source_relative.as_str() == "tool")
            .is_some_and(|file| file.executable),
        "executable intent survives the public path"
    );

    // Real public artifact metadata, with no Windows concept in it.
    let release_path = project.root.path().join("zup-release.json");
    let release = zup_artifact::ReleaseManifest::parse(
        &fs::read(&release_path).expect("the release description is written"),
    )
    .expect("the release description parses");
    assert_eq!(release.variants.len(), 1);
    assert_eq!(release.variants[0].target.as_str(), LINUX_TARGET);
    assert_eq!(
        release.variants[0].frontend,
        zup_core::Frontend::Console,
        "the frontend variant is recorded"
    );
    assert_eq!(release.artifacts.len(), 1);
    assert_eq!(release.artifacts[0].path, "Tool-Setup");
    let (size, digest) = zup_core::hash_reader(fs::File::open(&installer).expect("read"))
        .expect("the installer hashes");
    assert_eq!(release.artifacts[0].built.size, size);
    assert_eq!(release.artifacts[0].built.digest, digest);
    let encoded = serde_json::to_string(&release).expect("the release serializes");
    for banned in [".exe", "Authenticode", "windows-x64", "PE"] {
        assert!(
            !encoded.contains(banned),
            "Linux metadata names no Windows concept ({banned}): {encoded}"
        );
    }

    // The signing plan is honest about the absence of a native signing
    // operation: one post-compose file, no pre-compose runtime.
    let plan_path = project.root.path().join(zup_signing::SIGNING_PLAN_NAME);
    let signing = zup_signing::SigningPlan::parse(&fs::read(&plan_path).expect("a signing plan"))
        .expect("the signing plan parses");
    assert_eq!(signing.pre_compose().count(), 0);
    assert_eq!(signing.post_compose().count(), 1);
}

/// The full public lifecycle: build, install, run, uninstall.
#[test]
fn a_public_build_installs_runs_and_uninstalls() {
    let project = Project::new();
    let build = project.zup("build", &["--target", "linux"]);
    assert!(build.status.success(), "{}", stderr(&build));
    let installer = project.root.path().join("Tool-Setup");

    let user = IsolatedUser::isolate();
    let installed = user.installer(&installer, &[]);
    assert!(
        installed.status.success(),
        "install: {}",
        String::from_utf8_lossy(&installed.stderr)
    );

    let tool = user.programs().join("tool").join("tool");
    let version = Command::new(&tool)
        .arg("--version")
        .output()
        .expect("the installed executable runs");
    assert!(version.status.success());
    assert_eq!(
        String::from_utf8_lossy(&version.stdout).trim(),
        "tool 1.0.0"
    );
    assert_eq!(
        fs::read(user.programs().join("tool").join("keep.dat")).expect("keep.dat"),
        b"keep-v1"
    );

    let ledger = zup_linux::LinuxLedgerStore::new(&user.state)
        .load(
            &AppId::new("com.example.tool").expect("an id"),
            SelectedScope::User,
        )
        .expect("the ledger reads")
        .expect("an installation is recorded");
    assert_eq!(ledger.version.to_string(), "1.0.0");

    let uninstalled = user.installer(&installer, &["uninstall"]);
    assert!(
        uninstalled.status.success(),
        "uninstall: {}",
        String::from_utf8_lossy(&uninstalled.stderr)
    );
    assert!(!tool.exists(), "the payload is gone");
    let ledger = zup_linux::LinuxLedgerStore::new(&user.state)
        .load(
            &AppId::new("com.example.tool").expect("an id"),
            SelectedScope::User,
        )
        .expect("the ledger reads");
    assert!(ledger.is_none(), "the ledger is removed");
}

/// `zup artifact inspect` reads the produced Linux installer.
#[test]
fn inspect_reads_the_produced_installer() {
    let project = Project::new();
    let build = project.zup("build", &["--target", "linux"]);
    assert!(build.status.success(), "{}", stderr(&build));
    let installer = project.root.path().join("Tool-Setup");
    let inspected = Command::new(env!("CARGO_BIN_EXE_zup"))
        .args(["artifact", "inspect"])
        .arg(&installer)
        .output()
        .expect("zup runs");
    assert!(inspected.status.success(), "{}", stderr(&inspected));
    let text = stdout(&inspected);
    assert!(text.contains("Linux installer"), "{text}");
    assert!(text.contains(LINUX_TARGET), "{text}");
}

/// A GUI frontend is refused before any artifact work, naming the frontend.
#[test]
fn a_gui_frontend_is_refused_before_build() {
    let project =
        Project::with_manifest(&MANIFEST.replace("frontend = \"console\"", "frontend = \"gui\""));
    let result = project.zup("check", &["--target", "linux"]);
    assert!(!result.status.success(), "a GUI project was accepted");
    assert!(stderr(&result).contains("GUI"), "{}", stderr(&result));
}

/// A machine scope checks cleanly: machine installs run through the
/// privileged worker at install time, and `check`/`build` need no
/// authority. Machine desktop integration (not services) is still
/// refused, naming the deferred capability.
#[test]
fn a_machine_scope_checks_before_build() {
    let project = Project::with_manifest(
        &MANIFEST
            .replace("scope = \"user\"", "scope = \"machine\"")
            .replace(
                "user = \"${location.programs}/tool\"",
                "user = \"${location.programs}/tool\"\nmachine = \"${location.programs}/tool\"",
            ),
    );
    let result = project.zup("check", &["--target", "linux"]);
    assert!(
        result.status.success(),
        "a machine-scope project was refused: {}",
        stderr(&result)
    );
}

/// A machine-scope service project checks and builds: static manifest
/// services lower through systemd, still without authority at build time.
#[test]
fn a_machine_service_project_checks_and_builds() {
    let project = Project::with_manifest(&format!(
        "{}\n[[services]]\nid = \"tool\"\nname = \"Tool\"\nbinary = \"${{install}}/tool\"\nstart = \"automatic\"\n",
        MANIFEST
            .replace("scope = \"user\"", "scope = \"machine\"")
            .replace(
                "user = \"${location.programs}/tool\"",
                "user = \"${location.programs}/tool\"\nmachine = \"${location.programs}/tool\"",
            ),
    ));
    let result = project.zup("check", &["--target", "linux"]);
    assert!(
        result.status.success(),
        "a machine service project was refused: {}",
        stderr(&result)
    );
    let result = project.zup("build", &["--target", "linux"]);
    assert!(
        result.status.success(),
        "a machine service project did not build: {}",
        stderr(&result)
    );
    let installer = project.root.path().join("Tool-Setup");
    assert!(installer.is_file(), "the Linux installer artifact exists");
    let image = zup_binary::Executable::read(&installer).expect("the image reads");
    assert_eq!(image.format(), zup_binary::BinaryFormat::Elf);
}

/// An unsupported native resource is refused during capability validation.
#[test]
fn an_unsupported_native_resource_is_refused_before_build() {
    let project = Project::with_manifest(&format!(
        "{MANIFEST}\n[[services]]\nid = \"tool\"\nname = \"Tool\"\nbinary = \"${{install}}/tool\"\nstart = \"automatic\"\n"
    ));
    let result = project.zup("check", &["--target", "linux"]);
    assert!(!result.status.success(), "a service project was accepted");
    assert!(stderr(&result).contains("service"), "{}", stderr(&result));
}

/// A declared main that is not executable is refused with the fix spelled out.
#[test]
fn a_non_executable_main_is_refused_before_build() {
    let project = Project::with_manifest(&MANIFEST.replace(
        "version = \"1.0.0\"",
        "version = \"1.0.0\"\nmain = \"${install}/keep.dat\"",
    ));
    let result = project.zup("check", &["--target", "linux"]);
    assert!(
        !result.status.success(),
        "a non-executable main was accepted"
    );
    assert!(
        stderr(&result).contains("executable"),
        "{}",
        stderr(&result)
    );
}

/// A mismatched output count is refused before an artifact is written, on the
/// target the build actually composes.
#[test]
fn a_mismatched_output_count_is_refused_before_writing() {
    let project = Project::new();
    let first = project.root.path().join("first");
    let second = project.root.path().join("second");
    let result = project.zup(
        "build",
        &[
            "--target",
            "linux",
            "--output",
            first.to_str().unwrap(),
            "--output",
            second.to_str().unwrap(),
        ],
    );
    assert!(!result.status.success());
    let message = stderr(&result);
    assert!(
        message.contains("selected 1 targets (linux) but received 2 outputs"),
        "{message}"
    );
    assert!(!first.exists() && !second.exists());
}

/// A dispatcher composition has no Linux meaning and is refused precisely.
#[test]
fn a_universal_composition_is_refused_for_linux() {
    let project = Project::new();
    let result = project.zup("build", &["--target", "linux", "--universal"]);
    assert!(
        !result.status.success(),
        "a universal Linux build was accepted"
    );
    assert!(
        stderr(&result).contains("dispatcher"),
        "{}",
        stderr(&result)
    );
}
