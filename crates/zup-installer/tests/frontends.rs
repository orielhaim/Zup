#![cfg(windows)]

use std::{fs, process::Command};

#[path = "support/project.rs"]
mod project;

use project::{AppSpec, Payload, State, compose};

/// An internal verb exists for a contract that has to name it, and for nothing
/// else. A person must not be able to reach it by guessing.
#[test]
fn the_hidden_verbs_are_reachable_and_unadvertised() {
    let app = AppSpec::unique("hidden");
    let payload = vec![Payload::named("app.exe", b"payload", Some("core"))];
    let state = State::new();
    let _cleanup = project::cleanup(&app, &state);
    let installer = compose(&app, zup_core::Frontend::Headless, &payload);

    let help = Command::new(installer.path())
        .arg("--help")
        .output()
        .expect("the help is printed");
    assert!(help.status.success());
    let text = String::from_utf8_lossy(&help.stdout);
    for hidden in [
        "__upgrade",
        "__recover",
        "__worker",
        "__uninstall_runner",
        "__frontend",
    ] {
        assert!(
            !text.contains(hidden),
            "{hidden} is advertised to a person:\n{text}"
        );
    }
    for public in [
        "install",
        "upgrade",
        "modify",
        "repair",
        "update",
        "uninstall",
    ] {
        assert!(text.contains(public), "{public} is missing:\n{text}");
    }

    // The upgrade contract still works, which is why the verb exists.
    let next = app.at_version("2.0.0");
    let upgraded = compose(
        &next,
        zup_core::Frontend::Headless,
        &[Payload::named("app.exe", b"payload 2", Some("core"))],
    );
    installer.succeed(&state, "install", &[]);
    upgraded.succeed(&state, "__upgrade", &[]);
    assert_eq!(
        fs::read(app.install_directory().join("app.exe")).unwrap(),
        b"payload 2"
    );
}

/// The persisted copy is what Apps & Features and a deployment script run, so its
/// help has to describe the same verbs the downloaded one does.
#[test]
fn the_persisted_maintenance_copy_has_the_same_surface() {
    let app = AppSpec::unique("surface");
    let payload = vec![Payload::named("app.exe", b"payload", Some("core"))];
    let state = State::new();
    let _cleanup = project::cleanup(&app, &state);
    let installer = compose(&app, zup_core::Frontend::Headless, &payload);
    installer.succeed(&state, "install", &[]);

    let maintenance = state.maintenance(&app);
    assert!(maintenance.is_file());
    let help = Command::new(&maintenance)
        .arg("--help")
        .output()
        .expect("the help is printed");
    assert!(help.status.success());
    let text = String::from_utf8_lossy(&help.stdout);
    for public in [
        "install",
        "upgrade",
        "modify",
        "repair",
        "update",
        "uninstall",
    ] {
        assert!(text.contains(public), "{public} is missing:\n{text}");
    }
    for internal in ["--state-root", "--app-id", "--handoff"] {
        assert!(
            !text.contains(internal),
            "{internal} is a build-machine detail and must stay out of help:\n{text}"
        );
    }
    installer.succeed(&state, "uninstall", &[]);
}
