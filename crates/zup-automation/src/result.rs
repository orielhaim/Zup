//! The one document every machine-mode command ends with.
//!
//! # Why one envelope
//!
//! A consumer that has to learn a different shape per command has to learn N shapes and
//! still has to handle the Nth+1 that somebody adds. So the fields a general
//! integration needs — did it work, what was built, what failed, where do I look — are
//! in the envelope, and everything specific to one command is in
//! [`Details`].
//!
//! # Why not one bag of forty nullable fields
//!
//! The other way to avoid per-command shapes is a struct with `check_result`,
//! `build_result`, `publish_result`, `doctor_result`, …, all nullable. A consumer
//! cannot tell "this field does not apply" from "this field is missing because the
//! producer is older", and every new command adds a column to a table nobody reads.
//! One envelope plus one tagged payload keeps the common fields required and
//! enumerable, and makes the payload's *type* carry the operation's identity.
//!
//! # What is required
//!
//! [`AutomationResult::validate`] is the consumer's half of the contract, and it is
//! deliberately unforgiving about the fields a consumer reads without checking. A
//! document that parses but cannot be used is the failure mode that produces a summary
//! with `—` in it and no error anywhere.

use serde::{Deserialize, Serialize};

use crate::artifact::{Artifact, ByteCount};
use crate::diagnostic::Diagnostic;
use crate::identifier::Identifier;
use crate::publication::Publication;
use crate::version::PROTOCOL;

/// Whether the operation did what it was asked.
#[cfg_attr(feature = "bindings", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// It did, and the diagnostics that came with it did not stop it.
    Success,
    /// It did not. The diagnostics say why, and the process exited nonzero.
    Failure,
}

impl Status {
    /// The wire name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failure => "failure",
        }
    }
}

/// The application a command acted on.
#[cfg_attr(feature = "bindings", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Application {
    /// The reverse-DNS identifier, e.g. `com.acme.desktop`.
    pub id: String,
    /// The display name.
    pub name: String,
    /// The version, as the manifest spells it.
    pub version: String,
}

/// One selected target profile, and the machine it resolves to.
#[cfg_attr(feature = "bindings", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Target {
    /// The profile name from `[build.targets]`.
    pub profile: String,
    /// The canonical target triple the profile resolves to.
    pub target: String,
}

impl Target {
    /// A profile and its triple.
    pub fn new(profile: impl Into<String>, target: impl Into<String>) -> Self {
        Self {
            profile: profile.into(),
            target: target.into(),
        }
    }
}

/// The final result of one zup operation.
///
/// Every field is always written. The nullable ones carry `null` rather than being
/// omitted, so a consumer reads `result.publication` and gets `null` instead of having
/// to distinguish "not applicable" from "produced by an older zup", and so a validator
/// can require all eleven.
#[cfg_attr(feature = "bindings", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutomationResult {
    /// The contract this document is written to.
    pub protocol: crate::version::ProtocolVersion,
    /// The operation that ran, in zup's own vocabulary.
    pub operation: Identifier,
    /// Whether it did what it was asked.
    pub status: Status,
    /// The application, when the command read a manifest.
    #[cfg_attr(feature = "bindings", schemars(required))]
    pub application: Option<Application>,
    /// The targets the operation selected.
    pub targets: Vec<Target>,
    /// The files the operation produced.
    pub artifacts: Vec<Artifact>,
    /// The release description, project-relative, when one was written.
    #[cfg_attr(feature = "bindings", schemars(required))]
    pub release_manifest: Option<String>,
    /// Where a release went, when one was published.
    #[cfg_attr(feature = "bindings", schemars(required))]
    pub publication: Option<Publication>,
    /// Everything zup wants the reader to know. Present on success too.
    pub diagnostics: Vec<Diagnostic>,
    /// One line a human would have read. Never parsed.
    #[cfg_attr(feature = "bindings", schemars(required))]
    pub summary: Option<String>,
    /// The operation's own result, beyond what every integration needs.
    #[cfg_attr(feature = "bindings", schemars(required))]
    pub details: Option<Details>,
}

impl AutomationResult {
    /// A result with only the fields every document has.
    ///
    /// A builder rather than a struct literal because the required set is five fields
    /// and the optional set is eight; filling eight `None`s at every call site is how
    /// one of them gets a wrong value.
    pub fn new(operation: impl Into<String>) -> Self {
        let operation = Identifier::parse(&operation.into())
            .expect("a zup operation name satisfies the wire grammar");
        Self {
            protocol: PROTOCOL,
            operation,
            status: Status::Success,
            application: None,
            targets: Vec::new(),
            artifacts: Vec::new(),
            release_manifest: None,
            publication: None,
            diagnostics: Vec::new(),
            summary: None,
            details: None,
        }
    }

    /// Set the summary line.
    pub fn with_summary(mut self, summary: impl Into<String>) -> Self {
        self.summary = Some(summary.into());
        self
    }

    /// Set the application.
    pub fn with_application(mut self, application: Application) -> Self {
        self.application = Some(application);
        self
    }

    /// Set the selected targets.
    pub fn with_targets(mut self, targets: Vec<Target>) -> Self {
        self.targets = targets;
        self
    }

    /// Set the produced artifacts.
    pub fn with_artifacts(mut self, artifacts: Vec<Artifact>) -> Self {
        self.artifacts = artifacts;
        self
    }

    /// Set the release description path.
    pub fn with_release_manifest(mut self, path: impl Into<String>) -> Self {
        self.release_manifest = Some(path.into());
        self
    }

    /// Set the publication.
    pub fn with_publication(mut self, publication: Publication) -> Self {
        self.publication = Some(publication);
        self
    }

    /// Set the operation's own payload.
    pub fn with_details(mut self, details: Details) -> Self {
        self.details = Some(details);
        self
    }

    /// Mark the operation as failed.
    pub fn failed(mut self) -> Self {
        self.status = Status::Failure;
        self
    }

    /// Add a diagnostic.
    pub fn with_diagnostic(mut self, diagnostic: Diagnostic) -> Self {
        self.diagnostics.push(diagnostic);
        self
    }

    /// Add diagnostics.
    pub fn with_diagnostics(mut self, diagnostics: impl IntoIterator<Item = Diagnostic>) -> Self {
        self.diagnostics.extend(diagnostics);
        self
    }

    /// Every field a consumer reads without checking, checked.
    ///
    /// A conformance check rather than a parser: a Rust deserializer already guarantees
    /// the types, and what it cannot guarantee is that the document says something. A
    /// success with an error diagnostic, a failure with no diagnostic, an artifact with
    /// an ungrammatical digest — each is a document a consumer would act on wrongly.
    pub fn validate(&self) -> Result<(), ContractError> {
        if self.status == Status::Failure && self.diagnostics.is_empty() {
            return Err(ContractError::FailureWithoutDiagnostic {
                operation: self.operation.to_string(),
            });
        }
        if let Some(error) = self
            .diagnostics
            .iter()
            .find(|diagnostic| diagnostic.severity == crate::diagnostic::Severity::Error)
            && self.status == Status::Success
        {
            return Err(ContractError::ErrorDiagnosticOnSuccess {
                operation: self.operation.to_string(),
                code: error.code.to_string(),
            });
        }
        for artifact in &self.artifacts {
            if !artifact.digest.is_well_formed() {
                return Err(ContractError::MalformedDigest {
                    path: artifact.path.clone(),
                });
            }
            if !artifact.size.is_exact() {
                return Err(ContractError::UnsafeInteger {
                    path: artifact.path.clone(),
                });
            }
        }
        if let Some(publication) = &self.publication
            && publication.tag.trim().is_empty()
        {
            return Err(ContractError::PublicationWithoutTag {
                operation: self.operation.to_string(),
            });
        }
        if let Some(details) = &self.details
            && details.operation() != self.operation.as_str()
        {
            return Err(ContractError::DetailsForAnotherOperation {
                operation: self.operation.to_string(),
                kind: details.operation().to_string(),
            });
        }
        Ok(())
    }
}

/// Why a document is not a usable result.
#[cfg_attr(feature = "bindings", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContractError {
    /// A failure result with nothing explaining it.
    FailureWithoutDiagnostic { operation: String },
    /// A success result carrying an error diagnostic.
    ErrorDiagnosticOnSuccess { operation: String, code: String },
    /// An artifact whose digest is the wrong length, or not hex.
    MalformedDigest { path: String },
    /// An artifact whose size is not representable in every consumer.
    UnsafeInteger { path: String },
    /// A publication with no tag.
    PublicationWithoutTag { operation: String },
    /// A payload whose tag names a different operation than the envelope does.
    DetailsForAnotherOperation { operation: String, kind: String },
}

impl std::fmt::Display for ContractError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::FailureWithoutDiagnostic { operation } => {
                write!(f, "`{operation}` failed without saying why")
            }
            Self::ErrorDiagnosticOnSuccess { operation, code } => write!(
                f,
                "`{operation}` reports success while carrying the error `{code}`"
            ),
            Self::MalformedDigest { path } => {
                write!(
                    f,
                    "`{path}` has a digest that is not a lowercase hex SHA-256"
                )
            }
            Self::UnsafeInteger { path } => {
                write!(f, "`{path}` has a size no consumer can represent exactly")
            }
            Self::PublicationWithoutTag { operation } => {
                write!(f, "`{operation}` published something with no tag")
            }
            Self::DetailsForAnotherOperation { operation, kind } => {
                write!(f, "`{operation}` carries the payload of `{kind}`")
            }
        }
    }
}

impl std::error::Error for ContractError {}

/// What one operation produced, beyond the envelope.
///
/// Tagged by name rather than nested under one struct with three nullable fields, so
/// a consumer that switches on `details.kind` gets an exhaustive answer and one that
/// meets a `kind` from a newer minor can still read the envelope.
///
/// Every variant is tagged with its *operation* name, so `details.kind` and
/// `operation` are the same string. One vocabulary, not two that have to be kept in
/// step, and a consumer can route on either without knowing the difference.
#[cfg_attr(feature = "bindings", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum Details {
    /// `zup build`.
    #[serde(rename = "build")]
    Build(BuildDetails),
    /// `zup check`.
    #[serde(rename = "check")]
    Check(CheckDetails),
    /// `zup plan`.
    #[serde(rename = "plan")]
    Plan(PlanDetails),
    /// `zup doctor`.
    #[serde(rename = "doctor")]
    Doctor(DoctorDetails),
    /// `zup publish stage`.
    #[serde(rename = "publish.stage")]
    PublishStage(PublishStageDetails),
    /// `zup publish github`.
    #[serde(rename = "publish.github")]
    Publish(PublishDetails),
    /// `zup sign prepare`.
    #[serde(rename = "sign.prepare")]
    SignPrepare(SignPrepareDetails),
    /// `zup sign verify`.
    #[serde(rename = "sign.verify")]
    SignVerify(SignVerifyDetails),
    /// `zup toolchain install`.
    #[serde(rename = "toolchain.install")]
    ToolchainInstall(ToolchainInstallDetails),
    /// `zup toolchain status`.
    #[serde(rename = "toolchain.status")]
    ToolchainStatus(ToolchainStatusDetails),
    /// `zup toolchain clean`.
    #[serde(rename = "toolchain.clean")]
    ToolchainClean(ToolchainCleanDetails),
    /// `zup artifact inspect`.
    #[serde(rename = "artifact.inspect")]
    ArtifactInspect(ArtifactInspectDetails),
}

impl Details {
    /// The operation this payload belongs to, which is also its wire tag.
    pub fn operation(&self) -> &'static str {
        match self {
            Self::Build(_) => crate::operation::OPERATION_BUILD,
            Self::Check(_) => crate::operation::OPERATION_CHECK,
            Self::Plan(_) => crate::operation::OPERATION_PLAN,
            Self::Doctor(_) => crate::operation::OPERATION_DOCTOR,
            Self::PublishStage(_) => crate::operation::OPERATION_PUBLISH_STAGE,
            Self::Publish(_) => crate::operation::OPERATION_PUBLISH_GITHUB,
            Self::SignPrepare(_) => crate::operation::OPERATION_SIGN_PREPARE,
            Self::SignVerify(_) => crate::operation::OPERATION_SIGN_VERIFY,
            Self::ToolchainInstall(_) => crate::operation::OPERATION_TOOLCHAIN_INSTALL,
            Self::ToolchainStatus(_) => crate::operation::OPERATION_TOOLCHAIN_STATUS,
            Self::ToolchainClean(_) => crate::operation::OPERATION_TOOLCHAIN_CLEAN,
            Self::ArtifactInspect(_) => crate::operation::OPERATION_ARTIFACT_INSPECT,
        }
    }
}

/// What a build produced that is not an artifact.
#[cfg_attr(feature = "bindings", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct BuildDetails {
    /// The signing plan, project-relative, when one was written.
    pub signing_plan: Option<String>,
    /// How many files the plan says an external signer has to touch.
    pub pending_signatures: usize,
}

/// What a check established.
#[cfg_attr(feature = "bindings", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CheckDetails {
    /// How many variant descriptions the selected targets resolved into.
    pub variants: usize,
    /// Whether the selected targets can be composed into one artifact, and why not when
    /// they cannot. `None` means fewer than two were selected, which is not a finding.
    pub composition: Option<Composition>,
}

/// Whether a set of targets can become one artifact.
#[cfg_attr(feature = "bindings", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Composition {
    /// Whether they can.
    pub composable: bool,
    /// The dimension that refused, when it did.
    pub dimension: Option<Identifier>,
    /// The one-line reason, in zup's words.
    pub detail: String,
}

/// What an install plan would do.
#[cfg_attr(feature = "bindings", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PlanDetails {
    /// `user` or `machine`.
    pub scope: String,
    /// Where it would install, when the project chose a directory.
    pub install_directory: Option<String>,
    /// What it would write.
    pub estimated_bytes: ByteCount,
    /// How many individual changes it holds.
    pub change_count: usize,
}

/// What `zup doctor` found.
///
/// The whole check table crosses the wire, green rows included. A readiness check that
/// was *skipped* is not the same fact as one that *passed* — it says the answer was
/// never obtained — so a consumer that could only see the failures would report a
/// project as ready on the strength of checks that never ran.
#[cfg_attr(feature = "bindings", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct DoctorDetails {
    /// The manifest it read, project-relative.
    pub manifest: String,
    /// The build host's canonical target triple.
    pub host: String,
    /// Whether every selected target is ready to build.
    pub ready: bool,
    /// How many checks ran.
    pub checks: usize,
    /// Per-profile verdicts and the checks underneath each.
    pub targets: Vec<DoctorTarget>,
}

/// One profile's readiness.
///
/// No `Default`: a `status` is an [`Identifier`], and an empty string is not one. That
/// is the point of the type — a consumer that switches on `status` gets a value it can
/// match, and a producer cannot forget to set it.
#[cfg_attr(feature = "bindings", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoctorTarget {
    /// The profile name.
    pub profile: String,
    /// The triple it resolves to.
    pub target: String,
    /// `pass`, `fail` or `skip`.
    pub status: Identifier,
    /// Every check that ran, in report order.
    pub checks: Vec<DoctorCheck>,
}

impl DoctorTarget {
    /// One profile's verdict.
    pub fn new(
        profile: impl Into<String>,
        target: impl Into<String>,
        status: Identifier,
        checks: Vec<DoctorCheck>,
    ) -> Self {
        Self {
            profile: profile.into(),
            target: target.into(),
            status,
            checks,
        }
    }
}

/// One readiness check.
#[cfg_attr(feature = "bindings", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoctorCheck {
    /// The check's kind, e.g. `manifest_compile`.
    pub kind: Identifier,
    /// `pass`, `fail` or `skip`.
    pub status: Identifier,
    /// What it found.
    pub message: String,
    /// The file it examined, when it examined one.
    pub path: Option<String>,
}

impl DoctorCheck {
    /// One check's row.
    pub fn new(
        kind: Identifier,
        status: Identifier,
        message: impl Into<String>,
        path: Option<String>,
    ) -> Self {
        Self {
            kind,
            status,
            message: message.into(),
            path,
        }
    }

    /// Whether this check found a problem.
    pub fn failed(&self) -> bool {
        self.status.is("fail")
    }
}

/// What a staged release holds.
#[cfg_attr(feature = "bindings", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PublishStageDetails {
    /// The staged web tree, project-relative.
    pub web_root: String,
    /// The channel the tree answers to.
    pub channel: String,
    /// How many distinct content objects the tree carries.
    pub objects: u32,
    /// How big they are together.
    pub object_bytes: ByteCount,
    /// How many variants the release describes.
    pub variants: u32,
    /// How many documents `tuftool` is meant to sign.
    pub tuf_inputs: u32,
    /// The digest of the release descriptor, so a consumer can prove which release the
    /// tree is without reading it.
    pub release_digest: Option<String>,
    /// One transport package per variant, when `--packages` was asked for.
    pub packages: Vec<StagedPackage>,
    /// The two thin installers, when `--thin` was asked for.
    pub thin_installers: Vec<Artifact>,
    /// The release description folded from a build matrix, when `--release-dir` was
    /// given.
    pub release_manifest: Option<String>,
}

/// One transport package.
#[cfg_attr(feature = "bindings", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StagedPackage {
    /// The variant it carries.
    pub variant: String,
    /// The files, by flat asset name. Several when a package was sharded.
    pub names: Vec<String>,
    /// How big they are together.
    pub size: ByteCount,
    /// How many content objects went in.
    pub blob_count: u32,
}

/// What a publication did, beyond the release it produced.
#[cfg_attr(feature = "bindings", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PublishDetails {
    /// Whether nothing was written.
    pub dry_run: bool,
    /// Where the provider's receipt went, project-relative.
    pub receipt: Option<String>,
    /// The steps that failed, which is what a failed publication needs and what a
    /// successful one does not.
    pub failures: Vec<PublishFailure>,
}

/// One step a publication could not complete.
#[cfg_attr(feature = "bindings", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishFailure {
    /// The provider's phase name.
    pub phase: String,
    /// The file or check it was on.
    pub step: String,
    /// Why.
    pub detail: String,
}

/// What an external signer has to touch.
#[cfg_attr(feature = "bindings", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SignPrepareDetails {
    /// The plan that was written, project-relative.
    pub plan: String,
    /// The subjects, in signing order. Order is the plan's: a runtime is signed before
    /// the artifact that embeds it.
    pub subjects: Vec<SignSubject>,
}

/// What verification concluded, per file.
#[cfg_attr(feature = "bindings", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SignVerifyDetails {
    /// The plan that was read, project-relative.
    pub plan: String,
    /// Every subject, verified or not.
    pub subjects: Vec<SignSubject>,
    /// How many artifacts now carry a finalized published identity.
    pub finalized: u32,
    /// How many are finalized but carry no signature.
    pub unsigned: u32,
}

/// One file signing has an opinion about.
#[cfg_attr(feature = "bindings", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignSubject {
    /// Release-relative path, `/`-separated.
    pub path: String,
    /// `native_runtime` or `outer_artifact`.
    pub role: Identifier,
    /// `pre_compose` or `post_compose`.
    pub stage: Identifier,
    /// The variants it belongs to.
    pub variants: Vec<String>,
    /// Whether the check passed. `false` is a report, not a refusal: `--report-only`
    /// exists so a pipeline can see every failure before it decides to stop.
    pub verified: bool,
    /// What was established, in one line.
    pub detail: String,
    /// The digest measured from the bytes on disk, when they could be read.
    pub digest: Option<crate::artifact::Digest>,
    /// The size of those bytes.
    pub size: Option<ByteCount>,
}

impl SignSubject {
    /// A subject nobody has looked at yet, which is what `sign prepare` reports.
    pub fn pending(path: impl Into<String>, role: Identifier, stage: Identifier) -> Self {
        Self {
            path: path.into(),
            role,
            stage,
            variants: Vec::new(),
            verified: false,
            detail: "awaiting a signature".to_owned(),
            digest: None,
            size: None,
        }
    }

    /// A subject that was checked, with what was measured.
    pub fn checked(
        path: impl Into<String>,
        role: Identifier,
        stage: Identifier,
        verified: bool,
        detail: impl Into<String>,
        digest: Option<crate::artifact::Digest>,
        size: Option<ByteCount>,
    ) -> Self {
        Self {
            path: path.into(),
            role,
            stage,
            variants: Vec::new(),
            verified,
            detail: detail.into(),
            digest,
            size,
        }
    }
}

/// What a toolchain install copied.
#[cfg_attr(feature = "bindings", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ToolchainInstallDetails {
    /// The zup release the bytes came from.
    pub zup_version: String,
    /// The directory they were read from.
    pub source: String,
    /// The directory they now live in.
    pub cache: String,
    /// The component file names, sorted.
    pub components: Vec<String>,
}

/// What a build here would resolve.
#[cfg_attr(feature = "bindings", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ToolchainStatusDetails {
    /// The zup release these components have to come from.
    pub zup_version: String,
    /// The build host's canonical target triple.
    pub host: String,
    /// Where this version's cache lives on this machine.
    ///
    /// Absolute, and the one path in this contract that is: it answers "where would a
    /// build here look", which is the question this operation exists to answer, and
    /// nothing reads it back. It is for display and for matching against a machine's
    /// own state — never a release identity.
    pub cache: String,
    /// Whether every component a build needs is usable.
    pub complete: bool,
    /// One entry per component, in the order a build asks for them.
    pub components: Vec<ToolchainComponentStatus>,
}

/// One toolchain component's state.
#[cfg_attr(feature = "bindings", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolchainComponentStatus {
    /// The component, in the wording every refusal uses.
    pub component: String,
    /// Whether a usable copy was found.
    pub found: bool,
    /// Which arm of the search produced it: `override`, `root`, `cache` or `staged`.
    pub source: Option<Identifier>,
    /// The file, when one was found.
    pub path: Option<String>,
    /// Why it was refused, when it was.
    pub problem: Option<String>,
}

/// What a clean removed, or would remove.
#[cfg_attr(feature = "bindings", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ToolchainCleanDetails {
    /// The directory that was cleaned.
    pub cache: String,
    /// True when nothing was changed.
    pub dry_run: bool,
    /// One entry per removed version or file.
    pub removed: Vec<String>,
    /// The versions that were left alone.
    pub kept: Vec<String>,
}

/// What an artifact contains.
///
/// The whole report, because inspection is the one operation whose product *is* the
/// detail: a caller asking what a file contains wants the content accounting and the
/// trust questions, not a summary of them.
#[cfg_attr(feature = "bindings", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactInspectDetails {
    /// The file that was inspected, as it was named on the command line.
    pub artifact: String,
    /// `single`, `universal` or `thin`.
    pub artifact_kind: Identifier,
    /// `offline` or `online`.
    pub artifact_mode: Identifier,
    /// Whether the artifact always installs one version or follows a channel.
    pub pin: String,
    /// The launcher experience it presents.
    pub subsystem: String,
    /// One entry per variant it carries.
    pub variants: Vec<InspectedVariant>,
    /// What the artifact costs, and what composing it saved.
    pub content: InspectedContent,
    /// What can be proven about its trust, one field per question.
    pub trust: InspectedTrust,
}

/// One variant inside an artifact.
#[cfg_attr(feature = "bindings", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct InspectedVariant {
    pub id: String,
    pub target: String,
    pub frontend: String,
    /// The size this variant's content is worth, counting shared bytes.
    pub logical_size: ByteCount,
    pub file_count: u32,
    pub prerequisite_count: u32,
    pub plugin_count: u32,
    /// Whether the variant refuses to run under a compatibility layer.
    pub native_execution: bool,
}

/// What an artifact costs.
#[cfg_attr(feature = "bindings", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct InspectedContent {
    /// Every variant's content, counted once per variant.
    pub logical_size: ByteCount,
    /// The distinct content the artifact actually stores.
    pub stored_size: ByteCount,
    /// The distinct content before compression.
    pub content_size: ByteCount,
    /// Bytes more than one variant needs.
    pub shared_size: ByteCount,
    /// Bytes only one variant needs.
    pub exclusive_size: ByteCount,
    pub unique_blob_count: u32,
    /// Bytes the file on disk occupies.
    pub file_size: ByteCount,
}

/// What can be proven about an artifact's trust.
///
/// Each field is one *question*, and the words in it are the answer to that question
/// and no other. "The Authenticode digest matches" and "Windows trusts this publisher"
/// are different facts about different authorities, and a report that collapsed them
/// into one word would be claiming the second from evidence for the first.
#[cfg_attr(feature = "bindings", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct InspectedTrust {
    /// Whether the image carries a signature structure whose digest covers these bytes.
    pub authenticode: String,
    /// Whether the index parses and agrees with the content beside it.
    pub index: String,
    /// Whether every advertised content digest verifies, `named` when the artifact
    /// carries none to verify.
    pub content_digests: String,
    /// Whether every variant the index names is complete.
    pub variants: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifact::{Digest, SigningState};
    use crate::diagnostic::Severity;
    use crate::operation::OPERATION_BUILD;

    fn artifact() -> Artifact {
        Artifact {
            path: "Acme-Setup.exe".to_owned(),
            digest: Digest::sha256("a".repeat(64)),
            size: ByteCount::new(2048),
            kind: Identifier::fixed("single"),
            mode: Identifier::fixed("offline"),
            id: Some("windows-x64".to_owned()),
            target: Some("x86_64-pc-windows-msvc".to_owned()),
            variants: None,
            signing: Some(SigningState::unsigned()),
        }
    }

    /// The shape from the protocol documentation, produced by the Rust types. If this
    /// test and `docs/automation.md` disagree, one of them is a lie, and this one is
    /// regenerated.
    #[test]
    fn the_documented_envelope_is_the_shipped_envelope() {
        let result = AutomationResult::new(OPERATION_BUILD)
            .with_application(Application {
                id: "com.acme.desktop".to_owned(),
                name: "Acme".to_owned(),
                version: "1.4.0".to_owned(),
            })
            .with_artifacts(vec![artifact()])
            .with_release_manifest("dist/zup-release.json")
            .with_summary("Built 1 artifact");
        let value = serde_json::to_value(&result).unwrap();
        assert_eq!(value["protocol"], "1.0");
        assert_eq!(value["operation"], "build");
        assert_eq!(value["status"], "success");
        assert_eq!(value["application"]["version"], "1.4.0");
        assert_eq!(value["release_manifest"], "dist/zup-release.json");
        assert!(value["publication"].is_null());
        assert!(value["artifacts"].is_array());
        assert!(value["diagnostics"].is_array());
        assert_eq!(value["summary"], "Built 1 artifact");
        result.validate().expect("a conforming result");
    }

    /// Three refusals a consumer would otherwise act on wrongly. A failure with no
    /// reason, a success carrying an error, and a digest that is not a digest.
    #[test]
    fn a_document_that_parses_but_cannot_be_used_is_refused() {
        let failed = AutomationResult::new(OPERATION_BUILD).failed();
        assert!(matches!(
            failed.validate(),
            Err(ContractError::FailureWithoutDiagnostic { .. })
        ));

        let contradicted = AutomationResult::new(OPERATION_BUILD)
            .with_diagnostic(Diagnostic::error("zup.build.digest_mismatch", "changed"));
        assert!(matches!(
            contradicted.validate(),
            Err(ContractError::ErrorDiagnosticOnSuccess { .. })
        ));

        let mut broken = AutomationResult::new(OPERATION_BUILD).with_artifacts(vec![artifact()]);
        broken.artifacts[0].digest = Digest::sha256("nothex");
        assert!(matches!(
            broken.validate(),
            Err(ContractError::MalformedDigest { .. })
        ));

        let warning_only = AutomationResult::new(OPERATION_BUILD)
            .with_diagnostic(Diagnostic {
                severity: Severity::Warning,
                ..Diagnostic::error("zup.build.deferred", "signing was skipped")
            })
            .with_artifacts(vec![artifact()]);
        warning_only
            .validate()
            .expect("a warning does not contradict success");
    }
}
