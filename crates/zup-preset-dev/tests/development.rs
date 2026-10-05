//! `zup preset dev`, driven the way a preset author drives it.
//!
//! What is tested here is the half that only a preset author has: a Cargo project,
//! a compiler, and the rule that a save to Rust is a build while a save to the
//! development document is not. Everything about the machine, the child, the
//! session and the controls is `zup-preview`'s and is proved in that crate's own
//! suite, against the same fixtures and the same production transport.
//!
//! A failing build is proved with a real one, read through the same reader a
//! session uses. Inventing the JSON would prove only that the fixture parses.

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use zup_preset_dev::{Build, Development, Supervisor};

const PRESET: &str = r##"
fn main() {}
"##;

const PROBE_MANIFEST: &str = r#"
[package]
name = "probe"
version = "0.1.0"
edition = "2024"
publish = false

[[bin]]
name = "probe"
path = "src/main.rs"

[dependencies]

[workspace]
"#;

fn cargo() -> PathBuf {
    std::env::var_os("CARGO")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("cargo"))
}

/// A preset project of this test's own.
///
/// The source is never compiled in this suite - that is `zup preset dev`'s job, and it
/// is proved end to end by running the command - but the watcher and the
/// development document both need a project that really is one.
fn probe() -> tempfile::TempDir {
    let directory = tempfile::tempdir().expect("a scratch directory");
    std::fs::create_dir_all(directory.path().join("src")).expect("a source directory");
    std::fs::write(directory.path().join("src/main.rs"), PRESET).expect("the preset source");
    std::fs::write(directory.path().join("Cargo.toml"), PROBE_MANIFEST).expect("the manifest");
    directory
}

#[test]
fn the_development_document_is_data_and_not_a_preset_format() {
    let directory = tempfile::tempdir().expect("a scratch directory");
    std::fs::write(
        directory.path().join("zup.preset.dev.toml"),
        "[settings]\nhero = \"one\"\n\n[assets]\n\"a.svg\" = \"a.svg\"\n",
    )
    .expect("the document");
    let development = Development::read(directory.path()).expect("it reads");
    assert_eq!(development.settings["hero"], serde_json::json!("one"));
    assert_eq!(
        development.watched_files(directory.path()),
        [directory.path().join("a.svg")],
        "and the files it depends on are known, so an edit to one is not mistaken for code"
    );
}

/// The watcher watches the preset project, and says what happened.
///
/// An end-to-end claim over the real backend, because the one thing worth
/// proving about a filesystem watcher is that it notices a save and names it
/// correctly. It also proves the two paths this crate treats differently: an edit
/// to the document is not an edit to code.
#[test]
fn a_save_is_seen_and_named_for_what_it_changed() {
    let directory = probe();
    let root = directory.path();
    let project = zup_preset_dev::Project::read(root).expect("the probe is a preset project");
    let development = Development::read(root).expect("a document");
    let watcher = zup_preset_dev::Watcher::start(&project, &development).expect("it watches");

    // Read on a thread, because `next_change` blocks and a test that has to tell
    // "nothing has happened" from "something has" needs its own deadline rather
    // than a wait that never ends.
    let (seen, events) = std::sync::mpsc::channel();
    let mut watcher = watcher;
    std::thread::spawn(move || {
        while let Some(event) = watcher.next_change() {
            if seen.send(event).is_err() {
                return;
            }
        }
    });

    // A watch reports the tree it was pointed at as it takes it in, which is not a
    // change to the project and must not be read as one. Draining it is part of
    // starting the session, and a test that asserted over it would be asserting
    // that a fresh watcher says nothing at all - a claim about the backend's
    // startup, not about what a save is named.
    settle(&events);

    // Content that differs from what is on disk, because a write of identical
    // bytes is a write a filesystem is entitled not to report.
    let source = root.join("src").join("main.rs");
    let edited = PRESET.to_owned() + "\n// edited\n";
    std::fs::write(&source, edited).expect("the source is saved");
    assert!(
        wait_for(&events, is_source).is_some(),
        "a save to the preset's own code is a source change"
    );

    std::fs::write(
        root.join("zup.preset.dev.toml"),
        "[settings]\nhero = \"two\"\n",
    )
    .expect("the document is saved");
    assert!(
        wait_for(&events, is_configuration).is_some(),
        "a save to the document is not a source change, so it costs no compiler"
    );
}

/// Discard whatever the watch reported while it was still starting up.
///
/// Bounded by the same patience every wait here uses, and by a quiet period rather
/// than by a fixed sleep, so a backend that reports more of its own startup later
/// than this one still gets drained and one that reports nothing costs nothing.
fn settle(events: &std::sync::mpsc::Receiver<zup_preset_dev::Watched>) {
    let mut quiet = Instant::now();
    while Instant::now() - quiet < Duration::from_secs(2) {
        match events.recv_timeout(Duration::from_millis(250)) {
            Ok(_) => quiet = Instant::now(),
            Err(_) => continue,
        }
    }
}

/// The first change of the kind asked for, within a time a person would wait for.
///
/// Skipping the other kinds is not a weaker claim, it is the claim. A save to the
/// document is named `Configuration` if and only if a `Configuration` arrives, so a
/// document save misread as a source change still fails - by never producing the
/// event being waited for, which the deadline reports. What this stops asserting is
/// how many notifications the platform makes for one save, which is a fact about
/// the platform: Linux reports a save as a burst that settles some way after the
/// write, and a trailing batch can arrive after any fixed quiet period on a busy
/// runner. A test that read one event and named it was asserting that burst is one
/// notification, and failed on the runner rather than on anything here.
fn wait_for(
    events: &std::sync::mpsc::Receiver<zup_preset_dev::Watched>,
    wanted: impl Fn(&zup_preset_dev::Watched) -> bool,
) -> Option<zup_preset_dev::Watched> {
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return None;
        }
        match events.recv_timeout(remaining) {
            Ok(event) if wanted(&event) => return Some(event),
            Ok(_) => continue,
            Err(_) => return None,
        }
    }
}

/// A change naming the preset's own code.
fn is_source(event: &zup_preset_dev::Watched) -> bool {
    matches!(
        event,
        zup_preset_dev::Watched::Changed(zup_preset_dev::Change::Source)
    )
}

/// A change naming the development document.
fn is_configuration(event: &zup_preset_dev::Watched) -> bool {
    matches!(
        event,
        zup_preset_dev::Watched::Changed(zup_preset_dev::Change::Configuration)
    )
}

#[test]
fn a_build_that_does_not_compile_is_diagnostics_and_not_a_lost_window() {
    // A real failing build, read through the same reader a development session
    // uses.
    let directory = tempfile::tempdir().expect("a scratch directory");
    std::fs::create_dir_all(directory.path().join("src")).expect("a source directory");
    std::fs::write(
        directory.path().join("Cargo.toml"),
        r#"[package]
name = "broken"
version = "0.1.0"
edition = "2024"
publish = false

[[bin]]
name = "broken"
path = "src/main.rs"

[workspace]
"#,
    )
    .expect("the manifest");
    std::fs::write(
        directory.path().join("src/main.rs"),
        "fn main() { let _: u32 = \"not a number\"; }\n",
    )
    .expect("the source");

    let output = Command::new(cargo())
        .current_dir(directory.path())
        .args(["build", "--message-format=json"])
        // Nothing to resolve, so nothing to fetch, and a target directory of its
        // own so a test running beside a real build is not queued behind it. A
        // test that is occasionally slow for a reason unrelated to what it is
        // about is a test nobody can trust when it is red.
        .env("CARGO_NET_OFFLINE", "true")
        .env("CARGO_TARGET_DIR", directory.path().join("target"))
        .output()
        .expect("cargo runs");
    assert!(!output.status.success(), "the fixture does not compile");

    let build = Supervisor::read(std::io::BufReader::new(output.stdout.as_slice()), "broken");
    assert!(!build.succeeded(), "a build that did not compile says so");
    let Build::Failed { diagnostics } = &build else {
        panic!("and it is a failure rather than a missing artifact: {build:?}");
    };
    assert!(!diagnostics.is_empty(), "with what the compiler said");
    let first = &diagnostics[0];
    assert_eq!(first.level, "error");
    let where_ = first.where_.as_deref().unwrap_or_default();
    assert!(
        where_.ends_with("main.rs:2"),
        "as the compiler named it, with the line a person has to edit: {first}"
    );
}

#[test]
fn a_build_that_succeeded_without_naming_the_binary_is_not_run() {
    let input = "{\"reason\":\"build-finished\",\"success\":true}\n";
    let build = Supervisor::read(std::io::BufReader::new(input.as_bytes()), "probe");
    assert!(
        !build.succeeded(),
        "a guessed path is how a watcher ends up executing the previous build's output"
    );
}

/// A project that is not a preset project is refused with the reason, rather than
/// guessed at. `zup preset dev` is pointed at a Cargo project by a person who expects
/// a window, and "no package here" is a sentence they can act on.
#[test]
fn a_directory_with_no_manifest_is_refused_and_nothing_is_created() {
    let directory = tempfile::tempdir().expect("a scratch directory");
    let before: Vec<PathBuf> = std::fs::read_dir(directory.path())
        .expect("an empty directory")
        .flatten()
        .map(|entry| entry.path())
        .collect();
    let error = zup_preset_dev::Project::read(directory.path()).expect_err("not a project");
    assert!(
        matches!(error, zup_preset_dev::ProjectError::Metadata(..)),
        "and the refusal is Cargo's own, because a preset is a Cargo project: {error}"
    );
    // Compared as a directory rather than as one string, because a path has more
    // than one spelling: `TEMP` hands out the 8.3 short form of a directory whose
    // long form is what cargo resolves and then reports, and the claim is that a
    // person can tell which directory was refused. Either spelling identifies it,
    // so either one appearing is the claim holding. Portable on purpose - this
    // crate builds on every platform, so it cannot reach for a Windows-only helper
    // to normalise the path.
    let mut spellings = vec![directory.path().display().to_string()];
    if let Ok(resolved) = std::fs::canonicalize(directory.path()) {
        spellings.push(resolved.display().to_string());
    }
    let message = error.to_string();
    assert!(
        spellings.iter().any(|spelling| message.contains(spelling)),
        "which names the directory, so the refusal is actionable: {message}"
    );
    let after: Vec<PathBuf> = std::fs::read_dir(directory.path())
        .expect("still a directory")
        .flatten()
        .map(|entry| entry.path())
        .collect();
    assert_eq!(before, after, "and nothing was written to find out");
}
