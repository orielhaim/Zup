//! Versioned IPC wire types for zup parent ↔ elevated worker.
//!
//! Pure data: no Tokio, Kameo, Windows, or GPUI.

#![forbid(unsafe_code)]

mod messages;

pub use messages::{
    Capabilities, Completed, ExecuteTransaction, FILE_TRANSACTIONS_V1, Failed, LIFECYCLE_V1,
    MANAGED_INTEGRATIONS_V1, MAX_FRAME_BYTES, MAX_PLAN_BYTES, Message, PROTOCOL_VERSION,
    ParentHello, ProgressKind, ProgressReport, SHORTCUT_SERVICE_V1, SequenceTracker,
    TransactionStateChanged, WireEnvelope, WorkerHello, decode_payload, encode_payload,
};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

/// Runtime identity of one parent/worker IPC relationship.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(pub Uuid);

impl SessionId {
    pub fn new_v7() -> Self {
        Self(Uuid::now_v7())
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Protocol / framing errors.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum WireError {
    #[error("frame exceeds maximum size {max}")]
    FrameTooLarge { max: usize },

    #[error("malformed frame: {0}")]
    Malformed(String),

    #[error("protocol version mismatch: expected {expected}, found {found}")]
    VersionMismatch { expected: u32, found: u32 },

    #[error("session id mismatch")]
    SessionMismatch,

    #[error("duplicate sequence {sequence}")]
    DuplicateSequence { sequence: u64 },

    #[error("sequence went backwards: {previous} → {next}")]
    SequenceRegression { previous: u64, next: u64 },

    #[error("unexpected message")]
    UnexpectedMessage,

    #[error("plan hash mismatch")]
    PlanHashMismatch,

    #[error("capability missing: {0}")]
    MissingCapability(String),

    #[error("authentication failed: {0}")]
    AuthFailed(String),
}
