use std::path::PathBuf;
use std::process::ExitCode;

use zup_xtask::matrix::{self, Matrix};
const USAGE: &str = "\
xtask emit-portable-matrix [--matrix <name>]... [--format <text|cargo-args>]
xtask emit-host-packages --host <windows|linux> [--format <text|cargo-args>]
xtask toolchain build [--profile <name>] [--target <target>]
xtask toolchain package [--profile <name>] [--out <dir>]
xtask release clean-room [--material <dir>] [--work <dir>]
xtask automation generate [--root <dir>]
xtask automation check [--root <dir>]

options:
    --root <dir>         workspace to inspect (default: this repository)
    --matrix <name>      matrix to emit (repeatable)
    --host <name>        build host whose packages to emit
    --format <format>    text or cargo-args
    --profile <name>     cargo profile to build and stage beside (default: dev)
    --target <target>      cross-build target for `toolchain build` (a Windows or Linux target)
    --out <dir>          where to write the packaged release
    --material <dir>     release material to test (default: target/release-material/<version>)
    --work <dir>         an empty directory to run in (default: a fresh temp directory)

exit codes:
    0  clean
    1  problems found
    2  usage or unreadable workspace
";

const OPTIONS: &[&str] = &[
    "--root",
    "--matrix",
    "--host",
    "--format",
    "--profile",
    "--target",
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
    host: Option<String>,
    format: Format,
    profile: Option<String>,
    target: Option<String>,
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
        "emit-host-packages" => emit_host(&mut arguments),
        "toolchain" => stage_toolchain(&mut arguments),
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

fn emit_host(arguments: &mut impl Iterator<Item = String>) -> Result<ExitCode, String> {
    let options = parse(arguments, "emit-host-packages", &["--host", "--format"])?;
    let Some(name) = options.host else {
        return Err("emit-host-packages needs --host <windows|linux>".to_owned());
    };
    let host = match name.as_str() {
        "windows" => matrix::Host::Windows,
        "linux" => matrix::Host::Linux,
        "any" | "portable" => matrix::Host::Any,
        other => matrix::matrix(other)
            .map(|entry| entry.host)
            .ok_or_else(|| {
                format!("unknown host `{other}`; expected windows, linux, any, or a matrix name")
            })?,
    };
    let packages = matrix::packages_for_host(host);
    if options.format == Format::CargoArgs {
        println!(
            "{}",
            packages
                .iter()
                .map(|package| format!("-p {package}"))
                .collect::<Vec<_>>()
                .join(" ")
        );
        return Ok(ExitCode::SUCCESS);
    }
    println!("{}:", matrix::host_name(host));
    for package in packages {
        println!("  {package}");
    }
    Ok(ExitCode::SUCCESS)
}

fn stage_toolchain(arguments: &mut impl Iterator<Item = String>) -> Result<ExitCode, String> {
    let Some(subcommand) = arguments.next() else {
        return Err("toolchain needs `build` or `package`\n\n".to_owned() + USAGE);
    };
    let allowed: &[&str] = match subcommand.as_str() {
        "build" => &["--root", "--profile", "--target"],
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
        let written = zup_xtask::toolchain::package(&root, &profile, &out)?;
        println!("Release material in {}", written.display());
        return Ok(ExitCode::SUCCESS);
    }

    let written = zup_xtask::toolchain::build(&root, &profile, options.target.as_deref())?;
    println!("Staged {} components", written.len());
    Ok(ExitCode::SUCCESS)
}

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
    eprintln!("xtask: generated files are stale; run `cargo xtask automation generate`");
    Ok(ExitCode::from(1))
}

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
    let work = options
        .work
        .unwrap_or_else(|| std::env::temp_dir().join(format!("zup-clean-room-{version}")));
    if work.exists() {
        std::fs::remove_dir_all(&work).map_err(|error| format!("{}: {error}", work.display()))?;
    }
    let outcome = zup_xtask::cleanroom::run(&material, &work)?;
    println!("  {}", outcome.readiness);
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
        let value = match inline {
            Some(inline) => inline,
            None => arguments
                .next()
                .ok_or_else(|| format!("option `{flag}` needs a value"))?,
        };
        match flag.as_str() {
            "--root" => options.root = Some(PathBuf::from(value)),
            "--matrix" => options.matrices.push(value),
            "--host" => options.host = Some(value),
            "--profile" => options.profile = Some(value),
            "--target" => options.target = Some(value),
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
