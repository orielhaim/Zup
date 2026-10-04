//! `zup plugin init` and `zup plugin build`: authoring a plugin without knowing
//! how one is componentised.
//!
//! Building a Zup plugin is the same every time, and every part of it is a
//! detail of the Component Model rather than of the plugin: compile to
//! `wasm32-unknown-unknown` as a `cdylib`, then turn the resulting core module
//! into a component that implements the plugin world. Both happen here, so an
//! author runs one command instead of pinning a `wasm-tools` version they would
//! have to keep in step with Zup's.
//!
//! The WIT is not involved either. The SDK generates its bindings from the
//! contract Zup owns, so a plugin project contains no copy of it and there is no
//! vendored file to fall out of date.

use std::path::{Path, PathBuf};
use std::process::Command;

use clap::{Args, Subcommand, ValueHint};
use zup_plugin_build::componentize;

use crate::failure;

/// The target a plugin is compiled for.
///
/// Fixed rather than chosen: a plugin is Wasm, not native code, and the host
/// that runs it is the one that decides which interpreter executes it. A
/// cross-compile here would produce a module the host cannot load.
const GUEST_TARGET: &str = "wasm32-unknown-unknown";

/// Why the guest target could not be made available.
#[derive(Debug, thiserror::Error)]
pub enum GuestTargetError {
    #[error("could not run rustup: {0}")]
    Rustup(#[source] std::io::Error),
    #[error(
        "`{GUEST_TARGET}` is not installed for this toolchain, and rustup could not install it:\n\
         {stderr}\n\nInstall it with `rustup target add {GUEST_TARGET}` and try again."
    )]
    Install { stderr: String },
    #[error(
        "`{GUEST_TARGET}` is not installed for this toolchain and rustup is not available to \
         install it.\n\nInstall the target with `rustup target add {GUEST_TARGET}` and try again, \
         or build this plugin on a machine whose Rust toolchain has it."
    )]
    Unmanaged,
}

/// Make sure the guest target is installed before anything asks Cargo to build it.
///
/// The claim this command makes is that a plugin author has nothing to install
/// but zup, and a bare `cargo build --target` breaks that on any machine that
/// has not happened to add the target already: the failure arrives as
/// `can't find crate for core`, which says nothing about the fix.
///
/// The toolchain in use is the one asked, rather than one Zup would bring
/// along, because that is the one whose output a host will run. `rustup` is the
/// thing that knows which toolchain is active - the default one, whatever a
/// `rust-toolchain.toml` in the project selects - and the only thing that can
/// add a target to it. A Rust installation without rustup can still have the
/// target, so a missing `rustup` is a refusal with instructions rather than a
/// guess at what to do.
pub fn ensure_guest_target(root: &Path) -> Result<(), GuestTargetError> {
    let installed = Command::new("rustup")
        .current_dir(root)
        .args(["target", "list", "--installed"])
        .output()
        .map_err(GuestTargetError::Rustup)?;
    let listed = String::from_utf8_lossy(&installed.stdout);
    if target_is_installed(&listed, GUEST_TARGET) {
        return Ok(());
    }

    let added = Command::new("rustup")
        .current_dir(root)
        .args(["target", "add", GUEST_TARGET])
        .output()
        .map_err(GuestTargetError::Rustup)?;
    if !added.status.success() {
        return Err(GuestTargetError::Install {
            stderr: String::from_utf8_lossy(&added.stderr).trim().to_owned(),
        });
    }
    println!("installed the `{GUEST_TARGET}` target for this toolchain");
    Ok(())
}

/// Whether a `rustup target list --installed` listing includes `target`.
///
/// One target per line and nothing padded, so the answer is a whole-line match
/// rather than a substring: `wasm32-unknown-unknown` is not installed by the
/// fact that some longer target starts with it.
fn target_is_installed(listing: &str, target: &str) -> bool {
    listing.lines().any(|line| line.trim() == target)
}

/// Author and build a plugin.
#[derive(Debug, Args)]
pub struct PluginCommand {
    #[command(subcommand)]
    pub command: PluginVerb,
}

#[derive(Debug, Subcommand)]
pub enum PluginVerb {
    /// Create a plugin project.
    Init(PluginInitCommand),
    /// Build this project into a plugin component.
    Build(PluginBuildCommand),
}

/// Create a plugin project.
#[derive(Debug, Args)]
pub struct PluginInitCommand {
    /// What to call it. The project directory takes this name.
    pub name: String,
    /// The directory to create it in. Defaults to the current one.
    #[arg(long, value_name = "DIR", value_hint = ValueHint::DirPath)]
    pub directory: Option<PathBuf>,
}

/// Build a plugin into the component Zup loads.
#[derive(Debug, Args)]
pub struct PluginBuildCommand {
    /// The plugin project to build. Defaults to the current directory.
    #[arg(long, value_name = "DIR", value_hint = ValueHint::DirPath)]
    pub project: Option<PathBuf>,
    /// The Cargo profile to build with.
    #[arg(long, value_name = "PROFILE", default_value = "release")]
    pub profile: String,
    /// Where to write the component. Defaults to `dist/<name>.wasm` here.
    #[arg(long, short, value_name = "FILE", value_hint = ValueHint::FilePath)]
    pub output: Option<PathBuf>,
}

/// Create a plugin project named `name` in `parent`.
pub fn init(args: &PluginInitCommand) -> miette::Result<()> {
    let parent = match args.directory.clone() {
        Some(directory) => directory,
        None => std::env::current_dir()
            .map_err(|error| failure::error("zup.plugin.init_cwd", error.to_string()))?,
    };
    let project = crate::preset::init::Generator::new(&args.name, &parent, "zup.plugin.init")
        .map_err(|error| failure::error("zup.plugin.init_name", error))?;
    let name = project.name().to_owned();
    let root = project.root().to_path_buf();
    project.create(&[
        ("Cargo.toml", manifest(&name)),
        ("src/lib.rs", LIB.to_owned()),
        (".gitignore", GITIGNORE.to_owned()),
    ])?;
    println!("Created the plugin `{name}` in {}", root.display());
    println!("  cd {name}");
    println!("  zup plugin build");
    Ok(())
}

fn manifest(name: &str) -> String {
    format!(
        r#"[package]
name = "{name}"
version = "0.1.0"
edition = "2024"
description = "A zup installation plugin"
publish = false

# A plugin is Wasm, so it is a library that becomes a component rather than an
# executable. `zup plugin build` compiles it and componentises the result.
[lib]
crate-type = ["cdylib"]

[dependencies]
zup-sdk = {{ version = "0.1.0", features = ["plugin"] }}
"#
    )
}

const LIB: &str = r#"//! A Zup plugin.
//!
//! A plugin answers one question - given what is being installed and what a
//! person selected, what should exist afterwards - by returning a declaration.
//! Zup decides whether that declaration is safe and performs the install, so
//! there is nothing here that runs a command or writes to the machine directly.

use zup_sdk::plugin::prelude::*;

struct MyPlugin;

impl Plugin for MyPlugin {
    fn plan(context: Context) -> Result<Plan, Error> {
        Ok(Plan::new().generated_file(GeneratedFile::text(
            "${install}/installed-by.txt",
            format!("installed by {}", context.plugin_id),
        )))
    }
}

zup_sdk::plugin::export!(MyPlugin);
"#;

const GITIGNORE: &str = r#"/target
"#;

/// Build this project into a plugin component.
pub fn build(args: &PluginBuildCommand) -> miette::Result<()> {
    let root = match args.project.clone() {
        Some(project) => project,
        None => std::env::current_dir()
            .map_err(|error| failure::error("zup.plugin.build_cwd", error.to_string()))?,
    };
    let project = Project::read(&root)?;
    let cargo = crate::project::cargo_executable();

    // Before anything asks Cargo to build: a plugin author is not expected to
    // know which target a plugin is compiled for, so Zup does not let them find
    // out by being handed a `can't find crate for core`.
    ensure_guest_target(&root)
        .map_err(|error| failure::error("zup.plugin.build_target", error.to_string()))?;

    // Step one: the ordinary Cargo build, for the target a plugin is compiled
    // for. Cargo is asked where its output actually is rather than having a path
    // guessed, because a guessed path is right until someone sets a target
    // directory.
    let module = build_module(&cargo, &root, &project, &args.profile)?;

    // Step two: the part that is Zup's rather than the author's. A core module
    // is not a component, and a component is what a host loads. The world comes
    // from the contract Zup owns, so there is no second copy to disagree.
    let component = componentize(&module)
        .map_err(|error| failure::error("zup.plugin.componentize", error.to_string()))?;
    println!(
        "componentised {} -> {} bytes",
        project.name,
        component.len()
    );

    let output = args
        .output
        .clone()
        .unwrap_or_else(|| root.join("dist").join(format!("{}.wasm", project.name)));
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            failure::error(
                "zup.plugin.build_write",
                format!("{}: {error}", parent.display()),
            )
        })?;
    }
    std::fs::write(&output, &component).map_err(|error| {
        failure::error(
            "zup.plugin.build_write",
            format!("{}: {error}", output.display()),
        )
    })?;
    println!("{}", output.display());
    Ok(())
}

/// A plugin project, as Cargo describes it.
struct Project {
    name: String,
    library: String,
}

impl Project {
    /// Read the project's own package, which is where its identity comes from.
    fn read(root: &Path) -> miette::Result<Self> {
        let package = crate::project::own_package(root)?;
        let library = crate::project::target_of_kind(&package, "cdylib")?;
        Ok(Self {
            name: package.name.as_str().to_owned(),
            library: library.name.clone(),
        })
    }
}

/// Build the plugin for the guest target and return the module Cargo produced.
fn build_module(
    cargo: &Path,
    root: &Path,
    project: &Project,
    profile: &str,
) -> miette::Result<Vec<u8>> {
    let mut command = Command::new(cargo);
    command
        .current_dir(root)
        .arg("build")
        .arg("--message-format=json")
        .arg("--lib")
        .arg("--profile")
        .arg(profile)
        .arg("--target")
        .arg(GUEST_TARGET);
    let output = command
        .output()
        .map_err(|error| miette::miette!("could not run cargo: {error}"))?;
    if !output.status.success() {
        return Err(miette::miette!(
            "building the plugin for `{GUEST_TARGET}` failed:\n{}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let mut found = None;
    for line in output.stdout.split(|byte| *byte == b'\n') {
        if let Some(path) = artifact_executable(line, &project.library) {
            found = Some(PathBuf::from(path));
        }
    }
    let path = found.ok_or_else(|| {
        miette::miette!(
            "cargo built nothing named `{}` for `{GUEST_TARGET}`",
            project.library
        )
    })?;
    std::fs::read(&path).map_err(|error| miette::miette!("{}: {error}", path.display()))
}

/// The module one `compiler-artifact` message reports, if it is the one asked for.
///
/// Cargo interleaves compiler messages with build-script output and reports
/// artifacts for every target in a dependency graph, so the kind and the name
/// both have to match before a path is taken. Anything unparseable is skipped
/// rather than refused: a build that printed a line this does not understand
/// still produced its artifacts.
fn artifact_executable(line: &[u8], library: &str) -> Option<String> {
    let message: serde_json::Value = serde_json::from_slice(line).ok()?;
    if message["reason"] != "compiler-artifact" {
        return None;
    }
    let target = &message["target"];
    let is_a_library = target["kind"]
        .as_array()
        .is_some_and(|kinds| kinds.iter().any(|kind| kind == "cdylib"));
    if !is_a_library || target["name"] != library {
        return None;
    }
    message["filenames"]
        .as_array()
        .and_then(|files| files.first())
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    /// The answer to "is this target installed" is a whole-line match, so a
    /// target that merely starts with the one being asked about is not one.
    #[rstest]
    #[case("wasm32-unknown-unknown", true)]
    #[case("wasm32-unknown-unknown\nwasm32-wasip1", true)]
    #[case("wasm32-unknown-unknown-preview", false)]
    #[case("wasm32-wasip1", false)]
    #[case("", false)]
    #[case("\n\n", false)]
    fn the_listing_is_read_a_line_at_a_time(#[case] listing: &str, #[case] expected: bool) {
        assert_eq!(target_is_installed(listing, GUEST_TARGET), expected);
    }
}
