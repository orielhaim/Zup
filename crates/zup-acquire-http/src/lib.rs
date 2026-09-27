//! The HTTP transport for content acquisition.
//!
//! This crate is the whole of what `zup-acquire` does not want to know: a
//! connection pool, timeouts, redirects, range requests, and a retry policy. It
//! contributes exactly one thing to the engine — an [`HttpSource`] that fills the
//! verified cache — and it makes no claim about where content came from, because
//! the cache already proved that.
//!
//! # What the network is
//!
//! An untrusted transport. Every response is treated as hostile: the scheme is
//! checked before a request is made, the wire length is capped by a descriptor
//! that authenticated metadata supplied, the final digest is proved locally, and
//! nothing is executed or deserialized merely because a request returned 200.
//!
//! # What trust is
//!
//! Authenticated metadata and content identity. A blob may come from any origin
//! in the configured list, in any order, and is published only if it hashes to
//! the digest a TUF-authenticated release descriptor named. An origin that
//! appears first is a preference, not a grant.
//!
//! # Version notes
//!
//! - `reqwest` 0.13 is already the workspace version and already provides
//!   HTTP/2 over one pooled connection. Several immutable objects over one
//!   connection is the multiplexed behaviour this design wants, so HTTP/3 is not
//!   a milestone requirement — and `reqwest` still treats its HTTP/3 API as
//!   unstable, so adopting it early would be adopting churn.
//! - `tough` 0.24 brings its own compatible HTTP stack for TUF metadata. Two
//!   major `reqwest` versions in one binary is not a price worth paying for a
//!   unification that nothing currently needs.
//! - Proxy configuration is whatever the platform and the environment already
//!   say. This crate adds no proxy handling of its own, and never logs a URL.

#![forbid(unsafe_code)]

mod client;
mod error;
mod origin;
mod retry;
mod source;

pub use client::{HttpClient, HttpClientConfig, Response, SecretHeader, TimeoutPolicy, USER_AGENT};
pub use error::{HttpError, RetryDecision, is_resumable, is_retryable_status, retry_after};
pub use origin::{Origin, OriginSet, RepositoryLocation};
pub use retry::{BackoffPolicy, RetryOutcome, RetryState};
pub use source::{HttpSource, MetadataRequest, fetch_metadata};
