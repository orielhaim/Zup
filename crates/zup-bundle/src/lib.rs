//! Payload and container layer for zup.
//!
//! Portable payload access by `RelativePath` + expected digest. The final
//! `.zup` bundle/container format is not implemented yet.

mod format;
mod payload;

pub use format::{
    BundleError, BundlePayloadSource, BundleWriter, CompiledPluginArtifact, EmbeddedBundle,
    MAX_PLUGIN_AOT_TOTAL_BYTES, PayloadEntry, PeSubsystem, PluginArtifact, PortableBuildPlan,
    build_self_contained_executable, embed_bundle_file, read_pe_frontend, read_pe_subsystem,
    read_pe_target, validate_pe_frontend,
};
pub use payload::{
    AutoPayloadSource, DirectoryPayloadSource, OverlayPayloadSource, PayloadError, PayloadReader,
    PayloadSource,
};
