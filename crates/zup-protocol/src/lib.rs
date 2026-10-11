#![forbid(unsafe_code)]

mod messages;

pub use messages::PrivilegedSession;
pub use messages::{
    BACKEND_OPERATIONS_V1, Capabilities, Completed, ExecuteBootstrap, ExecuteOperation,
    ExecuteTransaction, FAILURE_KINDS, FILE_TRANSACTIONS_V1, Failed, LIFECYCLE_V1, MAX_FRAME_BYTES,
    MAX_INTENT_COMPONENTS, MAX_INTENT_STRING_BYTES, MAX_PAYLOAD_OVERLAY_PATH_BYTES, MAX_PLAN_BYTES,
    Message, PREREQUISITE_BOOTSTRAP_V1, PRIVILEGED_OPERATIONS, PROTOCOL_VERSION, ParentHello,
    PrepareOperation, PreparedOperation, ProgressKind, ProgressReport, SequenceTracker,
    TransactionStateChanged, WireEnvelope, WorkerHello, decode_payload, encode_payload, failure,
    privileged_operation,
};
pub use zup_core::SessionId;

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
    PlanHashMismatch,

    #[error("capability missing: {0}")]
    MissingCapability(String),

    #[error("authentication failed: {0}")]
    AuthFailed(String),
}
