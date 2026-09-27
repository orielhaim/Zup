//! Read zup release content back out of GitHub Releases.
//!
//! # The zero-infrastructure case
//!
//! A project that wants its thin installer to fetch content from a release, and
//! wants to not run a server, gets this:
//!
//! ```text
//! zup publish github
//!     ↓
//! Acme-Windows-Setup.exe        what a person downloads
//! Acme-Windows-x64.zup          one variant's content, one asset
//! zup-package-win-x64.json      the small document that names it
//! zup-release-stable.json       the authenticated release descriptor
//! zup-catalog-stable.json       the authenticated content catalog
//!     ↓
//! a thin installer, pinned to tag v1.4.0
//!     ↓
//! GET releases/download/v1.4.0/zup-release-stable.json   (discovery)
//! GET …/zup-catalog-stable.json                          (trust)
//! GET …/zup-package-win-x64.json                         (locator)
//! GET …/Acme-Windows-x64.zup                             (bytes)
//!     ↓
//! verified CAS objects, indistinguishable from a CDN's
//! ```
//!
//! No bucket, no CDN, no origin, no upload tooling. A release page that reads
//! like a release page, and content that a machine can use.
//!
//! # What it is not
//!
//! It is not a case per content object. Ten thousand blobs is one or two assets
//! per variant, because a Release page with ten thousand hexadecimal filenames
//! is unusable and GitHub's per-release asset limit is a thousand.
//!
//! It is not a second trust mechanism. Every blob is verified against the
//! release's own authenticated catalog before it reaches the cache. The package
//! descriptor says *where* the bytes are; the catalog says *what they are*, and
//! the catalog is the one that decides.
//!
//! It is not a channel system. A pinned tag is an identity and a `latest`
//! download is GitHub's idea of newest. `beta`, `nightly`, and `canary` stay on
//! the generic TUF path, where a channel is a signed pointer rather than a
//! redirect.
//!
//! # Range support is measured, not assumed
//!
//! GitHub does not document HTTP Range support for release assets. This crate
//! probes once, uses ranged reads when the host answers with a correct `206`,
//! falls back to whole pieces when it does not, and counts which happened — so
//! [`metrics`] can tell you whether the optimisation is real for your project
//! rather than assuming it is.

#![forbid(unsafe_code)]

mod descriptor;
mod error;
mod layout;
mod metrics;
mod package;
mod source;

pub use descriptor::{DESCRIPTOR_SCHEMA, FileRef, PackageDescriptor, ShardRef};
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
