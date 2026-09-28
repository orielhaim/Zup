//! Command line for the zup repository tasks.

use std::path::PathBuf;
use std::process::ExitCode;

use zup_xtask::boundary;
use zup_xtask::matrix::{self, Matrix};
use zup_xtask::pins;

const USAGE: &str = "\
xtask emit-portable-matrix [--matrix <name>]... [--format <text|cargo-args>]
    Print the package matrices. --format cargo-args prints `-p <package>`
    arguments for cargo and needs exactly one --matrix.

xtask verify-portable-boundaries [--root <dir>]
    Report every way a portable package depends on Windows, reintroduces a
    Windows-specific identifier, or spells a Windows concept in a string
    literal.

xtask github-action-pins check [--root <dir>] [--online]
    Check github-actions.lock.json: syntax, version and SHA agreement, that
    every action a generated workflow needs is present, and that no committed
    workflow uses a `uses:` the lock does not name. Offline unless --online,
    which additionally reports newer stable releases.

xtask github-action-pins refresh [--root <dir>] [--add <owner/name>]...
    Resolve each pinned action's newest stable release through GitHub and
    rewrite the lock. Never runs inside a build. Bun and the action's toolchain
    are pinned here too, because a runner that installs a different Bun produces
    a bundle nobody can reproduce.

xtask toolchain build [--profile <name>]
    Build the runtime templates and dispatchers this repository produces, and
    stage them beside `zup` with the descriptors the toolchain resolver checks.
    Run it once per profile; a contributor running `cargo test` or `cargo run`
    needs the debug profile, which is the default.

xtask toolchain package [--profile <name>] [--out <dir>]
    Assemble one directory that is a complete zup release: `zup`, the toolchain
    for this exact version, and `zup-toolchain.json` naming every file with its
    digest. Unzip it and `zup build` works; there is no second download.

xtask release clean-room [--material <dir>] [--work <dir>]
    Prove the released zup works from outside this repository. Verifies the
    release index, creates an empty project directory with a scrubbed
    environment, and runs `zup init`, `check`, `doctor` and `build` using only
    the release material. Asserts the artifact is a real Portable Executable and
    that nothing in the release description names the machine that built it.

xtask verify-dependency-graph [--root <dir>]
    Refuse a dependency graph that grew by accident. Fails when a workspace
    package reaches two versions of one external crate, and when development
    tooling has reached the graph of a binary that ships to users.

xtask automation generate [--root <dir>]
    Write the artifacts derived from the automation contract: the JSON Schema, the
    Action's TypeScript declarations, and the golden protocol fixtures. Never runs
    inside a build.

xtask automation check [--root <dir>]
    Report every generated file that no longer matches the Rust types it came from.
    This is the CI gate; `generate` is the fix.

options:
    --root <dir>         workspace to inspect (default: this repository)
    --online             reach GitHub to report newer releases
    --add <owner/name>   add an action to the lock before refreshing
    --profile <name>     cargo profile to build and stage beside (default: dev)
    --out <dir>          where to write the packaged release
    --material <dir>     release material to test (default: target/release-material/<version>)
    --work <dir>         an empty directory to run in (default: a fresh temp directory)

exit codes:
    0  clean
    1  problems found
    2  usage or unreadable workspace
";

/// Every option this tool understands.
const OPTIONS: &[&str] = &[
    "--root",
    "--matrix",
    "--format",
    "--online",
    "--add",
    "--profile",
    "--out",
    "--material",
    "--work",
];

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Format {
    #[default]
    Text,
    CargoArgs,
}

#[derive(Default)]
struct Options {
    root: Option<PathBuf>,
    matrices: Vec<String>,
    format: Format,
    online: bool,
    add: Vec<String>,
    profile: Option<String>,
    out: Option<PathBuf>,
    material: Option<PathBuf>,
    work: Option<PathBuf>,
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(message) => {
            eprintln!("xtask: {message}");
            ExitCode::from(2)
        }
    }
}

fn run() -> Result<ExitCode, String> {
    let mut arguments = std::env::args().skip(1);
    let Some(command) = arguments.next() else {
        return Err(format!("no command given\n\n{USAGE}"));
    };
    match command.as_str() {
        "help" | "-h" | "--help" => {
            print!("{USAGE}");
            Ok(ExitCode::SUCCESS)
        }
        "emit-portable-matrix" => emit(&mut arguments),
        "verify-portable-boundaries" => verify(&mut arguments),
        "github-action-pins" => action_pins(&mut arguments),
        "toolchain" => stage_toolchain(&mut arguments),
        "verify-dependency-graph" => dependency_graph(&mut arguments),
        "automation" => automation(&mut arguments),
        "release" => release(&mut arguments),
        unknown => Err(format!("unknown command `{unknown}`\n\n{USAGE}")),
    }
}

fn emit(arguments: &mut impl Iterator<Item = String>) -> Result<ExitCode, String> {
    let options = parse(
        arguments,
        "emit-portable-matrix",
        &["--root", "--matrix", "--format"],
    )?;
    let selected = select(&options.matrices)?;
    if options.format == Format::CargoArgs {
        if selected.len() != 1 {
            return Err("--format cargo-args needs exactly one --matrix".to_owned());
        }
        println!("{}", matrix::render_cargo_args(selected[0]));
        return Ok(ExitCode::SUCCESS);
    }
    print!("{}", matrix::render(&selected));
    Ok(ExitCode::SUCCESS)
}

fn verify(arguments: &mut impl Iterator<Item = String>) -> Result<ExitCode, String> {
    let options = parse(arguments, "verify-portable-boundaries", &["--root"])?;
    let root = match options.root {
        Some(root) => root,
        None => repository_root(),
    };
    let violations = boundary::check_workspace(&root)?;
    if violations.is_empty() {
        return Ok(ExitCode::SUCCESS);
    }
    eprintln!("xtask: {}", count(violations.len()));
    for violation in &violations {
        eprintln!("  {violation}");
    }
    Ok(ExitCode::from(1))
}

fn action_pins(arguments: &mut impl Iterator<Item = String>) -> Result<ExitCode, String> {
    let Some(subcommand) = arguments.next() else {
        return Err("github-action-pins needs `check` or `refresh`\n\n".to_owned() + USAGE);
    };
    let allowed: &[&str] = match subcommand.as_str() {
        "check" => &["--root", "--online"],
        "refresh" => &["--root", "--add"],
        unknown => {
            return Err(format!(
                "unknown github-action-pins subcommand `{unknown}`\n\n{USAGE}"
            ));
        }
    };
    let options = parse(
        arguments,
        &format!("github-action-pins {subcommand}"),
        allowed,
    )?;
    let root = match options.root {
        Some(root) => root,
        None => repository_root(),
    };

    let report = match subcommand.as_str() {
        "refresh" => pins::refresh(&root, &options.add).map_err(|error| error.to_string())?,
        _ => pins::check(&root, options.online),
    };

    // Problems go to stderr, because that is where a failing build's diagnostics
    // are read, and because a caller piping stdout to a file should not have a
    // list of failures silently written into it.
    if report.is_clean() {
        print!("{}", pins::render_report(&report));
        println!("github-action-pins: {}", report.detail);
        return Ok(ExitCode::SUCCESS);
    }
    eprint!("{}", pins::render_report(&report));
    eprintln!("github-action-pins: {}", report.detail);
    Ok(ExitCode::from(1))
}

/// Build the local toolchain a contributor's `zup build` composes from, or
/// assemble it into a directory a developer can unzip and use.
fn stage_toolchain(arguments: &mut impl Iterator<Item = String>) -> Result<ExitCode, String> {
    let Some(subcommand) = arguments.next() else {
        return Err("toolchain needs `build` or `package`\n\n".to_owned() + USAGE);
    };
    let allowed: &[&str] = match subcommand.as_str() {
        "build" => &["--root", "--profile"],
        "package" => &["--root", "--profile", "--out"],
        unknown => {
            return Err(format!(
                "unknown toolchain subcommand `{unknown}`\n\n{USAGE}"
            ));
        }
    };
    let options = parse(arguments, &format!("toolchain {subcommand}"), allowed)?;
    let root = options
        .root
        .unwrap_or_else(zup_xtask::toolchain::repository_root);
    let version = zup_xtask::toolchain::version()?;
    let profile = options.profile.unwrap_or_else(|| "dev".to_owned());

    if subcommand == "package" {
        let out = options
            .out
            .unwrap_or_else(|| root.join("target").join("release-material").join(&version));
        println!("Packaging the zup {version} release ({profile})");
        let written = zup_xtask::toolchain::package(&root, &profile, &out)?;
        let components = std::fs::read_dir(written.join("toolchain").join(&version))
            .map(|entries| {
                entries
                    .flatten()
                    .filter(|entry| entry.path().is_file())
                    .count()
            })
            .unwrap_or(0);
        println!();
        println!("Release material in {}", written.display());
        println!("  zup + {components} toolchain files");
        println!("  {}", zup_xtask::toolchain::RELEASE_INDEX_NAME);
        println!("\nUnzip it anywhere and run `zup build`: there is nothing else to install.");
        return Ok(ExitCode::SUCCESS);
    }

    println!("Building the zup {version} toolchain ({profile})");
    let written = zup_xtask::toolchain::build(&root, &profile)?;
    let staged = zup_xtask::toolchain::staging_directory(&root, &profile, &version);
    println!();
    println!(
        "Staged {} components in {}",
        written.len(),
        staged.display()
    );
    println!("`zup build` will find them without being told where they are.");
    Ok(ExitCode::SUCCESS)
}

/// Refuse a dependency graph that grew by accident.
fn dependency_graph(arguments: &mut impl Iterator<Item = String>) -> Result<ExitCode, String> {
    let options = parse(arguments, "verify-dependency-graph", &["--root"])?;
    let root = match options.root {
        Some(root) => root,
        None => repository_root(),
    };
    let findings = zup_xtask::graph::check(&root)?;
    if findings.is_clean() {
        println!("verify-dependency-graph: clean");
        return Ok(ExitCode::SUCCESS);
    }
    // Every finding is printed, not a count. A count tells a reviewer that
    // something is wrong; the offending edge is what they can act on.
    for duplicate in &findings.duplicates {
        eprintln!("xtask: {duplicate}");
    }
    for intrusion in &findings.intrusions {
        eprintln!("xtask: {intrusion}");
    }
    eprintln!(
        "xtask: {} finding(s)",
        findings.duplicates.len() + findings.intrusions.len()
    );
    Ok(ExitCode::from(1))
}

/// Generate the automation contract's derived files, or report that they have drifted.
fn automation(arguments: &mut impl Iterator<Item = String>) -> Result<ExitCode, String> {
    let Some(subcommand) = arguments.next() else {
        return Err("automation needs `generate` or `check`\n\n".to_owned() + USAGE);
    };
    let subcommand = match subcommand.as_str() {
        "generate" => "automation generate",
        "check" => "automation check",
        unknown => {
            return Err(format!(
                "unknown automation subcommand `{unknown}`\n\n{USAGE}"
            ));
        }
    };
    let options = parse(arguments, subcommand, &["--root"])?;
    let root = options
        .root
        .unwrap_or_else(zup_xtask::toolchain::repository_root);
    let files = zup_xtask::automation::generate();
    if subcommand == "automation generate" {
        let written = zup_xtask::automation::write(&root, &files)?;
        for path in written {
            println!("wrote {path}");
        }
        return Ok(ExitCode::SUCCESS);
    }
    let stale = zup_xtask::automation::drift(&root, &files);
    if stale.is_empty() {
        println!("automation: {} generated file(s) are current", files.len());
        return Ok(ExitCode::SUCCESS);
    }
    for path in stale {
        eprintln!("xtask: {path}");
    }
    eprintln!(
        "xtask: {} generated file(s) no longer match the Rust types; \
         run `cargo xtask automation generate`",
        files.len()
    );
    Ok(ExitCode::from(1))
}

/// Prove a packaged release works from outside this repository.
fn release(arguments: &mut impl Iterator<Item = String>) -> Result<ExitCode, String> {
    let Some(subcommand) = arguments.next() else {
        return Err("release needs `clean-room`\n\n".to_owned() + USAGE);
    };
    let allowed: &[&str] = match subcommand.as_str() {
        "clean-room" => &["--root", "--material", "--work"],
        unknown => return Err(format!("unknown release subcommand `{unknown}`\n\n{USAGE}")),
    };
    let options = parse(arguments, &format!("release {subcommand}"), allowed)?;
    let root = options
        .root
        .unwrap_or_else(zup_xtask::toolchain::repository_root);
    let version = zup_xtask::toolchain::version()?;
    let material = options
        .material
        .unwrap_or_else(|| root.join("target").join("release-material").join(&version));
    // A fresh directory every run: the assertion that matters is that the
    // project starts with nothing in it, and reusing one would test whatever the
    // last run left behind.
    let work = options
        .work
        .unwrap_or_else(|| std::env::temp_dir().join(format!("zup-clean-room-{version}")));
    if work.exists() {
        std::fs::remove_dir_all(&work).map_err(|error| format!("{}: {error}", work.display()))?;
    }
    println!("Clean-room run");
    println!("  material  {}", material.display());
    println!("  work      {}", work.display());
    let outcome = zup_xtask::cleanroom::run(&material, &work)?;
    println!();
    println!("  {}", outcome.readiness);
    println!("  artifact          {}", outcome.artifact.display());
    println!("  release           {}", outcome.release.display());
    println!();
    println!("zup init, check, doctor and build all work with no checkout, no target/,");
    println!("no staged runtime and no xtask - only the release material.");
    Ok(ExitCode::SUCCESS)
}

fn select(names: &[String]) -> Result<Vec<&'static Matrix>, String> {
    if names.is_empty() {
        return Ok(matrix::MATRICES.iter().collect());
    }
    names
        .iter()
        .map(|name| {
            matrix::matrix(name).ok_or_else(|| {
                format!(
                    "unknown matrix `{name}`; expected one of {}",
                    matrix::names().join(", ")
                )
            })
        })
        .collect()
}

/// The options that are flags rather than value-taking.
///
/// A flag must not consume the next argument: `--check --online` and
/// `--check --root .` differ only in where the flag sits, and a parser that
/// cannot tell them apart will eventually read a directory as a boolean.
const FLAGS: &[&str] = &["--online"];

fn parse(
    arguments: &mut impl Iterator<Item = String>,
    command: &str,
    allowed: &[&str],
) -> Result<Options, String> {
    let mut options = Options::default();
    while let Some(argument) = arguments.next() {
        let (flag, inline) = match argument.split_once('=') {
            Some((flag, value)) => (flag.to_owned(), Some(value.to_owned())),
            None => (argument.clone(), None),
        };
        if !allowed.contains(&flag.as_str()) {
            let complaint = if OPTIONS.contains(&flag.as_str()) {
                format!("option `{flag}` is not valid for `{command}`")
            } else {
                format!("unknown option `{flag}` for `{command}`")
            };
            return Err(complaint);
        }
        if FLAGS.contains(&flag.as_str()) {
            if let Some(value) = inline
                && value != "true"
            {
                return Err(format!("`{flag}` is a flag and takes no value"));
            }
            options.online = true;
            continue;
        }
        let value = match inline {
            Some(inline) => inline,
            None => arguments
                .next()
                .ok_or_else(|| format!("option `{flag}` needs a value"))?,
        };
        match flag.as_str() {
            "--root" => options.root = Some(PathBuf::from(value)),
            "--matrix" => options.matrices.push(value),
            "--add" => options.add.push(value),
            "--profile" => options.profile = Some(value),
            "--out" => options.out = Some(PathBuf::from(value)),
            "--material" => options.material = Some(PathBuf::from(value)),
            "--work" => options.work = Some(PathBuf::from(value)),
            "--format" => {
                options.format = match value.as_str() {
                    "text" => Format::Text,
                    "cargo-args" => Format::CargoArgs,
                    unknown => {
                        return Err(format!(
                            "unknown format `{unknown}`; expected text or cargo-args"
                        ));
                    }
                };
            }
            _ => unreachable!("allowed options are handled above"),
        }
    }
    Ok(options)
}

/// The workspace containing this xtask, found by walking up to the manifest
/// that declares `[workspace]`.
fn repository_root() -> PathBuf {
    let mut directory = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    loop {
        if let Ok(manifest) = zup_xtask::workspace::read_manifest(&directory.join("Cargo.toml"))
            && manifest.get("workspace").is_some()
        {
            return directory;
        }
        let Some(parent) = directory.parent() else {
            panic!(
                "no [workspace] manifest above {}",
                env!("CARGO_MANIFEST_DIR")
            );
        };
        directory = parent.to_path_buf();
    }
}

fn count(violations: usize) -> String {
    if violations == 1 {
        "1 portable boundary violation".to_owned()
    } else {
        format!("{violations} portable boundary violations")
    }
}
