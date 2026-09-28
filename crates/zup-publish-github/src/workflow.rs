//! Generating `.github/workflows/release.yml`.
//!
//! # A committed file, and a readable pipeline
//!
//! `zup ci github generate` writes a YAML file a developer can read, review, and
//! edit. A release pipeline is the thing a project most needs to audit, so "what does
//! your release do?" has to have an answer that is a file in the repository, and a
//! generator trusted with the release is trusted by a version range nobody reviewed.
//!
//! The generated file calls the official zup action rather than `cargo build -p zup`,
//! which only ever worked inside the zup repository itself. The action is invoked once
//! per phase: it never collapses the pipeline, and the release architecture stays
//! readable in the diff.
//!
//! # What is generated and what is chosen
//!
//! The *matrix* is generated, because zup knows the target profiles and a developer
//! should not have to enumerate them in YAML and keep the two in sync. Everything else
//! is a decision a project makes and the generator only reflects: whether to sign,
//! whether to attest, which environment to gate publication behind, and which zup
//! action ref the pipeline calls.
//!
//! # Reproducibility
//!
//! [`generate`] is a pure function of its inputs, byte for byte, so
//! `zup ci github check` is meaningful: it re-derives the file and compares, and the
//! only reason it can differ is that the generator, the manifest, or the action ref
//! lock changed. The lock lives in `github-actions.lock.json` and is compiled in, so
//! regenerating a matrix offline can never move a ref.

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
/// specific, and because the credential belongs to the project: zup orchestrates
/// an external provider and never holds a key. The generated workflow runs the
/// command in the release directory with `zup-signing.json` beside it, then runs
/// `zup sign verify`, which checks the result rather than trusting the command's
/// exit code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signing {
    /// The command, run from the release directory.
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
    /// Opt-in. Required reviewers, environment secrets and tag restrictions are all
    /// things a project configures on an environment in three clicks, and
    /// reimplementing them inside zup would be a worse version of the same thing.
    pub environment: Option<String>,
    /// Whether to generate build-provenance attestations.
    pub attestations: bool,
    /// Extra globs to attest, relative to the release directory.
    ///
    /// Empty by default, and the default is the point: which files a user runs is a
    /// project's decision, and a portable crate that spelled a platform's executable
    /// extensions would be assuming an answer it does not have. The release manifest
    /// is always attested, because it names the digests of everything else.
    pub attest_paths: Vec<String>,
    /// How to sign, if the project signs.
    pub signing: Option<Signing>,
    /// The runner that composes the final artifacts.
    ///
    /// Composition writes a portable executable, so on this project that is a Windows
    /// machine. A project whose backend emits a Mach-O or ELF container overrides it.
    pub compose_runner: String,
    /// Runner labels a project pinned itself, by triple.
    pub runner_overrides: BTreeMap<String, String>,
    /// The directory the release is composed in.
    pub release_dir: String,
    /// The path the publisher writes its receipt to.
    pub receipt: String,
    /// The zup action ref the generated pipeline calls, as `owner/repo@ref`.
    ///
    /// Defaults to a floating major ref, because that is what an action ref is for: a
    /// project writes it once and dependabot keeps it current. A project that wants
    /// the release pipeline pinned to an immutable ref names one here instead, and the
    /// generated file shows exactly what it will run.
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
    // Two jobs mutating one release is how a partially uploaded release becomes
    // published, so a second run queues behind the first rather than cancelling it.
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
    sign(policy, &mut out);
    let signed = vec!["compose", "sign"];
    if policy.attestations {
        attest(policy, &signed, &mut out);
    }
    publish(policy, &signed, policy.attestations, &mut out);
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
    // The plan is resolved by the CLI rather than enumerated in YAML, so the matrix
    // below and the manifest cannot drift apart. It runs before the matrix on purpose:
    // a stale workflow should cost one runner-second, not eleven minutes of
    // cross-compilation. `profiles` is the only declared output, because an output
    // nobody writes resolves to an empty string in the job that reads it.
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
    // Each build job produces one target's native output and nothing else. It does not
    // upload to the release: a matrix of jobs racing to mutate one release is how a
    // partially published release happens. It hands the variant to the compose job as
    // a workflow artifact instead, which is immutable and scoped to the run.
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
        "      - name: Compose release\n        uses: {}\n        with:\n          operation: compose\n          release-dir: {}",
        policy.action, policy.release_dir
    );
    // The inputs are declared so the path filter is visible in the file rather
    // than implied by what happens to be on disk.
    out.push_str("      # collected by this job:\n");
    for input in &inputs {
        let _ = writeln!(out, "      #   {input}");
    }
    if policy.signing.is_none() {
        // Nothing will change the bytes, so there is no reason to hand the tree
        // to another job and collect it again - a release can be gigabytes, and
        // uploading it twice to measure it is the expensive way to learn that.
        // Finalizing here is the same check, on the same bytes, for free.
        out.push_str(
            "      # No signing command is configured, so this release publishes unsigned\n\
             \x20     # artifacts and Windows SmartScreen will warn about them. Set\n\
             \x20     # `[publish.github.workflow] signing` in zup.toml and regenerate.\n",
        );
        let _ = writeln!(
            out,
            "      - name: Finalize\n        uses: {}\n        with:\n          operation: finalize\n          release-dir: {}\n          allow-unsigned: true",
            policy.action, policy.release_dir
        );
    }
    let _ = writeln!(
        out,
        "      - name: Upload the release\n        uses: {}\n        with:\n          name: {}\n          path: {}\n          retention-days: 7\n          if-no-files-found: error",
        uses("actions/upload-artifact"),
        composed_artifact(policy),
        policy.release_dir
    );
    out.push('\n');
}

/// The workflow artifact a compose job hands to signing.
///
/// Named for what it is when the project signs: the composed release, still
/// unsigned. A pipeline that signs nothing hands over the same tree under the
/// name everything downstream collects, because there is no other producer of
/// the finalized bytes.
fn composed_artifact(policy: &WorkflowPolicy) -> &'static str {
    if policy.signing.is_some() {
        "compose-unsigned"
    } else {
        "compose"
    }
}

/// Sign the composed release and finalize it.
///
/// The job runs on the compose runner, which is a Windows host, because both the
/// signing tool and `zup sign verify` are. A project signing from a hosted x64
/// runner while building ARM64 artifacts elsewhere is the ordinary arrangement,
/// and this is the job where the two meet - which is also why a centrally hosted
/// x64 signing job is the shape a cross-architecture release needs.
///
/// It is generated only when a signing command is configured. Without one there
/// is nothing to do but finalize, and that happens in the compose job where the
/// bytes already are.
fn sign(policy: &WorkflowPolicy, out: &mut String) {
    let Some(signing) = &policy.signing else {
        return;
    };
    out.push_str("  sign:\n");
    out.push_str("    name: sign and finalize\n");
    out.push_str("    needs: compose\n");
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
        "      - name: Collect the composed release\n        uses: {}\n        with:\n          name: {}\n          path: {}",
        uses("actions/download-artifact"),
        composed_artifact(policy),
        policy.release_dir
    );
    // The command is the project's, and it runs in the release directory with
    // `zup-signing.json` beside it. The plan names every file, in signing order;
    // honouring that order is the project's job, and the finalize step below
    // proves it did - including that a composed artifact embeds the runtime that
    // was signed, not a different one.
    let _ = writeln!(
        out,
        "      - name: Sign\n        shell: pwsh\n        working-directory: {}\n        run: |",
        policy.release_dir
    );
    for line in signing.command.lines() {
        let _ = writeln!(out, "          {line}");
    }
    let _ = writeln!(
        out,
        "      - name: Verify signatures and finalize\n        uses: {}\n        with:\n          operation: finalize\n          release-dir: {}",
        policy.action, policy.release_dir
    );
    let _ = writeln!(
        out,
        "      - name: Upload the signed release\n        uses: {}\n        with:\n          name: compose\n          path: {}\n          retention-days: 7\n          if-no-files-found: error",
        uses("actions/upload-artifact"),
        policy.release_dir
    );
    out.push('\n');
}

/// The phases a later job depends on.
///
/// A job that does not depend on signing cannot be trusted to have signed bytes:
/// `compose` alone hands over the unsigned tree, and attesting or publishing that
/// is the whole failure this generator exists to prevent.
fn needs(upstream: &[&str], attestations: bool) -> String {
    let mut list: Vec<String> = upstream.iter().map(|name| (*name).to_owned()).collect();
    if attestations {
        list.push("attest".to_owned());
    }
    format!("[{}]", list.join(", "))
}

fn attest(policy: &WorkflowPolicy, upstream: &[&str], out: &mut String) {
    out.push_str("  attest:\n");
    out.push_str("    name: attest\n");
    let _ = writeln!(out, "    needs: {}", needs(upstream, false));
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
    let _ = writeln!(
        out,
        "      - name: Collect the finalized release\n        uses: {}\n        with:\n          name: compose\n          path: {}",
        uses("actions/download-artifact"),
        policy.release_dir
    );
    // Only what a project says is worth attesting, plus the manifest that names the
    // hashes: attesting every icon would produce an attestation store nobody reads.
    // `attest: true` rather than an `actions/attest` step, so the subject list comes
    // from the release manifest rather than a glob a human wrote - and the action
    // refuses an unfinalized manifest, so the subjects are the signed bytes.
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

fn publish(policy: &WorkflowPolicy, upstream: &[&str], attestations: bool, out: &mut String) {
    out.push_str("  publish:\n");
    out.push_str("    name: publish\n");
    // A job that names a dependency which does not exist is a workflow that cannot
    // start, so the attest job is only in the list when it is generated.
    let _ = writeln!(out, "    needs: {}", needs(upstream, attestations));
    out.push_str("    runs-on: ubuntu-latest\n");
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
        "      - name: Collect the finalized release\n        uses: {}\n        with:\n          name: compose\n          path: {}",
        uses("actions/download-artifact"),
        policy.release_dir
    );
    // One call owns the whole publication: create-or-resume the draft, upload what is
    // missing, verify every remote digest, and publish once. A matrix of publish jobs
    // would race, and a partially published release is the one outcome this whole
    // sequence exists to prevent.
    //
    // The token is an *input* rather than a step-level `env:`, which is the whole
    // security design: a build may run Tauri, Electron, Cargo build scripts and npm
    // scripts, and a token in that environment is a token in every one of them. The
    // action gives the build no token at all and exposes it only to the
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
