//! Command line for the zup repository tasks.

use std::path::PathBuf;
use std::process::ExitCode;

use zup_xtask::boundary;
use zup_xtask::matrix::{self, Matrix};

const USAGE: &str = "\
xtask emit-portable-matrix [--matrix <name>]... [--format <text|cargo-args>]
    Print the package matrices. --format cargo-args prints `-p <package>`
    arguments for cargo and needs exactly one --matrix.

xtask verify-portable-boundaries [--root <dir>]
    Report every way a portable package depends on Windows, reintroduces a
    Windows-specific identifier, or spells a Windows concept in a string
    literal.

options:
    --root <dir>     workspace to inspect (default: this repository)

exit codes:
    0  clean
    1  boundary violations
    2  usage or unreadable workspace
";

/// Every option this tool understands.
const OPTIONS: &[&str] = &["--root", "--matrix", "--format"];

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
