use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use zup_bootstrap::BoundBootstrapPlan;
use zup_bundle::PayloadSource;
use zup_core::{AppId, ReleaseIdentity, SelectedScope, TargetTriple};
use zup_transaction::{
    CancellationProbe, FilesystemTransactionStore, TransactionId, TransactionPlan, TransactionStore,
};

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("plan validation failed: {0}")]
    PlanInvalid(String),

    #[error("authorization required")]
    AuthorizationRequired,

    #[error("authorization cancelled by user")]
    AuthorizationCancelled,

    #[error("worker launch failed: {0}")]
    WorkerLaunch(String),

    #[error("worker exited early (code {0:?})")]
    WorkerExited(Option<i32>),

    #[error("worker crashed: {0}")]
    WorkerCrashed(String),

    #[error("protocol/authentication failure: {0}")]
    Protocol(String),

    #[error("transaction failure: {0}")]
    Transaction(String),

    #[error("prerequisite failure: {0}")]
    Prerequisite(String),

    #[error("installation busy")]
    InstallationBusy,

    #[error("cancelled")]
    Cancelled,

    #[error("parent disconnect")]
    ParentDisconnect,

    #[error("recovery required")]
    RecoveryRequired,
}

#[derive(Clone)]
pub struct RuntimeRequest {
    pub target: TargetTriple,
    pub app_id: AppId,
    pub app_version: semver::Version,
    pub scope: SelectedScope,
    pub transaction_plan: TransactionPlan,
    pub state_root: PathBuf,
    pub work_root: PathBuf,
    pub recovery_id: Option<TransactionId>,
    pub bootstrap: Option<BootstrapRequest>,
    pub release: Option<ReleaseIdentity>,
}

#[derive(Clone)]
pub struct BootstrapRequest {
    pub plan: BoundBootstrapPlan,
    pub state_root: PathBuf,
    pub quarantine_root: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionPolicy {
    Interactive,
    NonInteractive,
}

impl ExecutionPolicy {
    pub fn allows_authorization(self) -> bool {
        matches!(self, Self::Interactive)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallOutcome {
    Committed,
    RebootRequired {
        prerequisite_id: String,
        exit_code: i32,
    },
    RolledBack,
    Cancelled,
    RecoveryRequired,
    /// It carries what was running, never where the lock lives: the lock file is
    Busy {
        operation: &'static str,
    },
    Failed(String),
}

#[derive(Clone, Debug, Default)]
pub struct CancellationHandle(CancellationToken);

impl CancellationHandle {
    pub fn new() -> Self {
        Self(CancellationToken::new())
    }

    pub fn probe(&self) -> TokenProbe {
        TokenProbe(self.0.clone())
    }
}

impl std::ops::Deref for CancellationHandle {
    type Target = CancellationToken;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[derive(Clone)]
pub struct TokenProbe(CancellationToken);

impl CancellationProbe for TokenProbe {
    fn is_cancelled(&self) -> bool {
        self.0.is_cancelled()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryStatus {
    NoRecoveryNeeded,
    PreparedTransaction { transaction_id: String },
    InterruptedApplyingTransaction { transaction_id: String },
    RecoveryRequired { transaction_id: String },
}

pub fn discover_recovery(state_root: &Path) -> Vec<RecoveryStatus> {
    let store = FilesystemTransactionStore::new(state_root);
    let mut out = Vec::new();
    let tx_dir = state_root.join("transactions");
    let Ok(entries) = std::fs::read_dir(&tx_dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Ok(uuid) = name.parse::<uuid::Uuid>() else {
            continue;
        };
        let id = TransactionId::from_uuid(uuid);
        let Ok(record) = store.load(&id) else {
            continue;
        };
        match record.phase {
            zup_transaction::TransactionPhase::Prepared => {
                out.push(RecoveryStatus::PreparedTransaction {
                    transaction_id: id.to_string(),
                });
            }
            zup_transaction::TransactionPhase::Applying
            | zup_transaction::TransactionPhase::RollingBack => {
                out.push(RecoveryStatus::InterruptedApplyingTransaction {
                    transaction_id: id.to_string(),
                });
            }
            zup_transaction::TransactionPhase::RecoveryRequired => {
                out.push(RecoveryStatus::RecoveryRequired {
                    transaction_id: id.to_string(),
                });
            }
            _ => out.push(RecoveryStatus::NoRecoveryNeeded),
        }
    }
    out
}

#[derive(Debug)]
pub struct RuntimeSession {
    pub session_id: zup_protocol::SessionId,
    pub state: RuntimeState,
    cancel: CancellationHandle,
    events: broadcast::Sender<RuntimeEvent>,
}

impl RuntimeSession {
    pub fn subscribe(&self) -> broadcast::Receiver<RuntimeEvent> {
        self.events.subscribe()
    }

    pub fn cancellation(&self) -> CancellationHandle {
        self.cancel.clone()
    }

    pub async fn cancel(&self) {
        self.cancel.cancel();
    }
}

pub type RuntimePayloadSource = Arc<dyn PayloadSource + Send + Sync>;

pub type RuntimeFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub struct RuntimeControl {
    pub cancellation: CancellationHandle,
    pub events: broadcast::Sender<RuntimeEvent>,
    pub policy: ExecutionPolicy,
}

impl RuntimeControl {
    pub fn new(
        cancellation: CancellationHandle,
        events: broadcast::Sender<RuntimeEvent>,
        policy: ExecutionPolicy,
    ) -> Self {
        Self {
            cancellation,
            events,
            policy,
        }
    }
}

pub trait RuntimeBackend: Send + Sync {
    fn payload_source(&self, request: &RuntimeRequest) -> RuntimePayloadSource;

    fn execute<'a>(
        &'a self,
        request: RuntimeRequest,
        control: RuntimeControl,
    ) -> RuntimeFuture<'a, Result<InstallOutcome, SessionError>>;
}

pub async fn run_install(
    backend: &dyn RuntimeBackend,
    request: RuntimeRequest,
) -> Result<(InstallOutcome, RuntimeSession), SessionError> {
    let session_id = zup_protocol::SessionId::new_v7();
    let (events, _) = broadcast::channel(256);
    let cancel = CancellationHandle::new();
    let outcome =
        run_install_control(backend, request.clone(), cancel.clone(), events.clone()).await?;
    let state = if request.scope == SelectedScope::Machine {
        RuntimeState::WaitingForAuthorization
    } else {
        RuntimeState::Preparing
    };
    Ok((
        outcome,
        RuntimeSession {
            session_id,
            state,
            cancel,
            events,
        },
    ))
}

pub async fn run_install_control(
    backend: &dyn RuntimeBackend,
    request: RuntimeRequest,
    cancel: CancellationHandle,
    events: broadcast::Sender<RuntimeEvent>,
) -> Result<InstallOutcome, SessionError> {
    run_install_control_with_policy(
        backend,
        request,
        cancel,
        events,
        ExecutionPolicy::Interactive,
    )
    .await
}

pub async fn run_install_control_with_policy(
    backend: &dyn RuntimeBackend,
    request: RuntimeRequest,
    cancel: CancellationHandle,
    events: broadcast::Sender<RuntimeEvent>,
    policy: ExecutionPolicy,
) -> Result<InstallOutcome, SessionError> {
    if request.target != request.transaction_plan.target {
        return Err(SessionError::PlanInvalid(
            "runtime target does not match the finalized transaction plan".into(),
        ));
    }
    if request
        .bootstrap
        .as_ref()
        .is_some_and(|bootstrap| bootstrap.plan.plan.key.target != request.target)
    {
        return Err(SessionError::PlanInvalid(
            "runtime target does not match the bootstrap plan".into(),
        ));
    }
    let session_log = SessionLog::start(&request, "lifecycle");
    if let Some(log) = &session_log {
        let _ = events.send(RuntimeEvent::LogPath {
            path: log.path().display().to_string(),
        });
        log.event(
            "plan_received",
            serde_json::json!({
                "target": request.target.to_string(),
                "total_work": request.transaction_plan.total_work(),
            }),
        );
    }
    let result = if cancel.is_cancelled() {
        Ok(InstallOutcome::Cancelled)
    } else {
        let _ = backend.payload_source(&request);
        let control = RuntimeControl::new(cancel, events.clone(), policy);
        backend.execute(request, control).await
    };
    match &result {
        Ok(outcome) => {
            if let Some(log) = &session_log {
                log.event(
                    "finished",
                    serde_json::json!({ "outcome": format!("{outcome:?}") }),
                );
            }
        }
        Err(error) => {
            if let Some(log) = &session_log {
                log.event("failed", serde_json::json!({ "error": error.to_string() }));
            }
        }
    }
    emit_terminal(&events, &result);
    result
}

pub async fn run_local_install(
    backend: &dyn RuntimeBackend,
    request: RuntimeRequest,
) -> Result<(InstallOutcome, RuntimeSession), SessionError> {
    run_install(backend, request).await
}

pub(crate) fn emit_terminal(
    events: &broadcast::Sender<RuntimeEvent>,
    result: &Result<InstallOutcome, SessionError>,
) {
    match result {
        Ok(InstallOutcome::Committed) => {
            let _ = events.send(RuntimeEvent::Completed {
                outcome: "committed".into(),
            });
        }
        Ok(InstallOutcome::RebootRequired {
            prerequisite_id,
            exit_code,
        }) => {
            let _ = events.send(RuntimeEvent::RebootRequired {
                id: prerequisite_id.clone(),
                exit_code: *exit_code,
            });
            let _ = events.send(RuntimeEvent::Completed {
                outcome: "reboot_required".into(),
            });
        }
        Ok(InstallOutcome::RolledBack) => {
            let _ = events.send(RuntimeEvent::RollingBack);
            let _ = events.send(RuntimeEvent::Completed {
                outcome: "rolled_back".into(),
            });
        }
        Ok(InstallOutcome::Cancelled) => {
            let _ = events.send(RuntimeEvent::StateChanged {
                state: RuntimeState::Cancelled,
            });
            let _ = events.send(RuntimeEvent::Completed {
                outcome: "cancelled".into(),
            });
        }
        Ok(InstallOutcome::RecoveryRequired) => {
            let _ = events.send(RuntimeEvent::Failed {
                kind: "recovery_required".into(),
                message: "recovery required".into(),
            });
        }
        Ok(InstallOutcome::Busy { operation }) => {
            let _ = events.send(RuntimeEvent::Failed {
                kind: "installation_busy".into(),
                message: format!("{operation} is already running for this installation"),
            });
        }
        Ok(InstallOutcome::Failed(message)) => {
            let _ = events.send(RuntimeEvent::Failed {
                kind: "transaction".into(),
                message: message.clone(),
            });
        }
        Err(SessionError::Cancelled) => {
            let _ = events.send(RuntimeEvent::StateChanged {
                state: RuntimeState::Cancelled,
            });
            let _ = events.send(RuntimeEvent::Completed {
                outcome: "cancelled".into(),
            });
        }
        Err(SessionError::InstallationBusy) => {
            let _ = events.send(RuntimeEvent::Failed {
                kind: "installation_busy".into(),
                message: "another maintenance operation is running for this installation".into(),
            });
        }
        Err(error) => {
            let kind = match error {
                SessionError::AuthorizationRequired => "authorization_required",
                SessionError::AuthorizationCancelled => "authorization_cancelled",
                SessionError::RecoveryRequired => "recovery_required",
                _ => "session",
            };
            let _ = events.send(RuntimeEvent::Failed {
                kind: kind.into(),
                message: error.to_string(),
            });
        }
    }
}

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeState {
    Preparing,
    CheckingPrerequisites,
    InstallingPrerequisites,
    WaitingForAuthorization,
    RebootRequired,
    ConnectingWorker,
    Executing,
    RollingBack,
    Completed,
    Cancelled,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum RuntimeEvent {
    StateChanged {
        state: RuntimeState,
    },
    WaitingForAuthorization,
    WorkerConnected,
    PrerequisiteCheck {
        id: String,
        name: String,
        satisfied: bool,
        version: Option<String>,
    },
    PrerequisiteDownload {
        id: String,
        completed: u64,
        total: Option<u64>,
    },
    PrerequisiteInstall {
        id: String,
        name: String,
    },
    RebootRequired {
        id: String,
        exit_code: i32,
    },
    PreflightStarted,
    ResourceBlocked {
        detail: String,
        pids: Vec<u32>,
    },
    StagingStarted {
        id: String,
    },
    StagingProgress {
        id: String,
        detail: String,
    },
    OperationStarted {
        id: String,
    },
    Progress {
        completed: u64,
        total: u64,
        action: String,
    },
    RollingBack,
    Completed {
        outcome: String,
    },
    Failed {
        kind: String,
        message: String,
    },
    LogPath {
        path: String,
    },
}

use std::io::Write;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::json;
use tracing_appender::non_blocking::{NonBlocking, NonBlockingBuilder, WorkerGuard};
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use uuid::Uuid;

pub struct SessionLog {
    path: PathBuf,
    operation: String,
    session_id: Uuid,
    writer: Arc<Mutex<NonBlocking>>,
    _guard: WorkerGuard,
}

impl SessionLog {
    pub fn start(request: &RuntimeRequest, operation: &str) -> Option<Self> {
        let session_id = Uuid::now_v7();
        let directory = std::env::temp_dir().join("zup").join("sessions");
        std::fs::create_dir_all(&directory).ok()?;
        let appender =
            RollingFileAppender::new(Rotation::NEVER, &directory, format!("{session_id}.log"));
        let (writer, guard) = NonBlockingBuilder::default().lossy(true).finish(appender);
        let log = Self {
            path: directory.join(format!("{session_id}.log")),
            operation: operation.to_owned(),
            session_id,
            writer: Arc::new(Mutex::new(writer)),
            _guard: guard,
        };
        log.event(
            "started",
            json!({
                "operation": operation,
                "app_id": request.app_id.as_str(),
                "app_version": request.app_version.to_string(),
                "scope": request.scope,
                "transaction_id": request.recovery_id.map(|id| id.to_string()),
                "state_root": request.state_root.display().to_string(),
            }),
        );
        Some(log)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn event(&self, name: &str, detail: serde_json::Value) {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_millis())
            .unwrap_or_default();
        let line = json!({
            "timestamp_ms": timestamp,
            "session_id": self.session_id,
            "event": name,
            "detail": detail,
        });
        if let Ok(mut writer) = self.writer.lock() {
            let _ = writeln!(writer, "{line}");
            let _ = writer.flush();
        }
        tracing::info!(session_id = %self.session_id, event = name, "session diagnostic");
    }

    pub fn summary(&self, request: &RuntimeRequest) -> String {
        format!(
            "zup session {}\noperation: {}\napp: {} {}\nscope: {}\nlog: {}",
            self.session_id,
            self.operation,
            request.app_id,
            request.app_version,
            scope_label(request.scope),
            self.path.display()
        )
    }
}

fn scope_label(scope: SelectedScope) -> &'static str {
    match scope {
        SelectedScope::User => "user",
        SelectedScope::Machine => "machine",
    }
}
