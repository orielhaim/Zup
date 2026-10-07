//! Wire messages and framing constants.

use serde::{Deserialize, Serialize};
use uuid::Uuid;
use zup_core::TargetTriple;

use crate::SessionId;

/// IPC protocol version.
pub const PROTOCOL_VERSION: u32 = 1;

/// Maximum length-delimited frame size (bytes). Rejects malicious length prefixes.
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// Maximum serialized transaction-plan payload size (bytes).
pub const MAX_PLAN_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_PAYLOAD_OVERLAY_PATH_BYTES: usize = 32 * 1024;

/// Worker capability token for the file-transaction node set.
pub const FILE_TRANSACTIONS_V1: &str = "file-transactions-v1";
pub const BACKEND_OPERATIONS_V1: &str = "backend-operations-v1";
pub const LIFECYCLE_V1: &str = "owned-lifecycle-v1";
pub const PREREQUISITE_BOOTSTRAP_V1: &str = "prerequisite-bootstrap-v1";

/// The closed vocabulary of `Failed.kind`.
///
/// A closed set, and it lives here rather than in either peer, because the
/// parent's response depends on it: a busy installation is a retry and an
/// authentication failure is a refusal, and a parent that could not tell them
/// apart offers the user the wrong advice. An unrecognized kind is a protocol
/// error and never a default - a newer worker talking to an older parent has to be
/// refused rather than reported as something the user did wrong.
pub mod failure {
    /// Another operation holds this installation's lock.
    pub const INSTALLATION_BUSY: &str = "installation_busy";
    /// The worker could not prove who started it or what it was asked to do.
    pub const AUTHENTICATION: &str = "authentication";
    /// The parent or the worker spoke something the other could not follow.
    pub const PROTOCOL: &str = "protocol";
    /// A transaction or a prerequisite failed.
    pub const TRANSACTION: &str = "transaction";
    /// The user cancelled.
    pub const CANCELLED: &str = "cancelled";
    /// A transaction did not finish safely and a human has to look at it.
    pub const RECOVERY_REQUIRED: &str = "recovery_required";
    /// The operation needs elevation the user did not grant.
    pub const AUTHORIZATION_REQUIRED: &str = "authorization_required";
    /// The worker repaired or recovered machine state while preparing, so a
    /// digest the client bound before that repair is stale. The client
    /// re-plans against the repaired world and tries once more with a fresh
    /// session - never by reusing the old authorization.
    pub const STALE_PLAN: &str = "stale_plan";
    /// The requested operation is outside the privileged path policy.
    ///
    /// Refusing with prose would leave the parent guessing whether to retry;
    /// a policy refusal is never retryable against the same worker.
    pub const POLICY: &str = "policy";
}

/// Every failure kind, so a reader can check the vocabulary is complete and a
/// test can check nothing outside it is sent.
pub const FAILURE_KINDS: &[&str] = &[
    failure::INSTALLATION_BUSY,
    failure::AUTHENTICATION,
    failure::PROTOCOL,
    failure::TRANSACTION,
    failure::CANCELLED,
    failure::RECOVERY_REQUIRED,
    failure::AUTHORIZATION_REQUIRED,
    failure::POLICY,
    failure::STALE_PLAN,
];

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
///
/// `ExecuteTransaction` is boxed because it is by far the largest message, and an
/// unboxed variant would put its size on the stack of every `match` over this
/// enum - including the ones that only handle `Cancel` or `Pong`. `Box<T>` is
/// transparent to serde, so the wire shape is unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum Message {
    WorkerHello(WorkerHello),
    ParentHello(ParentHello),
    ExecuteTransaction(Box<ExecuteTransaction>),
    ExecuteBootstrap(ExecuteBootstrap),
    /// Propose a privileged operation: intent plus the caller's choices.
    ///
    /// The worker independently reconstructs the plan from this intent and
    /// answers with `Prepared`. No mutation follows from this message alone.
    Prepare(PrepareOperation),
    /// The worker's answer: the digest of the plan it reconstructed.
    ///
    /// The client must compare this digest with the one it computed itself
    /// before sending `Execute`. A mismatch means the two sides disagree
    /// about what was authorized, and the session stops.
    Prepared(PreparedOperation),
    /// Authorize exactly one prepared plan. One-shot: a second `Execute`
    /// against the same prepared plan is a replay and is refused.
    Execute(ExecuteOperation),
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
    pub target: TargetTriple,
    pub worker_pid: u32,
    pub capabilities: Capabilities,
}

/// Parent accepts the worker and binds the transaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParentHello {
    pub protocol_version: u32,
    pub session_id: SessionId,
    pub target: TargetTriple,
    pub transaction_id: Uuid,
    /// Fingerprint the worker must independently re-validate.
    pub expected_plan_hash: String,
}

/// Advertised capability set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Capabilities {
    pub file_transactions_v1: bool,
    pub backend_operations_v1: bool,
    pub lifecycle_v1: bool,
    #[serde(default)]
    pub prerequisite_bootstrap_v1: bool,
}

/// Execute exactly one transaction plan (already compiled).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecuteTransaction {
    pub target: TargetTriple,
    /// Canonical JSON of `zup_transaction::TransactionPlan`.
    pub plan_json: String,
    /// SHA-256 hex of `plan_json` - must match launch-time `expected_plan_hash`.
    pub plan_hash: String,
    pub app_id: String,
    pub app_version: String,
    pub scope: String,
    pub payload_root: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_overlay_root: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_overlay_base_root: Option<String>,
    pub state_root: String,
    pub work_root: String,
    /// Resume this durable transaction instead of beginning another one.
    pub recovery_id: Option<uuid::Uuid>,
    /// The exact release graph being installed, for the worker to record.
    ///
    /// The worker publishes the ledger, so the identity has to reach it. It is
    /// an `Option` because a development run has none, and an absent identity is
    /// recorded as absent rather than invented.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release: Option<zup_core::ReleaseIdentity>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecuteBootstrap {
    pub target: TargetTriple,
    pub bootstrap_json: String,
    pub bootstrap_hash: String,
    pub bootstrap_id: uuid::Uuid,
    pub app_id: String,
    pub app_version: String,
    pub scope: String,
    pub state_root: String,
    pub quarantine_root: String,
    #[serde(default)]
    pub recovery_id: Option<uuid::Uuid>,
}

/// Bounds on untrusted intent fields in [`PrepareOperation`].
///
/// The frame limit already bounds the whole message; these bound the pieces,
/// so a worker never sizes a buffer from one hostile string.
pub const MAX_INTENT_STRING_BYTES: usize = 4096;
pub const MAX_INTENT_COMPONENTS: usize = 64;

/// The closed vocabulary of [`PrepareOperation::operation`].
pub mod privileged_operation {
    pub const INSTALL: &str = "install";
    pub const UPGRADE: &str = "upgrade";
    pub const REPAIR: &str = "repair";
    pub const UNINSTALL: &str = "uninstall";
    pub const APPLY: &str = "apply";
}

/// Every privileged operation, so a reader can check the vocabulary is
/// complete and a worker can refuse anything outside it.
pub const PRIVILEGED_OPERATIONS: &[&str] = &[
    privileged_operation::INSTALL,
    privileged_operation::UPGRADE,
    privileged_operation::REPAIR,
    privileged_operation::UNINSTALL,
    privileged_operation::APPLY,
];

/// One proposed privileged operation: user intent plus the caller's choices.
///
/// Everything in here is untrusted. The worker revalidates every field
/// against its own package and its own policy before `Prepared` is ever
/// sent; a field that does not survive that validation refuses the session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrepareOperation {
    /// One of [`PRIVILEGED_OPERATIONS`].
    pub operation: String,
    /// Repair restores present-but-different files too.
    pub force_files: bool,
    /// Caller-chosen install directory, if the project permits one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install_dir_override: Option<String>,
    /// Caller-chosen components, as text.
    #[serde(default)]
    pub selected_components: Vec<String>,
    /// The plan digest the caller computed itself, if it planned first.
    ///
    /// The worker compares its independently reconstructed digest with this
    /// one. An absent digest means the caller shows no plan to bind, which a
    /// worker may accept only where its own policy says the operation needs
    /// no client-side confirmation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_plan_digest: Option<String>,
    pub app_id: String,
    pub app_version: String,
    pub scope: String,
    pub target: TargetTriple,
}

impl PrepareOperation {
    /// Refuse an intent whose shape already breaks the bounds, before the
    /// worker spends any authority on it.
    pub fn validate_shape(&self) -> Result<(), crate::WireError> {
        if !PRIVILEGED_OPERATIONS.contains(&self.operation.as_str()) {
            return Err(crate::WireError::Malformed(
                "unknown privileged operation".into(),
            ));
        }
        let bounded = |value: &str| value.len() <= MAX_INTENT_STRING_BYTES;
        if !bounded(&self.operation)
            || !bounded(&self.app_id)
            || !bounded(&self.app_version)
            || !bounded(&self.scope)
        {
            return Err(crate::WireError::FrameTooLarge {
                max: MAX_INTENT_STRING_BYTES,
            });
        }
        if let Some(directory) = &self.install_dir_override
            && !bounded(directory)
        {
            return Err(crate::WireError::FrameTooLarge {
                max: MAX_INTENT_STRING_BYTES,
            });
        }
        if let Some(digest) = &self.expected_plan_digest
            && !bounded(digest)
        {
            return Err(crate::WireError::FrameTooLarge {
                max: MAX_INTENT_STRING_BYTES,
            });
        }
        if self.selected_components.len() > MAX_INTENT_COMPONENTS {
            return Err(crate::WireError::FrameTooLarge {
                max: MAX_INTENT_COMPONENTS,
            });
        }
        if self.selected_components.iter().any(|name| !bounded(name)) {
            return Err(crate::WireError::FrameTooLarge {
                max: MAX_INTENT_STRING_BYTES,
            });
        }
        Ok(())
    }
}

/// The worker's answer to [`PrepareOperation`]: the identity of the plan it
/// reconstructed, plus the summary the caller shows before authorizing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreparedOperation {
    /// SHA-256 hex of the canonical plan encoding the worker reconstructed.
    pub plan_digest: String,
    pub operation: String,
    pub app_id: String,
    pub app_version: String,
    pub scope: String,
    pub target: TargetTriple,
    pub file_count: u32,
}

/// Authorize exactly one prepared plan. The digest must equal the
/// [`PreparedOperation::plan_digest`] of this session; anything else is a
/// substitution or a replay and is refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecuteOperation {
    pub plan_digest: String,
}

/// Synchronous progress sample / lifecycle notice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgressReport {
    pub kind: ProgressKind,
    pub detail: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProgressKind {
    PreflightStarted,
    ResourceBlocked,
    StagingStarted,
    StagingProgress,
    OperationStarted,
    OperationProgress,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prerequisite_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
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
