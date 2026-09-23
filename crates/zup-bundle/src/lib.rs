//! Payload and container layer for zup.
//!
//! Portable payload access by `RelativePath` + expected digest. The final
//! `.zup` bundle/container format is not implemented yet.

#![forbid(unsafe_code)]

mod format;
mod payload;

pub use format::{
    BundleError, BundlePayloadSource, BundleWriter, EmbeddedBundle, PortableBuildPlan,
    append_bundle_file_to_executable, append_bundle_to_executable, build_self_contained_executable,
};
pub use payload::{
    AutoPayloadSource, DirectoryPayloadSource, PayloadError, PayloadReader, PayloadSource,
};
