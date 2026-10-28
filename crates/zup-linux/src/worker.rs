use std::collections::BTreeMap;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use zup_core::{AppId, ResourceKey, SelectedScope};
use zup_protocol::{
    Capabilities, Message, PROTOCOL_VERSION, PrepareOperation, PreparedOperation,
    PrivilegedSession, ProgressKind, ProgressReport, SequenceTracker, SessionId, WireEnvelope,
    WorkerHello,
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
    FailLedgerPublish,
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
) -> Result<String, IpcError> {
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

pub fn serve_session(stream: &mut UnixStream, context: WorkerContext) -> Result<String, IpcError> {
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
) -> Result<String, IpcError> {
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
        Message::Prepare(intent) => prepare_operation(context, &pin, intent)?,
        Message::Cancel => return Err(IpcError::Cancelled),
        _ => return Err(IpcError::WorkerProtocol("expected Prepare".into())),
    };
    session_state
        .prepared(context.session, &prepared.plan_digest)
        .map_err(|error| IpcError::WorkerProtocol(error.to_string()))?;
    channel.send(Message::Prepared(PreparedOperation {
        plan_digest: prepared.plan_digest.clone(),
        operation: prepared.operation.clone(),
        app_id: prepared.app_id.clone(),
        app_version: prepared.app_version.clone(),
        scope: prepared.scope.clone(),
        target: prepared.target,
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
            zup_protocol::WireError::PlanHashMismatch => {
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
        transaction_id: verdict.transaction_id,
        outcome: verdict.outcome.clone(),
        prerequisite_id: None,
        exit_code: None,
    }));
    Ok(verdict.outcome)
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
    plan_digest: String,
    operation: String,
    app_id: String,
    app_version: String,
    scope: String,
    target: zup_core::Target,
    file_count: u32,
    carrier_path: PathBuf,
    carrier_pin: Option<FilePin>,
    plan: zup_transaction::TransactionPlan,
    target_plan: zup_platform::TargetPlan,
    action: zup_exec::LifecycleAction,
    state_root: PathBuf,
    roots: MachineRoots,
    systemd: crate::machine::SystemdRoots,
    payload: PreparedPayload,

    _lock: zup_transaction::InstallationLock,
}

struct PreparedPayload {
    installer: PathBuf,
    maintenance_sha256: zup_core::Sha256Digest,
    maintenance_size: u64,
    generated: BTreeMap<String, Vec<u8>>,
    package: zup_bundle::PackagePayloadSource,
}

fn prepare_operation(
    context: &WorkerContext,
    _pin: &PeerPin,
    intent: PrepareOperation,
) -> Result<PreparedPlan, IpcError> {
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
    crate::machine::resolve_machine_state_root(Some(&state_root), &context.roots)
        .map_err(|error| IpcError::WorkerAuth(error.to_string()))?;
    crate::machine::ensure_machine_state_dir(&state_root, rustix::process::geteuid().as_raw())
        .map_err(|error| IpcError::WorkerAuth(error.to_string()))?;
    crate::machine::verify_machine_hierarchy(&state_root, rustix::process::geteuid().as_raw())
        .map_err(|error| IpcError::WorkerAuth(error.to_string()))?;
    crate::machine::verify_machine_structure(&state_root, rustix::process::geteuid().as_raw())
        .map_err(|error| IpcError::WorkerAuth(error.to_string()))?;
    crate::machine::ensure_transactions_dir(&state_root, rustix::process::geteuid().as_raw())
        .map_err(|error| IpcError::WorkerAuth(error.to_string()))?;
    crate::machine::normalize_state_modes(&state_root, rustix::process::geteuid().as_raw())
        .map_err(|error| IpcError::WorkerAuth(error.to_string()))?;
    let roots = context.roots.clone();

    let (carrier_path, carrier_pin) = select_trusted_carrier(
        &state_root,
        &context.worker_exe,
        &app_id,
        &app_version,
        &target,
    )?;
    let carrier = crate::carrier::Carrier::open(&carrier_path)
        .map_err(|error| IpcError::WorkerAuth(format!("trusted carrier: {error}")))?;
    verify_carrier_declares(&carrier, &app_id, &app_version, &target)?;

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

    if !build.installer.plugins.is_empty() {
        return Err(IpcError::Policy(
            "machine scope refuses projects that need plugin execution in the privileged worker"
                .into(),
        ));
    }
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

    let requested = match intent.operation.as_str() {
        zup_protocol::privileged_operation::INSTALL => zup_runtime::Operation::Install,
        zup_protocol::privileged_operation::UPGRADE => zup_runtime::Operation::Upgrade,
        zup_protocol::privileged_operation::MODIFY => zup_runtime::Operation::Modify,
        zup_protocol::privileged_operation::REPAIR => zup_runtime::Operation::Repair {
            force_files: intent.force_files,
        },
        zup_protocol::privileged_operation::UNINSTALL => zup_runtime::Operation::Uninstall,
        zup_protocol::privileged_operation::APPLY => zup_runtime::Operation::Apply,
        _ => {
            return Err(IpcError::WorkerProtocol(
                "unknown privileged operation".into(),
            ));
        }
    };

    let ledger_store = crate::ledger::LinuxLedgerStore::new(&state_root);
    let seed = ledger_store
        .load(&app_id, SelectedScope::Machine)
        .map_err(|error| IpcError::Transaction(error.to_string()))?;
    let lock_key = zup_transaction::InstallationLock::lock_key(app_id.as_str(), "machine");
    let lock = match zup_transaction::InstallationLock::try_acquire(&state_root, &lock_key)
        .map_err(|error| IpcError::Transaction(error.to_string()))?
    {
        Some(lock) => lock,
        None => return Err(IpcError::WorkerBusy),
    };
    crate::machine::normalize_state_modes(&state_root, rustix::process::geteuid().as_raw())
        .map_err(|error| IpcError::Transaction(error.to_string()))?;

    let settled_before = seed
        .as_ref()
        .map(|ledger| ledger.committed_transaction.clone());
    recover_pending(
        &state_root,
        &app_id,
        &carrier,
        &carrier_path,
        &RecoveryHost {
            roots: &roots,
            systemd: &context.systemd,
            lock: &lock,
        },
    )?;

    ledger_store
        .repair_committed(&app_id, SelectedScope::Machine)
        .map_err(|error| IpcError::Transaction(error.to_string()))?;
    crate::machine::verify_ledger_trust(&state_root, &app_id, rustix::process::geteuid().as_raw())
        .map_err(|error| IpcError::WorkerAuth(error.to_string()))?;
    let ledger = ledger_store
        .load(&app_id, SelectedScope::Machine)
        .map_err(|error| IpcError::Transaction(error.to_string()))?;
    let selection = zup_runtime::OperationSelection {
        enable: intent.selected_components.clone(),
        disable: intent.disabled_components.clone(),
        install_directory: intent.install_dir_override.as_ref().map(PathBuf::from),
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
    let prepared = zup_runtime::prepare_operation(&input, &host)
        .map_err(|error| IpcError::Policy(error.to_string()))?;
    let target_plan = prepared
        .target_plan()
        .cloned()
        .ok_or_else(|| IpcError::Policy("a machine transaction resolves its target".into()))?;
    enforce_machine_policy(&target_plan, &roots)?;
    if let Ok(host) = crate::lowering::to_host_path(&target_plan.install_directory) {
        authorize_machine_install_directory(&host, &roots)
            .map_err(|error| IpcError::Policy(error.to_string()))?;
        crate::fs::anchor_reachable(&host)
            .map_err(|error| IpcError::Policy(format!("install destination: {error}")))?;
    }

    let generated =
        crate::operation_host::generated_content(&prepared).map_err(IpcError::Policy)?;
    let plan = prepared.transaction_plan;
    if plan.nodes.iter().any(|node| {
        matches!(
            &node.kind,
            zup_transaction::NodeKind::BackendOperation { .. }
                | zup_transaction::NodeKind::BackendRemoval { .. }
        ) && !is_expected_backend(node)
    }) {
        return Err(IpcError::Policy(
            "a machine transaction holds no foreign backend operations".into(),
        ));
    }
    ledger_store
        .validate_plan(
            &target_plan.app.id,
            SelectedScope::Machine,
            &target_plan.app.version,
            &plan,
        )
        .map_err(|error| IpcError::Transaction(error.to_string()))?;

    let plan_digest = plan.fingerprint().to_hex();
    if let Some(expected) = &intent.expected_plan_digest
        && expected.to_lowercase() != plan_digest
    {
        let settled_after = ledger_store
            .load(&target_plan.app.id, SelectedScope::Machine)
            .map_err(|error| IpcError::Transaction(error.to_string()))?
            .map(|ledger| ledger.committed_transaction.clone());
        if settled_after != settled_before {
            return Err(IpcError::WorkerStalePlan);
        }
        return Err(IpcError::WorkerAuth(
            "the reconstructed plan differs from the authorized one".into(),
        ));
    }

    let (maintenance_size, maintenance_sha256) = crate::run::hash_carrier(&carrier_path)
        .map_err(|error| IpcError::Transaction(format!("carrier bytes: {error}")))?;
    let file_count = u32::try_from(target_plan.files.len())
        .map_err(|_| IpcError::Policy("too many files".into()))?;
    crate::machine::verify_machine_install_roots(&roots, rustix::process::geteuid().as_raw())
        .map_err(|error| IpcError::Policy(error.to_string()))?;
    Ok(PreparedPlan {
        plan_digest,
        operation: intent.operation.clone(),
        app_id: intent.app_id.clone(),
        app_version: intent.app_version.clone(),
        scope: intent.scope.clone(),
        target,
        file_count,
        carrier_path: carrier_path.clone(),
        carrier_pin,
        plan,
        target_plan,
        action: prepared.action,
        state_root,
        roots: roots.clone(),
        systemd: context.systemd.clone(),
        payload: PreparedPayload {
            installer: carrier_path,
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
    lock: &'a zup_transaction::InstallationLock,
}

fn recover_pending(
    state_root: &Path,
    app_id: &AppId,
    carrier: &crate::carrier::Carrier,
    carrier_path: &Path,
    host: &RecoveryHost<'_>,
) -> Result<(), IpcError> {
    let ledger_store = crate::ledger::LinuxLedgerStore::new(state_root);
    loop {
        match ledger_store.repair_committed(app_id, SelectedScope::Machine) {
            Ok(()) => return Ok(()),
            Err(crate::error::ExecError::RecoveryRequired(transaction)) => {
                recover_one(
                    state_root,
                    app_id,
                    carrier,
                    carrier_path,
                    &transaction,
                    host,
                )?;
            }
            Err(error) => return Err(IpcError::Transaction(error.to_string())),
        }
    }
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
    let record = zup_transaction::TransactionStore::load(
        &store,
        &zup_transaction::TransactionId::from_uuid(id),
    )
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
    if record.plan.nodes.iter().any(|node| {
        node.meta.backend.as_ref().is_some_and(|backend| {
            backend
                .id
                .as_str()
                .starts_with(crate::service_ops::SERVICE_BACKEND_PREFIX)
        })
    }) {
        let manager = crate::systemd::RealSystemd::connect()
            .map_err(|error| IpcError::Transaction(error.to_string()))?;
        executor = executor.with_services(crate::executor::ServiceSupport::isolated(
            host.roots.clone(),
            host.systemd.clone(),
            manager,
            rustix::process::geteuid().as_raw(),
        ));
    }
    executor
        .register_plan(&record.plan)
        .map_err(|error| IpcError::Transaction(error.to_string()))?;
    executor.set_transaction_id(record.transaction_id);
    let (record, outcome) = zup_transaction::recover(record, &store, &mut executor)
        .map_err(|error| IpcError::Transaction(error.to_string()))?;
    match outcome {
        zup_transaction::TransactionOutcome::Committed => {
            crate::integration::reap_generated(state_root, app_id, record.scope, host.lock)
                .map_err(|error| IpcError::Transaction(error.to_string()))?;
            let ledger = ledger_store_publish(state_root, &record)?;

            if !record.plan.uninstall {
                crate::machine::normalize_published_modes(state_root, &ledger)
                    .map_err(|error| IpcError::Transaction(error.to_string()))?;
            }
            crate::machine::normalize_state_modes(state_root, rustix::process::geteuid().as_raw())
                .map_err(|error| IpcError::Transaction(error.to_string()))?;
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
) -> Result<zup_exec::InstallLedger, IpcError> {
    crate::ledger::LinuxLedgerStore::new(state_root)
        .publish_committed(record, SelectedScope::Machine)
        .map_err(|error| IpcError::Transaction(error.to_string()))
}

fn is_expected_backend(node: &zup_transaction::TransactionNode) -> bool {
    let Some(backend) = node.meta.backend.as_ref() else {
        return false;
    };
    backend
        .id
        .as_str()
        .starts_with(crate::service_ops::SERVICE_BACKEND_PREFIX)
        || backend.id.as_str() == crate::refresh::REFRESH_MIME_ID
        || backend.id.as_str() == crate::refresh::REFRESH_DESKTOP_ID
}

fn is_preset_source(source: &zup_core::RelativePath) -> bool {
    source.as_str() == zup_bundle::PRESET_SOURCE
        || source
            .as_str()
            .strip_prefix(zup_bundle::ASSET_SOURCE_PREFIX)
            .is_some_and(|rest| rest.starts_with('/'))
}

fn enforce_machine_policy(
    target: &zup_platform::TargetPlan,
    roots: &MachineRoots,
) -> Result<(), IpcError> {
    let policy = |path: &zup_platform::TargetPath| {
        let host = crate::lowering::to_host_path(path)
            .map_err(|error| IpcError::Policy(format!("destination: {error}")))?;
        authorize_machine_destination(&host, roots)
            .map_err(|error| IpcError::Policy(error.to_string()))
    };
    let install_host = crate::lowering::to_host_path(&target.install_directory)
        .map_err(|error| IpcError::Policy(format!("install directory: {error}")))?;
    authorize_machine_install_directory(&install_host, roots)
        .map_err(|error| IpcError::Policy(error.to_string()))?;
    for file in &target.files {
        let destination = policy(&file.destination)?;
        match &file.key {
            ResourceKey::Maintenance { .. } => {
                if destination != crate::machine::MachineDestination::MachineState {
                    return Err(IpcError::Policy(
                        "maintenance belongs to machine state".into(),
                    ));
                }
            }
            ResourceKey::File { .. } if is_preset_source(&file.source_relative) => {
                if destination != crate::machine::MachineDestination::MachineState {
                    return Err(IpcError::Policy(
                        "preset content belongs to machine state".into(),
                    ));
                }
            }
            ResourceKey::File { .. } => {
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
                    file.key
                )));
            }
        }
    }

    if target.scope == SelectedScope::Machine && !target.services.is_empty() {
        let mut target_files = std::collections::BTreeMap::new();
        for file in &target.files {
            target_files.insert(file.destination.to_string(), file.executable);
        }
        for service in &target.services {
            let unit = crate::services::unit_name(&service.id)
                .map_err(|error| IpcError::Policy(format!("service identity: {error}")))?;

            let _ = unit;
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

fn select_trusted_carrier(
    state_root: &Path,
    worker_exe: &Path,
    app_id: &AppId,
    app_version: &semver::Version,
    target: &zup_core::Target,
) -> Result<(PathBuf, Option<FilePin>), IpcError> {
    let maintenance = zup_transaction::maintenance_runtime_path(
        state_root,
        app_id,
        SelectedScope::Machine,
        app_version,
        target.executable_suffix(),
    );
    if std::fs::symlink_metadata(&maintenance).is_ok() {
        crate::machine::verify_trusted_state_file(
            &maintenance,
            rustix::process::geteuid().as_raw(),
        )
        .map_err(|error| IpcError::WorkerAuth(error.to_string()))?;
        if let Ok(carrier) = crate::carrier::Carrier::open(&maintenance)
            && verify_carrier_declares(&carrier, app_id, app_version, target).is_ok()
        {
            return Ok((maintenance, None));
        }
    }

    let pin = FilePin::pin(worker_exe)?;
    if let Ok(carrier) = crate::carrier::Carrier::open(worker_exe)
        && verify_carrier_declares(&carrier, app_id, app_version, target).is_ok()
    {
        return Ok((worker_exe.to_path_buf(), Some(pin)));
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
    transaction_id: uuid::Uuid,
    outcome: String,
}

fn verdict(transaction_id: uuid::Uuid, outcome: &str) -> ExecutionVerdict {
    ExecutionVerdict {
        transaction_id,
        outcome: outcome.to_owned(),
    }
}

fn settled_name(
    settled: &zup_transaction::SettledTransaction,
    cancelled: bool,
) -> ExecutionVerdict {
    let transaction_id = settled.record.transaction_id.as_uuid();
    let outcome = match settled.outcome {
        zup_transaction::TransactionOutcome::Committed => "committed",
        zup_transaction::TransactionOutcome::RolledBack if cancelled => "cancelled",
        zup_transaction::TransactionOutcome::RolledBack => "rolled_back",
        zup_transaction::TransactionOutcome::RecoveryRequired => "recovery_required",
    };
    verdict(transaction_id, outcome)
}

fn execute_prepared(
    channel: &mut SessionChannel<'_>,
    prepared: &PreparedPlan,
    fault: WorkerFault,
) -> Result<ExecutionVerdict, IpcError> {
    let app_id = zup_core::AppId::new(&prepared.app_id)
        .map_err(|error| IpcError::Transaction(error.to_string()))?;
    crate::integration::reap_generated(
        &prepared.state_root,
        &app_id,
        SelectedScope::Machine,
        &prepared._lock,
    )
    .map_err(|error| IpcError::Transaction(error.to_string()))?;
    crate::integration::stage_generated(
        &prepared.state_root,
        &app_id,
        SelectedScope::Machine,
        &prepared._lock,
        &prepared.payload.generated,
    )
    .map_err(|error| IpcError::Transaction(error.to_string()))?;
    let payload = crate::run::RunnerPayload::from_prepared(
        prepared.payload.package.clone(),
        prepared.payload.installer.clone(),
        prepared.payload.maintenance_sha256,
        prepared.payload.maintenance_size,
        prepared.payload.generated.clone(),
    );
    let mut executor = crate::executor::LinuxFileExecutor::new()
        .with_payload(payload)
        .for_machine();

    if prepared.plan.nodes.iter().any(|node| {
        node.meta.backend.as_ref().is_some_and(|backend| {
            backend
                .id
                .as_str()
                .starts_with(crate::service_ops::SERVICE_BACKEND_PREFIX)
        })
    }) {
        let manager = crate::systemd::RealSystemd::connect()
            .map_err(|error| IpcError::Transaction(error.to_string()))?;
        executor = executor.with_services(crate::executor::ServiceSupport::isolated(
            prepared.roots.clone(),
            prepared.systemd.clone(),
            manager,
            rustix::process::geteuid().as_raw(),
        ));
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
            prepared.target_plan.app.id.clone(),
            SelectedScope::Machine,
            prepared.target_plan.app.version.clone(),
            prepared.plan.clone(),
        )
        .map_err(|error| IpcError::Transaction(error.to_string()))?;
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
                Err(_) => Ok(verdict(transaction_id, "recovery_required")),
            };
        }
        ControlGate::Cancel | ControlGate::Disconnected => match open.release(&mut executor) {
            Ok(settled) => settled,
            Err(_) => return Ok(verdict(transaction_id, "recovery_required")),
        },
        ControlGate::Continue => {
            let _ = channel.send(Message::Progress(ProgressReport {
                kind: ProgressKind::OperationStarted,
                detail: prepared.operation.clone(),
                completed: None,
                total: None,
            }));
            if fault == WorkerFault::ProgressDuringExecute {
                channel.fail_next_send = true;
                let _ = channel.send(Message::Progress(ProgressReport {
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
            match open.execute(&mut gated) {
                Ok(settled) => settled,
                Err(_) => return Ok(verdict(transaction_id, "recovery_required")),
            }
        }
    };
    if settled.outcome != zup_transaction::TransactionOutcome::Committed {
        if settled.outcome == zup_transaction::TransactionOutcome::RolledBack
            && let Err(error) = crate::integration::reap_generated(
                &prepared.state_root,
                &app_id,
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
            kind: ProgressKind::OperationProgress,
            detail: prepared.operation.clone(),
            completed: Some(total_files),
            total: Some(total_files),
        }));
    }
    if fault == WorkerFault::FailLedgerPublish {
        return Ok(verdict(transaction_id, "committed_pending_reconciliation"));
    }
    let record = settled.record;
    if let Err(error) = crate::integration::reap_generated(
        &prepared.state_root,
        &app_id,
        SelectedScope::Machine,
        &prepared._lock,
    ) {
        tracing::warn!(%error, "committed install left generated content for a later cleanup");
        return Ok(verdict(transaction_id, "committed_pending_reconciliation"));
    }
    let ledger_store = crate::ledger::LinuxLedgerStore::new(&prepared.state_root);
    let ledger = match ledger_store.publish_committed(&record, SelectedScope::Machine) {
        Ok(ledger) => ledger,
        Err(_) => {
            return Ok(verdict(transaction_id, "committed_pending_reconciliation"));
        }
    };
    if finalize_committed(prepared, &ledger).is_err() {
        return Ok(verdict(transaction_id, "committed_pending_reconciliation"));
    }
    let _ = channel.send(Message::Progress(ProgressReport {
        kind: ProgressKind::OperationProgress,
        detail: prepared.operation.clone(),
        completed: Some(total_files),
        total: Some(total_files),
    }));
    Ok(verdict(transaction_id, "committed"))
}

fn finalize_committed(
    prepared: &PreparedPlan,
    ledger: &zup_exec::InstallLedger,
) -> Result<(), IpcError> {
    if prepared.action != zup_exec::LifecycleAction::Uninstall {
        crate::machine::normalize_published_modes(&prepared.state_root, ledger)
            .map_err(|error| IpcError::Transaction(error.to_string()))?;
    }
    crate::machine::normalize_state_modes(
        &prepared.state_root,
        rustix::process::geteuid().as_raw(),
    )
    .map_err(|error| IpcError::Transaction(error.to_string()))?;
    Ok(())
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

    fn prepare(&mut self, operation: &zup_transaction::TransactionNode) -> Result<(), Self::Error> {
        self.note()?;
        self.inner.prepare(operation)
    }

    fn apply(
        &mut self,
        operation: &zup_transaction::TransactionNode,
    ) -> Result<zup_transaction::OperationReceipt, Self::Error> {
        self.note()?;
        self.inner.apply(operation)
    }

    fn verify(
        &mut self,
        operation: &zup_transaction::TransactionNode,
        receipt: &zup_transaction::OperationReceipt,
    ) -> Result<(), Self::Error> {
        self.inner.verify(operation, receipt)
    }

    fn rollback(
        &mut self,
        operation: &zup_transaction::TransactionNode,
        receipt: &zup_transaction::OperationReceipt,
    ) -> Result<(), Self::Error> {
        self.inner.rollback(operation, receipt)
    }

    fn reconcile(
        &mut self,
        operation: &zup_transaction::TransactionNode,
        receipt: Option<&zup_transaction::OperationReceipt>,
    ) -> Result<zup_transaction::ReconcileResult, Self::Error> {
        self.inner.reconcile(operation, receipt)
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
        let outside = zup_platform::TargetPath::new(target, "/etc/tool").expect("a path");
        assert!(
            authorize_machine_destination(
                &crate::lowering::to_host_path(&outside).expect("lowers"),
                &roots
            )
            .is_err()
        );
    }
}
