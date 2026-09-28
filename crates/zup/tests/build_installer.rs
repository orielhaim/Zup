//! What `zup build` accepts and refuses, end to end against the real binary.
//!
//! Every test here runs the actual `zup` executable against a real project on
//! disk. The portable graph is covered by `zup-build`, the Windows resource
//! mechanics by `zup-windows`, and the installer runtime's own behaviour by
//! `zup-installer`. What is left is the part a project author touches: which
//! template and dispatcher a build will accept, and what it says when it will
//! not.
//!
//! A build that composes a real installer needs a real runtime template, and the
//! resolver finds the one staged beside the test binary. A test that needs a
//! template for a machine this host is not - or one that is deliberately wrong -
//! writes a component of its own, descriptor and all, so the production check is
//! exercised rather than bypassed.

#![cfg(windows)]

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use tempfile::TempDir;
use wit_component::{ComponentEncoder, StringEncoding, dummy_module, embed_component_metadata};
use wit_parser::{ManglingAndAbi, Resolve};
use zup_core::Frontend;

#[path = "support/staged.rs"]
mod staged;
#[path = "support/toolchain_fixture.rs"]
mod toolchain_fixture;

const PLUGIN_WIT: &str = include_str!("../../../wit/zup-plugin.wit");
const HOST_TARGET: &str = zup_plugin_contract::HOST_TARGET;
const X64: &str = "x86_64-pc-windows-msvc";
const ARM64: &str = "aarch64-pc-windows-msvc";

/// A project with one payload file and no plugins, which is the smallest thing a
/// build can be asked to compose.
struct Project {
    root: TempDir,
}

impl Project {
    fn new() -> Self {
        Self::with_manifest(
            r#"
schema = 1
[app]
id = "com.example.build"
name = "Build"
version = "1.0.0"
[build]

[build.targets.default]
target = "x86_64-pc-windows-msvc"
source = { directory = "dist" }
[install]
scope = "user"
[install.directory]
user = "${location.user_data}/Build"
[[files]]
source = "**/*"
destination = "${install}"
"#,
        )
    }

    /// A project whose manifest is replaced, for the refusals that need a
    /// different declaration.
    fn with_manifest(manifest: &str) -> Self {
        let root = TempDir::new().expect("a project directory");
        fs::create_dir_all(root.path().join("dist")).expect("a source directory");
        fs::write(root.path().join("dist/app.exe"), b"payload").expect("a payload file");
        fs::write(root.path().join("zup.toml"), manifest).expect("a manifest");
        Self { root }
    }

    fn path(&self) -> &Path {
        self.root.path()
    }

    fn manifest(&self) -> PathBuf {
        self.path().join("zup.toml")
    }

    /// Read the manifest back and change one thing, which is how most of these
    /// tests are written: a build's refusals are about a declaration, so the
    /// fixture stays legible as a manifest rather than as a builder call chain.
    fn amend(&self, from: &str, to: &str) {
        let path = self.manifest();
        let source = fs::read_to_string(&path).expect("the manifest is readable");
        assert!(
            source.contains(from),
            "the fixture no longer contains {from}"
        );
        fs::write(&path, source.replace(from, to)).expect("the manifest is writable");
    }

    /// Run `zup build` over this project against the staged toolchain, so the
    /// template the build composes is the one a person who installed `zup` gets.
    fn build(&self, extra: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_zup"));
        command
            .args(["build", "--manifest"])
            .arg(self.manifest())
            .arg("--output")
            .arg(self.path().join("Setup.exe"))
            .arg("--force")
            .args(extra);
        command.output().expect("zup runs")
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// This test binary, which is where the staged toolchain was built beside.
fn test_executable() -> PathBuf {
    std::env::current_exe().expect("a test executable")
}

#[test]
fn a_build_composes_an_installer_from_the_staged_toolchain() {
    let project = Project::new();
    let result = project.build(&["--release-manifest", "none"]);
    assert!(result.status.success(), "{}", stderr(&result));
    let output = project.path().join("Setup.exe");
    assert!(output.is_file());
    let bundle = zup_windows::EmbeddedBundle::open(&output).expect("the installer opens");
    assert_eq!(bundle.target().as_str(), X64);
    assert_eq!(bundle.frontend(), Frontend::Gui);
    assert_eq!(
        zup_windows::read_pe_subsystem(&output).expect("the subsystem reads"),
        zup_windows::PeSubsystem::Gui,
        "the composed image is the template's own subsystem"
    );
    assert_eq!(bundle.plan().entries.len(), 1);
}

/// The frontend is a property of the template a build composes into, so a manifest
/// that asks for one and a flag that asks for another have to agree with what is
/// actually resolvable.
#[test]
fn a_frontend_flag_selects_the_matching_template() {
    let project = Project::new();
    let console = staged::host_runtime(&test_executable(), Frontend::Console);
    let result = project.build(&[
        "--runtime",
        console.to_str().expect("a path"),
        "--frontend",
        "console",
        "--release-manifest",
        "none",
    ]);
    assert!(result.status.success(), "{}", stderr(&result));
    let output = project.path().join("Setup.exe");
    let bundle = zup_windows::EmbeddedBundle::open(&output).expect("the installer opens");
    assert_eq!(bundle.frontend(), Frontend::Console);
    assert_eq!(bundle.plan().installer.frontend, Frontend::Console);
    assert_eq!(
        zup_windows::read_pe_subsystem(&output).expect("the subsystem reads"),
        zup_windows::PeSubsystem::Console
    );
}

/// A template is named for the machine and the frontend it is for, and the build
/// confirms both against the file's own header rather than against the name. A
/// refusal is the same shape either way: what was given, and what was wanted.
#[test]
fn a_template_for_the_wrong_machine_or_frontend_is_refused() {
    for (label, target, frontend, extra) in [
        ("machine", ARM64, Frontend::Gui, vec![]),
        (
            "frontend",
            HOST_TARGET,
            Frontend::Headless,
            vec!["--frontend", "console"],
        ),
    ] {
        let project = Project::new();
        let template =
            toolchain_fixture::runtime(target, frontend).write(&project.path().join("templates"));
        let mut args = vec![
            "--runtime",
            template.to_str().expect("a path"),
            "--release-manifest",
            "none",
        ];
        args.extend(extra);
        let result = project.build(&args);
        assert!(!result.status.success(), "{label} was accepted");
        let message = stderr(&result);
        assert!(message.contains("was wanted"), "{label}: {message}");
        assert!(!project.path().join("Setup.exe").exists(), "{label}");
    }
}

/// A file that is not a component at all is refused with a reason that names the
/// problem, not with a missing-file error.
#[test]
fn a_file_that_is_not_a_component_is_refused() {
    let project = Project::new();
    let runtime = project.path().join("not-a-component.exe");
    fs::write(&runtime, b"not a PE").expect("the file is written");
    let result = project.build(&[
        "--runtime",
        runtime.to_str().expect("a path"),
        "--release-manifest",
        "none",
    ]);
    assert!(!result.status.success());
    let message = stderr(&result);
    assert!(
        message.contains("toolchain component") || message.contains("could not be read"),
        "{message}"
    );
    assert!(!project.path().join("Setup.exe").exists());
}

/// A component whose bytes were replaced after the descriptor was written is
/// refused. A name and a descriptor can both be intact while the file is not the
/// one they describe - which is what a partial copy, a truncated download, or
/// somebody's stray edit looks like.
#[test]
fn a_component_whose_bytes_were_replaced_is_refused() {
    let project = Project::new();
    let template = toolchain_fixture::runtime(HOST_TARGET, Frontend::Gui)
        .write(&project.path().join("toolchain"));
    let descriptor = template.with_file_name(format!(
        "{}{}",
        template.file_name().expect("a file name").to_string_lossy(),
        zup_toolchain::DESCRIPTOR_SUFFIX
    ));
    assert!(
        descriptor.is_file(),
        "the descriptor travels with the component"
    );

    let mut bytes = fs::read(&template).expect("the component is readable");
    let last = bytes.len() - 1;
    bytes[last] ^= 0xff;
    fs::write(&template, bytes).expect("the component is writable");

    let result = project.build(&[
        "--runtime",
        template.to_str().expect("a path"),
        "--release-manifest",
        "none",
    ]);
    assert!(!result.status.success());
    let message = stderr(&result);
    assert!(message.contains("digest"), "{message}");
    assert!(!project.path().join("Setup.exe").exists());
}

/// A destination Windows cannot create is refused before an artifact is written,
/// because writing the artifact and then failing leaves the author with a file
/// they have to delete.
#[test]
fn an_invalid_windows_destination_is_refused_before_writing_an_artifact() {
    let project = Project::new();
    project.amend("\"${install}\"", "\"${install}/CON\"");
    let result = project.build(&["--release-manifest", "none"]);
    assert!(!result.status.success());
    let message = stderr(&result);
    assert!(message.contains("Windows lowering"), "{message}");
    assert!(message.contains("reserved device name"), "{message}");
    assert!(!project.path().join("Setup.exe").exists());
}

/// A build runs from wherever the reader is standing, and every path it reads is
/// named by the manifest. A build that only worked from the project directory
/// would be a build a CI job could not run.
#[test]
fn a_build_runs_from_outside_the_project_directory() {
    let project = Project::new();
    let elsewhere = TempDir::new().expect("an unrelated directory");
    let output = project.path().join("Portable-Setup.exe");
    let result = Command::new(env!("CARGO_BIN_EXE_zup"))
        .current_dir(elsewhere.path())
        .args(["build", "--manifest"])
        .arg(project.manifest())
        .arg("--output")
        .arg(&output)
        .arg("--release-manifest")
        .arg("none")
        .output()
        .expect("zup runs");
    assert!(result.status.success(), "{}", stderr(&result));
    let package = zup_windows::EmbeddedBundle::open(&output).expect("the installer opens");
    assert_eq!(
        package.plan().installer.app.id.as_str(),
        "com.example.build"
    );
    assert_eq!(package.plan().entries.len(), 1);
}

/// A declared plugin is compiled ahead-of-time and shipped as an artifact, and
/// the build plan that travels inside the package carries no trace of the source
/// it was compiled from.
#[test]
fn a_build_compiles_and_embeds_a_declared_plugin() {
    let project = TempDir::new().expect("a project directory");
    fs::create_dir_all(project.path().join("dist")).expect("a source directory");
    fs::create_dir_all(project.path().join("plugins")).expect("a plugin directory");
    fs::write(project.path().join("dist/app.exe"), b"payload").expect("a payload file");
    fs::write(
        project.path().join("plugins/helper.wasm"),
        plugin_component(),
    )
    .expect("a plugin component");
    fs::write(
        project.path().join("zup.toml"),
        format!(
            r#"
schema = 1
[app]
id = "com.example.plugin"
name = "Plugin"
version = "1.0.0"
[build]

[build.targets.default]
target = "{HOST_TARGET}"
source = {{ directory = "dist" }}
[install]
scope = "user"
[install.directory]
user = "${{location.user_data}}/Plugin"
[[files]]
source = "**/*"
destination = "${{install}}"
[[plugins]]
id = "helper"
source = "plugins/helper.wasm"
"#
        ),
    )
    .expect("a manifest");
    let output = project.path().join("Plugin-Setup.exe");
    let result = Command::new(env!("CARGO_BIN_EXE_zup"))
        .args(["build", "--manifest"])
        .arg(project.path().join("zup.toml"))
        .arg("--output")
        .arg(&output)
        .arg("--release-manifest")
        .arg("none")
        .output()
        .expect("zup runs");
    assert!(result.status.success(), "{}", stderr(&result));

    let package = zup_windows::EmbeddedBundle::open(&output).expect("the installer opens");
    let id = zup_core::PluginId::new("helper").expect("a plugin id");
    let metadata = package
        .plugin_artifact(&id)
        .expect("the artifact is embedded");
    assert_eq!(
        metadata.target,
        zup_core::TargetTriple::parse(HOST_TARGET).expect("a target")
    );
    assert!(
        !package
            .plugin_aot(&id)
            .expect("the artifact is readable")
            .is_empty()
    );
    assert!(
        package.build_plan().expect("the plan is readable").targets[0]
            .plugins
            .is_empty(),
        "the shipped plan names no build-time plugin source"
    );
    assert_eq!(package.plan().entries.len(), 1);
}

/// A minimal component, real enough that the build's ahead-of-time compiler
/// accepts it.
fn plugin_component() -> Vec<u8> {
    let mut resolve = Resolve::default();
    let package = resolve
        .push_str("zup-plugin.wit", PLUGIN_WIT)
        .expect("the plugin world parses");
    let world = resolve
        .select_world(&[package], Some("plugin"))
        .expect("the plugin world is selectable");
    let mut module = dummy_module(&resolve, world, ManglingAndAbi::Standard32);
    embed_component_metadata(&mut module, &resolve, world, StringEncoding::UTF8)
        .expect("the module carries component metadata");
    ComponentEncoder::default()
        .module(&module)
        .expect("the module encodes")
        .validate(true)
        .encode()
        .expect("the component encodes")
}
