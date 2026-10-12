#![forbid(unsafe_code)]

mod compat;
mod compose;
mod format;
mod index;
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
pub use format::{
    ArtifactError, Descriptor, MAX_BLOBS, MAX_INDEX_BYTES, MAX_SEGMENTS, MAX_VARIANTS, MediaType,
    from_bounded_json, to_canonical_json,
};
pub use index::{
    ARTIFACT_SCHEMA, ArtifactDescriptor, ArtifactIndex, ArtifactKind, ArtifactMode, ArtifactPin,
    ArtifactSavings, ArtifactTables, FEATURE_CHANNEL_PIN, FEATURE_SHARED_CAS,
    FEATURE_VARIANT_MANIFESTS, LauncherStrategy, SUPPORTED_FEATURES, VariantContentSet,
    check_features, covered_targets, remote_blob_path, savings, shared_requirements,
};
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
