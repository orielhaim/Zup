//! Why a session was refused.

use crate::ConfigurationError;

/// Protocol, framing, and session errors.
///
/// Every variant is a refusal. None of them is a retry hint: a peer that
/// produced one is not one this build can talk to, and continuing would mean
/// guessing.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WireError {
    #[error("frame exceeds maximum size {max}")]
    FrameTooLarge { max: usize },

    #[error("malformed frame: {0}")]
    Malformed(String),

    #[error("protocol version mismatch: expected {expected}, found {found}")]
    VersionMismatch { expected: u32, found: u32 },

    #[error("session id mismatch")]
    SessionMismatch,

    #[error("the session is already closed")]
    SessionClosed,

    #[error("duplicate sequence {sequence}")]
    DuplicateSequence { sequence: u64 },

    #[error("sequence went backwards: {previous} -> {next}")]
    SequenceRegression { previous: u64, next: u64 },

    #[error("capability not provided: {0}")]
    MissingCapability(String),

    #[error("expected a {expected} message, found `{found}`")]
    UnexpectedMessage {
        expected: &'static str,
        found: &'static str,
    },

    #[error("configuration is not one this protocol will carry: {0}")]
    Configuration(#[from] ConfigurationError),
}
