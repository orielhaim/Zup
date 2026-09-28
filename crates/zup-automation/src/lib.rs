//! The versioned wire contract zup's developer CLI reports to machines.
//!
//! # What this crate is
//!
//! The DTOs, and only the DTOs: one final result per operation, one diagnostic
//! shape, one artifact shape, one event stream. Every producer - `zup build`,
//! `zup publish github`, `zup sign verify`, `zup doctor`, `zup toolchain status` -
//! reports through these types, so a CI system, a release pipeline and the official
//! GitHub Action read one document shape rather than one per command.
//!
//! # What this crate is not
//!
//! It is not the implementation, the domain model, or the presentation. `zup build`
//! still knows what an artifact graph is; `zup-publish` still owns `PublishReport`;
//! `zup doctor` still owns its check table. Each of those is a better model for its
//! own question, and the translation into these types happens once, at the developer
//! CLI's composition layer, in one module that is the only place the two vocabularies
//! meet.
//!
//! It is also **not** the installed application runtime's protocol. That one lives in
//! `zup-presentation` as `RuntimeEvent`/`RuntimeResult` and describes installing,
//! modifying, repairing, updating and uninstalling on a user's own machine. Those are
//! two products with two consumers; one wire format for both would be a compromise
//! that is worse for each.
//!
//! # Version policy
//!
//! The protocol version is `MAJOR.MINOR` and lives in every document, in every stream's
//! first line, and in this crate's [`PROTOCOL`] constant.
//!
//! ```text
//! same major, any minor   a consumer accepts it, and ignores fields and event
//!                         types it does not know
//! different major         a consumer refuses it
//! ```
//!
//! This is not SemVer. There are no patch levels, no pre-release identifiers and no
//! build metadata, because a wire contract has exactly two questions: can I read this,
//! and can I keep reading it when it grows.
//!
//! - **Minor** increments for a backward-compatible addition: a new optional field, a
//!   new event type, a new diagnostic code, a new artifact kind, a new operation name.
//!   An existing consumer is unaffected, and a *newer minor* is always accepted.
//! - **Major** increments when an existing field changes meaning, changes type, becomes
//!   required, or disappears. There is no negotiation and no partial acceptance: a
//!   consumer that does not understand a major refuses rather than guesses.
//!
//! The rules live in code in [`ProtocolVersion::accepts`] and are tested against both
//! directions, because a version field that is not exercised is a comment.
//!
//! # Compatibility, from the producer's side
//!
//! A consumer is told to ignore what it does not know, so the producer is obliged to
//! make ignoring safe:
//!
//! - Optional and absent are the same thing. A field is either always present (possibly
//!   `null`) or never present; it is not present half the time.
//! - [`Identifier`] is open. A new artifact kind, mode, severity-adjacent state,
//!   diagnostic code or operation name is an additive change, not a breaking one, and
//!   the types that carry them do not enumerate the values.
//! - [`StreamEvent`] decodes an event type it does not know into [`StreamEvent::Unknown`]
//!   rather than failing, so a stream stays readable across a minor bump.
//!
//! # Numeric safety
//!
//! Every byte count crosses into JavaScript as a [`ByteCount`], which refuses a value
//! above `2^53 - 1` rather than emitting a number whose meaning differs between
//! languages. Counts that are structurally small (line numbers, item totals) are
//! `u32`, which is inside the safe range by construction. Identifiers that a provider
//! owns - a release id - are strings, because whether it fits in a double is the
//! provider's business, not zup's.
//!
//! # Paths
//!
//! A path in this contract is one of three things, and the field name says which:
//!
//! | Field | Meaning |
//! | --- | --- |
//! | [`Artifact::path`] | release-relative, `/`-separated, relative to the release root |
//! | [`AutomationResult::release_manifest`] | project-relative, `/`-separated |
//! | [`DiagnosticSource::file`] | as the diagnostic names it, usually project-relative |
//!
//! A build-machine absolute path is never an identity. Nothing here round-trips an
//! `OsString`: a path that is not valid UTF-8 is rendered lossily and a consumer is
//! expected to display it, not to hand it back.

#![forbid(unsafe_code)]

mod artifact;
mod diagnostic;
mod identifier;
mod operation;
mod publication;
mod result;
mod stream;
mod version;

#[cfg(feature = "bindings")]
mod bindings;
#[cfg(feature = "bindings")]
mod schema;

#[cfg(test)]
mod tests;

#[cfg(feature = "bindings")]
pub use crate::bindings::typescript;

pub use crate::artifact::{
    Artifact, ByteCount, Digest, MAX_SAFE_BYTES, SigningEvidence, SigningState,
};
pub use crate::diagnostic::{Diagnostic, DiagnosticSource, FALLBACK_CODE, Severity};
pub use crate::identifier::{Identifier, IdentifierError};
pub use crate::operation::{
    ALL_OPERATIONS, OPERATION_ARTIFACT_INSPECT, OPERATION_BUILD, OPERATION_CHECK, OPERATION_DOCTOR,
    OPERATION_PLAN, OPERATION_PUBLISH_GITHUB, OPERATION_PUBLISH_STAGE, OPERATION_SIGN_PREPARE,
    OPERATION_SIGN_VERIFY, OPERATION_TOOLCHAIN_CLEAN, OPERATION_TOOLCHAIN_INSTALL,
    OPERATION_TOOLCHAIN_STATUS, Operation,
};
pub use crate::publication::{Publication, PublicationAsset};
pub use crate::result::{
    Application, ArtifactInspectDetails, AutomationResult, BuildDetails, CheckDetails, Composition,
    ContractError, Details, DoctorCheck, DoctorDetails, DoctorTarget, InspectedContent,
    InspectedTrust, InspectedVariant, PlanDetails, PublishDetails, PublishFailure,
    PublishStageDetails, SignPrepareDetails, SignSubject, SignVerifyDetails, StagedPackage, Status,
    Target, ToolchainCleanDetails, ToolchainComponentStatus, ToolchainInstallDetails,
    ToolchainStatusDetails,
};
pub use crate::stream::{LogLevel, StreamEvent, StreamVersion};
pub use crate::version::{PROTOCOL, ProtocolError, ProtocolVersion};

#[cfg(feature = "bindings")]
pub use crate::schema::{SCHEMA_ID, schema_json};

/// The zup release that produced a document.
///
/// The crate version *is* the tool version, so this is `env!` rather than a constant
/// somebody has to keep in step. It appears in the first line of a `--format jsonl`
/// stream and in nothing else: a consumer that wanted the tool version in the final
/// result could ask `zup --version`.
pub const ZUP_VERSION: &str = env!("CARGO_PKG_VERSION");
