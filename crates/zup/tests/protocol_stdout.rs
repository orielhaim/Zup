//! The stdout rule, as tests that would notice if it broke.
//!
//! ```text
//! --format human   stdout is prose
//! --format json    stdout is exactly one AutomationResult
//! --format jsonl   stdout is the protocol stream
//! ```
//!
//! Everything else a command wants to say goes to stderr. The reason is not
//! tidiness. A `--format json` consumer is reading stdout, and a log line in the
//! middle of its document is a parse failure in a pipeline that spent ten minutes
//! building. The rule is only worth anything if something checks it, which is this
//! file.
//!
//! # The adversarial case
//!
//! The interesting way to break this is a *child* that writes to stdout. zup spawns
//! two: `git remote -v` and `gh auth token`. Both are captured through a pipe today,
//! which is why these tests pass — but "today" is not a property, and a future
//! `Stdio::inherit()` is exactly the kind of change that is easy to make and
//! invisible in review. So [`noisy_children`] puts a program on `PATH` under the
//! name of each child that writes a kilobyte of junk, runs the command that spawns
//! it, and asserts stdout is still exactly the protocol.
//!
//! It is deliberately not a mock. The point is what reaches the *pipe*, and a mock
//! proves nothing about that.

#![cfg(windows)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;
use tempfile::TempDir;

/// A project with one target, enough for `zup check` and `zup publish`.
fn project() -> TempDir {
    let project = TempDir::new().expect("a project directory");
    fs::create_dir_all(project.path().join("dist")).unwrap();
    fs::write(project.path().join("dist/app.bin"), b"payload").unwrap();
    fs::write(
        project.path().join("zup.toml"),
        r#"schema = 1

[app]
id = "com.example.stdout"
name = "Stdout App"
version = "1.0.0"
main = "app.bin"

[build]

[build.targets.default]
target = "x86_64-pc-windows-msvc"
source = { directory = "dist" }

[install]
scope = "user"

[install.directory]
user = "${location.user_data}/StdoutApp"

[[files]]
source = "**/*"
destination = "${install}"
"#,
    )
    .unwrap();
    project
}

fn zup() -> Command {
    Command::new(env!("CARGO_BIN_EXE_zup"))
}

/// `zup <args>` in `project`, with an environment a test controls.
fn run(project: &Path, path: Option<&Path>, args: &[&str]) -> Output {
    let mut command = zup();
    command
        .args(args)
        .arg("--manifest")
        .arg(project.join("zup.toml"))
        // Nothing ambient. A developer's own `GH_TOKEN` would make the spawn under
        // test the one that never happens.
        .env_remove("GH_TOKEN")
        .env_remove("GITHUB_TOKEN")
        .env_remove("ZUP_DRY_RUN");
    if let Some(path) = path {
        command.env("PATH", path);
    }
    command.output().expect("zup runs")
}

/// The whole of stdout, parsed as exactly one JSON document.
///
/// A helper rather than an inline parse in each test, because the assertion that
/// matters is always the same one: there is one document, and it is the whole of
/// stdout.
fn only_document(stdout: &[u8], what: &str) -> Value {
    let text = String::from_utf8_lossy(stdout);
    serde_json::from_str(&text).unwrap_or_else(|error| {
        panic!("{what}: stdout is not exactly one JSON document: {error}\n---\n{text}\n---")
    })
}

#[test]
fn json_mode_puts_one_document_on_stdout_and_nothing_else() {
    let project = project();
    let output = run(project.path(), None, &["check", "--format", "json"]);
    let document = only_document(&output.stdout, "zup check --format json");
    assert_eq!(document["protocol"], "1.0");
    assert_eq!(document["operation"], "check");
    assert_eq!(document["status"], "success");
}

#[test]
fn jsonl_mode_puts_a_stream_whose_last_line_is_the_result() {
    let project = project();
    let output = run(project.path(), None, &["check", "--format", "jsonl"]);
    let text = String::from_utf8(output.stdout).expect("utf-8 stdout");
    let lines: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();

    assert!(
        lines.len() >= 2,
        "a stream is a version line and a result: {text}"
    );
    let first: Value = serde_json::from_str(lines[0]).expect("the version line");
    assert_eq!(
        first["type"], "version",
        "the first line says which contract: {text}"
    );
    assert_eq!(first["protocol"], "1.0");
    assert_eq!(first["operation"], "check");

    let last: Value = serde_json::from_str(lines[lines.len() - 1]).expect("the result line");
    assert_eq!(last["type"], "completed");
    assert_eq!(last["result"]["status"], "success");

    // Every line is one document, and a document never spans two lines. A
    // pretty-printed result would be one object over many lines, which is exactly
    // the corruption this framing exists to prevent.
    for line in &lines {
        serde_json::from_str::<Value>(line).unwrap_or_else(|error| panic!("{line}: {error}"));
    }
}

#[test]
fn a_failure_still_emits_a_usable_document_and_exits_nonzero() {
    // A consumer that gets an empty stdout on failure has to reconstruct the reason
    // from the exit code, which is the one thing an exit code does not carry.
    let project = project();
    fs::write(
        project.path().join("zup.toml"),
        fs::read_to_string(project.path().join("zup.toml"))
            .unwrap()
            .replace(
                "[build.targets.default]\ntarget = \"x86_64-pc-windows-msvc\"",
                "[build.targets.default]\ntarget = \"not-a-triple\"",
            ),
    )
    .unwrap();

    let output = run(project.path(), None, &["check", "--format", "json"]);
    assert!(!output.status.success(), "a failure exits nonzero");
    let document = only_document(&output.stdout, "a failed zup check");
    assert_eq!(document["status"], "failure");
    let diagnostic = &document["diagnostics"][0];
    assert_eq!(diagnostic["severity"], "error");
    assert!(
        diagnostic["message"]
            .as_str()
            .is_some_and(|text| !text.is_empty())
    );
    // And the reason is on stderr too, because a person reading a CI log is not
    // going to pipe stdout into a JSON parser.
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("not-a-triple"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// The documented boundary: zup speaks the protocol for a command it could have
/// run, and clap for a command line it could not. A line that names an operation
/// gets a document, even when the document is a refusal; a line that names nothing
/// gets a usage message, because a document naming a made-up operation would be a
/// worse answer than a usage message.
#[test]
fn a_usage_error_speaks_the_protocol_only_when_it_names_an_operation() {
    let project = project();

    let named = run(
        project.path(),
        None,
        &["build", "--format", "json", "--nonsense"],
    );
    assert!(!named.status.success());
    let document = only_document(&named.stdout, "a refused zup build");
    assert_eq!(document["operation"], "build");
    assert_eq!(document["status"], "failure");
    assert_eq!(
        document["diagnostics"][0]["code"],
        "zup.cli.invalid_invocation"
    );

    let anonymous = run(project.path(), None, &["--format", "json"]);
    assert!(!anonymous.status.success());
    assert!(
        anonymous.stdout.is_empty(),
        "no document is invented for an unnamed operation"
    );
    assert!(String::from_utf8_lossy(&anonymous.stderr).contains("Usage"));
}

#[test]
fn a_raw_output_command_writes_its_product_and_no_envelope() {
    // `schema` is classified as a raw-output command: the schema *is* the product,
    // and wrapping it would be a JSON document inside a JSON document.
    let output = zup().arg("schema").output().expect("zup schema runs");
    let document = only_document(&output.stdout, "zup schema");
    assert!(
        document.get("properties").is_some(),
        "the schema is the document, not a result: {document}"
    );
    assert!(document.get("operation").is_none());
    assert!(document.get("protocol").is_none());
}

/// A directory holding programs that write junk to stdout and exit zero.
///
/// Named after the two executables zup spawns during `zup publish github`, and
/// copied from a real Windows binary rather than written as a script: a script would
/// need a shell, and the point is to prove zup does not consult one.
fn noisy_children() -> TempDir {
    let directory = TempDir::new().expect("a directory on PATH");
    // `robocopy` is on every Windows host, writes a banner to stdout, and ignores
    // arguments it does not understand — which is exactly the shape of the problem.
    // Copied under both names so whichever child zup reaches first, it reaches this.
    let source = PathBuf::from(std::env::var("SystemRoot").expect("SystemRoot"))
        .join("System32")
        .join("robocopy.exe");
    for name in ["gh.exe", "git.exe"] {
        fs::copy(&source, directory.path().join(name))
            .unwrap_or_else(|error| panic!("copying {} to {}: {error}", source.display(), name));
    }
    directory
}

#[test]
fn a_child_that_floods_stdout_cannot_corrupt_the_protocol() {
    // `zup publish github --dry-run` with no credential is the path that spawns both
    // children: it asks Git for the remote and then asks `gh` for a token, and finds
    // neither. Each of those children here writes roughly a kilobyte of banner to
    // stdout and exits zero.
    let project = project();
    let children = noisy_children();

    // `--repo` skips repository discovery, which is what reaches the fake `git`. The
    // fake `gh` is still spawned: credential discovery is next, and it is the spawn
    // whose output this test is about.
    let output = run(
        project.path(),
        Some(children.path()),
        &[
            "publish",
            "github",
            "--format",
            "json",
            "--dry-run",
            "--repo",
            "acme/acme",
        ],
    );

    let document = only_document(&output.stdout, "a dry-run publish with noisy children");
    // A plan, not a failure: everything but the write was checked, and the missing
    // credential is the diagnostic. The kilobyte the fake `gh` wrote is nowhere in
    // it.
    assert_eq!(document["operation"], "publish.github");
    assert_eq!(document["status"], "success");
    assert_eq!(document["publication"]["state"], "planned");
    assert_eq!(
        document["diagnostics"][0]["code"],
        "zup.publish.no_credential"
    );
    assert!(
        !String::from_utf8_lossy(&output.stdout).contains("Robocopy"),
        "a child's banner reached stdout"
    );

    // The whole of stdout, byte for byte, is that one document. Anything the child
    // wrote would be either a parse error above or a different digest here.
    let text = String::from_utf8(output.stdout).expect("utf-8");
    assert_eq!(
        text.trim(),
        serde_json::to_string_pretty(&document).unwrap().trim(),
        "stdout is not exactly the document, and nothing else"
    );
}

#[test]
fn a_child_that_floods_stdout_cannot_corrupt_the_stream() {
    // The same two children, read as a stream. Each line is still one document, and
    // the last one is still the result.
    let project = project();
    let children = noisy_children();

    let output = run(
        project.path(),
        Some(children.path()),
        &[
            "publish",
            "github",
            "--format",
            "jsonl",
            "--dry-run",
            "--repo",
            "acme/acme",
        ],
    );

    let text = String::from_utf8(output.stdout).expect("utf-8");
    let lines: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    assert!(lines.len() >= 2, "{text}");
    for line in &lines {
        let event: Value = serde_json::from_str(line)
            .unwrap_or_else(|error| panic!("a line is not one document: {error}\n{line}"));
        assert!(event.get("type").is_some(), "{line}");
    }
    let last: Value = serde_json::from_str(lines[lines.len() - 1]).expect("the result line");
    assert_eq!(last["type"], "completed");
    assert_eq!(last["result"]["operation"], "publish.github");
}

#[test]
fn a_credential_zup_read_is_never_written_to_its_own_output() {
    // The complement of the stdout rule, and the reason the spawn sites pipe their
    // children rather than inheriting them: a child that inherits zup's environment
    // inherits its credential. zup's own contract is that `gh` *does* read the token
    // from the environment, so this is not a property zup can enforce on the child.
    // What it can enforce, and what this test pins, is that a token zup read does not
    // reach zup's own stdout or stderr — where a CI log would keep it forever.
    //
    // The run is stopped at the release description, which is the first thing after
    // credential discovery, so no network happens and the assertion is about the
    // token rather than about GitHub.
    const TOKEN: &str = "ghp_this_value_must_never_appear_in_output";
    let project = project();
    let children = noisy_children();

    let mut command = zup();
    command
        .args([
            "publish",
            "github",
            "--format",
            "json",
            "--dry-run",
            "--repo",
            "acme/acme",
        ])
        .arg("--manifest")
        .arg(project.path().join("zup.toml"))
        .env("PATH", children.path())
        .env("GH_TOKEN", TOKEN);
    let output = command.output().expect("zup runs");

    // The token was found, or the run would have degraded to the no-credential plan
    // and this test would be asserting nothing.
    let document = only_document(&output.stdout, "a publish with a token in the environment");
    assert_ne!(
        document["diagnostics"][0]["code"], "zup.publish.no_credential",
        "zup did not read the token, so nothing was proved"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("zup-release.json"),
        "the run reached the release description: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Both streams, because a token on stderr is exactly as permanent as one on
    // stdout: a workflow log is world-readable for a public repository.
    let streams = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !streams.contains(TOKEN),
        "the credential reached zup's own output: {streams}"
    );
}
