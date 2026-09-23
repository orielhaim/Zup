//! Installation session dispatch, local execution, and recovery.

use std::path::PathBuf;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use zup_bundle::AutoPayloadSource;
use zup_core::{AppId, SelectedScope};
use zup_exec::ExecutionPlan;
use zup_transaction::{
    CancellationProbe, FilesystemTransactionStore, OperationExecutor, OperationReceipt,
    ReconcileResult, TransactionCoordinator, TransactionError, TransactionId, TransactionNode,
    TransactionOutcome, TransactionStore, compile_transaction,
};
use zup_windows::{
    FilePrecondition, InstallLedgerStore, InstallationLock, NullProgress, WindowsFileExecutor,
};

use crate::events::{RuntimeEvent, RuntimeState};

/// Session errors (typed, not flattened).
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("plan validation failed: {0}")]
    PlanInvalid(String),

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
pub struct RuntimeRequest {
    pub app_id: AppId,
    pub app_version: semver::Version,
    pub scope: SelectedScope,
    pub execution_plan: ExecutionPlan,
    pub state_root: PathBuf,
    pub work_root: PathBuf,
    /// Payload root for development `DirectoryPayloadSource`.
    pub payload_root: PathBuf,
    pub recovery_id: Option<TransactionId>,
}

/// Final outcome of a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallOutcome {
    Committed,
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
    if request.recovery_id.is_none() {
        validate_request(&request)?;
    }

    let needs_elevation = request.scope == SelectedScope::Machine
        || request.execution_plan.summary.requires_elevation;
    let already_elevated =
        zup_windows::is_process_elevated().map_err(|e| SessionError::Protocol(e.to_string()))?;

    if needs_elevation && !already_elevated {
        // Secure UAC worker route.
        return run_elevated_worker(request).await;
    }

    // Local (user-scope) or already-elevated privileged local path.
    run_local_install(request).await
}

/// Run a user-scope (or already-elevated) installation locally.
pub async fn run_local_install(
    request: RuntimeRequest,
) -> Result<(InstallOutcome, RuntimeSession), SessionError> {
    if (request.scope == SelectedScope::Machine
        || request.execution_plan.summary.requires_elevation)
        && !zup_windows::is_process_elevated().map_err(|e| SessionError::Protocol(e.to_string()))?
    {
        return Err(SessionError::Protocol(
            "machine mutation requires elevation".into(),
        ));
    }
    let session_id = zup_protocol::SessionId::new_v7();
    let (events, _) = broadcast::channel(256);
    let cancel = CancellationHandle {
        token: CancellationToken::new(),
    };
    let session = RuntimeSession {
        session_id,
        state: RuntimeState::Preparing,
        cancel: cancel.clone(),
        events: events.clone(),
    };

    let _ = events.send(RuntimeEvent::StateChanged {
        state: RuntimeState::Preparing,
    });
    if request.recovery_id.is_none() {
        validate_request(&request)?;
    }
    let _ = events.send(RuntimeEvent::StateChanged {
        state: RuntimeState::Executing,
    });

    let outcome = tokio::task::spawn_blocking({
        let cancel = cancel.probe();
        move || execute_local_blocking(request, cancel)
    })
    .await
    .map_err(|e| SessionError::WorkerCrashed(e.to_string()))?;

    emit_outcome(&events, &outcome);
    Ok((outcome, session))
}

/// Elevated worker route (UAC + named pipe + one-shot worker).
async fn run_elevated_worker(
    request: RuntimeRequest,
) -> Result<(InstallOutcome, RuntimeSession), SessionError> {
    let session_id = zup_protocol::SessionId::new_v7();
    let (events, _) = broadcast::channel(256);
    let cancel = CancellationHandle {
        token: CancellationToken::new(),
    };
    let session = RuntimeSession {
        session_id,
        state: RuntimeState::WaitingForElevation,
        cancel: cancel.clone(),
        events: events.clone(),
    };

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
                    | zup_transaction::ManagedResource::FileType,
                ..
            } | zup_transaction::NodeKind::OwnedRemoval {
                resource: zup_transaction::ManagedResource::PathEntry
                    | zup_transaction::ManagedResource::Protocol
                    | zup_transaction::ManagedResource::FileType,
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
    tokio::time::timeout(zup_windows::WORKER_CONNECT_TIMEOUT, server.connect())
        .await
        .map_err(|_| SessionError::WorkerExited(None))?
        .map_err(|e| SessionError::Protocol(e.to_string()))?;
    zup_windows::verify_client_pid(server.as_raw() as isize, worker.pid())
        .map_err(|e| SessionError::Protocol(e.to_string()))?;

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
                    let _ = events.send(RuntimeEvent::OperationStarted { id: progress.detail });
                }
                zup_protocol::Message::Completed(completed) => {
                    let outcome = match completed.outcome.as_str() {
                        "committed" => InstallOutcome::Committed,
                        "rolled_back" => InstallOutcome::RolledBack,
                        "recovery_required" => InstallOutcome::RecoveryRequired,
                        other => return Err(SessionError::Protocol(format!("unknown worker outcome {other}"))),
                    };
                    emit_outcome(&events, &outcome);
                    return Ok((outcome, session));
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
        InstallOutcome::Committed => {
            let _ = events.send(RuntimeEvent::Completed {
                outcome: "committed".into(),
            });
        }
        InstallOutcome::RolledBack => {
            let _ = events.send(RuntimeEvent::RollingBack);
        }
        InstallOutcome::Cancelled => {
            let _ = events.send(RuntimeEvent::StateChanged {
                state: RuntimeState::Cancelled,
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

fn validate_request(request: &RuntimeRequest) -> Result<(), SessionError> {
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
                    | zup_transaction::ManagedResource::FileType,
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
pub fn execute_local_blocking(
    request: RuntimeRequest,
    cancel: impl CancellationProbe + Send + 'static,
) -> InstallOutcome {
    let payload = match AutoPayloadSource::from_path(request.payload_root.clone()) {
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

/// Production `OperationExecutor` wrapping `WindowsFileExecutor`.
struct ProductionExecutor<P: zup_bundle::PayloadSource, C: CancellationProbe> {
    inner: WindowsFileExecutor<P>,
    cancel: C,
}

impl<P: zup_bundle::PayloadSource, C: CancellationProbe> OperationExecutor
    for ProductionExecutor<P, C>
{
    type Error = String;

    fn apply(&mut self, operation: &TransactionNode) -> Result<OperationReceipt, Self::Error> {
        if self.cancel.is_cancelled() {
            return Err("cancelled".into());
        }
        if matches!(operation.kind, zup_transaction::NodeKind::Barrier) {
            return Ok(OperationReceipt::Control);
        }
        if matches!(
            operation.kind,
            zup_transaction::NodeKind::ManagedIntegration { .. }
        ) {
            return zup_windows::apply_managed(operation).map_err(|e| e.to_string());
        }
        if let zup_transaction::NodeKind::OwnedRemoval { resource, .. } = operation.kind {
            return if resource == zup_transaction::ManagedResource::File {
                self.inner
                    .apply_owned_file_removal(operation)
                    .map_err(|e| e.to_string())
            } else {
                zup_windows::apply_owned_removal(operation).map_err(|e| e.to_string())
            };
        }
        // Delegate file nodes to the Windows executor via apply_node.
        let (source_relative, dest) = extract_file_identity(operation)?;
        let receipt = zup_windows::apply_node(&mut self.inner, operation, &source_relative, &dest)
            .map_err(|e| e.to_string())?;
        Ok(map_receipt(receipt))
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
            let ResourceKey::File { destination } = key else {
                return Err("not a file resource".into());
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
