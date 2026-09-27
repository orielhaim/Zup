//! The runtime's product surface, asserted.
//!
//! These tests exist because the mistake they guard against is easy to make and
//! hard to notice. A build verb added to this parser would compile, would look
//! reasonable, and would ship to every user's machine as a capability the product
//! does not have. A lifecycle verb removed from this parser would be a silent
//! regression for a deployment script nobody remembers writing.
//!
//! The assertions are about the help text, because the help text is what a person
//! and a script's author read.

use super::cli;

/// The public surface: what an application user or a deployment system is
/// expected to run.
#[test]
fn the_public_surface_is_the_lifecycle_an_end_user_needs() {
    assert_eq!(
        cli::public_commands(),
        vec!["install", "modify", "repair", "update", "uninstall"]
    );
}

/// `upgrade` is not a user-facing concept.
///
/// A person handed a newer `Acme-Setup.exe` does not need to know whether that is
/// an install or an upgrade, and neither does the framework updater that runs it:
/// `install` resolves the verb from the machine's own record. The internal verb
/// still exists, hidden, for a contract that has to name it.
#[test]
fn an_upgrade_is_resolved_from_the_machine_rather_than_typed_by_a_user() {
    let public = cli::public_commands();
    assert!(!public.iter().any(|name| name == "upgrade"));
    assert!(
        cli::internal_commands()
            .iter()
            .any(|name| name == "__upgrade"),
        "the internal verb is still reachable for a framework updater contract"
    );
}

/// Recovery is an engine capability.
///
/// An interrupted transaction is reconciled from the record the engine wrote
/// before it started, so a person never has to read a transaction identifier out
/// of a log. The explicit form exists for automation, and it is hidden.
#[test]
fn recovery_is_an_engine_capability_not_a_command() {
    assert!(!cli::public_commands().iter().any(|name| name == "recover"));
    assert!(
        cli::internal_commands()
            .iter()
            .any(|name| name == "__recover"),
        "automation still has a way in"
    );
}

/// Process boundaries stay out of the help a person reads.
#[test]
fn the_workers_own_processes_are_not_advertised() {
    let public = cli::public_commands();
    for internal in ["__worker", "__uninstall_runner", "__frontend"] {
        assert!(
            !public.iter().any(|name| name == internal),
            "{internal} must not appear in the public surface"
        );
    }
}

/// This parser is the runtime's, and it holds no authoring verbs.
#[test]
fn the_runtime_never_grew_a_build_plane() {
    let help = cli::parser().render_long_help().to_string();
    for authoring in [
        "build",
        "publish",
        "ci",
        "schema",
        "fmt",
        "doctor",
        "init",
        "check",
        "plan",
        "completions",
    ] {
        assert!(
            !help.contains(authoring),
            "`{authoring}` is a developer verb and has no business in the setup runtime:\n{help}"
        );
    }
}

/// The public options are the ones an installer user or a deployment system can
/// actually specify.
///
/// Read from a subcommand, because that is where a person's `--help` lands: the
/// root parser lists verbs, and its own options are the global ones. A surface
/// check that only read the root would pass on a parser whose verbs had no
/// options at all.
#[test]
fn the_public_options_are_the_ones_a_deployment_can_mean() {
    let help = subcommand_help("install");
    for public in [
        "--scope",
        "--enable",
        "--disable",
        "--install-directory",
        "--output",
        "--non-interactive",
        "--yes",
    ] {
        assert!(help.contains(public), "{public} is missing:\n{help}");
    }
    for internal in [
        "--state-root",
        "--work-root",
        "--handoff",
        "--handoff-digest",
        "--acquired",
        "--ui",
    ] {
        assert!(
            !help.contains(internal),
            "{internal} is an internal detail and must stay out of help:\n{help}"
        );
    }
}

/// The long help of one subcommand.
fn subcommand_help(name: &str) -> String {
    let subcommand = cli::parser()
        .get_subcommands_mut()
        .find(|command| command.get_name() == name)
        .unwrap_or_else(|| panic!("`{name}` is a verb"))
        .render_long_help()
        .to_string();
    // The usage line names the binary Cargo built, which a user's copy is renamed
    // from. It is stripped so a failure here is about a flag and not about the
    // crate name.
    subcommand
        .lines()
        .filter(|line| !line.trim_start().starts_with("Usage:"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// A product-specific runtime already knows which application it belongs to.
#[test]
fn the_runtime_does_not_ask_the_caller_which_application_it_is() {
    let help = cli::parser().render_long_help().to_string();
    assert!(
        !help.contains("--app-id"),
        "a maintenance executable knows its own application:\n{help}"
    );
}

/// The help is addressed to the file the person ran.
///
/// A generated installer is renamed to `Acme-Setup.exe` before anybody sees it,
/// and help that says `zup-setup-gui install` is a compile-time name leaking into
/// a shipped product.
#[test]
fn the_help_uses_the_invoked_executable_name() {
    let mut command = cli::parser();
    assert_eq!(command.get_name(), cli::invoked_name());
    let help = command.render_long_help().to_string();
    assert!(
        !help.contains("zup-setup-gui"),
        "the Cargo binary name must not reach a user's help screen:\n{help}"
    );
}
