use std::path::{Path, PathBuf};
use std::process::Command;

use clap::{Args, Subcommand, ValueHint};
use zup_artifact::preset::{PresetPackageView, PresetPackageWriter};
use zup_core::TargetTriple;
use zup_preset_protocol::{MAX_DESCRIBE_BYTES, PresetDescription};

use crate::failure;

#[derive(Debug, Args, Clone)]
pub struct BinarySource {
    #[arg(long, value_name = "TRIPLE")]
    pub target: String,
    #[arg(long, value_name = "FILE", value_hint = ValueHint::FilePath)]
    pub binary: PathBuf,
}

#[derive(Debug, Args)]
pub struct PresetCommand {
    #[command(subcommand)]
    pub command: PresetVerb,
}

#[derive(Debug, Subcommand)]
pub enum PresetVerb {
    Init(InitCommand),
    Dev(DevCommand),
    Pack(PackCommand),
    Inspect(InspectCommand),
}

#[derive(Debug, Args)]
pub struct InitCommand {
    pub name: String,
    #[arg(long, value_name = "DIR", value_hint = ValueHint::DirPath)]
    pub directory: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct DevCommand {
    #[arg(long, value_name = "DIR", value_hint = ValueHint::DirPath)]
    pub project: Option<PathBuf>,
    #[arg(long, value_name = "PROFILE", default_value = "dev")]
    pub profile: String,
}

pub fn dev(args: &DevCommand) -> miette::Result<()> {
    let _lines = std::io::LineWriter::new(std::io::stdout());
    let root = match args.project.clone() {
        Some(project) => project,
        None => std::env::current_dir()
            .map_err(|error| failure::error("zup.preset.dev_cwd", error.to_string()))?,
    };
    zup_preset_dev::develop(root, &args.profile)
        .map_err(|error| failure::error("zup.preset.dev", error.to_string()))
}

pub fn generate(args: &InitCommand) -> miette::Result<()> {
    let parent = match args.directory.clone() {
        Some(directory) => directory,
        None => std::env::current_dir()
            .map_err(|error| failure::error("zup.preset.init_cwd", error.to_string()))?,
    };
    init::init(&args.name, &parent)
}

#[derive(Debug, Args)]
pub struct PackCommand {
    #[arg(long, value_name = "DIR", value_hint = ValueHint::DirPath)]
    pub manifest: Option<PathBuf>,
    #[arg(long, short, value_name = "FILE", value_hint = ValueHint::FilePath)]
    pub output: Option<PathBuf>,
    #[arg(long = "binary", value_name = "TRIPLE=FILE", value_parser = parse_binary, num_args = 1)]
    pub binaries: Vec<BinarySource>,
    #[arg(long = "build", value_name = "TRIPLE")]
    pub build: Vec<String>,
    #[arg(long, value_name = "PROFILE", default_value = "release")]
    pub profile: String,
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Args)]
pub struct InspectCommand {
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

pub fn pack(args: &PackCommand) -> miette::Result<()> {
    let root = repository_root(args.manifest.as_deref())?;
    let cargo = crate::project::cargo_executable();
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
    println!("preset protocol   {}", package.ui_protocol);
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
        .arg(zup_preset_protocol::DESCRIBE_FLAG)
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

struct PresetIdentity {
    name: String,
    version: String,
    binary: String,
}

impl PresetIdentity {
    fn read(root: &Path) -> miette::Result<Self> {
        let package = crate::project::own_package(root)?;
        let binary = crate::project::target_of_kind(&package, "bin")?;
        Ok(Self {
            name: package.name.as_str().to_owned(),
            version: package.version.to_string(),
            binary: binary.name.clone(),
        })
    }

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

/// `--manifest` would then be compared against a path that can never start with
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

fn write_durably(path: &Path, bytes: &[u8]) -> miette::Result<()> {
    let temporary = path.with_extension("zup-tmp");
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

    fn artifact(name: &str, kind: &str, executable: Option<&str>) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "reason": "compiler-artifact",
            "target": { "name": name, "kind": [kind] },
            "executable": executable,
        }))
        .expect("a message")
    }

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

pub mod init {
    use std::path::{Path, PathBuf};

    use super::failure;

    pub struct Generator {
        name: String,
        root: PathBuf,
        write_code: &'static str,
    }

    impl Generator {
        pub fn new(raw: &str, parent: &Path, code: &'static str) -> Result<Self, String> {
            let name = package_name(raw)?;
            let root = parent.join(&name);
            if root.exists() {
                return Err(format!("`{}` already exists", root.display()));
            }
            Ok(Self {
                name,
                root,
                write_code: code,
            })
        }

        pub fn name(&self) -> &str {
            &self.name
        }

        pub fn root(&self) -> &Path {
            &self.root
        }

        pub fn create(self, files: &[(&str, String)]) -> miette::Result<()> {
            for (relative, contents) in files {
                self.write(relative, contents)?;
            }
            Ok(())
        }

        fn write(&self, relative: &str, contents: &str) -> miette::Result<()> {
            let path = self.root.join(relative);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|error| self.write_failed(parent, error))?;
            }
            std::fs::write(&path, contents).map_err(|error| self.write_failed(&path, error))
        }

        fn write_failed(&self, path: &Path, error: std::io::Error) -> miette::Report {
            failure::error(self.write_code, format!("{}: {error}", path.display()))
        }
    }

    fn package_name(raw: &str) -> Result<String, String> {
        let name: String = raw
            .trim()
            .to_owned()
            .to_ascii_lowercase()
            .chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() || character == '-' || character == '_' {
                    character
                } else {
                    '-'
                }
            })
            .collect();
        let name = name.trim_matches('-').to_owned();
        if name.is_empty() {
            return Err(format!("`{raw}` names nothing a package could be called"));
        }
        if name.starts_with(|character: char| character.is_ascii_digit()) {
            return Err(format!(
                "a package cannot be called `{name}`; it starts with a digit"
            ));
        }
        Ok(name)
    }

    pub fn init(name: &str, parent: &Path) -> miette::Result<()> {
        let project = Generator::new(name, parent, "zup.preset.init")
            .map_err(|error| failure::error("zup.preset.init_name", error))?;
        let name = project.name().to_owned();
        project.create(&[
            ("Cargo.toml", manifest(&name)),
            ("src/main.rs", MAIN.to_owned()),
            ("zup.preset.dev.toml", DEVELOPMENT.to_owned()),
            (".gitignore", GITIGNORE.to_owned()),
        ])
    }

    fn manifest(name: &str) -> String {
        format!(
            r#"[package]
name = "{name}"
version = "0.1.0"
edition = "2024"
description = "A zup installer preset"
publish = false

# The whole of what a preset needs. The GPUI stack this builds against, and the
# crates its settings are built from, are the ones this SDK was built with.
[dependencies]
zup-sdk = {{ version = "0.1.0", features = ["preset"] }}

# GPUI is a very large dependency tree, and compiling it at `opt-level = 0` is
# both slow and, for a text and layout engine, surprisingly slow at run time.
# These are the packages that dominate the cost; everything else stays at the
# development default so a change to this preset's own code is compiled in
# seconds.
#
# A name here that the GPUI stack has since renamed is a warning rather than an
# error, so it costs nothing but the optimisation it was buying. Cargo prints it
# on the first build, which is where it is worth knowing about.
[profile.dev.package.gpui-pre]
opt-level = 2
[profile.dev.package.gpui-pre-platform]
opt-level = 2
[profile.dev.package.gpui-pre-shared-string]
opt-level = 2
[profile.dev.package.gpui-pre-scheduler]
opt-level = 2
[profile.dev.package.gpui-pre-refineable]
opt-level = 2
[profile.dev.package.gpui-pre-derive-refineable]
opt-level = 2
[profile.dev.package.gpui-pre-macros]
opt-level = 2
[profile.dev.package.gpui-pre-util]
opt-level = 2
[profile.dev.package.gpui-pre-util-macros]
opt-level = 2
[profile.dev.package.gpui-base]
opt-level = 2
[profile.dev.package.gpui-component]
opt-level = 2
[profile.dev.package.gpui-kit-assets]
opt-level = 2
[profile.dev.package.taffy]
opt-level = 2
[profile.dev.package.smol_str]
opt-level = 2
[profile.dev.package.rustybuzz]
opt-level = 2
[profile.dev.package.resvg]
opt-level = 2
[profile.dev.package.usvg]
opt-level = 2
[profile.dev.package.tiny-skia]
opt-level = 2
[profile.dev.package.image]
opt-level = 2
[profile.dev.package.zune-jpeg]
opt-level = 2
[profile.dev.package.png]
opt-level = 2
[profile.dev.package.smol]
opt-level = 2
"#
        )
    }

    const MAIN: &str = r#"use zup_sdk::preset::prelude::*;

#[zup_sdk::preset::settings]
pub struct Settings {
    pub hero: Option<String>,
    pub logo: Option<AssetRef>,
    pub accent: Option<String>,
}

struct Aurora;

impl Preset for Aurora {
    const NAME: &'static str = env!("CARGO_PKG_NAME");
    const VERSION: &'static str = env!("CARGO_PKG_VERSION");

    type Settings = Settings;

    fn launch(context: PresetContext<Self::Settings>, cx: &mut App) {
        let session = context.session().clone();
        let settings = context.settings().clone();
        let state = session.state();

        gpui::open_window(gpui::WindowOptions::default(), cx, move |_window, cx| {
            let view = cx.new(|_| View {
                session: session.clone(),
                state: state.clone(),
                settings: settings.clone(),
                _state: Subscription::new(|| {}),
                _settings: Subscription::new(|| {}),
            });
            let refreshed = view.clone();
            view.update(cx, |view, cx| {
                view._state = cx.observe(&state, |_, _, cx| cx.notify());
                view._settings = settings.observe(cx, move |_, cx| {
                    refreshed.update(cx, |_, cx| cx.notify());
                });
            });
            view
        })
        .expect("open the installer window");
    }
}

struct View {
    session: Session,
    state: Entity<SessionState>,
    settings: PresetSettings<Settings>,
    _state: Subscription,
    _settings: Subscription,
}

impl Render for View {
    fn render(&mut self, _window: &mut gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
        use gpui::base::{Disableable, StyledExt};
        use gpui::component::button::Button;
        use gpui::component::{ActiveTheme, Theme};
        use gpui::{FontWeight, ParentElement, Styled, div, px};

        let theme: &Theme = cx.theme();
        let body = div()
            .v_flex()
            .gap_4()
            .p_6()
            .bg(theme.colors.background)
            .text_color(theme.colors.foreground);

        let Some(snapshot) = self.state.read(cx).snapshot() else {
            return body.child("Waiting for the installer…");
        };

        let mut column = body
            .child(
                div()
                    .text_size(px(21.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(snapshot.product.name.clone()),
            )
            .child(
                div()
                    .text_size(px(13.0))
                    .text_color(theme.colors.muted_foreground)
                    .child(self.settings.read(cx).hero.clone().unwrap_or_default()),
            );

        for component in snapshot.surface.components() {
            let action = Action::SetComponent {
                component: component.id.clone(),
                selected: !component.selected,
            };
            let session = self.session.clone();
            column = column.child(
                Button::new(component.id.to_string())
                    .label(format!(
                        "{} {}",
                        if component.selected { "[x]" } else { "[ ]" },
                        component.name
                    ))
                    .disabled(component.required)
                    .on_click(move |_, _, _| session.send(action.clone())),
            );
        }

        let session = self.session.clone();
        column.child(
            Button::new("install")
                .label("Install")
                .on_click(move |_, _, _| session.send(Action::Install)),
        )
    }
}

fn main() {
    if let Err(error) = zup_sdk::preset::run::<Aurora>() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
"#;

    const DEVELOPMENT: &str = r##"# The application `zup preset dev` presents.
#
# Settings are validated against the schema your `Settings` type generates, and
# an invalid one leaves the last valid settings in force, so a typo here cannot
# empty a window that is working. Nothing in this file is a preset format and
# none of it is read by a build.
[settings]
hero = "Install Acme"
accent = "#695cff"

# Application-provided assets, by the name the settings above refer to. Editing
# one of these files updates the running preset; it does not recompile anything.
[assets]
"branding/logo.svg" = "assets/logo.svg"
"##;

    const GITIGNORE: &str = r#"/target
# A development environment's own state: run executables and the files it
# materialized for the simulated application. Disposable by construction.
/.zup
"#;
}
