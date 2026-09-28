//! Why an HTTP acquisition failed.
//!
//! Every variant names a class of failure rather than a raw library error,
//! because the thing a caller needs is "is this worth retrying somewhere else"
//! and "what do I tell the person". A transport error message never carries a
//! URL: an origin is named by index and host, and a URL that could hold
//! credentials is never formatted into a diagnostic.

use std::time::Duration;

/// A failure while acquiring content over HTTP.
#[derive(Debug, thiserror::Error)]
pub enum HttpError {
    /// A URL could not be built from an origin and a relative path.
    #[error("origin {origin} cannot address {path}: {reason}")]
    Address {
        origin: String,
        path: String,
        reason: String,
    },
    /// The request never completed.
    #[error("origin {origin} could not be reached: {reason}")]
    Transport { origin: String, reason: String },
    /// The response status says the request will never succeed as sent.
    #[error("origin {origin} answered {status}")]
    Status { origin: String, status: u16 },
    /// The response body was longer than the descriptor permits.
    #[error("origin {origin} sent more than {limit} bytes")]
    TooLarge { origin: String, limit: u64 },
    /// The response was shorter than the descriptor declares.
    #[error("origin {origin} sent {received} bytes; the descriptor declares {expected}")]
    Truncated {
        origin: String,
        received: u64,
        expected: u64,
    },
    /// The server answered a range request with the whole object.
    ///
    /// This is not an error the user needs to see: the transfer simply restarts
    /// from zero, which is what a CDN without range support requires.
    #[error("origin {origin} does not honour range requests")]
    RangeUnsupported { origin: String },
    /// The server answered a range request with a range that does not line up.
    #[error(
        "origin {origin} answered range `{declared}` for a request that asked for `{requested}`"
    )]
    RangeMismatch {
        origin: String,
        requested: String,
        declared: String,
    },
    /// The redirect policy refused a hop.
    #[error("origin {origin} redirected somewhere that is not permitted: {reason}")]
    Redirect { origin: String, reason: String },
    /// A body stream ended early.
    #[error("origin {origin} closed the connection after {received} bytes")]
    Disconnected { origin: String, received: u64 },
    /// The caller's sink refused a chunk.
    ///
    /// The cache is the only sink that exists today, and its refusals are a bound
    /// or a digest mismatch - both of which mean the content is not what was
    /// promised, which is a fact about the origin rather than about this client.
    #[error("origin {origin} could not accept the content: {reason}")]
    Body { origin: String, reason: String },
    /// The caller cancelled.
    #[error("the transfer was cancelled")]
    Cancelled,
    /// The cache refused the content.
    #[error("{0}")]
    Cache(#[from] zup_acquire::CacheError),
}

impl HttpError {
    /// Whether trying the same request against a different origin could work.
    ///
    /// Origin is not the only variable, so a transport failure and a server
    /// error are both worth another origin; a refusal the server will repeat
    /// anywhere is not.
    pub const fn is_tryable_elsewhere(&self) -> bool {
        !matches!(
            self,
            Self::Cancelled | Self::Cache(_) | Self::Address { .. } | Self::Redirect { .. }
        )
    }

    /// The origin this failure came from, for the per-origin failure budget.
    pub fn origin(&self) -> Option<&str> {
        match self {
            Self::Address { origin, .. }
            | Self::Transport { origin, .. }
            | Self::Status { origin, .. }
            | Self::TooLarge { origin, .. }
            | Self::Truncated { origin, .. }
            | Self::RangeUnsupported { origin, .. }
            | Self::RangeMismatch { origin, .. }
            | Self::Redirect { origin, .. }
            | Self::Disconnected { origin, .. }
            | Self::Body { origin, .. } => Some(origin),
            Self::Cancelled | Self::Cache(_) => None,
        }
    }

    /// A short, credential-free reason for a log line.
    pub fn reason(&self) -> String {
        match self {
            Self::Transport { reason, .. } => reason.clone(),
            Self::Status { status, .. } => format!("status {status}"),
            Self::Disconnected { received, .. } => format!("closed after {received} bytes"),
            Self::Body { reason, .. } => reason.clone(),
            Self::Truncated { .. } => "short response".to_owned(),
            Self::TooLarge { .. } => "response too large".to_owned(),
            Self::RangeUnsupported { .. } => "no range support".to_owned(),
            Self::RangeMismatch { .. } => "range mismatch".to_owned(),
            Self::Address { reason, .. } => reason.clone(),
            Self::Redirect { reason, .. } => reason.clone(),
            Self::Cancelled => "cancelled".to_owned(),
            Self::Cache(error) => error.to_string(),
        }
    }
}

/// Whether a status code is worth sending the same request again.
///
/// The rule is deliberately narrow. A `404` will still be a `404` on the next
/// attempt and on every other origin, so retrying it only turns a clear refusal
/// into a slow one. A `429` and a `503` are the server asking for patience, and
/// a `408` is the server saying it gave up on a request it could have served.
pub const fn is_retryable_status(status: u16) -> bool {
    matches!(status, 408 | 429 | 500 | 502 | 503 | 504)
}

/// What to do about a response that carries a `Retry-After` header.
///
/// The header is honoured when it is a number of seconds, because a server that
/// tells you when to come back is more authoritative than any local backoff
/// curve. An HTTP-date is not parsed: an installer has no business trusting a
/// client's clock, and a wrong date would produce either a stall or a stampede.
pub fn retry_after(header: Option<&str>) -> Option<Duration> {
    let value = header?.trim();
    let seconds: u64 = value.parse().ok()?;
    Some(Duration::from_secs(seconds.clamp(1, 300)))
}

/// What a retry loop concluded about a failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryDecision {
    /// Send it again after this long.
    Again(Duration),
    /// Stop. This will not improve.
    Stop,
    /// Stop for this origin, and try the next one.
    Elsewhere,
}

/// Whether a body read failure is worth resuming rather than restarting.
///
/// A connection that drops mid-body is the normal shape of an interrupted
/// network, and the partial is already on disk with a verified prefix, so
/// resuming is both cheap and safe. A server that closes cleanly with fewer
/// bytes than it promised is a different thing: the object is wrong, not the
/// connection.
pub const fn is_resumable(error: &HttpError) -> bool {
    matches!(
        error,
        HttpError::Disconnected { .. } | HttpError::Transport { .. }
    )
}
