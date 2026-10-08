//! The rule every function here follows: a build-machine absolute path is never a
//! `Debug` rendering is never serialized.

use std::path::Path;

use zup_automation::{
    Application, Artifact, AutomationResult, ByteCount, Diagnostic, Digest, Identifier,
    Publication, PublicationAsset, SigningEvidence, SigningState, Target,
};

use crate::doctor::{CheckKind, CheckStatus, DoctorReport};
use crate::inspect_artifact::Inspection;

pub fn application(app: &zup_core::App) -> Application {
    Application {
        id: app.id.as_str().to_owned(),
        name: app.name.as_str().to_owned(),
        version: app.version.to_string(),
    }
}

pub fn targets(selected: &[zup_core::ResolvedTargetConfig]) -> Vec<Target> {
    selected
        .iter()
        .map(|config| Target::new(config.profile.to_string(), config.target.to_string()))
        .collect()
}

pub fn release_path(path: &str) -> String {
    path.replace('\\', "/")
}

pub fn project_path(path: &Path) -> String {
    crate::plain_path(path).replace('\\', "/")
}

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

/// A `Debug` rendering is never a wire name, and neither is a value that does not
fn identifier(value: &str) -> Identifier {
    Identifier::parse(value).unwrap_or_else(|_| Identifier::fixed("unknown"))
}

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

pub fn variant_ids(release: &zup_artifact::ReleaseManifest, id: &str) -> Option<Vec<String>> {
    let artifact = release.artifacts.iter().find(|entry| entry.id == id)?;
    if artifact.variants.is_empty() {
        return None;
    }
    Some(artifact.variants.clone())
}

pub fn artifacts(release: &zup_artifact::ReleaseManifest) -> Vec<Artifact> {
    release
        .artifacts
        .iter()
        .map(|artifact| release_artifact(artifact, release))
        .collect()
}

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

pub fn finalized_artifacts(release: &zup_artifact::ReleaseManifest) -> Vec<Artifact> {
    artifacts(release)
}

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

fn check_status(status: CheckStatus) -> &'static str {
    match status {
        CheckStatus::Pass => "pass",
        CheckStatus::Fail => "fail",
        CheckStatus::Skip => "skip",
    }
}

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

fn count(value: u64) -> u32 {
    value.min(u64::from(u32::MAX)) as u32
}

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
