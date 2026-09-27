//! `zup ci github` and `zup publish github`, end to end against the real binary.
//!
//! These are the two commands a maintainer actually types, and the things worth
//! proving about them are not "the code runs" — the unit tests cover that — but
//! four properties a user depends on:
//!
//! 1. **The committed pipeline is reproducible.** `generate` writes the file,
//!    running it again says it is current, and a hand-edited file is refused
//!    rather than silently overwritten. A generated pipeline nobody reviews is
//!    just a pipeline nobody reads.
//! 2. **The report is honest.** `check` names the workflow, the derived tag, the
//!    runner each target lands on, and every action at a commit SHA. A report
//!    that says "up to date" without saying what it checked is not a report.
//! 3. **Nothing is published by accident.** A dry run with no credential prints
//!    the plan and creates nothing; a release with no staged build says so
//!    instead of inventing a release.
//! 4. **A token has nowhere to live.** The manifest has no field for one, so a
//!    project that tries to commit one is stopped at the parse.

#![cfg(feature = "build")]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

/// A project with a manifest a GitHub release could be published from.
struct Project {
    root: TempDir,
    /// An empty directory, used as `PATH` so `gh` cannot be found.
    ///
    /// Credential discovery ends at `gh auth token`, and a developer's machine
    /// that has run `gh auth login` would otherwise make every "no credential"
    /// test pass for the wrong reason. An empty `PATH` is the only way to say
    /// "this machine has no credential" that does not depend on the machine.
    barren: TempDir,
}

impl Project {
    /// A project whose manifest names `extra` after the standard sections.
    fn new(extra: &str) -> Self {
        let root = TempDir::new().expect("a temporary directory");
        let barren = TempDir::new().expect("a temporary directory");
        let path = root.path();
        let source = path.join("src");
        fs::create_dir_all(source.join("bin")).expect("a source tree");
        fs::write(source.join("bin/Acme.exe"), b"x86_64 machine code").expect("a payload");
        fs::write(
            path.join("zup.toml"),
            format!(
                r#"
schema = 1
frontend = "gui"
[app]
id = "com.example.github"
name = "Github"
version = "1.4.0"
[build]

[build.targets.x64]
target = "x86_64-pc-windows-msvc"
source = {{ directory = "src" }}

[build.targets.arm64]
target = "aarch64-pc-windows-msvc"
source = {{ directory = "src" }}

[install]
scope = "user"
[install.directory]
user = "${{location.programs}}/Github"
[[files]]
source = "**/*"
destination = "${{install}}"
{extra}
"#
            ),
        )
        .expect("a manifest");
        Self { root, barren }
    }

    fn path(&self) -> &Path {
        self.root.path()
    }

    fn workflow(&self) -> PathBuf {
        self.path()
            .join(zup_publish_github::WORKFLOW_PATH.replace('/', std::path::MAIN_SEPARATOR_STR))
    }

    /// Run `zup` with `args` in the project directory, with no GitHub
    /// environment and no `gh` on the path.
    fn zup(&self, args: &[&str]) -> Output {
        self.command(args).output().expect("zup runs")
    }

    /// Run `zup` with a credential in the environment.
    ///
    /// Only for the tests whose subject is what happens *after* a credential is
    /// found. The token is a placeholder and is never sent anywhere: the code
    /// under test fails before its first request.
    fn zup_with_token(&self, args: &[&str]) -> Output {
        self.command(args)
            .env("GH_TOKEN", "ghp_a_placeholder_that_is_never_sent")
            .output()
            .expect("zup runs")
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_zup"));
        command
            .current_dir(self.path())
            .args(args)
            .env("PATH", self.barren.path())
            .env_remove("GH_TOKEN")
            .env_remove("GITHUB_TOKEN")
            .env_remove("GITHUB_REPOSITORY")
            .env_remove("GITHUB_API_URL")
            .env_remove("GITHUB_SERVER_URL");
        command
    }
}

fn stdout(result: &Output) -> String {
    String::from_utf8_lossy(&result.stdout).into_owned()
}

fn stderr(result: &Output) -> String {
    String::from_utf8_lossy(&result.stderr).into_owned()
}

/// Run `zup ci github <args>` with the arguments passed through individually.
fn ci(project: &Project, args: &[&str]) -> Output {
    let mut all = vec!["ci", "github"];
    all.extend_from_slice(args);
    project.zup(&all)
}

fn json(result: &Output) -> serde_json::Value {
    serde_json::from_str(&stdout(result)).unwrap_or_else(|error| {
        panic!(
            "the report is JSON: {error}\n{}\n{}",
            stdout(result),
            stderr(result)
        )
    })
}

#[test]
fn a_generated_workflow_is_reproducible_and_a_hand_edit_is_not_silently_kept() {
    let project = Project::new(
        r#"
[distribution]
host = "github"

[publish.github]
repository = "acme/acme"
"#,
    );

    let first = ci(&project, &["generate"]);
    assert!(first.status.success(), "{}", stderr(&first));
    let written = project.workflow();
    assert!(written.is_file(), "{} was not written", written.display());
    let original = fs::read_to_string(&written).expect("the workflow");
    assert!(
        original.contains("name: release"),
        "the generated file names the workflow:\n{original}"
    );
    assert!(
        original.contains("Generated by `zup ci github generate`"),
        "the file says it is generated, so nobody reviews it as if it were written by hand"
    );

    // Running it again with the file already current is a no-op, not a rewrite.
    let second = ci(&project, &["generate"]);
    assert!(second.status.success(), "{}", stderr(&second));
    assert!(
        stdout(&second).contains("already current"),
        "{}",
        stdout(&second)
    );
    assert_eq!(
        fs::read_to_string(&written).expect("the workflow"),
        original
    );

    // A hand edit is refused, and the file is left exactly as the maintainer
    // wrote it. Replacing it is a decision somebody makes after reading a diff.
    fs::write(
        &written,
        format!("{original}\n# a note from a maintainer\n"),
    )
    .expect("an edit");
    let edited = fs::read_to_string(&written).expect("the workflow");
    let third = ci(&project, &["generate"]);
    assert!(
        !third.status.success(),
        "a hand-edited workflow is not overwritten"
    );
    let message = stderr(&third);
    assert!(message.contains("--force"), "{message}");
    assert_eq!(fs::read_to_string(&written).expect("the workflow"), edited);

    let forced = ci(&project, &["generate", "--force"]);
    assert!(forced.status.success(), "{}", stderr(&forced));
    assert_eq!(
        fs::read_to_string(&written).expect("the workflow"),
        original
    );
}

#[test]
fn the_check_report_says_what_it_checked_and_where_each_target_lands() {
    let project = Project::new(
        r#"
[distribution]
host = "github"

[publish.github]
repository = "acme/acme"
"#,
    );
    assert!(ci(&project, &["generate"]).status.success());

    let result = ci(&project, &["check", "--format", "json"]);
    assert!(result.status.success(), "{}", stderr(&result));
    let report = json(&result);
    assert_eq!(report["version"], 1);
    assert_eq!(report["present"], true);
    assert_eq!(report["current"], true);
    assert_eq!(report["tag"], "v1.4.0", "the tag is derived, not typed");
    assert_eq!(report["attestations"], true);
    assert_eq!(report["profiles"].as_array().expect("profiles").len(), 2);

    // Every target names the runner it lands on, and says whether that runner's
    // own architecture matches — a cross-compiled target is a different thing
    // from a native one and a report that did not say so would be misleading.
    for profile in report["profiles"].as_array().expect("profiles") {
        let runner = profile["runner"].as_str().expect("a runner");
        assert!(!runner.is_empty(), "{profile}");
        assert!(
            profile["native"].is_boolean(),
            "{profile} says whether the runner is native"
        );
    }

    // Every action is a commit SHA. A version range is not a pin, and a workflow
    // that depends on one is a workflow nobody reviewed.
    let actions = report["actions"].as_array().expect("actions");
    assert!(!actions.is_empty(), "the workflow depends on some actions");
    for action in actions {
        let sha = action["sha"].as_str().expect("a sha");
        assert_eq!(sha.len(), 40, "`{action}` is pinned to a commit");
        assert!(
            sha.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "`{action}` is a sha"
        );
        assert!(!action["version"].as_str().expect("a version").is_empty());
    }

    // The human form carries the same facts, because that is the form a person
    // reads in a terminal.
    let human = ci(&project, &["check"]);
    assert!(human.status.success(), "{}", stderr(&human));
    let text = stdout(&human);
    assert!(text.contains("current"), "{text}");
    assert!(text.contains("v1.4.0"), "{text}");
    assert!(text.contains("Pinned actions"), "{text}");
    assert!(text.contains("Target matrix"), "{text}");
}

#[test]
fn a_stale_workflow_is_reported_as_stale_rather_than_fixed() {
    let project = Project::new(
        r#"
[distribution]
host = "github"

[publish.github]
repository = "acme/acme"
"#,
    );
    let workflow = project.workflow();
    fs::create_dir_all(workflow.parent().expect("a parent")).expect("a directory");
    fs::write(&workflow, "name: Something else\n").expect("a workflow");

    let result = ci(&project, &["check"]);
    assert!(!result.status.success(), "a stale workflow is not a pass");
    let text = stdout(&result);
    assert!(text.contains("stale"), "{text}");
    assert!(
        stderr(&result).contains("zup ci github generate"),
        "the failure says what to do: {}",
        stderr(&result)
    );

    // And the same answer in JSON, so CI can read it without parsing prose.
    let machine = ci(&project, &["check", "--format", "json"]);
    let report = json(&machine);
    assert_eq!(report["present"], true);
    assert_eq!(report["current"], false);
}

#[test]
fn a_project_with_no_workflow_is_told_to_generate_one() {
    let project = Project::new(
        r#"
[distribution]
host = "github"

[publish.github]
repository = "acme/acme"
"#,
    );
    let result = ci(&project, &["check", "--format", "json"]);
    let report = json(&result);
    assert_eq!(report["present"], false);
    assert_eq!(report["current"], false);
    assert!(
        report["detail"]
            .as_str()
            .expect("a detail")
            .contains("zup ci github generate"),
        "{}",
        report["detail"]
    );
    assert!(!result.status.success());
}

#[test]
fn a_dry_run_with_no_credential_prints_the_plan_and_publishes_nothing() {
    // The property that makes `--dry-run` safe to run anywhere: it degrades to a
    // plan rather than failing on a machine that has no token, and it says which
    // of the two happened.
    let project = Project::new(
        r#"
[distribution]
host = "github"

[publish.github]
repository = "acme/acme"
"#,
    );
    fs::create_dir_all(project.path().join("dist")).expect("a release directory");

    let result = project.zup(&["publish", "github", "--repo", "acme/acme", "--dry-run"]);
    let message = stderr(&result);
    assert!(message.contains("no credential"), "{message}");
    assert!(message.contains("not attempted"), "{message}");
    assert!(result.status.success(), "{message}");
    // Nothing was created, anywhere, because nothing was attempted.
    assert!(
        !project.workflow().exists(),
        "a dry run writes no workflow and creates no release"
    );
}

#[test]
fn a_publish_with_no_staged_build_says_so_instead_of_inventing_a_release() {
    let project = Project::new(
        r#"
[distribution]
host = "github"

[publish.github]
repository = "acme/acme"
"#,
    );
    let result = project.zup_with_token(&["publish", "github", "--repo", "acme/acme"]);
    assert!(!result.status.success(), "there is nothing to publish");
    let message = stderr(&result);
    assert!(message.contains("zup-release.json"), "{message}");
    assert!(
        message.contains("zup build"),
        "it says what to run: {message}"
    );
}

#[test]
fn a_dry_run_with_a_credential_still_refuses_a_release_that_was_never_built() {
    // The other half of the same property: with a credential in hand, the run
    // still refuses before its first request, because the release it would
    // publish does not exist. Nothing is fetched, and nothing is created.
    let project = Project::new(
        r#"
[distribution]
host = "github"

[publish.github]
repository = "acme/acme"
"#,
    );
    let result = project.zup_with_token(&["publish", "github", "--repo", "acme/acme", "--dry-run"]);
    assert!(!result.status.success());
    let message = stderr(&result);
    assert!(message.contains("zup build"), "{message}");
}

#[test]
fn a_repository_that_cannot_be_found_is_refused_rather_than_guessed() {
    // No `--repo`, no `[publish.github] repository`, no `GITHUB_REPOSITORY`, and
    // a directory that is not inside a git checkout. Publishing to a repository
    // nobody named is the worst thing this command could do.
    let project = Project::new("");
    let result = project.zup(&["publish", "github", "--dry-run"]);
    assert!(!result.status.success(), "a repository is never guessed");
    let message = stderr(&result);
    for expected in ["GITHUB_REPOSITORY", "--repo", "remote"] {
        assert!(
            message.contains(expected),
            "the refusal names {expected}: {message}"
        );
    }
}

#[test]
fn a_manifest_has_nowhere_to_put_a_token() {
    // A token in a committed manifest reaches every fork of the project. The
    // schema has no field for one, so the refusal is at the parse rather than at
    // a later "you should not do that".
    let project = Project::new(
        r#"
[publish.github]
repository = "acme/acme"
token = "ghp_thiswouldleak"
"#,
    );
    let result = ci(&project, &["check", "--format", "json"]);
    assert!(!result.status.success(), "an unknown field is not ignored");
    let message = stderr(&result);
    assert!(message.contains("token"), "{message}");
}

#[test]
fn a_tag_prefix_that_cannot_be_a_tag_is_refused_before_anything_else() {
    // A tag is spelled into a URL path, a ref, and a shell command. One with a
    // space in it is a 404 at best, and the refusal has to arrive before
    // anything is uploaded.
    let project = Project::new(
        r#"
[publish.github]
repository = "acme/acme"
tag = { prefix = "release " }
"#,
    );
    let result = ci(&project, &["check"]);
    assert!(!result.status.success());
    let message = stderr(&result);
    assert!(message.contains("tag prefix"), "{message}");
}

#[test]
fn a_tag_policy_that_spells_two_things_at_once_is_refused() {
    let project = Project::new(
        r#"
[publish.github]
repository = "acme/acme"
tag = { prefix = "v", name = "release-1.4.0" }
"#,
    );
    let result = ci(&project, &["check"]);
    assert!(!result.status.success());
    let message = stderr(&result);
    assert!(message.contains("not both"), "{message}");
}
