use serde::{Deserialize, Serialize};
use uuid::Uuid;
use zup_core::Target;

use zup_core::SessionId;

pub const PROTOCOL_VERSION: u32 = 1;

pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

pub const MAX_PLAN_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_PAYLOAD_OVERLAY_PATH_BYTES: usize = 32 * 1024;

pub const FILE_TRANSACTIONS_V1: &str = "file-transactions-v1";
pub const BACKEND_OPERATIONS_V1: &str = "backend-operations-v1";
pub const LIFECYCLE_V1: &str = "owned-lifecycle-v1";
pub const PREREQUISITE_BOOTSTRAP_V1: &str = "prerequisite-bootstrap-v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerFailure {
    InstallationBusy,
    Authentication,
    Protocol,
    Transaction,
    Cancelled,
    RecoveryRequired {
        transaction: Option<zup_core::TransactionId>,
    },
    AuthorizationRequired,
    StalePlan,
    Policy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireEnvelope {
    pub version: u32,
    pub session_id: SessionId,
    pub sequence: u64,
    pub message: Message,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum Message {
    WorkerHello(WorkerHello),
    ParentHello(ParentHello),
    PrepareTransaction(Box<PrepareTransaction>),
    ExecuteBootstrap(ExecuteBootstrap),
    Prepare(PrepareOperation),
    Prepared(PreparedOperation),
    Execute(ExecuteOperation),
    Cancel,
    Progress(ProgressReport),
    TransactionStateChanged(TransactionStateChanged),
    Completed(Completed),
    Failed(Failed),
    Ping,
    Pong,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerHello {
    pub protocol_version: u32,
    pub session_id: SessionId,
    pub target: Target,
    pub worker_pid: u32,
    pub capabilities: Capabilities,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParentHello {
    pub protocol_version: u32,
    pub session_id: SessionId,
    pub target: Target,
    pub transaction_id: Uuid,
    pub expected_request_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Capabilities {
    pub file_transactions_v1: bool,
    pub backend_operations_v1: bool,
    pub lifecycle_v1: bool,
    #[serde(default)]
    pub prerequisite_bootstrap_v1: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcquiredSource {
    pub handoff: zup_acquire::RuntimeHandoff,
    pub image: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrepareTransaction {
    pub content: Option<AcquiredSource>,
    pub operation: PreparedTransactionRequest,
    pub target: Target,
    pub app_id: String,
    pub app_version: String,
    pub scope: String,
    pub payload_root: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_overlay_root: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_overlay_base_root: Option<String>,
    pub state_root: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release: Option<zup_core::ReleaseIdentity>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PreparedTransactionRequest {
    Deployment {
        plan: Box<zup_plan::DeploymentPlan>,
        intent: Box<zup_plan::DeploymentIntent>,
    },
    Recovery {
        transaction: zup_core::TransactionId,
    },
}

impl PrepareTransaction {
    pub fn digest(&self) -> Result<String, serde_json::Error> {
        Ok(zup_core::hash_bytes(&serde_json::to_vec(self)?).to_hex())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecuteBootstrap {
    pub target: Target,
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

/// so a worker never sizes a buffer from one hostile string.
pub const MAX_INTENT_STRING_BYTES: usize = 4096;
pub const MAX_INTENT_COMPONENTS: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PrivilegedRequest {
    Deployment {
        intent: Box<zup_plan::DeploymentIntent>,
        expected_requirements: Option<Box<zup_plan::DeploymentRequirements>>,
    },
    Recovery {
        transaction: zup_core::TransactionId,
    },
}
impl PrivilegedRequest {
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Deployment { intent, .. } => intent.action.name(),
            Self::Recovery { .. } => "recover",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrepareOperation {
    pub operation: PrivilegedRequest,
    pub app_id: String,
    pub app_version: String,
    pub scope: String,
    pub target: Target,
    pub state_root: String,
}

impl PrepareOperation {
    pub fn validate_shape(&self) -> Result<(), crate::WireError> {
        let bounded = |value: &str| value.len() <= MAX_INTENT_STRING_BYTES;
        if !bounded(&self.app_id)
            || !bounded(&self.app_version)
            || !bounded(&self.scope)
            || !bounded(&self.state_root)
        {
            return Err(crate::WireError::FrameTooLarge {
                max: MAX_INTENT_STRING_BYTES,
            });
        }
        if let PrivilegedRequest::Deployment {
            intent,
            expected_requirements,
        } = &self.operation
        {
            if intent
                .install_directory
                .as_ref()
                .is_some_and(|directory| !bounded(&directory.to_string_lossy()))
                || intent
                    .enable
                    .iter()
                    .chain(&intent.disable)
                    .any(|name| !bounded(name))
            {
                return Err(crate::WireError::FrameTooLarge {
                    max: MAX_INTENT_STRING_BYTES,
                });
            }
            if intent.enable.len() > MAX_INTENT_COMPONENTS
                || intent.disable.len() > MAX_INTENT_COMPONENTS
            {
                return Err(crate::WireError::FrameTooLarge {
                    max: MAX_INTENT_COMPONENTS,
                });
            }
            if let Some(requirements) = expected_requirements
                && (requirements.validate().is_err()
                    || requirements.app.id.as_str() != self.app_id
                    || requirements.target != self.target)
            {
                return Err(crate::WireError::Malformed(
                    "deployment requirements differ from the prepared application".into(),
                ));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreparedOperation {
    pub plan: Box<zup_plan::DeploymentPlan>,
    pub plan_digest: String,
    pub operation: String,
    pub app_id: String,
    pub app_version: String,
    pub scope: String,
    pub target: Target,
    pub file_count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecuteOperation {
    pub plan_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgressReport {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocked_pids: Vec<u32>,
    pub phase: zup_core::OperationPhase,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransactionStateChanged {
    pub phase: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerOutcome {
    Committed,
    RolledBack,
    Cancelled,
    RecoveryRequired,
    CommittedPendingReconciliation,
    RebootRequired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Completed {
    pub detail: Option<String>,
    pub transaction_id: Uuid,
    pub outcome: WorkerOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prerequisite_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Failed {
    pub kind: WorkerFailure,
    pub message: String,
}

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

#[derive(Debug, Default, Clone)]
pub struct SequenceTracker {
    last: Option<u64>,
}

impl SequenceTracker {
    pub fn new() -> Self {
        Self::default()
    }

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

use super::WireError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrivilegedSession {
    session: SessionId,
    state: PrivilegedSessionState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PrivilegedSessionState {
    AcceptingPrepare,
    Prepared { plan_digest: String },
    Executed,
}

impl PrivilegedSession {
    pub fn new(session: SessionId) -> Self {
        Self {
            session,
            state: PrivilegedSessionState::AcceptingPrepare,
        }
    }

    pub fn session(&self) -> SessionId {
        self.session
    }

    pub fn prepared(&mut self, session: SessionId, plan_digest: &str) -> Result<(), WireError> {
        if session != self.session {
            return Err(WireError::SessionMismatch);
        }
        if !matches!(self.state, PrivilegedSessionState::AcceptingPrepare) {
            return Err(WireError::UnexpectedMessage);
        }
        if plan_digest.len() != 64 || !plan_digest.chars().all(|char| char.is_ascii_hexdigit()) {
            return Err(WireError::Malformed(
                "plan digest is not SHA-256 hex".into(),
            ));
        }
        self.state = PrivilegedSessionState::Prepared {
            plan_digest: plan_digest.to_lowercase(),
        };
        Ok(())
    }

    pub fn execute(&mut self, session: SessionId, plan_digest: &str) -> Result<(), WireError> {
        if session != self.session {
            return Err(WireError::SessionMismatch);
        }
        match &self.state {
            PrivilegedSessionState::Prepared { plan_digest: bound } => {
                if plan_digest.to_lowercase() != *bound {
                    return Err(WireError::RequestDigestMismatch);
                }
                self.state = PrivilegedSessionState::Executed;
                Ok(())
            }
            PrivilegedSessionState::AcceptingPrepare => Err(WireError::UnexpectedMessage),
            PrivilegedSessionState::Executed => Err(WireError::Replay),
        }
    }

    /// Whether this session has executed and must serve nothing further.
    pub fn executed(&self) -> bool {
        matches!(self.state, PrivilegedSessionState::Executed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIGEST_A: &str = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";
    const DIGEST_B: &str = "5feceb66ffc86f38d952786c6d696c79c2dbc239dd4e91b46729d73a27fb57e9";

    fn session() -> SessionId {
        SessionId(uuid::Uuid::now_v7())
    }

    #[test]
    fn prepare_then_execute_with_the_prepared_digest_succeeds_once() {
        let id = session();
        let mut tracker = PrivilegedSession::new(id);
        assert!(!tracker.executed());
        tracker.prepared(id, DIGEST_A).expect("prepare binds");
        tracker.execute(id, DIGEST_A).expect("exact execute runs");
        assert!(tracker.executed());
    }

    #[test]
    fn execute_before_prepare_authorizes_nothing() {
        let id = session();
        let mut tracker = PrivilegedSession::new(id);
        assert!(matches!(
            tracker.execute(id, DIGEST_A),
            Err(WireError::UnexpectedMessage)
        ));
    }

    #[test]
    fn a_different_digest_is_a_substitution_not_an_authorization() {
        let id = session();
        let mut tracker = PrivilegedSession::new(id);
        tracker.prepared(id, DIGEST_A).expect("prepare binds");
        assert!(matches!(
            tracker.execute(id, DIGEST_B),
            Err(WireError::RequestDigestMismatch)
        ));
        assert!(!tracker.executed());
    }

    #[test]
    fn a_second_execute_is_a_replay() {
        let id = session();
        let mut tracker = PrivilegedSession::new(id);
        tracker.prepared(id, DIGEST_A).expect("prepare binds");
        tracker.execute(id, DIGEST_A).expect("first execute runs");
        assert!(tracker.execute(id, DIGEST_A).is_err());
    }

    #[test]
    fn a_second_prepare_is_a_new_authorization_not_an_update() {
        let id = session();
        let mut tracker = PrivilegedSession::new(id);
        tracker.prepared(id, DIGEST_A).expect("prepare binds");
        assert!(matches!(
            tracker.prepared(id, DIGEST_B),
            Err(WireError::UnexpectedMessage)
        ));
    }

    #[test]
    fn another_session_binds_nothing_here() {
        let mut tracker = PrivilegedSession::new(session());
        let stranger = session();
        assert!(matches!(
            tracker.prepared(stranger, DIGEST_A),
            Err(WireError::SessionMismatch)
        ));
        assert!(matches!(
            tracker.execute(stranger, DIGEST_A),
            Err(WireError::SessionMismatch)
        ));
    }

    #[test]
    fn a_malformed_digest_prepares_nothing() {
        let id = session();
        let mut tracker = PrivilegedSession::new(id);
        assert!(tracker.prepared(id, "not-a-digest").is_err());
        assert!(matches!(
            tracker.execute(id, DIGEST_A),
            Err(WireError::UnexpectedMessage)
        ));
    }
}
