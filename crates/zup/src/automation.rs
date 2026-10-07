//! The one place the developer's domain models become wire types.
//!
//! Everything above this module speaks its own vocabulary: `zup-artifact` has an
//! `ArtifactIndex` and a `ReleaseManifest`, `zup-publish` has a `PublishReport` with
//! phases and steps, `zup doctor` has a check table. Each is a better model for its own
//! question than anything a consumer of a JSON document needs, and none of them is a
//! contract. This module is the boundary:
//!
//! ```text
//! ReleaseManifest ─┐
//! BuildOutcome     ─┼─→ AutomationResult → Reporter → stdout
//! PublishReport    ─┤
//! DoctorReport     ─┘
//! ```
//!
//! It is deliberately a set of small functions rather than a trait. There is no second
//! implementation to make generic over, and a trait here would be an abstraction with
//! one implementor and three call sites - which is how a `From` impl ends up in the
//! domain crate and the boundary quietly stops existing.
//!
//! The rule every function here follows: a build-machine absolute path is never a
//! release identity, a value zup does not know is `None` rather than a guess, and a
//! `Debug` rendering is never serialized.

use std::path::Path;

use zup_automation::{
    Application, Artifact, AutomationResult, ByteCount, Diagnostic, Digest, Identifier,
    Publication, PublicationAsset, SigningEvidence, SigningState, Target,
};

use crate::doctor::{CheckKind, CheckStatus, DoctorReport};
use crate::inspect_artifact::Inspection;

/// The application a command acted on.
pub fn application(app: &zup_core::App) -> Application {
    Application {
        id: app.id.as_str().to_owned(),
        name: app.name.as_str().to_owned(),
        version: app.version.to_string(),
    }
}

/// The targets a selection resolved to.
pub fn targets(selected: &[zup_core::ResolvedTargetConfig]) -> Vec<Target> {
    selected
        .iter()
        .map(|config| Target::new(config.profile.to_string(), config.target.to_string()))
        .collect()
}

/// A release-relative path, `/`-separated.
///
/// A `Path` is already the right shape: the release description is written with these
/// same file names, and a consumer resolves one against the same directory it named in
/// `release_manifest`. Normalised on the way in so a Windows build and a POSIX one
/// produce the same document.
pub fn release_path(path: &str) -> String {
    path.replace('\\', "/")
}

/// A project-relative path, `/`-separated, for a document zup wrote.
pub fn project_path(path: &Path) -> String {
    crate::plain_path(path).replace('\\', "/")
}

/// An artifact, from the release description a build just wrote.
///
/// The release description is the right source rather than a second measurement: it is
/// what the build recorded, it is what `zup sign verify` finalizes, and it is what
/// `zup publish github` measures against. Re-deriving any of it here would be a third
/// answer to a question two other documents already answered.
pub fn release_artifact(
    artifact: &zup_artifact::ReleaseArtifact,
    release: &zup_artifact::ReleaseManifest,
) -> Artifact {
    let published = artifact.finalized.as_ref();
    let digest = published.map_or(artifact.built.digest, |finalized| *finalized.digest());
    let size = published.map_or(artifact.built.size, |finalized| finalized.size());
    let signing = published.map(|finalized| {
        let evidence = finalized
            .evidence()
            .iter()
            .map(|entry| SigningEvidence::new(entry.fact.as_str(), entry.value.clone()))
            .collect::<Vec<_>>();
        if evidence.is_empty() {
            SigningState::unsigned()
        } else {
            SigningState::signed(evidence)
        }
    });
    Artifact {
        path: release_path(&artifact.path),
        digest: Digest::sha256(digest.to_hex()),
        size: ByteCount::new(size),
        kind: identifier(artifact.kind.as_str()),
        mode: identifier(artifact.mode.as_str()),
        id: Some(artifact.id.clone()),
        target: single_target(release, &artifact.id),
        variants: variant_ids(release, &artifact.id),
        signing,
    }
}

/// An internal value as a wire identifier, or `unknown`.
///
/// A `Debug` rendering is never a wire name, and neither is a value that does not
/// satisfy the grammar. `unknown` says zup could not say, which is a fact a consumer
/// can display; an invented name would be a claim.
fn identifier(value: &str) -> Identifier {
    Identifier::parse(value).unwrap_or_else(|_| Identifier::fixed("unknown"))
}

/// The triple an artifact serves, when it serves exactly one.
///
/// `None` for a multi-variant artifact rather than the first variant's triple: a
/// universal artifact is not "the x64 one", and reporting it as one is how a consumer
/// uploads a label for the wrong thing.
fn single_target(release: &zup_artifact::ReleaseManifest, id: &str) -> Option<String> {
    let variants = variant_ids(release, id)?;
    if variants.len() != 1 {
        return None;
    }
    release
        .variants
        .iter()
        .find(|variant| variant.id == variants[0])
        .map(|variant| variant.target.to_string())
}

/// The variant ids an artifact carries.
pub fn variant_ids(release: &zup_artifact::ReleaseManifest, id: &str) -> Option<Vec<String>> {
    let artifact = release.artifacts.iter().find(|entry| entry.id == id)?;
    if artifact.variants.is_empty() {
        return None;
    }
    Some(artifact.variants.clone())
}

/// Every artifact a release describes, in the order the build produced them.
pub fn artifacts(release: &zup_artifact::ReleaseManifest) -> Vec<Artifact> {
    release
        .artifacts
        .iter()
        .map(|artifact| release_artifact(artifact, release))
        .collect()
}

/// A publication, from the provider's own report.
///
/// The report is a good model of what a publication *did* and stays in `zup-publish`
/// as such. This is the projection a machine consumer needs: where it went, under what
/// name, in what state, at what address, with which files. A consumer that had to walk
/// the provider's phase list to learn a release URL would be reading a provider's
/// vocabulary, which is the thing this protocol exists to stop.
pub fn publication(report: &zup_publish::PublishReport, receipt: Option<&str>) -> Publication {
    let published = report.receipt.as_ref();
    let assets = published
        .map(|receipt| {
            receipt
                .products
                .iter()
                .map(|product| PublicationAsset {
                    name: product.name.clone(),
                    size: ByteCount::new(product.size),
                    digest: Some(Digest::sha256(product.digest.to_hex())),
                    state: identifier(product.state.as_str()),
                })
                .collect()
        })
        .unwrap_or_default();
    Publication {
        provider: report.provider.clone(),
        subject: report.subject.clone(),
        tag: report.tag.clone(),
        // A string because it is the provider's own reference and whether it fits in a
        // double is the provider's business. The receipt already models it as text for
        // exactly that reason.
        id: published
            .map(|receipt| receipt.reference.clone())
            .filter(|reference| !reference.is_empty()),
        state: published.map_or_else(
            || identifier("planned"),
            |receipt| identifier(receipt.state.as_str()),
        ),
        url: published.and_then(|receipt| receipt.url.clone()),
        immutable: published.and_then(immutability),
        assets,
        receipt: receipt.map(release_path),
    }
}

/// Whether the host said its releases cannot be changed again.
///
/// Read out of the receipt's integrity notices rather than a dedicated field, because
/// the provider-neutral receipt has no such field and adding one would put a GitHub
/// concept into a document that is deliberately not GitHub's. The notices are exactly
/// the place a provider states such a claim, and the three statuses map onto the three
/// honest answers: the host said yes, the host said no, or the host said nothing.
fn immutability(receipt: &zup_publish::PublishReceipt) -> Option<bool> {
    receipt
        .notices
        .iter()
        .find(|notice| notice.label.contains("immutable"))
        .and_then(|notice| match notice.status.as_str() {
            "ok" => Some(true),
            "warn" => Some(false),
            _ => None,
        })
}

/// Every artifact a release description carries, for a finalizing command.
pub fn finalized_artifacts(release: &zup_artifact::ReleaseManifest) -> Vec<Artifact> {
    artifacts(release)
}

/// Doctor's own report, whole.
///
/// Every row crosses, green ones included: a skipped check is a question that was never
/// answered, and a consumer that saw only the failures would call a project ready on the
/// strength of checks that never ran.
pub fn doctor(report: &DoctorReport) -> zup_automation::DoctorDetails {
    zup_automation::DoctorDetails {
        manifest: report.manifest.clone(),
        host: report.host.clone(),
        ready: report.is_ready(),
        checks: report
            .targets
            .iter()
            .flat_map(|target| &target.checks)
            .count(),
        targets: report
            .targets
            .iter()
            .map(|target| {
                zup_automation::DoctorTarget::new(
                    target.profile.clone(),
                    target.target.clone(),
                    Identifier::fixed(check_status(target.status)),
                    target
                        .checks
                        .iter()
                        .map(|check| {
                            zup_automation::DoctorCheck::new(
                                Identifier::fixed(check_kind(check.kind)),
                                Identifier::fixed(check_status(check.status)),
                                check.message.clone(),
                                check.path.clone(),
                            )
                        })
                        .collect(),
                )
            })
            .collect(),
    }
}

/// A check's verdict, as a wire identifier.
fn check_status(status: CheckStatus) -> &'static str {
    match status {
        CheckStatus::Pass => "pass",
        CheckStatus::Fail => "fail",
        CheckStatus::Skip => "skip",
    }
}

/// A check kind's wire name.
///
/// `Debug` is not a wire name: `RuntimeTemplate` is how a Rust enum variant is spelled
/// and `runtime_template` is what a consumer greps for.
fn check_kind(kind: CheckKind) -> &'static str {
    match kind {
        CheckKind::CanonicalTarget => "canonical_target",
        CheckKind::ManifestCompile => "manifest_compile",
        CheckKind::SourcePayload => "source_payload",
        CheckKind::PluginEngine => "plugin_engine",
        CheckKind::UpdateRoot => "update_root",
        CheckKind::Frontend => "frontend",
        CheckKind::RuntimeTemplate => "runtime_template",
        CheckKind::BuildBackend => "build_backend",
        CheckKind::TargetLowering => "target_lowering",
        CheckKind::OutputParent => "output_parent",
        CheckKind::Elevation => "elevation",
        CheckKind::ServiceRuntime => "service_runtime",
    }
}

/// A diagnostic for one failed readiness check.
///
/// A warning, not an error: `zup doctor` reports a project that cannot be built, and
/// the refusal is the command's exit code, not a severity the consumer has to
/// interpret. The command turns the verdict into the result's status.
pub fn doctor_diagnostic(report: &DoctorReport) -> Diagnostic {
    let failures = report.failures();
    let profile = report
        .targets
        .iter()
        .flat_map(|target| &target.checks)
        .find(|check| check.status == CheckStatus::Fail)
        .map(|check| check.profile.clone())
        .unwrap_or_default();
    let mut diagnostic = Diagnostic::error(
        if failures > 0 {
            "zup.doctor.not_ready"
        } else {
            "zup.doctor.check_failed"
        },
        format!(
            "{failures} check(s) failed across {} target(s)",
            report.targets.len()
        ),
    )
    .with_help("Run `zup doctor` for the full report, or `zup doctor --format json` for this one.");
    if !profile.is_empty() {
        diagnostic = diagnostic.in_file(report.manifest.clone());
    }
    diagnostic
}

/// An artifact inspection, whole.
///
/// Inspection's product *is* the detail, so nothing is dropped: a caller asking what a
/// file contains wants the content accounting and the four separate trust questions,
/// not a summary of them.
pub fn inspection(inspection: &Inspection) -> zup_automation::ArtifactInspectDetails {
    zup_automation::ArtifactInspectDetails {
        artifact: inspection.artifact.clone(),
        artifact_kind: identifier(&inspection.kind),
        artifact_mode: identifier(&inspection.mode),
        pin: inspection.pin.clone(),
        subsystem: inspection.subsystem.clone(),
        variants: inspection
            .variants
            .iter()
            .map(|variant| zup_automation::InspectedVariant {
                id: variant.id.clone(),
                target: variant.target.clone(),
                frontend: variant.frontend.clone(),
                logical_size: ByteCount::new(variant.logical_size),
                file_count: count(variant.file_count),
                prerequisite_count: count(variant.prerequisite_count),
                plugin_count: count(variant.plugin_count),
                native_execution: variant.native_execution,
                target_matches_binary: variant.target_matches_binary.clone(),
            })
            .collect(),
        content: zup_automation::InspectedContent {
            logical_size: ByteCount::new(inspection.content.logical_size),
            stored_size: ByteCount::new(inspection.content.stored_size),
            content_size: ByteCount::new(inspection.content.content_size),
            shared_size: ByteCount::new(inspection.content.shared_size),
            exclusive_size: ByteCount::new(inspection.content.exclusive_size),
            unique_blob_count: count(inspection.content.unique_blob_count),
            file_size: ByteCount::new(inspection.content.file_size),
        },
        trust: zup_automation::InspectedTrust {
            authenticode: inspection.trust.authenticode.clone(),
            index: inspection.trust.index.clone(),
            content_digests: inspection.trust.content_digests.clone(),
            variants: inspection.trust.variants.clone(),
        },
    }
}

/// A count that is structurally small, and so is inside the range every consumer
/// holds exactly.
///
/// `u32` rather than `u64` on the wire, and clamped here rather than trusted: a
/// closure with more than four billion files is a different problem, and a protocol
/// that cannot represent its own answer is a worse one.
fn count(value: u64) -> u32 {
    value.min(u64::from(u32::MAX)) as u32
}

/// The result of a command that produced a release description.
///
/// The shared shape of `zup build`, `zup publish stage --release-dir` and
/// `zup sign verify`: an application, the targets it covers, the artifacts it now
/// describes, and where the description is.
pub fn release_result(
    operation: &str,
    release: &zup_artifact::ReleaseManifest,
    selected: &[zup_core::ResolvedTargetConfig],
    manifest_path: Option<String>,
) -> AutomationResult {
    let built = artifacts(release);
    let mut result = AutomationResult::new(operation)
        .with_application(application(&release.application))
        .with_targets(targets(selected))
        .with_artifacts(built.clone())
        .with_summary(format!(
            "{} {} · {} artifact(s)",
            release.application.name,
            release.application.version,
            built.len()
        ));
    if let Some(path) = manifest_path {
        result = result.with_release_manifest(path);
    }
    result
}
