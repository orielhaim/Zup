//! Portable content-addressed package and payload access for zup.
//!
//! `BundleWriter` creates schema-1 package bytes from one `TargetBuildPlan`.
//! `Package::open` and `Package::parse` verify the package and expose payload,
//! plugin, and prerequisite data without requiring an executable. `Package`
//! contains one target plan; multi-target containers are not part of this
//! format.

mod acquired;
mod format;
mod payload;
mod preset;

pub use acquired::{AcquiredPayloadSource, refuse_self_verified};
pub use format::{
    BundleWriter, CompiledPluginArtifact, MAX_PLUGIN_COMPONENT_TOTAL_BYTES,
    MAX_PLUGIN_SOURCE_BYTES, PACKAGE_FEATURE_EXTERNAL_PAYLOAD, PACKAGE_SCHEMA, Package,
    PackageError, PackageIndex, PackagePayloadSource, validate_payload_entries,
    validate_plugin_artifact,
};
pub use payload::{
    AutoPayloadSource, DirectoryPayloadSource, OverlayPayloadSource, PayloadError, PayloadReader,
    PayloadSource, is_plugin_payload_path,
};
pub use preset::{
    ASSET_SOURCE_PREFIX, PRESET_SOURCE, PresetContentError, Resolved, asset_path,
    asset_source_name, preset_path, resolve as resolve_preset_content,
};
