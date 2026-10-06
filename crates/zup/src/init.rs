//! `zup init`: a project a person can read.
//!
//! The generated manifest is the first thing a new user reads about this tool, so
//! it is short, commented by example rather than by prose, and it names one
//! profile. Everything a real project needs beyond that is a decision the author
//! should make rather than accept.

use std::io::IsTerminal as _;
use std::path::{Path, PathBuf};

use crate::cli::{InitCommand, ScopeArg};
use crate::project;

pub fn run(args: InitCommand) -> miette::Result<()> {
    let manifest_path = absolute(&args.manifest);
    if manifest_path.exists() && !args.force {
        return Err(miette::miette!(
            "{} already exists; pass --force to replace it",
            manifest_path.display()
        ));
    }
    let interactive = !args.non_interactive && std::io::stdin().is_terminal();
    let answers = ask(args, &manifest_path, interactive)?;

    let scope_name = match answers.scope {
        ScopeArg::Machine => "machine",
        ScopeArg::Either => "either",
        ScopeArg::User => "user",
    };
    let install_name = project::slug(&answers.name);
    let frontend = answers.frontend;
    let target = crate::build_inputs::default_build_target();
    let mut document = format!(
        "#:schema https://zup.dev/schema/zup.toml.json\n\n\
         schema = {}\nfrontend = {}\n\n\
         [app]\nid = {}\nname = {}\nversion = {}\nmain = {}\n\n\
         [build]\n\n\
         [build.targets.default]\ntarget = {}\nsource = {{ directory = {} }}\n\n\
         [install]\nscope = {}\nallow_directory_override = true\n\n\
         [install.directory]\n",
        zup_manifest::SCHEMA_VERSION,
        project::toml_string(frontend.as_str()),
        project::toml_string(&answers.app_id),
        project::toml_string(&answers.name),
        project::toml_string(&answers.version),
        project::toml_string(&answers.main),
        project::toml_string(&target),
        project::toml_string(&answers.source),
        project::toml_string(scope_name),
    );
    if answers.scope != ScopeArg::Machine {
        document.push_str(&format!(
            "user = \"${{location.user_data}}/{install_name}\"\n"
        ));
    }
    if answers.scope != ScopeArg::User {
        document.push_str(&format!(
            "machine = \"${{location.programs}}/{install_name}\"\n"
        ));
    }
    if let Some(parent) = manifest_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| miette::miette!("project directory: {error}"))?;
        let source = Path::new(&answers.source);
        let source = if source.is_absolute() {
            source.to_path_buf()
        } else {
            parent.join(source)
        };
        std::fs::create_dir_all(&source)
            .map_err(|error| miette::miette!("source directory: {error}"))?;
    }
    std::fs::write(&manifest_path, document)
        .map_err(|error| miette::miette!("write manifest: {error}"))?;
    println!("Created {}", manifest_path.display());
    println!("Next: edit zup.toml, then run zup check");
    Ok(())
}

/// The answers `zup init` works from, whether it asked or was told.
struct Answers {
    name: String,
    app_id: String,
    version: String,
    source: String,
    main: String,
    scope: ScopeArg,
    frontend: zup_core::Frontend,
}

/// Ask for whatever was not supplied, then require the rest.
///
/// A non-interactive run is a script, and a script that did not say what the
/// application is called cannot be answered for it. Guessing `app` would produce
/// a project whose identity nobody chose.
///
/// The defaults that depend on the machine are the build host's, so a project
/// created on Linux builds on Linux: the default target comes from the same
/// function the build reads, the default main executable carries no Windows
/// suffix there, and the default frontend is one the host's backend ships.
fn ask(args: InitCommand, manifest_path: &Path, interactive: bool) -> miette::Result<Answers> {
    let directory_name = manifest_path
        .parent()
        .and_then(|path| path.file_name())
        .and_then(|name| name.to_str())
        .unwrap_or("app")
        .to_owned();
    let mut name = args.name;
    let mut app_id = args.app_id;
    let mut source = args.source;
    let mut scope = args.scope;
    let mut main = args.main;
    if interactive {
        if name.is_none() {
            name = Some(
                inquire::Text::new("Application name")
                    .with_default(&directory_name)
                    .prompt()
                    .map_err(|error| miette::miette!("prompt: {error}"))?,
            );
        }
        if app_id.is_none() {
            let default_id = format!(
                "com.example.{}",
                project::slug(name.as_deref().unwrap_or("app"))
            );
            app_id = Some(
                inquire::Text::new("Application ID")
                    .with_default(&default_id)
                    .prompt()
                    .map_err(|error| miette::miette!("prompt: {error}"))?,
            );
        }
        if source.is_none() {
            source = Some(
                inquire::Text::new("Source directory")
                    .with_default("dist")
                    .prompt()
                    .map_err(|error| miette::miette!("prompt: {error}"))?,
            );
        }
        if scope.is_none() {
            let value = inquire::Select::new("Install scope", vec!["user", "machine", "either"])
                .with_starting_cursor(0)
                .prompt()
                .map_err(|error| miette::miette!("prompt: {error}"))?;
            scope = Some(match value {
                "machine" => ScopeArg::Machine,
                "either" => ScopeArg::Either,
                _ => ScopeArg::User,
            });
        }
        if main.is_none() {
            main = Some(
                inquire::Text::new("Main executable")
                    .with_default(default_main())
                    .prompt()
                    .map_err(|error| miette::miette!("prompt: {error}"))?,
            );
        }
    }
    let name = name.ok_or_else(|| miette::miette!("--name is required in non-interactive mode"))?;
    let app_id =
        app_id.ok_or_else(|| miette::miette!("--app-id is required in non-interactive mode"))?;
    semver::Version::parse(&args.version).map_err(|error| miette::miette!("version: {error}"))?;
    Ok(Answers {
        name,
        app_id,
        version: args.version,
        source: source.unwrap_or_else(|| "dist".into()),
        main: main.unwrap_or_else(|| default_main().to_owned()),
        scope: scope.unwrap_or(ScopeArg::User),
        frontend: args
            .frontend
            .map(Into::into)
            .unwrap_or_else(default_frontend),
    })
}

/// The default main executable on this build host: a native executable name,
/// which carries a suffix on Windows and none elsewhere.
fn default_main() -> &'static str {
    if cfg!(windows) { "app.exe" } else { "app" }
}

/// The default installer frontend on this build host: the Linux backend ships
/// no GUI runtime, so a project created there starts with the console
/// installer its backend can build.
fn default_frontend() -> zup_core::Frontend {
    if cfg!(target_os = "linux") {
        zup_core::Frontend::Console
    } else {
        zup_core::Frontend::default()
    }
}

fn absolute(manifest: &Path) -> PathBuf {
    if manifest.is_absolute() {
        manifest.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(manifest)
    }
}
