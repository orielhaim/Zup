//! `zup ui pack` and `zup ui inspect`: producing and looking at a `.zupui`.
//!
//! A preset is a Rust project, so publishing one is a Cargo build followed by
//! packaging. Cargo is asked where its output actually is - through the
//! `CompilerArtifact` messages a build emits - rather than by guessing a path,
//! because a guessed path is right until someone sets a profile, a target
//! directory, or a target triple.
//!
//! One machine cannot build every target, so a package is assembled from
//! whatever binaries a caller has: a target built here, or one a CI runner built
//! elsewhere. Both are the same input, and there is one command for them rather
//! than a build command and an add command that disagree about the format.

use std::path::{Path, PathBuf};
use std::process::Command;

pub mod init;

use clap::{Args, Subcommand, ValueHint};
use zup_artifact::ui::{PresetPackageView, PresetPackageWriter};
use zup_core::TargetTriple;
use zup_ui_protocol::{MAX_DESCRIBE_BYTES, PresetDescription};

use crate::failure;

/// One preset binary, by target triple and file.
#[derive(Debug, Args, Clone)]
pub struct BinarySource {
    /// The target this binary was built for, as a canonical triple.
    #[arg(long, value_name = "TRIPLE")]
    pub target: String,
    /// The binary itself.
    #[arg(long, value_name = "FILE", value_hint = ValueHint::FilePath)]
    pub binary: PathBuf,
}

/// Author and look at preset packages.
#[derive(Debug, Args)]
pub struct UiCommand {
    #[command(subcommand)]
    pub command: UiVerb,
}

#[derive(Debug, Subcommand)]
pub enum UiVerb {
    /// Create a preset project.
    Init(InitCommand),
    /// Build and run the preset in this project, against a simulated installer.
    Dev(DevCommand),
    /// Build or collect preset binaries into a `.zupui`.
    Pack(PackCommand),
    /// Report what a `.zupui` contains, without running it.
    Inspect(InspectCommand),
}

/// Create a preset project.
#[derive(Debug, Args)]
pub struct InitCommand {
    /// What to call it. The project directory takes this name.
    pub name: String,
    /// The directory to create it in. Defaults to the current one.
    #[arg(long, value_name = "DIR", value_hint = ValueHint::DirPath)]
    pub directory: Option<PathBuf>,
}

/// Run the preset in this project against a simulated installer.
#[derive(Debug, Args)]
pub struct DevCommand {
    /// The preset project to develop. Defaults to the current directory.
    #[arg(long, value_name = "DIR", value_hint = ValueHint::DirPath)]
    pub project: Option<PathBuf>,
    /// The Cargo profile to build with.
    #[arg(long, value_name = "PROFILE", default_value = "dev")]
    pub profile: String,
}

/// Run `zup ui dev`.
pub fn dev(args: &DevCommand) -> miette::Result<()> {
    // What the session prints is its product, and a redirected stdout is block
    // buffered by default, so a development tool would say nothing at all for
    // the two minutes its first build takes. That is exactly when somebody is
    // watching to find out whether it started.
    let _lines = std::io::LineWriter::new(std::io::stdout());
    let root = match args.project.clone() {
        Some(project) => project,
        None => std::env::current_dir()
            .map_err(|error| failure::error("zup.ui.dev_cwd", error.to_string()))?,
    };
    zup_ui_dev::develop(root, &args.profile)
        .map_err(|error| failure::error("zup.ui.dev", error.to_string()))
}

/// Run `zup ui init`.
pub fn generate(args: &InitCommand) -> miette::Result<()> {
    let parent = match args.directory.clone() {
        Some(directory) => directory,
        None => std::env::current_dir()
            .map_err(|error| failure::error("zup.ui.init_cwd", error.to_string()))?,
    };
    init::init(&args.name, &parent)
}

#[derive(Debug, Args)]
pub struct PackCommand {
    /// The preset project to read. Defaults to the current directory.
    #[arg(long, value_name = "DIR", value_hint = ValueHint::DirPath)]
    pub manifest: Option<PathBuf>,
    /// The package to write. Defaults to `<preset-name>-<version>.zupui` here.
    #[arg(long, short, value_name = "FILE", value_hint = ValueHint::FilePath)]
    pub output: Option<PathBuf>,
    /// A binary to include, already built.
    #[arg(long = "binary", value_name = "TRIPLE=FILE", value_parser = parse_binary, num_args = 1)]
    pub binaries: Vec<BinarySource>,
    /// Build this target here with Cargo and include the result.
    #[arg(long = "build", value_name = "TRIPLE")]
    pub build: Vec<String>,
    /// Build with this Cargo profile.
    #[arg(long, value_name = "PROFILE", default_value = "release")]
    pub profile: String,
    /// Overwrite an existing package.
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Args)]
pub struct InspectCommand {
    /// The package to read.
    #[arg(value_name = "FILE", value_hint = ValueHint::FilePath)]
    pub package: PathBuf,
}

fn parse_binary(raw: &str) -> Result<BinarySource, String> {
    let (target, binary) = raw
        .split_once('=')
        .ok_or_else(|| "expected `--binary <triple>=<file>`".to_owned())?;
    let target = TargetTriple::parse(target).map_err(|error| error.to_string())?;
    Ok(BinarySource {
        target: target.as_str().to_owned(),
        binary: PathBuf::from(binary),
    })
}

/// Build or collect preset binaries into a `.zupui`.
pub fn pack(args: &PackCommand) -> miette::Result<()> {
    let root = repository_root(args.manifest.as_deref())?;
    let cargo = cargo_executable();
    let identity = PresetIdentity::read(&root)?;

    let mut sources: Vec<BinarySource> = args.binaries.clone();
    for triple in &args.build {
        sources.push(BinarySource {
            target: triple.clone(),
            binary: build_target(&root, &cargo, &identity.binary, triple, &args.profile)?,
        });
    }
    if sources.is_empty() {
        return Err(miette::miette!(
            "nothing to package: pass `--build <triple>` to build a target here, or \
             `--binary <triple>=<file>` for one built elsewhere"
        ));
    }

    let description = describe(&root, &cargo, &identity, &sources, &args.profile)?;
    description
        .validate()
        .map_err(|error| miette::miette!("the preset described itself as unusable: {error}"))?;
    identity.cross_check(&description)?;

    let version = description.version.clone();
    let mut writer =
        PresetPackageWriter::new(description).map_err(|error| miette::miette!("{error}"))?;
    for source in &sources {
        let target =
            TargetTriple::parse(&source.target).map_err(|error| miette::miette!("{error}"))?;
        let bytes = std::fs::read(&source.binary).map_err(|error| {
            miette::miette!("preset binary `{}`: {error}", source.binary.display())
        })?;
        writer
            .add_binary(target, bytes)
            .map_err(|error| miette::miette!("{error}"))?;
    }

    let output = args
        .output
        .clone()
        .unwrap_or_else(|| root.join(format!("{}-{}.zupui", identity.name, version)));
    if output.exists() && !args.force {
        return Err(miette::miette!(
            "`{}` already exists; pass --force to replace it",
            output.display()
        ));
    }
    let bytes = writer
        .finish()
        .map_err(|error| miette::miette!("{error}"))?;
    write_durably(&output, &bytes)?;

    // The package on disk is read back through the same reader `zup ui inspect`
    // uses, so a package that was written but cannot be read is not reported as
    // a success.
    let reopened = std::fs::read(&output).map_err(|error| miette::miette!("{error}"))?;
    let view = PresetPackageView::open(reopened)
        .map_err(|error| miette::miette!("the package just written does not read back: {error}"))?;
    view.verify()
        .map_err(|error| miette::miette!("the package just written does not verify: {error}"))?;

    for target in view.targets() {
        println!("  packed  {target}");
    }
    println!(
        "\n{} {} - {} target(s), {} bytes",
        view.name(),
        view.version(),
        view.targets().len(),
        bytes.len()
    );
    println!("{}", output.display());
    Ok(())
}

/// Report what a `.zupui` contains, without running it.
pub fn inspect(args: &InspectCommand) -> miette::Result<()> {
    let bytes = std::fs::read(&args.package)
        .map_err(|error| miette::miette!("`{}`: {error}", args.package.display()))?;
    let view = PresetPackageView::open(bytes).map_err(|error| {
        miette::miette!(
            "`{}` is not a usable preset package: {error}",
            args.package.display()
        )
    })?;
    view.verify().map_err(|error| {
        miette::miette!("`{}` does not verify: {error}", args.package.display())
    })?;
    let package = view.package();

    println!("preset        {} {}", view.name(), view.version());
    println!("package       schema {}", package.schema);
    println!("ui protocol   {}", package.ui_protocol);
    println!(
        "capabilities  {}",
        if package.required_capabilities.is_empty() {
            "none required".to_owned()
        } else {
            package.required_capabilities.names().join(", ")
        }
    );
    println!(
        "settings      {}",
        settings_summary(&package.settings_schema)
    );
    println!("\ntargets");
    for binary in &package.binaries {
        println!(
            "  {:<28} {:>10}  sha256:{}",
            binary.target.as_str(),
            human_bytes(binary.size),
            binary.sha256.to_hex()
        );
    }
    Ok(())
}

/// One line about what the settings accept.
fn settings_summary(schema: &serde_json::Value) -> String {
    let properties = schema
        .get("properties")
        .and_then(serde_json::Value::as_object)
        .map(|properties| {
            let mut names: Vec<&str> = properties.keys().map(String::as_str).collect();
            names.sort_unstable();
            names.join(", ")
        })
        .unwrap_or_default();
    if properties.is_empty() {
        "no settings".to_owned()
    } else {
        format!(
            "{properties} ({})",
            if schema["additionalProperties"] == serde_json::Value::Bool(false) {
                "no others accepted"
            } else {
                "others permitted"
            }
        )
    }
}

fn human_bytes(bytes: u64) -> String {
    zup_presentation::format_bytes(bytes)
}

/// Ask the preset what it is, by running its describe mode.
///
/// The only time a preset executable runs, and only the one this machine built
/// for itself: this reads a document and does not open a window. When the host
/// target is among the binaries being packaged, that binary is asked rather than
/// a second copy of it being built.
fn describe(
    root: &Path,
    cargo: &Path,
    identity: &PresetIdentity,
    sources: &[BinarySource],
    profile: &str,
) -> miette::Result<PresetDescription> {
    let host = TargetTriple::parse(&host_target()?).map_err(|error| miette::miette!("{error}"))?;
    let executable = match sources.iter().find(|source| source.target == host.as_str()) {
        Some(source) => source.binary.clone(),
        None => build_target(root, cargo, &identity.binary, host.as_str(), profile)?,
    };
    let output = Command::new(&executable)
        .arg(zup_ui_protocol::DESCRIBE_FLAG)
        .output()
        .map_err(|error| {
            miette::miette!(
                "could not run `{}` to describe the preset: {error}",
                executable.display()
            )
        })?;
    if !output.status.success() {
        return Err(miette::miette!(
            "the preset's describe mode failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let document = output.stdout;
    if document.len() > MAX_DESCRIBE_BYTES {
        return Err(miette::miette!(
            "the preset described itself in {} bytes; the limit is {MAX_DESCRIBE_BYTES}",
            document.len()
        ));
    }
    serde_json::from_slice(&document)
        .map_err(|error| miette::miette!("the preset's describe document is unusable: {error}"))
}

/// What the preset's Cargo manifest says it is.
struct PresetIdentity {
    name: String,
    version: String,
    binary: String,
}

impl PresetIdentity {
    /// Read a project's own package, which is where a preset's identity comes
    /// from.
    ///
    /// Both sides of the comparison are canonicalized, because on Windows a
    /// canonicalized path and the one Cargo reports can differ in their prefix and
    /// comparing two spellings of one directory has no useful answer. A preset
    /// that is a workspace member has no root package, so the member whose
    /// manifest sits in the requested directory is the one that was asked for.
    fn read(root: &Path) -> miette::Result<Self> {
        let mut command = cargo_metadata::MetadataCommand::new();
        command.no_deps().current_dir(root);
        let metadata = command
            .exec()
            .map_err(|error| miette::miette!("could not read Cargo metadata: {error}"))?;
        let wanted = std::fs::canonicalize(root)
            .map_err(|error| miette::miette!("`{}`: {error}", root.display()))?;
        let package = metadata
            .packages
            .iter()
            .find(|package| {
                package
                    .manifest_path
                    .parent()
                    .and_then(|directory| std::fs::canonicalize(directory).ok())
                    .is_some_and(|directory| directory == wanted)
            })
            .ok_or_else(|| {
                miette::miette!(
                    "`{}` holds no Cargo package; a preset is a project of its own",
                    root.display()
                )
            })?;
        let binary = package
            .targets
            .iter()
            .find(|target| target.kind.iter().any(|kind| kind.to_string() == "bin"))
            .ok_or_else(|| {
                miette::miette!(
                    "the package `{}` builds no binary, so it is not a preset",
                    package.name
                )
            })?;
        Ok(Self {
            name: package.name.as_str().to_owned(),
            version: package.version.to_string(),
            binary: binary.name.clone(),
        })
    }

    /// Refuse a build whose executable claims a different identity.
    ///
    /// The manifest is the authority. A preset that hard-codes its name, or
    /// bumps its version without bumping the manifest, would otherwise be
    /// published under a name and version no crate answers to.
    fn cross_check(&self, described: &PresetDescription) -> miette::Result<()> {
        if described.name != self.name {
            return Err(miette::miette!(
                "`{}` describes itself as `{}` but its Cargo package is named `{}`; \
                 read NAME from CARGO_PKG_NAME",
                self.binary,
                described.name,
                self.name
            ));
        }
        if described.version != self.version {
            return Err(miette::miette!(
                "`{}` describes itself as version `{}` but its Cargo package is version `{}`; \
                 read VERSION from CARGO_PKG_VERSION",
                self.binary,
                described.version,
                self.version
            ));
        }
        Ok(())
    }
}

/// Build one target and return the executable Cargo actually produced.
///
/// Cargo is asked. Every other answer is a guess about a directory layout that a
/// profile, a target directory, or a cross-compile can change.
fn build_target(
    root: &Path,
    cargo: &Path,
    binary_name: &str,
    triple: &str,
    profile: &str,
) -> miette::Result<PathBuf> {
    let mut command = Command::new(cargo);
    command
        .current_dir(root)
        .arg("build")
        .arg("--message-format=json")
        .arg("--bin")
        .arg(binary_name)
        .arg("--profile")
        .arg(profile)
        .arg("--target")
        .arg(triple);
    let output = command
        .output()
        .map_err(|error| miette::miette!("could not run cargo: {error}"))?;
    if !output.status.success() {
        return Err(miette::miette!(
            "building the preset for `{triple}` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let mut found = None;
    for line in output.stdout.split(|byte| *byte == b'\n') {
        if let Some(executable) = artifact_executable(line, binary_name) {
            found = Some(PathBuf::from(executable));
        }
    }
    found.ok_or_else(|| {
        miette::miette!(
            "cargo built nothing named `{binary_name}` for `{triple}`; a preset package has to \
             produce one executable per target it supports"
        )
    })
}

/// The executable one `compiler-artifact` message reports, if it is the one asked for.
///
/// Cargo interleaves compiler messages with build-script output and reports
/// artifacts for every target in a dependency graph, including libraries and
/// build scripts, so the reason, the kind, and the name all have to match before
/// a path is taken. Anything unparseable is skipped rather than refused: a build
/// that printed a line this does not understand still produced its artifacts.
fn artifact_executable(line: &[u8], binary_name: &str) -> Option<String> {
    let message: serde_json::Value = serde_json::from_slice(line).ok()?;
    if message["reason"] != "compiler-artifact" {
        return None;
    }
    let target = &message["target"];
    let is_executable = target["kind"]
        .as_array()
        .is_some_and(|kinds| kinds.iter().any(|kind| kind == "bin"));
    if !is_executable || target["name"] != binary_name {
        return None;
    }
    message["executable"].as_str().map(str::to_owned)
}

fn host_target() -> miette::Result<String> {
    Ok(zup_plugin_contract::HOST_TARGET.to_owned())
}

/// The project to pack, as an absolute path.
///
/// Absolute because Cargo reports absolute manifest paths, and a relative
/// `--manifest` would then be compared against a path that can never start with
/// it - which is a refusal about nothing.
fn repository_root(manifest: Option<&Path>) -> miette::Result<PathBuf> {
    let here = std::env::current_dir()
        .map_err(|error| miette::miette!("the current directory is unavailable: {error}"))?;
    let root: PathBuf = match manifest {
        Some(path) if path.is_file() => path
            .parent()
            .ok_or_else(|| miette::miette!("`{}` has no parent directory", path.display()))?
            .to_path_buf(),
        Some(path) => path.to_path_buf(),
        None => here,
    };
    root.canonicalize()
        .map_err(|error| miette::miette!("`{}`: {error}", root.display()))
}

fn cargo_executable() -> PathBuf {
    std::env::var_os("CARGO")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("cargo"))
}

fn write_durably(path: &Path, bytes: &[u8]) -> miette::Result<()> {
    let temporary = path.with_extension("zupui.tmp");
    std::fs::write(&temporary, bytes)
        .map_err(|error| miette::miette!("`{}`: {error}", temporary.display()))?;
    std::fs::rename(&temporary, path).map_err(|error| {
        let _ = std::fs::remove_file(&temporary);
        miette::miette!("`{}`: {error}", path.display())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    /// One `compiler-artifact` message as Cargo emits it.
    fn artifact(name: &str, kind: &str, executable: Option<&str>) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "reason": "compiler-artifact",
            "target": { "name": name, "kind": [kind] },
            "executable": executable,
        }))
        .expect("a message")
    }

    /// A preset's executable is found in the stream Cargo prints, among every
    /// other artifact in the build.
    #[test]
    fn cargo_artifact_discovery_finds_the_executable() {
        let message = artifact(
            "aurora-preset",
            "bin",
            Some("C:/t/release/aurora-preset.exe"),
        );
        assert_eq!(
            artifact_executable(&message, "aurora-preset").as_deref(),
            Some("C:/t/release/aurora-preset.exe")
        );
    }

    /// The last artifact for the name wins, because a build that recompiles
    /// reports the fresh one last and a stale path would package a stale preset.
    #[test]
    fn cargo_artifact_discovery_takes_the_last_executable() {
        let first = artifact("aurora-preset", "bin", Some("C:/t/old.exe"));
        let second = artifact("aurora-preset", "bin", Some("C:/t/new.exe"));
        let mut found = None;
        for line in [&first[..], &second[..]] {
            found = artifact_executable(line, "aurora-preset").or(found);
        }
        assert_eq!(found.as_deref(), Some("C:/t/new.exe"));
    }

    /// Everything else in the stream is skipped rather than refused: a build that
    /// printed a line this does not understand still produced its artifacts.
    #[rstest]
    #[case::another_binary(artifact("other-preset", "bin", Some("C:/t/other.exe")))]
    #[case::a_library(artifact("aurora-preset", "lib", Some("C:/t/libaurora.rlib")))]
    #[case::a_build_script(artifact("aurora-preset", "custom-build", Some("C:/t/build.exe")))]
    #[case::a_message_with_no_executable(artifact("aurora-preset", "bin", None))]
    #[case::a_compiler_message(br#"{"reason":"compiler-message","message":{}}"#.to_vec())]
    #[case::build_script_output(b"   Compiling aurora v1.4.2".to_vec())]
    fn cargo_artifact_discovery_ignores_everything_else(#[case] line: Vec<u8>) {
        assert_eq!(artifact_executable(&line, "aurora-preset"), None);
    }

    /// A build whose identity disagrees with its manifest is refused. A preset
    /// that hard-codes its name, or bumps its version without bumping the
    /// manifest, would otherwise be published under a name no crate answers to.
    #[test]
    fn a_preset_that_disagrees_with_its_manifest_is_refused() {
        let identity = PresetIdentity {
            name: "aurora".into(),
            version: "1.4.2".into(),
            binary: "aurora-preset".into(),
        };
        let described = PresetDescription::new("aurora", "1.4.2", serde_json::json!({}));
        identity
            .cross_check(&described)
            .expect("an executable that agrees with its manifest");

        let renamed = PresetDescription::new("borealis", "1.4.2", serde_json::json!({}));
        let refusal = identity
            .cross_check(&renamed)
            .expect_err("a different name");
        assert!(refusal.to_string().contains("borealis"), "{refusal}");

        let bumped = PresetDescription::new("aurora", "9.9.9", serde_json::json!({}));
        let refusal = identity
            .cross_check(&bumped)
            .expect_err("a different version");
        assert!(refusal.to_string().contains("9.9.9"), "{refusal}");
    }

    /// A prebuilt binary is named with the target it was built for, and a
    /// malformed one is refused before any file is opened.
    #[test]
    fn a_prebuilt_binary_is_named_by_its_target() {
        let source = parse_binary("x86_64-pc-windows-msvc=C:/build/aurora.exe")
            .expect("a triple and a path");
        assert_eq!(source.target, "x86_64-pc-windows-msvc");
        assert_eq!(source.binary, PathBuf::from("C:/build/aurora.exe"));
        assert!(
            parse_binary("C:/build/aurora.exe").is_err(),
            "a path with no target is not a binary source"
        );
        assert!(
            parse_binary("not a triple=aurora.exe").is_err(),
            "a target that is not a triple names no target"
        );
    }
}
