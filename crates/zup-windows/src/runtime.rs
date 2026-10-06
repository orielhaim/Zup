//! Windows implementation of the runtime backend.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::broadcast;
use zup_bootstrap::{
    BootstrapOutcome, BootstrapState, BootstrapStateStore, FilesystemBootstrapStateStore,
    Quarantine, execute_plan_with_persist,
};
use zup_bundle::PayloadSource;
use zup_core::SelectedScope;
use zup_runtime::{
    BootstrapRequest, CancellationHandle, ExecutionPolicy, InstallOutcome, RuntimeBackend,
    RuntimeControl, RuntimeEvent, RuntimeFuture, RuntimePayloadSource, RuntimeRequest,
    RuntimeState, SessionError,
};
use zup_transaction::{
    CancellationProbe, FileDelta, FilePrecondition, FilesystemTransactionStore, OperationExecutor,
    OperationReceipt, ReconcileResult, TransactionCoordinator, TransactionError, TransactionId,
    TransactionNode, TransactionOutcome, TransactionPlan, TransactionStore,
};

use crate::{
    AutoPayloadSource, BundleError, InstallLedgerStore, InstallationLock, NullProgress,
    PAYLOAD_OVERLAY_DIRECTORY, PayloadOverlayIdentity, WindowsFileExecutor,
    cleanup_payload_overlay, payload_overlay_base_root, verify_payload_overlay,
};

/// What to record about the release graph a committing request came from.
fn record_release(request: &RuntimeRequest) -> crate::ReleaseRecord<'_> {
    match &request.release {
        Some(identity) => crate::ReleaseRecord::Identity(identity),
        None => crate::ReleaseRecord::None,
    }
}

/// Windows execution policy for temporary payload overlays.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayPolicy {
    Cleanup,
    RetainOnBlocked,
}

/// Outcome prefix for an install a running application blocks, whether the
/// blocker was seen before the request or at a preflight barrier.
const BLOCKED_BY_RUNNING_APPLICATIONS: &str = "blocked by running applications";

/// A request-scoped Windows backend.
#[derive(Clone)]
pub struct WindowsRuntimeBackend {
    payload: RuntimePayloadSource,
    payload_root: PathBuf,
    payload_overlay_root: Option<PathBuf>,
    retain_on_blocked: bool,
}

impl WindowsRuntimeBackend {
    pub fn from_path(
        payload_root: impl Into<PathBuf>,
        payload_overlay_root: Option<PathBuf>,
    ) -> Result<Self, BundleError> {
        let payload_root = payload_root.into();
        let source =
            AutoPayloadSource::from_paths(payload_root.clone(), payload_overlay_root.clone())?;
        Ok(Self {
            payload: Arc::new(source),
            payload_root,
            payload_overlay_root,
            retain_on_blocked: false,
        })
    }

    pub fn from_path_with_generated_files(
        payload_root: impl Into<PathBuf>,
        state_root: &Path,
        scope: SelectedScope,
        install: &zup_plan::InstallPlan,
        generated_files: &[zup_plan::GeneratedFile],
        transaction_plan: &zup_transaction::TransactionPlan,
    ) -> Result<Self, String> {
        let payload_root = payload_root.into();
        let transaction_identity = PayloadOverlayIdentity::from_transaction(
            install.app.id.clone(),
            install.app.version.clone(),
            install.scope,
            transaction_plan,
        )
        .map_err(|error| error.to_string())?;
        let overlay = if !transaction_identity.has_files() || generated_files.is_empty() {
            None
        } else {
            let transaction_sources: BTreeSet<_> = transaction_plan
                .nodes
                .iter()
                .filter_map(|node| node.meta.source_relative.clone())
                .collect();
            let mut overlay_plan = install.clone();
            overlay_plan
                .files
                .retain(|file| transaction_sources.contains(&file.source_relative));
            let overlay_files: Vec<_> = generated_files
                .iter()
                .filter(|file| transaction_sources.contains(&file.source_relative))
                .cloned()
                .collect();
            let base = crate::payload_overlay_base_root(state_root, scope)
                .map_err(|error| error.to_string())?;
            match crate::materialize_payload_overlay(&base, &overlay_plan, &overlay_files) {
                Ok(overlay) => overlay,
                Err(error) => {
                    if let Ok(identity) = PayloadOverlayIdentity::from_install_plan(&overlay_plan)
                        && let Some(overlay) = identity.path_under(&base)
                    {
                        let _ = cleanup_payload_overlay(&base, Some(&overlay));
                    }
                    return Err(error.to_string());
                }
            }
        };
        Self::from_path(payload_root, overlay).map_err(|error| error.to_string())
    }

    pub fn for_recovery(
        payload_root: impl Into<PathBuf>,
        state_root: &Path,
        scope: SelectedScope,
        transaction_id: TransactionId,
    ) -> Result<Self, String> {
        let record = FilesystemTransactionStore::new(state_root)
            .load(&transaction_id)
            .map_err(|error| error.to_string())?;
        let identity = PayloadOverlayIdentity::from_transaction(
            record.app_id,
            record.app_version,
            record.scope,
            &record.plan,
        )
        .map_err(|error| error.to_string())?;
        let base = if identity.has_files() {
            Some(
                crate::payload_overlay_base_root(state_root, scope)
                    .map_err(|error| error.to_string())?,
            )
        } else {
            None
        };
        let overlay = base.as_deref().and_then(|base| identity.path_under(base));
        if let (Some(base), Some(overlay)) = (base.as_deref(), overlay.as_deref()) {
            verify_payload_overlay(base, &identity, overlay).map_err(|error| error.to_string())?;
        }
        Self::from_path(payload_root, overlay).map_err(|error| error.to_string())
    }

    /// A backend whose payload is a verified content cache.
    ///
    /// This is the online and graph-update path: the content came from a release
    /// graph rather than from an image, and the executor reads it by digest. The
    /// executor's own precondition checks are unchanged, so a blob that does not
    /// hash to what the plan names fails at the file operation rather than
    /// producing a file.
    pub fn from_acquired(
        payload: std::sync::Arc<crate::AcquiredPayloadSource>,
        payload_root: impl Into<PathBuf>,
    ) -> Result<Self, BundleError> {
        let payload_root = payload_root.into();
        Ok(Self {
            payload,
            payload_root,
            payload_overlay_root: None,
            retain_on_blocked: false,
        })
    }

    pub fn from_bundle(
        payload_root: impl Into<PathBuf>,
        bundle: crate::EmbeddedBundle,
    ) -> Result<Self, BundleError> {
        let payload_root = payload_root.into();
        let source = bundle.payload_source();
        Ok(Self {
            payload: Arc::new(source),
            payload_root,
            payload_overlay_root: None,
            retain_on_blocked: false,
        })
    }

    pub fn with_overlay_policy(mut self, policy: OverlayPolicy) -> Self {
        self.retain_on_blocked = matches!(policy, OverlayPolicy::RetainOnBlocked);
        self
    }

    pub fn payload_root(&self) -> &Path {
        &self.payload_root
    }

    pub fn payload_overlay_root(&self) -> Option<&Path> {
        self.payload_overlay_root.as_deref()
    }

    pub fn cleanup_overlay(&self, request: &RuntimeRequest) {
        if let Some(overlay) = &self.payload_overlay_root
            && let Ok(base) = crate::payload_overlay_base_root(&request.state_root, request.scope)
        {
            let _ = cleanup_payload_overlay(&base, Some(overlay));
        }
    }
}

impl RuntimeBackend for WindowsRuntimeBackend {
    fn payload_source(&self, _request: &RuntimeRequest) -> RuntimePayloadSource {
        self.payload.clone()
    }

    fn execute<'a>(
        &'a self,
        request: RuntimeRequest,
        control: RuntimeControl,
    ) -> RuntimeFuture<'a, Result<InstallOutcome, SessionError>> {
        Box::pin(async move { self.execute_inner(request, control).await })
    }
}

/// Run a request with a newly created Windows-backed session.
pub async fn run_install(
    backend: &WindowsRuntimeBackend,
    request: RuntimeRequest,
) -> Result<(InstallOutcome, zup_runtime::RuntimeSession), SessionError> {
    zup_runtime::run_install(backend, request).await
}

/// Run a request with frontend-owned cancellation and events.
pub async fn run_install_control(
    backend: &WindowsRuntimeBackend,
    request: RuntimeRequest,
    cancel: CancellationHandle,
    events: broadcast::Sender<RuntimeEvent>,
) -> Result<InstallOutcome, SessionError> {
    zup_runtime::run_install_control(backend, request, cancel, events).await
}

/// Run a request with an explicit authorization policy.
pub async fn run_install_control_with_policy(
    backend: &WindowsRuntimeBackend,
    request: RuntimeRequest,
    cancel: CancellationHandle,
    events: broadcast::Sender<RuntimeEvent>,
    policy: ExecutionPolicy,
) -> Result<InstallOutcome, SessionError> {
    zup_runtime::run_install_control_with_policy(backend, request, cancel, events, policy).await
}

/// Run a request through the Windows backend using its normal dispatch rules.
pub async fn run_local_install(
    backend: &WindowsRuntimeBackend,
    request: RuntimeRequest,
) -> Result<(InstallOutcome, zup_runtime::RuntimeSession), SessionError> {
    zup_runtime::run_local_install(backend, request).await
}

struct OverlayCleanup {
    base: Option<PathBuf>,
    root: Option<PathBuf>,
    retain: bool,
}

impl OverlayCleanup {
    fn from_backend(backend: &WindowsRuntimeBackend, request: &RuntimeRequest) -> Self {
        let root = backend.payload_overlay_root.clone();
        let base = root
            .as_ref()
            .and_then(|_| payload_overlay_base_root(&request.state_root, request.scope).ok());
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
    let quarantine = Quarantine::with_file_system(
        &request.quarantine_root,
        crate::windows_bootstrap_file_system(),
    )
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
    let elevated =
        crate::is_process_elevated().map_err(|error| SessionError::Protocol(error.to_string()))?;
    let bootstrap_state_root = if request.plan.plan.key.scope == SelectedScope::Machine && !elevated
    {
        request.quarantine_root.join("state")
    } else {
        request.state_root.clone()
    };
    let store = FilesystemBootstrapStateStore::with_file_system(
        &bootstrap_state_root,
        crate::windows_bootstrap_file_system(),
    );
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
    let satisfier = crate::WindowsPrerequisiteDetector;
    if state.operations.iter().any(|operation| {
        matches!(
            operation.state,
            zup_bootstrap::BootstrapOperationState::Running
        )
    }) {
        let old_revision = state.revision;
        let recovered = zup_bootstrap::recover(&request.plan.plan, &satisfier, &mut state)
            .map_err(|error| SessionError::Prerequisite(error.to_string()))?;
        state.revision = old_revision.saturating_add(1);
        store
            .compare_and_swap(old_revision, &state)
            .map_err(|error| SessionError::Prerequisite(error.to_string()))?;
        if matches!(recovered, BootstrapOutcome::RecoveryRequired) {
            return Ok(BootstrapOutcome::RecoveryRequired);
        }
    }
    zup_bootstrap::assess(&request.plan.plan, &satisfier, &mut state)
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
    // Windows turns a system-authorized operation into a UAC request; the
    // portable plan only says which operations need host-wide authority.
    let needs_system_authority = request
        .plan
        .plan
        .operations
        .iter()
        .filter(|operation| state.remaining.contains(&operation.id))
        .any(|operation| operation.installer.privilege == zup_core::Privilege::System);
    if needs_system_authority {
        if !policy.allows_authorization() {
            return Err(SessionError::AuthorizationRequired);
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
            let satisfier = crate::WindowsPrerequisiteDetector;
            let provider = crate::WindowsPrerequisiteProvider;
            let quarantine =
                match Quarantine::with_file_system(root, crate::windows_bootstrap_file_system()) {
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
                &satisfier,
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
    let _ = events.send(RuntimeEvent::WaitingForAuthorization);
    let executable =
        crate::current_exe().map_err(|error| SessionError::WorkerLaunch(error.to_string()))?;
    let bundle = crate::EmbeddedBundle::open(&executable)
        .map_err(|error| SessionError::PlanInvalid(format!("worker bundle: {error}")))?;
    crate::validate_embedded_bundle_target(&bundle, &request.plan.plan.key.target)
        .map_err(|error| SessionError::PlanInvalid(error.to_string()))?;
    let session_id = zup_protocol::SessionId::new_v7();
    let pipe = crate::pipe_name(&session_id.to_string());
    let mut server = crate::PipeServer::create(&pipe)
        .map_err(|error| SessionError::Protocol(error.to_string()))?;
    let bootstrap_json = serde_json::to_string(&request.plan)
        .map_err(|error| SessionError::Prerequisite(error.to_string()))?;
    let plan_hash = crate::plan_hash_hex(&bootstrap_json);
    let bootstrap = crate::WorkerBootstrap {
        protocol_version: zup_protocol::PROTOCOL_VERSION,
        session_id,
        pipe_name: pipe.clone(),
        expected_parent_pid: std::process::id(),
        expected_parent_sid: crate::UserSid::current()
            .map_err(|error| SessionError::Protocol(error.to_string()))?
            .display()
            .to_owned(),
        target: request.plan.plan.key.target.clone(),
        expected_plan_hash: plan_hash.clone(),
    };
    let params = format!(
        "__worker {}",
        crate::quote_arg(&crate::format_bootstrap(&bootstrap))
    );
    let executable =
        crate::current_exe().map_err(|error| SessionError::WorkerLaunch(error.to_string()))?;
    let worker =
        crate::launch_elevated_worker(&executable, &params).map_err(|error| match error {
            crate::TransportError::ElevationCancelled => SessionError::AuthorizationCancelled,
            other => SessionError::WorkerLaunch(other.to_string()),
        })?;
    server
        .connect_worker(worker.pid())
        .await
        .map_err(|error| SessionError::Protocol(error.to_string()))?;
    let (mut reader, mut writer) =
        crate::frame_server(server.into_inner().expect("connected server"));
    let hello = tokio::time::timeout(crate::HELLO_TIMEOUT, reader.recv())
        .await
        .map_err(|_| SessionError::Protocol("worker hello timeout".into()))?
        .map_err(|error| SessionError::Protocol(error.to_string()))?;
    let zup_protocol::Message::WorkerHello(hello) = hello.message else {
        return Err(SessionError::Protocol("expected WorkerHello".into()));
    };
    if hello.protocol_version != zup_protocol::PROTOCOL_VERSION
        || hello.session_id != session_id
        || hello.target != request.plan.plan.key.target
        || hello.worker_pid != worker.pid()
        || !hello.capabilities.prerequisite_bootstrap_v1
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
                target: request.plan.plan.key.target.clone(),
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
                target: request.plan.plan.key.target.clone(),
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
            () = cancel.cancelled(), if !cancel_sent => {
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
                        if cancel_sent && failed.kind == zup_protocol::failure::CANCELLED =>
                    {
                        return Err(SessionError::Cancelled);
                    }
                    zup_protocol::Message::Failed(failed) => {
                        return Err(match failed.kind.as_str() {
                            zup_protocol::failure::INSTALLATION_BUSY => {
                                SessionError::InstallationBusy
                            }
                            zup_protocol::failure::AUTHENTICATION => {
                                SessionError::Protocol(failed.message)
                            }
                            _ => SessionError::Prerequisite(failed.message),
                        });
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

impl WindowsRuntimeBackend {
    async fn execute_inner(
        &self,
        request: RuntimeRequest,
        control: RuntimeControl,
    ) -> Result<InstallOutcome, SessionError> {
        let cancellation = control.cancellation;
        let events = control.events;
        let policy = control.policy;
        let mut cleanup = OverlayCleanup::from_backend(self, &request);
        let recovery = request.recovery_id.is_some();
        let bootstrap_for_cleanup = request.bootstrap.clone();
        let result = self
            .execute_inner_with_control(request, cancellation, events, policy)
            .await;
        if matches!(result, Ok(InstallOutcome::Committed))
            && let Some(bootstrap) = bootstrap_for_cleanup
        {
            let _ = FilesystemBootstrapStateStore::with_file_system(
                &bootstrap.state_root,
                crate::windows_bootstrap_file_system(),
            )
            .remove(bootstrap.plan.id);
            let _ = std::fs::remove_dir_all(&bootstrap.quarantine_root);
        }
        match &result {
            Ok(InstallOutcome::RecoveryRequired) => cleanup.retain(),
            Ok(InstallOutcome::Failed(message))
                if self.retain_on_blocked
                    && message.starts_with(BLOCKED_BY_RUNNING_APPLICATIONS) =>
            {
                cleanup.retain()
            }
            Ok(_) => {}
            Err(_) if recovery => cleanup.retain(),
            Err(_) => {}
        }
        result
    }

    async fn execute_inner_with_control(
        &self,
        request: RuntimeRequest,
        cancel: CancellationHandle,
        events: broadcast::Sender<RuntimeEvent>,
        policy: ExecutionPolicy,
    ) -> Result<InstallOutcome, SessionError> {
        validate_runtime_request(&request, self)?;
        if let Some(bootstrap) = request.bootstrap.clone() {
            match run_bootstrap_phase(&bootstrap, &cancel, &events, policy).await? {
                BootstrapOutcome::Ready => {}
                BootstrapOutcome::RecoveryRequired => {
                    return Err(SessionError::RecoveryRequired);
                }
                BootstrapOutcome::RebootRequired {
                    exit_code,
                    prerequisite_id,
                } => {
                    return Ok(InstallOutcome::RebootRequired {
                        prerequisite_id: prerequisite_id.to_string(),
                        exit_code,
                    });
                }
            }
        }

        // Elevation is a Windows fact. Ask the transaction which operations
        // need host-wide authority, not which scope the application uses.
        let needs_authorization = request.transaction_plan.requires_authorization();
        let already_authorized = if needs_authorization {
            Some(
                crate::is_process_elevated()
                    .map_err(|error| SessionError::Protocol(error.to_string()))?,
            )
        } else {
            None
        };
        if needs_authorization
            && already_authorized == Some(false)
            && !policy.allows_authorization()
        {
            return Err(SessionError::AuthorizationRequired);
        }

        let mutating_paths = crate::plan_mutating_paths(&request.transaction_plan, &request.target);
        let blocker_paths = mutating_paths
            .iter()
            .map(|path| path.as_path())
            .collect::<Vec<_>>();
        let _ = events.send(RuntimeEvent::PreflightStarted);
        let blocked = crate::preflight(&blocker_paths)
            .map_err(|error| SessionError::Transaction(format!("resource preflight: {error}")))?;
        if let Some(detail) = crate::blocked_reason(&blocked) {
            let _ = events.send(RuntimeEvent::ResourceBlocked {
                pids: match &blocked {
                    crate::FilePreflight::Blocked { processes, .. } => {
                        processes.iter().map(|process| process.pid).collect()
                    }
                    crate::FilePreflight::Ready => Vec::new(),
                },
                detail: detail.clone(),
            });
            return Ok(InstallOutcome::Failed(
                "blocked by running applications".into(),
            ));
        }

        if needs_authorization && already_authorized == Some(false) {
            return run_elevated_worker(
                request,
                &self.payload_root,
                self.payload_overlay_root.clone(),
                cancel,
                events,
            )
            .await;
        }

        run_local_install_control(request, self, cancel, events).await
    }
}

struct SharedPayload(RuntimePayloadSource);

impl PayloadSource for SharedPayload {
    fn open(
        &self,
        path: &zup_core::RelativePath,
        expected_sha256: &zup_core::Sha256Digest,
        expected_size: u64,
    ) -> Result<zup_bundle::PayloadReader, zup_bundle::PayloadError> {
        self.0.open(path, expected_sha256, expected_size)
    }
}

async fn run_local_install_control(
    request: RuntimeRequest,
    backend: &WindowsRuntimeBackend,
    cancel: CancellationHandle,
    events: broadcast::Sender<RuntimeEvent>,
) -> Result<InstallOutcome, SessionError> {
    run_local_install_control_inner(request, backend, cancel, events).await
}

async fn run_local_install_control_inner(
    request: RuntimeRequest,
    backend: &WindowsRuntimeBackend,
    cancel: CancellationHandle,
    events: broadcast::Sender<RuntimeEvent>,
) -> Result<InstallOutcome, SessionError> {
    if request.transaction_plan.requires_authorization()
        && !crate::is_process_elevated()
            .map_err(|error| SessionError::Protocol(error.to_string()))?
    {
        return Err(SessionError::Protocol(
            "machine mutation requires authorization".into(),
        ));
    }
    let _ = events.send(RuntimeEvent::StateChanged {
        state: RuntimeState::Preparing,
    });
    validate_runtime_request(&request, backend)?;
    let _ = events.send(RuntimeEvent::StateChanged {
        state: RuntimeState::Executing,
    });
    let total_work = request.transaction_plan.total_work();
    let _ = events.send(RuntimeEvent::Progress {
        completed: 0,
        total: total_work,
        action: "Preparing…".into(),
    });

    let outcome = tokio::task::spawn_blocking({
        let cancel = cancel.probe();
        let events = events.clone();
        let payload = backend.payload.clone();
        move || execute_local_blocking_with_events(request, payload, cancel, Some(events))
    })
    .await
    .map_err(|error| SessionError::WorkerCrashed(error.to_string()))?;

    if outcome == InstallOutcome::Committed {
        let _ = events.send(RuntimeEvent::Progress {
            completed: total_work,
            total: total_work,
            action: "Finishing…".into(),
        });
    }
    Ok(outcome)
}

/// Elevated worker route (authenticated UAC + named pipe + one-shot worker).
async fn run_elevated_worker(
    request: RuntimeRequest,
    payload_root: &Path,
    payload_overlay_root: Option<PathBuf>,
    cancel: CancellationHandle,
    events: broadcast::Sender<RuntimeEvent>,
) -> Result<InstallOutcome, SessionError> {
    let payload_overlay_base_root = payload_overlay_root
        .as_ref()
        .and_then(|_| crate::payload_overlay_base_root(&request.state_root, request.scope).ok());
    let executable =
        crate::current_exe().map_err(|error| SessionError::WorkerLaunch(error.to_string()))?;
    let bundle = crate::EmbeddedBundle::open(&executable)
        .map_err(|error| SessionError::PlanInvalid(format!("worker bundle: {error}")))?;
    crate::validate_embedded_bundle_target(&bundle, &request.target)
        .map_err(|error| SessionError::PlanInvalid(error.to_string()))?;
    let session_id = zup_protocol::SessionId::new_v7();

    let _ = events.send(RuntimeEvent::WaitingForAuthorization);

    // Create the secured pipe BEFORE launching the worker.
    let pipe = crate::pipe_name(&session_id.to_string());
    let mut server =
        crate::PipeServer::create(&pipe).map_err(|e| SessionError::Protocol(e.to_string()))?;

    let plan = if let Some(id) = request.recovery_id {
        let record = FilesystemTransactionStore::new(&request.state_root)
            .load(&id)
            .map_err(|e| SessionError::Transaction(e.to_string()))?;
        if record.app_id != request.app_id
            || record.scope != request.scope
            || record.app_version != request.app_version
            || record.target != request.target
            || record.plan != request.transaction_plan
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
            payload_overlay_root.as_deref(),
            payload_overlay_base_root.as_deref(),
            true,
        )
        .map_err(SessionError::PlanInvalid)?;
        if identity.has_files() {
            verify_payload_overlay(
                payload_overlay_base_root
                    .as_deref()
                    .expect("validated overlay base"),
                &identity,
                payload_overlay_root
                    .as_deref()
                    .expect("validated overlay path"),
            )
            .map_err(|error| SessionError::PlanInvalid(error.to_string()))?;
        }
        record.plan
    } else {
        request.transaction_plan.clone()
    };
    let has_lifecycle = plan.uninstall
        || !plan.retired_keys.is_empty()
        || plan.nodes.iter().any(|node| {
            matches!(
                node.kind,
                zup_transaction::NodeKind::FileMutation {
                    delta: FileDelta::RestoreOwned | FileDelta::RepairOwned,
                    ..
                }
            )
        });
    let has_backend = plan.nodes.iter().any(|node| {
        matches!(
            node.kind,
            zup_transaction::NodeKind::BackendOperation { .. }
                | zup_transaction::NodeKind::BackendRemoval { .. }
        )
    });
    if plan.target != request.target {
        return Err(SessionError::PlanInvalid(
            "transaction plan target does not match runtime request".into(),
        ));
    }
    let plan_json =
        serde_json::to_string(&plan).map_err(|e| SessionError::PlanInvalid(e.to_string()))?;
    let total_work = transaction_work_total(&plan);
    let _ = events.send(RuntimeEvent::Progress {
        completed: 0,
        total: total_work,
        action: "Preparing…".into(),
    });
    let plan_hash = crate::plan_hash_hex(&plan_json);

    let bootstrap = crate::WorkerBootstrap {
        protocol_version: zup_protocol::PROTOCOL_VERSION,
        session_id,
        pipe_name: pipe.clone(),
        expected_parent_pid: std::process::id(),
        expected_parent_sid: crate::UserSid::current()
            .map_err(|e| SessionError::Protocol(e.to_string()))?
            .display()
            .to_owned(),
        target: request.target.clone(),
        expected_plan_hash: plan_hash.clone(),
    };
    let bootstrap_arg = crate::format_bootstrap(&bootstrap);
    let params = format!("__worker {}", crate::quote_arg(&bootstrap_arg));
    let worker = crate::launch_elevated_worker(&executable, &params).map_err(|e| match e {
        crate::TransportError::ElevationCancelled => SessionError::AuthorizationCancelled,
        other => SessionError::WorkerLaunch(other.to_string()),
    })?;
    server
        .connect_worker(worker.pid())
        .await
        .map_err(|error| SessionError::Protocol(error.to_string()))?;

    let (mut reader, mut writer) =
        crate::frame_server(server.into_inner().expect("connected server"));
    let hello = tokio::time::timeout(crate::HELLO_TIMEOUT, reader.recv())
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
        || hello.target != request.target
        || hello.worker_pid != worker.pid()
        || !hello.capabilities.file_transactions_v1
        || (has_backend && !hello.capabilities.backend_operations_v1)
        || (has_lifecycle && !hello.capabilities.lifecycle_v1)
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
                target: request.target.clone(),
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
            message: zup_protocol::Message::ExecuteTransaction(Box::new(
                zup_protocol::ExecuteTransaction {
                    target: request.target.clone(),
                    plan_json,
                    plan_hash,
                    app_id: request.app_id.to_string(),
                    app_version: request.app_version.to_string(),
                    scope: match request.scope {
                        SelectedScope::User => "user",
                        SelectedScope::Machine => "machine",
                    }
                    .into(),
                    payload_root: payload_root.display().to_string(),
                    payload_overlay_root: payload_overlay_root
                        .as_ref()
                        .map(|path| path.display().to_string()),
                    payload_overlay_base_root: payload_overlay_base_root
                        .as_ref()
                        .map(|path| path.display().to_string()),
                    state_root: request.state_root.display().to_string(),
                    work_root: request.work_root.display().to_string(),
                    recovery_id: request.recovery_id.map(|id| id.as_uuid()),
                    release: request.release.clone(),
                },
            )),
        })
        .await
        .map_err(|e| SessionError::Protocol(e.to_string()))?;

    let mut cancel_sent = false;
    loop {
        tokio::select! {
            () = cancel.cancelled(), if !cancel_sent => {
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
                    return Ok(outcome);
                }
                zup_protocol::Message::Failed(failed) => {
                    // The kind, not the message, is what the caller acts on. A
                    // busy installation becomes a typed state the frontends can
                    // offer to retry, rather than a sentence somebody has to read
                    // and classify. An unrecognized kind is a protocol failure,
                    // never a default: a newer worker talking to an older parent
                    // has to be refused rather than reported as a broken install.
                    return Err(match failed.kind.as_str() {
                        zup_protocol::failure::INSTALLATION_BUSY => {
                            SessionError::InstallationBusy
                        }
                        zup_protocol::failure::AUTHENTICATION => {
                            SessionError::Protocol(failed.message)
                        }
                        kind if zup_protocol::FAILURE_KINDS.contains(&kind) => {
                            SessionError::Transaction(failed.message)
                        }
                        other => {
                            return Err(SessionError::Protocol(format!(
                                "the worker reported an unknown failure kind `{other}`"
                            )));
                        }
                    });
                }
                _ => return Err(SessionError::Protocol("unexpected worker message".into())),
                }
            }
        }
    }
}

fn validate_runtime_request(
    request: &RuntimeRequest,
    backend: &WindowsRuntimeBackend,
) -> Result<(), SessionError> {
    let Some(id) = request.recovery_id else {
        return validate_request(request, backend);
    };
    let record = FilesystemTransactionStore::new(&request.state_root)
        .load(&id)
        .map_err(|error| SessionError::Transaction(error.to_string()))?;
    if record.app_id != request.app_id
        || record.app_version != request.app_version
        || record.scope != request.scope
        || record.target != request.target
        || record.plan != request.transaction_plan
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
    let base = backend
        .payload_overlay_root
        .as_ref()
        .and_then(|_| crate::payload_overlay_base_root(&request.state_root, request.scope).ok());
    validate_overlay_identity(
        &request.state_root,
        request.scope,
        &identity,
        backend.payload_overlay_root.as_deref(),
        base.as_deref(),
        true,
    )
    .map_err(SessionError::PlanInvalid)?;
    if identity.has_files() {
        verify_payload_overlay(
            base.as_deref().expect("validated overlay base"),
            &identity,
            backend
                .payload_overlay_root
                .as_deref()
                .expect("validated overlay path"),
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
        crate::payload_overlay_base_root(state_root, scope).map_err(|error| error.to_string())?;
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

fn validate_request(
    request: &RuntimeRequest,
    backend: &WindowsRuntimeBackend,
) -> Result<(), SessionError> {
    request
        .transaction_plan
        .validate()
        .map_err(|error| SessionError::PlanInvalid(error.to_string()))?;
    if request.target != request.transaction_plan.target {
        return Err(SessionError::PlanInvalid(
            "request target does not match the finalized plan".into(),
        ));
    }
    let identity = PayloadOverlayIdentity::from_transaction(
        request.app_id.clone(),
        request.app_version.clone(),
        request.scope,
        &request.transaction_plan,
    )
    .map_err(|error| SessionError::PlanInvalid(error.to_string()))?;
    let base = backend
        .payload_overlay_root
        .as_ref()
        .and_then(|_| crate::payload_overlay_base_root(&request.state_root, request.scope).ok());
    validate_overlay_identity(
        &request.state_root,
        request.scope,
        &identity,
        backend.payload_overlay_root.as_deref(),
        base.as_deref(),
        request.recovery_id.is_some(),
    )
    .map_err(SessionError::PlanInvalid)?;
    Ok(())
}

/// Blocking local execution with the production `WindowsFileExecutor`.
fn execute_local_blocking_with_events(
    request: RuntimeRequest,
    payload: RuntimePayloadSource,
    cancel: impl CancellationProbe + Send + 'static,
    events: Option<broadcast::Sender<RuntimeEvent>>,
) -> InstallOutcome {
    let payload = SharedPayload(payload);
    execute_local_blocking_with_events_inner(request, payload, cancel, events)
}

fn execute_local_blocking_with_events_inner(
    request: RuntimeRequest,
    payload: SharedPayload,
    cancel: impl CancellationProbe + Send + 'static,
    events: Option<broadcast::Sender<RuntimeEvent>>,
) -> InstallOutcome {
    let lock_key = InstallationLock::lock_key(
        request.app_id.as_str(),
        match request.scope {
            SelectedScope::User => "user",
            SelectedScope::Machine => "machine",
        },
    );
    let _lock = match InstallationLock::try_acquire(&request.state_root, &lock_key) {
        Ok(Some(lock)) => lock,
        // A typed state, not a failure string: a caller that can see this is one
        // more of the same operation can wait for the other to finish, and the
        // headless exit code says so.
        Ok(None) => {
            return InstallOutcome::Busy {
                operation: "another maintenance operation",
            };
        }
        Err(e) => return InstallOutcome::Failed(e.to_string()),
    };

    let store = FilesystemTransactionStore::new(&request.state_root);
    let coordinator = TransactionCoordinator::new(store);
    if let Some(id) = request.recovery_id {
        let record = match FilesystemTransactionStore::new(&request.state_root).load(&id) {
            Ok(record)
                if record.app_id == request.app_id
                    && record.scope == request.scope
                    && record.app_version == request.app_version
                    && record.target == request.target
                    && record.plan == request.transaction_plan =>
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
                record.target.executable_suffix(),
                Box::new(NullProgress),
            ),
            cancel,
            events,
            completed_work: 0,
            total_work: transaction_work_total(&record.plan),
            blocked_paths: crate::plan_mutating_paths(&record.plan, &request.target),
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
                        crate::notify_committed_path_change(&record);
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
    let plan = request.transaction_plan.clone();
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
            record.target.executable_suffix(),
            Box::new(NullProgress),
        ),
        cancel,
        events,
        completed_work: 0,
        total_work: transaction_work_total(&record.plan),
        blocked_paths: crate::plan_mutating_paths(&record.plan, &request.target),
    };

    // Register the immutable payload identity under each transaction operation.
    note_plan_files(&mut executor.inner, &record.plan);

    match coordinator.execute(record, &mut executor) {
        Ok((record, TransactionOutcome::Committed)) => {
            match InstallLedgerStore::new(&request.state_root).publish_committed(
                &record,
                request.scope,
                record_release(&request),
            ) {
                Ok(_) => {
                    crate::notify_committed_path_change(&record);
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

fn transaction_work_total(plan: &TransactionPlan) -> u64 {
    plan.total_work()
}

fn operation_action(operation: &TransactionNode) -> String {
    match &operation.kind {
        zup_transaction::NodeKind::StageFile { .. }
        | zup_transaction::NodeKind::FileMutation { .. } => "Installing files…".into(),
        zup_transaction::NodeKind::BackendOperation { .. }
        | zup_transaction::NodeKind::BackendRemoval { .. } => {
            "Updating application settings…".into()
        }
        zup_transaction::NodeKind::FileRemoval { .. } => "Removing files…".into(),
        zup_transaction::NodeKind::Barrier => "Finishing…".into(),
    }
}

/// Production `OperationExecutor` wrapping `WindowsFileExecutor`.
struct ProductionExecutor<P: zup_bundle::PayloadSource, C: CancellationProbe> {
    inner: WindowsFileExecutor<P>,
    cancel: C,
    events: Option<broadcast::Sender<RuntimeEvent>>,
    completed_work: u64,
    total_work: u64,
    /// Files this plan may mutate, for the Restart Manager preflight a
    /// barrier repeats.
    blocked_paths: Vec<PathBuf>,
}

impl<P: zup_bundle::PayloadSource, C: CancellationProbe> OperationExecutor
    for ProductionExecutor<P, C>
{
    type Error = String;

    fn prepare(&mut self, operation: &TransactionNode) -> Result<(), Self::Error> {
        if self.cancel.is_cancelled() {
            return Err("cancelled".into());
        }
        match &operation.kind {
            // The plan orders this barrier immediately before commit intent, so
            // this is the last chance to see a blocker before the transaction
            // mutates anything.
            zup_transaction::NodeKind::Barrier => {
                let blockers = self
                    .blocked_paths
                    .iter()
                    .map(|path| path.as_path())
                    .collect::<Vec<_>>();
                let blocked = crate::preflight(&blockers).map_err(|error| error.to_string())?;
                let Some(detail) = crate::blocked_reason(&blocked) else {
                    return Ok(());
                };
                if let Some(events) = &self.events {
                    let _ = events.send(RuntimeEvent::ResourceBlocked {
                        pids: match &blocked {
                            crate::FilePreflight::Blocked { processes, .. } => {
                                processes.iter().map(|process| process.pid).collect()
                            }
                            crate::FilePreflight::Ready => Vec::new(),
                        },
                        detail: detail.clone(),
                    });
                }
                Err(format!(
                    "{BLOCKED_BY_RUNNING_APPLICATIONS}: {}",
                    detail.replace('\n', ", ")
                ))
            }
            // Barriers are the only nodes the coordinator preflights; a file
            // node re-checks its own precondition in `apply`.
            _ => Ok(()),
        }
    }

    fn apply(&mut self, operation: &TransactionNode) -> Result<OperationReceipt, Self::Error> {
        if self.cancel.is_cancelled() {
            return Err("cancelled".into());
        }
        if let Some(events) = &self.events {
            let _ = events.send(RuntimeEvent::OperationStarted {
                id: operation.id.to_string(),
            });
        }
        let receipt = match &operation.kind {
            zup_transaction::NodeKind::Barrier => Ok(OperationReceipt::Control),
            zup_transaction::NodeKind::BackendOperation { .. } => {
                crate::apply_managed(operation).map_err(|e| e.to_string())
            }
            zup_transaction::NodeKind::BackendRemoval { .. } => {
                crate::apply_owned_removal(operation).map_err(|e| e.to_string())
            }
            zup_transaction::NodeKind::FileRemoval { .. } => self
                .inner
                .apply_owned_file_removal(operation)
                .map_err(|e| e.to_string()),
            zup_transaction::NodeKind::StageFile { .. }
            | zup_transaction::NodeKind::FileMutation { .. } => {
                let (source_relative, dest) = extract_file_identity(operation)?;
                crate::apply_node(&mut self.inner, operation, &source_relative, &dest)
                    .map(crate::transaction_receipt)
                    .map_err(|e| e.to_string())
            }
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

    fn verify(
        &mut self,
        operation: &TransactionNode,
        receipt: &OperationReceipt,
    ) -> Result<(), Self::Error> {
        if self.cancel.is_cancelled() {
            return Err("cancelled".into());
        }
        match &operation.kind {
            zup_transaction::NodeKind::BackendOperation { .. }
            | zup_transaction::NodeKind::BackendRemoval { .. } => {
                crate::integration::verify_managed(receipt).map_err(|error| error.to_string())
            }
            zup_transaction::NodeKind::Barrier => Ok(()),
            _ => crate::verify_installed_file(receipt).map_err(|error| error.to_string()),
        }
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
            _ => crate::rollback_managed(receipt).map_err(|e| e.to_string()),
        }
    }

    fn reconcile(
        &mut self,
        operation: &TransactionNode,
        _receipt: Option<&OperationReceipt>,
    ) -> Result<ReconcileResult, Self::Error> {
        match &operation.kind {
            zup_transaction::NodeKind::BackendOperation { .. } => {
                return crate::reconcile_managed(operation).map_err(|e| e.to_string());
            }
            zup_transaction::NodeKind::BackendRemoval { .. } => {
                return crate::reconcile_owned_removal(operation).map_err(|e| e.to_string());
            }
            zup_transaction::NodeKind::FileRemoval { .. } => {
                return self
                    .inner
                    .reconcile_owned_file_removal(operation)
                    .map_err(|e| e.to_string());
            }
            _ => {}
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

use zup_core::ResourceKey;
