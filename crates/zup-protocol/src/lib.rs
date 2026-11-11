#![forbid(unsafe_code)]

mod messages;

pub use messages::PrivilegedSession;
pub use messages::{
    AcquiredSource, BACKEND_OPERATIONS_V1, Capabilities, Completed, ExecuteBootstrap,
    ExecuteOperation, FILE_TRANSACTIONS_V1, Failed, LIFECYCLE_V1, MAX_FRAME_BYTES,
    MAX_INTENT_COMPONENTS, MAX_INTENT_STRING_BYTES, MAX_PAYLOAD_OVERLAY_PATH_BYTES, MAX_PLAN_BYTES,
    Message, PREREQUISITE_BOOTSTRAP_V1, PROTOCOL_VERSION, ParentHello, PrepareOperation,
    PrepareTransaction, PreparedOperation, PreparedTransactionRequest, PrivilegedRequest,
    ProgressKind, ProgressReport, SequenceTracker, TransactionStateChanged, WireEnvelope,
    WorkerFailure, WorkerHello, WorkerOutcome, decode_payload, encode_payload,
};

use thiserror::Error;

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

    #[error("replayed execute")]
    Replay,
    #[error("sequence went backwards: {previous} → {next}")]
    SequenceRegression { previous: u64, next: u64 },

    #[error("unexpected message")]
    UnexpectedMessage,

    #[error("plan hash mismatch")]
    RequestDigestMismatch,

    #[error("capability missing: {0}")]
    MissingCapability(String),

    #[error("authentication failed: {0}")]
    AuthFailed(String),
}
