//! Installation session dispatch, local execution, and recovery.

use std::path::{Path, PathBuf};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use zup_bootstrap::{
    BootstrapOutcome, BootstrapState, BootstrapStateStore, BoundBootstrapPlan,
    FilesystemBootstrapStateStore, Quarantine, execute_plan_with_persist,
};
use zup_bundle::AutoPayloadSource;
use zup_core::{AppId, SelectedScope};
use zup_exec::ExecutionPlan;
use zup_transaction::{
    CancellationProbe, FilesystemTransactionStore, OperationExecutor, OperationReceipt,
    ReconcileResult, TransactionCoordinator, TransactionError, TransactionId, TransactionNode,
    TransactionOutcome, TransactionStore, compile_transaction,
};
use zup_windows::{
    FilePrecondition, InstallLedgerStore, InstallationLock, NullProgress,
    PAYLOAD_OVERLAY_DIRECTORY, PayloadOverlayIdentity, WindowsFileExecutor,
    cleanup_payload_overlay, payload_overlay_base_root, verify_payload_overlay,
};

use crate::diagnostics::SessionLog;
use crate::events::{RuntimeEvent, RuntimeState};

/// Session errors (typed, not flattened).
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("plan validation failed: {0}")]
    PlanInvalid(String),

    #[error("elevation required")]
    ElevationRequired,

    #[error("elevation cancelled by user")]
    ElevationCancelled,

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

/// High-level install request (frontend-independent).
#[derive(Clone)]
pub struct RuntimeRequest {
    pub app_id: AppId,
    pub app_version: semver::Version,
    pub scope: SelectedScope,
    pub execution_plan: ExecutionPlan,
    pub state_root: PathBuf,
    pub work_root: PathBuf,
    /// Payload root for development `DirectoryPayloadSource`.
    pub payload_root: PathBuf,
    pub payload_overlay_root: Option<PathBuf>,
    pub payload_overlay_base_root: Option<PathBuf>,
    pub recovery_id: Option<TransactionId>,
    pub bootstrap: Option<BootstrapRequest>,
}

#[derive(Clone)]
pub struct BootstrapRequest {
    pub plan: BoundBootstrapPlan,
    pub state_root: PathBuf,
    pub quarantine_root: PathBuf,
}

struct OverlayCleanup {
    base: Option<PathBuf>,
    root: Option<PathBuf>,
    retain: bool,
}

impl OverlayCleanup {
    fn from_request(request: &RuntimeRequest) -> Self {
        let root = request.payload_overlay_root.clone();
        let base = request.payload_overlay_base_root.clone().or_else(|| {
            root.as_ref()
                .and_then(|_| payload_overlay_base_root(&request.state_root, request.scope).ok())
        });
        Self {
            base,
            root,
            retain: false,
        }
    }

    fn retain(&mut self) {
        self.retain = true;
    }
}

impl Drop for OverlayCleanup {
    fn drop(&mut self) {
        if !self.retain
            && let Some(root) = &self.root
            && let Some(base) = &self.base
        {
            let _ = cleanup_payload_overlay(base, Some(root));
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionPolicy {
    Interactive,
    NonInteractive,
}

impl ExecutionPolicy {
    pub fn allows_elevation(self) -> bool {
        matches!(self, Self::Interactive)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayPolicy {
    Cleanup,
    RetainOnBlocked,
}

/// Final outcome of a session.
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
    Failed(String),
}

/// Handle for cooperative cancellation.
#[derive(Clone, Debug)]
pub struct CancellationHandle {
    token: CancellationToken,
}

impl CancellationHandle {
    /// Create a cooperative cancellation token for an externally hosted session.
    pub fn new() -> Self {
        Self {
            token: CancellationToken::new(),
        }
    }

    pub fn cancel(&self) {
        self.token.cancel();
    }

    pub fn is_cancelled(&self) -> bool {
        self.token.is_cancelled()
    }

    pub fn probe(&self) -> TokenProbe {
        TokenProbe(self.token.clone())
    }
}

impl zup_plan::CancellationQuery for CancellationHandle {
    fn is_cancelled(&self) -> bool {
        self.is_cancelled()
    }
}

impl Default for CancellationHandle {
    fn default() -> Self {
        Self::new()
    }
}

/// Adapts `CancellationToken` to `CancellationProbe`.
#[derive(Clone)]
pub struct TokenProbe(CancellationToken);

impl CancellationProbe for TokenProbe {
    fn is_cancelled(&self) -> bool {
        self.0.is_cancelled()
    }
}

/// Recovery discovery result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryStatus {
    NoRecoveryNeeded,
    PreparedTransaction { transaction_id: String },
    InterruptedApplyingTransaction { transaction_id: String },
    RecoveryRequired { transaction_id: String },
}

/// Discover unfinished transactions under `state_root` (read-only).
pub fn discover_recovery(state_root: &PathBuf) -> Vec<RecoveryStatus> {
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

/// A live installation session (Kameo-owned orchestration state).
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

/// Production dispatch: local vs elevated worker vs already-elevated.
pub async fn run_install(
    request: RuntimeRequest,
) -> Result<(InstallOutcome, RuntimeSession), SessionError> {
    let session_id = zup_protocol::SessionId::new_v7();
    let (events, _) = broadcast::channel(256);
    let cancel = CancellationHandle::new();
    let state = if request.scope == SelectedScope::Machine
        || request.execution_plan.summary.requires_elevation
    {
        RuntimeState::WaitingForElevation
    } else {
        RuntimeState::Preparing
    };
    let outcome = run_install_control(request, cancel.clone(), events.clone()).await?;
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

async fn run_bootstrap_phase(
    request: &BootstrapRequest,
    cancel: &CancellationHandle,
    events: &broadcast::Sender<RuntimeEvent>,
    policy: ExecutionPolicy,
) -> Result<BootstrapOutcome, SessionError> {
    if cancel.is_cancelled() {
        return Err(SessionError::Cancelled);
    }
    let _ = events.send(RuntimeEvent::StateChanged {
        state: RuntimeState::CheckingPrerequisites,
    });
    let quarantine = Quarantine::new(&request.quarantine_root)
        .map_err(|error| SessionError::Prerequisite(error.to_string()))?;
    let bootstrap_lock_key = InstallationLock::lock_key(
        request.plan.plan.key.app_id.as_str(),
        &request.plan.plan.key.scope.to_string(),
    );
    let _bootstrap_lock =
        InstallationLock::try_acquire(&request.quarantine_root, &bootstrap_lock_key)
            .map_err(|_| SessionError::InstallationBusy)?
            .ok_or(SessionError::InstallationBusy)?;
    for artifact in request.plan.artifacts.values() {
        quarantine
            .verify(artifact)
            .map_err(|error| SessionError::Prerequisite(error.to_string()))?;
    }
    let elevated = zup_windows::is_process_elevated()
        .map_err(|error| SessionError::Protocol(error.to_string()))?;
    let bootstrap_state_root = if request.plan.plan.key.scope == SelectedScope::Machine && !elevated
    {
        request.quarantine_root.join("state")
    } else {
        request.state_root.clone()
    };
    let store = FilesystemBootstrapStateStore::new(&bootstrap_state_root);
    let mut state = match store.load(request.plan.id) {
        Ok(state) => state,
        Err(zup_bootstrap::BootstrapStoreError::Missing) => {
            let mut state = BootstrapState::new(&request.plan.plan);
            state.id = request.plan.id;
            store
                .create(&state)
                .map_err(|error| SessionError::Prerequisite(error.to_string()))?;
            state
        }
        Err(error) => return Err(SessionError::Prerequisite(error.to_string())),
    };
    state
        .validate(&request.plan.plan)
        .map_err(|error| SessionError::Prerequisite(error.to_string()))?;
    let detector = zup_windows::WindowsPrerequisiteDetector;
    if state.operations.iter().any(|operation| {
        matches!(
            operation.state,
            zup_bootstrap::BootstrapOperationState::Running
        )
    }) {
        let old_revision = state.revision;
        let recovered = zup_bootstrap::recover(&request.plan.plan, &detector, &mut state)
            .map_err(|error| SessionError::Prerequisite(error.to_string()))?;
        state.revision = old_revision.saturating_add(1);
        store
            .compare_and_swap(old_revision, &state)
            .map_err(|error| SessionError::Prerequisite(error.to_string()))?;
        if matches!(recovered, BootstrapOutcome::RecoveryRequired) {
            return Ok(BootstrapOutcome::RecoveryRequired);
        }
    }
    zup_bootstrap::assess(&request.plan.plan, &detector, &mut state)
        .map_err(|error| SessionError::Prerequisite(error.to_string()))?;
    for operation in &request.plan.plan.operations {
        if let Some(state) = state.operation_mut(&operation.id) {
            let satisfied = matches!(
                state,
                zup_bootstrap::BootstrapOperationState::Satisfied { .. }
            );
            let version = match state {
                zup_bootstrap::BootstrapOperationState::Satisfied { version, .. } => {
                    version.as_ref().map(ToString::to_string)
                }
                _ => None,
            };
            let _ = events.send(RuntimeEvent::PrerequisiteCheck {
                id: operation.id.to_string(),
                name: operation.name.clone(),
                satisfied,
                version,
            });
        }
    }
    if state.remaining.is_empty() {
        let revision = state.revision;
        state.recompute();
        state.revision = revision.saturating_add(1);
        store
            .compare_and_swap(revision, &state)
            .map_err(|error| SessionError::Prerequisite(error.to_string()))?;
        return Ok(BootstrapOutcome::Ready);
    }
    let assessed_revision = state.revision;
    state.revision = assessed_revision.saturating_add(1);
    state.recompute();
    store
        .compare_and_swap(assessed_revision, &state)
        .map_err(|error| SessionError::Prerequisite(error.to_string()))?;
    let needs_elevation = request
        .plan
        .plan
        .operations
        .iter()
        .filter(|operation| state.remaining.contains(&operation.id))
        .any(|operation| operation.installer.privilege == zup_core::Privilege::Machine);
    if needs_elevation {
        if !policy.allows_elevation() {
            return Err(SessionError::ElevationRequired);
        }
        for operation in &request.plan.plan.operations {
            if state.remaining.contains(&operation.id) {
                let _ = events.send(RuntimeEvent::PrerequisiteInstall {
                    id: operation.id.to_string(),
                    name: operation.name.clone(),
                });
            }
        }
        return run_elevated_bootstrap(request, events, cancel.clone()).await;
    }
    let _ = events.send(RuntimeEvent::StateChanged {
        state: RuntimeState::InstallingPrerequisites,
    });
    for operation in &request.plan.plan.operations {
        if state.remaining.contains(&operation.id) {
            let _ = events.send(RuntimeEvent::PrerequisiteInstall {
                id: operation.id.to_string(),
                name: operation.name.clone(),
            });
        }
    }
    let plan = request.plan.clone();
    let root = request.quarantine_root.clone();
    let store_for_task = store.clone();
    let store_inside_task = store_for_task.clone();
    let (outcome, mut completed_state, persisted_revision) =
        tokio::task::spawn_blocking(move || {
            let detector = zup_windows::WindowsPrerequisiteDetector;
            let provider = zup_windows::WindowsPrerequisiteProvider;
            let quarantine = match Quarantine::new(root) {
                Ok(quarantine) => quarantine,
                Err(error) => {
                    let revision = state.revision;
                    return (
                        Err(zup_bootstrap::BootstrapError::Provider(error.to_string())),
                        state,
                        revision,
                    );
                }
            };
            let mut revision = state.revision;
            let mut persist =
                |snapshot: &BootstrapState| -> Result<(), zup_bootstrap::BootstrapError> {
                    let mut next = snapshot.clone();
                    let expected = revision;
                    next.revision =
                        expected
                            .checked_add(1)
                            .ok_or(zup_bootstrap::BootstrapError::Limit(
                                "bootstrap revision overflow",
                            ))?;
                    store_inside_task
                        .compare_and_swap(expected, &next)
                        .map_err(|error| {
                            zup_bootstrap::BootstrapError::Provider(error.to_string())
                        })?;
                    revision = next.revision;
                    Ok(())
                };
            let outcome = execute_plan_with_persist(
                &plan.plan,
                &detector,
                &provider,
                |operation| {
                    let artifact = plan.artifacts.get(&operation.id).ok_or_else(|| {
                        zup_bootstrap::BootstrapError::MissingArtifact(operation.id.to_string())
                    })?;
                    quarantine
                        .resolve(&artifact.relative_path)
                        .map_err(|error| zup_bootstrap::BootstrapError::Provider(error.to_string()))
                },
                &mut state,
                &mut persist,
            );
            (outcome, state, revision)
        })
        .await
        .map_err(|error| SessionError::Prerequisite(error.to_string()))?;
    completed_state.revision = persisted_revision;
    if let Err(error) = &outcome {
        if completed_state.operations.iter().any(|operation| {
            matches!(
                operation.state,
                zup_bootstrap::BootstrapOperationState::Running
                    | zup_bootstrap::BootstrapOperationState::Failed { .. }
            )
        }) {
            completed_state.phase = zup_bootstrap::BootstrapPhase::RecoveryRequired;
        } else {
            completed_state.recompute();
        }
        let old_revision = completed_state.revision;
        completed_state.revision = old_revision.saturating_add(1);
        let _ = store_for_task.compare_and_swap(old_revision, &completed_state);
        return Err(SessionError::Prerequisite(error.to_string()));
    }
    completed_state.recompute();
    let outcome = outcome.map_err(|error| SessionError::Prerequisite(error.to_string()))?;
    Ok(outcome)
}

async fn run_elevated_bootstrap(
    request: &BootstrapRequest,
    events: &broadcast::Sender<RuntimeEvent>,
    cancel: CancellationHandle,
) -> Result<BootstrapOutcome, SessionError> {
    let _ = events.send(RuntimeEvent::WaitingForElevation);
    let session_id = zup_protocol::SessionId::new_v7();
    let pipe = zup_windows::pipe_name(&session_id.to_string());
    let mut server = zup_windows::PipeServer::create(&pipe)
        .map_err(|error| SessionError::Protocol(error.to_string()))?;
    let bootstrap_json = serde_json::to_string(&request.plan)
        .map_err(|error| SessionError::Prerequisite(error.to_string()))?;
    let plan_hash = zup_windows::plan_hash_hex(&bootstrap_json);
    let bootstrap = zup_windows::WorkerBootstrap {
        protocol_version: zup_protocol::PROTOCOL_VERSION,
        session_id,
        pipe_name: pipe.clone(),
        expected_parent_pid: std::process::id(),
        expected_parent_sid: zup_windows::UserSid::current()
            .map_err(|error| SessionError::Protocol(error.to_string()))?
            .display()
            .to_owned(),
        expected_plan_hash: plan_hash.clone(),
    };
    let params = format!(
        "__worker {}",
        zup_windows::quote_arg(&zup_windows::format_bootstrap(&bootstrap))
    );
    let executable = zup_windows::current_exe()
        .map_err(|error| SessionError::WorkerLaunch(error.to_string()))?;
    let worker =
        zup_windows::launch_elevated_worker(&executable, &params).map_err(|error| match error {
            zup_windows::TransportError::ElevationCancelled => SessionError::ElevationCancelled,
            other => SessionError::WorkerLaunch(other.to_string()),
        })?;
    server
        .connect_worker(worker.pid())
        .await
        .map_err(|error| SessionError::Protocol(error.to_string()))?;
    let (mut reader, mut writer) =
        zup_windows::frame_server(server.into_inner().expect("connected server"));
    let hello = tokio::time::timeout(zup_windows::HELLO_TIMEOUT, reader.recv())
        .await
        .map_err(|_| SessionError::Protocol("worker hello timeout".into()))?
        .map_err(|error| SessionError::Protocol(error.to_string()))?;
    let zup_protocol::Message::WorkerHello(hello) = hello.message else {
        return Err(SessionError::Protocol("expected WorkerHello".into()));
    };
    if hello.protocol_version != zup_protocol::PROTOCOL_VERSION
        || hello.session_id != session_id
        || hello.worker_pid != worker.pid()
        || !hello.capabilities.has_prerequisite_bootstrap_v1()
    {
        return Err(SessionError::Protocol(
            "bootstrap worker capability missing".into(),
        ));
    }
    let _ = events.send(RuntimeEvent::WorkerConnected);
    writer
        .send(&zup_protocol::WireEnvelope {
            version: zup_protocol::PROTOCOL_VERSION,
            session_id,
            sequence: 1,
            message: zup_protocol::Message::ParentHello(zup_protocol::ParentHello {
                protocol_version: zup_protocol::PROTOCOL_VERSION,
                session_id,
                transaction_id: request.plan.id.as_uuid(),
                expected_plan_hash: plan_hash.clone(),
            }),
        })
        .await
        .map_err(|error| SessionError::Protocol(error.to_string()))?;
    writer
        .send(&zup_protocol::WireEnvelope {
            version: zup_protocol::PROTOCOL_VERSION,
            session_id,
            sequence: 2,
            message: zup_protocol::Message::ExecuteBootstrap(zup_protocol::ExecuteBootstrap {
                bootstrap_json,
                bootstrap_hash: plan_hash,
                bootstrap_id: request.plan.id.as_uuid(),
                app_id: request.plan.plan.key.app_id.to_string(),
                app_version: request.plan.plan.key.app_version.to_string(),
                scope: request.plan.plan.key.scope.to_string(),
                state_root: request.state_root.display().to_string(),
                quarantine_root: request.quarantine_root.display().to_string(),
                recovery_id: None,
            }),
        })
        .await
        .map_err(|error| SessionError::Protocol(error.to_string()))?;
    let mut cancel_sent = false;
    loop {
        tokio::select! {
            () = cancel.token.cancelled(), if !cancel_sent => {
                writer
                    .send(&zup_protocol::WireEnvelope {
                        version: zup_protocol::PROTOCOL_VERSION,
                        session_id,
                        sequence: 3,
                        message: zup_protocol::Message::Cancel,
                    })
                    .await
                    .map_err(|error| SessionError::Protocol(error.to_string()))?;
                cancel_sent = true;
            }
            message = reader.recv() => {
                let message = message.map_err(|error| SessionError::Protocol(error.to_string()))?;
                match message.message {
                    zup_protocol::Message::Completed(completed) => {
                        if completed.transaction_id != request.plan.id.as_uuid() {
                            return Err(SessionError::Protocol("bootstrap completion identity mismatch".into()));
                        }
                        return Ok(match completed.outcome.as_str() {
                            "committed" => BootstrapOutcome::Ready,
                            "reboot_required" => {
                                let exit_code = completed.exit_code.ok_or_else(|| {
                                    SessionError::Protocol("bootstrap completion omitted reboot code".into())
                                })?;
                                let prerequisite_id = completed
                                    .prerequisite_id
                                    .as_deref()
                                    .ok_or_else(|| {
                                        SessionError::Protocol(
                                            "bootstrap completion omitted prerequisite identity".into(),
                                        )
                                    })
                                    .and_then(|value| {
                                        zup_core::PrerequisiteId::new(value).map_err(|error| {
                                            SessionError::Protocol(error.to_string())
                                        })
                                    })?;
                                let operation = request
                                    .plan
                                    .plan
                                    .operation(&prerequisite_id)
                                    .ok_or_else(|| {
                                        SessionError::Protocol(
                                            "bootstrap completion referenced an unknown prerequisite".into(),
                                        )
                                    })?;
                                if exit_code != 1641
                                    && !operation.installer.reboot_exit_codes.contains(&exit_code)
                                {
                                    return Err(SessionError::Protocol(
                                        "bootstrap completion returned an invalid reboot code".into(),
                                    ));
                                }
                                BootstrapOutcome::RebootRequired {
                                    exit_code,
                                    prerequisite_id,
                                }
                            }
                            "recovery_required" => BootstrapOutcome::RecoveryRequired,
                            _ => {
                                return Err(SessionError::Protocol(
                                    "unknown bootstrap completion outcome".into(),
                                ));
                            }
                        });
                    }
                    zup_protocol::Message::Failed(failed)
                        if cancel_sent && failed.kind == "cancelled" =>
                    {
                        return Err(SessionError::Cancelled);
                    }
                    zup_protocol::Message::Failed(failed) => {
                        return Err(SessionError::Prerequisite(failed.message));
                    }
                    zup_protocol::Message::Progress(progress) => {
                        let _ = events.send(RuntimeEvent::OperationStarted { id: progress.detail });
                    }
                    _ => return Err(SessionError::Protocol("unexpected bootstrap worker message".into())),
                }
            }
        }
    }
}

/// Run an operation using control and event channels owned by a frontend.
///
/// The frontend can subscribe before this future starts and request safe
/// cancellation without owning or terminating the worker process.
pub async fn run_install_control(
    request: RuntimeRequest,
    cancel: CancellationHandle,
    events: broadcast::Sender<RuntimeEvent>,
) -> Result<InstallOutcome, SessionError> {
    run_install_control_with_policy(
        request,
        cancel,
        events,
        ExecutionPolicy::Interactive,
        OverlayPolicy::Cleanup,
    )
    .await
}

pub async fn run_install_control_with_policy(
    request: RuntimeRequest,
    cancel: CancellationHandle,
    events: broadcast::Sender<RuntimeEvent>,
    policy: ExecutionPolicy,
    overlay_policy: OverlayPolicy,
) -> Result<InstallOutcome, SessionError> {
    let bootstrap = request.bootstrap.clone();
    run_install_control_with_bootstrap(request, bootstrap, cancel, events, policy, overlay_policy)
        .await
}

pub async fn run_install_control_with_bootstrap(
    request: RuntimeRequest,
    bootstrap: Option<BootstrapRequest>,
    cancel: CancellationHandle,
    events: broadcast::Sender<RuntimeEvent>,
    policy: ExecutionPolicy,
    overlay_policy: OverlayPolicy,
) -> Result<InstallOutcome, SessionError> {
    let session_log = SessionLog::start(&request, "lifecycle");
    if let Some(log) = &session_log {
        let _ = events.send(RuntimeEvent::LogPath {
            path: log.path().display().to_string(),
        });
        log.event(
            "plan_validated",
            serde_json::json!({ "total_work": request.execution_plan.summary.write_bytes }),
        );
    }
    let mut cleanup = OverlayCleanup::from_request(&request);
    let recovery = request.recovery_id.is_some();
    let bootstrap_for_cleanup = bootstrap.clone();
    let result = run_install_control_inner(request, bootstrap, cancel, events, policy).await;
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
    if matches!(result, Ok(InstallOutcome::Committed))
        && let Some(bootstrap) = bootstrap_for_cleanup
    {
        let _ = FilesystemBootstrapStateStore::new(&bootstrap.state_root).remove(bootstrap.plan.id);
        let _ = std::fs::remove_dir_all(&bootstrap.quarantine_root);
    }
    match result {
        Ok(InstallOutcome::RecoveryRequired) => cleanup.retain(),
        Ok(InstallOutcome::Failed(ref message))
            if overlay_policy == OverlayPolicy::RetainOnBlocked
                && message == "blocked by running applications" =>
        {
            cleanup.retain()
        }
        Ok(_) => {}
        Err(_) if recovery => cleanup.retain(),
        Err(_) => {}
    }
    result
}

async fn run_install_control_inner(
    request: RuntimeRequest,
    bootstrap: Option<BootstrapRequest>,
    cancel: CancellationHandle,
    events: broadcast::Sender<RuntimeEvent>,
    policy: ExecutionPolicy,
) -> Result<InstallOutcome, SessionError> {
    validate_runtime_request(&request)?;
    if let Some(bootstrap) = bootstrap {
        match run_bootstrap_phase(&bootstrap, &cancel, &events, policy).await? {
            BootstrapOutcome::Ready => {}
            BootstrapOutcome::RecoveryRequired => {
                return Err(SessionError::RecoveryRequired);
            }
            BootstrapOutcome::RebootRequired {
                exit_code,
                prerequisite_id,
            } => {
                let id = prerequisite_id.to_string();
                let outcome = InstallOutcome::RebootRequired {
                    prerequisite_id: id,
                    exit_code,
                };
                emit_outcome(&events, &outcome);
                return Ok(outcome);
            }
        }
    }

    let needs_elevation = request.scope == SelectedScope::Machine
        || request.execution_plan.summary.requires_elevation;
    let already_elevated = if needs_elevation {
        Some(
            zup_windows::is_process_elevated()
                .map_err(|e| SessionError::Protocol(e.to_string()))?,
        )
    } else {
        None
    };
    if needs_elevation && already_elevated == Some(false) && !policy.allows_elevation() {
        return Err(SessionError::ElevationRequired);
    }

    let mutating_paths = request
        .execution_plan
        .files
        .iter()
        .filter(|file| {
            matches!(
                file.kind,
                zup_exec::FileOperationKind::Create | zup_exec::FileOperationKind::Replace
            )
        })
        .map(|file| file.destination.as_path().to_path_buf())
        .chain(
            request
                .execution_plan
                .removals
                .iter()
                .filter_map(|removal| {
                    if let zup_core::ResourceKey::File { destination } = &removal.key {
                        Some(std::path::PathBuf::from(destination))
                    } else {
                        None
                    }
                }),
        )
        .collect::<Vec<_>>();
    let blocker_paths = mutating_paths
        .iter()
        .map(|path| path.as_path())
        .collect::<Vec<_>>();
    let _ = events.send(RuntimeEvent::PreflightStarted);
    match zup_windows::preflight(&blocker_paths)
        .map_err(|error| SessionError::Transaction(format!("Restart Manager preflight: {error}")))?
    {
        zup_windows::FilePreflight::Ready => {}
        zup_windows::FilePreflight::Blocked {
            processes,
            reboot_reason,
        } => {
            let mut detail = processes
                .iter()
                .map(|process| format!("{} (PID {})", process.name, process.pid))
                .collect::<Vec<_>>();
            if detail.is_empty() {
                detail.push(format!(
                    "Windows requested a restart (reason {reboot_reason})"
                ));
            }
            let _ = events.send(RuntimeEvent::BlockingProcessesFound {
                pids: processes.iter().map(|process| process.pid).collect(),
                detail: detail.join("\n"),
            });
            return Ok(InstallOutcome::Failed(
                "blocked by running applications".into(),
            ));
        }
    }

    if needs_elevation && already_elevated == Some(false) {
        return run_elevated_worker(request, cancel, events).await;
    }

    // Local (user-scope) or already-elevated privileged local path.
    run_local_install_control(request, cancel, events).await
}

async fn run_local_install_with_bootstrap(
    request: RuntimeRequest,
    bootstrap: Option<BootstrapRequest>,
    cancel: CancellationHandle,
    events: broadcast::Sender<RuntimeEvent>,
) -> Result<InstallOutcome, SessionError> {
    let outcome = if let Some(bootstrap) = bootstrap.as_ref() {
        match run_bootstrap_phase(bootstrap, &cancel, &events, ExecutionPolicy::Interactive).await?
        {
            BootstrapOutcome::Ready => run_local_install_control(request, cancel, events).await?,
            BootstrapOutcome::RecoveryRequired => InstallOutcome::RecoveryRequired,
            BootstrapOutcome::RebootRequired {
                exit_code,
                prerequisite_id,
            } => InstallOutcome::RebootRequired {
                prerequisite_id: prerequisite_id.to_string(),
                exit_code,
            },
        }
    } else {
        run_local_install_control(request, cancel, events).await?
    };
    if matches!(outcome, InstallOutcome::Committed)
        && let Some(bootstrap) = bootstrap
    {
        let _ = FilesystemBootstrapStateStore::new(&bootstrap.state_root).remove(bootstrap.plan.id);
        let _ = std::fs::remove_dir_all(&bootstrap.quarantine_root);
    }
    Ok(outcome)
}

/// Run a user-scope (or already-elevated) installation locally.
pub async fn run_local_install(
    request: RuntimeRequest,
) -> Result<(InstallOutcome, RuntimeSession), SessionError> {
    let session_id = zup_protocol::SessionId::new_v7();
    let (events, _) = broadcast::channel(256);
    let cancel = CancellationHandle::new();
    let bootstrap = request.bootstrap.clone();
    let outcome =
        run_local_install_with_bootstrap(request, bootstrap, cancel.clone(), events.clone())
            .await?;
    Ok((
        outcome,
        RuntimeSession {
            session_id,
            state: RuntimeState::Executing,
            cancel,
            events,
        },
    ))
}

async fn run_local_install_control(
    request: RuntimeRequest,
    cancel: CancellationHandle,
    events: broadcast::Sender<RuntimeEvent>,
) -> Result<InstallOutcome, SessionError> {
    let mut cleanup = OverlayCleanup::from_request(&request);
    let recovery = request.recovery_id.is_some();
    let result = run_local_install_control_inner(request, cancel, events).await;
    match result {
        Ok(InstallOutcome::RecoveryRequired) => cleanup.retain(),
        Ok(_) => {}
        Err(_) if recovery => cleanup.retain(),
        Err(_) => {}
    }
    result
}

async fn run_local_install_control_inner(
    request: RuntimeRequest,
    cancel: CancellationHandle,
    events: broadcast::Sender<RuntimeEvent>,
) -> Result<InstallOutcome, SessionError> {
    if (request.scope == SelectedScope::Machine
        || request.execution_plan.summary.requires_elevation)
        && !zup_windows::is_process_elevated().map_err(|e| SessionError::Protocol(e.to_string()))?
    {
        return Err(SessionError::Protocol(
            "machine mutation requires elevation".into(),
        ));
    }
    let _ = events.send(RuntimeEvent::StateChanged {
        state: RuntimeState::Preparing,
    });
    validate_runtime_request(&request)?;
    let _ = events.send(RuntimeEvent::StateChanged {
        state: RuntimeState::Executing,
    });
    let total_work = compile_transaction(&request.execution_plan)
        .map(|plan| transaction_work_total(&plan))
        .map_err(|error| SessionError::PlanInvalid(error.to_string()))?;
    let _ = events.send(RuntimeEvent::Progress {
        completed: 0,
        total: total_work,
        action: "Preparing…".into(),
    });

    let outcome = tokio::task::spawn_blocking({
        let cancel = cancel.probe();
        let events = events.clone();
        move || execute_local_blocking_with_events(request, cancel, Some(events))
    })
    .await
    .map_err(|e| SessionError::WorkerCrashed(e.to_string()))?;

    if outcome == InstallOutcome::Committed {
        let _ = events.send(RuntimeEvent::Progress {
            completed: total_work,
            total: total_work,
            action: "Finishing…".into(),
        });
    }

    emit_outcome(&events, &outcome);
    Ok(outcome)
}

/// Elevated worker route (UAC + named pipe + one-shot worker).
async fn run_elevated_worker(
    request: RuntimeRequest,
    cancel: CancellationHandle,
    events: broadcast::Sender<RuntimeEvent>,
) -> Result<InstallOutcome, SessionError> {
    let session_id = zup_protocol::SessionId::new_v7();

    let _ = events.send(RuntimeEvent::WaitingForElevation);

    // Create the secured pipe BEFORE launching the worker.
    let pipe = zup_windows::pipe_name(&session_id.to_string());
    let mut server = zup_windows::PipeServer::create(&pipe)
        .map_err(|e| SessionError::Protocol(e.to_string()))?;

    let plan = if let Some(id) = request.recovery_id {
        let record = FilesystemTransactionStore::new(&request.state_root)
            .load(&id)
            .map_err(|e| SessionError::Transaction(e.to_string()))?;
        if record.app_id != request.app_id
            || record.scope != request.scope
            || record.app_version != request.app_version
        {
            return Err(SessionError::Protocol("recovery identity mismatch".into()));
        }
        let identity = PayloadOverlayIdentity::from_transaction(
            request.app_id.clone(),
            request.app_version.clone(),
            request.scope,
            &record.plan,
        )
        .map_err(|error| SessionError::PlanInvalid(error.to_string()))?;
        validate_overlay_identity(
            &request.state_root,
            request.scope,
            &identity,
            request.payload_overlay_root.as_deref(),
            request.payload_overlay_base_root.as_deref(),
            true,
        )
        .map_err(SessionError::PlanInvalid)?;
        if identity.has_files() {
            verify_payload_overlay(
                request
                    .payload_overlay_base_root
                    .as_deref()
                    .expect("validated overlay base"),
                &identity,
                request
                    .payload_overlay_root
                    .as_deref()
                    .expect("validated overlay path"),
            )
            .map_err(|error| SessionError::PlanInvalid(error.to_string()))?;
        }
        record.plan
    } else {
        compile_transaction(&request.execution_plan)
            .map_err(|e| SessionError::PlanInvalid(e.to_string()))?
    };
    let has_lifecycle = plan.uninstall
        || !plan.retired_keys.is_empty()
        || plan.nodes.iter().any(|node| {
            matches!(
                node.kind,
                zup_transaction::NodeKind::FileMutation {
                    delta: zup_exec::Delta::RestoreOwned | zup_exec::Delta::RepairOwned,
                    ..
                } | zup_transaction::NodeKind::ManagedIntegration {
                    delta: zup_exec::Delta::RestoreOwned,
                    ..
                }
            )
        });
    let plan_json =
        serde_json::to_string(&plan).map_err(|e| SessionError::PlanInvalid(e.to_string()))?;
    let total_work = transaction_work_total(&plan);
    let _ = events.send(RuntimeEvent::Progress {
        completed: 0,
        total: total_work,
        action: "Preparing…".into(),
    });
    let has_shortcut_service = plan.nodes.iter().any(|node| {
        matches!(
            node.kind,
            zup_transaction::NodeKind::ManagedIntegration {
                resource: zup_transaction::ManagedResource::Shortcut
                    | zup_transaction::ManagedResource::Service,
                ..
            } | zup_transaction::NodeKind::OwnedRemoval {
                resource: zup_transaction::ManagedResource::Shortcut
                    | zup_transaction::ManagedResource::Service,
                ..
            }
        )
    });
    let has_managed = plan.nodes.iter().any(|node| {
        matches!(
            node.kind,
            zup_transaction::NodeKind::ManagedIntegration {
                resource: zup_transaction::ManagedResource::PathEntry
                    | zup_transaction::ManagedResource::Protocol
                    | zup_transaction::ManagedResource::FileType
                    | zup_transaction::ManagedResource::UninstallEntry,
                ..
            } | zup_transaction::NodeKind::OwnedRemoval {
                resource: zup_transaction::ManagedResource::PathEntry
                    | zup_transaction::ManagedResource::Protocol
                    | zup_transaction::ManagedResource::FileType
                    | zup_transaction::ManagedResource::UninstallEntry,
                ..
            }
        )
    });
    let plan_hash = zup_windows::plan_hash_hex(&plan_json);

    let bootstrap = zup_windows::WorkerBootstrap {
        protocol_version: zup_protocol::PROTOCOL_VERSION,
        session_id,
        pipe_name: pipe.clone(),
        expected_parent_pid: std::process::id(),
        expected_parent_sid: zup_windows::UserSid::current()
            .map_err(|e| SessionError::Protocol(e.to_string()))?
            .display()
            .to_owned(),
        expected_plan_hash: plan_hash.clone(),
    };
    let bootstrap_arg = zup_windows::format_bootstrap(&bootstrap);
    let params = format!("__worker {}", zup_windows::quote_arg(&bootstrap_arg));
    let exe = zup_windows::current_exe().map_err(|e| SessionError::WorkerLaunch(e.to_string()))?;

    let worker = zup_windows::launch_elevated_worker(&exe, &params).map_err(|e| match e {
        zup_windows::TransportError::ElevationCancelled => SessionError::ElevationCancelled,
        other => SessionError::WorkerLaunch(other.to_string()),
    })?;
    server
        .connect_worker(worker.pid())
        .await
        .map_err(|error| SessionError::Protocol(error.to_string()))?;

    let (mut reader, mut writer) =
        zup_windows::frame_server(server.into_inner().expect("connected server"));
    let hello = tokio::time::timeout(zup_windows::HELLO_TIMEOUT, reader.recv())
        .await
        .map_err(|_| SessionError::Protocol("worker hello timeout".into()))?
        .map_err(|e| SessionError::Protocol(e.to_string()))?;
    let mut worker_sequence = zup_protocol::SequenceTracker::new();
    worker_sequence
        .accept(hello.sequence)
        .map_err(|e| SessionError::Protocol(e.to_string()))?;
    let zup_protocol::Message::WorkerHello(hello) = hello.message else {
        return Err(SessionError::Protocol("expected WorkerHello".into()));
    };
    if hello.protocol_version != zup_protocol::PROTOCOL_VERSION
        || hello.session_id != session_id
        || hello.worker_pid != worker.pid()
        || !hello.capabilities.has_file_transactions_v1()
        || (has_managed && !hello.capabilities.has_managed_integrations_v1())
        || (has_shortcut_service && !hello.capabilities.has_shortcut_service_v1())
        || (has_lifecycle && !hello.capabilities.has_lifecycle_v1())
    {
        return Err(SessionError::Protocol(
            "worker hello authentication failed".into(),
        ));
    }
    let _ = events.send(RuntimeEvent::WorkerConnected);

    writer
        .send(&zup_protocol::WireEnvelope {
            version: zup_protocol::PROTOCOL_VERSION,
            session_id,
            sequence: 1,
            message: zup_protocol::Message::ParentHello(zup_protocol::ParentHello {
                protocol_version: zup_protocol::PROTOCOL_VERSION,
                session_id,
                transaction_id: uuid::Uuid::now_v7(),
                expected_plan_hash: plan_hash.clone(),
            }),
        })
        .await
        .map_err(|e| SessionError::Protocol(e.to_string()))?;
    writer
        .send(&zup_protocol::WireEnvelope {
            version: zup_protocol::PROTOCOL_VERSION,
            session_id,
            sequence: 2,
            message: zup_protocol::Message::ExecuteTransaction(zup_protocol::ExecuteTransaction {
                plan_json,
                plan_hash,
                app_id: request.app_id.to_string(),
                app_version: request.app_version.to_string(),
                scope: match request.scope {
                    SelectedScope::User => "user",
                    SelectedScope::Machine => "machine",
                }
                .into(),
                payload_root: request.payload_root.display().to_string(),
                payload_overlay_root: request
                    .payload_overlay_root
                    .as_ref()
                    .map(|path| path.display().to_string()),
                payload_overlay_base_root: request
                    .payload_overlay_base_root
                    .as_ref()
                    .map(|path| path.display().to_string()),
                state_root: request.state_root.display().to_string(),
                work_root: request.work_root.display().to_string(),
                recovery_id: request.recovery_id.map(|id| id.as_uuid()),
            }),
        })
        .await
        .map_err(|e| SessionError::Protocol(e.to_string()))?;

    let mut cancel_sent = false;
    loop {
        tokio::select! {
            () = cancel.token.cancelled(), if !cancel_sent => {
                writer.send(&zup_protocol::WireEnvelope {
                    version: zup_protocol::PROTOCOL_VERSION,
                    session_id,
                    sequence: 3,
                    message: zup_protocol::Message::Cancel,
                }).await.map_err(|e| SessionError::Protocol(e.to_string()))?;
                cancel_sent = true;
            }
            message = reader.recv() => {
                let message = message.map_err(|e| SessionError::Protocol(e.to_string()))?;
                worker_sequence.accept(message.sequence)
                    .map_err(|e| SessionError::Protocol(e.to_string()))?;
                match message.message {
                zup_protocol::Message::Progress(progress) => {
                    if progress.kind == zup_protocol::ProgressKind::OperationProgress {
                        if let (Some(completed), Some(total)) = (progress.completed, progress.total) {
                            let _ = events.send(RuntimeEvent::Progress {
                                completed,
                                total,
                                action: progress.detail,
                            });
                        }
                    } else {
                        let _ = events.send(RuntimeEvent::OperationStarted { id: progress.detail });
                    }
                }
                zup_protocol::Message::Completed(completed) => {
                    let outcome = match completed.outcome.as_str() {
                        "committed" => InstallOutcome::Committed,
                        "rolled_back" => InstallOutcome::RolledBack,
                        "recovery_required" => InstallOutcome::RecoveryRequired,
                        other => return Err(SessionError::Protocol(format!("unknown worker outcome {other}"))),
                    };
                    let mut cleanup = OverlayCleanup::from_request(&request);
                    if matches!(outcome, InstallOutcome::RecoveryRequired) {
                        cleanup.retain();
                    }
                    emit_outcome(&events, &outcome);
                    return Ok(outcome);
                }
                zup_protocol::Message::Failed(failed) => return Err(SessionError::Transaction(failed.message)),
                _ => return Err(SessionError::Protocol("unexpected worker message".into())),
                }
            }
        }
    }
}

fn emit_outcome(events: &broadcast::Sender<RuntimeEvent>, outcome: &InstallOutcome) {
    match outcome {
        InstallOutcome::RebootRequired {
            prerequisite_id,
            exit_code,
        } => {
            let _ = events.send(RuntimeEvent::RebootRequired {
                id: prerequisite_id.clone(),
                exit_code: *exit_code,
            });
            let _ = events.send(RuntimeEvent::Completed {
                outcome: "reboot_required".into(),
            });
        }
        InstallOutcome::Committed => {
            let _ = events.send(RuntimeEvent::Completed {
                outcome: "committed".into(),
            });
        }
        InstallOutcome::RolledBack => {
            let _ = events.send(RuntimeEvent::RollingBack);
            let _ = events.send(RuntimeEvent::Completed {
                outcome: "rolled_back".into(),
            });
        }
        InstallOutcome::Cancelled => {
            let _ = events.send(RuntimeEvent::StateChanged {
                state: RuntimeState::Cancelled,
            });
            let _ = events.send(RuntimeEvent::Completed {
                outcome: "cancelled".into(),
            });
        }
        InstallOutcome::RecoveryRequired => {
            let _ = events.send(RuntimeEvent::Failed {
                kind: "recovery_required".into(),
                message: "recovery required".into(),
            });
        }
        InstallOutcome::Failed(msg) => {
            let _ = events.send(RuntimeEvent::Failed {
                kind: "transaction".into(),
                message: msg.clone(),
            });
        }
    }
}

fn validate_runtime_request(request: &RuntimeRequest) -> Result<(), SessionError> {
    let Some(id) = request.recovery_id else {
        return validate_request(request);
    };
    let record = FilesystemTransactionStore::new(&request.state_root)
        .load(&id)
        .map_err(|error| SessionError::Transaction(error.to_string()))?;
    if record.app_id != request.app_id
        || record.app_version != request.app_version
        || record.scope != request.scope
    {
        return Err(SessionError::Protocol("recovery identity mismatch".into()));
    }
    let identity = PayloadOverlayIdentity::from_transaction(
        request.app_id.clone(),
        request.app_version.clone(),
        request.scope,
        &record.plan,
    )
    .map_err(|error| SessionError::PlanInvalid(error.to_string()))?;
    validate_overlay_identity(
        &request.state_root,
        request.scope,
        &identity,
        request.payload_overlay_root.as_deref(),
        request.payload_overlay_base_root.as_deref(),
        true,
    )
    .map_err(SessionError::PlanInvalid)?;
    if identity.has_files() {
        let overlay = request
            .payload_overlay_root
            .as_deref()
            .expect("validated overlay path");
        verify_payload_overlay(
            request
                .payload_overlay_base_root
                .as_deref()
                .expect("validated overlay base"),
            &identity,
            overlay,
        )
        .map_err(|error| SessionError::PlanInvalid(error.to_string()))?;
    }
    Ok(())
}

fn validate_overlay_identity(
    state_root: &Path,
    scope: zup_core::SelectedScope,
    identity: &PayloadOverlayIdentity,
    actual: Option<&Path>,
    overlay_base_root: Option<&Path>,
    recovery: bool,
) -> Result<(), String> {
    let expected_base =
        payload_overlay_base_root(state_root, scope).map_err(|error| error.to_string())?;
    if identity.has_files() {
        let base = overlay_base_root.ok_or_else(|| {
            "generated plugin payload requires a deterministic overlay base".to_owned()
        })?;
        if base != expected_base {
            return Err(format!(
                "payload overlay base mismatch: expected `{}`, found `{}`",
                expected_base.display(),
                base.display()
            ));
        }
        let expected = identity
            .path_under(base)
            .ok_or_else(|| "generated payload identity has no path".to_owned())?;
        let actual = actual.ok_or_else(|| {
            "generated plugin payload requires a deterministic overlay".to_owned()
        })?;
        if actual != expected {
            return Err(format!(
                "payload overlay path mismatch: expected `{}`, found `{}`",
                expected.display(),
                actual.display()
            ));
        }
        return Ok(());
    }
    if overlay_base_root.is_some() {
        return Err("overlay base supplied without generated plugin payload".into());
    }
    if let Some(actual) = actual {
        if recovery {
            return Err("recovery supplied an overlay without generated plugin payload".into());
        }
        let namespace = expected_base.join(PAYLOAD_OVERLAY_DIRECTORY);
        if !actual.is_absolute() || !actual.starts_with(namespace) {
            return Err("payload overlay path is outside the state overlay namespace".into());
        }
    }
    Ok(())
}

fn validate_request(request: &RuntimeRequest) -> Result<(), SessionError> {
    let identity = PayloadOverlayIdentity::from_execution_plan(
        request.app_id.clone(),
        request.app_version.clone(),
        request.scope,
        &request.execution_plan,
    )
    .map_err(|error| SessionError::PlanInvalid(error.to_string()))?;
    validate_overlay_identity(
        &request.state_root,
        request.scope,
        &identity,
        request.payload_overlay_root.as_deref(),
        request.payload_overlay_base_root.as_deref(),
        false,
    )
    .map_err(SessionError::PlanInvalid)?;
    let plan = compile_transaction(&request.execution_plan)
        .map_err(|e| SessionError::PlanInvalid(e.to_string()))?;
    for node in &plan.nodes {
        match node.kind {
            zup_transaction::NodeKind::Barrier
            | zup_transaction::NodeKind::StageFile { .. }
            | zup_transaction::NodeKind::FileMutation { .. } => {}
            zup_transaction::NodeKind::ManagedIntegration {
                resource:
                    zup_transaction::ManagedResource::Shortcut
                    | zup_transaction::ManagedResource::Service
                    | zup_transaction::ManagedResource::PathEntry
                    | zup_transaction::ManagedResource::Protocol
                    | zup_transaction::ManagedResource::FileType
                    | zup_transaction::ManagedResource::UninstallEntry,
                ..
            } => {}
            zup_transaction::NodeKind::OwnedRemoval { .. } => {}
            _ => {
                return Err(SessionError::PlanInvalid(format!(
                    "unsupported node {}",
                    node.id
                )));
            }
        }
    }
    let _ = request;
    Ok(())
}

/// Blocking local execution with the production `WindowsFileExecutor`.
fn execute_local_blocking_with_events(
    request: RuntimeRequest,
    cancel: impl CancellationProbe + Send + 'static,
    events: Option<broadcast::Sender<RuntimeEvent>>,
) -> InstallOutcome {
    execute_local_blocking_with_events_inner(request, cancel, events)
}

fn execute_local_blocking_with_events_inner(
    request: RuntimeRequest,
    cancel: impl CancellationProbe + Send + 'static,
    events: Option<broadcast::Sender<RuntimeEvent>>,
) -> InstallOutcome {
    let payload = match AutoPayloadSource::from_paths(
        request.payload_root.clone(),
        request.payload_overlay_root.clone(),
    ) {
        Ok(source) => source,
        Err(error) => return InstallOutcome::Failed(format!("payload package: {error}")),
    };
    let lock_key = InstallationLock::lock_key(
        request.app_id.as_str(),
        match request.scope {
            SelectedScope::User => "user",
            SelectedScope::Machine => "machine",
        },
    );
    let _lock = match InstallationLock::try_acquire(&request.state_root, &lock_key) {
        Ok(Some(lock)) => lock,
        Ok(None) => return InstallOutcome::Failed("installation busy".into()),
        Err(e) => return InstallOutcome::Failed(e.to_string()),
    };

    let store = FilesystemTransactionStore::new(&request.state_root);
    let coordinator = TransactionCoordinator::new(store);
    if let Some(id) = request.recovery_id {
        let record = match FilesystemTransactionStore::new(&request.state_root).load(&id) {
            Ok(record)
                if record.app_id == request.app_id
                    && record.scope == request.scope
                    && record.app_version == request.app_version =>
            {
                record
            }
            Ok(_) => return InstallOutcome::Failed("recovery identity mismatch".into()),
            Err(error) => return InstallOutcome::Failed(error.to_string()),
        };
        let mut executor = ProductionExecutor {
            inner: WindowsFileExecutor::new(
                payload,
                request.work_root.clone(),
                id.to_string(),
                Box::new(NullProgress),
            ),
            cancel,
            events,
            completed_work: 0,
            total_work: transaction_work_total(&record.plan),
        };
        note_plan_files(&mut executor.inner, &record.plan);
        return match zup_transaction::recover(
            record,
            &FilesystemTransactionStore::new(&request.state_root),
            &mut executor,
        ) {
            Ok((record, TransactionOutcome::Committed)) => {
                match InstallLedgerStore::new(&request.state_root)
                    .repair_committed(&request.app_id, request.scope)
                {
                    Ok(_) => {
                        zup_windows::notify_committed_path_change(&record);
                        InstallOutcome::Committed
                    }
                    Err(error) => InstallOutcome::Failed(error.to_string()),
                }
            }
            Ok((_, TransactionOutcome::RolledBack)) => InstallOutcome::RolledBack,
            Ok((_, TransactionOutcome::RecoveryRequired)) => InstallOutcome::RecoveryRequired,
            Err(error) => InstallOutcome::Failed(error.to_string()),
        };
    }
    if let Err(e) = InstallLedgerStore::new(&request.state_root)
        .repair_committed(&request.app_id, request.scope)
    {
        return InstallOutcome::Failed(e.to_string());
    }
    let plan = match compile_transaction(&request.execution_plan) {
        Ok(plan) => plan,
        Err(e) => return InstallOutcome::Failed(e.to_string()),
    };
    if let Err(e) = InstallLedgerStore::new(&request.state_root).validate_plan(
        &request.app_id,
        request.scope,
        &request.app_version,
        &plan,
    ) {
        return InstallOutcome::Failed(e.to_string());
    }
    let record = match coordinator.begin(
        request.app_id.clone(),
        request.scope,
        request.app_version.clone(),
        plan,
    ) {
        Ok(r) => r,
        Err(e) => return InstallOutcome::Failed(e.to_string()),
    };

    if cancel.is_cancelled() {
        return InstallOutcome::Cancelled;
    }

    let mut executor = ProductionExecutor {
        inner: WindowsFileExecutor::new(
            payload,
            request.work_root.clone(),
            record.transaction_id.to_string(),
            Box::new(NullProgress),
        ),
        cancel,
        events,
        completed_work: 0,
        total_work: transaction_work_total(&record.plan),
    };

    // Register the immutable payload identity under each transaction operation.
    note_plan_files(&mut executor.inner, &record.plan);

    match coordinator.execute(record, &mut executor) {
        Ok((record, TransactionOutcome::Committed)) => {
            match InstallLedgerStore::new(&request.state_root)
                .publish_committed(&record, request.scope)
            {
                Ok(_) => {
                    zup_windows::notify_committed_path_change(&record);
                    InstallOutcome::Committed
                }
                Err(e) => InstallOutcome::Failed(format!(
                    "transaction committed but ledger publication failed: {e}"
                )),
            }
        }
        Ok((_record, TransactionOutcome::RolledBack)) => InstallOutcome::RolledBack,
        Ok((_record, TransactionOutcome::RecoveryRequired)) => InstallOutcome::RecoveryRequired,
        Err(TransactionError::Executor { message, .. }) if message.contains("cancelled") => {
            InstallOutcome::Cancelled
        }
        Err(e) => InstallOutcome::Failed(e.to_string()),
    }
}

fn note_plan_files<P: zup_bundle::PayloadSource>(
    executor: &mut WindowsFileExecutor<P>,
    plan: &zup_transaction::TransactionPlan,
) {
    for node in &plan.nodes {
        if matches!(
            node.kind,
            zup_transaction::NodeKind::StageFile { .. }
                | zup_transaction::NodeKind::FileMutation { .. }
        ) {
            let (sha256, size) = match (node.meta.expected_sha256, node.meta.expected_size) {
                (Some(h), Some(s)) => (h, s),
                _ => continue,
            };
            let precondition = node
                .meta
                .file_precondition
                .unwrap_or(FilePrecondition::Absent);
            executor.note_file(&node.id, precondition, sha256, size);
        }
    }
}

fn operation_work(operation: &TransactionNode) -> u64 {
    if matches!(
        operation.kind,
        zup_transaction::NodeKind::StageFile { .. }
            | zup_transaction::NodeKind::FileMutation { .. }
    ) {
        operation.meta.expected_size.unwrap_or(1).max(1)
    } else {
        1
    }
}

fn transaction_work_total(plan: &zup_transaction::TransactionPlan) -> u64 {
    plan.nodes.iter().map(operation_work).sum::<u64>().max(1)
}

fn operation_action(operation: &TransactionNode) -> String {
    match &operation.kind {
        zup_transaction::NodeKind::StageFile { .. }
        | zup_transaction::NodeKind::FileMutation { .. } => "Installing files…".into(),
        zup_transaction::NodeKind::ManagedIntegration {
            resource: zup_transaction::ManagedResource::Service,
            ..
        }
        | zup_transaction::NodeKind::OwnedRemoval {
            resource: zup_transaction::ManagedResource::Service,
            ..
        } => "Registering services…".into(),
        zup_transaction::NodeKind::ManagedIntegration {
            resource: zup_transaction::ManagedResource::Shortcut,
            ..
        }
        | zup_transaction::NodeKind::OwnedRemoval {
            resource: zup_transaction::ManagedResource::Shortcut,
            ..
        } => "Updating shortcuts…".into(),
        zup_transaction::NodeKind::Barrier => "Finishing…".into(),
        _ => "Updating application settings…".into(),
    }
}

/// Production `OperationExecutor` wrapping `WindowsFileExecutor`.
struct ProductionExecutor<P: zup_bundle::PayloadSource, C: CancellationProbe> {
    inner: WindowsFileExecutor<P>,
    cancel: C,
    events: Option<broadcast::Sender<RuntimeEvent>>,
    completed_work: u64,
    total_work: u64,
}

impl<P: zup_bundle::PayloadSource, C: CancellationProbe> OperationExecutor
    for ProductionExecutor<P, C>
{
    type Error = String;

    fn apply(&mut self, operation: &TransactionNode) -> Result<OperationReceipt, Self::Error> {
        if self.cancel.is_cancelled() {
            return Err("cancelled".into());
        }
        if let Some(events) = &self.events {
            let _ = events.send(RuntimeEvent::OperationStarted {
                id: operation.id.to_string(),
            });
        }
        let receipt = if matches!(operation.kind, zup_transaction::NodeKind::Barrier) {
            Ok(OperationReceipt::Control)
        } else if matches!(
            operation.kind,
            zup_transaction::NodeKind::ManagedIntegration { .. }
        ) {
            zup_windows::apply_managed(operation).map_err(|e| e.to_string())
        } else if let zup_transaction::NodeKind::OwnedRemoval { resource, .. } = operation.kind {
            if resource == zup_transaction::ManagedResource::File {
                self.inner
                    .apply_owned_file_removal(operation)
                    .map_err(|e| e.to_string())
            } else {
                zup_windows::apply_owned_removal(operation).map_err(|e| e.to_string())
            }
        } else {
            let (source_relative, dest) = extract_file_identity(operation)?;
            zup_windows::apply_node(&mut self.inner, operation, &source_relative, &dest)
                .map(map_receipt)
                .map_err(|e| e.to_string())
        }?;

        self.completed_work = self
            .completed_work
            .saturating_add(operation_work(operation));
        if let Some(events) = &self.events {
            let _ = events.send(RuntimeEvent::Progress {
                completed: self.completed_work.min(self.total_work),
                total: self.total_work,
                action: operation_action(operation),
            });
        }
        Ok(receipt)
    }

    fn rollback(
        &mut self,
        _operation: &TransactionNode,
        receipt: &OperationReceipt,
    ) -> Result<(), Self::Error> {
        match receipt {
            OperationReceipt::StageFile { .. }
            | OperationReceipt::CreateFile { .. }
            | OperationReceipt::ReplaceFile { .. }
            | OperationReceipt::RemoveFile { .. } => self
                .inner
                .rollback_transaction_receipt(receipt)
                .map_err(|e| e.to_string()),
            _ => zup_windows::rollback_managed(receipt).map_err(|e| e.to_string()),
        }
    }

    fn reconcile(
        &mut self,
        operation: &TransactionNode,
        _receipt: Option<&OperationReceipt>,
    ) -> Result<ReconcileResult, Self::Error> {
        if matches!(
            operation.kind,
            zup_transaction::NodeKind::ManagedIntegration { .. }
        ) {
            return zup_windows::reconcile_managed(operation).map_err(|e| e.to_string());
        }
        if let zup_transaction::NodeKind::OwnedRemoval { resource, .. } = operation.kind {
            return if resource == zup_transaction::ManagedResource::File {
                self.inner
                    .reconcile_owned_file_removal(operation)
                    .map_err(|e| e.to_string())
            } else {
                zup_windows::reconcile_owned_removal(operation).map_err(|e| e.to_string())
            };
        }
        self.inner
            .reconcile_transaction_node(operation)
            .map_err(|e| e.to_string())
    }
}

fn extract_file_identity(
    operation: &TransactionNode,
) -> Result<(zup_core::RelativePath, std::path::PathBuf), String> {
    match &operation.kind {
        zup_transaction::NodeKind::StageFile { key }
        | zup_transaction::NodeKind::FileMutation { key, .. } => {
            let destination = match key {
                ResourceKey::File { destination }
                | ResourceKey::Maintenance { destination, .. } => destination,
                _ => return Err("not a file resource".into()),
            };
            let dest = std::path::PathBuf::from(destination);
            let source_relative = operation
                .meta
                .source_relative
                .clone()
                .ok_or_else(|| "file operation has no payload path".to_owned())?;
            Ok((source_relative, dest))
        }
        _ => Err("unsupported node".into()),
    }
}

fn map_receipt(receipt: zup_windows::OperationReceipt) -> OperationReceipt {
    use zup_windows::OperationReceipt as W;
    match receipt {
        W::Control => OperationReceipt::Control,
        W::StageFile(r) => OperationReceipt::StageFile {
            staged_path: r.staged_path,
            size: r.size,
            sha256: r.sha256.to_hex(),
        },
        W::CreateFile(r) => OperationReceipt::CreateFile {
            destination: r.destination,
            installed_sha256: r.installed_sha256.to_hex(),
            installed_size: r.installed_size,
            created_directories: r.created_directories,
        },
        W::ReplaceFile(r) => OperationReceipt::ReplaceFile {
            destination: r.destination,
            previous_sha256: r.previous_sha256.to_hex(),
            previous_size: r.previous_size,
            backup_path: r.backup_path,
            new_sha256: r.new_sha256.to_hex(),
            new_size: r.new_size,
        },
    }
}

use zup_core::ResourceKey;
