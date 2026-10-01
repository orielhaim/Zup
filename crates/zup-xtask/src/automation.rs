//! Generating and checking the artifacts derived from the automation contract.
//!
//! # What this owns
//!
//! ```text
//! schema/automation-v1.schema.json     the language-neutral contract
//! action/src/protocol.generated.ts     the Action's TypeScript declarations
//! fixtures/automation/*.json|jsonl     the golden documents both sides read
//! ```
//!
//! All three come from the Rust DTOs in `zup-automation`, through one command, and
//! `check` is the same command without the writes. CI runs `check`, so a Rust type that
//! changed without its artifacts being regenerated fails the build rather than
//! producing an Action compiled against a shape zup stopped emitting.
//!
//! # Why the fixtures are generated and not hand-written
//!
//! A fixture hand-written in TypeScript proves that a TypeScript object satisfies a
//! TypeScript interface, which is true of every fixture ever written and says nothing
//! about whether Rust serializes that shape. These are serialized by the same code that
//! writes a real result, so a change to a field name moves the fixture, the Action's
//! decoder fails, and the failure names the field.
//!
//! Nothing in a fixture is derived from the environment. There are no timestamps, no
//! absolute paths and no digests of files that only exist on the machine that wrote them,
//! so the same commit produces the same bytes on every host.

use std::path::Path;

use zup_automation::{
    Application, Artifact, ArtifactInspectDetails, AutomationResult, BuildDetails, ByteCount,
    CheckDetails, Composition, Details, Diagnostic, DiagnosticSource, Digest, DoctorCheck,
    DoctorDetails, DoctorTarget, Identifier, InspectedContent, InspectedTrust, InspectedVariant,
    LogLevel, PlanDetails, Publication, PublicationAsset, PublishDetails, PublishFailure,
    PublishStageDetails, SignPrepareDetails, SignSubject, SignVerifyDetails, StagedPackage,
    StreamEvent, StreamVersion, Target, ToolchainComponentStatus, ToolchainStatusDetails,
};

/// The committed JSON Schema, relative to the repository root.
pub const SCHEMA_PATH: &str = "schema/automation-v1.schema.json";

/// The Action's generated declarations, relative to the repository root.
pub const TYPESCRIPT_PATH: &str = "action/src/protocol.generated.ts";

/// The golden fixtures, relative to the repository root.
pub const FIXTURE_DIRECTORY: &str = "fixtures/automation";

/// One generated file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Generated {
    /// Repository-relative, `/`-separated.
    pub path: String,
    pub contents: String,
}

/// Every file the contract owns, in a stable order.
pub fn generate() -> Vec<Generated> {
    let mut files = vec![
        Generated {
            path: SCHEMA_PATH.to_owned(),
            contents: zup_automation::schema_json(),
        },
        Generated {
            path: TYPESCRIPT_PATH.to_owned(),
            contents: zup_automation::typescript(),
        },
    ];
    files.extend(fixtures());
    files
}

/// Write every file, creating parent directories.
pub fn write(root: &Path, files: &[Generated]) -> Result<Vec<String>, String> {
    let mut written = Vec::new();
    for file in files {
        let path = root.join(&file.path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("{}: {error}", parent.display()))?;
        }
        std::fs::write(&path, file.contents.as_bytes())
            .map_err(|error| format!("{}: {error}", path.display()))?;
        written.push(file.path.clone());
    }
    Ok(written)
}

/// Which committed files do not match what the Rust types produce.
///
/// Paths only, with a byte count, because a generated file is large and a diff of one is
/// unreadable in a CI log. The command to fix it is in every message.
pub fn drift(root: &Path, files: &[Generated]) -> Vec<String> {
    let mut stale = Vec::new();
    for file in files {
        match std::fs::read_to_string(root.join(&file.path)) {
            Ok(committed) if committed == file.contents => {}
            Ok(committed) => stale.push(format!(
                "{} ({} bytes committed, {} generated)",
                file.path,
                committed.len(),
                file.contents.len()
            )),
            Err(_) => stale.push(format!("{} (missing)", file.path)),
        }
    }
    stale
}

/// The golden documents, produced by serializing the real types.
///
/// Each one is a case an integration has to get right, and none of them is a
/// hypothetical: a build that succeeded, a build whose manifest was wrong, a
/// publication, a publication refused over a conflicting asset, a signature check that
/// failed, a toolchain report, a stream, and a diagnostic with a source span.
fn fixtures() -> Vec<Generated> {
    vec![
        json("build-success.json", build_success()),
        json("build-failure.json", build_failure()),
        json("check-not-composable.json", check_not_composable()),
        json("plan.json", plan()),
        json("doctor.json", doctor()),
        json("publish-stage.json", publish_stage()),
        json("publish-success.json", publish_success()),
        json("publish-conflict.json", publish_conflict()),
        json("sign-prepare.json", sign_prepare()),
        json("sign-verify-failure.json", sign_verify_failure()),
        json("artifact-inspect.json", artifact_inspect()),
        json("toolchain-status.json", toolchain_status()),
        json("diagnostic-span.json", diagnostic_span()),
        jsonl("jsonl-progress.jsonl", &jsonl_progress()),
    ]
}

fn json(name: &str, result: AutomationResult) -> Generated {
    let mut contents = serde_json::to_string_pretty(&result)
        .unwrap_or_else(|error| panic!("{name} does not serialize: {error}"));
    contents.push('\n');
    Generated {
        path: format!("{FIXTURE_DIRECTORY}/{name}"),
        contents,
    }
}

fn jsonl(name: &str, events: &[StreamEvent]) -> Generated {
    let contents = events
        .iter()
        .map(StreamEvent::to_line)
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    Generated {
        path: format!("{FIXTURE_DIRECTORY}/{name}"),
        contents,
    }
}

fn application() -> Application {
    Application {
        id: "com.acme.desktop".to_owned(),
        name: "Acme".to_owned(),
        version: "1.4.0".to_owned(),
    }
}

fn target() -> Target {
    Target::new("windows-x64", "x86_64-pc-windows-msvc")
}

fn digest(byte: char) -> Digest {
    Digest::sha256(std::iter::repeat_n(byte, 64).collect::<String>())
}

fn artifact(path: &str, kind: &'static str, mode: &'static str, size: u64) -> Artifact {
    Artifact {
        path: path.to_owned(),
        digest: digest('a'),
        size: ByteCount::new(size),
        kind: Identifier::fixed(kind),
        mode: Identifier::fixed(mode),
        id: Some("windows-x64".to_owned()),
        target: Some("x86_64-pc-windows-msvc".to_owned()),
        variants: None,
        signing: None,
    }
}

fn build_success() -> AutomationResult {
    AutomationResult::new(zup_automation::OPERATION_BUILD)
        .with_application(application())
        .with_targets(vec![target()])
        .with_artifacts(vec![artifact(
            "Acme-Windows-Setup.exe",
            "single",
            "offline",
            248_512_896,
        )])
        .with_release_manifest("dist/zup-release.json")
        .with_details(Details::Build(BuildDetails {
            signing_plan: Some("dist/zup-signing.json".to_owned()),
            pending_signatures: 1,
        }))
        .with_summary("Built 1 artifact for 1 target")
}

fn build_failure() -> AutomationResult {
    AutomationResult::new(zup_automation::OPERATION_BUILD)
        .with_application(application())
        .with_diagnostic(
            Diagnostic::error(
                "zup.manifest.unknown_target",
                "artifact `windows` includes `linux`, which is not among the selected targets",
            )
            .with_help("declare it under [build.targets.linux], or build it in its own matrix job")
            .in_file("zup.toml"),
        )
        .failed()
        .with_summary("Nothing was built")
}

/// A project that is valid but cannot produce one universal installer.
///
/// The case a warning exists for, and the reason the protocol's severity and status
/// have to agree: an error diagnostic on a success would make a green project read as
/// a broken one, and a consumer that failed on it would be wrong.
fn check_not_composable() -> AutomationResult {
    AutomationResult::new(zup_automation::OPERATION_CHECK)
        .with_application(application())
        .with_targets(vec![
            target(),
            Target::new("windows-arm64", "aarch64-pc-windows-msvc"),
        ])
        .with_diagnostic(
            Diagnostic::warning(
                "zup.check.not_composable",
                "windows-x64, windows-arm64 cannot be composed: the architectures differ",
            )
            .with_help("Build them separately with `zup build --target <profile>`."),
        )
        .with_details(Details::Check(CheckDetails {
            variants: 2,
            composition: Some(Composition {
                composable: false,
                dimension: Some(Identifier::fixed("architecture")),
                detail: "windows-x64, windows-arm64 cannot be composed: the architectures differ"
                    .to_owned(),
            }),
        }))
        .with_summary("Acme is valid · 2 file(s) across 2 target(s)")
}

/// What an install would do, which is a plan rather than a change.
fn plan() -> AutomationResult {
    AutomationResult::new(zup_automation::OPERATION_PLAN)
        .with_application(application())
        .with_targets(vec![target()])
        .with_details(Details::Plan(PlanDetails {
            scope: "user".to_owned(),
            install_directory: Some("${location.user_data}/Acme".to_owned()),
            estimated_bytes: ByteCount::new(248_512_896),
            change_count: 3,
        }))
        .with_summary("3 change(s) · 237.0 MiB under ${location.user_data}/Acme")
}

/// A project that cannot be built, with the whole check table rather than only the
/// failures: a skipped check is a question that was never answered.
fn doctor() -> AutomationResult {
    AutomationResult::new(zup_automation::OPERATION_DOCTOR)
        .with_application(application())
        .with_diagnostic(Diagnostic::error(
            "zup.doctor.not_ready",
            "1 check(s) failed across 1 target(s)",
        ))
        .with_details(Details::Doctor(DoctorDetails {
            manifest: "zup.toml".to_owned(),
            host: "x86_64-pc-windows-msvc".to_owned(),
            ready: false,
            checks: 3,
            targets: vec![DoctorTarget::new(
                "default",
                "x86_64-pc-windows-msvc",
                Identifier::fixed("fail"),
                vec![
                    DoctorCheck::new(
                        Identifier::fixed("canonical_target"),
                        Identifier::fixed("pass"),
                        "canonical target is `x86_64-pc-windows-msvc`",
                        None,
                    ),
                    DoctorCheck::new(
                        Identifier::fixed("update_root"),
                        Identifier::fixed("skip"),
                        "no [updates] section; no trusted update root is embedded",
                        Some("zup.toml".to_owned()),
                    ),
                    DoctorCheck::new(
                        Identifier::fixed("output_parent"),
                        Identifier::fixed("fail"),
                        "output parent is not writable",
                        Some("dist".to_owned()),
                    ),
                ],
            )],
        }))
        .failed()
        .with_summary("not ready: 1 of 1 target(s) can be built")
}

/// A staged tree, which is the document a TUF repository signs and a static origin
/// serves.
fn publish_stage() -> AutomationResult {
    AutomationResult::new(zup_automation::OPERATION_PUBLISH_STAGE)
        .with_application(application())
        .with_targets(vec![target()])
        .with_artifacts(vec![artifact(
            "Acme-Windows-x64.zup",
            "package",
            "package",
            18_874_368,
        )])
        .with_details(Details::PublishStage(PublishStageDetails {
            web_root: "dist/web".to_owned(),
            channel: "stable".to_owned(),
            objects: 4,
            object_bytes: ByteCount::new(18_874_368),
            variants: 1,
            tuf_inputs: 2,
            release_digest: Some("b".repeat(64)),
            packages: vec![StagedPackage {
                variant: "windows-x64".to_owned(),
                names: vec!["Acme-Windows-x64.zup".to_owned()],
                size: ByteCount::new(18_874_368),
                blob_count: 4,
            }],
            thin_installers: Vec::new(),
            release_manifest: Some("dist/zup-release.json".to_owned()),
        }))
        .with_summary("Staged 4 objects · 1 variant")
}

fn publish_success() -> AutomationResult {
    AutomationResult::new(zup_automation::OPERATION_PUBLISH_GITHUB)
        .with_application(application())
        .with_artifacts(vec![artifact(
            "Acme-Windows-Setup.exe",
            "single",
            "offline",
            248_512_896,
        )])
        .with_release_manifest("dist/zup-release.json")
        .with_publication(Publication {
            provider: "github".to_owned(),
            subject: "acme/acme".to_owned(),
            tag: "v1.4.0".to_owned(),
            id: Some("1234567890123456789".to_owned()),
            state: Identifier::fixed("published"),
            url: Some("https://github.com/acme/acme/releases/tag/v1.4.0".to_owned()),
            immutable: Some(true),
            assets: vec![PublicationAsset {
                name: "Acme-Windows-Setup.exe".to_owned(),
                size: ByteCount::new(248_512_896),
                digest: Some(digest('a')),
                state: Identifier::fixed("uploaded"),
            }],
            receipt: Some("dist/github-publish.json".to_owned()),
        })
        .with_details(Details::Publish(PublishDetails {
            dry_run: false,
            receipt: Some("dist/github-publish.json".to_owned()),
            failures: Vec::new(),
        }))
        .with_summary("Published v1.4.0 with 1 asset")
}

fn publish_conflict() -> AutomationResult {
    AutomationResult::new(zup_automation::OPERATION_PUBLISH_GITHUB)
        .with_application(application())
        .with_diagnostic(Diagnostic::error(
            "zup.publish.asset_conflict",
            "Acme-Windows-Setup.exe already exists on v1.4.0 with different bytes",
        ))
        .with_details(Details::Publish(PublishDetails {
            dry_run: false,
            receipt: None,
            failures: vec![PublishFailure {
                phase: "Uploading".to_owned(),
                step: "Acme-Windows-Setup.exe".to_owned(),
                detail: "the host refused a replacement on a published release".to_owned(),
            }],
        }))
        .failed()
        .with_summary("v1.4.0 was not published")
}

/// The list an external signer has to work through, in the order it has to work
/// through it: a runtime is signed before the artifact that embeds it.
fn sign_prepare() -> AutomationResult {
    AutomationResult::new(zup_automation::OPERATION_SIGN_PREPARE)
        .with_application(application())
        .with_artifacts(vec![artifact(
            "Acme-Windows-Setup.exe",
            "single",
            "offline",
            248_512_896,
        )])
        .with_release_manifest("dist/zup-release.json")
        .with_details(Details::SignPrepare(SignPrepareDetails {
            plan: "dist/zup-signing.json".to_owned(),
            subjects: vec![SignSubject::checked(
                "Acme-Windows-Setup.exe",
                Identifier::fixed("outer_artifact"),
                Identifier::fixed("post_compose"),
                false,
                "awaiting a signature",
                Some(digest('a')),
                Some(ByteCount::new(248_512_896)),
            )],
        }))
        .with_summary("1 file(s) require a signature; sign them, then run `zup sign verify`")
}

fn sign_verify_failure() -> AutomationResult {
    AutomationResult::new(zup_automation::OPERATION_SIGN_VERIFY)
        .with_application(application())
        .with_artifacts(vec![Artifact {
            signing: Some(zup_automation::SigningState::unsigned()),
            ..artifact("Acme-Windows-Setup.exe", "single", "offline", 248_512_896)
        }])
        .with_diagnostic(Diagnostic::error(
            "zup.signing.untrusted_chain",
            "the certificate does not chain to a root this machine trusts",
        ))
        .with_details(Details::SignVerify(SignVerifyDetails {
            plan: "dist/zup-signing.json".to_owned(),
            subjects: vec![SignSubject::checked(
                "Acme-Windows-Setup.exe",
                Identifier::fixed("outer_artifact"),
                Identifier::fixed("post_compose"),
                false,
                "the signature is present but the chain is not trusted",
                Some(digest('a')),
                Some(ByteCount::new(248_512_896)),
            )],
            finalized: 0,
            unsigned: 0,
        }))
        .failed()
        .with_summary("1 of 1 check failed; the release was not finalized")
}

/// An inspection, whole. This is the one operation whose product *is* the detail, so
/// nothing is summarized away.
fn artifact_inspect() -> AutomationResult {
    AutomationResult::new(zup_automation::OPERATION_ARTIFACT_INSPECT)
        .with_application(application())
        .with_targets(vec![target()])
        .with_details(Details::ArtifactInspect(ArtifactInspectDetails {
            artifact: "Acme-Windows-Setup.exe".to_owned(),
            artifact_kind: Identifier::fixed("universal"),
            artifact_mode: Identifier::fixed("offline"),
            pin: "1.4.0".to_owned(),
            subsystem: "console".to_owned(),
            variants: vec![InspectedVariant {
                id: "windows-x64".to_owned(),
                target: "x86_64-pc-windows-msvc".to_owned(),
                frontend: "gui".to_owned(),
                logical_size: ByteCount::new(248_512_896),
                file_count: 1,
                prerequisite_count: 0,
                plugin_count: 0,
                native_execution: true,
                target_matches_binary: "matches".to_owned(),
            }],
            content: InspectedContent {
                logical_size: ByteCount::new(248_512_896),
                stored_size: ByteCount::new(18_874_368),
                content_size: ByteCount::new(18_874_368),
                shared_size: ByteCount::new(0),
                exclusive_size: ByteCount::new(18_874_368),
                unique_blob_count: 4,
                file_size: ByteCount::new(18_874_368),
            },
            trust: InspectedTrust {
                authenticode: "no certificate table".to_owned(),
                index: "valid".to_owned(),
                content_digests: "valid".to_owned(),
                variants: "complete".to_owned(),
            },
        }))
        .with_summary("Acme 1.4.0 · 1 variant(s) · 18.0 MiB on disk")
}

fn toolchain_status() -> AutomationResult {
    AutomationResult::new(zup_automation::OPERATION_TOOLCHAIN_STATUS)
        .with_details(Details::ToolchainStatus(ToolchainStatusDetails {
            zup_version: "0.0.1".to_owned(),
            host: "x86_64-pc-windows-msvc".to_owned(),
            // Absolute, and display-only: it is the answer to "where would a build on
            // this machine look", which is the question `toolchain status` exists to
            // answer. Nothing reads it back, so it is the one path in the protocol that
            // is not relative, and the fixture spells it with an environment variable so
            // a committed document stays the same on every host.
            cache: "%LOCALAPPDATA%\\zup\\toolchain\\0.0.1".to_owned(),
            complete: false,
            components: vec![ToolchainComponentStatus {
                component: "runtime gui for x86_64-pc-windows-msvc".to_owned(),
                found: false,
                source: None,
                path: None,
                problem: Some("no copy was found in any search location".to_owned()),
            }],
        }))
        .with_diagnostic(Diagnostic::error(
            "zup.toolchain.component_missing",
            "1 of 7 components are missing, so a build here cannot compose anything",
        ))
        .failed()
        .with_summary("not ready: 1 of 7 component(s) missing")
}

fn diagnostic_span() -> AutomationResult {
    let mut diagnostic = Diagnostic::error(
        "zup.manifest.unknown_target",
        "the build target matrix must contain at least one profile",
    )
    .with_help("declare a profile under [build.targets.<profile>]");
    diagnostic.source = Some(DiagnosticSource::at("zup.toml", 4, 1));
    AutomationResult::new(zup_automation::OPERATION_CHECK)
        .with_application(application())
        .with_diagnostic(diagnostic)
        .with_details(Details::Check(CheckDetails {
            variants: 0,
            composition: None,
        }))
        .failed()
        .with_summary("Nothing was checked")
}

fn jsonl_progress() -> Vec<StreamEvent> {
    let mut events = vec![
        StreamEvent::Version(StreamVersion::new(Identifier::fixed(
            zup_automation::OPERATION_PUBLISH_STAGE,
        ))),
        StreamEvent::Phase {
            phase: "compose".to_owned(),
            message: "Composing the release graph".to_owned(),
        },
        StreamEvent::Progress {
            completed: 1,
            total: 4,
            label: "4 objects".to_owned(),
        },
        StreamEvent::Artifact {
            artifact: artifact("Acme-Windows-x64.zup", "package", "package", 18_874_368),
        },
        StreamEvent::Log {
            level: LogLevel::Info,
            message: "→ Wrote dist/web/releases/stable.json".to_owned(),
        },
        StreamEvent::Publication {
            publication: Publication {
                provider: "local".to_owned(),
                subject: "dist/web".to_owned(),
                tag: "stable".to_owned(),
                id: None,
                state: Identifier::fixed("staged"),
                url: None,
                immutable: None,
                assets: Vec::new(),
                receipt: None,
            },
        },
        StreamEvent::Diagnostic {
            diagnostic: Diagnostic {
                severity: zup_automation::Severity::Warning,
                code: Identifier::fixed("zup.publish.stage_thin_skipped"),
                message: "no [updates] section, so no thin installer was composed".to_owned(),
                source: Some(DiagnosticSource::file("zup.toml")),
                help: Some("add [updates] to publish a bootstrapper".to_owned()),
            },
        },
    ];
    events.push(StreamEvent::completed(
        AutomationResult::new(zup_automation::OPERATION_PUBLISH_STAGE)
            .with_application(application())
            .with_targets(vec![target()])
            .with_artifacts(vec![artifact(
                "Acme-Windows-x64.zup",
                "package",
                "package",
                18_874_368,
            )])
            .with_details(Details::PublishStage(PublishStageDetails {
                web_root: "dist/web".to_owned(),
                channel: "stable".to_owned(),
                objects: 4,
                object_bytes: ByteCount::new(18_874_368),
                variants: 1,
                tuf_inputs: 2,
                release_digest: Some("b".repeat(64)),
                packages: vec![StagedPackage {
                    variant: "windows-x64".to_owned(),
                    names: vec!["Acme-Windows-x64.zup".to_owned()],
                    size: ByteCount::new(18_874_368),
                    blob_count: 4,
                }],
                thin_installers: Vec::new(),
                release_manifest: Some("dist/zup-release.json".to_owned()),
            }))
            .with_summary("Staged 4 objects · 1 variant"),
    ));
    events
}

/// Where a generated file belongs, for a diagnostic.
pub fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every fixture is a document this build's own types accept, and every
    /// *failed* one says why. A fixture that does not parse is worse than no
    /// fixture, because a consumer test that reads it would be testing a document
    /// zup cannot produce.
    ///
    /// Only the fixtures: the schema describes them rather than being one, and the
    /// declarations are TypeScript.
    #[test]
    fn every_fixture_is_a_document_zup_can_produce() {
        for file in fixtures() {
            if file.path.ends_with(".jsonl") {
                let lines: Vec<&str> = file.contents.lines().collect();
                assert!(!lines.is_empty(), "{}", file.path);
                for line in lines {
                    serde_json::from_str::<StreamEvent>(line)
                        .unwrap_or_else(|error| panic!("{}: {error}\n{line}", file.path));
                }
                continue;
            }
            let result: AutomationResult = serde_json::from_str(&file.contents)
                .unwrap_or_else(|error| panic!("{}: {error}", file.path));
            result
                .validate()
                .unwrap_or_else(|error| panic!("{}: {error}", file.path));
        }
    }

    /// Nothing in a fixture describes the machine that wrote it. A fixture with an
    /// absolute path is a fixture that cannot be committed, and a fixture with a
    /// timestamp is a fixture that produces a diff on every run.
    #[test]
    fn no_fixture_names_a_build_machine_or_a_clock() {
        for file in fixtures() {
            for banned in ["C:\\", "C:/Users", "/Users/", "/home/", "\\Users\\"] {
                assert!(
                    !file.contents.contains(banned),
                    "`{banned}` in {}",
                    file.path
                );
            }
            // Quoted, because a fixture is prose as well as data and `validated`
            // contains `date`. A wall clock in a *field* is the thing that makes a
            // document non-reproducible.
            for banned in [
                "\"timestamp\"",
                "\"time\"",
                "\"elapsed\"",
                "\"date\"",
                "\"clock\"",
            ] {
                assert!(
                    !file.contents.contains(banned),
                    "`{banned}` in {}",
                    file.path
                );
            }
        }
    }
}
