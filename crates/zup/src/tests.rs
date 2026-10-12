use super::command;

fn surface() -> Vec<clap::Command> {
    let mut queue = vec![command()];
    let mut found = Vec::new();
    while let Some(sub) = queue.pop() {
        found.push(sub.clone());
        queue.extend(sub.get_subcommands().cloned());
    }
    found
}

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
