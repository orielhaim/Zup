//! HTTP acquisition with bounded transfers, redirects, retries, and exact-origin credentials.
//!
//! Transport is untrusted. Authenticated descriptors bound wire lengths and the
//! acquisition cache verifies content digests before publication.

#![forbid(unsafe_code)]

mod client;
mod error;
mod origin;
mod retry;
mod source;

pub use client::{
    Body, HttpClient, HttpClientConfig, Range, Request, Response, SecretHeader, TimeoutPolicy,
    USER_AGENT,
};
pub use error::{HttpError, RetryDecision, is_resumable, is_retryable_status, retry_after};
pub use origin::{Origin, OriginSet, RepositoryLocation};
pub use retry::BackoffPolicy;
pub use source::{HttpSource, MetadataRequest, fetch_metadata};
