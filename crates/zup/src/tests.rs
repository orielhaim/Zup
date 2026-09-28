//! The developer CLI's product surface, asserted.
//!
//! The mistake these guard against is not hypothetical. `crates/zup` used to hold
//! both halves of the product — the authoring tool *and* the installed
//! application's runtime — behind one parser and one `build` feature, so
//! `cargo run -- --help` failed unless a feature was chosen. Nothing about that
//! was visible in a test; it was visible the first time a new contributor tried
//! the documented command.
//!
//! So the boundary is executable. A lifecycle verb added here is a verb a user
//! could invoke on a machine that has a different application installed, and a
//! runtime verb appearing in the generated installer's help is a capability that
//! shipped to every user of the product.

use super::command;

/// The developer surface: what a person does to a project.
#[test]
fn the_developer_surface_is_authoring_and_distribution() {
    let help = command().render_long_help().to_string();
    for verb in [
        "init",
        "check",
        "doctor",
        "plan",
        "build",
        "artifact",
        "sign",
        "publish",
        "ci",
        "toolchain",
        "schema",
        "fmt",
        "completions",
    ] {
        assert!(
            help.contains(verb),
            "`{verb}` is missing from help:\n{help}"
        );
    }
}

/// A developer tool has no application lifecycle.
///
/// Installing, repairing and uninstalling an application happen in the generated
/// installer. Exposing them here would make the developer's global executable a
/// way to change an installed machine, which is exactly the coupling this package
/// split exists to remove.
#[test]
fn the_developer_cli_has_no_application_lifecycle() {
    let help = command().render_long_help().to_string();
    for verb in [
        "install",
        "upgrade",
        "update",
        "modify",
        "repair",
        "uninstall",
        "recover",
    ] {
        assert!(
            !contains_command(&help, verb),
            "`{verb}` is an application lifecycle verb and does not belong in the developer \
             CLI:\n{help}"
        );
    }
}

/// …including in `zup toolchain`, which has its own subcommand list.
///
/// Worth its own assertion because `zup toolchain install` puts the word
/// "install" on a command line, and a check that only ever looked at the top
/// level would pass on a surface that had grown an installer verb one level down.
#[test]
fn no_subcommand_anywhere_offers_an_application_lifecycle() {
    let mut queue = vec![command()];
    let mut seen = 0;
    while let Some(mut command) = queue.pop() {
        seen += 1;
        for sub in command.get_subcommands_mut() {
            let help = sub.render_long_help().to_string();
            for verb in [
                "upgrade",
                "update",
                "modify",
                "repair",
                "uninstall",
                "recover",
                "__worker",
            ] {
                assert!(
                    !contains_command(&help, verb),
                    "`{} {verb}` is an application lifecycle or runtime verb:\n{help}",
                    sub.get_name()
                );
            }
            queue.push(sub.clone());
        }
    }
    assert!(seen > 10, "the whole surface was walked: {seen} commands");
}

/// Process boundaries belong to the runtime, not to the tool that built it.
#[test]
fn the_developer_cli_exposes_no_worker_internals() {
    let help = command().render_long_help().to_string();
    for internal in ["__worker", "__uninstall_runner", "__frontend", "WorkerHelp"] {
        assert!(
            !help.contains(internal),
            "{internal} is a runtime process boundary:\n{help}"
        );
    }
}

/// The help describes the product, not the technology.
#[test]
fn the_help_describes_a_developer_tool() {
    let about = command().get_about().expect("an about line").to_string();
    assert_eq!(about, "Build and distribute zup installers");
    assert!(
        !about.contains("programmable application installer"),
        "the developer tool is not the application installer:\n{about}"
    );
}

/// `zup` with no verb prints usage.
///
/// It does not go looking for an installer package. That behaviour belonged to
/// the generated executable, and in the developer tool it meant that typing `zup`
/// in a project directory could try to install something.
#[test]
fn an_empty_invocation_prints_help_rather_than_looking_for_a_package() {
    let parsed = <super::cli::Cli as clap::Parser>::parse_from(["zup"]);
    assert!(
        parsed.command.is_none(),
        "no verb is a usage request, not an install request"
    );
}

/// The escaped-overrides are advanced, not the documented path.
///
/// A normal user runs `zup build` and the toolchain resolver finds the runtime
/// template. The flags exist for a contributor with a toolchain staged
/// somewhere unusual, and hiding them keeps the ordinary path the ordinary one.
#[test]
fn toolchain_overrides_are_advanced() {
    let help = command().render_long_help().to_string();
    assert!(!help.contains("--runtime"), "{help}");
    assert!(!help.contains("--dispatcher"), "{help}");
    assert!(
        help.contains("--toolchain"),
        "a directory of components is the documented way to point zup at a toolchain:\n{help}"
    );
}

/// Whether a help line offers `verb` as a command, rather than mentioning it.
///
/// A substring test would flag a description that says "install", so the line has
/// to look like a command line: an indented name at the start of a line, with
/// nothing else before it.
fn contains_command(help: &str, verb: &str) -> bool {
    help.lines().any(|line| {
        let line = line.trim();
        line == verb || line.starts_with(&format!("{verb} "))
    })
}
