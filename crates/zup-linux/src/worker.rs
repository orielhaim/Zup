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
use crate::machine::{
    MachineRoots, authorize_machine_destination, authorize_machine_install_directory,
};
use crate::socket::{
    HANDSHAKE_TIMEOUT, PeerPin, peer_alive, peer_identity, pin_peer, recv_envelope, send_envelope,
};

pub const EXECUTE_TIMEOUT: Duration = Duration::from_secs(300);

pub const ACCEPT_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
pub struct WorkerContext {
    pub roots: MachineRoots,

    pub systemd: crate::machine::SystemdRoots,

    pub invoking_uid: u32,

    pub expected_client_pid: u32,

    pub session: SessionId,

    pub worker_exe: PathBuf,

    pub carrier_pin: Option<FilePin>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilePin {
    pub device: u64,
    pub inode: u64,
    pub size: u64,
}

impl FilePin {
    pub fn pin(path: &Path) -> Result<Self, IpcError> {
        let metadata = std::fs::metadata(path).map_err(|error| {
            IpcError::WorkerAuth(format!("carrier at `{}`: {error}", path.display()))
        })?;
        use std::os::unix::fs::MetadataExt as _;
        Ok(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            size: metadata.len(),
        })
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
    crate::fs::refuse_symlink_ancestors(directory).map_err(|error| {
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
}

impl SessionChannel<'_> {
    fn send(&mut self, message: Message) -> Result<(), IpcError> {
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
        target: prepared.target.clone(),
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

    let outcome = execute_prepared(channel, &prepared)?;
    channel.send(Message::Completed(zup_protocol::Completed {
        transaction_id: prepared.transaction_id,
        outcome: outcome.clone(),
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

fn worker_target(context: &WorkerContext) -> Result<zup_core::TargetTriple, IpcError> {
    let carrier = crate::carrier::Carrier::open(&context.worker_exe)
        .map_err(|error| IpcError::WorkerAuth(format!("worker carrier: {error}")))?;
    Ok(carrier.target().clone())
}

struct PreparedPlan {
    plan_digest: String,
    operation: String,
    app_id: String,
    app_version: String,
    scope: String,
    target: zup_core::TargetTriple,
    file_count: u32,
    transaction_id: uuid::Uuid,
    carrier_path: PathBuf,
    carrier_pin: Option<FilePin>,
    plan: zup_transaction::TransactionPlan,
    target_plan: zup_platform::TargetPlan,
    action: crate::run::LinuxAction,
    state_root: PathBuf,
    roots: MachineRoots,
    systemd: crate::machine::SystemdRoots,
    payload: PreparedPayload,

    _lock: zup_transaction::InstallationLock,
}

struct PreparedPayload {
    maintenance: Vec<u8>,
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
    let target = intent.target.clone();
    let app_id =
        AppId::new(&intent.app_id).map_err(|error| IpcError::Policy(format!("app id: {error}")))?;
    let app_version: semver::Version = intent
        .app_version
        .parse()
        .map_err(|error| IpcError::Policy(format!("app version: {error}")))?;

    let state_root = crate::machine::ensure_machine_state_root(
        &context.roots,
        rustix::process::geteuid().as_raw(),
    )
    .map_err(|error| IpcError::WorkerAuth(error.to_string()))?;
    crate::machine::verify_machine_structure(&state_root, rustix::process::geteuid().as_raw())
        .map_err(|error| IpcError::WorkerAuth(error.to_string()))?;
    crate::machine::normalize_state_modes(&state_root, rustix::process::geteuid().as_raw())
        .map_err(|error| IpcError::WorkerAuth(error.to_string()))?;

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
        zup_protocol::privileged_operation::INSTALL => crate::run::LinuxAction::Install,
        zup_protocol::privileged_operation::UPGRADE => crate::run::LinuxAction::Upgrade,
        zup_protocol::privileged_operation::REPAIR => crate::run::LinuxAction::Repair {
            force_files: intent.force_files,
        },
        zup_protocol::privileged_operation::UNINSTALL => crate::run::LinuxAction::Uninstall,
        zup_protocol::privileged_operation::APPLY => crate::run::LinuxAction::Apply,
        _ => {
            return Err(IpcError::WorkerProtocol(
                "unknown privileged operation".into(),
            ));
        }
    };

    let mut plan_request = zup_plan::PlanRequest::new(target.clone(), SelectedScope::Machine);
    for name in &intent.selected_components {
        let id = zup_core::ComponentId::new(name)
            .map_err(|error| IpcError::Policy(format!("component: {error}")))?;
        plan_request.components.enable.insert(id);
    }
    if let Some(directory) = &intent.install_dir_override {
        let host = PathBuf::from(directory);
        authorize_machine_install_directory(&host, &context.roots)
            .map_err(|error| IpcError::Policy(error.to_string()))?;
        let template = crate::machine::host_to_install_template(&host, &context.roots, &target)
            .map_err(|error| IpcError::Policy(format!("install directory override: {error}")))?;
        plan_request.install_directory = Some(template);
    }

    let install = zup_plan::plan(
        &zup_plan::BuildPlan {
            targets: vec![build],
        },
        &plan_request,
    )
    .map_err(|error| IpcError::Policy(format!("plan: {error}")))?;
    let mut target_plan = crate::resolve::resolve_target(
        &install,
        &crate::locations::LinuxInstallLocationResolver::with_machine_roots(context.roots.clone()),
    )
    .map_err(|error| IpcError::Policy(format!("target plan: {error}")))?;
    crate::run::attach_maintenance_copy_for(
        &mut target_plan,
        &carrier_path,
        &state_root,
        SelectedScope::Machine,
    )
    .map_err(|error| IpcError::Transaction(error.to_string()))?;
    enforce_machine_policy(&target_plan, &context.roots)?;
    if let Ok(host) = crate::lowering::to_host_path(&target_plan.install_directory) {
        authorize_machine_install_directory(&host, &context.roots)
            .map_err(|error| IpcError::Policy(error.to_string()))?;
        crate::fs::refuse_symlink_ancestors(&host)
            .map_err(|error| IpcError::Policy(format!("install destination: {error}")))?;
    }

    let ledger_store = crate::ledger::LinuxLedgerStore::new(&state_root);
    let lock_key = zup_transaction::InstallationLock::lock_key(app_id.as_str(), "machine");
    let lock = match zup_transaction::InstallationLock::try_acquire(&state_root, &lock_key)
        .map_err(|error| IpcError::Transaction(error.to_string()))?
    {
        Some(lock) => lock,
        None => return Err(IpcError::WorkerBusy),
    };
    crate::machine::normalize_state_modes(&state_root, rustix::process::geteuid().as_raw())
        .map_err(|error| IpcError::Transaction(error.to_string()))?;

    let settled_before = ledger_store
        .load(&app_id, SelectedScope::Machine)
        .map_err(|error| IpcError::Transaction(error.to_string()))?
        .map(|ledger| ledger.committed_transaction.clone());
    recover_pending(
        &state_root,
        &app_id,
        &carrier,
        &carrier_path,
        &context.roots,
        &context.systemd,
    )?;

    ledger_store
        .repair_committed(&target_plan.app.id, SelectedScope::Machine)
        .map_err(|error| IpcError::Transaction(error.to_string()))?;
    crate::machine::verify_ledger_trust(
        &state_root,
        &target_plan.app.id,
        rustix::process::geteuid().as_raw(),
    )
    .map_err(|error| IpcError::WorkerAuth(error.to_string()))?;
    let ledger = ledger_store
        .load(&target_plan.app.id, SelectedScope::Machine)
        .map_err(|error| IpcError::Transaction(error.to_string()))?;
    let action = crate::run::resolve_action(requested, ledger.as_ref(), &target_plan.app.version)
        .map_err(|error| IpcError::Policy(error.to_string()))?;

    let mut snapshot = crate::executor::snapshot_target(&target_plan);

    let needs_manager = crate::input::requires_service_manager(&target_plan, ledger.as_ref());
    let mut manager = if needs_manager {
        Some(
            crate::systemd::RealSystemd::connect()
                .map_err(|error| IpcError::Policy(format!("systemd is unavailable: {error}")))?,
        )
    } else {
        None
    };
    if let Some(manager) = manager.as_mut() {
        snapshot.services =
            crate::input::snapshot_services(&target_plan, manager, &context.systemd)
                .map_err(|error| IpcError::Policy(format!("service snapshot: {error}")))?;
    }
    let owned_matches = crate::run::inspect_owned_matches_for(ledger.as_ref());
    let execution = zup_exec::plan_lifecycle(
        action,
        (action != zup_exec::LifecycleAction::Uninstall).then_some(&target_plan),
        Some(&snapshot),
        ledger.as_ref(),
        &owned_matches,
    )
    .map_err(|error| IpcError::Policy(format!("lifecycle: {error}")))?;
    let force_services = matches!(
        requested,
        crate::run::LinuxAction::Repair { force_files: true }
    );
    let input = if needs_manager {
        let manager = manager.as_mut().ok_or_else(|| {
            IpcError::Policy("a service transaction without a systemd manager".into())
        })?;
        crate::input::compile_machine_execution_plan(
            &execution,
            &target_plan,
            crate::input::ServiceCompilation {
                roots: &context.roots,
                systemd: &context.systemd,
                manager,
                force_services,
            },
        )
        .map_err(|error| IpcError::Policy(format!("transaction input: {error}")))?
    } else {
        crate::input::compile_execution_plan(&execution, &target_plan)
            .map_err(|error| IpcError::Policy(format!("transaction input: {error}")))?
    };
    let plan = zup_transaction::compile_transaction(&input)
        .map_err(|error| IpcError::Policy(format!("transaction plan: {error}")))?;

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

    let maintenance = std::fs::read(&carrier_path).map_err(|error| {
        IpcError::Transaction(format!(
            "carrier bytes at `{}`: {error}",
            carrier_path.display()
        ))
    })?;
    let (maintenance_size, maintenance_sha256) = zup_core::hash_reader(maintenance.as_slice())
        .map_err(|error| IpcError::Transaction(format!("carrier bytes: {error}")))?;
    let file_count = u32::try_from(target_plan.files.len())
        .map_err(|_| IpcError::Policy("too many files".into()))?;
    Ok(PreparedPlan {
        plan_digest,
        operation: intent.operation.clone(),
        app_id: intent.app_id.clone(),
        app_version: intent.app_version.clone(),
        scope: intent.scope.clone(),
        target,
        file_count,
        transaction_id: uuid::Uuid::now_v7(),
        carrier_path,
        carrier_pin,
        plan,
        target_plan,
        action: match action {
            zup_exec::LifecycleAction::Install => crate::run::LinuxAction::Install,
            zup_exec::LifecycleAction::Upgrade => crate::run::LinuxAction::Upgrade,
            zup_exec::LifecycleAction::Repair { force_files } => {
                crate::run::LinuxAction::Repair { force_files }
            }
            zup_exec::LifecycleAction::Uninstall => crate::run::LinuxAction::Uninstall,
            zup_exec::LifecycleAction::Modify => {
                return Err(IpcError::Policy(
                    "a machine transaction modifies nothing outside its lifecycle".into(),
                ));
            }
        },
        state_root,
        roots: context.roots.clone(),
        systemd: context.systemd.clone(),
        payload: PreparedPayload {
            maintenance,
            maintenance_sha256,
            maintenance_size,
            generated: BTreeMap::new(),
            package: carrier.package().payload_source(),
        },
        _lock: lock,
    })
}

fn recover_pending(
    state_root: &Path,
    app_id: &AppId,
    carrier: &crate::carrier::Carrier,
    carrier_path: &Path,
    roots: &MachineRoots,
    systemd: &crate::machine::SystemdRoots,
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
                    roots,
                    systemd,
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
    roots: &MachineRoots,
    systemd: &crate::machine::SystemdRoots,
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
    let store = zup_transaction::FilesystemTransactionStore::new(state_root);
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

    let maintenance = std::fs::read(carrier_path).map_err(|error| {
        IpcError::Transaction(format!(
            "carrier bytes at `{}`: {error}",
            carrier_path.display()
        ))
    })?;
    let (maintenance_size, maintenance_sha256) = zup_core::hash_reader(maintenance.as_slice())
        .map_err(|error| IpcError::Transaction(format!("carrier bytes: {error}")))?;
    let payload = crate::run::RunnerPayload::from_prepared(
        carrier.package().payload_source(),
        maintenance,
        maintenance_sha256,
        maintenance_size,
        BTreeMap::new(),
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
            roots.clone(),
            systemd.clone(),
            manager,
            rustix::process::geteuid().as_raw(),
        ));
    }
    executor
        .register_plan(&record.plan)
        .map_err(|error| IpcError::Transaction(error.to_string()))?;
    let (record, outcome) = zup_transaction::recover(record, &store, &mut executor)
        .map_err(|error| IpcError::Transaction(error.to_string()))?;
    match outcome {
        zup_transaction::TransactionOutcome::Committed => {
            let ledger = ledger_store_publish(state_root, &record)?;

            if !record.plan.uninstall {
                crate::machine::normalize_published_modes(state_root, &ledger)
                    .map_err(|error| IpcError::Transaction(error.to_string()))?;
            }
            crate::machine::normalize_state_modes(state_root, rustix::process::geteuid().as_raw())
                .map_err(|error| IpcError::Transaction(error.to_string()))?;
            Ok(())
        }
        zup_transaction::TransactionOutcome::RolledBack => Ok(()),
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
    target: &zup_core::TargetTriple,
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
    target: &zup_core::TargetTriple,
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

fn execute_prepared(
    channel: &mut SessionChannel<'_>,
    prepared: &PreparedPlan,
) -> Result<String, IpcError> {
    let lock_key = zup_transaction::InstallationLock::lock_key(&prepared.app_id, "machine");
    let store = zup_transaction::FilesystemTransactionStore::new(&prepared.state_root);
    let coordinator = zup_transaction::TransactionCoordinator::new(store);
    let record = coordinator
        .begin(
            prepared.target_plan.app.id.clone(),
            SelectedScope::Machine,
            prepared.target_plan.app.version.clone(),
            prepared.plan.clone(),
        )
        .map_err(|error| IpcError::Transaction(error.to_string()))?;

    let payload = crate::run::RunnerPayload::from_prepared(
        prepared.payload.package.clone(),
        prepared.payload.maintenance.clone(),
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

    let (record, outcome) = coordinator
        .execute(record, &mut executor)
        .map_err(|error| IpcError::Transaction(error.to_string()))?;
    channel.send(Message::Progress(ProgressReport {
        kind: ProgressKind::OperationProgress,
        detail: "finishing".into(),
        completed: None,
        total: None,
    }))?;
    let ledger_store = crate::ledger::LinuxLedgerStore::new(&prepared.state_root);
    match outcome {
        zup_transaction::TransactionOutcome::Committed => {
            let ledger = ledger_store
                .publish_committed(&record, SelectedScope::Machine)
                .map_err(|error| IpcError::Transaction(error.to_string()))?;

            if prepared.action != crate::run::LinuxAction::Uninstall {
                crate::machine::normalize_published_modes(&prepared.state_root, &ledger)
                    .map_err(|error| IpcError::Transaction(error.to_string()))?;
            }
            crate::machine::normalize_state_modes(
                &prepared.state_root,
                rustix::process::geteuid().as_raw(),
            )
            .map_err(|error| IpcError::Transaction(error.to_string()))?;
            if prepared.action == crate::run::LinuxAction::Upgrade {
                crate::run::retire_old_generations_for(
                    &prepared.state_root,
                    &prepared.target_plan.app.id,
                    SelectedScope::Machine,
                    &ledger.version,
                )
                .map_err(|error| IpcError::Transaction(error.to_string()))?;
            }
            if prepared.action == crate::run::LinuxAction::Uninstall {
                let _ = zup_transaction::InstallationLock::remove_if_unheld(
                    &prepared.state_root,
                    &lock_key,
                );
            }
            Ok("committed".to_owned())
        }
        zup_transaction::TransactionOutcome::RolledBack => Ok("rolled_back".to_owned()),
        zup_transaction::TransactionOutcome::RecoveryRequired => Ok("recovery_required".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let target = zup_core::TargetTriple::parse("x86_64-unknown-linux-gnu").expect("a target");
        let roots = MachineRoots::production();
        let outside = zup_platform::TargetPath::new(target.clone(), "/etc/tool").expect("a path");
        assert!(
            authorize_machine_destination(
                &crate::lowering::to_host_path(&outside).expect("lowers"),
                &roots
            )
            .is_err()
        );
    }
}
