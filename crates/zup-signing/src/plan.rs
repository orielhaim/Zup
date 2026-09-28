//! The signing plan: which subjects need a signature, in what order, and what
//! the signature has to satisfy.
//!
//! The plan is the *only* thing zup hands an external signer. It is deliberately
//! narrow — a list of files, an expected identity, and requirements — because a
//! document that carried the whole build plan would be a document nobody reads
//! and a place for a manifest to drift out of agreement with the release it
//! describes. See the crate documentation for why signing is a file contract
//! rather than a call.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use zup_core::{App, Sha256Digest};

/// Version of the signing plan shape.
///
/// The plan is written by one tool and read by another, possibly a different
/// version of each, so the number is explicit and a reader refuses a shape it
/// does not implement rather than guessing.
pub const SIGNING_PLAN_SCHEMA: u32 = 1;

/// The file a build writes its signing plan to.
///
/// One name, one schema, and it is not optional for a release: a pipeline that
/// cannot say what it is signing is not a pipeline zup can verify.
pub const SIGNING_PLAN_NAME: &str = "zup-signing.json";

/// What a signed subject *is*.
///
/// The role is what a signature over it will and will not mean, which is why it
/// is a closed set rather than a free-text label. A runtime's signature travels
/// with the runtime when the runtime is extracted from a composed artifact; an
/// outer artifact's signature does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SigningRole {
    /// A native installer runtime: the executable that becomes the running
    /// installer, and byte for byte the persisted maintenance executable, the
    /// elevated worker and the uninstall runner, which are copies and
    /// re-invocations of it.
    NativeRuntime,
    /// The file a person downloads and runs: a single-target installer, or a
    /// composed universal artifact.
    ///
    /// A single-target installer is also its own runtime, so it carries both
    /// roles: its signature is the outer artifact's *and* the runtime's.
    OuterArtifact,
}

impl SigningRole {
    /// The wire name, for a report.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NativeRuntime => "native_runtime",
            Self::OuterArtifact => "outer_artifact",
        }
    }
}

/// Where in a pipeline a subject is signed.
///
/// The declaration order is the signing order, and the derived [`Ord`] is the
/// ordering itself, so "which comes first" is a property of the type rather than
/// of a sort a caller has to remember to apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SigningStage {
    /// Signed before composition, then embedded. The bytes inside the container
    /// are the bytes that were signed.
    PreCompose,
    /// Signed after composition, then measured and published. Authenticode and
    /// its equivalents cover embedded resources, so signing earlier would
    /// invalidate the container.
    PostCompose,
}

impl SigningStage {
    /// The wire name, for a report.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PreCompose => "pre_compose",
            Self::PostCompose => "post_compose",
        }
    }
}

/// Why a subject is in the plan.
///
/// A signable with no stated reason is a signable nobody can audit, so the reason
/// travels with the subject rather than living in a provider's configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum SigningReason {
    /// The runtime this variant's installer and maintenance executable are.
    VariantRuntime { variant: String },
    /// A thin artifact's bare runtime: it carries the plan and no content, it is
    /// launched before anything has been downloaded, and it is therefore a
    /// distributed executable in its own right.
    ThinBootstrapper { variant: String },
    /// The file a person downloads.
    Installer { artifact: String },
}

/// One file that requires a signature, as the signer sees it.
///
/// Only what a signer needs: the path, what the bytes are now, and which
/// variants the file is or carries. Not the release description, not the plan's
/// other steps, not the build graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SigningSubject {
    /// Path relative to the release root, with `/` separators.
    pub path: String,
    /// The digest of the bytes as they stand when the plan is written.
    ///
    /// A pre-compose subject's digest is over the **unsigned** bytes: the signer
    /// is expected to change it, and that change is the point. Verification
    /// checks that a post-compose subject's bytes changed and that a
    /// pre-compose subject's *embedded* form still hashes to the value a
    /// post-compose artifact carries — not that this field survived signing.
    pub digest: Sha256Digest,
    /// The size of those bytes.
    pub size: u64,
    /// The variants this file is, or carries, a native runtime for.
    ///
    /// Empty for a subject with no embedded runtime. Populated for a pre-compose
    /// runtime so verification knows which resource to compare, and for a
    /// post-compose artifact so a report can say which executables its signature
    /// does *not* cover.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub variants: Vec<String>,
}

impl SigningSubject {
    /// The path, resolved against a release root.
    pub fn resolve(&self, release_root: &Path) -> PathBuf {
        release_root.join(self.path.replace('/', std::path::MAIN_SEPARATOR_STR))
    }
}

/// The digest algorithm a signature has to be built with.
///
/// One variant, and that is the honest state of the world: SHA-256 everywhere in
/// zup, and current guidance for every platform zup can name. The type exists so
/// the requirement is *stated* in the document a signer reads rather than assumed
/// — a signer that chose differently would produce a release whose final digest
/// zup cannot reproduce — and so the choice has somewhere to go when it changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DigestAlgorithm {
    #[default]
    Sha256,
}

impl DigestAlgorithm {
    /// The wire name, for a report and for a signer's command line.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Sha256 => "sha256",
        }
    }
}

/// Whether a timestamp has to be present, and which form is acceptable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimestampRequirement {
    /// An RFC 3161 timestamp must be present, and it is the only accepted form.
    ///
    /// A signing certificate expires, and the countersignature is what keeps a
    /// signature valid after it does; the legacy Authenticode timestamp is still
    /// inside the signed bytes but is not what current guidance asks for. A
    /// pipeline that quietly produced a legacy timestamp should fail here rather
    /// than on a user's machine.
    #[default]
    Required,
    /// No timestamp is required, and the legacy form is acceptable.
    ///
    /// What a local development certificate produces. Relaxing the requirement
    /// and refusing the legacy form are the same decision, so they are one
    /// variant: the two flags that used to express them could be set in a
    /// combination that asks for a contradiction.
    Optional,
}

/// What a signature has to satisfy.
///
/// A statement about the *identity*, never about the *key*. A subject name and a
/// certificate thumbprint are public; nothing here is a secret and nothing here
/// can be used to sign anything.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SigningRequirement {
    /// A subject the signer must carry, matched as a normalized substring.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publisher: Option<String>,
    /// A certificate thumbprint the signer must carry, compared exactly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thumbprint: Option<String>,
    /// The digest algorithm the signature must be built with.
    pub digest: DigestAlgorithm,
    /// Whether a timestamp is required.
    pub timestamp: TimestampRequirement,
    /// The platform's own trust policy must accept the chain.
    ///
    /// On by default, and off only in [`SigningRequirement::development`]: a
    /// local self-signed certificate is exactly that case, and turning this on is
    /// a statement about the *machine's* certificate store rather than about the
    /// artifact. A non-Windows verifier cannot answer it at all, so a plan that
    /// requires it says the release may only be verified on Windows.
    pub trusted_chain: bool,
}

impl Default for SigningRequirement {
    fn default() -> Self {
        Self {
            publisher: None,
            thumbprint: None,
            digest: DigestAlgorithm::default(),
            timestamp: TimestampRequirement::default(),
            trusted_chain: true,
        }
    }
}

impl SigningRequirement {
    /// The production requirement.
    pub fn production() -> Self {
        Self::default()
    }

    /// What a local development certificate satisfies.
    ///
    /// Not "a weaker production check": a separate, self-consistent set of
    /// statements, so no caller can loosen the shipped one.
    pub fn development() -> Self {
        Self {
            timestamp: TimestampRequirement::Optional,
            trusted_chain: false,
            ..Self::default()
        }
    }

    /// Name the publisher.
    pub fn signed_by(mut self, publisher: impl Into<String>) -> Self {
        self.publisher = Some(publisher.into());
        self
    }
}

/// One signing operation, and the subject it applies to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SigningStep {
    pub role: SigningRole,
    pub stage: SigningStage,
    pub reason: SigningReason,
    pub subject: SigningSubject,
}

impl SigningStep {
    /// A step for `subject`.
    pub fn new(
        role: SigningRole,
        stage: SigningStage,
        reason: SigningReason,
        subject: SigningSubject,
    ) -> Self {
        Self {
            role,
            stage,
            reason,
            subject,
        }
    }

    /// The subject's path.
    pub fn path(&self) -> &str {
        &self.subject.path
    }
}

/// The complete set of subjects a release needs signed, in signing order.
///
/// The order is an invariant of the value: [`SigningPlan::push`] inserts, so a
/// plan cannot be built out of order however the steps were discovered or
/// shuffled between two jobs. [`SigningPlan::validate`] re-checks the invariant
/// on a parsed document, because a document that arrived from somewhere is not
/// the value this crate constructed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SigningPlan {
    pub schema: u32,
    pub application: App,
    /// Relative to the release root, always `.`.
    pub root: String,
    /// The requirement every signature has to satisfy.
    pub requirement: SigningRequirement,
    /// The steps, in signing order.
    pub steps: Vec<SigningStep>,
}

impl SigningPlan {
    /// An empty plan for `application`.
    pub fn new(application: &App, requirement: SigningRequirement) -> Self {
        Self {
            schema: SIGNING_PLAN_SCHEMA,
            application: application.clone(),
            root: ".".to_owned(),
            requirement,
            steps: Vec::new(),
        }
    }

    /// Add a step, keeping the plan in signing order.
    ///
    /// Insertion rather than a later `order()` call, because a plan that is
    /// briefly out of order is a plan somebody can sign: the function that signs
    /// it reads the list as it stands, not after a sort it does not know about.
    pub fn push(&mut self, step: SigningStep) -> Result<(), SigningPlanError> {
        if self
            .steps
            .iter()
            .any(|existing| existing.subject.path == step.subject.path)
        {
            return Err(SigningPlanError::Duplicate(step.subject.path));
        }
        let at = self
            .steps
            .partition_point(|existing| signing_key(existing) <= signing_key(&step));
        self.steps.insert(at, step);
        Ok(())
    }

    /// The steps that must be signed before composition.
    pub fn pre_compose(&self) -> impl Iterator<Item = &SigningStep> {
        self.steps
            .iter()
            .filter(|step| step.stage == SigningStage::PreCompose)
    }

    /// The steps that must be signed after composition.
    pub fn post_compose(&self) -> impl Iterator<Item = &SigningStep> {
        self.steps
            .iter()
            .filter(|step| step.stage == SigningStage::PostCompose)
    }

    /// The step for `path`.
    pub fn step(&self, path: &str) -> Option<&SigningStep> {
        self.steps.iter().find(|step| step.subject.path == path)
    }

    /// The pre-compose steps `step` composes, and so must be signed *and
    /// verified* before `step` may be finalized.
    ///
    /// The ordering dependency, as data. A post-compose step that carries a
    /// variant depends on the pre-compose step for that variant, and a verifier
    /// that finalizes the container without proving the embedded bytes are the
    /// signed ones has published a release whose executable nobody signed. An
    /// empty result means the step embeds no runtime, which is what a
    /// single-target installer's *own* signature is for.
    pub fn embeds(&self, step: &SigningStep) -> Vec<&SigningStep> {
        let Some(variant) = step.subject.variants.first() else {
            return Vec::new();
        };
        self.pre_compose()
            .filter(|candidate| {
                candidate
                    .subject
                    .variants
                    .iter()
                    .any(|held| held == variant)
            })
            .collect()
    }

    /// Serialize canonically.
    pub fn encode(&self) -> Result<Vec<u8>, SigningPlanError> {
        Ok(serde_json::to_vec(self)?)
    }

    /// Parse a plan, rejecting an unrecognized schema before the rest.
    pub fn parse(bytes: &[u8]) -> Result<Self, SigningPlanError> {
        let plan: Self = serde_json::from_slice(bytes)?;
        plan.validate()?;
        Ok(plan)
    }

    /// Check the plan on its own terms, including the ordering invariant.
    pub fn validate(&self) -> Result<(), SigningPlanError> {
        if self.schema != SIGNING_PLAN_SCHEMA {
            return Err(SigningPlanError::Schema { found: self.schema });
        }
        if self.root != "." {
            return Err(SigningPlanError::Invalid);
        }
        for (index, step) in self.steps.iter().enumerate() {
            let path = &step.subject.path;
            if path.is_empty()
                || path.starts_with('/')
                || path.contains(':')
                || path.split('/').any(|segment| segment == "..")
            {
                return Err(SigningPlanError::Path(path.clone()));
            }
            if self.steps[..index]
                .iter()
                .any(|earlier| earlier.subject.path == *path)
            {
                return Err(SigningPlanError::Duplicate(path.clone()));
            }
            if index > 0 && signing_key(&self.steps[index - 1]) > signing_key(step) {
                return Err(SigningPlanError::OutOfOrder(path.clone()));
            }
        }
        Ok(())
    }
}

/// The total order a plan is kept in: the stage, then the path.
///
/// Two steps in the same stage have no real ordering between them, so the path
/// breaks the tie. Without it the order would depend on discovery order, and two
/// machines that built the same release would produce two different plans.
fn signing_key(step: &SigningStep) -> (SigningStage, &str) {
    (step.stage, step.subject.path.as_str())
}

/// Why a signing plan cannot be used.
#[derive(Debug, thiserror::Error)]
pub enum SigningPlanError {
    #[error("the signing plan is schema {found}; this build reads schema {SIGNING_PLAN_SCHEMA}")]
    Schema { found: u32 },
    #[error("`{0}` is not a path inside the release root")]
    Path(String),
    #[error("`{0}` appears more than once in the signing plan")]
    Duplicate(String),
    #[error(
        "`{0}` is signed out of order; a pre-compose subject is signed after a post-compose one"
    )]
    OutOfOrder(String),
    #[error("the signing plan is malformed")]
    Invalid,
    #[error("signing plan JSON: {0}")]
    Json(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use semver::Version;
    use zup_core::{AppId, NonEmptyString};

    fn app() -> App {
        App {
            id: AppId::new("com.acme.desktop").unwrap(),
            name: NonEmptyString::new("Acme").unwrap(),
            version: Version::parse("1.4.0").unwrap(),
            publisher: None,
            main: None,
            description: None,
        }
    }

    fn subject(path: &str, variants: &[&str]) -> SigningSubject {
        SigningSubject {
            path: path.to_owned(),
            digest: Sha256Digest::from_bytes([0xab; 32]),
            size: 1,
            variants: variants.iter().map(|v| (*v).to_owned()).collect(),
        }
    }

    fn artifact_step(path: &str, variants: &[&str]) -> SigningStep {
        SigningStep::new(
            SigningRole::OuterArtifact,
            SigningStage::PostCompose,
            SigningReason::Installer {
                artifact: "windows".to_owned(),
            },
            subject(path, variants),
        )
    }

    fn runtime_step(variant: &str) -> SigningStep {
        SigningStep::new(
            SigningRole::NativeRuntime,
            SigningStage::PreCompose,
            SigningReason::VariantRuntime {
                variant: variant.to_owned(),
            },
            subject(&format!("runtime/{variant}.exe"), &[variant]),
        )
    }

    /// The ordering is a property of the value, not of a sort a caller has to
    /// remember to apply: a runtime discovered *after* the artifact that embeds
    /// it is still signed first, and a plan that came out of `push` can never be
    /// out of order.
    #[test]
    fn a_runtime_is_signed_before_the_artifact_that_embeds_it() {
        let mut plan = SigningPlan::new(&app(), SigningRequirement::production());
        plan.push(artifact_step(
            "dist/Acme-Windows-Setup.exe",
            &["windows-x64"],
        ))
        .unwrap();
        plan.push(runtime_step("windows-x64")).unwrap();
        assert_eq!(
            plan.steps
                .iter()
                .map(|step| (step.stage, step.path().to_owned()))
                .collect::<Vec<_>>(),
            vec![
                (
                    SigningStage::PreCompose,
                    "runtime/windows-x64.exe".to_owned()
                ),
                (
                    SigningStage::PostCompose,
                    "dist/Acme-Windows-Setup.exe".to_owned()
                ),
            ]
        );
        assert_eq!(plan.pre_compose().count(), 1);
        assert_eq!(plan.post_compose().count(), 1);
    }

    /// The ordering dependency, as data. This is the check `zup sign verify`
    /// walks before it will finalize a composed artifact, and it is the whole
    /// reason the plan is a plan rather than a list of files.
    #[test]
    fn a_plan_states_which_signed_subject_each_artifact_embeds() {
        let mut plan = SigningPlan::new(&app(), SigningRequirement::production());
        let runtime = runtime_step("windows-x64");
        plan.push(runtime.clone()).unwrap();
        plan.push(artifact_step("dist/Acme-Setup.exe", &["windows-x64"]))
            .unwrap();
        plan.push(artifact_step("dist/Acme-Thin.exe", &[])).unwrap();

        let composed = plan.step("dist/Acme-Setup.exe").expect("the artifact");
        assert_eq!(plan.embeds(composed), vec![&runtime]);

        // A thin artifact embeds no runtime, and a plan that says so is not
        // asserting a check it cannot make.
        let thin = plan.step("dist/Acme-Thin.exe").expect("the thin artifact");
        assert!(plan.embeds(thin).is_empty());
    }

    #[test]
    fn a_plan_round_trips_and_refuses_what_it_cannot_describe() {
        let mut plan = SigningPlan::new(&app(), SigningRequirement::production().signed_by("Acme"));
        plan.push(runtime_step("windows-x64")).unwrap();
        plan.push(artifact_step("dist/Acme-Setup.exe", &["windows-x64"]))
            .unwrap();
        let encoded = plan.encode().expect("encode");
        assert_eq!(SigningPlan::parse(&encoded).expect("parse"), plan);

        let mut wrong = plan.clone();
        wrong.schema = SIGNING_PLAN_SCHEMA + 1;
        assert!(matches!(
            SigningPlan::parse(&wrong.encode().expect("encode")),
            Err(SigningPlanError::Schema { .. })
        ));

        for escape in ["../escape.exe", "/absolute.exe", "C:/out.exe", ""] {
            let mut bad = plan.clone();
            bad.steps[1].subject.path = escape.to_owned();
            assert!(
                matches!(
                    SigningPlan::parse(&bad.encode().expect("encode")),
                    Err(SigningPlanError::Path(_))
                ),
                "{escape} must not be a signable path"
            );
        }
    }

    /// A parsed document is not the value this crate constructed, so the
    /// ordering invariant is re-checked on the way in. Without this a hand-edited
    /// or reordered plan would sign a runtime after the artifact that embeds it,
    /// which is exactly the release the crate exists to prevent.
    #[test]
    fn a_document_arriving_out_of_order_is_refused() {
        let mut plan = SigningPlan::new(&app(), SigningRequirement::production());
        plan.push(runtime_step("windows-x64")).unwrap();
        plan.push(artifact_step("dist/Acme-Setup.exe", &["windows-x64"]))
            .unwrap();
        plan.steps.swap(0, 1);
        assert!(matches!(
            SigningPlan::parse(&plan.encode().expect("encode")),
            Err(SigningPlanError::OutOfOrder(_))
        ));
    }

    #[test]
    fn two_steps_for_one_path_are_refused() {
        let mut plan = SigningPlan::new(&app(), SigningRequirement::production());
        plan.push(runtime_step("windows-x64")).unwrap();
        assert!(matches!(
            plan.push(runtime_step("windows-x64")),
            Err(SigningPlanError::Duplicate(path)) if path == "runtime/windows-x64.exe"
        ));
    }

    /// A plan is committed, uploaded as a workflow artifact, and printed in job
    /// logs. If a field could hold a secret, this is the place it would leak
    /// from.
    #[test]
    fn a_plan_names_no_credential() {
        let mut plan = SigningPlan::new(&app(), SigningRequirement::production().signed_by("Acme"));
        plan.push(runtime_step("windows-x64")).unwrap();
        plan.push(artifact_step("dist/Acme-Setup.exe", &["windows-x64"]))
            .unwrap();
        let encoded = String::from_utf8(plan.encode().expect("encode")).expect("utf8");
        for word in [
            "password",
            "pfx",
            "secret",
            "token",
            "private",
            "key",
            "pin",
            "credential",
        ] {
            assert!(!encoded.contains(word), "the plan mentions `{word}`");
        }
    }

    /// A requirement that asks for "no timestamp" and "refuse the legacy
    /// timestamp" at once is a contradiction a single flag used to be able to
    /// express. One variant makes it unrepresentable.
    #[test]
    fn relaxing_the_timestamp_relaxes_the_whole_timestamp_rule() {
        let development = SigningRequirement::development();
        assert_eq!(development.timestamp, TimestampRequirement::Optional);
        assert!(!development.trusted_chain);
        let production = SigningRequirement::production();
        assert_eq!(production.timestamp, TimestampRequirement::Required);
        assert!(production.trusted_chain);
    }
}
