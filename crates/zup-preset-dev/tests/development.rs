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

#[test]
fn a_save_is_seen_and_named_for_what_it_changed() {
    let directory = probe();
    let root = directory.path();
    let project = zup_preset_dev::Project::read(root).expect("the probe is a preset project");
    let development = Development::read(root).expect("a document");
    let watcher = zup_preset_dev::Watcher::start(&project, &development).expect("it watches");

    let (seen, events) = std::sync::mpsc::channel();
    let mut watcher = watcher;
    std::thread::spawn(move || {
        while let Some(event) = watcher.next_change() {
            if seen.send(event).is_err() {
                return;
            }
        }
    });

    settle(&events);

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

fn settle(events: &std::sync::mpsc::Receiver<zup_preset_dev::Watched>) {
    let mut quiet = Instant::now();
    while Instant::now() - quiet < Duration::from_secs(2) {
        match events.recv_timeout(Duration::from_millis(250)) {
            Ok(_) => quiet = Instant::now(),
            Err(_) => continue,
        }
    }
}

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

fn is_source(event: &zup_preset_dev::Watched) -> bool {
    matches!(
        event,
        zup_preset_dev::Watched::Changed(zup_preset_dev::Change::Source)
    )
}

fn is_configuration(event: &zup_preset_dev::Watched) -> bool {
    matches!(
        event,
        zup_preset_dev::Watched::Changed(zup_preset_dev::Change::Configuration)
    )
}

#[test]
fn a_build_that_does_not_compile_is_diagnostics_and_not_a_lost_window() {
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
