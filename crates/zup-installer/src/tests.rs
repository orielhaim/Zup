use super::cli;

/// the record the engine wrote before it started, so a person never reads a transaction
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
