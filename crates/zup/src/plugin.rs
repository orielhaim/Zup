use std::path::{Path, PathBuf};
use std::process::Command;

use clap::{Args, Subcommand, ValueHint};
use zup_plugin_build::componentize;

use crate::failure;

const GUEST_TARGET: &str = "wasm32-unknown-unknown";

#[derive(Debug, thiserror::Error)]
pub enum GuestTargetError {
    #[error("could not run `{compiler}` to ask whether `{GUEST_TARGET}` is available: {source}")]
    Probe {
        compiler: String,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "`{GUEST_TARGET}` is not available and rustup could not install it:\n{stderr}\n\n\
         Install it with `rustup target add {GUEST_TARGET}` and try again."
    )]
    Install { stderr: String },
    #[error(
        "`{GUEST_TARGET}` is not available to this Rust toolchain, and rustup is not \
         installed to add it.\n\nInstall the target with `rustup target add {GUEST_TARGET}` and \
         try again, or build this plugin on a machine whose Rust toolchain has it."
    )]
    Unmanaged,
    #[error("`{GUEST_TARGET}` is not available and rustup could not be run to add it: {source}")]
    Rustup {
        #[source]
        source: std::io::Error,
    },
}

fn rustc_executable() -> PathBuf {
    std::env::var_os("RUSTC")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("rustc"))
}

pub fn ensure_guest_target(root: &Path) -> Result<(), GuestTargetError> {
    if target_is_available(root)? {
        return Ok(());
    }

    let added = Command::new("rustup")
        .current_dir(root)
        .args(["target", "add", GUEST_TARGET])
        .output()
        .map_err(|source| match source.kind() {
            std::io::ErrorKind::NotFound => GuestTargetError::Unmanaged,
            _ => GuestTargetError::Rustup { source },
        })?;
    if !added.status.success() {
        return Err(GuestTargetError::Install {
            stderr: String::from_utf8_lossy(&added.stderr).trim().to_owned(),
        });
    }

    if !target_is_available(root)? {
        return Err(GuestTargetError::Install {
            stderr: format!(
                "rustup reported installing `{GUEST_TARGET}`, but {} still cannot build for it",
                rustc_executable().display()
            ),
        });
    }
    println!("installed the `{GUEST_TARGET}` target for this toolchain");
    Ok(())
}

fn target_is_available(root: &Path) -> Result<bool, GuestTargetError> {
    let compiler = rustc_executable();
    let output = Command::new(&compiler)
        .current_dir(root)
        .args(["--print", "target-libdir", "--target", GUEST_TARGET])
        .output()
        .map_err(|source| GuestTargetError::Probe {
            compiler: compiler.display().to_string(),
            source,
        })?;
    if !output.status.success() {
        return Ok(false);
    }
    Ok(library_directory(&output.stdout).is_some_and(|directory| directory.is_dir()))
}

fn library_directory(stdout: &[u8]) -> Option<PathBuf> {
    let reported = String::from_utf8_lossy(stdout);
    let directory = reported.trim().lines().last()?.trim();
    (!directory.is_empty()).then(|| PathBuf::from(directory))
}

#[derive(Debug, Args)]
pub struct PluginCommand {
    #[command(subcommand)]
    pub command: PluginVerb,
}

#[derive(Debug, Subcommand)]
pub enum PluginVerb {
    Init(PluginInitCommand),
    Build(PluginBuildCommand),
}

#[derive(Debug, Args)]
pub struct PluginInitCommand {
    pub name: String,
    #[arg(long, value_name = "DIR", value_hint = ValueHint::DirPath)]
    pub directory: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct PluginBuildCommand {
    #[arg(long, value_name = "DIR", value_hint = ValueHint::DirPath)]
    pub project: Option<PathBuf>,
    #[arg(long, value_name = "PROFILE", default_value = "release")]
    pub profile: String,
    #[arg(long, short, value_name = "FILE", value_hint = ValueHint::FilePath)]
    pub output: Option<PathBuf>,
}

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

pub fn build(args: &PluginBuildCommand) -> miette::Result<()> {
    let root = match args.project.clone() {
        Some(project) => project,
        None => std::env::current_dir()
            .map_err(|error| failure::error("zup.plugin.build_cwd", error.to_string()))?,
    };
    let project = Project::read(&root)?;
    let cargo = crate::project::cargo_executable();

    ensure_guest_target(&root)
        .map_err(|error| failure::error("zup.plugin.build_target", error.to_string()))?;

    let module = build_module(&cargo, &root, &project, &args.profile)?;

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

struct Project {
    name: String,
    library: String,
}

impl Project {
    fn read(root: &Path) -> miette::Result<Self> {
        let package = crate::project::own_package(root)?;
        let library = crate::project::target_of_kind(&package, "cdylib")?;
        Ok(Self {
            name: package.name.as_str().to_owned(),
            library: library.name.clone(),
        })
    }
}

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

    #[rstest]
    #[case::path("/toolchain/lib/rustlib/wasm32-unknown-unknown/lib\n", true)]
    #[case::path_with_a_carriage_return(
        "/toolchain/lib/rustlib/wasm32-unknown-unknown/lib\r\n",
        true
    )]
    #[case::last_of_several(
        "warning: something\n/toolchain/lib/rustlib/wasm32-unknown-unknown/lib\n",
        true
    )]
    #[case::empty("", false)]
    #[case::only_newlines("\n\n", false)]
    fn the_library_directory_is_the_last_thing_the_compiler_printed(
        #[case] stdout: &str,
        #[case] expected: bool,
    ) {
        assert_eq!(library_directory(stdout.as_bytes()).is_some(), expected);
    }

    #[test]
    fn a_named_directory_that_does_not_exist_is_not_a_target() {
        let missing = std::env::temp_dir().join("zup-no-such-target-libdir");
        assert!(!missing.is_dir(), "the stand-in does not exist");
        let named = library_directory(format!("{}\n", missing.display()).as_bytes())
            .expect("a directory was named");
        assert!(
            !named.is_dir(),
            "so it is not a target this toolchain can build"
        );
    }

    /// question the suite must not assume: the whole point is to handle machines
    #[test]
    fn a_compiler_that_cannot_be_run_is_not_reported_as_a_missing_target() {
        let missing = "zup-no-such-rustc";
        let error = Command::new(missing)
            .args(["--print", "target-libdir"])
            .output()
            .expect_err("a compiler that does not exist cannot be run");
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);

        let reported = GuestTargetError::Probe {
            compiler: missing.to_owned(),
            source: error,
        }
        .to_string();
        assert!(
            reported.contains("could not run") && reported.contains(missing),
            "a compiler that cannot be run is a different problem from an absent target, and \
             is reported as one: {reported}"
        );
    }

    #[test]
    fn a_missing_rustup_is_distinct_from_one_that_will_not_run() {
        let refused = GuestTargetError::Unmanaged.to_string();
        assert!(
            refused.contains("rustup is not installed"),
            "so it says rustup is absent: {refused}"
        );

        let denied = GuestTargetError::Rustup {
            source: std::io::Error::new(std::io::ErrorKind::PermissionDenied, "access is denied"),
        }
        .to_string();
        assert!(
            denied.contains("access is denied"),
            "and a rustup that would not run keeps the reason rather than being reported as \
             absent: {denied}"
        );
        assert!(
            !denied.contains("rustup is not installed"),
            "which would send the author looking for a rustup that is installed"
        );
    }

    #[test]
    fn the_failures_name_the_one_thing_the_author_can_do() {
        let unmanaged = GuestTargetError::Unmanaged.to_string();
        assert!(
            unmanaged.contains(&format!("rustup target add {GUEST_TARGET}")),
            "without rustup, the instruction is the command to run by hand: {unmanaged}"
        );

        let failed = GuestTargetError::Install {
            stderr: "error: no such toolchain".to_owned(),
        }
        .to_string();
        assert!(
            failed.contains("no such toolchain"),
            "a failed installation keeps what rustup said: {failed}"
        );
    }
}
