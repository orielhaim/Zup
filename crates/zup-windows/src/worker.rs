//! Hidden elevated/worker mode entry (`zup __worker <bootstrap>`).
//!
//! Bootstrap carries only: protocol version, session id, pipe name,
//! expected parent identity, canonical target, and expected plan hash. The plan
//! itself travels over IPC.

use std::path::{Path, PathBuf};

use thiserror::Error;
use zup_core::TargetTriple;

use zup_protocol::{
    Capabilities, Message, PROTOCOL_VERSION, ParentHello, SequenceTracker, SessionId, WireEnvelope,
    WorkerHello, decode_payload, encode_payload,
};

/// Worker bootstrap arguments (command line, no plan/secrets).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerBootstrap {
    pub protocol_version: u32,
    pub session_id: SessionId,
    pub pipe_name: String,
    pub expected_parent_pid: u32,
    pub expected_parent_sid: String,
    pub target: TargetTriple,
    pub expected_plan_hash: String,
}

/// Worker startup / execution errors.
#[derive(Debug, Error)]
pub enum WorkerError {
    #[error("invalid bootstrap: {0}")]
    InvalidBootstrap(String),

    #[error("authentication failed: {0}")]
    AuthFailed(String),

    #[error("plan hash mismatch")]
    PlanHashMismatch,

    #[error("target mismatch")]
    TargetMismatch,

    #[error("capability missing: {0}")]
    MissingCapability(String),

    #[error("protocol error: {0}")]
    Protocol(String),

    #[error("transaction failed: {0}")]
    Transaction(String),

    /// Another operation holds this installation's lock.
    ///
    /// A distinct error rather than a `Transaction` string, because the parent
    /// has to turn it into "wait and retry" rather than "this installation is
    /// broken", and the only way to do that across a process boundary is for the
    /// worker to say which it is.
    #[error("another operation is running for this installation")]
    Busy,

    #[error("parent disconnect")]
    ParentDisconnect,
}

pub fn worker_capabilities() -> Capabilities {
    Capabilities {
        file_transactions_v1: true,
        backend_operations_v1: true,
        lifecycle_v1: true,
        prerequisite_bootstrap_v1: true,
    }
}

/// Parse `zup __worker <bootstrap-json>` arguments.
pub fn parse_bootstrap(arg: &str) -> Result<WorkerBootstrap, WorkerError> {
    // Strict bootstrap: version|session|pipe|parent_pid|parent_sid|target|plan_hash
    let parts: Vec<&str> = arg.split('|').collect();
    if parts.len() != 7 {
        return Err(WorkerError::InvalidBootstrap(format!(
            "expected 6 fields, got {}",
            parts.len()
        )));
    }
    let protocol_version: u32 = parts[0]
        .parse()
        .map_err(|_| WorkerError::InvalidBootstrap("protocol version".into()))?;
    if protocol_version != PROTOCOL_VERSION {
        return Err(WorkerError::InvalidBootstrap(format!(
            "protocol version {protocol_version}"
        )));
    }
    let session_id = parts[1]
        .parse::<uuid::Uuid>()
        .map_err(|_| WorkerError::InvalidBootstrap("session id".into()))?;
    let pipe_name = parts[2].to_owned();
    if pipe_name.is_empty() || pipe_name.len() > 64 || pipe_name.contains('\\') {
        return Err(WorkerError::InvalidBootstrap("pipe name".into()));
    }
    let expected_parent_pid: u32 = parts[3]
        .parse()
        .map_err(|_| WorkerError::InvalidBootstrap("parent pid".into()))?;
    if expected_parent_pid == 0 {
        return Err(WorkerError::InvalidBootstrap("parent pid is zero".into()));
    }
    let expected_parent_sid = parts[4].to_owned();
    if !expected_parent_sid.starts_with("S-1-") {
        return Err(WorkerError::InvalidBootstrap("parent sid".into()));
    }
    let target = TargetTriple::parse(parts[5])
        .map_err(|error| WorkerError::InvalidBootstrap(error.to_string()))?;
    let expected_plan_hash = parts[6].to_owned();
    if expected_plan_hash.len() != 64 || !expected_plan_hash.chars().all(|c| c.is_ascii_hexdigit())
    {
        return Err(WorkerError::InvalidBootstrap("plan hash".into()));
    }

    Ok(WorkerBootstrap {
        protocol_version,
        session_id: SessionId(session_id),
        pipe_name,
        expected_parent_pid,
        expected_parent_sid,
        target,
        expected_plan_hash: expected_plan_hash.to_lowercase(),
    })
}

/// Serialize bootstrap for the worker command line (quoting handled by caller).
pub fn format_bootstrap(bootstrap: &WorkerBootstrap) -> String {
    format!(
        "{}|{}|{}|{}|{}|{}|{}",
        bootstrap.protocol_version,
        bootstrap.session_id,
        bootstrap.pipe_name,
        bootstrap.expected_parent_pid,
        bootstrap.expected_parent_sid,
        bootstrap.target,
        bootstrap.expected_plan_hash
    )
}

/// Compute the canonical plan fingerprint (matches `TransactionPlan::fingerprint` hex).
pub fn plan_hash_hex(plan_json: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(plan_json.as_bytes());
    let digest = zup_core::Sha256Digest::from_hasher(hasher);
    digest.to_hex()
}

/// Worker session driver (one connection, one transaction, then exit).
pub struct WorkerSession {
    pub bootstrap: WorkerBootstrap,
    sequence: SequenceTracker,
    authenticated: bool,
    plan_hash_checked: bool,
}

impl WorkerSession {
    pub fn new(bootstrap: WorkerBootstrap) -> Self {
        Self {
            bootstrap,
            sequence: SequenceTracker::new(),
            authenticated: false,
            plan_hash_checked: false,
        }
    }

    /// Handle an inbound envelope. Returns an optional reply.
    pub fn handle_message(
        &mut self,
        envelope: WireEnvelope,
    ) -> Result<Option<WireEnvelope>, WorkerError> {
        self.sequence
            .accept(envelope.sequence)
            .map_err(|e| WorkerError::Protocol(e.to_string()))?;
        if envelope.session_id != self.bootstrap.session_id {
            return Err(WorkerError::AuthFailed("session id mismatch".into()));
        }
        if envelope.version != PROTOCOL_VERSION {
            return Err(WorkerError::AuthFailed("protocol version mismatch".into()));
        }

        match envelope.message {
            Message::WorkerHello(_) => Err(WorkerError::Protocol(
                "worker hello is parent-side only".into(),
            )),
            Message::ParentHello(hello) => self.on_parent_hello(envelope.sequence, hello),
            Message::ExecuteTransaction(exec) => self.on_execute(envelope.sequence, *exec),
            Message::ExecuteBootstrap(exec) => self.on_bootstrap(envelope.sequence, exec),
            Message::Cancel => Ok(None),
            Message::Ping => Ok(Some(WireEnvelope {
                version: PROTOCOL_VERSION,
                session_id: self.bootstrap.session_id,
                sequence: envelope.sequence,
                message: Message::Pong,
            })),
            Message::Pong
            | Message::Progress(_)
            | Message::TransactionStateChanged(_)
            | Message::Completed(_)
            | Message::Failed(_) => Err(WorkerError::Protocol("unexpected message".into())),
        }
    }

    /// Worker hello (first message to parent).
    pub fn hello(&self) -> WireEnvelope {
        WireEnvelope {
            version: PROTOCOL_VERSION,
            session_id: self.bootstrap.session_id,
            sequence: 0,
            message: Message::WorkerHello(WorkerHello {
                protocol_version: PROTOCOL_VERSION,
                session_id: self.bootstrap.session_id,
                target: self.bootstrap.target.clone(),
                worker_pid: std::process::id(),
                capabilities: worker_capabilities(),
            }),
        }
    }

    fn on_parent_hello(
        &mut self,
        sequence: u64,
        hello: ParentHello,
    ) -> Result<Option<WireEnvelope>, WorkerError> {
        if self.authenticated {
            return Err(WorkerError::Protocol("duplicate hello".into()));
        }
        if hello.protocol_version != PROTOCOL_VERSION {
            return Err(WorkerError::AuthFailed("protocol version".into()));
        }
        if hello.session_id != self.bootstrap.session_id {
            return Err(WorkerError::AuthFailed("session id".into()));
        }
        if hello.target != self.bootstrap.target {
            return Err(WorkerError::TargetMismatch);
        }
        if hello.expected_plan_hash != self.bootstrap.expected_plan_hash {
            return Err(WorkerError::PlanHashMismatch);
        }
        self.authenticated = true;
        let _ = sequence;
        Ok(None)
    }

    fn on_bootstrap(
        &mut self,
        _sequence: u64,
        exec: zup_protocol::ExecuteBootstrap,
    ) -> Result<Option<WireEnvelope>, WorkerError> {
        if !self.authenticated {
            return Err(WorkerError::AuthFailed("not authenticated".into()));
        }
        if self.plan_hash_checked {
            return Err(WorkerError::Protocol("second operation rejected".into()));
        }
        if exec.target != self.bootstrap.target {
            return Err(WorkerError::TargetMismatch);
        }
        if exec.bootstrap_json.len() > zup_protocol::MAX_PLAN_BYTES {
            return Err(WorkerError::Protocol("bootstrap plan too large".into()));
        }
        let hash = plan_hash_hex(&exec.bootstrap_json);
        if hash != self.bootstrap.expected_plan_hash || hash != exec.bootstrap_hash {
            return Err(WorkerError::PlanHashMismatch);
        }
        let plan: zup_bootstrap::BoundBootstrapPlan = serde_json::from_str(&exec.bootstrap_json)
            .map_err(|error| WorkerError::Protocol(format!("bad bootstrap plan: {error}")))?;
        let declared_plan_hash = plan.plan_hash;
        let validated =
            zup_bootstrap::BoundBootstrapPlan::with_id(plan.id, plan.plan, plan.artifacts)
                .map_err(|error| WorkerError::AuthFailed(error.to_string()))?;
        if validated.plan.key.target != self.bootstrap.target
            || exec.target != validated.plan.key.target
        {
            return Err(WorkerError::TargetMismatch);
        }
        if validated.plan_hash != declared_plan_hash
            || validated.id.as_uuid() != exec.bootstrap_id
            || validated.plan.key.app_id.as_str() != exec.app_id
            || validated.plan.key.app_version.to_string() != exec.app_version
            || validated.plan.key.scope.to_string() != exec.scope
        {
            return Err(WorkerError::AuthFailed(
                "bootstrap identity mismatch".into(),
            ));
        }
        if exec.recovery_id.is_some() {
            return Err(WorkerError::AuthFailed(
                "bootstrap recovery ids are not supported".into(),
            ));
        }
        if exec.state_root.is_empty()
            || exec.quarantine_root.is_empty()
            || exec.state_root.len() > zup_protocol::MAX_PAYLOAD_OVERLAY_PATH_BYTES
            || exec.quarantine_root.len() > zup_protocol::MAX_PAYLOAD_OVERLAY_PATH_BYTES
            || exec.state_root.contains('\0')
            || exec.quarantine_root.contains('\0')
            || !Path::new(&exec.state_root).is_absolute()
            || !Path::new(&exec.quarantine_root).is_absolute()
        {
            return Err(WorkerError::AuthFailed(
                "bootstrap roots must be absolute".into(),
            ));
        }
        self.plan_hash_checked = true;
        Ok(Some(WireEnvelope {
            version: PROTOCOL_VERSION,
            session_id: self.bootstrap.session_id,
            sequence: 0,
            message: Message::Progress(zup_protocol::ProgressReport {
                kind: zup_protocol::ProgressKind::OperationStarted,
                detail: "bootstrap plan accepted".into(),
                completed: None,
                total: None,
            }),
        }))
    }

    fn on_execute(
        &mut self,
        _sequence: u64,
        exec: zup_protocol::ExecuteTransaction,
    ) -> Result<Option<WireEnvelope>, WorkerError> {
        if !self.authenticated {
            return Err(WorkerError::AuthFailed("not authenticated".into()));
        }
        if self.plan_hash_checked {
            return Err(WorkerError::Protocol("second transaction rejected".into()));
        }
        if exec.target != self.bootstrap.target {
            return Err(WorkerError::TargetMismatch);
        }
        if exec.plan_json.len() > zup_protocol::MAX_PLAN_BYTES {
            return Err(WorkerError::Protocol("plan too large".into()));
        }
        let hash = plan_hash_hex(&exec.plan_json);
        if hash != self.bootstrap.expected_plan_hash || hash != exec.plan_hash {
            return Err(WorkerError::PlanHashMismatch);
        }
        self.plan_hash_checked = true;

        let plan: zup_transaction::TransactionPlan = serde_json::from_str(&exec.plan_json)
            .map_err(|e| WorkerError::Protocol(format!("bad plan: {e}")))?;
        if exec.target != plan.target {
            return Err(WorkerError::TargetMismatch);
        }
        plan.validate()
            .map_err(|error| WorkerError::Protocol(format!("invalid plan: {error}")))?;

        // Actual execution is driven by the caller (coordinator + executor).
        Ok(Some(WireEnvelope {
            version: PROTOCOL_VERSION,
            session_id: self.bootstrap.session_id,
            sequence: 0,
            message: Message::Progress(zup_protocol::ProgressReport {
                kind: zup_protocol::ProgressKind::OperationStarted,
                detail: "plan accepted".into(),
                completed: None,
                total: None,
            }),
        }))
    }
}

/// Encode a reply envelope with the next sequence number.
pub fn encode_reply(session_id: SessionId, sequence: u64, message: Message) -> Vec<u8> {
    encode_payload(&WireEnvelope {
        version: PROTOCOL_VERSION,
        session_id,
        sequence,
        message,
    })
    .unwrap_or_default()
}

/// Decode a payload (shared with parent).
pub fn decode_frame(bytes: &[u8]) -> Result<WireEnvelope, WorkerError> {
    decode_payload(bytes).map_err(|e| WorkerError::Protocol(e.to_string()))
}

/// Executable path of the current trusted host binary (never via PATH).
pub fn current_exe() -> Result<PathBuf, WorkerError> {
    std::env::current_exe().map_err(|e| WorkerError::InvalidBootstrap(e.to_string()))
}
