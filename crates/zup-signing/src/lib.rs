//! The signing, evidence and finalization contract for a zup release.
//!
//! Signing is the one step in a build that a *different tool*, on a *different
//! machine*, with a *credential zup must never hold* performs. That makes it
//! awkward to model as a function call and easy to model as a file: zup writes
//! down exactly which subjects need a signature, in what order, and what the
//! result has to prove; an external signer consumes that document and mutates
//! those files and nothing else; zup then reads the result back and records what
//! it found.
//!
//! # What this crate is, and what it refuses to be
//!
//! It is a *description*: a plan, a requirement, evidence, and the transition
//! from "what a build produced" to "what will be published". Those four things
//! are properties of a release, and they are the same on every platform.
//!
//! It is not an integration, and it knows nothing about any of them. There is no
//! `Signer` trait here, and adding one would be a mistake: Windows, macOS and
//! the package systems have different finalization sequences rather than
//! different arguments to one sequence, and a `trait Signer { fn sign(&self, path: &Path) }`
//! would have to invent fields that mean the same thing on all of them and
//! therefore mean nothing precise on any. What is genuinely shared is the shape
//! of the problem:
//!
//! ```text
//!   sign a nested subject  →  compose  →  sign the container
//!                          →  finalize the digest  →  attest  →  publish
//! ```
//!
//! A macOS release wants `sign nested code → sign the bundle → package → sign the
//! container → notarize → staple → verify → finalize`. That is eight steps where
//! the Windows pipeline has two, and one of them (`notarize`) has no Windows
//! analogue at all. The portable model records a *sequence of subjects and a
//! requirement*, and it happens to have exactly one answer to give today.
//!
//! # No credential, ever
//!
//! A plan is committed, uploaded as a workflow artifact, and printed in job logs.
//! No private key, PFX password, PKCS#11 PIN, cloud credential or API secret is
//! modelled here, and no field is capable of holding one: a plan names *files*,
//! an *expected identity*, and a *result location*. The signer owns the
//! credential; zup owns the contract and the proof.
//!
//! # The order is the content
//!
//! ```text
//! native runtime
//!        ↓ sign
//! verify runtime
//!        ↓
//! compose the outer artifact
//!        ↓ sign
//! verify the outer artifact
//!        ↓
//! finalize the digest
//!        ↓
//! attest
//!        ↓
//! publish
//! ```
//!
//! A universal artifact's native runtime is written into it as a resource and
//! later extracted to disk and executed. The outer artifact's signature covers
//! those bytes *as resource data* and does not transfer to the extracted file, so
//! an unsigned runtime is an executable that runs on a user's machine carrying no
//! signature of its own, however well the outer file is signed. Signing the
//! runtime first is what puts a signature inside the bytes that will actually
//! run.
//!
//! That is why [`SigningStage`] is ordered, why [`SigningPlan::push`] refuses to
//! produce an out-of-order plan, and why
//! [`SigningPlan::embeds`] is part of the model rather than a fact the caller has
//! to remember: an out-of-order signing pass is not a warning, it is a release
//! nobody can verify.

#![forbid(unsafe_code)]

mod evidence;
mod plan;

pub use evidence::{
    EvidenceFact, FinalizationError, FinalizedArtifact, Measured, SigningEvidence, covers_bytes,
    finalize, publisher, thumbprint, timestamp,
};
pub use plan::{
    DigestAlgorithm, SIGNING_PLAN_NAME, SIGNING_PLAN_SCHEMA, SigningPlan, SigningPlanError,
    SigningReason, SigningRequirement, SigningRole, SigningStage, SigningStep, SigningSubject,
    TimestampRequirement,
};
