//! The manifest's own tools: schema, formatting, completions.
//!
//! Three small commands that need nothing from the build plane, grouped because
//! they are three small commands. `schema` is the contract other tools read,
//! `fmt` is the one that preserves a human's comments, and `completions` is the
//! only command whose output is not about this project at all.

use crate::cli::{CompletionsCommand, FmtCommand, SchemaCommand};

/// Print the authoritative zup.toml JSON Schema.
pub fn run_schema(args: SchemaCommand) -> miette::Result<()> {
    let json = zup_manifest::schema_json().map_err(|error| miette::miette!("schema: {error}"))?;
    match args.output {
        Some(path) => std::fs::write(&path, format!("{json}\n"))
            .map_err(|error| miette::miette!("write schema: {error}"))?,
        None => println!("{json}"),
    }
    Ok(())
}

/// Format a manifest, or say whether it is formatted.
///
/// `toml_edit` rather than a serializer, because a manifest is a document a person
/// wrote: their ordering, their spacing, and their comments are content, and a
/// formatter that drops them is a formatter people stop running.
pub fn run_fmt(args: FmtCommand) -> miette::Result<()> {
    let path = args
        .manifest
        .canonicalize()
        .map_err(|error| miette::miette!("manifest: {error}"))?;
    let source =
        std::fs::read_to_string(&path).map_err(|error| miette::miette!("manifest: {error}"))?;
    zup_manifest::parse_named(&source, &crate::plain_path(&path)).map_err(miette::Report::new)?;
    let document = source
        .parse::<toml_edit::DocumentMut>()
        .map_err(|error| miette::miette!("format manifest: {error}"))?;
    let mut formatted = document.to_string();
    if !formatted.ends_with('\n') {
        formatted.push('\n');
    }
    if args.check {
        if formatted != source {
            return Err(miette::miette!("{} is not formatted", path.display()));
        }
        println!("{} is formatted", path.display());
    } else if formatted != source {
        std::fs::write(&path, formatted)
            .map_err(|error| miette::miette!("write manifest: {error}"))?;
        println!("Formatted {}", path.display());
    } else {
        println!("{} is already formatted", path.display());
    }
    Ok(())
}

/// Generate shell completions for zup.
pub fn run_completions(args: CompletionsCommand) -> miette::Result<()> {
    let mut command = crate::command();
    clap_complete::generate(args.shell, &mut command, "zup", &mut std::io::stdout());
    Ok(())
}
