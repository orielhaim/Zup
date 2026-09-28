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

/// The public surface, and the internal one it hides.
///
/// A person handed a newer `Acme-Setup.exe` does not type `upgrade`: `install` resolves
/// the verb from the machine's own record. An interrupted transaction is reconciled from
/// the record the engine wrote before it started, so a person never reads a transaction
/// identifier out of a log. Both explicit forms exist, hidden, for a contract that has
/// to name one — a framework updater, an automation system.
#[test]
fn the_public_surface_is_the_lifecycle_an_end_user_needs() {
    assert_eq!(
        cli::public_commands(),
        vec!["install", "modify", "repair", "update", "uninstall"]
    );
    let internal = cli::internal_commands();
    for reachable in ["__upgrade", "__recover"] {
        assert!(
            internal.iter().any(|name| name == reachable),
            "{reachable} is how automation reaches the hidden half of the lifecycle"
        );
    }
    for hidden in [
        "upgrade",
        "recover",
        "__worker",
        "__uninstall_runner",
        "__frontend",
    ] {
        assert!(
            !cli::public_commands().iter().any(|name| name == hidden),
            "{hidden} must not appear in the public surface"
        );
    }
}
