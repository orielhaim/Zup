use std::collections::BTreeMap;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use zup_core::SessionId;
use zup_core::{AppId, ResourceKey, SelectedScope};
use zup_protocol::{
    Capabilities, Message, PROTOCOL_VERSION, PrepareOperation, PreparedOperation,
    PrivilegedSession, ProgressKind, ProgressReport, SequenceTracker, WireEnvelope, WorkerHello,
};

use crate::error::IpcError;
use crate::locations::MachineRoots;
use crate::machine::{authorize_machine_destination, authorize_machine_install_directory};
use crate::socket::{
    HANDSHAKE_TIMEOUT, PeerPin, peer_alive, peer_identity, pin_peer, recv_envelope, send_envelope,
};

pub const EXECUTE_TIMEOUT: Duration = Duration::from_secs(300);

pub const ACCEPT_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WorkerFault {
    #[default]
    None,
    IpcBeforeBegin,
    IpcAfterBegin,
    CancelAfterBegin,
    ProgressDuringExecute,
    DisconnectAfterCommit,
    FailReconciliation,
}

#[derive(Debug, Clone)]
pub struct WorkerContext {
    pub roots: MachineRoots,

    pub systemd: crate::machine::SystemdRoots,

    pub invoking_uid: u32,

    pub expected_client_pid: u32,

    pub session: SessionId,

    pub worker_exe: PathBuf,

    pub carrier_pin: Option<FilePin>,

    pub fault: WorkerFault,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilePin {
    digest: zup_core::Sha256Digest,
}

impl FilePin {
    pub fn pin(path: &Path) -> Result<Self, IpcError> {
        let file = std::fs::File::open(path).map_err(|error| {
            IpcError::WorkerAuth(format!("carrier at `{}`: {error}", path.display()))
        })?;
        let (_, digest) = zup_core::hash_reader(file).map_err(|error| {
            IpcError::WorkerAuth(format!("carrier at `{}`: {error}", path.display()))
        })?;
        Ok(Self { digest })
    }

    pub fn verify(&self, path: &Path) -> Result<(), IpcError> {
        let current = Self::pin(path)?;
        if current != *self {
            return Err(IpcError::WorkerAuth(
                "the installer changed between verification and execution".to_owned(),
            ));
        }
        Ok(())
    }
}

pub fn run_worker_mode(
    session: SessionId,
    expected_client_pid: u32,
    socket_path: &Path,
) -> Result<zup_protocol::WorkerOutcome, IpcError> {
    if rustix::process::geteuid().as_raw() != 0 {
        return Err(IpcError::WorkerAuth(
            "the privileged worker runs as root after authorization".into(),
        ));
    }
    if expected_client_pid == 0 {
        return Err(IpcError::WorkerAuth("no client process".into()));
    }
    let invoking = std::env::var("PKEXEC_UID")
        .map_err(|_| IpcError::WorkerAuth("no authorizing user".into()))?
        .parse::<u32>()
        .map_err(|_| IpcError::WorkerAuth("no authorizing user".into()))?;

    let previous_umask = rustix::process::umask(rustix::fs::Mode::from_bits_truncate(0o077));
    let _ = previous_umask;
    let worker_exe = std::env::current_exe()
        .map_err(|error| IpcError::WorkerAuth(format!("own executable: {error}")))?;
    validate_rendezvous(socket_path, invoking)?;
    let mut stream = crate::socket::connect(socket_path, ACCEPT_TIMEOUT)
        .map_err(|error| IpcError::WorkerProtocol(error.to_string()))?;
    let context = WorkerContext {
        roots: MachineRoots::production(),
        systemd: crate::machine::SystemdRoots::production(),
        invoking_uid: invoking,
        expected_client_pid,
        session,
        worker_exe,
        carrier_pin: None,
        fault: WorkerFault::None,
    };
    serve_session(&mut stream, context)
}

pub(crate) fn validate_rendezvous(socket: &Path, invoking_uid: u32) -> Result<(), IpcError> {
    if !socket.is_absolute() {
        return Err(IpcError::WorkerAuth(
            "the rendezvous is an absolute pathname".into(),
        ));
    }
    if socket.file_name().is_none_or(|name| name != "worker.sock") {
        return Err(IpcError::WorkerAuth(
            "the rendezvous names the session socket".into(),
        ));
    }
    let directory = socket
        .parent()
        .ok_or_else(|| IpcError::WorkerAuth("the rendezvous has no directory".into()))?;
    crate::fs::anchor_reachable(directory).map_err(|error| {
        IpcError::WorkerAuth(format!(
            "the rendezvous must not pass through a link: {error}"
        ))
    })?;
    let metadata = std::fs::symlink_metadata(directory).map_err(|error| {
        IpcError::WorkerAuth(format!("rendezvous at `{}`: {error}", directory.display()))
    })?;
    use std::os::unix::fs::MetadataExt as _;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(IpcError::WorkerAuth(
            "the rendezvous is a real directory, not a link".into(),
        ));
    }
    if metadata.uid() != invoking_uid {
        return Err(IpcError::WorkerAuth(
            "the rendezvous belongs to the authorizing user".into(),
        ));
    }
    if metadata.mode() & 0o077 != 0 {
        return Err(IpcError::WorkerAuth(
            "the rendezvous is private to the authorizing user".into(),
        ));
    }
    Ok(())
}

pub fn serve_session(
    stream: &mut UnixStream,
    context: WorkerContext,
) -> Result<zup_protocol::WorkerOutcome, IpcError> {
    let mut channel = SessionChannel {
        stream,
        session: context.session,
        incoming: SequenceTracker::new(),
        outgoing: 0,
        fail_next_send: false,
    };
    match serve_inner(&mut channel, &context) {
        Ok(outcome) => Ok(outcome),
        Err(error) => {
            let _ = channel.send(Message::Failed(zup_protocol::Failed {
                kind: error.kind().to_owned(),
                message: error.to_string(),
            }));
            Err(error)
        }
    }
}

struct SessionChannel<'a> {
    stream: &'a mut UnixStream,
    session: SessionId,
    incoming: SequenceTracker,
    outgoing: u64,
    fail_next_send: bool,
}

impl SessionChannel<'_> {
    fn send(&mut self, message: Message) -> Result<(), IpcError> {
        if self.fail_next_send {
            self.fail_next_send = false;
            return Err(IpcError::Io {
                path: "<telemetry>".into(),
                source: std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "progress delivery failed",
                ),
            });
        }
        let envelope = WireEnvelope {
            version: PROTOCOL_VERSION,
            session_id: self.session,
            sequence: self.outgoing,
            message,
        };
        send_envelope(self.stream, &envelope)
            .map_err(|error| IpcError::WorkerProtocol(error.to_string()))?;
        self.outgoing = self.outgoing.saturating_add(1);
        Ok(())
    }

    fn recv(&mut self, timeout: Duration) -> Result<WireEnvelope, IpcError> {
        let envelope = recv_envelope(self.stream, timeout)
            .map_err(|error| IpcError::WorkerProtocol(error.to_string()))?;
        self.incoming
            .accept(envelope.sequence)
            .map_err(|error| IpcError::WorkerProtocol(error.to_string()))?;
        if envelope.session_id != self.session {
            return Err(IpcError::WorkerAuth("session mismatch".into()));
        }
        if envelope.version != PROTOCOL_VERSION {
            return Err(IpcError::WorkerProtocol("protocol version mismatch".into()));
        }
        Ok(envelope)
    }

    fn poll_cancel(&mut self) -> Result<bool, IpcError> {
        if !crate::socket::inbound_frame_ready(self.stream)? {
            return Ok(false);
        }
        let envelope = recv_envelope(self.stream, Duration::from_secs(1))?;
        self.incoming
            .accept(envelope.sequence)
            .map_err(|error| IpcError::WorkerProtocol(error.to_string()))?;
        if envelope.session_id != self.session {
            return Err(IpcError::WorkerAuth("session mismatch".into()));
        }
        if envelope.version != PROTOCOL_VERSION {
            return Err(IpcError::WorkerProtocol("protocol version mismatch".into()));
        }
        match envelope.message {
            Message::Cancel => Ok(true),
            _ => Err(IpcError::WorkerProtocol(
                "unexpected message during execution".into(),
            )),
        }
    }
}

enum ControlGate {
    Continue,
    Cancel,
    Disconnected,
    FailClosed(IpcError),
}

fn control_gate(polled: Result<bool, IpcError>) -> ControlGate {
    match polled {
        Ok(false) => ControlGate::Continue,
        Ok(true) => ControlGate::Cancel,
        Err(error) if fail_closed(&error) => ControlGate::FailClosed(error),
        Err(_) => ControlGate::Disconnected,
    }
}

fn fail_closed(error: &IpcError) -> bool {
    matches!(
        error,
        IpcError::WorkerAuth(_) | IpcError::WorkerProtocol(_) | IpcError::Framing(_)
    )
}

fn serve_inner(
    channel: &mut SessionChannel<'_>,
    context: &WorkerContext,
) -> Result<zup_protocol::WorkerOutcome, IpcError> {
    let peer =
        peer_identity(channel.stream).map_err(|error| IpcError::WorkerAuth(error.to_string()))?;
    if peer.uid != context.invoking_uid {
        return Err(IpcError::WorkerAuth(format!(
            "client uid {} is not the authorizing user",
            peer.uid
        )));
    }
    if peer.pid != context.expected_client_pid {
        return Err(IpcError::WorkerAuth(
            "client process is not the launched installer".into(),
        ));
    }
    let pin =
        pin_peer(peer.pid, peer.uid).map_err(|error| IpcError::WorkerAuth(error.to_string()))?;

    let target = worker_target(context)?;
    channel.send(Message::WorkerHello(WorkerHello {
        protocol_version: PROTOCOL_VERSION,
        session_id: context.session,
        target,
        worker_pid: std::process::id(),
        capabilities: worker_capabilities(),
    }))?;

    let mut session_state = PrivilegedSession::new(context.session);
    let envelope = channel.recv(HANDSHAKE_TIMEOUT)?;
    let prepared = match envelope.message {
        Message::Prepare(intent) => {
            if matches!(
                intent.operation,
                zup_protocol::PrivilegedRequest::Recovery { .. }
            ) {
                return serve_recovery(channel, context, &pin, intent);
            }
            prepare_operation(context, &pin, intent)?
        }
        Message::Cancel => return Err(IpcError::Cancelled),
        _ => return Err(IpcError::WorkerProtocol("expected Prepare".into())),
    };
    session_state
        .prepared(context.session, &prepared.plan_digest)
        .map_err(|error| IpcError::WorkerProtocol(error.to_string()))?;
    channel.send(Message::Prepared(PreparedOperation {
        plan: Box::new(prepared.plan.clone()),
        plan_digest: prepared.plan_digest.clone(),
        operation: prepared.operation.clone(),
        app_id: prepared.app_id.to_string(),
        app_version: prepared.app_version.to_string(),
        scope: SelectedScope::Machine.to_string(),
        target: prepared.plan.target,
        file_count: prepared.file_count,
    }))?;

    let envelope = channel.recv(EXECUTE_TIMEOUT)?;
    let execute = match envelope.message {
        Message::Execute(execute) => execute,
        Message::Cancel => return Err(IpcError::Cancelled),
        _ => return Err(IpcError::WorkerProtocol("expected Execute".into())),
    };
    session_state
        .execute(context.session, &execute.plan_digest)
        .map_err(|error| match error {
            zup_protocol::WireError::RequestDigestMismatch => {
                IpcError::WorkerAuth("execute does not name the prepared plan".into())
            }
            zup_protocol::WireError::Replay => IpcError::WorkerProtocol("execute replayed".into()),
            other => IpcError::WorkerProtocol(other.to_string()),
        })?;

    if !peer_alive(&pin) {
        return Err(IpcError::WorkerAuth("the authorized client is gone".into()));
    }
    if let Some(pin) = &prepared.carrier_pin {
        pin.verify(&prepared.carrier_path)?;
    }

    let verdict = execute_prepared(channel, &prepared, context.fault)?;
    let _ = channel.send(Message::Completed(zup_protocol::Completed {
        detail: verdict.detail,
        transaction_id: verdict.transaction_id,
        outcome: verdict.outcome,
        prerequisite_id: None,
        exit_code: None,
    }));
    Ok(verdict.outcome)
}

fn serve_recovery(
    channel: &mut SessionChannel<'_>,
    context: &WorkerContext,
    peer: &PeerPin,
    intent: PrepareOperation,
) -> Result<zup_protocol::WorkerOutcome, IpcError> {
    let prepared = prepare_carrier(context, &intent)?;
    let zup_protocol::PrivilegedRequest::Recovery { transaction: id } = intent.operation else {
        return Err(IpcError::Policy("recovery transaction is required".into()));
    };
    let app = &prepared.carrier.package().plan().installer.app;
    let key = zup_transaction::ResourceLock::lock_key(app.id.as_str(), "machine");
    let lock = zup_transaction::ResourceLock::try_acquire(&prepared.state_root, &key)
        .map_err(|error| IpcError::Transaction(error.to_string()))?
        .ok_or(IpcError::WorkerBusy)?;
    let store = zup_transaction::FilesystemTransactionStore::private_at(&prepared.state_root);
    let record = zup_transaction::TransactionStore::load(&store, &id)
        .map_err(|error| IpcError::Transaction(error.to_string()))?;
    if record.app_id != app.id
        || record.scope != SelectedScope::Machine
        || record.target != intent.target
    {
        return Err(IpcError::WorkerAuth("recovery identity mismatch".into()));
    }
    let digest = record.plan_hash.to_hex();
    channel.send(Message::Prepared(PreparedOperation {
        plan: Box::new(record.plan.clone()),
        plan_digest: digest.clone(),
        operation: "recover".into(),
        app_id: app.id.to_string(),
        app_version: record.app_version.to_string(),
        scope: "machine".into(),
        target: record.target,
        file_count: 0,
    }))?;
    match channel.recv(EXECUTE_TIMEOUT)?.message {
        Message::Cancel => return Err(IpcError::Cancelled),
        Message::Execute(execute) if execute.plan_digest == digest => {}
        _ => {
            return Err(IpcError::WorkerAuth(
                "execute does not name the recovery plan".into(),
            ));
        }
    }
    if !peer_alive(peer) {
        return Err(IpcError::WorkerAuth("the authorized client is gone".into()));
    }
    if let Some(pin) = &prepared.carrier_pin {
        pin.verify(&prepared.carrier_path)?;
    }
    let result = if record.phase == zup_transaction::TransactionPhase::Committed
        && record.cleanup_complete()
    {
        crate::ledger::LinuxInstallationStore::new(&prepared.state_root)
            .reconcile_installation(&record, record.scope)
            .map(|_| ())
            .map_err(|error| IpcError::Transaction(error.to_string()))
    } else {
        recover_one(
            &prepared.state_root,
            &record.app_id,
            &prepared.carrier,
            &prepared.carrier_path,
            &id.to_string(),
            &RecoveryHost {
                roots: &context.roots,
                systemd: &context.systemd,
                lock: &lock,
            },
        )
    };
    let fresh = zup_transaction::TransactionStore::load(&store, &id)
        .map_err(|error| IpcError::Transaction(error.to_string()))?;
    let (outcome, detail) = match (result, fresh.phase) {
        (Ok(()), zup_transaction::TransactionPhase::Committed) => {
            (zup_protocol::WorkerOutcome::Committed, None)
        }
        (Ok(()), zup_transaction::TransactionPhase::RolledBack) => {
            (zup_protocol::WorkerOutcome::RolledBack, None)
        }
        (Err(error), zup_transaction::TransactionPhase::Committed) => (
            zup_protocol::WorkerOutcome::CommittedPendingReconciliation,
            Some(error.to_string()),
        ),
        (Err(error), _) => (
            zup_protocol::WorkerOutcome::RecoveryRequired,
            Some(error.to_string()),
        ),
        _ => (
            zup_protocol::WorkerOutcome::RecoveryRequired,
            Some("recovery did not settle the transaction".into()),
        ),
    };
    channel.send(Message::Completed(zup_protocol::Completed {
        transaction_id: id.as_uuid(),
        outcome,
        detail,
        prerequisite_id: None,
        exit_code: None,
    }))?;
    Ok(outcome)
}

fn worker_capabilities() -> Capabilities {
    Capabilities {
        file_transactions_v1: true,
        backend_operations_v1: false,
        lifecycle_v1: true,
        prerequisite_bootstrap_v1: false,
    }
}

fn worker_target(context: &WorkerContext) -> Result<zup_core::Target, IpcError> {
    let carrier = crate::carrier::Carrier::open(&context.worker_exe)
        .map_err(|error| IpcError::WorkerAuth(format!("worker carrier: {error}")))?;
    Ok(*carrier.target())
}

struct PreparedPlan {
    release: Option<zup_core::ReleaseIdentity>,
    plan_digest: String,
    operation: String,
    app_id: zup_core::AppId,
    app_version: semver::Version,
    file_count: u32,
    carrier_path: PathBuf,
    carrier_pin: Option<FilePin>,
    plan: zup_plan::DeploymentPlan,
    state_root: PathBuf,
    roots: MachineRoots,
    systemd: crate::machine::SystemdRoots,
    payload: PreparedPayload,

    _lock: zup_transaction::ResourceLock,
}

struct PreparedPayload {
    maintenance_sha256: zup_core::Sha256Digest,
    maintenance_size: u64,
    generated: BTreeMap<String, Vec<u8>>,
    package: zup_bundle::PackagePayloadSource,
}

struct PreparedCarrier {
    state_root: PathBuf,
    carrier_path: PathBuf,
    carrier_pin: Option<FilePin>,
    carrier: crate::carrier::Carrier,
}

fn prepare_carrier(
    context: &WorkerContext,
    intent: &PrepareOperation,
) -> Result<PreparedCarrier, IpcError> {
    intent
        .validate_shape()
        .map_err(|error| IpcError::WorkerProtocol(error.to_string()))?;
    if intent.scope != "machine" {
        return Err(IpcError::Policy(
            "the privileged worker serves machine scope only".into(),
        ));
    }
    if intent.target.operating_system() != zup_core::TargetOperatingSystem::Linux {
        return Err(IpcError::Policy(
            "the privileged worker serves Linux targets only".into(),
        ));
    }
    let target = intent.target;
    let app_id =
        AppId::new(&intent.app_id).map_err(|error| IpcError::Policy(format!("app id: {error}")))?;
    let app_version: semver::Version = intent
        .app_version
        .parse()
        .map_err(|error| IpcError::Policy(format!("app version: {error}")))?;

    let state_root = PathBuf::from(&intent.state_root);
    if !state_root.is_absolute() {
        return Err(IpcError::Policy(
            "machine state travels as an absolute path".into(),
        ));
    }
    if state_root.components().any(|component| {
        matches!(
            component,
            std::path::Component::ParentDir | std::path::Component::CurDir
        )
    }) {
        return Err(IpcError::Policy(
            "machine state names plain components".into(),
        ));
    }
    crate::machine::resolve_machine_state_root(Some(&state_root), &context.roots, &app_id)
        .map_err(|error| IpcError::WorkerAuth(error.to_string()))?;
    let authority_version = if matches!(&intent.operation, zup_protocol::PrivilegedRequest::Deployment { intent, .. } if matches!(intent.action, zup_plan::LifecycleAction::Rollback { .. } | zup_plan::LifecycleAction::Cleanup { .. }))
    {
        crate::carrier::Carrier::open(&context.worker_exe)
            .map_err(|error| IpcError::WorkerAuth(error.to_string()))?
            .package()
            .plan()
            .installer
            .app
            .version
            .clone()
    } else {
        app_version
    };
    let (carrier_path, carrier_pin) = select_trusted_carrier(
        &state_root,
        &context.worker_exe,
        &app_id,
        &authority_version,
        &target,
    )?;
    let carrier = crate::carrier::Carrier::open(&carrier_path)
        .map_err(|error| IpcError::WorkerAuth(format!("trusted carrier: {error}")))?;
    verify_carrier_declares(&carrier, &app_id, &authority_version, &target)?;

    match &intent.operation {
        zup_protocol::PrivilegedRequest::Deployment { .. } => {
            crate::machine::ensure_machine_state_root(
                &context.roots,
                &app_id,
                rustix::process::geteuid().as_raw(),
            )
            .map_err(|error| IpcError::WorkerAuth(error.to_string()))?;
        }
        zup_protocol::PrivilegedRequest::Recovery { transaction } => {
            let store = zup_transaction::FilesystemTransactionStore::private_at(&state_root);
            let record = zup_transaction::TransactionStore::load(&store, transaction)
                .map_err(|error| IpcError::WorkerAuth(error.to_string()))?;
            if record.app_id != app_id
                || record.scope != SelectedScope::Machine
                || record.target != target
            {
                return Err(IpcError::WorkerAuth(
                    "recovery journal differs from the authenticated application".into(),
                ));
            }
        }
    }
    crate::machine::verify_machine_hierarchy(&state_root, rustix::process::geteuid().as_raw())
        .map_err(|error| IpcError::WorkerAuth(error.to_string()))?;
    crate::machine::verify_machine_structure(&state_root, rustix::process::geteuid().as_raw())
        .map_err(|error| IpcError::WorkerAuth(error.to_string()))?;
    crate::machine::ensure_transactions_dir(&state_root, rustix::process::geteuid().as_raw())
        .map_err(|error| IpcError::WorkerAuth(error.to_string()))?;
    crate::machine::normalize_state_modes(&state_root, rustix::process::geteuid().as_raw())
        .map_err(|error| IpcError::WorkerAuth(error.to_string()))?;

    Ok(PreparedCarrier {
        state_root,
        carrier_path,
        carrier_pin,
        carrier,
    })
}

fn prepare_operation(
    context: &WorkerContext,
    _pin: &PeerPin,
    intent: PrepareOperation,
) -> Result<PreparedPlan, IpcError> {
    let PreparedCarrier {
        state_root,
        carrier_path,
        carrier_pin,
        carrier,
    } = prepare_carrier(context, &intent)?;
    let app_id = carrier.package().plan().installer.app.id.clone();
    let roots = context.roots.clone();

    let mut targets = carrier
        .package()
        .build_plan()
        .map_err(|error| IpcError::Policy(format!("package: {error}")))?
        .targets;
    if targets.len() != 1 {
        return Err(IpcError::Policy(format!(
            "an installer package holds exactly one target; this one holds {}",
            targets.len()
        )));
    }
    let build = targets.remove(0);

    if !build.installer.prerequisites.is_empty() {
        return Err(IpcError::Policy(
            "machine scope runs no prerequisite installers as root".into(),
        ));
    }
    if !build.installer.install.scope.allows_machine() {
        return Err(IpcError::Policy(
            "the package does not declare machine scope".into(),
        ));
    }

    let zup_protocol::PrivilegedRequest::Deployment {
        intent: deployment,
        expected_requirements,
    } = &intent.operation
    else {
        return Err(IpcError::WorkerProtocol(
            "deployment request required".into(),
        ));
    };
    let requested = zup_runtime::Operation::from(deployment.action);

    let ledger_store = crate::ledger::LinuxInstallationStore::new(&state_root);
    let seed = ledger_store
        .load(&app_id, SelectedScope::Machine)
        .map_err(|error| IpcError::Transaction(error.to_string()))?;
    let lock_key = zup_transaction::ResourceLock::lock_key(app_id.as_str(), "machine");
    let lock = match zup_transaction::ResourceLock::try_acquire(&state_root, &lock_key)
        .map_err(|error| IpcError::Transaction(error.to_string()))?
    {
        Some(lock) => lock,
        None => return Err(IpcError::WorkerBusy),
    };
    crate::machine::normalize_state_modes(&state_root, rustix::process::geteuid().as_raw())
        .map_err(|error| IpcError::Transaction(error.to_string()))?;

    let settled_before = seed.as_ref().map(|ledger| ledger.commit.transaction);
    ledger_store
        .repair_committed(&app_id, SelectedScope::Machine)
        .map_err(|error| match error {
            crate::error::ExecError::RecoveryRequired {
                transaction,
                detail,
            } => IpcError::RecoveryRequired {
                transaction,
                detail,
            },
            error => IpcError::Transaction(error.to_string()),
        })?;
    crate::machine::verify_ledger_trust(&state_root, &app_id, rustix::process::geteuid().as_raw())
        .map_err(|error| IpcError::WorkerAuth(error.to_string()))?;
    let ledger = ledger_store
        .load(&app_id, SelectedScope::Machine)
        .map_err(|error| IpcError::Transaction(error.to_string()))?;
    let selection = zup_runtime::OperationSelection {
        deployment: Some(deployment.deployment.clone()),
        enable: deployment.enable.clone(),
        disable: deployment.disable.clone(),
        install_directory: deployment.install_directory.clone(),
    };
    let host = crate::operation_host::LinuxOperationHost {
        installer: carrier_path.clone(),
        machine: Some(crate::operation_host::MachineHost {
            roots: roots.clone(),
            systemd: context.systemd.clone(),
        }),
    };
    let input = zup_runtime::OperationInput {
        build: &build,
        installer: &carrier_path,
        scope: SelectedScope::Machine,
        operation: requested,
        selection,
        prior: ledger.as_ref(),
        state_root: &state_root,
        cancellation: &zup_plan::NeverCancelled,
    };
    let prepared = zup_runtime::prepare_operation(&input, &host).map_err(|error| match error {
        zup_runtime::OperationError::RecoveryRequired(transaction) => IpcError::RecoveryRequired {
            transaction: Some(transaction),
            detail: format!("unfinished transaction {transaction} requires recovery"),
        },
        error => IpcError::Policy(error.to_string()),
    })?;
    if let Some(target) = prepared.target_plan() {
        enforce_machine_policy(target, &roots)?;
        let host = crate::lowering::to_host_path(&target.install_directory)
            .map_err(|error| IpcError::Policy(error.to_string()))?;
        crate::fs::anchor_reachable(&host)
            .map_err(|error| IpcError::Policy(format!("install destination: {error}")))?;
    } else if let Some(ledger) = &ledger {
        let sources = ledger
            .current
            .resources
            .iter()
            .filter_map(|(key, resource)| {
                let (ResourceKey::Service { id }, zup_plan::OwnedResource::Service(service)) =
                    (key, resource)
                else {
                    return None;
                };
                let zup_plan::ServiceState::Registration { execution, .. } =
                    service.installed.as_ref()
                else {
                    return None;
                };
                Some(service_source_path(&service.name, execution).map(|path| (id.clone(), path)))
            })
            .collect::<Result<std::collections::BTreeMap<_, _>, _>>()?;
        for (key, resource) in &ledger.current.resources {
            if let zup_plan::OwnedResource::File {
                destination,
                source_relative,
                ..
            } = resource
            {
                enforce_machine_file_policy(key, destination, source_relative, &roots, &sources)?;
            }
        }
    }

    let generated = crate::operation_host::generated_content(&prepared, Some(&roots))
        .map_err(IpcError::Policy)?;
    let release = match &prepared.body {
        zup_runtime::OperationBody::Retained { release, .. } => release.clone(),
        _ => carrier.package().release_identity().cloned(),
    };
    let plan = prepared.transaction_plan;
    if plan.nodes.iter().any(|node| {
        matches!(
            &node.kind,
            zup_plan::NodeKind::BackendOperation { .. } | zup_plan::NodeKind::BackendRemoval { .. }
        )
    }) {
        return Err(IpcError::Policy(
            "a machine transaction holds no foreign backend operations".into(),
        ));
    }
    ledger_store
        .validate_plan(&app_id, SelectedScope::Machine, &plan)
        .map_err(|error| IpcError::Transaction(error.to_string()))?;

    let plan_digest = plan.fingerprint().to_hex();
    if let Some(expected) = expected_requirements
        && expected.as_ref() != &plan.requirements
    {
        let settled_after = ledger_store
            .load(&app_id, SelectedScope::Machine)
            .map_err(|error| IpcError::Transaction(error.to_string()))?
            .map(|ledger| ledger.commit.transaction);
        if settled_after != settled_before {
            return Err(IpcError::WorkerStalePlan);
        }
        return Err(IpcError::WorkerAuth(
            "the reconstructed plan differs from the authorized one".into(),
        ));
    }

    let (maintenance_size, maintenance_sha256) = crate::run::hash_carrier(&carrier_path)
        .map_err(|error| IpcError::Transaction(format!("carrier bytes: {error}")))?;
    let file_count = u32::try_from(
        plan.nodes
            .iter()
            .filter(|node| {
                matches!(
                    node.kind,
                    zup_plan::NodeKind::FileMutation { .. }
                        | zup_plan::NodeKind::FileRemoval { .. }
                )
            })
            .count(),
    )
    .map_err(|_| IpcError::Policy("too many files".into()))?;
    crate::machine::verify_machine_install_roots(&roots, rustix::process::geteuid().as_raw())
        .map_err(|error| IpcError::Policy(error.to_string()))?;
    Ok(PreparedPlan {
        release,
        plan_digest,
        operation: intent.operation.name().into(),
        app_id,
        app_version: plan.requirements.app.version.clone(),
        file_count,
        carrier_path,
        carrier_pin,
        plan,
        state_root,
        roots: roots.clone(),
        systemd: context.systemd.clone(),
        payload: PreparedPayload {
            maintenance_sha256,
            maintenance_size,
            generated,
            package: carrier.package().payload_source(),
        },
        _lock: lock,
    })
}

struct RecoveryHost<'a> {
    roots: &'a MachineRoots,
    systemd: &'a crate::machine::SystemdRoots,
    lock: &'a zup_transaction::ResourceLock,
}

fn recover_one(
    state_root: &Path,
    app_id: &AppId,
    carrier: &crate::carrier::Carrier,
    carrier_path: &Path,
    transaction: &str,
    host: &RecoveryHost<'_>,
) -> Result<(), IpcError> {
    let id = transaction
        .parse::<uuid::Uuid>()
        .map_err(|_| IpcError::Transaction("unparsable transaction identity".into()))?;

    crate::machine::verify_trusted_state_file(
        &state_root
            .join("transactions")
            .join(id.to_string())
            .join("transaction.json"),
        rustix::process::geteuid().as_raw(),
    )
    .map_err(|error| IpcError::WorkerAuth(error.to_string()))?;
    let store = zup_transaction::FilesystemTransactionStore::private_at(state_root);
    let record =
        zup_transaction::TransactionStore::load(&store, &zup_core::TransactionId::from_uuid(id))
            .map_err(|error| IpcError::Transaction(error.to_string()))?;
    if record.app_id != *app_id || record.scope != SelectedScope::Machine {
        return Err(IpcError::Transaction(
            "recovery record identity mismatch".into(),
        ));
    }

    crate::machine::verify_machine_install_roots(host.roots, rustix::process::geteuid().as_raw())
        .map_err(|error| IpcError::Transaction(error.to_string()))?;
    let (maintenance_size, maintenance_sha256) = crate::run::hash_carrier(carrier_path)
        .map_err(|error| IpcError::Transaction(format!("carrier bytes: {error}")))?;
    let generated = if crate::integration::phase_keeps_generated(record.phase) {
        crate::integration::generated_for_plan(
            state_root,
            app_id,
            record.scope,
            host.lock,
            &record.plan,
        )
        .map_err(|error| IpcError::Transaction(error.to_string()))?
    } else {
        std::collections::BTreeMap::new()
    };
    let payload = crate::run::RunnerPayload::from_prepared(
        carrier.package().payload_source(),
        carrier_path.to_path_buf(),
        maintenance_sha256,
        maintenance_size,
        generated,
    );
    let mut executor = crate::executor::LinuxFileExecutor::new()
        .with_payload(payload)
        .for_machine();
    if crate::service_ops::plan_requires_manager(&record.plan) {
        executor = executor.with_services(
            crate::executor::ServiceSupport::for_host(host.roots.clone(), host.systemd.clone())
                .map_err(IpcError::Transaction)?,
        );
    }
    executor
        .register_plan(&record.plan)
        .map_err(|error| IpcError::Transaction(error.to_string()))?;
    executor.set_state_authority(
        state_root.to_path_buf(),
        record.app_id.clone(),
        record.scope,
    );
    executor.set_transaction_id(record.transaction_id);
    let (record, outcome) = zup_transaction::recover(
        record,
        &store,
        &mut executor,
        &crate::ledger::LinuxInstallationStore::new(state_root),
    )
    .map_err(|error| IpcError::Transaction(error.to_string()))?;
    match outcome {
        zup_transaction::TransactionOutcome::Committed => {
            crate::integration::reap_generated(state_root, app_id, record.scope, host.lock)
                .map_err(|error| IpcError::Transaction(error.to_string()))?;
            ledger_store_publish(state_root, &record)?;
            Ok(())
        }
        zup_transaction::TransactionOutcome::RolledBack => {
            crate::integration::reap_generated(
                state_root,
                app_id,
                SelectedScope::Machine,
                host.lock,
            )
            .map_err(|error| IpcError::Transaction(error.to_string()))?;
            Ok(())
        }
        zup_transaction::TransactionOutcome::RecoveryRequired => Err(IpcError::Transaction(
            format!("transaction {transaction} requires recovery"),
        )),
    }
}

fn ledger_store_publish(
    state_root: &Path,
    record: &zup_transaction::TransactionRecord,
) -> Result<zup_plan::InstallationRecord, IpcError> {
    crate::ledger::LinuxInstallationStore::new(state_root)
        .reconcile_installation(record, SelectedScope::Machine)
        .map_err(|error| IpcError::Transaction(error.to_string()))
}

fn enforce_machine_policy(
    target: &zup_plan::BoundDeployment,
    roots: &MachineRoots,
) -> Result<(), IpcError> {
    let install_host = crate::lowering::to_host_path(&target.install_directory)
        .map_err(|error| IpcError::Policy(format!("install directory: {error}")))?;
    authorize_machine_install_directory(&install_host, roots)
        .map_err(|error| IpcError::Policy(error.to_string()))?;
    let sources = target
        .services
        .iter()
        .map(|service| {
            service_source_path(service.name.as_str(), &service.execution)
                .map(|path| (service.id.clone(), path))
        })
        .collect::<Result<std::collections::BTreeMap<_, _>, _>>()?;
    for file in &target.files {
        enforce_machine_file_policy(
            &file.key,
            file.destination(),
            &file.source_relative,
            roots,
            &sources,
        )?;
    }

    if target.scope == SelectedScope::Machine && !target.services.is_empty() {
        let mut target_files = std::collections::BTreeMap::new();
        for file in &target.files {
            target_files.insert(file.destination().to_string(), file.executable);
        }
        for service in &target.services {
            crate::service_ops::validate_executable(
                &service.command,
                &target_files,
                roots,
                rustix::process::geteuid().as_raw(),
                false,
            )
            .map_err(|error| IpcError::Policy(format!("service binary: {error}")))?;
        }
    }
    Ok(())
}

fn service_source_path(
    name: &str,
    execution: &zup_core::ServiceExecution,
) -> Result<PathBuf, IpcError> {
    if matches!(execution, zup_core::ServiceExecution::OpenrcUser { .. }) {
        if !crate::openrc::valid_name(name) {
            return Err(IpcError::Policy("invalid OpenRC service name".into()));
        }
        Ok(PathBuf::from("/etc/init.d").join(name))
    } else {
        crate::machine::authorize_systemd_unit(name, &crate::machine::SystemdRoots::production())
            .map_err(|error| IpcError::Policy(error.to_string()))
    }
}

fn enforce_machine_file_policy(
    key: &ResourceKey,
    path: &zup_core::TargetPath,
    source: &zup_core::RelativePath,
    roots: &MachineRoots,
    service_sources: &std::collections::BTreeMap<zup_core::ServiceId, PathBuf>,
) -> Result<(), IpcError> {
    let host = crate::lowering::to_host_path(path)
        .map_err(|error| IpcError::Policy(format!("destination: {error}")))?;
    if let ResourceKey::File {
        id:
            zup_core::FileResourceId::Native {
                id: zup_core::NativeFileId::ServiceSource { id },
            },
    } = key
    {
        if service_sources.get(id) != Some(&host)
            || source.as_str() != crate::integration::service_source_name(key)
        {
            return Err(IpcError::Policy(
                "service source does not match its native identity".into(),
            ));
        }
        return Ok(());
    }
    let destination = authorize_machine_destination(&host, roots)
        .map_err(|error| IpcError::Policy(error.to_string()))?;
    match key {
        ResourceKey::File {
            id:
                zup_core::FileResourceId::Native {
                    id: zup_core::NativeFileId::Environment,
                },
        } => {
            if destination != crate::machine::MachineDestination::Environment {
                return Err(IpcError::Policy(
                    "environment configuration belongs to the environment.d tree".into(),
                ));
            }
        }
        ResourceKey::File {
            id: zup_core::FileResourceId::Payload { .. } | zup_core::FileResourceId::Plugin { .. },
        } => {
            if destination != crate::machine::MachineDestination::Programs
                && destination != crate::machine::MachineDestination::SharedData
            {
                return Err(IpcError::Policy(
                    "payload belongs to the program or variable-data tree".into(),
                ));
            }
        }
        _ => {
            return Err(IpcError::Policy(format!(
                "machine scope holds no {:?} resources",
                key
            )));
        }
    }
    Ok(())
}

fn select_trusted_carrier(
    state_root: &Path,
    worker_exe: &Path,
    app_id: &AppId,
    app_version: &semver::Version,
    target: &zup_core::Target,
) -> Result<(PathBuf, Option<FilePin>), IpcError> {
    let pin = FilePin::pin(worker_exe)?;
    if let Ok(carrier) = crate::carrier::Carrier::open(worker_exe)
        && verify_carrier_declares(&carrier, app_id, app_version, target).is_ok()
    {
        return Ok((worker_exe.to_path_buf(), Some(pin)));
    }
    let installed = crate::ledger::LinuxInstallationStore::new(state_root)
        .load(app_id, SelectedScope::Machine)
        .map_err(|error| IpcError::WorkerAuth(error.to_string()))?;
    if let Some(installed) = installed
        && installed.is_installed()
        && installed.current.version == *app_version
        && installed.target == *target
        && let Some(spec) = installed.current.maintenance.as_ref()
    {
        let name = format!("maintenance{}", target.executable_suffix());
        let expected = spec
            .descriptor()
            .files()
            .iter()
            .find(|file| file.path.as_str() == name && file.executable)
            .ok_or_else(|| {
                IpcError::WorkerAuth(
                    "installed maintenance generation has no runtime executable".into(),
                )
            })?;
        let parent = crate::lowering::to_host_path(spec.directory())
            .map_err(|error| IpcError::WorkerAuth(error.to_string()))?;
        let directory = crate::fs::OwnedDirectory::open(&parent)
            .map_err(|error| IpcError::WorkerAuth(error.to_string()))?;
        directory
            .verify_descendants(Path::new("."), rustix::process::geteuid().as_raw())
            .map_err(|error| IpcError::WorkerAuth(error.to_string()))?;
        let observed = directory
            .stream_regular(&name)
            .map_err(|error| IpcError::WorkerAuth(error.to_string()))?;
        if observed.uid != rustix::process::geteuid().as_raw()
            || observed.mode & 0o022 != 0
            || observed.size != expected.size
            || observed.sha256 != expected.sha256
        {
            return Err(IpcError::WorkerAuth(
                "installed maintenance runtime differs from protected ownership evidence".into(),
            ));
        }
        let maintenance = parent.join(name);
        let carrier = crate::carrier::Carrier::open(&maintenance)
            .map_err(|error| IpcError::WorkerAuth(error.to_string()))?;
        verify_carrier_declares(&carrier, app_id, app_version, target)?;
        return Ok((
            maintenance,
            Some(FilePin {
                digest: expected.sha256,
            }),
        ));
    }
    Err(IpcError::WorkerAuth(
        "no trusted carrier declares the requested operation".into(),
    ))
}

fn verify_carrier_declares(
    carrier: &crate::carrier::Carrier,
    app_id: &AppId,
    app_version: &semver::Version,
    target: &zup_core::Target,
) -> Result<(), IpcError> {
    let installer = carrier
        .package()
        .build_plan()
        .map_err(|error| IpcError::WorkerAuth(format!("package: {error}")))?
        .targets
        .into_iter()
        .next()
        .ok_or_else(|| IpcError::WorkerAuth("the package declares no target".into()))?;
    if &installer.installer.app.id != app_id
        || &installer.installer.app.version != app_version
        || &installer.installer.target != target
        || !installer.installer.install.scope.allows_machine()
    {
        return Err(IpcError::WorkerAuth(
            "the carrier does not declare the requested machine operation".into(),
        ));
    }
    Ok(())
}

struct ExecutionVerdict {
    detail: Option<String>,
    transaction_id: uuid::Uuid,
    outcome: zup_protocol::WorkerOutcome,
}

fn verdict(transaction_id: uuid::Uuid, outcome: zup_protocol::WorkerOutcome) -> ExecutionVerdict {
    ExecutionVerdict {
        detail: None,
        transaction_id,
        outcome,
    }
}

fn pending_verdict(transaction_id: uuid::Uuid, detail: String) -> ExecutionVerdict {
    ExecutionVerdict {
        transaction_id,
        outcome: zup_protocol::WorkerOutcome::CommittedPendingReconciliation,
        detail: Some(detail),
    }
}

fn settled_name(
    settled: &zup_transaction::SettledTransaction,
    cancelled: bool,
) -> ExecutionVerdict {
    let transaction_id = settled.record.transaction_id.as_uuid();
    let outcome = match settled.outcome {
        zup_transaction::TransactionOutcome::Committed => zup_protocol::WorkerOutcome::Committed,
        zup_transaction::TransactionOutcome::RolledBack if cancelled => {
            zup_protocol::WorkerOutcome::Cancelled
        }
        zup_transaction::TransactionOutcome::RolledBack => zup_protocol::WorkerOutcome::RolledBack,
        zup_transaction::TransactionOutcome::RecoveryRequired
            if settled.record.phase == zup_transaction::TransactionPhase::Committed =>
        {
            zup_protocol::WorkerOutcome::CommittedPendingReconciliation
        }
        zup_transaction::TransactionOutcome::RecoveryRequired => {
            zup_protocol::WorkerOutcome::RecoveryRequired
        }
    };
    ExecutionVerdict {
        transaction_id,
        outcome,
        detail: match settled.outcome {
            zup_transaction::TransactionOutcome::RecoveryRequired => {
                Some(settled.record.recovery_detail())
            }
            zup_transaction::TransactionOutcome::RolledBack => settled
                .record
                .failure
                .as_ref()
                .map(|failure| failure.message.clone()),
            zup_transaction::TransactionOutcome::Committed => None,
        },
    }
}

fn execute_prepared(
    channel: &mut SessionChannel<'_>,
    prepared: &PreparedPlan,
    fault: WorkerFault,
) -> Result<ExecutionVerdict, IpcError> {
    let app_id = &prepared.app_id;
    crate::integration::reap_generated(
        &prepared.state_root,
        app_id,
        SelectedScope::Machine,
        &prepared._lock,
    )
    .map_err(|error| IpcError::Transaction(error.to_string()))?;
    crate::integration::stage_generated(
        &prepared.state_root,
        app_id,
        SelectedScope::Machine,
        &prepared._lock,
        &prepared.payload.generated,
    )
    .map_err(|error| IpcError::Transaction(error.to_string()))?;
    let payload = crate::run::RunnerPayload::from_prepared(
        prepared.payload.package.clone(),
        prepared.carrier_path.clone(),
        prepared.payload.maintenance_sha256,
        prepared.payload.maintenance_size,
        prepared.payload.generated.clone(),
    );
    let mut executor = crate::executor::LinuxFileExecutor::new()
        .with_payload(payload)
        .for_machine();

    if crate::service_ops::plan_requires_manager(&prepared.plan) {
        executor = executor.with_services(
            crate::executor::ServiceSupport::for_host(
                prepared.roots.clone(),
                prepared.systemd.clone(),
            )
            .map_err(IpcError::Transaction)?,
        );
    }
    executor
        .register_plan(&prepared.plan)
        .map_err(|error| IpcError::Transaction(error.to_string()))?;
    if fault == WorkerFault::IpcBeforeBegin {
        return Err(IpcError::Io {
            path: "<telemetry>".into(),
            source: std::io::Error::new(std::io::ErrorKind::BrokenPipe, "injected disconnect"),
        });
    }
    match control_gate(channel.poll_cancel()) {
        ControlGate::Continue => {}
        ControlGate::Cancel | ControlGate::Disconnected => return Err(IpcError::Cancelled),
        ControlGate::FailClosed(error) => return Err(error),
    }

    let store = zup_transaction::FilesystemTransactionStore::private_at(&prepared.state_root);
    let open = zup_transaction::TransactionCoordinator::new(store)
        .begin(
            prepared.app_id.clone(),
            SelectedScope::Machine,
            prepared.app_version.clone(),
            prepared.plan.clone(),
        )
        .map_err(|error| IpcError::Transaction(error.to_string()))?;
    let open = open
        .with_release(prepared.release.clone())
        .map_err(|error| IpcError::Transaction(error.to_string()))?;
    executor.set_state_authority(
        prepared.state_root.clone(),
        prepared.app_id.clone(),
        SelectedScope::Machine,
    );
    executor.set_transaction_id(open.record().transaction_id);
    let transaction_id = open.record().transaction_id.as_uuid();
    let gate = match fault {
        WorkerFault::CancelAfterBegin => ControlGate::Cancel,
        WorkerFault::IpcAfterBegin => ControlGate::Disconnected,
        _ => control_gate(channel.poll_cancel()),
    };
    let total_files = prepared.file_count as u64;
    let mut cancelled = matches!(gate, ControlGate::Cancel | ControlGate::Disconnected);
    let settled = match gate {
        ControlGate::FailClosed(error) => {
            return match open.release(&mut executor) {
                Ok(_) => Err(error),
                Err(error) => Ok(ExecutionVerdict {
                    transaction_id,
                    outcome: zup_protocol::WorkerOutcome::RecoveryRequired,
                    detail: Some(error.to_string()),
                }),
            };
        }
        ControlGate::Cancel | ControlGate::Disconnected => match open.release(&mut executor) {
            Ok(settled) => settled,
            Err(error) => {
                return Ok(ExecutionVerdict {
                    transaction_id,
                    outcome: zup_protocol::WorkerOutcome::RecoveryRequired,
                    detail: Some(error.to_string()),
                });
            }
        },
        ControlGate::Continue => {
            let _ = channel.send(Message::Progress(ProgressReport {
                blocked_pids: Vec::new(),
                phase: zup_core::OperationPhase::Prepare,
                kind: ProgressKind::OperationStarted,
                detail: prepared.operation.clone(),
                completed: None,
                total: None,
            }));
            if fault == WorkerFault::ProgressDuringExecute {
                channel.fail_next_send = true;
                let _ = channel.send(Message::Progress(ProgressReport {
                    blocked_pids: Vec::new(),
                    phase: zup_core::OperationPhase::Files,
                    kind: ProgressKind::OperationProgress,
                    detail: prepared.operation.clone(),
                    completed: Some(0),
                    total: Some(total_files),
                }));
            }
            let mut gated = GatedExecutor {
                inner: executor,
                channel,
                cancelled: &mut cancelled,
            };
            match open.execute(
                &mut gated,
                &crate::ledger::LinuxInstallationStore::new(&prepared.state_root),
            ) {
                Ok(settled) => settled,
                Err(error) => {
                    return Ok(ExecutionVerdict {
                        transaction_id,
                        outcome: zup_protocol::WorkerOutcome::RecoveryRequired,
                        detail: Some(error.to_string()),
                    });
                }
            }
        }
    };
    if settled.outcome != zup_transaction::TransactionOutcome::Committed {
        if settled.outcome == zup_transaction::TransactionOutcome::RolledBack
            && let Err(error) = crate::integration::reap_generated(
                &prepared.state_root,
                app_id,
                SelectedScope::Machine,
                &prepared._lock,
            )
        {
            tracing::warn!(%error, "generated content remains until the next locked cleanup");
        }
        return Ok(settled_name(&settled, cancelled));
    }
    if fault == WorkerFault::DisconnectAfterCommit {
        channel.fail_next_send = true;
        let _ = channel.send(Message::Progress(ProgressReport {
            blocked_pids: Vec::new(),
            phase: zup_core::OperationPhase::Files,
            kind: ProgressKind::OperationProgress,
            detail: prepared.operation.clone(),
            completed: Some(total_files),
            total: Some(total_files),
        }));
    }
    let record = settled.record;
    if fault == WorkerFault::FailReconciliation {
        let store = zup_transaction::FilesystemTransactionStore::private_at(&prepared.state_root);
        let detail = zup_transaction::reconcile_committed::<()>(
            &store,
            &record,
            &prepared.state_root,
            |_| Err("injected post-commit reconciliation failure".into()),
        )
        .unwrap_err();
        return Ok(pending_verdict(transaction_id, detail));
    }
    if let Err(error) = crate::integration::reap_generated(
        &prepared.state_root,
        app_id,
        SelectedScope::Machine,
        &prepared._lock,
    ) {
        tracing::warn!(%error, "committed install left generated content for a later cleanup");
    }
    let ledger_store = crate::ledger::LinuxInstallationStore::new(&prepared.state_root);
    zup_transaction::fault_injection::boundary(
        zup_transaction::fault_injection::Boundary::ReconciliationBefore,
        &transaction_id.to_string(),
    );
    match ledger_store.reconcile_installation(&record, SelectedScope::Machine) {
        Ok(_) => {}
        Err(error) => {
            return Ok(pending_verdict(transaction_id, error.to_string()));
        }
    };
    zup_transaction::fault_injection::boundary(
        zup_transaction::fault_injection::Boundary::Reconciled,
        &transaction_id.to_string(),
    );
    let _ = channel.send(Message::Progress(ProgressReport {
        blocked_pids: Vec::new(),
        phase: zup_core::OperationPhase::Files,
        kind: ProgressKind::OperationProgress,
        detail: prepared.operation.clone(),
        completed: Some(total_files),
        total: Some(total_files),
    }));
    Ok(verdict(
        transaction_id,
        zup_protocol::WorkerOutcome::Committed,
    ))
}

struct GatedExecutor<'a, 'b> {
    inner: crate::executor::LinuxFileExecutor,
    channel: &'a mut SessionChannel<'b>,
    cancelled: &'a mut bool,
}

impl GatedExecutor<'_, '_> {
    fn note(&mut self) -> Result<(), crate::error::ExecError> {
        match control_gate(self.channel.poll_cancel()) {
            ControlGate::Continue => Ok(()),
            ControlGate::Cancel | ControlGate::Disconnected => {
                *self.cancelled = true;
                Err(crate::error::ExecError::Cancelled)
            }
            ControlGate::FailClosed(error) => {
                Err(crate::error::ExecError::Worker(error.to_string()))
            }
        }
    }
}

impl zup_transaction::OperationExecutor for GatedExecutor<'_, '_> {
    type Error = crate::error::ExecError;

    fn prepare_compensation(
        &mut self,
        operation: &zup_plan::DeploymentNode,
        receipt: &zup_transaction::OperationReceipt,
    ) -> Result<zup_transaction::OperationReceipt, Self::Error> {
        self.inner.prepare_compensation(operation, receipt)
    }

    fn reconcile_rollback(
        &mut self,
        operation: &zup_plan::DeploymentNode,
        receipt: &zup_transaction::OperationReceipt,
    ) -> Result<zup_transaction::RollbackReconcileResult, Self::Error> {
        self.inner.reconcile_rollback(operation, receipt)
    }

    fn prepare(&mut self, operation: &zup_plan::DeploymentNode) -> Result<(), Self::Error> {
        self.note()?;
        self.inner.prepare(operation)
    }

    fn apply(
        &mut self,
        operation: &zup_plan::DeploymentNode,
        transaction: &zup_transaction::TransactionRecord,
    ) -> Result<zup_transaction::OperationReceipt, Self::Error> {
        self.note()?;
        self.inner.apply(operation, transaction)
    }

    fn verify(
        &mut self,
        operation: &zup_plan::DeploymentNode,
        receipt: &zup_transaction::OperationReceipt,
    ) -> Result<(), Self::Error> {
        self.inner.verify(operation, receipt)
    }

    fn rollback(
        &mut self,
        operation: &zup_plan::DeploymentNode,
        receipt: &zup_transaction::OperationReceipt,
    ) -> Result<(), Self::Error> {
        self.inner.rollback(operation, receipt)
    }

    fn reconcile(
        &mut self,
        operation: &zup_plan::DeploymentNode,
        receipt: Option<&zup_transaction::OperationReceipt>,
        transaction: &zup_transaction::TransactionRecord,
    ) -> Result<zup_transaction::ReconcileResult, Self::Error> {
        self.inner.reconcile(operation, receipt, transaction)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_execution_cancel_is_readable_without_consuming_later_frames() {
        let (mut client, mut worker) = UnixStream::pair().expect("socket pair");
        let session = SessionId::new_v7();
        send_envelope(
            &mut client,
            &WireEnvelope {
                version: PROTOCOL_VERSION,
                session_id: session,
                sequence: 0,
                message: Message::Cancel,
            },
        )
        .expect("send cancel");
        let mut channel = SessionChannel {
            stream: &mut worker,
            session,
            incoming: SequenceTracker::new(),
            outgoing: 0,
            fail_next_send: false,
        };
        assert!(channel.poll_cancel().expect("poll"));
        assert!(!channel.poll_cancel().expect("quiet"));
    }

    #[test]
    fn file_pins_detect_a_swap() {
        let first = tempfile::tempdir().expect("a directory");
        let path = first.path().join("installer");
        std::fs::write(&path, b"v1").expect("write");
        let pin = FilePin::pin(&path).expect("pin");
        pin.verify(&path).expect("unchanged verifies");
        std::fs::remove_file(&path).expect("remove");
        std::fs::write(&path, b"v2").expect("replace");
        assert!(pin.verify(&path).is_err(), "a swapped file fails the pin");
        std::fs::write(&path, b"v1").expect("write");
        let pin = FilePin::pin(&path).expect("pin");
        std::fs::write(&path, b"v2").expect("overwrite in place");
        assert!(
            pin.verify(&path).is_err(),
            "an in-place overwrite fails the pin despite the same inode and size"
        );
    }

    #[test]
    fn worker_mode_requires_root() {
        if rustix::process::geteuid().as_raw() == 0 {
            return;
        }
        assert!(matches!(
            run_worker_mode(
                SessionId::new_v7(),
                std::process::id(),
                std::path::Path::new("/run/zup/worker.sock")
            ),
            Err(IpcError::WorkerAuth(_))
        ));
    }

    #[test]
    fn machine_policy_rejects_a_foreign_destination() {
        let target = zup_core::Target::parse("linux-x64").expect("a target");
        let roots = MachineRoots::production();
        let outside = zup_core::TargetPath::new(target, "/etc/tool").expect("a path");
        assert!(
            authorize_machine_destination(
                &crate::lowering::to_host_path(&outside).expect("lowers"),
                &roots
            )
            .is_err()
        );
    }
}
