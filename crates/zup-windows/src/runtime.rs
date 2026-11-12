use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use zup_plan::DeploymentNode;

use tokio::sync::broadcast;
use zup_bootstrap::{
    BootstrapOutcome, BootstrapState, BootstrapStateStore, FilesystemBootstrapStateStore,
    Quarantine, execute_plan_with_persist,
};
use zup_bundle::{AcquiredPayloadSource, PayloadSource};
use zup_core::SelectedScope;
use zup_plan::FileOperationKind;
use zup_runtime::{
    BootstrapRequest, CancellationHandle, ExecutionPolicy, InstallOutcome, RuntimeBackend,
    RuntimeControl, RuntimeEvent, RuntimeFuture, RuntimeOperation, RuntimeRequest, RuntimeState,
    SessionError,
};
use zup_transaction::{
    CancellationProbe, FilesystemTransactionStore, OperationExecutor, OperationReceipt,
    ReconcileResult, ResourceLock, TransactionCoordinator, TransactionOutcome, TransactionStore,
};

type SharedPayloadSource = Arc<dyn PayloadSource + Send + Sync>;

use crate::{
    AutoPayloadSource, BundleError, InstallationRecordStore, PayloadOverlayIdentity,
    WindowsFileExecutor, cleanup_payload_overlay, payload_overlay_base_root,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayPolicy {
    Cleanup,
    RetainOnBlocked,
}

const BLOCKED_BY_RUNNING_APPLICATIONS: &str = "blocked by running applications";

#[derive(Clone)]
pub struct WindowsRuntimeBackend {
    payload: SharedPayloadSource,
    payload_root: PathBuf,
    payload_overlay_root: Option<PathBuf>,
    retain_on_blocked: bool,
    content: Option<AcquiredContent>,
}

#[derive(Clone)]
struct AcquiredContent {
    handoff: zup_acquire::RuntimeHandoff,
    image: Arc<crate::AcquiredRuntime>,
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
            content: None,
        })
    }

    pub fn with_generated_files(
        mut self,
        prepared: &zup_runtime::PreparedOperation,
        state_root: &Path,
    ) -> Result<Self, String> {
        let Some(install) = prepared.install_plan() else {
            return Ok(self);
        };
        let scope = install.scope;
        let generated_files = prepared.generated_files();
        let transaction_plan = &prepared.transaction_plan;
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
            let transaction_sources: BTreeSet<_> = transaction_identity
                .files()
                .map(|file| &file.source_relative)
                .collect();
            let overlay_files: Vec<_> = generated_files
                .iter()
                .filter(|file| transaction_sources.contains(&file.source_relative))
                .cloned()
                .collect();
            let base = crate::payload_overlay_base_root(state_root, scope)
                .map_err(|error| error.to_string())?;
            match crate::materialize_payload_overlay(&base, &transaction_identity, &overlay_files) {
                Ok(overlay) => overlay,
                Err(error) => {
                    if let Some(overlay) = transaction_identity.path_under(&base) {
                        let _ = cleanup_payload_overlay(&base, Some(&overlay));
                    }
                    return Err(error.to_string());
                }
            }
        };
        if let Some(overlay) = overlay {
            self.payload = Arc::new(zup_bundle::OverlayPayloadSource::new(
                SharedPayload(self.payload),
                Some(overlay.clone()),
            ));
            self.payload_overlay_root = Some(overlay);
        }
        Ok(self)
    }

    pub fn for_recovery() -> Self {
        Self {
            payload: Arc::new(crate::recovery::RetainedSource::empty()),
            payload_root: PathBuf::new(),
            payload_overlay_root: None,
            retain_on_blocked: false,
            content: None,
        }
    }

    pub fn from_acquired(
        payload: std::sync::Arc<AcquiredPayloadSource>,
        payload_root: impl Into<PathBuf>,
        content: zup_acquire::RuntimeHandoff,
        image: Arc<crate::AcquiredRuntime>,
    ) -> Self {
        let payload_root = payload_root.into();
        Self {
            payload: Arc::new(image.payload_source(payload)),
            payload_root,
            payload_overlay_root: None,
            retain_on_blocked: false,
            content: Some(AcquiredContent {
                handoff: content,
                image,
            }),
        }
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
            content: None,
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
    fn execute<'a>(
        &'a self,
        request: RuntimeRequest,
        control: RuntimeControl,
    ) -> RuntimeFuture<'a, Result<InstallOutcome, SessionError>> {
        Box::pin(async move { self.execute_inner(request, control).await })
    }
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
    let bootstrap_lock_key = ResourceLock::lock_key(
        request.plan.plan.key.app_id.as_str(),
        &request.plan.plan.key.scope.to_string(),
    );
    let _bootstrap_lock = ResourceLock::try_acquire(&request.quarantine_root, &bootstrap_lock_key)
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
    let session_id = zup_core::SessionId::new_v7();
    let pipe = crate::pipe_name(&session_id.to_string());
    let mut server = crate::PipeServer::create(&pipe)
        .map_err(|error| SessionError::Protocol(error.to_string()))?;
    let bootstrap_json = serde_json::to_string(&request.plan)
        .map_err(|error| SessionError::Prerequisite(error.to_string()))?;
    let plan_hash = crate::request_digest_hex(&bootstrap_json);
    let bootstrap = crate::WorkerBootstrap {
        protocol_version: zup_protocol::PROTOCOL_VERSION,
        session_id,
        pipe_name: pipe.clone(),
        expected_parent_pid: std::process::id(),
        expected_parent_sid: crate::UserSid::current()
            .map_err(|error| SessionError::Protocol(error.to_string()))?
            .display()
            .to_owned(),
        target: request.plan.plan.key.target,
        expected_request_digest: plan_hash.clone(),
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
    let hello = tokio::time::timeout(crate::HANDSHAKE_TIMEOUT, reader.recv())
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
                target: request.plan.plan.key.target,
                transaction_id: request.plan.id.as_uuid(),
                expected_request_digest: plan_hash.clone(),
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
                target: request.plan.plan.key.target,
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
                        return Ok(match completed.outcome {
                            zup_protocol::WorkerOutcome::Committed => BootstrapOutcome::Ready,
                            zup_protocol::WorkerOutcome::RebootRequired => {
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
                            zup_protocol::WorkerOutcome::RecoveryRequired => BootstrapOutcome::RecoveryRequired,
                            _ => {
                                return Err(SessionError::Protocol(
                                    "unknown bootstrap completion outcome".into(),
                                ));
                            }
                        });
                    }
                    zup_protocol::Message::Failed(failed)
                        if cancel_sent && failed.kind == zup_protocol::WorkerFailure::Cancelled =>
                    {
                        return Err(SessionError::Cancelled);
                    }
                    zup_protocol::Message::Failed(failed) => {
                        return Err(match failed.kind {
                            zup_protocol::WorkerFailure::InstallationBusy => {
                                SessionError::InstallationBusy
                            }
                            zup_protocol::WorkerFailure::Authentication => {
                                SessionError::Protocol(failed.message)
                            }
                            zup_protocol::WorkerFailure::Protocol => SessionError::Protocol(failed.message),
                            zup_protocol::WorkerFailure::Cancelled => SessionError::Cancelled,
                            zup_protocol::WorkerFailure::RecoveryRequired { transaction } => SessionError::RecoveryRequired { transaction: transaction.map(|id| id.0), detail: failed.message },
                            zup_protocol::WorkerFailure::AuthorizationRequired => SessionError::AuthorizationRequired,
                            zup_protocol::WorkerFailure::StalePlan | zup_protocol::WorkerFailure::Policy => SessionError::PlanInvalid(failed.message),
                            zup_protocol::WorkerFailure::Transaction => SessionError::Prerequisite(failed.message),
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
        let recovery = request.operation.recovery_transaction().is_some();
        let bootstrap_for_cleanup = request
            .operation
            .deployment()
            .and_then(|deployment| deployment.bootstrap.clone());
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
            Ok(InstallOutcome::RecoveryRequired { .. })
            | Ok(InstallOutcome::RecoveryAssessed { .. })
            | Ok(InstallOutcome::CommittedPendingReconciliation { .. }) => cleanup.retain(),
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
        if let Some(bootstrap) = request
            .operation
            .deployment()
            .and_then(|deployment| deployment.bootstrap.clone())
        {
            match run_bootstrap_phase(&bootstrap, &cancel, &events, policy).await? {
                BootstrapOutcome::Ready => {}
                BootstrapOutcome::RecoveryRequired => {
                    return Err(SessionError::RecoveryRequired { transaction: None, detail: "Prerequisite recovery is required before deployment can continue; rerun this installer to reconcile its bootstrap state".into() });
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

        let needs_authorization = request.scope == SelectedScope::Machine
            || request
                .operation
                .deployment()
                .map(|deployment| &deployment.plan)
                .is_some_and(zup_plan::DeploymentPlan::requires_authorization);
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

        if request.scope == SelectedScope::Machine
            || needs_authorization && already_authorized == Some(false)
        {
            return run_elevated_worker(
                request,
                &self.payload_root,
                self.payload_overlay_root.clone(),
                self.content
                    .as_ref()
                    .map(|content| zup_protocol::AcquiredSource {
                        handoff: content.handoff.clone(),
                        image: crate::plain_path_text(content.image.path()),
                    }),
                cancel,
                events,
            )
            .await;
        }

        run_local_install_control(request, self, cancel, events).await
    }
}

pub(crate) struct SharedPayload(pub(crate) SharedPayloadSource);

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
    if request
        .operation
        .deployment()
        .map(|deployment| &deployment.plan)
        .is_some_and(zup_plan::DeploymentPlan::requires_authorization)
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
    let total_work = request
        .operation
        .deployment()
        .map(|deployment| &deployment.plan)
        .map_or(0, zup_plan::DeploymentPlan::total_work);
    let _ = events.send(RuntimeEvent::Progress {
        phase: zup_core::OperationPhase::Prepare,
        completed: 0,
        total: total_work,
        action: "Preparing…".into(),
    });

    let outcome = tokio::task::spawn_blocking({
        let cancel = cancel.probe();
        let events = events.clone();
        let payload = backend.payload.clone();
        move || {
            execute_local_blocking_with_events(
                request,
                SharedPayload(payload),
                cancel,
                Some(events),
            )
        }
    })
    .await
    .map_err(|error| SessionError::WorkerCrashed(error.to_string()))?;

    if outcome == InstallOutcome::Committed {
        let _ = events.send(RuntimeEvent::Progress {
            phase: zup_core::OperationPhase::Finish,
            completed: total_work,
            total: total_work,
            action: "Finishing…".into(),
        });
    }
    Ok(outcome)
}

async fn run_elevated_worker(
    request: RuntimeRequest,
    payload_root: &Path,
    payload_overlay_root: Option<PathBuf>,
    content: Option<zup_protocol::AcquiredSource>,
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
    let session_id = zup_core::SessionId::new_v7();

    let _ = events.send(RuntimeEvent::WaitingForAuthorization);

    let pipe = crate::pipe_name(&session_id.to_string());
    let mut server =
        crate::PipeServer::create(&pipe).map_err(|e| SessionError::Protocol(e.to_string()))?;

    let recovering = request.operation.recovery_transaction().is_some();
    let (payload_root, payload_overlay_root, payload_overlay_base_root, content) = if recovering {
        (Path::new(""), None, None, None)
    } else {
        (
            payload_root,
            payload_overlay_root,
            payload_overlay_base_root,
            content,
        )
    };
    let plan = request
        .operation
        .deployment()
        .map(|deployment| deployment.plan.clone());
    let has_lifecycle = recovering
        || plan.as_ref().is_some_and(|plan| {
            plan.transition.is_remove()
                || !plan.retired_keys.is_empty()
                || plan.nodes.iter().any(|node| {
                    matches!(
                        node.kind,
                        zup_plan::NodeKind::FileMutation {
                            delta: FileOperationKind::RestoreOwned | FileOperationKind::RepairOwned,
                            ..
                        }
                    )
                })
        });
    let has_backend = recovering
        || plan.as_ref().is_some_and(|plan| {
            plan.nodes.iter().any(|node| {
                matches!(
                    node.kind,
                    zup_plan::NodeKind::NativeService { .. }
                        | zup_plan::NodeKind::BackendOperation { .. }
                        | zup_plan::NodeKind::BackendRemoval { .. }
                )
            })
        });
    if plan
        .as_ref()
        .is_some_and(|plan| plan.target != request.target)
    {
        return Err(SessionError::PlanInvalid(
            "transaction plan target does not match runtime request".into(),
        ));
    }
    let operation = match &request.operation {
        RuntimeOperation::Deployment(deployment) => {
            zup_protocol::PreparedTransactionRequest::Deployment {
                plan: Box::new(deployment.plan.clone()),
                intent: Box::new(deployment.intent.clone()),
            }
        }
        RuntimeOperation::Recovery {
            transaction,
            request: recovery_request,
        } => zup_protocol::PreparedTransactionRequest::Recovery {
            transaction: *transaction,
            request: recovery_request.clone(),
        },
    };
    let prepare = zup_protocol::PrepareTransaction {
        content,
        operation,
        target: request.target,
        app_id: request.app_id.to_string(),
        app_version: request.app_version.to_string(),
        scope: request.scope.to_string(),
        payload_root: payload_root.display().to_string(),
        payload_overlay_root: payload_overlay_root
            .as_ref()
            .map(|path| path.display().to_string()),
        payload_overlay_base_root: payload_overlay_base_root
            .as_ref()
            .map(|path| path.display().to_string()),
        state_root: request.state_root.display().to_string(),
        release: request
            .operation
            .deployment()
            .and_then(|deployment| deployment.release.clone()),
    };
    let total_work = plan
        .as_ref()
        .map_or(0, zup_plan::DeploymentPlan::total_work);
    let _ = events.send(RuntimeEvent::Progress {
        phase: zup_core::OperationPhase::Prepare,
        completed: 0,
        total: total_work,
        action: "Preparing…".into(),
    });
    let plan_hash = prepare
        .digest()
        .map_err(|error| SessionError::PlanInvalid(error.to_string()))?;

    let bootstrap = crate::WorkerBootstrap {
        protocol_version: zup_protocol::PROTOCOL_VERSION,
        session_id,
        pipe_name: pipe.clone(),
        expected_parent_pid: std::process::id(),
        expected_parent_sid: crate::UserSid::current()
            .map_err(|e| SessionError::Protocol(e.to_string()))?
            .display()
            .to_owned(),
        target: request.target,
        expected_request_digest: plan_hash.clone(),
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
    let hello = tokio::time::timeout(crate::HANDSHAKE_TIMEOUT, reader.recv())
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
                target: request.target,
                transaction_id: uuid::Uuid::now_v7(),
                expected_request_digest: plan_hash.clone(),
            }),
        })
        .await
        .map_err(|e| SessionError::Protocol(e.to_string()))?;
    writer
        .send(&zup_protocol::WireEnvelope {
            version: zup_protocol::PROTOCOL_VERSION,
            session_id,
            sequence: 2,
            message: zup_protocol::Message::PrepareTransaction(Box::new(prepare)),
        })
        .await
        .map_err(|e| SessionError::Protocol(e.to_string()))?;

    let mut cancel_sent = false;
    let mut execution_sent = false;
    let mut sequence = 3;
    loop {
        tokio::select! {
                   () = cancel.cancelled(), if !cancel_sent => {
                       writer.send(&zup_protocol::WireEnvelope {
                           version: zup_protocol::PROTOCOL_VERSION,
                           session_id,
                           sequence,
                           message: zup_protocol::Message::Cancel,
                       }).await.map_err(|e| SessionError::Protocol(e.to_string()))?;
                       cancel_sent = true;
                       sequence += 1;
                   }
                   message = reader.recv() => {
                       let message = message.map_err(|e| SessionError::Protocol(e.to_string()))?;
                       worker_sequence.accept(message.sequence)
                           .map_err(|e| SessionError::Protocol(e.to_string()))?;
                       match message.message {
                       zup_protocol::Message::Prepared(prepared) => {
                           if execution_sent || prepared.target != request.target
                               || prepared.plan.requirements.app.id != request.app_id
                               || prepared.plan.requirements.scope != request.scope
                               || prepared.plan.fingerprint().to_hex() != prepared.plan_digest
                               || (!recovering && prepared.plan.requirements.app.version != request.app_version)
                               || plan.as_ref().is_some_and(|expected| expected.requirements != prepared.plan.requirements
                                   || expected.install_directory != prepared.plan.install_directory || expected.transition.is_remove() != prepared.plan.transition.is_remove())
                           {
                               return Err(SessionError::Protocol("worker preparation differs from the authorized operation".into()));
                           }
                           prepared.plan.validate().map_err(|error| SessionError::PlanInvalid(error.to_string()))?;
                           let _ = events.send(RuntimeEvent::PlanPrepared { plan: prepared.plan });
                           if cancel_sent { continue; }
                           writer.send(&zup_protocol::WireEnvelope {
                               version: zup_protocol::PROTOCOL_VERSION, session_id, sequence,
                               message: zup_protocol::Message::Execute(zup_protocol::ExecuteOperation { plan_digest: prepared.plan_digest }),
                           }).await.map_err(|error| SessionError::Protocol(error.to_string()))?;
                           sequence += 1;
                           execution_sent = true;
                       }
                       zup_protocol::Message::Progress(progress) => {
                           if progress.kind == zup_protocol::ProgressKind::ResourceBlocked {
                               let _ = events.send(RuntimeEvent::ResourceBlocked { pids: progress.blocked_pids, detail: progress.detail });
                           } else if progress.kind == zup_protocol::ProgressKind::PreflightStarted {
                               let _ = events.send(RuntimeEvent::PreflightStarted);
                           } else if progress.kind == zup_protocol::ProgressKind::OperationProgress {
                               if let (Some(completed), Some(total)) = (progress.completed, progress.total) {
                                   let _ = events.send(RuntimeEvent::Progress {
        phase: progress.phase,
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
                           let outcome = match completed.outcome {
                               zup_protocol::WorkerOutcome::Committed => InstallOutcome::Committed,
                               zup_protocol::WorkerOutcome::RolledBack => InstallOutcome::RolledBack,
                               zup_protocol::WorkerOutcome::Cancelled => InstallOutcome::Cancelled,
                               zup_protocol::WorkerOutcome::RecoveryAssessed => InstallOutcome::RecoveryAssessed {
                                   assessment: Box::new(serde_json::from_str(completed.detail.as_deref().ok_or_else(|| SessionError::Protocol("worker assessment missing".into()))?).map_err(|error| SessionError::Protocol(format!("worker assessment: {error}")))?),
                               },
                               zup_protocol::WorkerOutcome::RecoveryRequired => InstallOutcome::RecoveryRequired {
                                   transaction: Some(completed.transaction_id),
                                   detail: completed.detail.unwrap_or_else(|| "The transaction requires recovery before another operation can proceed".into()),
                               },
                               zup_protocol::WorkerOutcome::CommittedPendingReconciliation => {
                                   InstallOutcome::CommittedPendingReconciliation {
                                       transaction: Some(completed.transaction_id),
                                       detail: completed.detail.unwrap_or_else(|| "The transaction committed; ownership reconciliation requires recovery".into()),
                                   }
                               }
                               other => return Err(SessionError::Protocol(format!("unexpected worker outcome {other:?}"))),
                           };
                           return Ok(outcome);
                       }
                       zup_protocol::Message::Failed(failed) => {

                           return Err(match failed.kind {
                               zup_protocol::WorkerFailure::InstallationBusy => {
                                   SessionError::InstallationBusy
                               }
                               zup_protocol::WorkerFailure::Authentication => {
                                   SessionError::Protocol(failed.message)
                               }
                               zup_protocol::WorkerFailure::Protocol => SessionError::Protocol(failed.message),
                               zup_protocol::WorkerFailure::Cancelled => SessionError::Cancelled,
                               zup_protocol::WorkerFailure::RecoveryRequired { transaction } => SessionError::RecoveryRequired { transaction: transaction.map(|id| id.0), detail: failed.message },
                               zup_protocol::WorkerFailure::AuthorizationRequired => SessionError::AuthorizationRequired,
                               zup_protocol::WorkerFailure::StalePlan | zup_protocol::WorkerFailure::Policy => SessionError::PlanInvalid(failed.message),
                               zup_protocol::WorkerFailure::Transaction => SessionError::Transaction(failed.message),
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
    let Some(id) = request.operation.recovery_transaction() else {
        return validate_request(request, backend);
    };
    if request.scope == SelectedScope::Machine {
        return Ok(());
    }
    let record = FilesystemTransactionStore::private_at(&request.state_root)
        .load(&id)
        .map_err(|error| SessionError::Transaction(error.to_string()))?;
    if record.app_id != request.app_id
        || record.scope != request.scope
        || record.target != request.target
    {
        return Err(SessionError::Protocol("recovery identity mismatch".into()));
    }

    Ok(())
}

fn validate_overlay_identity(
    state_root: &Path,
    scope: zup_core::SelectedScope,
    identity: &PayloadOverlayIdentity,
    actual: Option<&Path>,
    overlay_base_root: Option<&Path>,
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
    if overlay_base_root.is_some() || actual.is_some() {
        return Err("overlay supplied without generated plugin payload".into());
    }
    Ok(())
}

fn validate_request(
    request: &RuntimeRequest,
    backend: &WindowsRuntimeBackend,
) -> Result<(), SessionError> {
    let RuntimeOperation::Deployment(deployment) = &request.operation else {
        return Err(SessionError::Protocol(
            "deployment validation requires a deployment request".into(),
        ));
    };
    let plan = &deployment.plan;
    plan.validate()
        .map_err(|error| SessionError::PlanInvalid(error.to_string()))?;
    if request.target != plan.target {
        return Err(SessionError::PlanInvalid(
            "request target does not match the finalized plan".into(),
        ));
    }
    let identity = PayloadOverlayIdentity::from_transaction(
        request.app_id.clone(),
        request.app_version.clone(),
        request.scope,
        plan,
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
    )
    .map_err(SessionError::PlanInvalid)?;
    Ok(())
}

fn execute_local_blocking_with_events(
    request: RuntimeRequest,
    payload: SharedPayload,
    cancel: impl CancellationProbe + Send + 'static,
    events: Option<broadcast::Sender<RuntimeEvent>>,
) -> InstallOutcome {
    if request.scope != SelectedScope::User {
        return InstallOutcome::Failed(
            "machine transactions require the authenticated worker".into(),
        );
    }
    let lock_key = ResourceLock::lock_key(request.app_id.as_str(), "user");
    let state_root = request.state_root.clone();
    let lock_result =
        ResourceLock::try_acquire(&state_root, &lock_key).map_err(|error| error.to_string());
    let _lock = match lock_result {
        Ok(Some(lock)) => lock,
        Ok(None) => {
            return InstallOutcome::Busy {
                operation: "another maintenance operation",
            };
        }
        Err(error) => return InstallOutcome::Failed(error),
    };

    let store = FilesystemTransactionStore::private_at(&state_root);
    let coordinator = TransactionCoordinator::new(store.clone());
    if let Some(id) = request.operation.recovery_transaction() {
        let record = match store.load(&id) {
            Ok(record)
                if record.app_id == request.app_id
                    && record.scope == request.scope
                    && record.target == request.target =>
            {
                record
            }
            Ok(_) => return InstallOutcome::Failed("recovery identity mismatch".into()),
            Err(error) => return InstallOutcome::Failed(error.to_string()),
        };
        let payload = match crate::recovery::RetainedSource::load(&state_root, &record) {
            Ok(source) => source,
            Err(error) => {
                return InstallOutcome::RecoveryRequired {
                    transaction: Some(id.as_uuid()),
                    detail: error,
                };
            }
        };
        let mut executor = ProductionExecutor {
            lifecycle: crate::service_control::ScmLifecycle::default(),
            inner: WindowsFileExecutor::new(payload, id, record.target.executable_suffix())
                .with_state_authority(state_root.clone(), request.scope, &request.app_id),
            cancel,
            events,
            completed_work: 0,
            total_work: record.plan.total_work(),
            blocked_paths: crate::preflight_paths(&record.plan),
            service_controls: record
                .plan
                .nodes
                .iter()
                .filter_map(|node| match &node.kind {
                    zup_plan::NodeKind::ServiceControl { operation } => {
                        Some(operation.as_ref().clone())
                    }
                    _ => None,
                })
                .collect(),
        };
        let result = zup_transaction::recover_requested(
            record,
            &store,
            &mut executor,
            &InstallationRecordStore::new(&state_root),
            request.operation.recovery_request(),
        );
        drop(executor);
        return match result {
            Ok((_, TransactionOutcome::Assessed(assessment))) => {
                InstallOutcome::RecoveryAssessed { assessment }
            }
            Ok((record, TransactionOutcome::Committed)) => {
                match InstallationRecordStore::new(&state_root)
                    .repair_committed(&request.app_id, request.scope)
                {
                    Ok(_) => {
                        crate::notify_committed_path_change(&record);
                        InstallOutcome::Committed
                    }
                    Err(error) => InstallOutcome::CommittedPendingReconciliation {
                        transaction: Some(record.transaction_id.as_uuid()),
                        detail: error.to_string(),
                    },
                }
            }
            Ok((record, TransactionOutcome::RolledBack)) => {
                let _ = crate::recovery::collect(&state_root, &record);
                InstallOutcome::RolledBack
            }
            Ok((record, TransactionOutcome::RecoveryRequired)) => InstallOutcome::recovery(&record),
            Err(error) => InstallOutcome::RecoveryRequired {
                transaction: Some(id.as_uuid()),
                detail: error.to_string(),
            },
        };
    }
    if let Err(e) =
        InstallationRecordStore::new(&state_root).repair_committed(&request.app_id, request.scope)
    {
        return InstallOutcome::Failed(e.to_string());
    }
    let RuntimeOperation::Deployment(deployment) = &request.operation else {
        return InstallOutcome::Failed("deployment request has no plan".into());
    };
    let plan = deployment.plan.clone();
    if let Err(e) = InstallationRecordStore::new(&state_root).validate_plan(
        &request.app_id,
        request.scope,
        &plan,
    ) {
        return InstallOutcome::Failed(e.to_string());
    }
    let open = match coordinator
        .begin(
            request.app_id.clone(),
            request.scope,
            request.app_version.clone(),
            plan,
        )
        .and_then(|open| {
            open.with_release(
                request
                    .operation
                    .deployment()
                    .and_then(|deployment| deployment.release.clone()),
            )
        }) {
        Ok(open) => open,
        Err(e) => return InstallOutcome::Failed(e.to_string()),
    };
    let payload =
        match crate::recovery::RetainedSource::capture(&state_root, open.record(), &payload) {
            Ok(source) => source,
            Err(detail) => {
                let record = open.preserve();
                return InstallOutcome::RecoveryRequired {
                    transaction: Some(record.transaction_id.as_uuid()),
                    detail,
                };
            }
        };
    if request.scope == SelectedScope::User
        && let Ok(identity) = PayloadOverlayIdentity::from_transaction(
            request.app_id.clone(),
            request.app_version.clone(),
            request.scope,
            &open.record().plan,
        )
        && let Some(overlay) = identity.path_under(&state_root)
    {
        let _ = cleanup_payload_overlay(&state_root, Some(&overlay));
    }
    let transaction_id = open.record().transaction_id;
    let plan_for_executor = open.record().plan.clone();
    let target_suffix = open.record().target.executable_suffix();

    if cancel.is_cancelled() {
        let mut executor = ProductionExecutor {
            lifecycle: crate::service_control::ScmLifecycle::default(),
            inner: WindowsFileExecutor::new(payload, transaction_id, target_suffix)
                .with_state_authority(state_root.clone(), request.scope, &request.app_id),
            cancel,
            events,
            completed_work: 0,
            total_work: 0,
            blocked_paths: Vec::new(),
            service_controls: Vec::new(),
        };
        return match open.release(&mut executor) {
            Ok(zup_transaction::SettledTransaction {
                outcome: TransactionOutcome::Assessed(assessment),
                ..
            }) => InstallOutcome::RecoveryAssessed { assessment },
            Ok(zup_transaction::SettledTransaction {
                outcome: TransactionOutcome::RolledBack,
                ..
            }) => InstallOutcome::Cancelled,
            Ok(zup_transaction::SettledTransaction {
                outcome: TransactionOutcome::RecoveryRequired,
                record,
            }) => InstallOutcome::recovery(&record),
            Err(error) => InstallOutcome::RecoveryRequired {
                transaction: Some(transaction_id.as_uuid()),
                detail: error.to_string(),
            },
            Ok(zup_transaction::SettledTransaction {
                outcome: TransactionOutcome::Committed,
                ..
            }) => InstallOutcome::Failed("a cancelled transaction cannot commit".into()),
        };
    }

    let mut executor = ProductionExecutor {
        lifecycle: crate::service_control::ScmLifecycle::default(),
        inner: WindowsFileExecutor::new(payload, transaction_id, target_suffix)
            .with_state_authority(state_root.clone(), request.scope, &request.app_id),
        cancel,
        events,
        completed_work: 0,
        total_work: plan_for_executor.total_work(),
        blocked_paths: crate::preflight_paths(&plan_for_executor),
        service_controls: plan_for_executor
            .nodes
            .iter()
            .filter_map(|node| match &node.kind {
                zup_plan::NodeKind::ServiceControl { operation } => {
                    Some(operation.as_ref().clone())
                }
                _ => None,
            })
            .collect(),
    };

    let result = open.execute(&mut executor, &InstallationRecordStore::new(&state_root));
    let was_cancelled = executor.cancel.is_cancelled();
    drop(executor);
    match result {
        Ok(zup_transaction::SettledTransaction {
            outcome: TransactionOutcome::Assessed(assessment),
            ..
        }) => InstallOutcome::RecoveryAssessed { assessment },
        Ok(zup_transaction::SettledTransaction {
            record,
            outcome: TransactionOutcome::Committed,
        }) => {
            match InstallationRecordStore::new(&state_root)
                .reconcile_installation(&record, request.scope)
            {
                Ok(_) => {
                    if let Ok(record) = store.load(&record.transaction_id) {
                        let _ = crate::recovery::collect(&state_root, &record);
                    }
                    crate::notify_committed_path_change(&record);
                    InstallOutcome::Committed
                }
                Err(e) => InstallOutcome::CommittedPendingReconciliation {
                    transaction: Some(record.transaction_id.as_uuid()),
                    detail: format!("installation committed but reconciliation failed: {e}"),
                },
            }
        }
        Ok(zup_transaction::SettledTransaction {
            outcome: TransactionOutcome::RolledBack,
            record,
        }) => {
            let _ = crate::recovery::collect(&state_root, &record);
            if was_cancelled {
                InstallOutcome::Cancelled
            } else {
                InstallOutcome::RolledBack
            }
        }
        Ok(zup_transaction::SettledTransaction {
            outcome: TransactionOutcome::RecoveryRequired,
            record,
        }) => InstallOutcome::recovery(&record),
        Err(error) => InstallOutcome::RecoveryRequired {
            transaction: Some(transaction_id.as_uuid()),
            detail: error.to_string(),
        },
    }
}

struct ProductionExecutor<P: zup_bundle::PayloadSource, C: CancellationProbe> {
    inner: WindowsFileExecutor<P>,
    cancel: C,
    events: Option<broadcast::Sender<RuntimeEvent>>,
    completed_work: u64,
    total_work: u64,

    blocked_paths: Vec<PathBuf>,
    service_controls: Vec<zup_plan::ServiceControlOperation>,
    lifecycle: crate::service_control::ScmLifecycle,
}

impl<P: zup_bundle::PayloadSource, C: CancellationProbe> OperationExecutor
    for ProductionExecutor<P, C>
{
    type Error = String;

    fn prepare_compensation(
        &mut self,
        _operation: &DeploymentNode,
        receipt: &OperationReceipt,
    ) -> Result<OperationReceipt, String> {
        match receipt {
            OperationReceipt::ServiceControl(receipt) => {
                zup_transaction::prepare_service_compensation(&mut self.lifecycle, receipt)
                    .map(|receipt| OperationReceipt::ServiceControl(Box::new(receipt)))
            }
            _ => Ok(receipt.clone()),
        }
    }

    fn reconcile_rollback(
        &mut self,
        operation: &DeploymentNode,
        receipt: &OperationReceipt,
    ) -> Result<zup_transaction::RollbackReconcileResult, String> {
        match receipt {
            OperationReceipt::PersistentDirectory(owned) => {
                self.inner.verify_persistent(operation, owned)?;
                Ok(zup_transaction::RollbackReconcileResult::Restored)
            }
            OperationReceipt::NativeService(receipt) => {
                let zup_plan::NodeKind::NativeService { operation } = &operation.kind else {
                    return Err("native service receipt requires a native service operation".into());
                };
                crate::services::reconcile_native_rollback(operation, receipt)
                    .map_err(|error| error.to_string())
            }
            OperationReceipt::Generation(receipt) => {
                self.inner.retain_generation(operation, receipt)?;
                Ok(zup_transaction::RollbackReconcileResult::Restored)
            }
            OperationReceipt::ServiceControl(receipt) => {
                zup_transaction::reconcile_service_rollback(&mut self.lifecycle, receipt)
            }
            OperationReceipt::Control => Ok(zup_transaction::RollbackReconcileResult::Restored),
            OperationReceipt::RetainedFile { .. }
            | OperationReceipt::StageFile { .. }
            | OperationReceipt::CreateFile { .. }
            | OperationReceipt::ReplaceFile { .. }
            | OperationReceipt::RemoveFile { .. } => {
                Ok(zup_transaction::RollbackReconcileResult::Retry)
            }
            _ => Ok(zup_transaction::RollbackReconcileResult::Ambiguous),
        }
    }

    fn prepare(&mut self, operation: &DeploymentNode) -> Result<(), Self::Error> {
        if self.cancel.is_cancelled() {
            return Err("cancelled".into());
        }
        match &operation.kind {
            zup_plan::NodeKind::Barrier => {
                if let Some(events) = &self.events {
                    let _ = events.send(RuntimeEvent::PreflightStarted);
                }
                let blockers = self
                    .blocked_paths
                    .iter()
                    .map(|path| path.as_path())
                    .collect::<Vec<_>>();
                let blocked = crate::restart_manager::preflight_services(
                    &blockers,
                    &self.service_controls,
                    &mut self.lifecycle,
                )
                .map_err(|error| error.to_string())?;
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

            _ => Ok(()),
        }
    }

    fn apply(
        &mut self,
        operation: &DeploymentNode,
        transaction: &zup_transaction::TransactionRecord,
    ) -> Result<OperationReceipt, Self::Error> {
        if self.cancel.is_cancelled() {
            return Err("cancelled".into());
        }
        if let Some(events) = &self.events {
            let _ = events.send(RuntimeEvent::OperationStarted {
                id: operation.id.to_string(),
            });
        }
        let receipt = match &operation.kind {
            zup_plan::NodeKind::Cleanup { .. } => self.inner.apply_cleanup(operation, transaction),
            zup_plan::NodeKind::PersistentDirectory { .. } => {
                self.inner.apply_persistent(operation, transaction)
            }
            zup_plan::NodeKind::NativeService { operation } => {
                crate::services::apply_native(operation, &mut self.lifecycle)
                    .map_err(|error| error.to_string())
            }
            zup_plan::NodeKind::NativeCache { .. } => {
                Err("Linux native cache operation is unsupported on Windows".into())
            }
            zup_plan::NodeKind::Generation { .. } => self.inner.apply_generation(operation),
            zup_plan::NodeKind::Retention { .. }
            | zup_plan::NodeKind::ReserveInstallation
            | zup_plan::NodeKind::PublishInstallation => {
                Err("installation publication belongs to the coordinator".into())
            }
            zup_plan::NodeKind::ServiceControl { operation } => {
                zup_transaction::apply_service_control(&mut self.lifecycle, operation)
            }
            zup_plan::NodeKind::Barrier => Ok(OperationReceipt::Control),
            zup_plan::NodeKind::BackendOperation { .. } => {
                crate::integration::apply_managed(operation).map_err(|e| e.to_string())
            }
            zup_plan::NodeKind::BackendRemoval { .. } => {
                crate::integration::apply_owned_removal(operation).map_err(|e| e.to_string())
            }
            zup_plan::NodeKind::FileRemoval { .. } => self
                .inner
                .apply_owned_file_removal(operation)
                .map_err(|e| e.to_string()),
            zup_plan::NodeKind::StageFile { .. }
            | zup_plan::NodeKind::RetainFile { .. }
            | zup_plan::NodeKind::FileMutation { .. } => {
                crate::apply_node(&mut self.inner, operation, transaction)
                    .map_err(|e| e.to_string())
            }
        }?;

        self.completed_work = self.completed_work.saturating_add(operation.work());
        if let Some(events) = &self.events {
            let _ = events.send(RuntimeEvent::Progress {
                phase: zup_runtime::operation_phase(operation),
                completed: self.completed_work.min(self.total_work),
                total: self.total_work,
                action: zup_runtime::operation_result_action(operation, &receipt).into(),
            });
        }
        Ok(receipt)
    }

    fn verify(
        &mut self,
        operation: &DeploymentNode,
        receipt: &OperationReceipt,
    ) -> Result<(), Self::Error> {
        if matches!(receipt, OperationReceipt::Cleanup { .. }) {
            return self.inner.verify_cleanup(operation, receipt);
        }
        if let OperationReceipt::PersistentDirectory(owned) = receipt {
            return self.inner.verify_persistent(operation, owned);
        }
        if self.cancel.is_cancelled() {
            return Err("cancelled".into());
        }
        if let OperationReceipt::NativeService(receipt) = receipt {
            let zup_plan::NodeKind::NativeService { operation } = &operation.kind else {
                return Err("native service receipt requires a native service operation".into());
            };
            return crate::services::verify_native(operation, receipt, &mut self.lifecycle)
                .map_err(|error| error.to_string());
        }
        if let OperationReceipt::Generation(receipt) = receipt {
            return self.inner.verify_generation(operation, receipt);
        }
        if let OperationReceipt::ServiceControl(receipt) = receipt {
            return zup_transaction::verify_service_control(&mut self.lifecycle, receipt);
        }
        match &operation.kind {
            zup_plan::NodeKind::BackendOperation { .. }
            | zup_plan::NodeKind::BackendRemoval { .. } => {
                crate::integration::verify_managed(operation, receipt)
                    .map_err(|error| error.to_string())
            }
            zup_plan::NodeKind::Barrier => Ok(()),
            _ => {
                crate::verify_installed_file(operation, receipt).map_err(|error| error.to_string())
            }
        }
    }

    fn rollback(
        &mut self,
        operation: &DeploymentNode,
        receipt: &OperationReceipt,
    ) -> Result<(), Self::Error> {
        match receipt {
            OperationReceipt::PersistentDirectory(owned) => {
                self.inner.verify_persistent(operation, owned)
            }
            OperationReceipt::NativeService(receipt) => {
                let zup_plan::NodeKind::NativeService { operation } = &operation.kind else {
                    return Err("native service receipt requires a native service operation".into());
                };
                crate::services::rollback_native(operation, receipt, &mut self.lifecycle)
                    .map_err(|error| error.to_string())
            }
            OperationReceipt::Generation(receipt) => {
                self.inner.retain_generation(operation, receipt)
            }
            OperationReceipt::ServiceControl(receipt) => {
                zup_transaction::rollback_service_control(&mut self.lifecycle, receipt)
            }
            OperationReceipt::RetainedFile { .. }
            | OperationReceipt::StageFile { .. }
            | OperationReceipt::CreateFile { .. }
            | OperationReceipt::ReplaceFile { .. }
            | OperationReceipt::RemoveFile { .. } => self
                .inner
                .rollback_transaction_receipt(operation, receipt)
                .map_err(|e| e.to_string()),
            _ => {
                crate::integration::rollback_managed(operation, receipt).map_err(|e| e.to_string())
            }
        }
    }

    fn reconcile(
        &mut self,
        operation: &DeploymentNode,
        _receipt: Option<&OperationReceipt>,
        transaction: &zup_transaction::TransactionRecord,
    ) -> Result<ReconcileResult, Self::Error> {
        if matches!(operation.kind, zup_plan::NodeKind::Cleanup { .. }) {
            return self
                .inner
                .apply_cleanup(operation, transaction)
                .map(ReconcileResult::AppliedWithReceipt);
        }
        if let zup_plan::NodeKind::NativeService { operation } = &operation.kind {
            return crate::services::reconcile_native(operation, &mut self.lifecycle)
                .map_err(|error| error.to_string());
        }
        if matches!(operation.kind, zup_plan::NodeKind::Generation { .. }) {
            return self.inner.reconcile_generation(operation, _receipt);
        }
        if let zup_plan::NodeKind::ServiceControl { operation } = &operation.kind {
            let receipt = match _receipt {
                Some(OperationReceipt::ServiceControl(receipt)) => Some(receipt.as_ref()),
                _ => None,
            };
            return zup_transaction::reconcile_service_control(
                &mut self.lifecycle,
                operation,
                receipt,
            );
        }
        match &operation.kind {
            zup_plan::NodeKind::BackendOperation { .. } => {
                return crate::reconcile_managed(operation).map_err(|e| e.to_string());
            }
            zup_plan::NodeKind::BackendRemoval { .. } => {
                return crate::reconcile_owned_removal(operation).map_err(|e| e.to_string());
            }
            zup_plan::NodeKind::FileRemoval { .. } => {
                return self
                    .inner
                    .reconcile_owned_file_removal(operation)
                    .map_err(|e| e.to_string());
            }
            _ => {}
        }
        self.inner
            .reconcile_transaction_node(operation, _receipt, transaction)
            .map_err(|e| e.to_string())
    }
}
