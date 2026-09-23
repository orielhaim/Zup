//! Payload and container layer for zup.
//!
//! Portable payload access by `RelativePath` + expected digest. The final
//! `.zup` bundle/container format is not implemented yet.

mod format;
mod payload;

pub use format::{
    BundleError, BundlePayloadSource, BundleWriter, EmbeddedBundle, PortableBuildPlan,
    build_self_contained_executable, embed_bundle_file,
};
pub use payload::{
    AutoPayloadSource, DirectoryPayloadSource, PayloadError, PayloadReader, PayloadSource,
};
