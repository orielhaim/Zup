use std::path::{Path, PathBuf};
use zup_plan::DeploymentNode;
use zup_transaction::TransactionStore;

use thiserror::Error;
use zup_bootstrap::{
    BootstrapId, BootstrapState, BootstrapStateStore, BoundBootstrapPlan,
    FilesystemBootstrapStateStore, Quarantine, execute_plan_with_persist, recover,
};

use crate::{AutoPayloadSource, EmbeddedBundle};
use tokio_util::sync::CancellationToken;
use zup_core::SessionId;
use zup_core::Target;
use zup_protocol::{
    Capabilities, Message, PROTOCOL_VERSION, ProgressKind, ProgressReport, SequenceTracker,
    WireEnvelope, WorkerHello,
};
use zup_transaction::{
    CancellationProbe, FilesystemTransactionStore, OperationExecutor, OperationReceipt,
    ReconcileResult, TransactionCoordinator, TransactionOutcome,
};

use crate::file_executor::{WindowsFileExecutor, apply_node};
use crate::payload_overlay::{
    PayloadOverlayIdentity, cleanup_payload_overlay, validate_payload_overlay_base,
};
use zup_transaction::{LockScope, ResourceLock};

use crate::pipe::{ClientReader, ClientWriter, HANDSHAKE_TIMEOUT, PipeError, frame_client};
use crate::transport::verify_server_pid;

fn drop_overlay(peer: &crate::peer_access::PeerAccess, base: Option<&Path>, root: Option<&Path>) {
    if let (Some(base), Some(root)) = (base, root)
        && let Ok(_guard) = peer.enter()
    {
        let _ = cleanup_payload_overlay(base, Some(root));
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerBootstrap {
    pub protocol_version: u32,
    pub session_id: SessionId,
    pub pipe_name: String,
    pub expected_parent_pid: u32,
    pub expected_parent_sid: String,
    pub target: Target,
    pub expected_request_digest: String,
}

#[derive(Debug, Error)]
pub enum WorkerError {
    #[error("transaction {transaction} remains pending: {detail}")]
    RecoverySourceUnavailable {
        transaction: zup_core::TransactionId,
        detail: String,
    },
    #[error("unfinished transaction {0} requires recovery")]
    RecoveryRequired(zup_core::TransactionId),
    #[error("transaction preparation cancelled")]
    Cancelled,
    #[error("invalid bootstrap: {0}")]
    InvalidBootstrap(String),

    #[error("authentication failed: {0}")]
    AuthFailed(String),

    #[error("authenticated request digest mismatch")]
    RequestDigestMismatch,

    #[error("target mismatch")]
    TargetMismatch,

    #[error("deployment changed or requested effects differ from authenticated content")]
    PlanChanged,

    #[error("capability missing: {0}")]
    MissingCapability(String),

    #[error("protocol error: {0}")]
    Protocol(String),

    #[error("transaction failed: {0}")]
    Transaction(String),

    #[error("another operation is running for this installation")]
    Busy,

    #[error("parent disconnect")]
    ParentDisconnect,
}

impl WorkerError {
    fn failure_kind(&self) -> zup_protocol::WorkerFailure {
        use zup_protocol::WorkerFailure;
        match self {
            Self::RecoveryRequired(transaction)
            | Self::RecoverySourceUnavailable { transaction, .. } => {
                WorkerFailure::RecoveryRequired {
                    transaction: Some(*transaction),
                }
            }
            Self::Cancelled => WorkerFailure::Cancelled,
            Self::AuthFailed(_) => WorkerFailure::Authentication,
            Self::PlanChanged => WorkerFailure::StalePlan,
            Self::Busy => WorkerFailure::InstallationBusy,
            Self::Transaction(_) => WorkerFailure::Transaction,
            Self::InvalidBootstrap(_)
            | Self::RequestDigestMismatch
            | Self::TargetMismatch
            | Self::MissingCapability(_)
            | Self::Protocol(_)
            | Self::ParentDisconnect => WorkerFailure::Protocol,
        }
    }
}

pub fn worker_capabilities() -> Capabilities {
    Capabilities {
        file_transactions_v1: true,
        backend_operations_v1: true,
        lifecycle_v1: true,
        prerequisite_bootstrap_v1: true,
    }
}

pub fn parse_bootstrap(arg: &str) -> Result<WorkerBootstrap, WorkerError> {
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
    let target = Target::parse(parts[5])
        .map_err(|error| WorkerError::InvalidBootstrap(error.to_string()))?;
    let expected_request_digest = parts[6].to_owned();
    if expected_request_digest.len() != 64
        || !expected_request_digest
            .chars()
            .all(|c| c.is_ascii_hexdigit())
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
        expected_request_digest: expected_request_digest.to_lowercase(),
    })
}

pub fn format_bootstrap(bootstrap: &WorkerBootstrap) -> String {
    format!(
        "{}|{}|{}|{}|{}|{}|{}",
        bootstrap.protocol_version,
        bootstrap.session_id,
        bootstrap.pipe_name,
        bootstrap.expected_parent_pid,
        bootstrap.expected_parent_sid,
        bootstrap.target,
        bootstrap.expected_request_digest
    )
}

pub fn request_digest_hex(plan_json: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(plan_json.as_bytes());
    let digest = zup_core::Sha256Digest::from_hasher(hasher);
    digest.to_hex()
}

pub fn current_exe() -> Result<PathBuf, WorkerError> {
    std::env::current_exe().map_err(|e| WorkerError::InvalidBootstrap(e.to_string()))
}

pub async fn run_worker(
    bootstrap: WorkerBootstrap,
    cancel: CancellationToken,
) -> Result<zup_protocol::WorkerOutcome, WorkerError> {
    if !crate::transport::is_process_elevated()
        .map_err(|e| WorkerError::AuthFailed(e.to_string()))?
    {
        return Err(WorkerError::AuthFailed("worker is not elevated".into()));
    }
    run_worker_inner(bootstrap, cancel, zup_core::SelectedScope::Machine).await
}

#[doc(hidden)]
#[cfg(feature = "test-launcher")]
pub async fn run_worker_for_test(
    bootstrap: WorkerBootstrap,
    cancel: CancellationToken,
    authorized_scope: zup_core::SelectedScope,
) -> Result<zup_protocol::WorkerOutcome, WorkerError> {
    run_worker_inner(bootstrap, cancel, authorized_scope).await
}

async fn run_worker_inner(
    bootstrap: WorkerBootstrap,
    cancel: CancellationToken,
    authorized_scope: zup_core::SelectedScope,
) -> Result<zup_protocol::WorkerOutcome, WorkerError> {
    let client = crate::pipe::PipeClient::connect(&bootstrap.pipe_name)
        .await
        .map_err(|e| WorkerError::Protocol(e.to_string()))?;

    verify_server_pid(client.as_raw() as isize, bootstrap.expected_parent_pid)
        .map_err(|e| WorkerError::AuthFailed(e.to_string()))?;
    let peer = crate::peer_access::PeerAccess::for_process(
        bootstrap.expected_parent_pid,
        &bootstrap.expected_parent_sid,
    )
    .map_err(|error| WorkerError::AuthFailed(error.to_string()))?;

    let (mut reader, mut writer) = frame_client(client.into_inner());
    let mut incoming = SequenceTracker::new();
    let mut outgoing: u64 = 0;

    let hello = WireEnvelope {
        version: PROTOCOL_VERSION,
        session_id: bootstrap.session_id,
        sequence: 0,
        message: Message::WorkerHello(WorkerHello {
            protocol_version: PROTOCOL_VERSION,
            session_id: bootstrap.session_id,
            target: bootstrap.target,
            worker_pid: std::process::id(),
            capabilities: worker_capabilities(),
        }),
    };
    writer
        .send(&hello)
        .await
        .map_err(|e| WorkerError::Protocol(e.to_string()))?;
    outgoing = outgoing.saturating_add(1);

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
                || hello.expected_request_digest != bootstrap.expected_request_digest
            {
                return Err(WorkerError::AuthFailed("parent hello mismatch".into()));
            }
        }
        _ => return Err(WorkerError::Protocol("expected ParentHello".into())),
    }

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
        Message::PrepareTransaction(exec) => exec,
        Message::Cancel => return Err(WorkerError::Cancelled),
        _ => {
            return Err(WorkerError::Protocol(
                "expected PrepareTransaction or ExecuteBootstrap".into(),
            ));
        }
    };

    let recovering = matches!(
        exec.operation,
        zup_protocol::PreparedTransactionRequest::Recovery { .. }
    );
    let (execute_tx, execute_rx) = tokio::sync::oneshot::channel();
    let reader_task = split_reader(reader, cancel.clone(), incoming, Some(execute_tx));
    let preparation = async {
        if exec.target != bootstrap.target {
            return Err(WorkerError::TargetMismatch);
        }
        let hash = exec
            .digest()
            .map_err(|error| WorkerError::Protocol(error.to_string()))?;
        if hash != bootstrap.expected_request_digest {
            return Err(WorkerError::RequestDigestMismatch);
        }
        let recovery_id = match &exec.operation {
            zup_protocol::PreparedTransactionRequest::Recovery { transaction } => {
                Some(*transaction)
            }
            _ => None,
        };
        let intent = match &exec.operation {
            zup_protocol::PreparedTransactionRequest::Deployment { intent, .. } => Some(intent),
            _ => None,
        };
        let mut plan = match &exec.operation {
            zup_protocol::PreparedTransactionRequest::Deployment { plan, .. } => {
                if plan.target != exec.target {
                    return Err(WorkerError::TargetMismatch);
                }
                plan.validate()
                    .map_err(|error| WorkerError::Protocol(format!("invalid plan: {error}")))?;
                Some(plan.as_ref().clone())
            }
            zup_protocol::PreparedTransactionRequest::Recovery { .. } => None,
        };

        let app_id = zup_core::AppId::new(&exec.app_id)
            .map_err(|e| WorkerError::Protocol(format!("bad app id: {e}")))?;
        let mut app_version: semver::Version = exec
            .app_version
            .parse()
            .map_err(|e| WorkerError::Protocol(format!("bad app version: {e}")))?;
        let scope = match exec.scope.as_str() {
            "user" => zup_core::SelectedScope::User,
            "machine" => zup_core::SelectedScope::Machine,
            _ => return Err(WorkerError::Protocol("invalid install scope".into())),
        };
        if scope != authorized_scope {
            return Err(WorkerError::AuthFailed(
                "transaction scope exceeds worker authority".into(),
            ));
        }
        {
            let executable =
                current_exe().map_err(|error| WorkerError::AuthFailed(error.to_string()))?;
            let bundle = EmbeddedBundle::open(&executable)
                .map_err(|error| WorkerError::AuthFailed(format!("worker bundle: {error}")))?;
            validate_transaction_bundle_identity(
                &app_id,
                if recovering
                    || exec.content.is_some()
                    || intent.is_some_and(|intent| {
                        matches!(
                            intent.action,
                            zup_plan::LifecycleAction::Rollback { .. }
                                | zup_plan::LifecycleAction::Cleanup { .. }
                        )
                    })
                {
                    &bundle.plan().installer.app.version
                } else {
                    &app_version
                },
                scope,
                &bootstrap.target,
                &bundle,
            )?;
        }
        let machine = if scope == zup_core::SelectedScope::Machine {
            Some(
                crate::machine_authority::MachineRoot::acquire(&app_id)
                    .map_err(|error| WorkerError::AuthFailed(error.to_string()))?,
            )
        } else {
            None
        };
        let state_root = match &machine {
            Some(machine) => machine.path().to_path_buf(),
            None => PathBuf::from(exec.state_root),
        };
        let store = match &machine {
            Some(machine) => machine.transaction_store(),
            None => FilesystemTransactionStore::private_at(&state_root),
        };
        let lock_key = ResourceLock::lock_key(app_id.as_str(), &exec.scope);
        let _lock = match &machine {
            Some(machine) => machine
                .try_lock(&lock_key)
                .map_err(|error| WorkerError::Transaction(error.to_string()))?,
            None => ResourceLock::try_acquire(&state_root, &lock_key)
                .map_err(|error| WorkerError::Transaction(error.to_string()))?,
        }
        .ok_or_else(|| WorkerError::Transaction("installation busy".into()))?;
        if recovering
            && (exec.content.is_some()
                || exec.release.is_some()
                || exec.payload_overlay_root.is_some()
                || exec.payload_overlay_base_root.is_some()
                || !exec.payload_root.is_empty())
        {
            return Err(WorkerError::Protocol(
                "recovery source is owned by the protected transaction".into(),
            ));
        }
        let payload_overlay_root = decode_overlay_path(exec.payload_overlay_root.as_deref())?;
        let payload_overlay_base_root =
            decode_overlay_path(exec.payload_overlay_base_root.as_deref())?;
        let existing_record = if let Some(id) = recovery_id {
            let record = zup_transaction::TransactionStore::load(&store, &id)
                .map_err(|e| WorkerError::Transaction(e.to_string()))?;
            if record.app_id != app_id || record.scope != scope || record.target != exec.target {
                return Err(WorkerError::AuthFailed("recovery record mismatch".into()));
            }
            if let Some(plan) = &plan {
                if record.plan != *plan || record.target != plan.target {
                    return Err(WorkerError::AuthFailed("recovery record mismatch".into()));
                }
            } else {
                plan = Some(record.plan.clone());
            }
            app_version = record.app_version.clone();
            Some(record)
        } else {
            let plan = plan
                .as_ref()
                .ok_or_else(|| WorkerError::Protocol("missing transaction plan".into()))?;
            let identity = PayloadOverlayIdentity::from_transaction(
                app_id.clone(),
                app_version.clone(),
                scope,
                plan,
            )
            .map_err(|error| WorkerError::AuthFailed(error.to_string()))?;
            validate_worker_overlay(
                &state_root,
                scope,
                &identity,
                payload_overlay_root.as_deref(),
                payload_overlay_base_root.as_deref(),
                &bootstrap.expected_parent_sid,
            )?;
            None
        };
        let mut release = existing_record
            .as_ref()
            .and_then(|record| record.release.clone());
        let payload_root = PathBuf::from(exec.payload_root);
        let executable =
            current_exe().map_err(|error| WorkerError::AuthFailed(error.to_string()))?;
        let authority = EmbeddedBundle::open(&executable)
            .map_err(|error| WorkerError::AuthFailed(error.to_string()))?;
        let acquired = if let Some(source) = &exec.content {
            if source.handoff.scope != exec.scope
                || source.handoff.app_id != app_id
                || source.handoff.target != exec.target
            {
                return Err(WorkerError::AuthFailed(
                    "acquired content context mismatch".into(),
                ));
            }
            let source = crate::acquired::AuthenticatedSource::open(
                source,
                &payload_root,
                &state_root,
                &authority,
                &peer,
                machine.as_ref(),
            )
            .await
            .map_err(|error| WorkerError::AuthFailed(error.to_string()))?;
            if exec.release.as_ref() != Some(&source.release) {
                return Err(WorkerError::AuthFailed(
                    "acquired release identity mismatch".into(),
                ));
            }
            Some(source)
        } else {
            None
        };
        let source_image = acquired
            .as_ref()
            .map_or(executable.as_path(), |source| source.image.as_path());
        let source_bundle = acquired
            .as_ref()
            .map_or(&authority, |source| &source.bundle);
        let payload = if let Some(record) = &existing_record {
            crate::runtime::SharedPayload(std::sync::Arc::new(
                crate::recovery::RetainedSource::load(&state_root, record).map_err(|detail| {
                    WorkerError::RecoverySourceUnavailable {
                        transaction: record.transaction_id,
                        detail,
                    }
                })?,
            ))
        } else {
            let base_payload = match &acquired {
                Some(source) => AutoPayloadSource::Acquired(std::sync::Arc::new(
                    crate::acquired::AcquiredDeploymentSource::new(
                        std::sync::Arc::clone(&source.payload),
                        source.image.clone(),
                        source.runtime,
                        source.manifest.clone(),
                    ),
                )),
                None => {
                    let _guard = peer
                        .enter()
                        .map_err(|error| WorkerError::AuthFailed(error.to_string()))?;
                    AutoPayloadSource::from_path(payload_root.clone()).map_err(|error| {
                        WorkerError::Transaction(format!("payload package: {error}"))
                    })?
                }
            };
            let live = crate::peer_access::PeerPayloadSource {
                source: AutoPayloadSource::Overlay(zup_bundle::OverlayPayloadSource::new(
                    base_payload,
                    payload_overlay_root.clone(),
                )),
                peer: peer.clone(),
            };
            crate::runtime::SharedPayload(std::sync::Arc::new(live))
        };

        let mut authorized = plan
            .clone()
            .ok_or_else(|| WorkerError::Protocol("missing transaction plan".into()))?;
        if recovery_id.is_none() {
            crate::ledger::InstallationRecordStore::new(&state_root)
                .repair_committed(&app_id, scope)
                .map_err(|error| match error {
                    crate::ledger::LedgerError::RecoveryRequired(transaction) => {
                        WorkerError::RecoveryRequired(transaction)
                    }
                    error => WorkerError::Transaction(error.to_string()),
                })?;
            let intent = intent.ok_or_else(|| {
                WorkerError::AuthFailed("fresh execution requires deployment intent".into())
            })?;
            validate_transaction_bundle_identity(
                &app_id,
                if matches!(
                    intent.action,
                    zup_plan::LifecycleAction::Rollback { .. }
                        | zup_plan::LifecycleAction::Cleanup { .. }
                ) {
                    &source_bundle.plan().installer.app.version
                } else {
                    &app_version
                },
                scope,
                &bootstrap.target,
                source_bundle,
            )?;
            let build = source_bundle
                .package()
                .build_plan()
                .map_err(|error| WorkerError::AuthFailed(error.to_string()))?;
            let build = build
                .targets
                .first()
                .ok_or_else(|| WorkerError::AuthFailed("worker bundle has no target".into()))?;
            let ledger = crate::ledger::InstallationRecordStore::new(&state_root)
                .load(&app_id, scope)
                .map_err(|error| WorkerError::AuthFailed(error.to_string()))?;
            if matches!(
                intent.action,
                zup_plan::LifecycleAction::Repair { .. }
                    | zup_plan::LifecycleAction::Modify
                    | zup_plan::LifecycleAction::Rollback { .. }
            ) && let Some(source) = &acquired
            {
                let expected = match intent.action {
                    zup_plan::LifecycleAction::Rollback { revision } => ledger
                        .as_ref()
                        .and_then(|record| {
                            record
                                .retained
                                .iter()
                                .find(|snapshot| snapshot.revision == revision)
                        })
                        .and_then(|snapshot| snapshot.release.as_ref()),
                    _ => ledger
                        .as_ref()
                        .and_then(zup_plan::InstallationRecord::release_identity),
                }
                .ok_or_else(|| {
                    WorkerError::AuthFailed(
                        "maintenance requires the selected release's authenticated source identity"
                            .into(),
                    )
                })?;
                source
                    .release
                    .verify_source(expected)
                    .map_err(|error| WorkerError::AuthFailed(error.to_string()))?;
            }
            let host = crate::operation_host::WindowsOperationHost {
                app_id: app_id.clone(),
                scope,
                frontend: source_bundle.frontend(),
                state_root: state_root.clone(),
                content: match &acquired {
                    Some(source) => crate::operation_host::WindowsContent::Acquired(
                        &source.manifest,
                        &source.payload,
                    ),
                    None => crate::operation_host::WindowsContent::Embedded(source_bundle),
                },
                acquire_prerequisites: false,
            };
            let host = crate::peer_access::PeerOperationHost {
                inner: host,
                peer: &peer,
            };
            let prepared = zup_runtime::prepare_operation(
                &zup_runtime::OperationInput {
                    build,
                    installer: source_image,
                    scope,
                    operation: intent.action.into(),
                    selection: zup_runtime::OperationSelection {
                        deployment: Some(intent.deployment.clone()),
                        enable: intent.enable.clone(),
                        disable: intent.disable.clone(),
                        install_directory: intent.install_directory.clone(),
                    },
                    prior: ledger.as_ref(),
                    state_root: &state_root,
                    cancellation: &zup_plan::NeverCancelled,
                },
                &host,
            )
            .map_err(|error| match error {
                zup_runtime::OperationError::RecoveryRequired(transaction) => {
                    WorkerError::RecoveryRequired(transaction)
                }
                error => WorkerError::AuthFailed(error.to_string()),
            })?;
            release = match &prepared.body {
                zup_runtime::OperationBody::Retained { release, .. }
                | zup_runtime::OperationBody::Cleanup { release } => release.clone(),
                zup_runtime::OperationBody::Planned(_) | zup_runtime::OperationBody::Uninstall => {
                    acquired.as_ref().map(|source| source.release.clone())
                }
            };
            if exec.release != release {
                return Err(WorkerError::AuthFailed(
                    "release provenance differs from authenticated source authority".into(),
                ));
            }

            if prepared.transaction_plan.requirements != authorized.requirements
                || prepared.transaction_plan.install_directory != authorized.install_directory
                || prepared.transaction_plan.transition.is_remove()
                    != authorized.transition.is_remove()
            {
                return Err(WorkerError::PlanChanged);
            }
            authorized = prepared.transaction_plan;
            crate::ledger::InstallationRecordStore::new(&state_root)
                .validate_plan(&app_id, scope, &authorized)
                .map_err(|e| WorkerError::Transaction(e.to_string()))?;
        }

        if cancel.is_cancelled() {
            return Err(WorkerError::Cancelled);
        }
        Ok::<_, WorkerError>((
            authorized,
            existing_record,
            release,
            store,
            payload,
            _lock,
            state_root,
            scope,
            payload_overlay_base_root,
            payload_overlay_root,
            acquired,
        ))
    };
    let prepared = tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(WorkerError::Cancelled),
        result = preparation => result,
    };
    let (
        authorized,
        existing_record,
        release,
        store,
        payload,
        _lock,
        state_root,
        scope,
        payload_overlay_base_root,
        payload_overlay_root,
        _acquired_source,
    ) = match prepared {
        Ok(prepared) => prepared,
        Err(error) => {
            reader_task.abort();
            let _ = writer
                .send(&WireEnvelope {
                    version: PROTOCOL_VERSION,
                    session_id: bootstrap.session_id,
                    sequence: outgoing,
                    message: Message::Failed(zup_protocol::Failed {
                        kind: error.failure_kind(),
                        message: error.to_string(),
                    }),
                })
                .await;
            return Err(error);
        }
    };
    let digest = authorized.fingerprint().to_hex();
    writer
        .send(&WireEnvelope {
            version: PROTOCOL_VERSION,
            session_id: bootstrap.session_id,
            sequence: outgoing,
            message: Message::Prepared(zup_protocol::PreparedOperation {
                plan: Box::new(authorized.clone()),
                plan_digest: digest.clone(),
                operation: if recovering { "recover" } else { "deploy" }.into(),
                app_id: authorized.requirements.app.id.to_string(),
                app_version: authorized.requirements.app.version.to_string(),
                scope: scope.to_string(),
                target: authorized.target,
                file_count: authorized
                    .nodes
                    .iter()
                    .filter(|node| matches!(node.kind, zup_plan::NodeKind::FileMutation { .. }))
                    .count()
                    .try_into()
                    .map_err(|_| {
                        WorkerError::Protocol("plan file count exceeds its limit".into())
                    })?,
            }),
        })
        .await
        .map_err(|error| WorkerError::Protocol(error.to_string()))?;
    outgoing = outgoing.saturating_add(1);
    let execute = tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(WorkerError::Cancelled),
        result = tokio::time::timeout(std::time::Duration::from_secs(600), execute_rx) => match result {
            Ok(Ok(execute)) if execute.plan_digest == digest => Ok(execute),
            Ok(Ok(_)) => Err(WorkerError::RequestDigestMismatch),
            Ok(Err(_)) => Err(WorkerError::ParentDisconnect),
            Err(_) => Err(WorkerError::Protocol("prepared plan execution timeout".into())),
        },
    };
    if let Err(error) = execute {
        reader_task.abort();
        let _ = writer
            .send(&WireEnvelope {
                version: PROTOCOL_VERSION,
                session_id: bootstrap.session_id,
                sequence: outgoing,
                message: Message::Failed(zup_protocol::Failed {
                    kind: error.failure_kind(),
                    message: error.to_string(),
                }),
            })
            .await;
        return Err(error);
    }
    let fresh = if existing_record.is_none() {
        Some(
            TransactionCoordinator::new(store.clone())
                .begin(
                    authorized.requirements.app.id.clone(),
                    scope,
                    authorized.requirements.app.version.clone(),
                    authorized,
                )
                .and_then(|open| open.with_release(release))
                .map_err(|error| WorkerError::Transaction(error.to_string()))?,
        )
    } else {
        None
    };
    let record = match &fresh {
        Some(open) => open.record().clone(),
        None => existing_record.expect("recovery loads its protected journal"),
    };
    let payload = if fresh.is_some() {
        match crate::recovery::RetainedSource::capture(&state_root, &record, &payload) {
            Ok(retained) => crate::runtime::SharedPayload(std::sync::Arc::new(retained)),
            Err(detail) => {
                let error = WorkerError::RecoverySourceUnavailable {
                    transaction: record.transaction_id,
                    detail,
                };
                reader_task.abort();
                let _ = send_and_close(
                    writer,
                    bootstrap.session_id,
                    outgoing,
                    Message::Failed(zup_protocol::Failed {
                        kind: error.failure_kind(),
                        message: error.to_string(),
                    }),
                )
                .await;
                return Err(error);
            }
        }
    } else {
        payload
    };
    drop_overlay(
        &peer,
        payload_overlay_base_root.as_deref(),
        payload_overlay_root.as_deref(),
    );
    writer
        .send(&WireEnvelope {
            version: PROTOCOL_VERSION,
            session_id: bootstrap.session_id,
            sequence: outgoing,
            message: Message::Progress(ProgressReport {
                blocked_pids: Vec::new(),
                phase: zup_core::OperationPhase::Prepare,
                kind: ProgressKind::OperationStarted,
                detail: "transaction accepted".into(),
                completed: None,
                total: None,
            }),
        })
        .await
        .map_err(|e| WorkerError::Protocol(e.to_string()))?;
    outgoing = outgoing.saturating_add(1);

    let total_work = record.plan.total_work();
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

    let executor_cancel = TokenProbe(cancel.clone());
    let executor_progress = progress_tx.clone();
    let journal_id = record.transaction_id;

    let publisher = crate::ledger::InstallationRecordStore::new(&state_root);
    let executor_state_root = state_root.clone();
    let cleanup_store = store.clone();
    let result = tokio::task::spawn_blocking(move || {
        let mut executor = WorkerFileExecutor {
            lifecycle: crate::service_control::ScmLifecycle::default(),
            inner: WindowsFileExecutor::new(
                payload,
                record.transaction_id,
                record.target.executable_suffix(),
            )
            .with_state_authority(executor_state_root, record.scope, &record.app_id),
            cancel: executor_cancel,
            progress: executor_progress,
            completed_work: 0,
            total_work,
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
        if recovering {
            zup_transaction::recover(record, &store, &mut executor, &publisher)
        } else {
            fresh
                .expect("a fresh transaction")
                .execute(&mut executor, &publisher)
                .map(|settled| (settled.record, settled.outcome))
        }
    })
    .await;
    let result = match result {
        Ok(inner) => inner,
        Err(_) => Err(zup_transaction::TransactionError::InvalidState(
            journal_id.to_string(),
        )),
    };

    drop(progress_tx);
    let progress_ended = progress_task.await;

    let (transaction_id, outcome, detail) = match result {
        Ok((r, TransactionOutcome::Committed)) => {
            drop_overlay(
                &peer,
                payload_overlay_base_root.as_deref(),
                payload_overlay_root.as_deref(),
            );
            let ledgers = crate::ledger::InstallationRecordStore::new(&state_root);
            zup_transaction::fault_injection::boundary(
                zup_transaction::fault_injection::Boundary::ReconciliationBefore,
                &r.transaction_id.to_string(),
            );
            let publication = if recovering {
                ledgers.repair_committed(&r.app_id, scope)
            } else {
                ledgers.reconcile_installation(&r, scope).map(|_| ())
            };
            let (outcome, detail) = match publication {
                Ok(()) => {
                    zup_transaction::fault_injection::boundary(
                        zup_transaction::fault_injection::Boundary::Reconciled,
                        &r.transaction_id.to_string(),
                    );
                    crate::integration::notify_committed_path_change(&r);
                    (zup_protocol::WorkerOutcome::Committed, None)
                }
                Err(error) => (
                    zup_protocol::WorkerOutcome::CommittedPendingReconciliation,
                    Some(format!(
                        "Deployment committed; ownership reconciliation requires recovery: {error}"
                    )),
                ),
            };
            (r.transaction_id.as_uuid(), outcome, detail)
        }
        Ok((r, TransactionOutcome::RolledBack)) => {
            drop_overlay(
                &peer,
                payload_overlay_base_root.as_deref(),
                payload_overlay_root.as_deref(),
            );
            (
                r.transaction_id.as_uuid(),
                zup_protocol::WorkerOutcome::RolledBack,
                r.failure.as_ref().map(|failure| failure.message.clone()),
            )
        }
        Ok((r, TransactionOutcome::RecoveryRequired)) => (
            r.transaction_id.as_uuid(),
            if r.phase == zup_transaction::TransactionPhase::Committed {
                zup_protocol::WorkerOutcome::CommittedPendingReconciliation
            } else {
                zup_protocol::WorkerOutcome::RecoveryRequired
            },
            Some(r.recovery_detail()),
        ),
        Err(error) => (
            journal_id.as_uuid(),
            zup_protocol::WorkerOutcome::RecoveryRequired,
            Some(error.to_string()),
        ),
    };
    if let Ok(record) = cleanup_store.load(&zup_core::TransactionId::from_uuid(transaction_id)) {
        let _ = crate::recovery::collect(&state_root, &record);
    }
    drop(_lock);

    if let Ok(Ok((writer, outgoing))) = progress_ended {
        let _ = send_and_close(
            writer,
            bootstrap.session_id,
            outgoing,
            Message::Completed(zup_protocol::Completed {
                detail,
                transaction_id,
                outcome,
                prerequisite_id: None,
                exit_code: None,
            }),
        )
        .await;
    }
    reader_task.abort();
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
) -> Result<zup_protocol::WorkerOutcome, WorkerError> {
    if exec.target != bootstrap.target {
        return Err(WorkerError::TargetMismatch);
    }
    if exec.bootstrap_json.len() > zup_protocol::MAX_PLAN_BYTES {
        return Err(WorkerError::Protocol("bootstrap plan too large".into()));
    }
    let hash = request_digest_hex(&exec.bootstrap_json);
    if hash != bootstrap.expected_request_digest || hash != exec.bootstrap_hash {
        return Err(WorkerError::RequestDigestMismatch);
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
    let executable = current_exe().map_err(|error| WorkerError::AuthFailed(error.to_string()))?;
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

    let lock_key = ResourceLock::key_for(&exec.app_id, &exec.scope, LockScope::Bootstrap);
    let _lock = match ResourceLock::try_acquire(&state_root, &lock_key) {
        Ok(Some(lock)) => lock,

        Ok(None) => {
            let _ = send_and_close(
                writer,
                bootstrap.session_id,
                outgoing,
                Message::Failed(zup_protocol::Failed {
                    kind: zup_protocol::WorkerFailure::InstallationBusy,
                    message: "another operation is running for this installation".to_owned(),
                }),
            )
            .await;
            return Err(WorkerError::Busy);
        }
        Err(error) => {
            return Err(WorkerError::Transaction(error.to_string()));
        }
    };
    writer
        .send(&WireEnvelope {
            version: PROTOCOL_VERSION,
            session_id: bootstrap.session_id,
            sequence: outgoing,
            message: Message::Progress(ProgressReport {
                blocked_pids: Vec::new(),
                phase: zup_core::OperationPhase::Prepare,
                kind: ProgressKind::OperationStarted,
                detail: "Checking prerequisites…".into(),
                completed: None,
                total: None,
            }),
        })
        .await
        .map_err(|error| WorkerError::Protocol(error.to_string()))?;
    outgoing = outgoing.saturating_add(1);
    let reader_task = split_reader(reader, cancel.clone(), incoming, None);
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
                    detail: None,
                    transaction_id: exec.bootstrap_id,
                    outcome: zup_protocol::WorkerOutcome::RecoveryRequired,
                    prerequisite_id: None,
                    exit_code: None,
                }),
            )
            .await;
            return Ok(zup_protocol::WorkerOutcome::RecoveryRequired);
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
                    kind: zup_protocol::WorkerFailure::Cancelled,
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
                    kind: zup_protocol::WorkerFailure::Transaction,
                    message: error.to_string(),
                }),
            })
            .await;
        return Err(WorkerError::Transaction(error.to_string()));
    }
    let (outcome, prerequisite_id, exit_code) = match result {
        Ok(zup_bootstrap::BootstrapOutcome::Ready) => {
            (zup_protocol::WorkerOutcome::Committed, None, None)
        }
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
            (
                zup_protocol::WorkerOutcome::RebootRequired,
                prerequisite_id,
                Some(exit_code),
            )
        }
        Ok(zup_bootstrap::BootstrapOutcome::RecoveryRequired) => {
            (zup_protocol::WorkerOutcome::RecoveryRequired, None, None)
        }
        Err(error) => return Err(WorkerError::Transaction(error.to_string())),
    };
    let _ = send_and_close(
        writer,
        bootstrap.session_id,
        outgoing,
        Message::Completed(zup_protocol::Completed {
            detail: None,
            transaction_id: exec.bootstrap_id,
            outcome,
            prerequisite_id,
            exit_code,
        }),
    )
    .await;
    let _ = cancel;
    reader_task.abort();
    let _ = reader_task.await;
    Ok(outcome)
}

fn validate_transaction_bundle_identity(
    app_id: &zup_core::AppId,
    app_version: &semver::Version,
    scope: zup_core::SelectedScope,
    target: &zup_core::Target,
    bundle: &EmbeddedBundle,
) -> Result<(), WorkerError> {
    validate_transaction_installer(&bundle.plan().installer, app_id, app_version, scope, target)
}

fn validate_transaction_installer(
    installer: &zup_core::Installer,
    app_id: &zup_core::AppId,
    app_version: &semver::Version,
    scope: zup_core::SelectedScope,
    target: &zup_core::Target,
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
        let detail = "overlay has no generated plugin payload";
        return Err(WorkerError::AuthFailed(detail.into()));
    }
    Ok(())
}

fn split_reader(
    mut reader: ClientReader,
    cancel: CancellationToken,
    mut sequence: SequenceTracker,
    mut execute: Option<tokio::sync::oneshot::Sender<zup_protocol::ExecuteOperation>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Ok(env) = reader.recv().await {
            if sequence.accept(env.sequence).is_err() {
                break;
            }
            match env.message {
                Message::Cancel => cancel.cancel(),
                Message::Execute(operation) => {
                    let Some(sender) = execute.take() else {
                        break;
                    };
                    if sender.send(operation).is_err() {
                        break;
                    }
                }
                Message::Ping => {}
                _ => break,
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

struct WorkerFileExecutor {
    inner: WindowsFileExecutor<crate::runtime::SharedPayload>,
    cancel: TokenProbe,
    progress: tokio::sync::mpsc::UnboundedSender<ProgressReport>,
    completed_work: u64,
    total_work: u64,

    blocked_paths: Vec<PathBuf>,
    service_controls: Vec<zup_plan::ServiceControlOperation>,
    lifecycle: crate::service_control::ScmLifecycle,
}

struct TokenProbe(CancellationToken);

impl CancellationProbe for TokenProbe {
    fn is_cancelled(&self) -> bool {
        self.0.is_cancelled()
    }
}

impl OperationExecutor for WorkerFileExecutor {
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
        _operation: &DeploymentNode,
        receipt: &OperationReceipt,
    ) -> Result<zup_transaction::RollbackReconcileResult, String> {
        match receipt {
            OperationReceipt::PersistentDirectory(owned) => {
                self.inner.verify_persistent(_operation, owned)?;
                Ok(zup_transaction::RollbackReconcileResult::Restored)
            }
            OperationReceipt::NativeService(receipt) => {
                let zup_plan::NodeKind::NativeService { operation } = &_operation.kind else {
                    return Err("native service receipt requires a native service operation".into());
                };
                crate::services::reconcile_native_rollback(operation, receipt)
                    .map_err(|error| error.to_string())
            }
            OperationReceipt::Generation(receipt) => {
                self.inner.retain_generation(_operation, receipt)?;
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
        if !matches!(operation.kind, zup_plan::NodeKind::Barrier) {
            return Ok(());
        }
        let _ = self.progress.send(ProgressReport {
            blocked_pids: Vec::new(),
            phase: zup_core::OperationPhase::Files,
            kind: ProgressKind::PreflightStarted,
            detail: String::new(),
            completed: None,
            total: None,
        });

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
        let _ = self.progress.send(ProgressReport {
            blocked_pids: match &blocked {
                crate::FilePreflight::Blocked { processes, .. } => {
                    processes.iter().map(|process| process.pid).collect()
                }
                crate::FilePreflight::Ready => Vec::new(),
            },
            phase: zup_core::OperationPhase::Files,
            kind: ProgressKind::ResourceBlocked,
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
                crate::integration::verify_managed(operation, receipt).map_err(|e| e.to_string())
            }
            zup_plan::NodeKind::Barrier => Ok(()),
            _ => crate::verify_installed_file(operation, receipt).map_err(|e| e.to_string()),
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
                apply_node(&mut self.inner, operation, transaction).map_err(|e| e.to_string())
            }
        }?;

        self.completed_work = self.completed_work.saturating_add(operation.work());
        let _ = self.progress.send(ProgressReport {
            blocked_pids: Vec::new(),
            phase: zup_runtime::operation_phase(operation),
            kind: ProgressKind::OperationProgress,
            detail: zup_runtime::operation_result_action(operation, &receipt).into(),
            completed: Some(self.completed_work.min(self.total_work)),
            total: Some(self.total_work),
        });
        Ok(receipt)
    }

    fn rollback(
        &mut self,
        op: &DeploymentNode,
        receipt: &OperationReceipt,
    ) -> Result<(), Self::Error> {
        match receipt {
            OperationReceipt::PersistentDirectory(owned) => self.inner.verify_persistent(op, owned),
            OperationReceipt::NativeService(receipt) => {
                let zup_plan::NodeKind::NativeService { operation } = &op.kind else {
                    return Err("native service receipt requires a native service operation".into());
                };
                crate::services::rollback_native(operation, receipt, &mut self.lifecycle)
                    .map_err(|error| error.to_string())
            }
            OperationReceipt::Generation(receipt) => self.inner.retain_generation(op, receipt),
            OperationReceipt::ServiceControl(receipt) => {
                zup_transaction::rollback_service_control(&mut self.lifecycle, receipt)
            }
            OperationReceipt::RetainedFile { .. }
            | OperationReceipt::StageFile { .. }
            | OperationReceipt::CreateFile { .. }
            | OperationReceipt::ReplaceFile { .. }
            | OperationReceipt::RemoveFile { .. } => self
                .inner
                .rollback_transaction_receipt(op, receipt)
                .map_err(|e| e.to_string()),
            _ => crate::integration::rollback_managed(op, receipt).map_err(|e| e.to_string()),
        }
    }

    fn reconcile(
        &mut self,
        op: &DeploymentNode,
        _receipt: Option<&OperationReceipt>,
        transaction: &zup_transaction::TransactionRecord,
    ) -> Result<ReconcileResult, Self::Error> {
        if matches!(op.kind, zup_plan::NodeKind::Cleanup { .. }) {
            return self
                .inner
                .apply_cleanup(op, transaction)
                .map(ReconcileResult::AppliedWithReceipt);
        }
        if let zup_plan::NodeKind::NativeService { operation } = &op.kind {
            return crate::services::reconcile_native(operation, &mut self.lifecycle)
                .map_err(|error| error.to_string());
        }
        if matches!(op.kind, zup_plan::NodeKind::Generation { .. }) {
            return self.inner.reconcile_generation(op, _receipt);
        }
        if let zup_plan::NodeKind::ServiceControl { operation } = &op.kind {
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
        match &op.kind {
            zup_plan::NodeKind::BackendOperation { .. } => {
                return crate::integration::reconcile_managed(op).map_err(|e| e.to_string());
            }
            zup_plan::NodeKind::BackendRemoval { .. } => {
                return crate::integration::reconcile_owned_removal(op).map_err(|e| e.to_string());
            }
            zup_plan::NodeKind::FileRemoval { .. } => {
                return self
                    .inner
                    .reconcile_owned_file_removal(op)
                    .map_err(|e| e.to_string());
            }
            _ => {}
        }
        self.inner
            .reconcile_transaction_node(op, _receipt, transaction)
            .map_err(|e| e.to_string())
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
            target = "windows-x64"
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
        let target = installer.target;
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
