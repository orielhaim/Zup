use std::io::{IsTerminal as _, Write as _};
use std::path::Path;

use inquire::validator::{MinLengthValidator, Validation};
use zup_core::{Frontend, RelativePath, Target};

use crate::cli::{InitCommand, ScopePolicyArg};
use crate::project;

const TARGETS: &[(Target, &str)] = &[
    (Target::WINDOWS_X64, "Windows x64"),
    (Target::WINDOWS_X86, "Windows x86"),
    (Target::WINDOWS_ARM64, "Windows ARM64"),
    (Target::LINUX_X64, "Linux x64"),
    (Target::LINUX_ARM64, "Linux ARM64"),
    (Target::LINUX_X64_MUSL, "Linux x64 (musl)"),
    (Target::LINUX_ARM64_MUSL, "Linux ARM64 (musl)"),
];

pub fn run(args: InitCommand) -> miette::Result<()> {
    let manifest_path = if args.manifest.is_absolute() {
        args.manifest.clone()
    } else {
        std::env::current_dir()
            .map_err(|error| miette::miette!("project directory: {error}"))?
            .join(&args.manifest)
    };
    if manifest_path.exists() && !args.force {
        return Err(miette::miette!(
            "{} already exists; pass --force to replace it",
            manifest_path.display()
        ));
    }
    if !args.yes
        && !std::io::stdin().is_terminal()
        && (args.name.is_none()
            || args.app_id.is_none()
            || args.targets.is_empty()
            || args.source.is_none())
    {
        return Err(miette::miette!(
            "interactive initialization needs a terminal; pass --yes to use defaults"
        ));
    }
    let root = manifest_path.parent().expect("an absolute manifest path");
    let force = args.force;
    let answers = resolve(args, root)?;
    let document = answers.document();
    zup_manifest::parse(&document).map_err(|error| miette::miette!("{error}"))?;

    std::fs::create_dir_all(root).map_err(|error| miette::miette!("project directory: {error}"))?;
    std::fs::create_dir_all(root.join(&answers.source))
        .map_err(|error| miette::miette!("source directory: {error}"))?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(force)
        .truncate(force)
        .create_new(!force)
        .open(&manifest_path)
        .map_err(|error| miette::miette!("create manifest: {error}"))?;
    file.write_all(document.as_bytes())
        .map_err(|error| miette::miette!("write manifest: {error}"))?;
    println!("Created {}", manifest_path.display());
    println!(
        "Next: review {}, then run zup check",
        manifest_path.display()
    );
    Ok(())
}

struct Answers {
    name: String,
    app_id: String,
    version: String,
    targets: Vec<Target>,
    source: String,
    main: Option<String>,
    scope: ScopePolicyArg,
    frontend: Frontend,
}

fn resolve(args: InitCommand, root: &Path) -> miette::Result<Answers> {
    semver::Version::parse(&args.version).map_err(|error| miette::miette!("version: {error}"))?;
    let interactive = !args.yes;
    let directory_name = root
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("app");
    let name = text(args.name, "Application name", directory_name, interactive)?;
    let app_id = text(
        args.app_id,
        "Application ID",
        &format!("com.example.{}", project::slug(&name)),
        interactive,
    )?;
    let targets = if args.targets.is_empty() {
        let host = Target::host().map_err(|error| miette::miette!("{error}"))?;
        if interactive {
            let default = TARGETS
                .iter()
                .position(|(target, _)| *target == host)
                .unwrap();
            inquire::MultiSelect::new(
                "Target platforms",
                TARGETS.iter().map(|(_, label)| *label).collect(),
            )
            .with_default(&[default])
            .without_filtering()
            .with_help_message("Space to select, arrows to move, Enter to continue")
            .with_validator(MinLengthValidator::new(1).with_message("Select at least one target"))
            .raw_prompt()
            .map_err(|error| miette::miette!("prompt: {error}"))?
            .into_iter()
            .map(|option| TARGETS[option.index].0)
            .collect()
        } else {
            vec![host]
        }
    } else {
        let mut targets = Vec::new();
        for target in args.targets {
            if !targets.contains(&target) {
                targets.push(target);
            }
        }
        targets
    };
    let source = match args.source {
        Some(source) => source.trim().to_owned(),
        None => {
            let default = match ["dist", "build", "out"]
                .into_iter()
                .find(|name| root.join(name).is_dir())
            {
                Some(source) => source,
                None if discover_main(root, &targets)?.is_some() => ".",
                None => "dist",
            };
            text(None, "Source directory", default, interactive)?
        }
    };
    let main = match args.main {
        Some(main) => {
            let main = RelativePath::new(main)
                .map_err(|error| miette::miette!("main executable: {error}"))?;
            if !root.join(&source).join(main.as_str()).is_file() {
                return Err(miette::miette!(
                    "main executable `{main}` does not exist in `{source}`"
                ));
            }
            Some(main.to_string())
        }
        None => discover_main(&root.join(&source), &targets)?,
    };
    Ok(Answers {
        name,
        app_id,
        version: args.version,
        targets,
        source,
        main,
        scope: args.scope.unwrap_or(ScopePolicyArg::User),
        frontend: args.frontend.map(Into::into).unwrap_or_default(),
    })
}

fn text(
    value: Option<String>,
    message: &str,
    default: &str,
    interactive: bool,
) -> miette::Result<String> {
    let value = match value {
        Some(value) => value,
        None if interactive => inquire::Text::new(message)
            .with_initial_value(default)
            .with_validator(|value: &str| {
                Ok(if value.trim().is_empty() {
                    Validation::Invalid("Enter a value".into())
                } else {
                    Validation::Valid
                })
            })
            .prompt()
            .map_err(|error| miette::miette!("prompt: {error}"))?,
        None => default.to_owned(),
    };
    Ok(value.trim().to_owned())
}

fn discover_main(source: &Path, targets: &[Target]) -> miette::Result<Option<String>> {
    let entries = match std::fs::read_dir(source) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(miette::miette!(
                "source directory {}: {error}",
                source.display()
            ));
        }
    };
    let mut main = None;
    for entry in entries {
        let entry = entry.map_err(|error| miette::miette!("source directory: {error}"))?;
        let path = entry.path();
        if !path.is_file()
            || !path
                .extension()
                .is_none_or(|ext| ext.eq_ignore_ascii_case("exe"))
        {
            continue;
        }
        let Ok(binary) = zup_binary::Executable::read(&path) else {
            continue;
        };
        if !targets.iter().all(|target| {
            binary.matches_target(target)
                && matches!(
                    (binary.format(), target.is_windows()),
                    (zup_binary::BinaryFormat::Pe, true) | (zup_binary::BinaryFormat::Elf, false)
                )
        }) {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if main.is_some() {
            return Ok(None);
        }
        main = Some(name);
    }
    Ok(main)
}

impl Answers {
    fn document(&self) -> String {
        let scope = match self.scope {
            ScopePolicyArg::User => "user",
            ScopePolicyArg::Machine => "machine",
            ScopePolicyArg::Either => "either",
        };
        let mut document = format!(
            "#:schema https://zup.dev/schema/zup.toml.json\n\nschema = {}\nfrontend = {}\n\n[app]\nid = {}\nname = {}\nversion = {}\n",
            zup_manifest::SCHEMA_VERSION,
            project::toml_string(self.frontend.as_str()),
            project::toml_string(&self.app_id),
            project::toml_string(&self.name),
            project::toml_string(&self.version),
        );
        if let Some(main) = &self.main {
            document.push_str(&format!("main = {}\n", project::toml_string(main)));
        }
        for target in &self.targets {
            document.push_str(&format!(
                "\n[build.targets.{target}]\nsource = {{ directory = {} }}\n",
                project::toml_string(&self.source),
            ));
        }
        document.push_str(&format!("\n[install]\nscope = \"{scope}\"\nallow_directory_override = true\n\n[install.directory]\n"));
        let install_name = project::slug(&self.name);
        if self.scope != ScopePolicyArg::Machine {
            document.push_str(&format!(
                "user = \"${{location.user_data}}/{install_name}\"\n"
            ));
        }
        if self.scope != ScopePolicyArg::User {
            document.push_str(&format!(
                "machine = \"${{location.programs}}/{install_name}\"\n"
            ));
        }
        document
    }
}
