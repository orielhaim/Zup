//! Wire messages and framing constants.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::SessionId;

/// IPC protocol version.
pub const PROTOCOL_VERSION: u32 = 4;

/// Maximum length-delimited frame size (bytes). Rejects malicious length prefixes.
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// Maximum serialized transaction-plan payload size (bytes).
pub const MAX_PLAN_BYTES: usize = 8 * 1024 * 1024;

/// Worker capability token for the file-transaction node set.
pub const FILE_TRANSACTIONS_V1: &str = "file-transactions-v1";
pub const MANAGED_INTEGRATIONS_V1: &str = "path-protocol-file-type-v1";
pub const SHORTCUT_SERVICE_V1: &str = "shortcut-service-v1";
pub const LIFECYCLE_V1: &str = "owned-lifecycle-v1";

/// Versioned envelope wrapping every message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireEnvelope {
    pub version: u32,
    pub session_id: SessionId,
    /// Monotonically increasing per sender.
    pub sequence: u64,
    pub message: Message,
}

/// All protocol messages. No generic "run command" surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum Message {
    WorkerHello(WorkerHello),
    ParentHello(ParentHello),
    ExecuteTransaction(ExecuteTransaction),
    Cancel,
    Progress(ProgressReport),
    TransactionStateChanged(TransactionStateChanged),
    Completed(Completed),
    Failed(Failed),
    Ping,
    Pong,
}

/// First message from worker → parent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerHello {
    pub protocol_version: u32,
    pub session_id: SessionId,
    pub worker_pid: u32,
    pub capabilities: Capabilities,
}

/// Parent accepts the worker and binds the transaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParentHello {
    pub protocol_version: u32,
    pub session_id: SessionId,
    pub transaction_id: Uuid,
    /// Fingerprint the worker must independently re-validate.
    pub expected_plan_hash: String,
}

/// Advertised capability set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Capabilities {
    pub file_transactions_v1: bool,
    pub managed_integrations_v1: bool,
    pub shortcut_service_v1: bool,
    pub lifecycle_v1: bool,
}

impl Capabilities {
    pub fn supported() -> Self {
        Self {
            file_transactions_v1: true,
            managed_integrations_v1: true,
            shortcut_service_v1: true,
            lifecycle_v1: true,
        }
    }

    pub fn has_file_transactions_v1(&self) -> bool {
        self.file_transactions_v1
    }

    pub fn has_managed_integrations_v1(&self) -> bool {
        self.managed_integrations_v1
    }

    pub fn has_shortcut_service_v1(&self) -> bool {
        self.shortcut_service_v1
    }

    pub fn has_lifecycle_v1(&self) -> bool {
        self.lifecycle_v1
    }
}

/// Execute exactly one transaction plan (already compiled).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecuteTransaction {
    /// Canonical JSON of `zup_transaction::TransactionPlan`.
    pub plan_json: String,
    /// SHA-256 hex of `plan_json` — must match launch-time `expected_plan_hash`.
    pub plan_hash: String,
    pub app_id: String,
    pub app_version: String,
    pub scope: String,
    pub payload_root: String,
    pub state_root: String,
    pub work_root: String,
    /// Resume this durable transaction instead of beginning another one.
    pub recovery_id: Option<uuid::Uuid>,
}

/// Synchronous progress sample / lifecycle notice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgressReport {
    pub kind: ProgressKind,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProgressKind {
    PreflightStarted,
    BlockingProcessesFound,
    StagingStarted,
    StagingProgress,
    OperationStarted,
    Verifying,
    RollingBack,
    Committed,
}

/// Durable transaction phase change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransactionStateChanged {
    pub phase: String,
}

/// Terminal success.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Completed {
    pub transaction_id: Uuid,
    pub outcome: String,
}

/// Terminal failure with a typed reason tag.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Failed {
    pub kind: String,
    pub message: String,
}

/// Encode an envelope to a length-delimited JSON payload (without length prefix).
pub fn encode_payload(envelope: &WireEnvelope) -> Result<Vec<u8>, crate::WireError> {
    let bytes =
        serde_json::to_vec(envelope).map_err(|e| crate::WireError::Malformed(e.to_string()))?;
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(crate::WireError::FrameTooLarge {
            max: MAX_FRAME_BYTES,
        });
    }
    Ok(bytes)
}

/// Decode a payload into an envelope, enforcing version and size.
pub fn decode_payload(bytes: &[u8]) -> Result<WireEnvelope, crate::WireError> {
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(crate::WireError::FrameTooLarge {
            max: MAX_FRAME_BYTES,
        });
    }
    let envelope: WireEnvelope =
        serde_json::from_slice(bytes).map_err(|e| crate::WireError::Malformed(e.to_string()))?;
    if envelope.version != PROTOCOL_VERSION {
        return Err(crate::WireError::VersionMismatch {
            expected: PROTOCOL_VERSION,
            found: envelope.version,
        });
    }
    Ok(envelope)
}

/// Session sequence tracker (monotonic, no duplicates).
#[derive(Debug, Default, Clone)]
pub struct SequenceTracker {
    last: Option<u64>,
}

impl SequenceTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Accept `sequence` if it is strictly greater than the last seen value.
    pub fn accept(&mut self, sequence: u64) -> Result<(), crate::WireError> {
        match self.last {
            None => {
                self.last = Some(sequence);
                Ok(())
            }
            Some(previous) if sequence > previous => {
                self.last = Some(sequence);
                Ok(())
            }
            Some(previous) if sequence == previous => {
                Err(crate::WireError::DuplicateSequence { sequence })
            }
            Some(previous) => Err(crate::WireError::SequenceRegression {
                previous,
                next: sequence,
            }),
        }
    }
}
