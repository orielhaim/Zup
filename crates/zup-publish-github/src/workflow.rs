//! Generating `.github/workflows/release.yml`.
//!
//! # A committed file, and a readable pipeline
//!
//! `zup ci github generate` writes a YAML file a developer can read, review, and
//! edit. It is not hidden behind a runtime generator inside someone else's CI,
//! because a release pipeline is the thing a project most needs to audit: "what
//! does your release do?" has to have an answer that is a file in the repository,
//! and a generator trusted with the release is trusted by a version range nobody
//! reviewed. The phases are already explicit, so the file keeps one job per phase
//! and the zup action appears inside them rather than replacing the pipeline.
//!
//! # The zup action, and what it is not
//!
//! The generated workflow calls the official zup action to install and run the
//! CLI, because a consumer project does not have a zup workspace checked out.
//! `cargo build -p zup` only ever worked inside the zup repository itself, so a
//! generated file using it was correct for exactly one project. An action that
//! installs a released zup is correct for all of them.
//!
//! What the action must never become is a way to collapse the pipeline: it is
//! invoked once per phase, the phases stay visible, and the release architecture
//! is still readable in the diff.
//!
//! # What is generated and what is chosen
//!
//! The *matrix* is generated, because zup knows the target profiles and a
//! developer should not have to enumerate them in YAML and keep the two in sync.
//! Everything else is a decision a project makes and the generator only reflects:
//! whether to sign, whether to attest, which environment to gate publication
//! behind, and which zup action ref the pipeline calls.
//!
//! # Reproducibility
//!
//! [`generate`] is a pure function of its inputs, byte for byte, so
//! `zup ci github check` is meaningful: it re-derives the file and compares, and
//! the only reason it can differ is that the generator, the manifest, or the
//! action ref lock changed — a change somebody made on purpose. The lock lives in
//! `github-actions.lock.json` and is compiled in, so regenerating a matrix offline
//! can never move a ref.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use crate::pins::{PinError, pin};
use crate::runner::{Runner, cross_runner, native_runner};

/// The action ref a generated workflow uses when the project names none.
///
/// A floating tag rather than a version, so a project writes `zup@action-v1` once
/// and picks up action fixes without a dependabot pull request. A project that
/// wants an immutable pipeline sets `[publish.github.workflow] action`.
pub const DEFAULT_ACTION: &str = "orielhaim/zup@action-v1";

/// The generated file's path, relative to the repository root.
pub const WORKFLOW_PATH: &str = ".github/workflows/release.yml";

/// One target profile the matrix builds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatrixTarget {
    /// The profile name from `zup.toml`.
    pub profile: String,
    /// The canonical target triple.
    pub triple: String,
}

impl MatrixTarget {
    /// One target.
    pub fn new(profile: impl Into<String>, triple: impl Into<String>) -> Self {
        Self {
            profile: profile.into(),
            triple: triple.into(),
        }
    }
}

/// How a command signs the composed artifacts.
///
/// A command rather than a vendor, because signing is platform and vendor
/// specific and this milestone does not own either. The generated workflow runs
/// it, checks that it succeeded, and then does nothing else — the sign step is a
/// phase boundary, not an integration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signing {
    /// The command, run from the compose directory.
    pub command: String,
}

/// Everything a project chooses about its release workflow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowPolicy {
    /// The workflow's `name:`.
    pub name: String,
    /// The tag glob that triggers a release.
    pub tag_glob: String,
    /// A GitHub environment to gate the publish phase behind.
    ///
    /// Opt-in. Required reviewers, environment secrets, and tag restrictions are
    /// all things a project can configure on an environment in three clicks, and
    /// reimplementing any of them inside zup would be a worse version of the
    /// same thing.
    pub environment: Option<String>,
    /// Whether to generate build-provenance attestations.
    pub attestations: bool,
    /// Extra globs to attest, relative to the release directory.
    ///
    /// Empty by default, and the default is the point: which files a user runs is
    /// a project's decision, not this crate's, and a portable crate that spelled a
    /// platform's executable extensions would be assuming an answer it does not
    /// have. The release manifest is always attested, because it is the document
    /// that names the digests of everything else.
    pub attest_paths: Vec<String>,
    /// How to sign, if the project signs.
    pub signing: Option<Signing>,
    /// The runner that composes the final artifacts.
    ///
    /// Composition writes a portable executable, so on this project that is a
    /// Windows machine. A project whose backend emits a Mach-O or ELF container
    /// overrides it.
    pub compose_runner: String,
    /// Runner labels a project pinned itself, by triple.
    pub runner_overrides: BTreeMap<String, String>,
    /// The directory the release is composed in.
    pub release_dir: String,
    /// The path the publisher writes its receipt to.
    pub receipt: String,
    /// The zup action ref the generated pipeline calls, as `owner/repo@ref`.
    ///
    /// The action installs a released zup rather than compiling one, which is the
    /// only thing that works in a project that is not the zup repository. A
    /// project that wants an immutable pipeline names an exact ref here.
    pub action: String,
}

impl Default for WorkflowPolicy {
    fn default() -> Self {
        Self {
            name: "release".to_owned(),
            tag_glob: "v*".to_owned(),
            environment: None,
            attestations: true,
            attest_paths: Vec::new(),
            signing: None,
            compose_runner: "windows-latest".to_owned(),
            runner_overrides: BTreeMap::new(),
            release_dir: "dist".to_owned(),
            receipt: "dist/github-publish.json".to_owned(),
            action: DEFAULT_ACTION.to_owned(),
        }
    }
}

/// What `zup ci github check` found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Freshness {
    /// Whether the file on disk is what the generator produces.
    pub current: bool,
    /// The path it checked.
    pub path: String,
    /// Whether the file exists at all.
    pub present: bool,
    /// A one-line reason, for a report.
    pub detail: String,
}

/// The banner every generated workflow carries.
///
/// Two lines, and the second one is the important one: the file is generated,
/// the manifest is the source, and CI will notice if they drift.
const GENERATED_HEADER: &str = "\
# Generated by `zup ci github generate`. Edit `zup.toml`, not this file.
# `zup ci github check` fails when the two disagree.
";

/// The `uses:` line for a third-party action, as `github-actions.lock.json`
/// tracks it. A release pipeline is the one file in a project where "which
/// version of that action am I trusting with my signing key" deserves an answer a
/// reviewer can read without a lookup.
fn uses(repository: &str) -> String {
    match pin(repository) {
        Ok(action) => action.uses(),
        Err(PinError::Unknown { .. }) => {
            panic!("{repository} is not in the action pin lock; see github-actions.lock.json")
        }
    }
}

/// Render `.github/workflows/release.yml`.
pub fn generate(policy: &WorkflowPolicy, targets: &[MatrixTarget]) -> String {
    let mut out = String::new();
    out.push_str(GENERATED_HEADER);
    let _ = writeln!(out, "name: {}", policy.name);
    out.push('\n');
    out.push_str("on:\n");
    let _ = writeln!(out, "  push:\n    tags: [\"{}\"]", policy.tag_glob);
    out.push_str("  workflow_dispatch:\n");
    out.push_str("    inputs:\n");
    out.push_str(
        "      tag:\n        description: \"release tag to publish\"\n        required: true\n",
    );
    out.push_str("      dry_run:\n");
    out.push_str("        description: \"plan and verify without publishing\"\n");
    out.push_str("        type: boolean\n        default: false\n");
    out.push('\n');
    out.push_str("# Least privilege at the top, and narrower still per job. A build job\n");
    out.push_str("# cannot write to the repository; only the publish job can.\n");
    out.push_str("permissions:\n  contents: read\n");
    out.push('\n');
    // A release must never be cancelled half-published by a second run of the
    // same workflow. `cancel-in-progress: false` is the whole reason this group
    // exists: two jobs mutating one release is how a partially uploaded release
    // becomes published.
    out.push_str("concurrency:\n");
    let _ = writeln!(
        out,
        "  group: release-${{{{ github.repository }}}}-${{{{ github.event.inputs.tag || github.ref_name }}}}"
    );
    out.push_str("  cancel-in-progress: false\n");
    out.push('\n');
    out.push_str("jobs:\n");

    plan(policy, targets, &mut out);
    build(policy, targets, &mut out);
    compose(policy, targets, &mut out);
    if policy.attestations {
        attest(policy, &mut out);
    }
    publish(policy, policy.attestations, &mut out);
    out
}

fn plan(_policy: &WorkflowPolicy, targets: &[MatrixTarget], out: &mut String) {
    out.push_str("  plan:\n");
    out.push_str("    name: plan\n");
    out.push_str("    runs-on: ubuntu-latest\n");
    out.push_str("    permissions:\n      contents: read\n");
    out.push_str("    outputs:\n");
    out.push_str("      profiles: ${{ steps.plan.outputs.profiles }}\n");
    out.push_str("    steps:\n");
    step(
        out,
        "Checkout",
        &uses("actions/checkout"),
        &["fetch-depth: '0'"],
    );
    // The plan is resolved by the CLI rather than enumerated in YAML, so the
    // matrix below and the manifest cannot drift apart. It runs before the
    // matrix on purpose: a stale workflow should cost one runner-second, not
    // eleven minutes of cross-compilation.
    //
    // `profiles` is the one thing a hand-written job downstream might want, and
    // the only declared output — an output nobody writes is worse than none,
    // because it resolves to an empty string in the job that reads it.
    out.push_str("      - name: Resolve the plan\n        run: |\n");
    out.push_str("          zup ci github check --format json\n");
    let _ = writeln!(
        out,
        "          echo \"profiles={profile_list}\" >> \"$GITHUB_OUTPUT\"",
        profile_list = targets
            .iter()
            .map(|target| target.profile.as_str())
            .collect::<Vec<_>>()
            .join(",")
    );
    out.push('\n');
}

fn build(policy: &WorkflowPolicy, targets: &[MatrixTarget], out: &mut String) {
    out.push_str("  build:\n");
    out.push_str("    name: build ${{ matrix.profile }}\n");
    out.push_str("    needs: plan\n");
    out.push_str("    runs-on: ${{ matrix.runner }}\n");
    out.push_str("    permissions:\n      contents: read\n");
    out.push_str("    strategy:\n      fail-fast: false\n      matrix:\n");
    out.push_str("        include:\n");
    for target in targets {
        let runner = policy
            .runner_overrides
            .get(&target.triple)
            .cloned()
            .unwrap_or_else(|| {
                native_runner(&target.triple)
                    .map(|runner: Runner| runner.label().to_owned())
                    .unwrap_or_else(|| cross_runner(&target.triple).to_owned())
            });
        let _ = writeln!(
            out,
            "          - profile: {}\n            target: {}\n            runner: {runner}",
            target.profile, target.triple
        );
    }
    out.push_str("    steps:\n");
    step(
        out,
        "Checkout",
        &uses("actions/checkout"),
        &["persist-credentials: false"],
    );
    // Each build job produces one target's native output and nothing else. It does
    // not upload to the release: a matrix of jobs racing to mutate one release is
    // how a partially published release happens. It hands the variant to the
    // compose job as a workflow artifact instead, which is immutable and scoped to
    // the run.
    //
    // `--target` takes the profile name, which is what the developer writes in
    // `zup.toml`, rather than the triple the matrix also carries.
    let _ = writeln!(
        out,
        "      - name: Build variant\n        uses: {}\n        with:\n          operation: build\n          target: ${{{{ matrix.profile }}}}\n          release-dir: {release}/variants/${{{{ matrix.profile }}}}\n          upload-workflow-artifacts: true\n          workflow-artifact-name: variant-${{{{ matrix.profile }}}}\n          artifact-retention-days: 7",
        policy.action,
        release = policy.release_dir
    );
    out.push('\n');
}

fn compose(policy: &WorkflowPolicy, targets: &[MatrixTarget], out: &mut String) {
    out.push_str("  compose:\n");
    out.push_str("    name: compose\n");
    out.push_str("    needs: [plan, build]\n");
    let _ = writeln!(out, "    runs-on: {}", policy.compose_runner);
    out.push_str("    permissions:\n      contents: read\n");
    out.push_str("    steps:\n");
    step(
        out,
        "Checkout",
        &uses("actions/checkout"),
        &["persist-credentials: false"],
    );
    let _ = writeln!(
        out,
        "      - name: Collect variants\n        uses: {}\n        with:\n          pattern: variant-*\n          path: {}/variants\n          merge-multiple: true",
        uses("actions/download-artifact"),
        policy.release_dir
    );
    // Every `dist/` file except each variant's own release description, which the
    // build job wrote and the compose job folds into one.
    let mut inputs = vec![format!("{}/**", policy.release_dir)];
    for target in targets {
        inputs.push(format!(
            "!{}/variants/{}/{}",
            policy.release_dir,
            target.profile,
            zup_artifact::RELEASE_MANIFEST_NAME
        ));
    }
    let _ = writeln!(
        out,
        "      - name: Compose release\n        uses: {}\n        with:\n          operation: compose\n          release-dir: {}\n          upload-workflow-artifacts: true\n          workflow-artifact-name: compose",
        policy.action, policy.release_dir
    );
    // The inputs are declared so the path filter is visible in the file rather
    // than implied by what happens to be on disk.
    out.push_str("      # collected by this job:\n");
    for input in &inputs {
        let _ = writeln!(out, "      #   {input}");
    }
    if let Some(signing) = &policy.signing {
        // Sign after compose and before anything hashes the final bytes, so the
        // digest that is attested and published is the digest of the signed file.
        out.push_str("      - name: Sign\n        shell: bash\n        run: |\n");
        for line in signing.command.lines() {
            let _ = writeln!(out, "          {line}");
        }
    }
    out.push('\n');
}

fn attest(policy: &WorkflowPolicy, out: &mut String) {
    out.push_str("  attest:\n");
    out.push_str("    name: attest\n");
    out.push_str("    needs: compose\n");
    out.push_str("    runs-on: ubuntu-latest\n");
    out.push_str("    permissions:\n");
    out.push_str("      contents: read\n");
    out.push_str("      id-token: write\n");
    out.push_str("      attestations: write\n");
    out.push_str("    steps:\n");
    step(
        out,
        "Checkout",
        &uses("actions/checkout"),
        &["persist-credentials: false"],
    );
    step(
        out,
        "Collect release",
        &uses("actions/download-artifact"),
        &["pattern: compose", "path: dist"],
    );
    // Only what a project says is worth attesting, plus the manifest that names
    // the hashes. Attesting every icon would produce an attestation store nobody
    // reads, and guessing which files a user runs would be a portable crate
    // assuming a platform's answer.
    //
    // `attest: true` rather than an `actions/attest` step, so the subject list
    // comes from the release manifest rather than a glob a human wrote: the action
    // attests the final bytes of exactly the artifacts the manifest names, which is
    // what makes an attestation mean something.
    let _ = writeln!(
        out,
        "      - name: Attest provenance\n        uses: {}\n        with:\n          operation: attest\n          release-dir: {}",
        policy.action, policy.release_dir
    );
    if !policy.attest_paths.is_empty() {
        let _ = writeln!(out, "          attest-paths: |");
        for path in &policy.attest_paths {
            let _ = writeln!(out, "            {path}");
        }
    }
    out.push('\n');
}

fn publish(policy: &WorkflowPolicy, attestations: bool, out: &mut String) {
    out.push_str("  publish:\n");
    out.push_str("    name: publish\n");
    // A job that names a dependency which does not exist is a workflow that
    // cannot start, so the attest job is only in the list when it is generated.
    out.push_str(if attestations {
        "    needs: [compose, attest]\n"
    } else {
        "    needs: [compose]\n"
    });
    out.push_str("    runs-on: ubuntu-latest\n");
    // The only job in the file that can write to the repository.
    out.push_str("    permissions:\n      contents: write\n");
    if let Some(environment) = &policy.environment {
        let _ = writeln!(out, "    environment: {environment}");
    }
    out.push_str("    steps:\n");
    step(
        out,
        "Checkout",
        &uses("actions/checkout"),
        &["persist-credentials: false"],
    );
    let _ = writeln!(
        out,
        "      - name: Collect release\n        uses: {}\n        with:\n          pattern: compose\n          path: dist",
        uses("actions/download-artifact")
    );
    // One call owns the whole publication: create-or-resume the draft, upload
    // what is missing, verify every remote digest, and publish once. A matrix of
    // publish jobs would race, and a partially published release is the one
    // outcome this whole sequence exists to prevent.
    //
    // The token is an *input* rather than a step-level `env:`, which is the whole
    // security design: a build may run Tauri, Electron, Cargo build scripts and npm
    // scripts, and a token in that environment is a token in every one of them.
    // The action gives the build no token at all and exposes it only to the
    // `zup publish github` process.
    let _ = writeln!(
        out,
        "      - name: Publish release\n        uses: {}\n        with:\n          operation: publish\n          release-dir: {}\n          receipt: {}\n          github-token: ${{{{ secrets.GITHUB_TOKEN }}}}\n          dry-run: ${{{{ github.event.inputs.dry_run == 'true' }}}}",
        policy.action, policy.release_dir, policy.receipt
    );
}

fn step(out: &mut String, name: &str, uses: &str, inputs: &[&str]) {
    let _ = writeln!(out, "      - name: {name}\n        uses: {uses}");
    if inputs.is_empty() {
        return;
    }
    out.push_str("        with:\n");
    for input in inputs {
        let _ = writeln!(out, "          {input}");
    }
}

/// Compare a generated workflow against the one on disk.
pub fn check(
    root: &std::path::Path,
    policy: &WorkflowPolicy,
    targets: &[MatrixTarget],
) -> Freshness {
    let expected = generate(policy, targets);
    let path = root.join(WORKFLOW_PATH);
    let present = path.is_file();
    if !present {
        return Freshness {
            current: false,
            present: false,
            path: WORKFLOW_PATH.to_owned(),
            detail: format!("{WORKFLOW_PATH} does not exist; run `zup ci github generate`"),
        };
    }
    let actual = std::fs::read_to_string(&path).unwrap_or_default();
    if actual == expected {
        return Freshness {
            current: true,
            present: true,
            path: WORKFLOW_PATH.to_owned(),
            detail: format!("{WORKFLOW_PATH} matches the generator"),
        };
    }
    Freshness {
        current: false,
        present: true,
        path: WORKFLOW_PATH.to_owned(),
        detail: format!(
            "{WORKFLOW_PATH} no longer matches the generator; run `zup ci github generate` \
             and review the diff"
        ),
    }
}
