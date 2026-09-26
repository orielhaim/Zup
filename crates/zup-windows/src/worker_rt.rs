//! Real worker runtime: connect, authenticate, handshake, one transaction, exit.
use std::path::{Path, PathBuf};

use std::time::Duration;
use zup_bootstrap::{
    BootstrapId, BootstrapState, BootstrapStateStore, BoundBootstrapPlan,
    FilesystemBootstrapStateStore, Quarantine, execute_plan_with_persist, recover,
};

use crate::{AutoPayloadSource, EmbeddedBundle};
use tokio_util::sync::CancellationToken;
use zup_protocol::{
    Message, PROTOCOL_VERSION, ProgressKind, ProgressReport, SequenceTracker, SessionId,
    WireEnvelope, WorkerHello,
};
use zup_transaction::{
    CancellationProbe, FilesystemTransactionStore, OperationExecutor, OperationReceipt,
    ReconcileResult, TransactionCoordinator, TransactionNode, TransactionOutcome, TransactionPlan,
};

use crate::FilePrecondition;
use crate::durable::InstallationLock;
use crate::file_executor::{NullProgress, WindowsFileExecutor, apply_node};
use crate::payload_overlay::{
    PayloadOverlayIdentity, cleanup_payload_overlay, validate_payload_overlay_base,
    verify_payload_overlay,
};
use crate::pipe::{ClientReader, ClientWriter, PipeError, frame_client};
use crate::transport::{UserSid, verify_server_pid};
use crate::worker::{WorkerBootstrap, WorkerError, plan_hash_hex};

/// Timeouts (no timeout on installation execution).
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

struct WorkerOverlayCleanup {
    base: Option<PathBuf>,
    root: Option<PathBuf>,
    retain: bool,
}

impl WorkerOverlayCleanup {
    fn new(base: Option<PathBuf>, root: Option<PathBuf>, retain_on_error: bool) -> Self {
        Self {
            base,
            root,
            retain: retain_on_error,
        }
    }

    fn retain(&mut self) {
        self.retain = true;
    }

    fn cleanup(&mut self) {
        self.retain = false;
    }
}

impl Drop for WorkerOverlayCleanup {
    fn drop(&mut self) {
        if !self.retain
            && let Some(root) = &self.root
            && let Some(base) = &self.base
        {
            let _ = cleanup_payload_overlay(base, Some(root));
        }
    }
}

/// Run the complete worker lifecycle against the parent named pipe.
///
/// Pre-auth zero-side-effect: no journal, lock, staging, or mutation until
/// authentication + plan validation complete.
pub async fn run_worker(
    bootstrap: WorkerBootstrap,
    cancel: CancellationToken,
) -> Result<String, WorkerError> {
    run_worker_inner(bootstrap, cancel, true).await
}

#[doc(hidden)]
#[cfg(feature = "test-launcher")]
pub async fn run_worker_for_test(
    bootstrap: WorkerBootstrap,
    cancel: CancellationToken,
) -> Result<String, WorkerError> {
    run_worker_inner(bootstrap, cancel, false).await
}

async fn run_worker_inner(
    bootstrap: WorkerBootstrap,
    cancel: CancellationToken,
    require_elevation: bool,
) -> Result<String, WorkerError> {
    // 1. Connect (bounded retries for transient pipe-not-ready only).
    let client = crate::pipe::PipeClient::connect(&bootstrap.pipe_name)
        .await
        .map_err(|e| WorkerError::Protocol(e.to_string()))?;

    // 2. Verify server PID matches bootstrap parent PID (zero mutation).
    verify_server_pid(client.as_raw() as isize, bootstrap.expected_parent_pid)
        .map_err(|e| WorkerError::AuthFailed(e.to_string()))?;
    let parent_sid = UserSid::for_process(bootstrap.expected_parent_pid)
        .map_err(|e| WorkerError::AuthFailed(e.to_string()))?;
    if parent_sid.display() != bootstrap.expected_parent_sid {
        return Err(WorkerError::AuthFailed("parent SID mismatch".into()));
    }

    // 3. Verify our own elevation for machine-scope workers.
    if require_elevation
        && !crate::transport::is_process_elevated()
            .map_err(|e| WorkerError::AuthFailed(e.to_string()))?
    {
        return Err(WorkerError::AuthFailed("worker is not elevated".into()));
    }

    // 4. Parent SID check: server process user SID must match initiating SID
    //    embedded in bootstrap. (Worker SID may differ — over-the-shoulder UAC.)
    let (mut reader, mut writer) = frame_client(client.into_inner());
    let mut incoming = SequenceTracker::new();
    let mut outgoing: u64 = 0;

    // 5. WorkerHello
    let hello = WireEnvelope {
        version: PROTOCOL_VERSION,
        session_id: bootstrap.session_id,
        sequence: 0,
        message: Message::WorkerHello(WorkerHello {
            protocol_version: PROTOCOL_VERSION,
            session_id: bootstrap.session_id,
            target: bootstrap.target.clone(),
            worker_pid: std::process::id(),
            capabilities: crate::worker_capabilities(),
        }),
    };
    writer
        .send(&hello)
        .await
        .map_err(|e| WorkerError::Protocol(e.to_string()))?;
    outgoing = outgoing.saturating_add(1);

    // 6. ParentHello (bounded timeout).
    let parent_hello = tokio::time::timeout(HANDSHAKE_TIMEOUT, reader.recv())
        .await
        .map_err(|_| WorkerError::Protocol("hello timeout".into()))?
        .map_err(|e| WorkerError::Protocol(e.to_string()))?;
    incoming
        .accept(parent_hello.sequence)
        .map_err(|e| WorkerError::Protocol(e.to_string()))?;
    match &parent_hello.message {
        Message::ParentHello(hello) => {
            if hello.protocol_version != PROTOCOL_VERSION
                || hello.session_id != bootstrap.session_id
                || hello.target != bootstrap.target
                || hello.expected_plan_hash != bootstrap.expected_plan_hash
            {
                return Err(WorkerError::AuthFailed("parent hello mismatch".into()));
            }
        }
        _ => return Err(WorkerError::Protocol("expected ParentHello".into())),
    }
    // Authenticated — only now may mutation state be created.

    // 7. Read ExecuteTransaction (one only).
    let exec_env = tokio::time::timeout(HANDSHAKE_TIMEOUT, reader.recv())
        .await
        .map_err(|_| WorkerError::Protocol("execute timeout".into()))?
        .map_err(|e| WorkerError::Protocol(e.to_string()))?;
    incoming
        .accept(exec_env.sequence)
        .map_err(|e| WorkerError::Protocol(e.to_string()))?;
    let exec = match exec_env.message {
        Message::ExecuteBootstrap(exec) => {
            return run_bootstrap_worker(
                bootstrap, exec, reader, writer, incoming, outgoing, cancel,
            )
            .await;
        }
        Message::ExecuteTransaction(exec) => exec,
        Message::Cancel => return Err(WorkerError::Protocol("cancelled".into())),
        _ => {
            return Err(WorkerError::Protocol(
                "expected ExecuteTransaction or ExecuteBootstrap".into(),
            ));
        }
    };

    // 8. Plan binding + capability check (still before lock/journal).
    if exec.target != bootstrap.target {
        return Err(WorkerError::TargetMismatch);
    }
    if exec.plan_json.len() > zup_protocol::MAX_PLAN_BYTES {
        return Err(WorkerError::Protocol("plan too large".into()));
    }
    let hash = plan_hash_hex(&exec.plan_json);
    if hash != bootstrap.expected_plan_hash || hash != exec.plan_hash {
        return Err(WorkerError::PlanHashMismatch);
    }
    let plan: TransactionPlan = serde_json::from_str(&exec.plan_json)
        .map_err(|e| WorkerError::Protocol(format!("bad plan: {e}")))?;
    if exec.target != plan.target {
        return Err(WorkerError::TargetMismatch);
    }
    plan.validate()
        .map_err(|error| WorkerError::Protocol(format!("invalid plan: {error}")))?;
    for node in &plan.nodes {
        match node.kind {
            zup_transaction::NodeKind::Barrier
            | zup_transaction::NodeKind::StageFile { .. }
            | zup_transaction::NodeKind::FileMutation { .. }
            | zup_transaction::NodeKind::FileRemoval { .. }
            | zup_transaction::NodeKind::BackendOperation { .. }
            | zup_transaction::NodeKind::BackendRemoval { .. } => {}
        }
    }

    let app_id = zup_core::AppId::new(&exec.app_id)
        .map_err(|e| WorkerError::Protocol(format!("bad app id: {e}")))?;
    let app_version: semver::Version = exec
        .app_version
        .parse()
        .map_err(|e| WorkerError::Protocol(format!("bad app version: {e}")))?;
    let scope = match exec.scope.as_str() {
        "user" => zup_core::SelectedScope::User,
        "machine" => zup_core::SelectedScope::Machine,
        _ => return Err(WorkerError::Protocol("invalid install scope".into())),
    };
    if require_elevation {
        let executable = crate::worker::current_exe()
            .map_err(|error| WorkerError::AuthFailed(error.to_string()))?;
        let bundle = EmbeddedBundle::open(&executable)
            .map_err(|error| WorkerError::AuthFailed(format!("worker bundle: {error}")))?;
        validate_transaction_bundle_identity(
            &app_id,
            &app_version,
            scope,
            &bootstrap.target,
            &bundle,
        )?;
    }
    let state_root = PathBuf::from(exec.state_root);
    let work_root = PathBuf::from(exec.work_root);
    let payload_overlay_root = decode_overlay_path(exec.payload_overlay_root.as_deref())?;
    let payload_overlay_base_root = decode_overlay_path(exec.payload_overlay_base_root.as_deref())?;
    let mut overlay_cleanup = WorkerOverlayCleanup::new(
        payload_overlay_base_root.clone(),
        payload_overlay_root.clone(),
        exec.recovery_id.is_some(),
    );
    let identity =
        PayloadOverlayIdentity::from_transaction(app_id.clone(), app_version.clone(), scope, &plan)
            .map_err(|error| WorkerError::AuthFailed(error.to_string()))?;
    validate_worker_overlay(
        &state_root,
        scope,
        &identity,
        payload_overlay_root.as_deref(),
        payload_overlay_base_root.as_deref(),
        exec.recovery_id.is_some(),
        &bootstrap.expected_parent_sid,
    )?;
    let existing_record = if let Some(id) = exec.recovery_id {
        let record = zup_transaction::TransactionStore::load(
            &FilesystemTransactionStore::new(&state_root),
            &zup_transaction::TransactionId::from_uuid(id),
        )
        .map_err(|e| WorkerError::Transaction(e.to_string()))?;
        if record.app_id != app_id
            || record.scope != scope
            || record.app_version != app_version
            || record.target != plan.target
            || record.plan != plan
        {
            return Err(WorkerError::AuthFailed("recovery record mismatch".into()));
        }
        if identity.has_files() {
            verify_payload_overlay(
                payload_overlay_base_root.as_deref().ok_or_else(|| {
                    WorkerError::AuthFailed("missing recovery overlay base".into())
                })?,
                &identity,
                payload_overlay_root
                    .as_deref()
                    .ok_or_else(|| WorkerError::AuthFailed("missing recovery overlay".into()))?,
            )
            .map_err(|error| WorkerError::AuthFailed(error.to_string()))?;
        }
        Some(record)
    } else {
        None
    };
    let payload_root = PathBuf::from(exec.payload_root);
    let payload = AutoPayloadSource::from_paths(payload_root.clone(), payload_overlay_root.clone())
        .map_err(|e| WorkerError::Transaction(format!("payload package: {e}")))?;

    writer
        .send(&WireEnvelope {
            version: PROTOCOL_VERSION,
            session_id: bootstrap.session_id,
            sequence: outgoing,
            message: Message::Progress(ProgressReport {
                kind: ProgressKind::OperationStarted,
                detail: "transaction accepted".into(),
                completed: None,
                total: None,
            }),
        })
        .await
        .map_err(|e| WorkerError::Protocol(e.to_string()))?;
    outgoing = outgoing.saturating_add(1);

    // 9. NOW we may touch mutation state: lock + journal + executor.
    let lock_key = InstallationLock::lock_key(app_id.as_str(), &exec.scope);
    let _lock = InstallationLock::try_acquire(&state_root, &lock_key)
        .map_err(|e| WorkerError::Transaction(e.to_string()))?
        .ok_or_else(|| WorkerError::Transaction("installation busy".into()))?;

    if exec.recovery_id.is_none() {
        crate::ledger::InstallLedgerStore::new(&state_root)
            .repair_committed(&app_id, scope)
            .map_err(|e| WorkerError::Transaction(e.to_string()))?;
        crate::ledger::InstallLedgerStore::new(&state_root)
            .validate_plan(&app_id, scope, &app_version, &plan)
            .map_err(|e| WorkerError::Transaction(e.to_string()))?;
    }

    let store = FilesystemTransactionStore::new(&state_root);
    let coordinator = TransactionCoordinator::new(store);
    let record = if let Some(record) = existing_record {
        record
    } else {
        coordinator
            .begin(app_id, scope, app_version, plan)
            .map_err(|e| WorkerError::Transaction(e.to_string()))?
    };

    let total_work = transaction_work_total(&record.plan);
    let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel::<ProgressReport>();
    let mut progress_sequence = outgoing;
    let progress_session = bootstrap.session_id;
    let progress_task = tokio::spawn(async move {
        while let Some(progress) = progress_rx.recv().await {
            writer
                .send(&WireEnvelope {
                    version: PROTOCOL_VERSION,
                    session_id: progress_session,
                    sequence: progress_sequence,
                    message: Message::Progress(progress),
                })
                .await
                .map_err(|error| WorkerError::Protocol(error.to_string()))?;
            progress_sequence = progress_sequence.saturating_add(1);
        }
        Ok::<_, WorkerError>((writer, progress_sequence))
    });

    let mut executor = WorkerFileExecutor {
        inner: WindowsFileExecutor::new(
            payload,
            work_root,
            record.transaction_id.to_string(),
            Box::new(NullProgress),
        ),
        cancel: TokenProbe(cancel.clone()),
        progress: progress_tx.clone(),
        completed_work: 0,
        total_work,
        blocked_paths: crate::plan_mutating_paths(&record.plan, &record.target),
    };
    for node in &record.plan.nodes {
        if matches!(
            node.kind,
            zup_transaction::NodeKind::StageFile { .. }
                | zup_transaction::NodeKind::FileMutation { .. }
        ) && let (Some(sha256), Some(size)) =
            (node.meta.expected_sha256, node.meta.expected_size)
        {
            let precondition = node
                .meta
                .file_precondition
                .unwrap_or(FilePrecondition::Absent);
            executor
                .inner
                .note_file(&node.id, precondition, sha256, size);
        }
    }

    // Full-duplex: spawn a reader task for Cancel while executing.
    let cancel_token = cancel.clone();
    let reader_task = split_reader(reader, cancel_token, incoming);

    let recovering = exec.recovery_id.is_some();
    let recovery_root = state_root.clone();
    let result = tokio::task::spawn_blocking(move || {
        if recovering {
            zup_transaction::recover(
                record,
                &FilesystemTransactionStore::new(&recovery_root),
                &mut executor,
            )
        } else {
            coordinator.execute(record, &mut executor)
        }
    })
    .await
    .map_err(|e| WorkerError::Transaction(e.to_string()))?;

    if matches!(&result, Ok((_, TransactionOutcome::Committed))) {
        let _ = progress_tx.send(ProgressReport {
            kind: ProgressKind::OperationProgress,
            detail: "Finishing…".into(),
            completed: Some(total_work),
            total: Some(total_work),
        });
    }

    drop(progress_tx);
    let (writer, outgoing) = progress_task
        .await
        .map_err(|error| WorkerError::Protocol(error.to_string()))??;

    let (transaction_id, outcome) = match result {
        Ok((r, TransactionOutcome::Committed)) => {
            overlay_cleanup.cleanup();
            let ledgers = crate::ledger::InstallLedgerStore::new(&state_root);
            let publication = if recovering {
                ledgers.repair_committed(&r.app_id, scope)
            } else {
                ledgers.publish_committed(&r, scope).map(|_| ())
            };
            publication.map_err(|e| {
                WorkerError::Transaction(format!(
                    "transaction committed but ledger publication failed: {e}"
                ))
            })?;
            crate::integration::notify_committed_path_change(&r);
            (r.transaction_id.as_uuid(), "committed".to_owned())
        }
        Ok((r, TransactionOutcome::RolledBack)) => {
            overlay_cleanup.cleanup();
            (r.transaction_id.as_uuid(), "rolled_back".to_owned())
        }
        Ok((r, TransactionOutcome::RecoveryRequired)) => {
            overlay_cleanup.retain();
            (r.transaction_id.as_uuid(), "recovery_required".to_owned())
        }
        Err(e) => {
            // Send Failed then exit.
            let _ = send_and_close(
                writer,
                bootstrap.session_id,
                outgoing,
                Message::Failed(zup_protocol::Failed {
                    kind: "transaction".into(),
                    message: e.to_string(),
                }),
            )
            .await;
            let _ = reader_task.await;
            return Err(WorkerError::Transaction(e.to_string()));
        }
    };
    drop(_lock);
    // 10. Durable terminal state → Completed → flush → close → exit.
    let _ = send_and_close(
        writer,
        bootstrap.session_id,
        outgoing,
        Message::Completed(zup_protocol::Completed {
            transaction_id,
            outcome: outcome.clone(),
            prerequisite_id: None,
            exit_code: None,
        }),
    )
    .await;
    let _ = reader_task.await;
    Ok(outcome)
}

async fn run_bootstrap_worker(
    bootstrap: WorkerBootstrap,
    exec: zup_protocol::ExecuteBootstrap,
    reader: ClientReader,
    mut writer: ClientWriter,
    incoming: SequenceTracker,
    mut outgoing: u64,
    cancel: CancellationToken,
) -> Result<String, WorkerError> {
    if exec.target != bootstrap.target {
        return Err(WorkerError::TargetMismatch);
    }
    if exec.bootstrap_json.len() > zup_protocol::MAX_PLAN_BYTES {
        return Err(WorkerError::Protocol("bootstrap plan too large".into()));
    }
    let hash = plan_hash_hex(&exec.bootstrap_json);
    if hash != bootstrap.expected_plan_hash || hash != exec.bootstrap_hash {
        return Err(WorkerError::PlanHashMismatch);
    }
    let bound: BoundBootstrapPlan = serde_json::from_str(&exec.bootstrap_json)
        .map_err(|error| WorkerError::Protocol(format!("bad bootstrap plan: {error}")))?;
    let declared_plan_hash = bound.plan_hash;
    let validated = BoundBootstrapPlan::with_id(bound.id, bound.plan, bound.artifacts)
        .map_err(|error| WorkerError::AuthFailed(error.to_string()))?;
    if bound.id.as_uuid() != exec.bootstrap_id
        || validated.plan.key.target != bootstrap.target
        || exec.target != validated.plan.key.target
        || validated.plan_hash != declared_plan_hash
        || validated.plan.key.app_id.as_str() != exec.app_id
        || validated.plan.key.app_version.to_string() != exec.app_version
        || validated.plan.key.scope.to_string() != exec.scope
    {
        return Err(WorkerError::AuthFailed(
            "bootstrap identity mismatch".into(),
        ));
    }
    let bound = validated;
    let executable =
        crate::worker::current_exe().map_err(|error| WorkerError::AuthFailed(error.to_string()))?;
    let bundle = EmbeddedBundle::open(&executable)
        .map_err(|error| WorkerError::AuthFailed(format!("worker bundle: {error}")))?;
    validate_bootstrap_bundle(&bound.plan, &bundle)?;
    if exec.recovery_id.is_some() {
        return Err(WorkerError::AuthFailed(
            "bootstrap recovery ids are not supported".into(),
        ));
    }
    let state_root = PathBuf::from(&exec.state_root);
    let quarantine_root = PathBuf::from(&exec.quarantine_root);
    if exec.state_root.is_empty()
        || exec.quarantine_root.is_empty()
        || exec.state_root.len() > zup_protocol::MAX_PAYLOAD_OVERLAY_PATH_BYTES
        || exec.quarantine_root.len() > zup_protocol::MAX_PAYLOAD_OVERLAY_PATH_BYTES
        || exec.state_root.contains('\0')
        || exec.quarantine_root.contains('\0')
        || !state_root.is_absolute()
        || !quarantine_root.is_absolute()
    {
        return Err(WorkerError::AuthFailed(
            "bootstrap roots must be absolute".into(),
        ));
    }
    let quarantine =
        Quarantine::with_file_system(&quarantine_root, crate::windows_bootstrap_file_system())
            .map_err(|error| WorkerError::AuthFailed(error.to_string()))?;
    for artifact in bound.artifacts.values() {
        quarantine
            .verify(artifact)
            .map_err(|error| WorkerError::AuthFailed(error.to_string()))?;
    }
    let store = FilesystemBootstrapStateStore::with_file_system(
        &state_root,
        crate::windows_bootstrap_file_system(),
    );
    let bootstrap_id = BootstrapId::from_uuid(exec.bootstrap_id);
    let mut state = match store.load(bootstrap_id) {
        Ok(state) => state,
        Err(zup_bootstrap::BootstrapStoreError::Missing) => {
            let mut state = BootstrapState::new(&bound.plan);
            state.id = bootstrap_id;
            store
                .create(&state)
                .map_err(|error| WorkerError::Transaction(error.to_string()))?;
            state
        }
        Err(error) => return Err(WorkerError::Transaction(error.to_string())),
    };
    state
        .validate(&bound.plan)
        .map_err(|error| WorkerError::AuthFailed(error.to_string()))?;
    let lock_key =
        InstallationLock::lock_key(&format!("bootstrap-{}", exec.bootstrap_id), &exec.scope);
    let _lock = InstallationLock::try_acquire(&state_root, &lock_key)
        .map_err(|error| WorkerError::Transaction(error.to_string()))?
        .ok_or_else(|| WorkerError::Transaction("bootstrap is busy".into()))?;
    writer
        .send(&WireEnvelope {
            version: PROTOCOL_VERSION,
            session_id: bootstrap.session_id,
            sequence: outgoing,
            message: Message::Progress(ProgressReport {
                kind: ProgressKind::OperationStarted,
                detail: "Checking prerequisites…".into(),
                completed: None,
                total: None,
            }),
        })
        .await
        .map_err(|error| WorkerError::Protocol(error.to_string()))?;
    outgoing = outgoing.saturating_add(1);
    let reader_task = split_reader(reader, cancel.clone(), incoming);
    let satisfier = crate::WindowsPrerequisiteDetector;
    let provider = crate::WindowsPrerequisiteProvider;
    if state.operations.iter().any(|operation| {
        matches!(
            operation.state,
            zup_bootstrap::BootstrapOperationState::Running
        )
    }) {
        let old_revision = state.revision;
        let recovered = recover(&bound.plan, &satisfier, &mut state)
            .map_err(|error| WorkerError::Transaction(error.to_string()))?;
        state.revision = old_revision.saturating_add(1);
        store
            .compare_and_swap(old_revision, &state)
            .map_err(|error| WorkerError::Transaction(error.to_string()))?;
        if matches!(recovered, zup_bootstrap::BootstrapOutcome::RecoveryRequired) {
            let _ = send_and_close(
                writer,
                bootstrap.session_id,
                outgoing,
                Message::Completed(zup_protocol::Completed {
                    transaction_id: exec.bootstrap_id,
                    outcome: "recovery_required".into(),
                    prerequisite_id: None,
                    exit_code: None,
                }),
            )
            .await;
            return Ok("recovery_required".into());
        }
    }
    let assessed_revision = state.revision;
    zup_bootstrap::assess(&bound.plan, &satisfier, &mut state)
        .map_err(|error| WorkerError::AuthFailed(error.to_string()))?;
    state.revision = assessed_revision.saturating_add(1);
    store
        .compare_and_swap(assessed_revision, &state)
        .map_err(|error| WorkerError::Transaction(error.to_string()))?;
    let mut revision = state.revision;
    let mut persist = |snapshot: &BootstrapState| -> Result<(), zup_bootstrap::BootstrapError> {
        let mut next = snapshot.clone();
        let expected = revision;
        next.revision = expected
            .checked_add(1)
            .ok_or(zup_bootstrap::BootstrapError::Limit(
                "bootstrap revision overflow",
            ))?;
        store
            .compare_and_swap(expected, &next)
            .map_err(|error| zup_bootstrap::BootstrapError::Provider(error.to_string()))?;
        revision = next.revision;
        Ok(())
    };
    let result = execute_plan_with_persist(
        &bound.plan,
        &satisfier,
        &provider,
        |operation| {
            let artifact = bound.artifacts.get(&operation.id).ok_or_else(|| {
                zup_bootstrap::BootstrapError::MissingArtifact(operation.id.to_string())
            })?;
            quarantine
                .resolve(&artifact.relative_path)
                .map_err(|error| zup_bootstrap::BootstrapError::Provider(error.to_string()))
        },
        &mut state,
        &mut persist,
    );
    state.revision = revision;
    if cancel.is_cancelled() {
        let _ = writer
            .send(&WireEnvelope {
                version: PROTOCOL_VERSION,
                session_id: bootstrap.session_id,
                sequence: outgoing,
                message: Message::Failed(zup_protocol::Failed {
                    kind: "cancelled".into(),
                    message: "bootstrap cancelled".into(),
                }),
            })
            .await;
        return Err(WorkerError::Transaction("bootstrap cancelled".into()));
    }
    if let Err(error) = &result {
        if state.operations.iter().any(|operation| {
            matches!(
                operation.state,
                zup_bootstrap::BootstrapOperationState::Running
                    | zup_bootstrap::BootstrapOperationState::Failed { .. }
            )
        }) {
            state.phase = zup_bootstrap::BootstrapPhase::RecoveryRequired;
        } else {
            state.recompute();
        }
        let old_revision = state.revision;
        state.revision = old_revision.saturating_add(1);
        let _ = store.compare_and_swap(old_revision, &state);
        let _ = writer
            .send(&WireEnvelope {
                version: PROTOCOL_VERSION,
                session_id: bootstrap.session_id,
                sequence: outgoing,
                message: Message::Failed(zup_protocol::Failed {
                    kind: "prerequisite".into(),
                    message: error.to_string(),
                }),
            })
            .await;
        return Err(WorkerError::Transaction(error.to_string()));
    }
    let (outcome, prerequisite_id, exit_code) = match result {
        Ok(zup_bootstrap::BootstrapOutcome::Ready) => ("committed", None, None),
        Ok(zup_bootstrap::BootstrapOutcome::RebootRequired { exit_code, .. }) => {
            let prerequisite_id = state
                .operations
                .iter()
                .find_map(|operation| {
                    if matches!(
                        operation.state,
                        zup_bootstrap::BootstrapOperationState::RebootRequired { .. }
                    ) {
                        Some(operation.id.to_string())
                    } else {
                        None
                    }
                })
                .or_else(|| {
                    bound
                        .plan
                        .operations
                        .first()
                        .map(|operation| operation.id.to_string())
                });
            ("reboot_required", prerequisite_id, Some(exit_code))
        }
        Ok(zup_bootstrap::BootstrapOutcome::RecoveryRequired) => ("recovery_required", None, None),
        Err(error) => return Err(WorkerError::Transaction(error.to_string())),
    };
    let _ = send_and_close(
        writer,
        bootstrap.session_id,
        outgoing,
        Message::Completed(zup_protocol::Completed {
            transaction_id: exec.bootstrap_id,
            outcome: outcome.to_owned(),
            prerequisite_id,
            exit_code,
        }),
    )
    .await;
    let _ = cancel;
    let _ = reader_task.await;
    Ok(outcome.to_owned())
}

fn validate_transaction_bundle_identity(
    app_id: &zup_core::AppId,
    app_version: &semver::Version,
    scope: zup_core::SelectedScope,
    target: &zup_core::TargetTriple,
    bundle: &EmbeddedBundle,
) -> Result<(), WorkerError> {
    validate_transaction_installer(&bundle.plan().installer, app_id, app_version, scope, target)
}

fn validate_transaction_installer(
    installer: &zup_core::Installer,
    app_id: &zup_core::AppId,
    app_version: &semver::Version,
    scope: zup_core::SelectedScope,
    target: &zup_core::TargetTriple,
) -> Result<(), WorkerError> {
    if &installer.app.id != app_id
        || &installer.app.version != app_version
        || &installer.target != target
        || !match scope {
            zup_core::SelectedScope::User => installer.install.scope.allows_user(),
            zup_core::SelectedScope::Machine => installer.install.scope.allows_machine(),
        }
    {
        return Err(WorkerError::AuthFailed(
            "transaction identity is not declared by the worker bundle".into(),
        ));
    }
    Ok(())
}

fn validate_bootstrap_bundle(
    plan: &zup_bootstrap::BootstrapPlan,
    bundle: &EmbeddedBundle,
) -> Result<(), WorkerError> {
    let installer = &bundle.plan().installer;
    if plan.key.app_id != installer.app.id
        || plan.key.app_version != installer.app.version
        || plan.key.target != installer.target
        || !matches!(
            (plan.key.scope, installer.install.scope),
            (zup_core::SelectedScope::User, zup_core::InstallScope::User)
                | (
                    zup_core::SelectedScope::User,
                    zup_core::InstallScope::Either
                )
                | (
                    zup_core::SelectedScope::Machine,
                    zup_core::InstallScope::Machine
                )
                | (
                    zup_core::SelectedScope::Machine,
                    zup_core::InstallScope::Either
                )
        )
    {
        return Err(WorkerError::AuthFailed(
            "bootstrap plan is not declared by the worker bundle".into(),
        ));
    }
    for operation in &plan.operations {
        let declared = installer
            .prerequisites
            .iter()
            .find(|prerequisite| prerequisite.id == operation.id);
        let Some(declared) = declared else {
            return Err(WorkerError::AuthFailed(
                "bootstrap operation is not declared by the worker bundle".into(),
            ));
        };
        if &zup_bootstrap::BootstrapOperation::from_prerequisite(declared) != operation {
            return Err(WorkerError::AuthFailed(
                "bootstrap operation differs from the worker bundle".into(),
            ));
        }
    }
    Ok(())
}

fn decode_overlay_path(value: Option<&str>) -> Result<Option<PathBuf>, WorkerError> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.is_empty()
        || value.len() > zup_protocol::MAX_PAYLOAD_OVERLAY_PATH_BYTES
        || value.contains('\0')
    {
        return Err(WorkerError::Protocol("invalid payload overlay path".into()));
    }
    let path = PathBuf::from(value);
    if !path.is_absolute() {
        return Err(WorkerError::Protocol(
            "payload overlay path must be absolute".into(),
        ));
    }
    Ok(Some(path))
}

fn validate_worker_overlay(
    state_root: &Path,
    scope: zup_core::SelectedScope,
    identity: &PayloadOverlayIdentity,
    actual: Option<&Path>,
    overlay_base_root: Option<&Path>,
    recovery: bool,
    expected_parent_sid: &str,
) -> Result<(), WorkerError> {
    if identity.has_files() {
        let base = overlay_base_root.ok_or_else(|| {
            WorkerError::AuthFailed("generated plugin payload has no overlay base".into())
        })?;
        validate_payload_overlay_base(state_root, scope, base, expected_parent_sid)
            .map_err(|error| WorkerError::AuthFailed(error.to_string()))?;
        let expected = identity.path_under(base).ok_or_else(|| {
            WorkerError::AuthFailed("generated payload identity has no overlay path".into())
        })?;
        let actual = actual.ok_or_else(|| {
            WorkerError::AuthFailed("generated plugin payload has no overlay".into())
        })?;
        if actual != expected {
            return Err(WorkerError::AuthFailed(
                "payload overlay identity mismatch".into(),
            ));
        }
        return Ok(());
    }
    if overlay_base_root.is_some() || actual.is_some() {
        let detail = if recovery {
            "recovery overlay has no generated plugin payload"
        } else {
            "overlay has no generated plugin payload"
        };
        return Err(WorkerError::AuthFailed(detail.into()));
    }
    Ok(())
}

/// Split reader into a background task that cancels on `Cancel` messages.
fn split_reader(
    mut reader: ClientReader,
    cancel: CancellationToken,
    mut sequence: SequenceTracker,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Ok(env) = reader.recv().await {
            if sequence.accept(env.sequence).is_err() {
                break;
            }
            if matches!(env.message, Message::Cancel) {
                cancel.cancel();
            }
        }
        cancel.cancel();
    })
}

async fn send_and_close(
    mut writer: ClientWriter,
    session_id: SessionId,
    sequence: u64,
    message: Message,
) -> Result<(), PipeError> {
    writer
        .send(&WireEnvelope {
            version: PROTOCOL_VERSION,
            session_id,
            sequence,
            message,
        })
        .await
}

/// Worker-side file executor wrapper (cancellation-aware).
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

struct WorkerFileExecutor {
    inner: WindowsFileExecutor<AutoPayloadSource>,
    cancel: TokenProbe,
    progress: tokio::sync::mpsc::UnboundedSender<ProgressReport>,
    completed_work: u64,
    total_work: u64,
    /// Files this plan may mutate, for the Restart Manager preflight a barrier
    /// repeats.
    blocked_paths: Vec<PathBuf>,
}

struct TokenProbe(CancellationToken);

impl CancellationProbe for TokenProbe {
    fn is_cancelled(&self) -> bool {
        self.0.is_cancelled()
    }
}

impl OperationExecutor for WorkerFileExecutor {
    type Error = String;

    fn prepare(&mut self, operation: &TransactionNode) -> Result<(), Self::Error> {
        if self.cancel.is_cancelled() {
            return Err("cancelled".into());
        }
        if !matches!(operation.kind, zup_transaction::NodeKind::Barrier) {
            // Barriers are the only nodes the coordinator preflights; a file
            // node re-checks its own precondition in `apply`.
            return Ok(());
        }
        // The plan orders this barrier immediately before commit intent, so
        // this is the last chance to see a blocker before anything mutates.
        let blockers = self
            .blocked_paths
            .iter()
            .map(|path| path.as_path())
            .collect::<Vec<_>>();
        let blocked = crate::preflight(&blockers).map_err(|error| error.to_string())?;
        let Some(detail) = crate::blocked_reason(&blocked) else {
            return Ok(());
        };
        let _ = self.progress.send(ProgressReport {
            kind: ProgressKind::OperationProgress,
            detail: format!("blocked: {}", detail.replace('\n', ", ")),
            completed: Some(self.completed_work.min(self.total_work)),
            total: Some(self.total_work),
        });
        Err(format!(
            "blocked by running applications: {}",
            detail.replace('\n', ", ")
        ))
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
                crate::integration::verify_managed(receipt).map_err(|e| e.to_string())
            }
            zup_transaction::NodeKind::Barrier => Ok(()),
            _ => crate::verify_installed_file(receipt).map_err(|e| e.to_string()),
        }
    }

    fn apply(&mut self, operation: &TransactionNode) -> Result<OperationReceipt, Self::Error> {
        if self.cancel.is_cancelled() {
            return Err("cancelled".into());
        }
        let receipt = match &operation.kind {
            zup_transaction::NodeKind::Barrier => Ok(OperationReceipt::Control),
            zup_transaction::NodeKind::BackendOperation { .. } => {
                crate::integration::apply_managed(operation).map_err(|e| e.to_string())
            }
            zup_transaction::NodeKind::BackendRemoval { .. } => {
                crate::integration::apply_owned_removal(operation).map_err(|e| e.to_string())
            }
            zup_transaction::NodeKind::FileRemoval { .. } => self
                .inner
                .apply_owned_file_removal(operation)
                .map_err(|e| e.to_string()),
            zup_transaction::NodeKind::StageFile { .. }
            | zup_transaction::NodeKind::FileMutation { .. } => {
                let (rel, dest) = extract_file(operation)?;
                apply_node(&mut self.inner, operation, &rel, &dest)
                    .map(crate::transaction_receipt)
                    .map_err(|e| e.to_string())
            }
        }?;

        self.completed_work = self
            .completed_work
            .saturating_add(operation_work(operation));
        let _ = self.progress.send(ProgressReport {
            kind: ProgressKind::OperationProgress,
            detail: operation_action(operation),
            completed: Some(self.completed_work.min(self.total_work)),
            total: Some(self.total_work),
        });
        Ok(receipt)
    }

    fn rollback(
        &mut self,
        _op: &TransactionNode,
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
            _ => crate::integration::rollback_managed(receipt).map_err(|e| e.to_string()),
        }
    }

    fn reconcile(
        &mut self,
        op: &TransactionNode,
        _receipt: Option<&OperationReceipt>,
    ) -> Result<ReconcileResult, Self::Error> {
        match &op.kind {
            zup_transaction::NodeKind::BackendOperation { .. } => {
                return crate::integration::reconcile_managed(op).map_err(|e| e.to_string());
            }
            zup_transaction::NodeKind::BackendRemoval { .. } => {
                return crate::integration::reconcile_owned_removal(op).map_err(|e| e.to_string());
            }
            zup_transaction::NodeKind::FileRemoval { .. } => {
                return self
                    .inner
                    .reconcile_owned_file_removal(op)
                    .map_err(|e| e.to_string());
            }
            _ => {}
        }
        self.inner
            .reconcile_transaction_node(op)
            .map_err(|e| e.to_string())
    }
}

fn extract_file(
    operation: &TransactionNode,
) -> Result<(zup_core::RelativePath, std::path::PathBuf), String> {
    match &operation.kind {
        zup_transaction::NodeKind::StageFile { key }
        | zup_transaction::NodeKind::FileMutation { key, .. } => {
            let destination = match key {
                zup_core::ResourceKey::File { destination }
                | zup_core::ResourceKey::Maintenance { destination, .. } => destination,
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

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use zup_core::{AppId, RelativePath, SelectedScope, Sha256Digest};

    fn machine_base_for_sid(sid: &str) -> PathBuf {
        let mut hasher = Sha256::new();
        hasher.update(sid.as_bytes());
        let key = Sha256Digest::from_hasher(hasher).to_hex();
        PathBuf::from(r"C:\Users\Parent\AppData\Local\Temp")
            .join(format!("zup-payload-overlays-{}", &key[..32]))
    }

    fn identity_with_file() -> PayloadOverlayIdentity {
        PayloadOverlayIdentity::new(
            AppId::new("com.example.overlay-auth").unwrap(),
            semver::Version::parse("1.0.0").unwrap(),
            SelectedScope::Machine,
            std::iter::empty(),
            [crate::payload_overlay::PayloadOverlayFileIdentity {
                source_relative: RelativePath::new("__zup_plugins__/generated.bin").unwrap(),
                size: 1,
                sha256: Sha256Digest::from_bytes([0; 32]),
            }],
        )
        .unwrap()
    }

    #[test]
    fn uses_authenticated_parent_sid_for_machine_overlay_base() {
        let parent_sid = "S-1-5-21-1111111111-2222222222-3333333333";
        let state_root = PathBuf::from(r"C:\state");
        let identity = identity_with_file();
        let base = machine_base_for_sid(parent_sid);
        let overlay = identity.path_under(&base).unwrap();

        assert!(
            validate_worker_overlay(
                &state_root,
                SelectedScope::Machine,
                &identity,
                Some(&overlay),
                Some(&base),
                false,
                parent_sid,
            )
            .is_ok()
        );
        assert!(
            validate_worker_overlay(
                &state_root,
                SelectedScope::Machine,
                &identity,
                Some(&overlay),
                Some(&base),
                false,
                "S-1-5-21-9999999999-8888888888-7777777777",
            )
            .is_err()
        );
    }

    #[test]
    fn rejects_overlay_or_base_without_generated_files() {
        let parent_sid = "S-1-5-21-1111111111-2222222222-3333333333";
        let state_root = PathBuf::from(r"C:\state");
        let identity = PayloadOverlayIdentity::new(
            AppId::new("com.example.overlay-auth").unwrap(),
            semver::Version::parse("1.0.0").unwrap(),
            SelectedScope::Machine,
            std::iter::empty(),
            std::iter::empty(),
        )
        .unwrap();
        let base = machine_base_for_sid(parent_sid);

        assert!(
            validate_worker_overlay(
                &state_root,
                SelectedScope::Machine,
                &identity,
                Some(&base),
                Some(&base),
                false,
                parent_sid,
            )
            .is_err()
        );
        assert!(
            validate_worker_overlay(
                &state_root,
                SelectedScope::Machine,
                &identity,
                None,
                Some(&base),
                false,
                parent_sid,
            )
            .is_err()
        );
    }

    #[test]
    fn user_overlay_base_must_equal_state_root() {
        let state_root = PathBuf::from(r"C:\state");
        assert!(
            validate_payload_overlay_base(
                &state_root,
                SelectedScope::User,
                &state_root,
                "S-1-5-21-1111111111-2222222222-3333333333",
            )
            .is_ok()
        );
        assert!(
            validate_payload_overlay_base(
                &state_root,
                SelectedScope::User,
                &state_root.join("other"),
                "S-1-5-21-1111111111-2222222222-3333333333",
            )
            .is_err()
        );
    }

    #[test]
    fn transaction_identity_must_match_bundle_installer() {
        let installer = zup_manifest::parse_and_compile(
            r#"
            schema = 1
            [app]
            id = "com.example.app"
            name = "Example"
            version = "1.0.0"
            [build]
            [build.targets.default]
            target = "x86_64-pc-windows-msvc"
            source = { directory = "dist" }
            [install]
            scope = "user"
            [install.directory]
            user = "${location.user_data}/Example"
            "#,
            "default",
        )
        .unwrap();
        let app_id = installer.app.id.clone();
        let app_version = installer.app.version.clone();
        let target = installer.target.clone();
        assert!(
            validate_transaction_installer(
                &installer,
                &app_id,
                &app_version,
                SelectedScope::User,
                &target,
            )
            .is_ok()
        );
        assert!(matches!(
            validate_transaction_installer(
                &installer,
                &zup_core::AppId::new("com.example.other").unwrap(),
                &app_version,
                SelectedScope::User,
                &target,
            ),
            Err(WorkerError::AuthFailed(_))
        ));
        assert!(matches!(
            validate_transaction_installer(
                &installer,
                &app_id,
                &semver::Version::new(2, 0, 0),
                SelectedScope::User,
                &target,
            ),
            Err(WorkerError::AuthFailed(_))
        ));
        assert!(matches!(
            validate_transaction_installer(
                &installer,
                &app_id,
                &app_version,
                SelectedScope::Machine,
                &target,
            ),
            Err(WorkerError::AuthFailed(_))
        ));
    }
}
