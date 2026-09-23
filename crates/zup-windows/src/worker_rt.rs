//! Real worker runtime: connect, authenticate, handshake, one transaction, exit.
use std::path::PathBuf;
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use zup_bundle::AutoPayloadSource;
use zup_protocol::{
    Capabilities, FILE_TRANSACTIONS_V1, Message, PROTOCOL_VERSION, ProgressKind, ProgressReport,
    SequenceTracker, SessionId, WireEnvelope, WorkerHello,
};
use zup_transaction::{
    CancellationProbe, FilesystemTransactionStore, OperationExecutor, OperationReceipt,
    ReconcileResult, TransactionCoordinator, TransactionNode, TransactionOutcome, TransactionPlan,
};

use crate::FilePrecondition;
use crate::durable::InstallationLock;
use crate::file_executor::{NullProgress, WindowsFileExecutor, apply_node};
use crate::pipe::{ClientReader, ClientWriter, PipeError, frame_client};
use crate::transport::{UserSid, verify_server_pid};
use crate::worker::{WorkerBootstrap, WorkerError, plan_hash_hex};

/// Timeouts (no timeout on installation execution).
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

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
            worker_pid: std::process::id(),
            capabilities: Capabilities::supported(),
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
        Message::ExecuteTransaction(exec) => exec,
        Message::Cancel => return Err(WorkerError::Protocol("cancelled".into())),
        _ => return Err(WorkerError::Protocol("expected ExecuteTransaction".into())),
    };

    // 8. Plan binding + capability check (still before lock/journal).
    if exec.plan_json.len() > zup_protocol::MAX_PLAN_BYTES {
        return Err(WorkerError::Protocol("plan too large".into()));
    }
    let hash = plan_hash_hex(&exec.plan_json);
    if hash != bootstrap.expected_plan_hash || hash != exec.plan_hash {
        return Err(WorkerError::PlanHashMismatch);
    }
    let plan: TransactionPlan = serde_json::from_str(&exec.plan_json)
        .map_err(|e| WorkerError::Protocol(format!("bad plan: {e}")))?;
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
                return Err(WorkerError::MissingCapability(
                    FILE_TRANSACTIONS_V1.to_owned(),
                ));
            }
        }
    }

    let app_id = zup_core::AppId::new(&exec.app_id)
        .map_err(|e| WorkerError::Protocol(format!("bad app id: {e}")))?;
    let app_version = exec
        .app_version
        .parse()
        .map_err(|e| WorkerError::Protocol(format!("bad app version: {e}")))?;
    let scope = match exec.scope.as_str() {
        "user" => zup_core::SelectedScope::User,
        "machine" => zup_core::SelectedScope::Machine,
        _ => return Err(WorkerError::Protocol("invalid install scope".into())),
    };
    let payload_root = PathBuf::from(exec.payload_root);
    let payload = AutoPayloadSource::from_path(payload_root.clone())
        .map_err(|e| WorkerError::Transaction(format!("payload package: {e}")))?;
    let state_root = PathBuf::from(exec.state_root);
    let work_root = PathBuf::from(exec.work_root);

    writer
        .send(&WireEnvelope {
            version: PROTOCOL_VERSION,
            session_id: bootstrap.session_id,
            sequence: outgoing,
            message: Message::Progress(ProgressReport {
                kind: ProgressKind::OperationStarted,
                detail: "transaction accepted".into(),
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
    let record = if let Some(id) = exec.recovery_id {
        let record = zup_transaction::TransactionStore::load(
            &FilesystemTransactionStore::new(&state_root),
            &zup_transaction::TransactionId::from_uuid(id),
        )
        .map_err(|e| WorkerError::Transaction(e.to_string()))?;
        if record.app_id != app_id
            || record.scope != scope
            || record.app_version != app_version
            || record.plan != plan
        {
            return Err(WorkerError::AuthFailed("recovery record mismatch".into()));
        }
        record
    } else {
        coordinator
            .begin(app_id, scope, app_version, plan)
            .map_err(|e| WorkerError::Transaction(e.to_string()))?
    };

    let mut executor = WorkerFileExecutor {
        inner: WindowsFileExecutor::new(
            payload,
            work_root,
            record.transaction_id.to_string(),
            Box::new(NullProgress),
        ),
        cancel: TokenProbe(cancel.clone()),
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

    let (transaction_id, outcome) = match result {
        Ok((r, TransactionOutcome::Committed)) => {
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
            (r.transaction_id.as_uuid(), "rolled_back".to_owned())
        }
        Ok((r, TransactionOutcome::RecoveryRequired)) => {
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
        }),
    )
    .await;
    let _ = reader_task.await;
    Ok(outcome)
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
struct WorkerFileExecutor {
    inner: WindowsFileExecutor<AutoPayloadSource>,
    cancel: TokenProbe,
}

struct TokenProbe(CancellationToken);

impl CancellationProbe for TokenProbe {
    fn is_cancelled(&self) -> bool {
        self.0.is_cancelled()
    }
}

impl OperationExecutor for WorkerFileExecutor {
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
            return crate::integration::apply_managed(operation).map_err(|e| e.to_string());
        }
        if let zup_transaction::NodeKind::OwnedRemoval { resource, .. } = operation.kind {
            return if resource == zup_transaction::ManagedResource::File {
                self.inner
                    .apply_owned_file_removal(operation)
                    .map_err(|e| e.to_string())
            } else {
                crate::integration::apply_owned_removal(operation).map_err(|e| e.to_string())
            };
        }
        let (rel, dest) = extract_file(operation)?;
        let receipt =
            apply_node(&mut self.inner, operation, &rel, &dest).map_err(|e| e.to_string())?;
        Ok(map_receipt(receipt))
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
        if matches!(
            op.kind,
            zup_transaction::NodeKind::ManagedIntegration { .. }
        ) {
            return crate::integration::reconcile_managed(op).map_err(|e| e.to_string());
        }
        if let zup_transaction::NodeKind::OwnedRemoval { resource, .. } = op.kind {
            return if resource == zup_transaction::ManagedResource::File {
                self.inner
                    .reconcile_owned_file_removal(op)
                    .map_err(|e| e.to_string())
            } else {
                crate::integration::reconcile_owned_removal(op).map_err(|e| e.to_string())
            };
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

fn map_receipt(receipt: crate::file_executor::OperationReceipt) -> OperationReceipt {
    use crate::file_executor::OperationReceipt as W;
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
