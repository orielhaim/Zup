//! The three frontends, driven as the three different things they are.
//!
//! The runtime ships as three binaries because a person meets three situations: a
//! window, a terminal, and an automation system. What makes them different is not
//! their internals but their contract - a window asks questions, a terminal
//! answers them, and an automation system states the answer and reads a document.
//! These tests hold each of those contracts, because the failure mode is silent:
//! a front end that asks a question a redirected terminal cannot answer hangs
//! forever, and a machine-readable format that emits prose cannot be parsed.

#![cfg(windows)]

use std::{fs, process::Command};

use serde_json::Value;

#[path = "support/project.rs"]
mod project;

use project::{AppSpec, Payload, State, compose};

/// A headless run: `--output json` is one document, `--output jsonl` is a stream.
#[test]
fn a_headless_install_and_uninstall_speak_both_machine_formats() {
    let app = AppSpec::unique("headless");
    let payload = vec![Payload::named("app.exe", b"payload", Some("core"))];
    let state = State::new();
    let _cleanup = project::cleanup(&app, &state);
    let installer = compose(&app, zup_core::Frontend::Headless, &payload);

    let install = installer.succeed(&state, "install", &["--yes", "--output", "json"]);
    let document: Value =
        serde_json::from_slice(&install.stdout).expect("one JSON document on stdout");
    assert_eq!(document["outcome"], "success");
    assert_eq!(document["protocol_version"], 1);

    installer.succeed(&state, "uninstall", &["--yes", "--output", "json"]);

    for verb in ["install", "uninstall"] {
        let streamed = installer.succeed(&state, verb, &["--yes", "--output", "jsonl"]);
        let events = String::from_utf8(streamed.stdout)
            .expect("the stream is UTF-8")
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).expect("every line is one event"))
            .collect::<Vec<_>>();
        let first = events.first().expect("a started event");
        let last = events.last().expect("a completed event");
        assert_eq!(first["type"], "started", "{verb}: {first}");
        assert_eq!(first["protocol_version"], 1, "{verb}: {first}");
        assert_eq!(last["type"], "completed", "{verb}: {last}");
        assert_eq!(last["outcome"], "success", "{verb}: {last}");
    }
}

/// A console front end whose output is not a terminal must not ask a question.
///
/// A person running this in a pipeline, in CI, or from a scheduled task has nobody
/// to answer, and the process would sit there until it was killed.
#[test]
fn a_console_install_with_redirected_output_never_prompts() {
    let app = AppSpec::unique("console");
    let payload = vec![Payload::named("app.exe", b"payload", Some("core"))];
    let state = State::new();
    let _cleanup = project::cleanup(&app, &state);
    let installer = compose(&app, zup_core::Frontend::Console, &payload);

    // `--yes` is absent on purpose: the answer is that a front end which cannot
    // ask has to decide, and for a redirected stream the decision is the
    // non-interactive one.
    let install = installer.succeed(&state, "install", &["--output", "json"]);
    let document: Value =
        serde_json::from_slice(&install.stdout).expect("one JSON document on stdout");
    assert_eq!(document["outcome"], "success");

    installer.succeed(&state, "uninstall", &["--yes"]);
}

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
    for public in ["install", "modify", "repair", "update", "uninstall"] {
        assert!(text.contains(public), "{public} is missing:\n{text}");
    }
    assert!(
        !text.contains("upgrade"),
        "`upgrade` is not a thing a person types; `install` resolves it:\n{text}"
    );

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
    for public in ["install", "modify", "repair", "update", "uninstall"] {
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
