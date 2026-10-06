//! Platform-neutral distribution artifacts for zup.
//!
//! A **target profile** is a friendly name. A **distribution variant** is one
//! fully resolved native target: its content graph, its runtime template, its
//! requirements, and the build-machine inputs it came from. A **distribution
//! artifact** is a file a user downloads, which may contain or reference any
//! number of variants.
//!
//! ```text
//! TargetProfile
//!     ↓
//! ResolvedTargetConfig
//!     ↓
//! DistributionVariant
//!     ↓
//! ArtifactComposer
//!     ↓
//! DistributionArtifact
//! ```
//!
//! # The graph
//!
//! An artifact is a descriptor graph in the shape of an OCI image index, without
//! being OCI:
//!
//! ```text
//! ArtifactIndex                 small, read first, validated on its own
//!   ├── artifact               id, kind, mode, pin, launcher, output name
//!   ├── variants[]             platform, frontend, requirements, manifest digest
//!   └── tables.blobs           digest of the blob table
//!
//! BlobTable                     one entry per unique blob, packed into segments
//! VariantManifest              one per variant: the installer's content plan
//! Blob                          one per unique payload byte range, stored once
//! Runtime                       one per variant: the native runtime image
//! ```
//!
//! Nothing is inlined to be shared. Two variants that need the same bytes point
//! at the same digest, and the store holds it once, so composing costs
//! deduplication rather than multiplication.
//!
//! # Properties
//!
//! - **Deterministic.** Documents are canonically serialized, collections are
//!   sorted by construction, and segment layout is a function of digest order.
//! - **Content addressed.** Every reference is a digest and a length, and every
//!   read is verified against both.
//! - **Bounds checked.** Each document type has its own size, count, and
//!   contiguity bounds, checked before anything is dereferenced.
//! - **Forward versioned.** `required_features` names the shape features a
//!   document needs; a reader that does not implement a required feature fails
//!   closed rather than guessing.
//! - **Selection is typed.** A host reports what it is, a platform backend
//!   reports what it can execute, and selection is a total function over the
//!   two. A tie is a refusal, not a coin flip.
//!
//! # OCI
//!
//! OCI is an adapter, not a foundation. [`oci`] translates a composed graph into
//! an OCI image index and per-variant manifests, keeping the same digests, and
//! can write a local `oci-layout` directory as proof. No OCI type appears
//! outside that module, and `zup-core` does not depend on this crate.

#![forbid(unsafe_code)]

mod compat;
mod compose;
mod descriptor;
mod error;
mod index;
mod media_type;
mod platform;
pub mod preset;
mod release;
mod select;
mod store;
mod table;
mod variant;
mod web;

pub mod oci;

pub use compat::{
    CompatibilityDimension, Incompatibility, Incompatible, LauncherSubsystem, VariantShape,
    check_compatibility, frontend_subsystem, requires_native, satisfies_minimum_host,
};
pub use compose::{
    ArtifactComposer, ArtifactGraph, ArtifactRequest, ComposedImage, ComposedManifest,
    CompositionStorage,
};
pub use descriptor::{Descriptor, from_bounded_json, to_canonical_json};
pub use error::ArtifactError;
pub use index::{
    ARTIFACT_SCHEMA, ArtifactDescriptor, ArtifactIndex, ArtifactKind, ArtifactMode, ArtifactPin,
    ArtifactSavings, ArtifactTables, FEATURE_CHANNEL_PIN, FEATURE_SHARED_CAS,
    FEATURE_VARIANT_MANIFESTS, LauncherStrategy, SUPPORTED_FEATURES, VariantContentSet,
    check_features, covered_targets, remote_blob_path, savings, shared_requirements,
};
pub use media_type::{MAX_BLOBS, MAX_INDEX_BYTES, MAX_SEGMENTS, MAX_VARIANTS, MediaType};
pub use platform::{HostArchitecture, Platform};
pub use release::{
    BuildArtifact, Measured, RELEASE_MANIFEST_NAME, RELEASE_SCHEMA, ReleaseArtifact, ReleaseError,
    ReleaseManifest, ReleaseRuntime, ReleaseVariant, SigningPlan, SingleTarget,
};
pub use select::{
    CandidateVariant, Compatibility, HostExecution, ScoredCandidate, Selection, rank_all, score,
    select, select_from_index, select_in,
};
pub use store::{
    ArtifactView, ContentSource, FileSegments, MemorySegments, MemorySource, MetadataSet,
    SegmentReader, SegmentSource, SpoolSource, limit_for,
};
pub use table::{BLOB_TABLE_SCHEMA, BlobEntry, BlobTable, MAX_SEGMENT_BYTES};
pub use variant::{
    DistributionVariant, HostVersion, MinimumHost, PlatformCapability, PlatformOs,
    VARIANT_MANIFEST_SCHEMA, VariantDescriptor, VariantDescriptorContent, VariantManifest,
    VariantRequirements, VariantSources,
};
pub use web::{ReleaseFile, WebExport, WebTree, export_web_tree, export_web_tree_with};
