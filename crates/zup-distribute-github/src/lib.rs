//! Verified content transfer through GitHub release package assets.
//!
//! Source authentication belongs to `zup-update`. This crate verifies package
//! descriptors and content against the authenticated catalog.

#![forbid(unsafe_code)]

mod descriptor;
mod error;
mod layout;
mod metrics;
mod package;
mod source;

pub use descriptor::{
    DESCRIPTOR_SCHEMA, FileRef, MAX_DESCRIPTOR_BYTES, PackageDescriptor, ShardRef,
};
pub use error::{DistributeError, PackageError};
pub use layout::{ReleaseLayout, ReleaseRef};
pub use metrics::{Metrics, MetricsSnapshot};
pub use package::{
    Document, FEATURE_FRAMES, Frame, HEADER_LEN, Index, LEVEL, MAGIC, MAX_METADATA, Metadata,
    PackageHeader, SCHEMA, SUPPORTED_FEATURES, Shard, Writer, decode_frame,
};
pub use source::{GithubContentSource, Package, RangeSupport, client, load_descriptor, open};

/// A `reqwest` failure, in words that cannot carry a URL.
pub fn safe_reason(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        return "the request timed out".to_owned();
    }
    if error.is_connect() {
        return "the connection could not be made".to_owned();
    }
    if error.is_request() {
        return "the request could not be built".to_owned();
    }
    if error.is_body() || error.is_decode() {
        return "the response body could not be read".to_owned();
    }
    if error.is_redirect() {
        return "a redirect was refused".to_owned();
    }
    "the connection failed".to_owned()
}
