//! The developer CLI's product surface, asserted.
//!
//! `crates/zup` used to hold both halves of the product — the authoring tool *and*
//! the installed application's runtime — behind one parser, so `cargo run -- --help`
//! failed unless a feature was chosen. A lifecycle verb added here is a verb a user
//! could invoke on a machine that has a different application installed, and a
//! runtime verb appearing in help is a capability that shipped to every user.
//!
//! The surface is walked recursively, so a verb that grows one level down is caught
//! as well as one added at the top.

use super::command;

/// Every command in the tree, in the order the walk reaches them.
fn surface() -> Vec<clap::Command> {
    let mut queue = vec![command()];
    let mut found = Vec::new();
    while let Some(sub) = queue.pop() {
        found.push(sub.clone());
        queue.extend(sub.get_subcommands().cloned());
    }
    found
}

/// Whether a help line offers `verb` as a command, rather than mentioning it.
///
/// A substring test would flag a description that says "install", so the line has
/// to look like a command line: an indented name at the start of a line.
fn contains_command(help: &str, verb: &str) -> bool {
    help.lines().any(|line| {
        let line = line.trim();
        line == verb || line.starts_with(&format!("{verb} "))
    })
}

#[test]
fn the_developer_surface_offers_authoring_and_never_an_application_lifecycle() {
    let mut commands = surface();
    assert!(
        commands.len() > 10,
        "the whole surface was walked: {commands:?}"
    );

    let top = commands[0].render_long_help().to_string();
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
            contains_command(&top, verb),
            "`{verb}` is missing from help:\n{top}"
        );
    }

    // The check has to reach one level down: `zup toolchain install` puts the word
    // "install" on a command line, so a check that only looked at the top level
    // would pass on a surface that had grown an installer verb underneath. That
    // verb is legitimate — it caches the components a *build* composes from — so
    // the property pinned here is the whole path: a command two levels under
    // `zup` may not be `zup <group> <application-lifecycle-verb>`.
    for command in &mut commands {
        let help = command.render_long_help().to_string();
        let name = command.get_name().to_owned();
        if name == "toolchain" {
            continue;
        }
        for verb in [
            "install",
            "upgrade",
            "update",
            "modify",
            "repair",
            "uninstall",
            "recover",
            "__worker",
            "__uninstall_runner",
            "__frontend",
            "WorkerHelp",
        ] {
            assert!(
                !contains_command(&help, verb),
                "`{name} {verb}` is an application lifecycle verb or a runtime \
                 process boundary:\n{help}"
            );
        }
    }
}
