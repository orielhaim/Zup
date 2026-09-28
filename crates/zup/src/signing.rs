//! `zup sign`: hand a build's output to an external signer and finalize it.
//!
//! # Why signing is a file contract and not a call
//!
//! Signing is the one build step zup cannot perform, for a reason that has
//! nothing to do with architecture: the credential belongs to a key the project
//! owns, and a build tool that can reach a private key is a build tool whose
//! compromise is a signing compromise. So zup writes down what needs signing,
//! gets out of the way, and then *proves* what came back.
//!
//! That makes three commands where a naive design would have one, and the middle
//! one is the user's own tooling:
//!
//! ```text
//! zup build            →  artifacts, zup-release.json, zup-signing.json
//! signtool /tr … /fd SHA256 …   ← whatever the project already uses
//! zup sign verify      →  signatures checked, zup-release.json finalized
//! ```
//!
//! It also means the pipeline is not GitHub-shaped. Azure Artifact Signing,
//! SignPath, a hardware token, a company KMS, and a shell script that shells out
//! to `signtool` are the same contract, and none of them needs a zup feature.
//!
//! # What `verify` proves, in order
//!
//! 1. Every file in the plan exists.
//! 2. Every file carries a signature, and the signature covers *its* bytes.
//! 3. The signature is from the expected publisher, when one was named.
//! 4. The signature carries an RFC 3161 timestamp, unless the plan says
//!    otherwise — which only a development plan does.
//! 5. For a composed artifact, the native runtime embedded inside it is
//!    byte-for-byte the runtime the plan named, and that runtime is signed.
//!
//! Step 5 is the one a signing-only pipeline misses, and it is the one that
//! matters: the outer artifact's signature covers the embedded runtime as
//! resource data, and the runtime is later extracted to `maintenance.exe` and
//! executed. A release that skips step 5 ships an unsigned executable on every
//! user's machine, behind a well-signed one.
//!
//! # What `verify` never does
//!
//! It does not sign, and it does not hold a credential. A project that cannot
//! produce a signed artifact is asked to say so with `--allow-unsigned`, and the
//! release stays unfinalized, and a publisher refuses to upload it.
//!
//! # Where the answers come from
//!
//! Windows is the only platform that can say whether a signature is *trusted*, so
//! the trust and publisher questions are `zup_windows::signing`'s to answer and
//! this module's only to act on. The portable plan, the requirement it states,
//! the evidence recorded, and the built-to-finalized transition are
//! `zup_signing`'s, and the release description that carries the result is
//! `zup_artifact`'s. None of the three knows about the other two's job.

use std::path::{Path, PathBuf};

use zup_signing::{
    Measured as FileMeasured, SIGNING_PLAN_NAME, SigningPlan, SigningReason, SigningRole,
    SigningStage, SigningStep, SigningSubject, TimestampRequirement, covers_bytes, publisher,
    timestamp,
};
use zup_windows::signing::{SignaturePolicy, Timestamp};

use crate::cli::{SignCommand, SignPrepareCommand, SignVerb, SignVerifyCommand};

/// Run one `zup sign` invocation.
pub fn run(args: SignCommand) -> miette::Result<()> {
    let root = args.location.release_dir;
    match args.command {
        SignVerb::Prepare(args) => prepare(&root, args),
        SignVerb::Verify(args) => verify(&root, args),
    }
}

fn manifest_path(root: &Path) -> PathBuf {
    root.join(zup_artifact::RELEASE_MANIFEST_NAME)
}

fn plan_path(root: &Path) -> PathBuf {
    root.join(SIGNING_PLAN_NAME)
}

fn read_manifest(root: &Path) -> miette::Result<zup_artifact::ReleaseManifest> {
    let path = manifest_path(root);
    let bytes = std::fs::read(&path).map_err(|error| {
        miette::miette!(
            "`{}` could not be read: {error}; run `zup build` first",
            path.display()
        )
    })?;
    zup_artifact::ReleaseManifest::parse(&bytes).map_err(|error| {
        miette::miette!("`{}` is not a release description: {error}", path.display())
    })
}

fn read_plan(root: &Path) -> miette::Result<SigningPlan> {
    let path = plan_path(root);
    let bytes = std::fs::read(&path).map_err(|error| {
        miette::miette!(
            "`{}` could not be read: {error}; run `zup build` first, or `zup sign prepare`",
            path.display()
        )
    })?;
    SigningPlan::parse(&bytes)
        .map_err(|error| miette::miette!("`{}` is not a signing plan: {error}", path.display()))
}

/// Write the signing plan, from the release description beside it.
///
/// `prepare` re-derives rather than re-reads, so a plan can be regenerated after
/// an artifact was replaced and cannot describe a release that no longer exists.
fn prepare(root: &Path, args: SignPrepareCommand) -> miette::Result<()> {
    let release = read_manifest(root)?;
    let mut requirement = if args.allow_untrusted_chain || args.allow_missing_timestamp {
        zup_signing::SigningRequirement::development()
    } else {
        zup_signing::SigningRequirement::production()
    };
    if let Some(subject) = &args.subject {
        requirement = requirement.signed_by(subject);
    }
    if let Some(thumbprint) = &args.thumbprint {
        requirement.thumbprint = Some(thumbprint.clone());
    }

    let mut plan = SigningPlan::new(&release.application, requirement);
    for (variant, _) in embedded_by_variant(&release) {
        plan.push(runtime_step(root, &release, &variant)?)
            .map_err(|error| miette::miette!("signing plan: {error}"))?;
    }
    for artifact in &release.artifacts {
        plan.push(SigningStep::new(
            SigningRole::OuterArtifact,
            SigningStage::PostCompose,
            SigningReason::Installer {
                artifact: artifact.id.clone(),
            },
            SigningSubject {
                path: artifact.path.clone(),
                digest: artifact.built.digest,
                size: artifact.built.size,
                variants: artifact.variants.clone(),
            },
        ))
        .map_err(|error| miette::miette!("signing plan: {error}"))?;
    }

    let path = plan_path(root);
    zup_windows::write_durable(
        &path,
        &plan
            .encode()
            .map_err(|error| miette::miette!("signing plan: {error}"))?,
    )
    .map_err(|error| miette::miette!("`{}`: {error}", path.display()))?;

    print!("{}", render(&plan));
    Ok(())
}

/// The plan, as a person reads it.
///
/// Deterministic and derived entirely from the plan, so two machines that built
/// the same release print the same thing — which is what makes a CI log diffable.
/// The subject names are the release's own (`windows-x64`, a variant id), not a
/// crate or a type, because the reader of this output is a person deciding what to
/// hand to their signing service.
fn render(plan: &SigningPlan) -> String {
    let mut out = String::new();
    out.push_str("Signing plan\n\n");
    if plan.steps.is_empty() {
        out.push_str("  Nothing requires a signature.\n");
        return out;
    }
    let width = plan
        .steps
        .iter()
        .map(|step| step.subject.variants.first().map_or(0, |name| name.len()))
        .max()
        .unwrap_or(0);
    for (index, step) in plan.steps.iter().enumerate() {
        let subject = step.subject.variants.first().map_or("", String::as_str);
        out.push_str(&format!(
            "{:>3}. {:<14} {:<width$}  {}\n",
            index + 1,
            step.role.as_str(),
            subject,
            step.subject.path,
            width = width,
        ));
    }
    out.push_str("\nAfter signing:\n");
    out.push_str("  zup sign verify");
    if plan.requirement.trusted_chain {
        out.push_str(" --release-dir .");
    } else {
        out.push_str(
            " --release-dir . --allow-untrusted-chain --allow-missing-timestamp\n\
             \n  (a development certificate: the same checks, relaxed for a chain this\n\
             \x20  machine does not trust and a TSA it cannot reach)",
        );
    }
    out.push('\n');
    out
}

/// One native runtime the release needs signed before composition.
///
/// A composed artifact's runtime lives at `runtime/<variant>.exe` beside the
/// artifacts, because a build does not copy the toolchain's template into the
/// release: a release pipeline stages the *signed* copy there, and composes from
/// that. A missing file is a fact worth reporting rather than an error here —
/// the pipeline may not have staged it yet, and `verify` is where absence is
/// fatal.
fn runtime_step(
    root: &Path,
    release: &zup_artifact::ReleaseManifest,
    variant: &str,
) -> miette::Result<SigningStep> {
    let path = format!("runtime/{variant}.exe");
    let file = root.join(&path);
    let (size, digest) = match std::fs::metadata(&file) {
        Ok(meta) => (
            meta.len(),
            crate::project::digest_of(&file).unwrap_or(zup_core::Sha256Digest::from_bytes([0; 32])),
        ),
        Err(_) => (
            0,
            release
                .variants
                .iter()
                .find(|entry| entry.id == variant)
                .and_then(|entry| entry.runtime.as_ref())
                .map(|runtime| runtime.digest)
                .unwrap_or(zup_core::Sha256Digest::from_bytes([0; 32])),
        ),
    };
    Ok(SigningStep::new(
        SigningRole::NativeRuntime,
        SigningStage::PreCompose,
        SigningReason::VariantRuntime {
            variant: variant.to_owned(),
        },
        SigningSubject {
            path,
            digest,
            size,
            variants: vec![variant.to_owned()],
        },
    ))
}

/// The variants a release's universal artifacts embed, and which artifacts carry
/// them.
fn embedded_by_variant(release: &zup_artifact::ReleaseManifest) -> Vec<(String, Vec<String>)> {
    let mut by_variant: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    for artifact in &release.artifacts {
        if artifact.kind != zup_artifact::ArtifactKind::Universal {
            continue;
        }
        for variant in &artifact.variants {
            by_variant
                .entry(variant.clone())
                .or_default()
                .push(artifact.id.clone());
        }
    }
    by_variant.into_iter().collect()
}

/// The policy a plan's requirement becomes.
///
/// The two vocabularies state the same requirements, and this is the one place
/// they are translated, so a new field has to be answered twice or not at all.
fn policy(plan: &SigningPlan, online_revocation: bool) -> SignaturePolicy {
    SignaturePolicy {
        require_trusted_chain: plan.requirement.trusted_chain,
        // The requirement is one decision, so the policy's two timestamp rules
        // cannot disagree: relaxing the timestamp relaxes the legacy form with it.
        require_rfc3161_timestamp: plan.requirement.timestamp == TimestampRequirement::Required,
        reject_legacy_timestamp: plan.requirement.timestamp == TimestampRequirement::Required,
        subject: plan.requirement.publisher.clone(),
        thumbprint: plan.requirement.thumbprint.clone(),
        online_revocation,
    }
}

/// What verification concluded about one file.
struct Finding {
    path: String,
    detail: String,
    ok: bool,
}

/// Verify every signature and finalize the release description.
fn verify(root: &Path, args: SignVerifyCommand) -> miette::Result<()> {
    let mut release = read_manifest(root)?;
    let plan = read_plan(root)?;
    let policy = policy(&plan, args.online_revocation);
    let mut findings: Vec<Finding> = Vec::new();

    // The pre-compose subjects are verified first and their post-signature
    // digests kept, because step 5 compares them against what an artifact
    // actually embeds. A runtime discovered later in the list is still verified
    // before any artifact is finalized: the plan's order is the order.
    let mut signed_runtimes: std::collections::BTreeMap<String, zup_core::Sha256Digest> =
        std::collections::BTreeMap::new();
    for file in &plan.steps {
        let path = file.subject.resolve(root);
        match zup_windows::signing::verify(&path, &policy) {
            Ok(verified) => {
                let measured = FileMeasured::of(&path)
                    .map_err(|error| miette::miette!("`{}`: {error}", path.display()))?;
                signed_runtimes.insert(file.subject.path.clone(), measured.digest);
                findings.push(Finding {
                    path: file.subject.path.clone(),
                    detail: format!(
                        "{} · {} · sha256:{} · {} bytes",
                        verified.image.identity.subject,
                        timestamp_text(verified.image.timestamp),
                        measured.digest.to_hex(),
                        measured.size
                    ),
                    ok: true,
                });
                // A post-compose subject is the published file, so it is
                // finalized here and nowhere else: the digest and size recorded
                // are measured from the bytes that carry the signature that was
                // just checked, and `finalize` measures them a second time so a
                // file that moved in between cannot be published.
                if file.role == SigningRole::OuterArtifact {
                    let id = artifact_id(&release, &file.subject.path)?;
                    release
                        .finalize(root, &id, &measured, verified.evidence())
                        .map_err(|error| miette::miette!("`{}`: {error}", file.subject.path))?;
                }
            }
            Err(error) if args.allow_unsigned => {
                // An unsigned release is legitimate for an internal or
                // development distribution, and its published identity is simply
                // the bytes as they stand. What must never happen is finalizing
                // something that is *not* what it claims to be, so the file is
                // still measured and the refusal is reported as a fact.
                match unsigned_finalize(&mut release, root, file) {
                    Ok(detail) => findings.push(Finding {
                        path: file.subject.path.clone(),
                        detail,
                        ok: true,
                    }),
                    Err(problem) => findings.push(Finding {
                        path: file.subject.path.clone(),
                        detail: format!("{error}; and it cannot be finalized unsigned: {problem}"),
                        ok: false,
                    }),
                }
            }
            Err(error) => findings.push(Finding {
                path: file.subject.path.clone(),
                detail: error.to_string(),
                ok: false,
            }),
        }
    }

    // The nested-runtime check: an artifact embeds the runtime it was composed
    // from, and that runtime's own signature is what a user ultimately executes.
    //
    // It is a separate pass because the answer is not in the artifact: the bytes
    // have to be read back out of it, and compared against the *signed* runtime
    // rather than against the plan's pre-signature digest.
    for step in plan.post_compose().collect::<Vec<_>>() {
        let path = step.subject.resolve(root);
        if !path.is_file() {
            continue;
        }
        for embedded in plan.embeds(step) {
            let runtime_path = embedded.subject.path.clone();
            let Some(signed) = signed_runtimes.get(&runtime_path) else {
                findings.push(Finding {
                    path: step.subject.path.clone(),
                    detail: format!(
                        "embeds `{runtime_path}`, which is unsigned or missing; a signature on the \
                         container does not travel with the executable extracted from it"
                    ),
                    ok: false,
                });
                continue;
            };
            let variant = embedded
                .subject
                .variants
                .first()
                .cloned()
                .unwrap_or_default();
            match embedded_runtime_digest(&path, &variant) {
                Ok(digest) if digest == *signed => {
                    findings.push(Finding {
                        path: step.subject.path.clone(),
                        detail: format!("embeds the signed runtime for `{variant}`"),
                        ok: true,
                    });
                    if let Some(evidence) = signed_evidence_of(root, embedded) {
                        release
                            .note_runtime_evidence(&variant, evidence)
                            .map_err(|error| miette::miette!("`{variant}`: {error}"))?;
                    }
                }
                Ok(digest) => findings.push(Finding {
                    path: step.subject.path.clone(),
                    detail: format!(
                        "embeds `{variant}` as sha256:{}, but the signed runtime is sha256:{}; \
                         the artifact was composed from a runtime you did not sign",
                        digest.to_hex(),
                        signed.to_hex()
                    ),
                    ok: false,
                }),
                Err(error) => findings.push(Finding {
                    path: step.subject.path.clone(),
                    detail: format!("variant `{variant}`: {error}"),
                    ok: false,
                }),
            }
        }
        // A single-target installer is its own runtime, so the outer signature is
        // the runtime's signature. Recording it is what tells a downloader the
        // persisted maintenance executable is signed.
        if plan.embeds(step).is_empty()
            && step.subject.variants.len() == 1
            && let Some(variant) = step.subject.variants.first()
            && let Some(evidence) = signed_evidence_of(root, step)
        {
            release
                .note_runtime_evidence(variant, evidence)
                .map_err(|error| miette::miette!("`{variant}`: {error}"))?;
        }
    }

    for finding in &findings {
        println!(
            "{} {}",
            if finding.ok { "ok  " } else { "FAIL" },
            finding.path
        );
        println!("       {}", finding.detail);
    }

    let failed: Vec<&Finding> = findings.iter().filter(|finding| !finding.ok).collect();
    if !failed.is_empty() {
        if args.report_only {
            println!(
                "\n{} file(s) failed verification; the release was not finalized.",
                failed.len()
            );
            return Ok(());
        }
        return Err(miette::miette!(
            "{} of {} check(s) failed; the release was not finalized",
            failed.len(),
            findings.len()
        ));
    }

    if !release.is_finalized() {
        return Err(miette::miette!(
            "the release has no finalized identity for: {}",
            release.unfinalized().join(", ")
        ));
    }

    let path = manifest_path(root);
    zup_windows::write_durable(
        &path,
        &release
            .encode()
            .map_err(|error| miette::miette!("release description: {error}"))?,
    )
    .map_err(|error| miette::miette!("`{}`: {error}", path.display()))?;

    println!("\nFinalized {} artifact(s):", release.artifacts.len());
    for artifact in &release.artifacts {
        let Some(finalized) = &artifact.finalized else {
            continue;
        };
        let evidence = finalized.evidence();
        let note = match (covers_bytes(evidence), timestamp(evidence)) {
            (false, _) => " · UNSIGNED".to_owned(),
            (true, Some("rfc3161")) => format!(
                " · {}{}",
                publisher(evidence).unwrap_or("unknown publisher"),
                " (RFC 3161)"
            ),
            (true, Some("legacy")) => format!(
                " · {} (legacy timestamp)",
                publisher(evidence).unwrap_or("unknown publisher")
            ),
            (true, _) => format!(
                " · {} (no timestamp)",
                publisher(evidence).unwrap_or("unknown publisher")
            ),
        };
        println!(
            "  {} · sha256:{} · {} bytes{note}",
            artifact.path,
            finalized.digest().to_hex(),
            finalized.size()
        );
    }
    let unsigned = release.unsigned();
    if !unsigned.is_empty() {
        println!(
            "\n! {} artifact(s) are unsigned: {}",
            unsigned.len(),
            unsigned.join(", ")
        );
        println!("  Windows SmartScreen will warn about them. See docs/signing.md.");
    }
    println!(
        "\n`{}` now describes the bytes that will be published.",
        path.display()
    );
    Ok(())
}

/// The signing evidence of a subject whose file is already on disk and was
/// already found to carry a signature.
///
/// Read with [`zup_windows::signing::inspect`] rather than `verify`, deliberately:
/// the policy was applied once, in the loop above, and a second pass with a
/// different policy would be a second opinion from a laxer question. What is
/// wanted here is the *evidence* about a file whose signature has already been
/// accepted, not a fresh decision about whether to accept it.
fn signed_evidence_of(
    root: &Path,
    step: &SigningStep,
) -> Option<Vec<zup_signing::SigningEvidence>> {
    let path = step.subject.resolve(root);
    match zup_windows::signing::inspect(&path).ok()? {
        zup_windows::signing::Authenticode::Signed(image) => {
            Some(zup_windows::signing::VerifiedFile { path, image }.evidence())
        }
        _ => None,
    }
}

/// Finalize one file that carries no signature.
///
/// The identity is the bytes, re-measured from the file rather than copied from
/// the build, so an unsigned release is as verifiable as a signed one — the only
/// difference is that nothing proves who produced it. A runtime that is unsigned
/// cannot be "verified", so it is finalized against its own bytes and reported
/// plainly; an outer artifact is finalized through the manifest so the release
/// records the fact rather than the reader inferring it.
fn unsigned_finalize(
    release: &mut zup_artifact::ReleaseManifest,
    root: &Path,
    step: &SigningStep,
) -> miette::Result<String> {
    let path = step.subject.resolve(root);
    if !path.is_file() {
        return Err(miette::miette!("`{}` does not exist", path.display()));
    }
    let measured = FileMeasured::of(&path)
        .map_err(|error| miette::miette!("`{}`: {error}", path.display()))?;
    if step.role == SigningRole::OuterArtifact {
        let id = artifact_id(release, &step.subject.path)?;
        release
            .finalize(root, &id, &measured, Vec::new())
            .map_err(|error| miette::miette!("`{}`: {error}", step.subject.path))?;
    }
    Ok(format!(
        "unsigned · sha256:{} · {} bytes",
        measured.digest.to_hex(),
        measured.size
    ))
}

fn timestamp_text(timestamp: Timestamp) -> &'static str {
    match timestamp {
        Timestamp::Rfc3161 => "RFC 3161 timestamp",
        Timestamp::LegacyOnly => "legacy timestamp only",
        Timestamp::None => "no timestamp",
    }
}

/// The artifact id a signable path belongs to.
fn artifact_id(release: &zup_artifact::ReleaseManifest, path: &str) -> miette::Result<String> {
    release
        .artifacts
        .iter()
        .find(|artifact| artifact.path == path)
        .map(|artifact| artifact.id.clone())
        .ok_or_else(|| miette::miette!("`{path}` is not an artifact in this release"))
}

/// The digest of the native runtime a composed artifact actually embeds.
///
/// Read out of the artifact rather than taken from the release description,
/// because the question is what the bytes are, not what anybody claimed about
/// them. A resource-addressed store is the only place they exist.
fn embedded_runtime_digest(
    artifact: &Path,
    variant: &str,
) -> miette::Result<zup_core::Sha256Digest> {
    let composed = zup_windows::UniversalArtifact::open(artifact).map_err(|error| {
        miette::miette!(
            "`{}` is not a composed artifact: {error}; a single-target installer is its own runtime",
            artifact.display()
        )
    })?;
    let bytes = composed
        .embedded_runtime(variant)
        .map_err(|error| miette::miette!("`{}`: {error}", artifact.display()))?;
    zup_core::hash_reader(bytes.as_slice())
        .map(|(_, digest)| digest)
        .map_err(|error| miette::miette!("`{}`: {error}", artifact.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use semver::Version;
    use zup_core::{App, AppId, NonEmptyString};

    fn app() -> App {
        App {
            id: AppId::new("com.acme.desktop").unwrap(),
            name: NonEmptyString::new("Acme").unwrap(),
            version: Version::parse("1.0.0").unwrap(),
            publisher: None,
            main: None,
            description: None,
        }
    }

    fn plan(requirement: zup_signing::SigningRequirement) -> SigningPlan {
        SigningPlan::new(&app(), requirement)
    }

    #[test]
    fn a_production_requirement_becomes_a_production_policy() {
        let policy = policy(
            &plan(zup_signing::SigningRequirement::production().signed_by("Acme")),
            false,
        );
        assert!(policy.require_trusted_chain);
        assert!(policy.require_rfc3161_timestamp);
        assert!(policy.reject_legacy_timestamp);
        assert_eq!(policy.subject.as_deref(), Some("Acme"));
    }

    /// A development requirement is one decision — a self-signed chain and no
    /// TSA — so it cannot produce a policy that demands a timestamp it has already
    /// stopped requiring. The two old flags could be passed separately and reach
    /// that contradiction; the single `TimestampRequirement` cannot.
    #[test]
    fn a_development_requirement_relaxes_the_whole_timestamp_rule() {
        let policy = policy(&plan(zup_signing::SigningRequirement::development()), false);
        assert!(!policy.require_trusted_chain);
        assert!(!policy.require_rfc3161_timestamp);
        assert!(
            !policy.reject_legacy_timestamp,
            "refusing the legacy timestamp while not requiring any timestamp is a contradiction"
        );
    }

    /// The plan a signer reads is small, and a plan is committed, uploaded as a
    /// workflow artifact, and printed in job logs. What a signer needs is the
    /// path, the role, the subject and the requirement.
    #[test]
    fn the_signed_plan_carries_a_requirement_and_nothing_else() {
        let mut plan = plan(zup_signing::SigningRequirement::production().signed_by("Acme"));
        plan.push(SigningStep::new(
            SigningRole::NativeRuntime,
            SigningStage::PreCompose,
            SigningReason::VariantRuntime {
                variant: "windows-x64".to_owned(),
            },
            SigningSubject {
                path: "runtime/windows-x64.exe".to_owned(),
                digest: zup_core::Sha256Digest::from_bytes([1; 32]),
                size: 10,
                variants: vec!["windows-x64".to_owned()],
            },
        ))
        .expect("a distinct path");
        let encoded = plan.encode().unwrap();
        let text = String::from_utf8(encoded).expect("utf8");
        // The release description's own fields are not here: a signer gets the
        // files to sign, not the manifest, the build graph or the variants list.
        assert!(!text.contains("logical_size"));
        assert!(!text.contains("artifact_index"));
        assert!(text.contains("\"publisher\":\"Acme\""));
    }

    /// The printed plan is what a person reads to decide what to hand their
    /// signing service, so it names the release's own subjects rather than any
    /// crate or type, and it is the same on every machine that built the same
    /// release.
    #[test]
    fn the_printed_plan_is_deterministic_and_names_no_crate() {
        let mut plan = plan(zup_signing::SigningRequirement::production());
        // Two variants, so the table's column widths are exercised against
        // subjects of different lengths and the order is asserted rather than
        // being trivially one element.
        for variant in ["windows-x64", "windows-arm64"] {
            plan.push(SigningStep::new(
                SigningRole::NativeRuntime,
                SigningStage::PreCompose,
                SigningReason::VariantRuntime {
                    variant: variant.to_owned(),
                },
                SigningSubject {
                    path: format!("runtime/{variant}.exe"),
                    digest: zup_core::Sha256Digest::from_bytes([1; 32]),
                    size: 10,
                    variants: vec![variant.to_owned()],
                },
            ))
            .expect("a distinct path");
            plan.push(SigningStep::new(
                SigningRole::OuterArtifact,
                SigningStage::PostCompose,
                SigningReason::Installer {
                    artifact: format!("Acme-{variant}-Setup.exe"),
                },
                SigningSubject {
                    path: format!("dist/Acme-{variant}-Setup.exe"),
                    digest: zup_core::Sha256Digest::from_bytes([2; 32]),
                    size: 20,
                    variants: vec![variant.to_owned()],
                },
            ))
            .expect("a distinct path");
        }
        // The runtimes are signed first, whichever variant was pushed first: the
        // order is a property of the plan, not of how it was built.
        let printed = render(&plan);
        assert!(printed.find("native_runtime").unwrap() < printed.find("outer_artifact").unwrap());
        assert_eq!(plan.pre_compose().count(), 2);
        assert_eq!(plan.post_compose().count(), 2);
        let printed = render(&plan);
        assert_eq!(printed, render(&plan));
        assert!(printed.contains("windows-x64"));
        assert!(printed.contains("dist/Acme-windows-x64-Setup.exe"));
        assert!(printed.contains("zup sign verify"));
        for word in [
            "zup_signing",
            "zup-pe",
            "zup_windows",
            "SigningStep",
            "ReleaseArtifact",
        ] {
            assert!(!printed.contains(word), "the plan mentions `{word}`");
        }
    }
}
