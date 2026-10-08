//! # What `verify` never does

use std::path::{Path, PathBuf};

use zup_automation::{
    AutomationResult, ByteCount, Details, Diagnostic, Digest, Identifier, LogLevel, SignSubject,
};
#[cfg(windows)]
use zup_signing::TimestampRequirement;
use zup_signing::{
    Measured as FileMeasured, SIGNING_PLAN_NAME, SigningPlan, SigningReason, SigningRole,
    SigningStage, SigningStep, SigningSubject, covers_bytes, publisher, timestamp,
};
#[cfg(windows)]
use zup_windows::signing::{SignaturePolicy, Timestamp};

use crate::cli::{SignPrepareCommand, SignVerifyCommand};
use crate::failure::Reporter;

fn manifest_path(root: &Path) -> PathBuf {
    root.join(zup_artifact::RELEASE_MANIFEST_NAME)
}

fn plan_path(root: &Path) -> PathBuf {
    root.join(SIGNING_PLAN_NAME)
}

fn read_manifest(root: &Path) -> miette::Result<zup_artifact::ReleaseManifest> {
    let path = manifest_path(root);
    let bytes = std::fs::read(&path).map_err(|error| {
        crate::failure::error_with_help(
            "zup.signing.manifest_missing",
            format!("`{}` could not be read: {error}", path.display()),
            "Run `zup build` first; the release description names the files to sign.",
        )
    })?;
    zup_artifact::ReleaseManifest::parse(&bytes).map_err(|error| {
        crate::failure::error(
            "zup.signing.manifest_invalid",
            format!("`{}` is not a release description: {error}", path.display()),
        )
    })
}

fn read_plan(root: &Path) -> miette::Result<SigningPlan> {
    let path = plan_path(root);
    let bytes = std::fs::read(&path).map_err(|error| {
        crate::failure::error_with_help(
            "zup.signing.plan_missing",
            format!("`{}` could not be read: {error}", path.display()),
            "Run `zup build` first, or `zup sign prepare`.",
        )
    })?;
    SigningPlan::parse(&bytes).map_err(|error| {
        crate::failure::error(
            "zup.signing.plan_invalid",
            format!("`{}` is not a signing plan: {error}", path.display()),
        )
    })
}

pub fn run_prepare(root: PathBuf, args: SignPrepareCommand) -> miette::Result<AutomationResult> {
    let reporter = Reporter::new(args.format);
    let release = read_manifest(&root)?;
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
        plan.push(runtime_step(&root, &release, &variant)?)
            .map_err(|error| {
                crate::failure::error(
                    "zup.signing.plan_rejected",
                    format!("signing plan: {error}"),
                )
            })?;
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
        .map_err(|error| {
            crate::failure::error(
                "zup.signing.plan_rejected",
                format!("signing plan: {error}"),
            )
        })?;
    }

    let path = plan_path(&root);
    let encoded = plan.encode().map_err(|error| {
        crate::failure::error(
            "zup.signing.plan_unencodable",
            format!("signing plan: {error}"),
        )
    })?;
    zup_platform::publish(&path, &encoded).map_err(|error| {
        crate::failure::error(
            "zup.signing.plan_unwritable",
            format!("`{}`: {error}", path.display()),
        )
    })?;
    reporter.log(LogLevel::Info, render(&plan));
    let subjects = plan
        .steps
        .iter()
        .map(|step| subject_of(step, false, String::new(), None))
        .collect::<Vec<_>>();
    let count = subjects.len();
    Ok(
        AutomationResult::new(zup_automation::OPERATION_SIGN_PREPARE)
            .with_application(crate::automation::application(&release.application))
            .with_artifacts(crate::automation::artifacts(&release))
            .with_release_manifest(crate::automation::project_path(&manifest_path(&root)))
            .with_details(Details::SignPrepare(zup_automation::SignPrepareDetails {
                plan: crate::automation::project_path(&path),
                subjects,
            }))
            .with_summary(format!(
                "{} file(s) require a signature; sign them, then run `zup sign verify`",
                count
            )),
    )
}

fn subject_of(
    step: &SigningStep,
    verified: bool,
    detail: String,
    measured: Option<(Digest, ByteCount)>,
) -> SignSubject {
    let (digest, size) = match measured {
        Some(pair) => (Some(pair.0), Some(pair.1)),
        None => (
            Some(Digest::sha256(step.subject.digest.to_hex())),
            Some(ByteCount::new(step.subject.size)),
        ),
    };
    SignSubject {
        path: crate::automation::release_path(&step.subject.path),
        role: Identifier::fixed(step.role.as_str()),
        stage: Identifier::fixed(step.stage.as_str()),
        variants: step.subject.variants.clone(),
        verified,
        detail: if detail.is_empty() {
            "awaiting a signature".to_owned()
        } else {
            detail
        },
        digest,
        size,
    }
}

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

#[cfg(windows)]
fn policy(plan: &SigningPlan, online_revocation: bool) -> SignaturePolicy {
    SignaturePolicy {
        require_trusted_chain: plan.requirement.trusted_chain,
        require_rfc3161_timestamp: plan.requirement.timestamp == TimestampRequirement::Required,
        reject_legacy_timestamp: plan.requirement.timestamp == TimestampRequirement::Required,
        subject: plan.requirement.publisher.clone(),
        thumbprint: plan.requirement.thumbprint.clone(),
        online_revocation,
    }
}

struct Finding {
    path: String,
    detail: String,
    ok: bool,
    measured: Option<(zup_core::Sha256Digest, u64)>,
}

fn step_is_linux(release: &zup_artifact::ReleaseManifest, step: &SigningStep) -> bool {
    let mut targets = Vec::new();
    if step.role == SigningRole::OuterArtifact {
        if let Some(artifact) = release
            .artifacts
            .iter()
            .find(|artifact| artifact.path == step.subject.path)
        {
            for variant in &artifact.variants {
                if let Some(entry) = release.variants.iter().find(|entry| entry.id == *variant) {
                    targets.push(entry.target.clone());
                }
            }
        }
    } else {
        for variant in &step.subject.variants {
            if let Some(entry) = release.variants.iter().find(|entry| entry.id == *variant) {
                targets.push(entry.target.clone());
            }
        }
    }
    !targets.is_empty()
        && targets
            .iter()
            .all(|target| target.operating_system() == zup_core::TargetOperatingSystem::Linux)
}

pub fn run_verify(root: PathBuf, args: SignVerifyCommand) -> miette::Result<AutomationResult> {
    let reporter = Reporter::new(args.format);
    let mut release = read_manifest(&root)?;
    let plan = read_plan(&root)?;
    #[cfg(windows)]
    let policy = policy(&plan, args.online_revocation);
    let mut findings: Vec<Finding> = Vec::new();

    #[cfg(windows)]
    let mut signed_runtimes: std::collections::BTreeMap<String, zup_core::Sha256Digest> =
        std::collections::BTreeMap::new();
    for file in &plan.steps {
        #[cfg(windows)]
        let path = file.subject.resolve(&root);
        if step_is_linux(&release, file) {
            if !args.allow_unsigned {
                findings.push(Finding {
                    path: file.subject.path.clone(),
                    detail: "this Linux installer carries no platform-native signature: its \
                             artifact digest and release identity are its authenticity; re-run \
                             with `--allow-unsigned` to finalize the measured bytes"
                        .to_owned(),
                    ok: false,
                    measured: measured_of(&root, file),
                });
                continue;
            }
            match unsigned_finalize(&mut release, &root, file) {
                Ok(detail) => findings.push(Finding {
                    path: file.subject.path.clone(),
                    detail,
                    ok: true,
                    measured: measured_of(&root, file),
                }),
                Err(problem) => findings.push(Finding {
                    path: file.subject.path.clone(),
                    detail: format!("it cannot be finalized unsigned: {problem}"),
                    ok: false,
                    measured: measured_of(&root, file),
                }),
            }
            continue;
        }
        #[cfg(not(windows))]
        {
            if !args.allow_unsigned {
                findings.push(Finding {
                    path: file.subject.path.clone(),
                    detail: "signature verification for this Windows artifact requires a \
                             Windows build host; re-run with `--allow-unsigned` to finalize \
                             the measured bytes instead"
                        .to_owned(),
                    ok: false,
                    measured: measured_of(&root, file),
                });
                continue;
            }
            match unsigned_finalize(&mut release, &root, file) {
                Ok(detail) => findings.push(Finding {
                    path: file.subject.path.clone(),
                    detail,
                    ok: true,
                    measured: measured_of(&root, file),
                }),
                Err(problem) => findings.push(Finding {
                    path: file.subject.path.clone(),
                    detail: format!("it cannot be finalized unsigned: {problem}"),
                    ok: false,
                    measured: measured_of(&root, file),
                }),
            }
            continue;
        }
        #[cfg(windows)]
        match zup_windows::signing::verify(&path, &policy) {
            Ok(verified) => {
                let measured = FileMeasured::of(&path).map_err(|error| {
                    crate::failure::error(
                        "zup.signing.subject_unreadable",
                        format!("`{}`: {error}", path.display()),
                    )
                })?;
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
                    measured: Some((measured.digest, measured.size)),
                });
                if file.role == SigningRole::OuterArtifact {
                    let id = artifact_id(&release, &file.subject.path)?;
                    release
                        .finalize(&root, &id, &measured, verified.evidence())
                        .map_err(|error| {
                            crate::failure::error(
                                "zup.signing.finalize_failed",
                                format!("`{}`: {error}", file.subject.path),
                            )
                        })?;
                }
            }
            Err(error) if args.allow_unsigned => {
                // the bytes as they stand. What must never happen is finalizing
                match unsigned_finalize(&mut release, &root, file) {
                    Ok(detail) => findings.push(Finding {
                        path: file.subject.path.clone(),
                        detail,
                        ok: true,
                        measured: measured_of(&root, file),
                    }),
                    Err(problem) => findings.push(Finding {
                        path: file.subject.path.clone(),
                        detail: format!("{error}; and it cannot be finalized unsigned: {problem}"),
                        ok: false,
                        measured: measured_of(&root, file),
                    }),
                }
            }
            Err(error) => findings.push(Finding {
                path: file.subject.path.clone(),
                detail: signing_diagnostic_code(&error)
                    .map_or_else(
                        || crate::failure::error("zup.signing.failed", error.to_string()),
                        |code| crate::failure::error(code, error.to_string()),
                    )
                    .to_string(),
                ok: false,
                measured: measured_of(&root, file),
            }),
        }
    }

    for step in plan.post_compose().collect::<Vec<_>>() {
        let path = step.subject.resolve(&root);
        if !path.is_file() {
            continue;
        }
        // composition machinery. A Linux release never embeds one, so this
        #[cfg(not(windows))]
        if !plan.embeds(step).is_empty() {
            findings.push(Finding {
                path: step.subject.path.clone(),
                detail: "this composed artifact embeds a native runtime that can only be proven \
                         on a Windows build host"
                    .to_owned(),
                ok: false,
                measured: measured_of(&root, step),
            });
        }
        #[cfg(windows)]
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
                    measured: measured_of(&root, step),
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
                        measured: measured_of(&root, step),
                    });
                    if let Some(evidence) = signed_evidence_of(&root, embedded) {
                        release
                            .note_runtime_evidence(&variant, evidence)
                            .map_err(|error| {
                                crate::failure::error(
                                    "zup.signing.evidence_rejected",
                                    format!("`{variant}`: {error}"),
                                )
                            })?;
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
                    measured: measured_of(&root, step),
                }),
                Err(error) => findings.push(Finding {
                    path: step.subject.path.clone(),
                    detail: format!("variant `{variant}`: {error}"),
                    ok: false,
                    measured: measured_of(&root, step),
                }),
            }
        }
        if plan.embeds(step).is_empty()
            && step.subject.variants.len() == 1
            && let Some(variant) = step.subject.variants.first()
            && let Some(evidence) = signed_evidence_of(&root, step)
        {
            release
                .note_runtime_evidence(variant, evidence)
                .map_err(|error| {
                    crate::failure::error(
                        "zup.signing.evidence_rejected",
                        format!("`{variant}`: {error}"),
                    )
                })?;
        }
    }

    for finding in &findings {
        let diagnostic = Diagnostic::error("zup.signing.failed", finding.detail.clone())
            .with_help(format!("`{}`", finding.path));
        if finding.ok {
            reporter.log(
                LogLevel::Info,
                format!("ok   {}\n     {}", finding.path, finding.detail),
            );
        } else {
            reporter.diagnostic(&diagnostic);
            reporter.log(
                LogLevel::Error,
                format!("FAIL {}\n     {}", finding.path, finding.detail),
            );
        }
    }

    let subjects = plan
        .steps
        .iter()
        .map(|step| {
            let finding = findings
                .iter()
                .find(|finding| finding.path == step.subject.path);
            subject_of(
                step,
                finding.is_some_and(|finding| finding.ok),
                finding.map_or_else(String::new, |finding| finding.detail.clone()),
                finding.and_then(|finding| {
                    finding.measured.map(|(digest, size)| {
                        (Digest::sha256(digest.to_hex()), ByteCount::new(size))
                    })
                }),
            )
        })
        .collect::<Vec<_>>();
    let failed = findings.iter().filter(|finding| !finding.ok).count();
    let path = manifest_path(&root);
    let mut result = AutomationResult::new(zup_automation::OPERATION_SIGN_VERIFY)
        .with_application(crate::automation::application(&release.application))
        .with_targets(
            release
                .variants
                .iter()
                .map(|variant| {
                    zup_automation::Target::new(variant.id.clone(), variant.target.to_string())
                })
                .collect(),
        )
        .with_artifacts(crate::automation::artifacts(&release))
        .with_release_manifest(crate::automation::project_path(&path));

    if failed > 0 {
        let _ = args.report_only;
        let mut result = result.failed().with_summary(format!(
            "{failed} of {} check(s) failed; the release was not finalized",
            findings.len()
        ));
        for finding in findings.iter().filter(|finding| !finding.ok) {
            result = result.with_diagnostic(
                Diagnostic::error("zup.signing.failed", finding.detail.clone())
                    .with_help(format!("`{}`", finding.path)),
            );
        }
        result = result.with_details(Details::SignVerify(zup_automation::SignVerifyDetails {
            plan: crate::automation::project_path(&plan_path(&root)),
            subjects,
            finalized: 0,
            unsigned: 0,
        }));
        return Ok(result);
    }

    if !release.is_finalized() {
        return Err(crate::failure::error_with_help(
            "zup.signing.unfinalized",
            format!(
                "the release has no finalized identity for: {}",
                release.unfinalized().join(", ")
            ),
            "Every subject in the plan must be verified or explicitly allowed unsigned.",
        ));
    }

    let encoded = release.encode().map_err(|error| {
        crate::failure::error(
            "zup.signing.manifest_unencodable",
            format!("release description: {error}"),
        )
    })?;
    zup_platform::publish(&path, &encoded).map_err(|error| {
        crate::failure::error(
            "zup.signing.manifest_unwritable",
            format!("`{}`: {error}", path.display()),
        )
    })?;

    let unsigned = release.unsigned();
    reporter.log(LogLevel::Info, finalize_text(&release, &unsigned, &path));
    if !unsigned.is_empty() {
        let all_linux = unsigned.iter().all(|id| {
            release
                .artifacts
                .iter()
                .find(|artifact| artifact.id == *id)
                .is_some_and(|artifact| {
                    artifact.variants.iter().all(|variant| {
                        release
                            .variants
                            .iter()
                            .find(|entry| entry.id == *variant)
                            .is_some_and(|entry| {
                                entry.target.operating_system()
                                    == zup_core::TargetOperatingSystem::Linux
                            })
                    })
                })
        });
        reporter.log(
            LogLevel::Warning,
            if all_linux {
                format!(
                    "\n! {} artifact(s) carry no platform-native signature: {}\n  Linux \
                     installers are authenticated by their artifact digest and release identity.",
                    unsigned.len(),
                    unsigned.join(", ")
                )
            } else {
                format!(
                    "\n! {} artifact(s) are unsigned: {}\n  Windows SmartScreen will warn about them. \
                     See docs/signing.md.",
                    unsigned.len(),
                    unsigned.join(", ")
                )
            },
        );
    }
    result = result.with_details(Details::SignVerify(zup_automation::SignVerifyDetails {
        plan: crate::automation::project_path(&plan_path(&root)),
        subjects,
        finalized: count(release.artifacts.len()),
        unsigned: count(unsigned.len()),
    }));
    Ok(result)
}

fn count(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

fn finalize_text(
    release: &zup_artifact::ReleaseManifest,
    unsigned: &[&str],
    path: &Path,
) -> String {
    let mut out = format!("\nFinalized {} artifact(s):", release.artifacts.len());
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
        out.push_str(&format!(
            "\n  {} · sha256:{} · {} bytes{note}",
            artifact.path,
            finalized.digest().to_hex(),
            finalized.size()
        ));
    }
    let _ = unsigned;
    out.push_str(&format!(
        "\n\n`{}` now describes the bytes that will be published.",
        path.display()
    ));
    out
}

fn measured_of(root: &Path, step: &SigningStep) -> Option<(zup_core::Sha256Digest, u64)> {
    let path = step.subject.resolve(root);
    FileMeasured::of(&path)
        .ok()
        .map(|measured| (measured.digest, measured.size))
}

#[cfg(windows)]
fn signing_diagnostic_code(
    error: &zup_windows::signing::VerificationError,
) -> Option<&'static str> {
    let message = error.to_string().to_ascii_lowercase();
    Some(if message.contains("chain") || message.contains("trust") {
        "zup.signing.untrusted_chain"
    } else if message.contains("timestamp") {
        "zup.signing.timestamp_missing"
    } else if message.contains("publisher") || message.contains("subject") {
        "zup.signing.unexpected_publisher"
    } else if message.contains("missing") || message.contains("not found") {
        "zup.signing.subject_missing"
    } else {
        "zup.signing.failed"
    })
}

#[cfg(not(windows))]
fn signed_evidence_of(
    _root: &Path,
    _step: &SigningStep,
) -> Option<Vec<zup_signing::SigningEvidence>> {
    None
}

#[cfg(windows)]
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

fn unsigned_finalize(
    release: &mut zup_artifact::ReleaseManifest,
    root: &Path,
    step: &SigningStep,
) -> miette::Result<String> {
    let path = step.subject.resolve(root);
    if !path.is_file() {
        return Err(crate::failure::error(
            "zup.signing.subject_missing",
            format!("`{}` does not exist", path.display()),
        ));
    }
    let measured = FileMeasured::of(&path).map_err(|error| {
        crate::failure::error(
            "zup.signing.subject_unreadable",
            format!("`{}`: {error}", path.display()),
        )
    })?;
    if step.role == SigningRole::OuterArtifact {
        let id = artifact_id(release, &step.subject.path)?;
        release
            .finalize(root, &id, &measured, Vec::new())
            .map_err(|error| {
                crate::failure::error(
                    "zup.signing.finalize_failed",
                    format!("`{}`: {error}", step.subject.path),
                )
            })?;
    }
    Ok(format!(
        "unsigned · sha256:{} · {} bytes",
        measured.digest.to_hex(),
        measured.size
    ))
}

#[cfg(windows)]
fn timestamp_text(timestamp: Timestamp) -> &'static str {
    match timestamp {
        Timestamp::Rfc3161 => "RFC 3161 timestamp",
        Timestamp::LegacyOnly => "legacy timestamp only",
        Timestamp::None => "no timestamp",
    }
}

fn artifact_id(release: &zup_artifact::ReleaseManifest, path: &str) -> miette::Result<String> {
    release
        .artifacts
        .iter()
        .find(|artifact| artifact.path == path)
        .map(|artifact| artifact.id.clone())
        .ok_or_else(|| {
            crate::failure::error(
                "zup.signing.subject_not_an_artifact",
                format!("`{path}` is not an artifact in this release"),
            )
        })
}

#[cfg(windows)]
fn embedded_runtime_digest(
    artifact: &Path,
    variant: &str,
) -> miette::Result<zup_core::Sha256Digest> {
    let composed = zup_windows::UniversalArtifact::open(artifact).map_err(|error| {
        crate::failure::error(
            "zup.signing.not_a_composed_artifact",
            format!(
                "`{}` is not a composed artifact: {error}; a single-target installer is its own \
                 runtime",
                artifact.display()
            ),
        )
    })?;
    let bytes = composed.embedded_runtime(variant).map_err(|error| {
        crate::failure::error(
            "zup.signing.artifact_unreadable",
            format!("`{}`: {error}", artifact.display()),
        )
    })?;
    zup_core::hash_reader(bytes.as_slice())
        .map(|(_, digest)| digest)
        .map_err(|error| {
            crate::failure::error(
                "zup.signing.embedded_runtime_unreadable",
                format!("`{}`: {error}", artifact.display()),
            )
        })
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

    #[cfg(windows)]
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

    #[cfg(windows)]
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

    #[test]
    fn the_printed_plan_names_no_crate_and_orders_runtimes_before_installers() {
        let mut plan = plan(zup_signing::SigningRequirement::production());
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
        let printed = render(&plan);
        assert!(printed.find("native_runtime").unwrap() < printed.find("outer_artifact").unwrap());
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
