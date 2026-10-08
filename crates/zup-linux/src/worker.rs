//! The privileged worker: one session, one prepared plan, one exit.
//!
//! The worker is the privilege boundary. Everything the unprivileged process
//! sent - arguments, paths, intent, digests, the session itself - is
//! untrusted, including the [`PrepareOperation`] that proposes the work. The
//! worker independently establishes that the requested operation is one Zup
//! is allowed to perform, through this order:
//!
//! ```text
//! verify the peer (uid, pidfd pin, session)
//! verify the package from a trusted carrier
//! resolve machine locations itself
//! replan the machine transaction itself
//! enforce the privileged path policy
//! compare the plan digest with the authorized one
//! acquire the machine lock
//! execute, publish, exit
//! ```
//!
//! What the worker never does: execute application code, launch the installed
//! application, run plugins, run prerequisite installers, or expose any
//! operation outside the typed installation lifecycle. There is no generic
//! root RPC here - only prepare, execute, and exit.
//!
//! [`PrepareOperation`]: zup_protocol::PrepareOperation

use std::collections::BTreeMap;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use zup_core::{AppId, ResourceKey, SelectedScope};
use zup_protocol::{
    Capabilities, Message, PROTOCOL_VERSION, PrepareOperation, PreparedOperation,
    PrivilegedSession, ProgressKind, ProgressReport, SequenceTracker, SessionId, WireEnvelope,
    WorkerHello, failure,
};

use crate::machine::{
    MachineRoots, authorize_machine_destination, authorize_machine_install_directory,
};
use crate::socket::{
    HANDSHAKE_TIMEOUT, PeerPin, peer_alive, peer_identity, pin_peer, recv_envelope, send_envelope,
};

/// How long the worker waits for the final Execute after preparing.
///
/// Bounded: a prepared plan that is never authorized must not hold the
/// session - or the administrator's intent - open forever. No mutation has
/// happened at this point, so expiry means cleanup and exit.
pub const EXECUTE_TIMEOUT: Duration = Duration::from_secs(300);
/// How long the worker waits to be accepted after connecting.
pub const ACCEPT_TIMEOUT: Duration = Duration::from_secs(30);

/// Why the worker refused or failed.
#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    #[error("worker authentication failed: {0}")]
    AuthFailed(String),

    #[error("worker protocol failed: {0}")]
    Protocol(String),

    #[error("worker policy refusal: {0}")]
    Policy(String),

    #[error("worker transaction failed: {0}")]
    Transaction(String),

    #[error("cancelled")]
    Cancelled,

    /// The worker repaired or recovered state while preparing, so the
    /// client's digest is stale. The client re-plans and retries once with
    /// a fresh session.
    #[error("the confirmed plan went stale during preparation")]
    StalePlan,

    #[error("another operation holds this installation's lock")]
    Busy,
}

impl WorkerError {
    /// The closed failure kind the parent acts on.
    fn kind(&self) -> &'static str {
        match self {
            WorkerError::AuthFailed(_) => failure::AUTHENTICATION,
            WorkerError::Protocol(_) => failure::PROTOCOL,
            WorkerError::Policy(_) => failure::POLICY,
            WorkerError::Transaction(_) => failure::TRANSACTION,
            WorkerError::Cancelled => failure::CANCELLED,
            WorkerError::StalePlan => failure::STALE_PLAN,
            WorkerError::Busy => failure::INSTALLATION_BUSY,
        }
    }
}

/// The worker's trusted context: everything it enforces but never accepts
/// from the peer.
#[derive(Debug, Clone)]
pub struct WorkerContext {
    /// The roots the worker enforces. Always production in a real worker;
    /// isolated roots exist only for tests driving [`serve_session`].
    pub roots: MachineRoots,
    /// The systemd unit-source root the worker enforces. Always production
    /// in a real worker; isolated in tests so no test writes the host's
    /// unit tree.
    pub systemd: crate::machine::SystemdRoots,
    /// The uid `pkexec` reports as the authorizing user. The peer must be it.
    pub invoking_uid: u32,
    /// The client process the worker serves: the peer pid must equal this,
    /// pinned for the session. The value arrives over an untrusted channel
    /// and is verified against kernel credentials, never trusted as a
    /// claim - but without it, any same-user process that reached the
    /// rendezvous first could drive the session.
    pub expected_client_pid: u32,
    /// This elevation's session. Frames outside it are cross-session
    /// confusion and are refused.
    pub session: SessionId,
    /// This worker's own executable: the carrier for an initial install or
    /// upgrade, and the fallback when no trusted maintenance exists.
    pub worker_exe: PathBuf,
    /// Inode pin of the carrier file at verification time, to detect a
    /// swap of a user-writable installer between validation and use.
    pub carrier_pin: Option<FilePin>,
}

/// A file's identity at verification time: device, inode, and size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilePin {
    pub device: u64,
    pub inode: u64,
    pub size: u64,
}

impl FilePin {
    /// Pin the file currently at `path`.
    pub fn pin(path: &Path) -> Result<Self, WorkerError> {
        let metadata = std::fs::metadata(path).map_err(|error| {
            WorkerError::AuthFailed(format!("carrier at `{}`: {error}", path.display()))
        })?;
        use std::os::unix::fs::MetadataExt as _;
        Ok(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            size: metadata.len(),
        })
    }

    /// Prove the file at `path` is still the pinned one.
    pub fn verify(&self, path: &Path) -> Result<(), WorkerError> {
        let current = Self::pin(path)?;
        if current != *self {
            return Err(WorkerError::AuthFailed(
                "the installer changed between verification and execution".to_owned(),
            ));
        }
        Ok(())
    }
}

/// Entry for the `__privileged-worker` mode: serve one session, exit.
///
/// Refuses unless this process is uid 0 with a `PKEXEC_UID` authorizing
/// user: the worker mode is an internal detail, not a command anyone runs
/// by hand to gain authority they do not have. The expected client pid
/// arrives over the untrusted command line and is verified against kernel
/// peer credentials before anything is served.
///
/// The socket pathname also arrives over the command line - `pkexec`
/// sanitizes the environment, so the worker cannot re-derive the client's
/// rendezvous from `XDG_RUNTIME_DIR` or from an independently generated
/// fallback directory. The pathname is untrusted and fully validated by
/// [`validate_rendezvous`]: the containing directory must be real, owned by
/// the authorizing user, and private to them, and the peer on the other end
/// must still prove uid, pid, session, and liveness.
pub fn run_worker_mode(
    session: SessionId,
    expected_client_pid: u32,
    socket_path: &Path,
) -> Result<String, WorkerError> {
    if rustix::process::geteuid().as_raw() != 0 {
        return Err(WorkerError::AuthFailed(
            "the privileged worker runs as root after authorization".into(),
        ));
    }
    if expected_client_pid == 0 {
        return Err(WorkerError::AuthFailed("no client process".into()));
    }
    let invoking = std::env::var("PKEXEC_UID")
        .map_err(|_| WorkerError::AuthFailed("no authorizing user".into()))?
        .parse::<u32>()
        .map_err(|_| WorkerError::AuthFailed("no authorizing user".into()))?;
    // Private state first: anything the worker creates from here is
    // root-owned and never group- or world-writable unless explicitly
    // published otherwise below.
    let previous_umask = rustix::process::umask(rustix::fs::Mode::from_bits_truncate(0o077));
    let _ = previous_umask;
    let worker_exe = std::env::current_exe()
        .map_err(|error| WorkerError::AuthFailed(format!("own executable: {error}")))?;
    validate_rendezvous(socket_path, invoking)?;
    let mut stream = crate::socket::connect(socket_path, ACCEPT_TIMEOUT)
        .map_err(|error| WorkerError::Protocol(error.to_string()))?;
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

/// Prove the rendezvous the worker is about to use belongs to the session.
///
/// The socket pathname arrives over the untrusted command line - the only
/// rendezvous establishment that survives a sanitized `pkexec` environment -
/// so every property is checked: the pathname is absolute and names the
/// session's socket inside its directory, the directory is real, owned by
/// the authorizing user, and private to them, and it holds no symlink on the
/// way down. A replaced pathname or a raced endpoint fails here, before any
/// peer is trusted. Peer uid/pid, session, and liveness are verified
/// separately once connected.
pub(crate) fn validate_rendezvous(socket: &Path, invoking_uid: u32) -> Result<(), WorkerError> {
    if !socket.is_absolute() {
        return Err(WorkerError::AuthFailed(
            "the rendezvous is an absolute pathname".into(),
        ));
    }
    if socket.file_name().is_none_or(|name| name != "worker.sock") {
        return Err(WorkerError::AuthFailed(
            "the rendezvous names the session socket".into(),
        ));
    }
    let directory = socket
        .parent()
        .ok_or_else(|| WorkerError::AuthFailed("the rendezvous has no directory".into()))?;
    crate::fs::refuse_symlink_ancestors(directory).map_err(|error| {
        WorkerError::AuthFailed(format!(
            "the rendezvous must not pass through a link: {error}"
        ))
    })?;
    let metadata = std::fs::symlink_metadata(directory).map_err(|error| {
        WorkerError::AuthFailed(format!("rendezvous at `{}`: {error}", directory.display()))
    })?;
    use std::os::unix::fs::MetadataExt as _;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(WorkerError::AuthFailed(
            "the rendezvous is a real directory, not a link".into(),
        ));
    }
    if metadata.uid() != invoking_uid {
        return Err(WorkerError::AuthFailed(
            "the rendezvous belongs to the authorizing user".into(),
        ));
    }
    if metadata.mode() & 0o077 != 0 {
        return Err(WorkerError::AuthFailed(
            "the rendezvous is private to the authorizing user".into(),
        ));
    }
    Ok(())
}

/// Serve one privileged session over an already-connected stream.
///
/// Shared by the real worker, the already-root loopback, and the tests: the
/// transport differs, the verification does not. Returns the terminal
/// outcome name (`committed`, `rolled_back`, `recovery_required`).
///
/// Every refusal and failure is reported as a typed `Failed` message before
/// the worker exits, so the client learns the kind rather than a closed
/// socket: busy, authentication, policy, protocol, transaction, or
/// cancelled. Best effort at the very end - a dead client cannot be told.
pub fn serve_session(
    stream: &mut UnixStream,
    context: WorkerContext,
) -> Result<String, WorkerError> {
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

/// One session's framing: bounded envelopes in, bounded envelopes out, both
/// bound to the session and the sequence.
struct SessionChannel<'a> {
    stream: &'a mut UnixStream,
    session: SessionId,
    incoming: SequenceTracker,
    outgoing: u64,
}

impl SessionChannel<'_> {
    fn send(&mut self, message: Message) -> Result<(), WorkerError> {
        let envelope = WireEnvelope {
            version: PROTOCOL_VERSION,
            session_id: self.session,
            sequence: self.outgoing,
            message,
        };
        send_envelope(self.stream, &envelope)
            .map_err(|error| WorkerError::Protocol(error.to_string()))?;
        self.outgoing = self.outgoing.saturating_add(1);
        Ok(())
    }

    fn recv(&mut self, timeout: Duration) -> Result<WireEnvelope, WorkerError> {
        let envelope = recv_envelope(self.stream, timeout)
            .map_err(|error| WorkerError::Protocol(error.to_string()))?;
        self.incoming
            .accept(envelope.sequence)
            .map_err(|error| WorkerError::Protocol(error.to_string()))?;
        if envelope.session_id != self.session {
            return Err(WorkerError::AuthFailed("session mismatch".into()));
        }
        if envelope.version != PROTOCOL_VERSION {
            return Err(WorkerError::Protocol("protocol version mismatch".into()));
        }
        Ok(envelope)
    }
}

fn serve_inner(
    channel: &mut SessionChannel<'_>,
    context: &WorkerContext,
) -> Result<String, WorkerError> {
    // The peer is whoever holds the other end right now - not whoever the
    // first message claims to be. Both halves are kernel evidence: the uid
    // must be the authorizing user, and the pid must be the exact client
    // process this session was launched for.
    let peer = peer_identity(channel.stream)
        .map_err(|error| WorkerError::AuthFailed(error.to_string()))?;
    if peer.uid != context.invoking_uid {
        return Err(WorkerError::AuthFailed(format!(
            "client uid {} is not the authorizing user",
            peer.uid
        )));
    }
    if peer.pid != context.expected_client_pid {
        return Err(WorkerError::AuthFailed(
            "client process is not the launched installer".into(),
        ));
    }
    let pin =
        pin_peer(peer.pid, peer.uid).map_err(|error| WorkerError::AuthFailed(error.to_string()))?;

    // WorkerHello first: the client must know it reached the privileged
    // worker for this session before it proposes anything.
    let target = worker_target(context)?;
    channel.send(Message::WorkerHello(WorkerHello {
        protocol_version: PROTOCOL_VERSION,
        session_id: context.session,
        target,
        worker_pid: std::process::id(),
        capabilities: worker_capabilities(),
    }))?;

    // Exactly one Prepare per session.
    let mut session_state = PrivilegedSession::new(context.session);
    let envelope = channel.recv(HANDSHAKE_TIMEOUT)?;
    let prepared = match envelope.message {
        Message::Prepare(intent) => prepare_operation(context, &pin, intent)?,
        Message::Cancel => return Err(WorkerError::Cancelled),
        _ => return Err(WorkerError::Protocol("expected Prepare".into())),
    };
    session_state
        .prepared(context.session, &prepared.plan_digest)
        .map_err(|error| WorkerError::Protocol(error.to_string()))?;
    channel.send(Message::Prepared(PreparedOperation {
        plan_digest: prepared.plan_digest.clone(),
        operation: prepared.operation.clone(),
        app_id: prepared.app_id.clone(),
        app_version: prepared.app_version.clone(),
        scope: prepared.scope.clone(),
        target: prepared.target.clone(),
        file_count: prepared.file_count,
    }))?;

    // No mutation has happened yet: a client that disappears here leaves
    // nothing behind, and expiry cleans the session and exits.
    let envelope = channel.recv(EXECUTE_TIMEOUT)?;
    let execute = match envelope.message {
        Message::Execute(execute) => execute,
        Message::Cancel => return Err(WorkerError::Cancelled),
        _ => return Err(WorkerError::Protocol("expected Execute".into())),
    };
    session_state
        .execute(context.session, &execute.plan_digest)
        .map_err(|error| match error {
            zup_protocol::WireError::PlanHashMismatch => {
                WorkerError::AuthFailed("execute does not name the prepared plan".into())
            }
            zup_protocol::WireError::Replay => WorkerError::Protocol("execute replayed".into()),
            other => WorkerError::Protocol(other.to_string()),
        })?;
    // The peer must still be the authenticated process: authorization ends
    // when it goes away.
    if !peer_alive(&pin) {
        return Err(WorkerError::AuthFailed(
            "the authorized client is gone".into(),
        ));
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

/// The worker's capabilities: file transactions plus the lifecycle it drives.
fn worker_capabilities() -> Capabilities {
    Capabilities {
        file_transactions_v1: true,
        backend_operations_v1: false,
        lifecycle_v1: true,
        prerequisite_bootstrap_v1: false,
    }
}

/// The Linux target this worker serves, read from its own carrier.
fn worker_target(context: &WorkerContext) -> Result<zup_core::TargetTriple, WorkerError> {
    let carrier = crate::carrier::Carrier::open(&context.worker_exe)
        .map_err(|error| WorkerError::AuthFailed(format!("worker carrier: {error}")))?;
    Ok(carrier.target().clone())
}

/// One prepared operation: the verified plan plus where it came from.
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
    /// The machine lock, held from preparation through execution.
    ///
    /// Holding it across the Prepare→Execute round trip is what makes the
    /// root lock authoritative: a second worker preparing the same
    /// installation refuses while this session is still deciding, rather
    /// than racing it to the journal.
    _lock: zup_transaction::InstallationLock,
}

/// Payload bytes pinned at preparation: the verified carrier's own bytes
/// plus the deterministic generated map (empty in machine scope).
struct PreparedPayload {
    maintenance: Vec<u8>,
    maintenance_sha256: zup_core::Sha256Digest,
    maintenance_size: u64,
    generated: BTreeMap<String, Vec<u8>>,
    package: zup_bundle::PackagePayloadSource,
}

/// Reconstruct the privileged plan from the intent - never from a plan the
/// client sends, because the client sends none.
///
/// Every field of the intent is revalidated against the trusted carrier and
/// the machine policy. A mismatch anywhere refuses the session before the
/// lock is taken and before any journal exists.
fn prepare_operation(
    context: &WorkerContext,
    _pin: &PeerPin,
    intent: PrepareOperation,
) -> Result<PreparedPlan, WorkerError> {
    intent
        .validate_shape()
        .map_err(|error| WorkerError::Protocol(error.to_string()))?;
    if intent.scope != "machine" {
        return Err(WorkerError::Policy(
            "the privileged worker serves machine scope only".into(),
        ));
    }
    if intent.target.operating_system() != zup_core::TargetOperatingSystem::Linux {
        return Err(WorkerError::Policy(
            "the privileged worker serves Linux targets only".into(),
        ));
    }
    let target = intent.target.clone();
    let app_id = AppId::new(&intent.app_id)
        .map_err(|error| WorkerError::Policy(format!("app id: {error}")))?;
    let app_version: semver::Version = intent
        .app_version
        .parse()
        .map_err(|error| WorkerError::Policy(format!("app version: {error}")))?;

    // Machine state first: untrusted state is never planned against.
    // Structure is verified before modes are normalized - journals a
    // previous run left behind carry whatever mode the old umask gave
    // them - and the normalization refuses the same evil it fixes.
    let state_root = crate::machine::ensure_machine_state_root(
        &context.roots,
        rustix::process::geteuid().as_raw(),
    )
    .map_err(|error| WorkerError::AuthFailed(error.to_string()))?;
    crate::machine::verify_machine_structure(&state_root, rustix::process::geteuid().as_raw())
        .map_err(|error| WorkerError::AuthFailed(error.to_string()))?;
    crate::machine::normalize_state_modes(&state_root, rustix::process::geteuid().as_raw())
        .map_err(|error| WorkerError::AuthFailed(error.to_string()))?;

    // The carrier: the root-owned maintenance generation when it declares
    // this operation, else this worker's own executable when it does.
    // Whoever the client ran, the bytes root trusts are root's.
    let (carrier_path, carrier_pin) = select_trusted_carrier(
        &state_root,
        &context.worker_exe,
        &app_id,
        &app_version,
        &target,
    )?;
    let carrier = crate::carrier::Carrier::open(&carrier_path)
        .map_err(|error| WorkerError::AuthFailed(format!("trusted carrier: {error}")))?;
    verify_carrier_declares(&carrier, &app_id, &app_version, &target)?;

    let mut targets = carrier
        .package()
        .build_plan()
        .map_err(|error| WorkerError::Policy(format!("package: {error}")))?
        .targets;
    if targets.len() != 1 {
        return Err(WorkerError::Policy(format!(
            "an installer package holds exactly one target; this one holds {}",
            targets.len()
        )));
    }
    let build = targets.remove(0);
    // Plugins would have to execute to plan: refused before planning.
    if !build.installer.plugins.is_empty() {
        return Err(WorkerError::Policy(
            "machine scope refuses projects that need plugin execution in the privileged worker"
                .into(),
        ));
    }
    if !build.installer.prerequisites.is_empty() {
        return Err(WorkerError::Policy(
            "machine scope runs no prerequisite installers as root".into(),
        ));
    }
    if !build.installer.install.scope.allows_machine() {
        return Err(WorkerError::Policy(
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
        _ => return Err(WorkerError::Protocol("unknown privileged operation".into())),
    };

    // Components and the install-directory override are caller choices the
    // worker re-applies to its own planning - and re-validates.
    let mut plan_request = zup_plan::PlanRequest::new(target.clone(), SelectedScope::Machine);
    for name in &intent.selected_components {
        let id = zup_core::ComponentId::new(name)
            .map_err(|error| WorkerError::Policy(format!("component: {error}")))?;
        plan_request.components.enable.insert(id);
    }
    if let Some(directory) = &intent.install_dir_override {
        let host = PathBuf::from(directory);
        authorize_machine_install_directory(&host, &context.roots)
            .map_err(|error| WorkerError::Policy(error.to_string()))?;
        let template = crate::machine::host_to_install_template(&host, &context.roots, &target)
            .map_err(|error| WorkerError::Policy(format!("install directory override: {error}")))?;
        plan_request.install_directory = Some(template);
    }

    let install = zup_plan::plan(
        &zup_plan::BuildPlan {
            targets: vec![build],
        },
        &plan_request,
    )
    .map_err(|error| WorkerError::Policy(format!("plan: {error}")))?;
    let mut target_plan = crate::resolve::resolve_target_with(
        &install,
        &crate::locations::LinuxInstallLocationResolver::with_machine_roots(context.roots.clone()),
    )
    .map_err(|error| WorkerError::Policy(format!("target resolution: {error}")))?;
    crate::run::attach_maintenance_copy_for(
        &mut target_plan,
        &carrier_path,
        &state_root,
        SelectedScope::Machine,
    )
    .map_err(|error| WorkerError::Transaction(error.to_string()))?;
    crate::capabilities::validate_target_plan(&target_plan)
        .map_err(|error| WorkerError::Policy(format!("capabilities: {error}")))?;
    enforce_machine_policy(&target_plan, &context.roots)?;
    if let Ok(host) = crate::lowering::to_host_path(&target_plan.install_directory) {
        authorize_machine_install_directory(&host, &context.roots)
            .map_err(|error| WorkerError::Policy(error.to_string()))?;
        crate::fs::refuse_symlink_ancestors(&host)
            .map_err(|error| WorkerError::Policy(format!("install destination: {error}")))?;
    }

    // Recovery first: an interrupted machine transaction is repaired before
    // any new planning, because planning against a stale ledger plans the
    // wrong transition.
    let ledger_store = crate::ledger::LinuxLedgerStore::new(&state_root);
    let lock_key = zup_transaction::InstallationLock::lock_key(app_id.as_str(), "machine");
    let lock = match zup_transaction::InstallationLock::try_acquire(&state_root, &lock_key)
        .map_err(|error| WorkerError::Transaction(error.to_string()))?
    {
        Some(lock) => lock,
        None => return Err(WorkerError::Busy),
    };
    crate::machine::normalize_state_modes(&state_root, rustix::process::geteuid().as_raw())
        .map_err(|error| WorkerError::Transaction(error.to_string()))?;
    // What the ledger said before this session touched it: repair or
    // recovery below may publish a newer truth, in which case a digest the
    // client bound beforehand is stale rather than forged.
    let settled_before = ledger_store
        .load(&app_id, SelectedScope::Machine)
        .map_err(|error| WorkerError::Transaction(error.to_string()))?
        .map(|ledger| ledger.committed_transaction.clone());
    recover_pending(
        &state_root,
        &app_id,
        &carrier,
        &carrier_path,
        &context.roots,
        &context.systemd,
    )?;
    // Re-establish the gap repair after recovery: a commit the recovery
    // published must be visible before the new plan reads the ledger.
    ledger_store
        .repair_committed(&target_plan.app.id, SelectedScope::Machine)
        .map_err(|error| WorkerError::Transaction(error.to_string()))?;
    crate::machine::verify_ledger_trust(
        &state_root,
        &target_plan.app.id,
        rustix::process::geteuid().as_raw(),
    )
    .map_err(|error| WorkerError::AuthFailed(error.to_string()))?;
    let ledger = ledger_store
        .load(&target_plan.app.id, SelectedScope::Machine)
        .map_err(|error| WorkerError::Transaction(error.to_string()))?;
    let action = crate::run::resolve_action(requested, ledger.as_ref(), &target_plan.app.version)
        .map_err(|error| WorkerError::Policy(error.to_string()))?;

    let mut snapshot = crate::snapshot::snapshot_target(&target_plan);
    // Services observe through the manager: unit source plus persistent
    // start policy. The decision is `requires_service_manager` - the same
    // predicate the unprivileged planner uses - so an upgrade that removes
    // the final service (or a full uninstall) still snapshots and compiles
    // through the manager on both sides. File-only installers never touch
    // the bus.
    let needs_manager = crate::input::requires_service_manager(&target_plan, ledger.as_ref());
    let mut manager = if needs_manager {
        Some(
            crate::systemd::RealSystemd::connect()
                .map_err(|error| WorkerError::Policy(format!("systemd is unavailable: {error}")))?,
        )
    } else {
        None
    };
    if let Some(manager) = manager.as_mut() {
        snapshot.services =
            crate::snapshot::snapshot_services(&target_plan, manager, &context.systemd)
                .map_err(|error| WorkerError::Policy(format!("service snapshot: {error}")))?;
    }
    let owned_matches = crate::run::inspect_owned_matches_for(ledger.as_ref());
    let execution = zup_exec::plan_lifecycle(
        action,
        (action != zup_exec::LifecycleAction::Uninstall).then_some(&target_plan),
        Some(&snapshot),
        ledger.as_ref(),
        &owned_matches,
    )
    .map_err(|error| WorkerError::Policy(format!("lifecycle: {error}")))?;
    let force_services = matches!(
        requested,
        crate::run::LinuxAction::Repair { force_files: true }
    );
    let input = if needs_manager {
        let manager = manager.as_mut().ok_or_else(|| {
            WorkerError::Policy("a service transaction without a systemd manager".into())
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
        .map_err(|error| WorkerError::Policy(format!("transaction input: {error}")))?
    } else {
        crate::input::compile_execution_plan(&execution, &target_plan)
            .map_err(|error| WorkerError::Policy(format!("transaction input: {error}")))?
    };
    let plan = zup_transaction::compile_transaction(&input)
        .map_err(|error| WorkerError::Policy(format!("transaction plan: {error}")))?;
    // Only typed service operations and derived-cache refreshes travel as
    // backend nodes: anything else backend-shaped is foreign to this worker.
    if plan.nodes.iter().any(|node| {
        matches!(
            &node.kind,
            zup_transaction::NodeKind::BackendOperation { .. }
                | zup_transaction::NodeKind::BackendRemoval { .. }
        ) && !is_expected_backend(node)
    }) {
        return Err(WorkerError::Policy(
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
        .map_err(|error| WorkerError::Transaction(error.to_string()))?;

    let plan_digest = plan.fingerprint().to_hex();
    if let Some(expected) = &intent.expected_plan_digest
        && expected.to_lowercase() != plan_digest
    {
        // State settled while preparing (a repair published, a recovery
        // committed) exactly when the client's digest predates it: stale,
        // not forged. Anything else is a substitution and refuses as one.
        let settled_after = ledger_store
            .load(&target_plan.app.id, SelectedScope::Machine)
            .map_err(|error| WorkerError::Transaction(error.to_string()))?
            .map(|ledger| ledger.committed_transaction.clone());
        if settled_after != settled_before {
            return Err(WorkerError::StalePlan);
        }
        return Err(WorkerError::AuthFailed(
            "the reconstructed plan differs from the authorized one".into(),
        ));
    }

    // Pin the payload bytes the plan was built from.
    let maintenance = std::fs::read(&carrier_path).map_err(|error| {
        WorkerError::Transaction(format!(
            "carrier bytes at `{}`: {error}",
            carrier_path.display()
        ))
    })?;
    let (maintenance_size, maintenance_sha256) = zup_core::hash_reader(maintenance.as_slice())
        .map_err(|error| WorkerError::Transaction(format!("carrier bytes: {error}")))?;
    let file_count = u32::try_from(target_plan.files.len())
        .map_err(|_| WorkerError::Policy("too many files".into()))?;
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
                return Err(WorkerError::Policy(
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

/// Recover an interrupted machine transaction before accepting new work.
///
/// The portable journal model, not a privileged-only format: the record is
/// replayed with the trusted carrier's payload, exactly as maintenance
/// recovery replays it. A recovery that cannot complete refuses the new
/// operation rather than mutating beside the unfinished one.
fn recover_pending(
    state_root: &Path,
    app_id: &AppId,
    carrier: &crate::carrier::Carrier,
    carrier_path: &Path,
    roots: &MachineRoots,
    systemd: &crate::machine::SystemdRoots,
) -> Result<(), WorkerError> {
    let ledger_store = crate::ledger::LinuxLedgerStore::new(state_root);
    loop {
        match ledger_store.repair_committed(app_id, SelectedScope::Machine) {
            Ok(()) => return Ok(()),
            Err(crate::ledger::LinuxLedgerError::RecoveryRequired(transaction)) => {
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
            Err(error) => return Err(WorkerError::Transaction(error.to_string())),
        }
    }
}

/// Recover one interrupted machine transaction from the journal.
fn recover_one(
    state_root: &Path,
    app_id: &AppId,
    carrier: &crate::carrier::Carrier,
    carrier_path: &Path,
    transaction: &str,
    roots: &MachineRoots,
    systemd: &crate::machine::SystemdRoots,
) -> Result<(), WorkerError> {
    let id = transaction
        .parse::<uuid::Uuid>()
        .map_err(|_| WorkerError::Transaction("unparsable transaction identity".into()))?;
    // The journal earns trust the same way the ledger does: a root-owned
    // regular file, never a link or a user-writable note.
    crate::machine::verify_trusted_state_file(
        &state_root
            .join("transactions")
            .join(id.to_string())
            .join("transaction.json"),
        rustix::process::geteuid().as_raw(),
    )
    .map_err(|error| WorkerError::AuthFailed(error.to_string()))?;
    let store = zup_transaction::FilesystemTransactionStore::new(state_root);
    let record = zup_transaction::TransactionStore::load(
        &store,
        &zup_transaction::TransactionId::from_uuid(id),
    )
    .map_err(|error| WorkerError::Transaction(error.to_string()))?;
    if record.app_id != *app_id || record.scope != SelectedScope::Machine {
        return Err(WorkerError::Transaction(
            "recovery record identity mismatch".into(),
        ));
    }
    // The payload serves the record's digests from the trusted carrier: the
    // carrier bytes read here are the same bytes the plan was verified
    // against, so recovery replays what was journaled, not what a download
    // happens to hold today.
    let maintenance = std::fs::read(carrier_path).map_err(|error| {
        WorkerError::Transaction(format!(
            "carrier bytes at `{}`: {error}",
            carrier_path.display()
        ))
    })?;
    let (maintenance_size, maintenance_sha256) = zup_core::hash_reader(maintenance.as_slice())
        .map_err(|error| WorkerError::Transaction(format!("carrier bytes: {error}")))?;
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
            .map_err(|error| WorkerError::Transaction(error.to_string()))?;
        executor = executor.with_services(crate::executor::ServiceSupport::isolated(
            roots.clone(),
            systemd.clone(),
            manager,
            rustix::process::geteuid().as_raw(),
        ));
    }
    executor
        .register_plan(&record.plan)
        .map_err(|error| WorkerError::Transaction(error.to_string()))?;
    let (record, outcome) = zup_transaction::recover(record, &store, &mut executor)
        .map_err(|error| WorkerError::Transaction(error.to_string()))?;
    match outcome {
        zup_transaction::TransactionOutcome::Committed => {
            let ledger = ledger_store_publish(state_root, &record)?;
            // A recovered uninstall publishes no ledger either.
            if !record.plan.uninstall {
                crate::machine::normalize_published_modes(state_root, &ledger)
                    .map_err(|error| WorkerError::Transaction(error.to_string()))?;
            }
            crate::machine::normalize_state_modes(state_root, rustix::process::geteuid().as_raw())
                .map_err(|error| WorkerError::Transaction(error.to_string()))?;
            Ok(())
        }
        zup_transaction::TransactionOutcome::RolledBack => Ok(()),
        zup_transaction::TransactionOutcome::RecoveryRequired => Err(WorkerError::Transaction(
            format!("transaction {transaction} requires recovery"),
        )),
    }
}

/// Publish a recovered commit through the machine ledger store.
fn ledger_store_publish(
    state_root: &Path,
    record: &zup_transaction::TransactionRecord,
) -> Result<zup_exec::InstallLedger, WorkerError> {
    crate::ledger::LinuxLedgerStore::new(state_root)
        .publish_committed(record, SelectedScope::Machine)
        .map_err(|error| WorkerError::Transaction(error.to_string()))
}

/// Whether a backend node is one the machine worker emitted: a typed
/// systemd service operation or a derived-cache refresh. Anything else
/// backend-shaped is foreign to this worker.
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

/// Enforce the privileged destination policy over the lowered desired state.
///
/// The worker is the final policy enforcement point: even a manifest parser
/// that already refused something does not excuse the worker from proving
/// every privileged mutation belongs to an allowed root. One violation
/// refuses the whole transaction before any mutation.
fn enforce_machine_policy(
    target: &zup_platform::TargetPlan,
    roots: &MachineRoots,
) -> Result<(), WorkerError> {
    let policy = |path: &zup_platform::TargetPath| {
        let host = crate::lowering::to_host_path(path)
            .map_err(|error| WorkerError::Policy(format!("destination: {error}")))?;
        authorize_machine_destination(&host, roots)
            .map_err(|error| WorkerError::Policy(error.to_string()))
    };
    let install_host = crate::lowering::to_host_path(&target.install_directory)
        .map_err(|error| WorkerError::Policy(format!("install directory: {error}")))?;
    authorize_machine_install_directory(&install_host, roots)
        .map_err(|error| WorkerError::Policy(error.to_string()))?;
    for file in &target.files {
        let destination = policy(&file.destination)?;
        match &file.key {
            ResourceKey::Maintenance { .. } => {
                if destination != crate::machine::MachineDestination::MachineState {
                    return Err(WorkerError::Policy(
                        "maintenance belongs to machine state".into(),
                    ));
                }
            }
            ResourceKey::File { .. } => {
                if destination != crate::machine::MachineDestination::Programs
                    && destination != crate::machine::MachineDestination::SharedData
                {
                    return Err(WorkerError::Policy(
                        "payload belongs to the program or variable-data tree".into(),
                    ));
                }
            }
            _ => {
                return Err(WorkerError::Policy(format!(
                    "machine scope holds no {:?} resources",
                    file.key
                )));
            }
        }
    }
    // Service binaries are trusted machine content: the executable must
    // resolve under the program tree and correspond to a Zup-owned
    // executable payload. The live ownership bits are revalidated at apply
    // time; this is the up-front policy half, before any journal exists.
    if target.scope == SelectedScope::Machine && !target.services.is_empty() {
        let mut target_files = std::collections::BTreeMap::new();
        for file in &target.files {
            target_files.insert(file.destination.to_string(), file.executable);
        }
        for service in &target.services {
            let unit = crate::services::unit_name(&service.id)
                .map_err(|error| WorkerError::Policy(format!("service identity: {error}")))?;
            // The unit name is derived here so an unrepresentable identity
            // refuses before the transaction exists.
            let _ = unit;
            crate::service_ops::validate_executable(
                &service.command,
                &target_files,
                roots,
                rustix::process::geteuid().as_raw(),
                false,
            )
            .map_err(|error| WorkerError::Policy(format!("service binary: {error}")))?;
        }
    }
    Ok(())
}

/// Choose the carrier the worker trusts for this operation.
///
/// The root-owned maintenance generation wins when it declares the requested
/// identity: after the original download is gone it is the only copy root
/// trusts, and an arbitrary file in `~/Downloads` must not override it. The
/// worker's own executable serves an initial install, an upgrade to a new
/// version the maintenance generation cannot declare, and the fallback when
/// no trusted maintenance exists yet.
fn select_trusted_carrier(
    state_root: &Path,
    worker_exe: &Path,
    app_id: &AppId,
    app_version: &semver::Version,
    target: &zup_core::TargetTriple,
) -> Result<(PathBuf, Option<FilePin>), WorkerError> {
    let maintenance = zup_transaction::maintenance_runtime_path(
        state_root,
        app_id,
        SelectedScope::Machine,
        app_version,
        target.executable_suffix(),
    );
    if std::fs::symlink_metadata(&maintenance).is_ok() {
        // An existing maintenance generation that fails trust is not a
        // fallback case: the machine state is suspect, and installing from
        // an arbitrary executable over it would launder that suspicion into
        // a trusted install. Refuse outright.
        crate::machine::verify_trusted_state_file(
            &maintenance,
            rustix::process::geteuid().as_raw(),
        )
        .map_err(|error| WorkerError::AuthFailed(error.to_string()))?;
        if let Ok(carrier) = crate::carrier::Carrier::open(&maintenance)
            && verify_carrier_declares(&carrier, app_id, app_version, target).is_ok()
        {
            // Root-owned and verified: pinning is the directory's ownership,
            // which the hierarchy check already established.
            return Ok((maintenance, None));
        }
    }
    // The worker's own executable: pin the inode so a user-writable
    // installer swapped after validation is detected, not installed.
    let pin = FilePin::pin(worker_exe)?;
    if let Ok(carrier) = crate::carrier::Carrier::open(worker_exe)
        && verify_carrier_declares(&carrier, app_id, app_version, target).is_ok()
    {
        return Ok((worker_exe.to_path_buf(), Some(pin)));
    }
    Err(WorkerError::AuthFailed(
        "no trusted carrier declares the requested operation".into(),
    ))
}

/// Prove the carrier declares exactly the requested identity: structure,
/// digest, target, application, version, and machine scope.
fn verify_carrier_declares(
    carrier: &crate::carrier::Carrier,
    app_id: &AppId,
    app_version: &semver::Version,
    target: &zup_core::TargetTriple,
) -> Result<(), WorkerError> {
    let installer = carrier
        .package()
        .build_plan()
        .map_err(|error| WorkerError::AuthFailed(format!("package: {error}")))?
        .targets
        .into_iter()
        .next()
        .ok_or_else(|| WorkerError::AuthFailed("the package declares no target".into()))?;
    if &installer.installer.app.id != app_id
        || &installer.installer.app.version != app_version
        || &installer.installer.target != target
        || !installer.installer.install.scope.allows_machine()
    {
        return Err(WorkerError::AuthFailed(
            "the carrier does not declare the requested machine operation".into(),
        ));
    }
    Ok(())
}

/// Execute exactly the prepared plan: lock, journal, publish. No
/// replanning, no new inputs, no second Execute.
fn execute_prepared(
    channel: &mut SessionChannel<'_>,
    prepared: &PreparedPlan,
) -> Result<String, WorkerError> {
    // The machine lock is already held from preparation: the same guard that
    // refused a second worker then serializes execution now.
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
        .map_err(|error| WorkerError::Transaction(error.to_string()))?;

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
    // Typed service operations ride the same journal: the executor serves
    // the real system bus here, exactly as planning preflighted it. Plans
    // without service nodes run without touching the bus at all.
    if prepared.plan.nodes.iter().any(|node| {
        node.meta.backend.as_ref().is_some_and(|backend| {
            backend
                .id
                .as_str()
                .starts_with(crate::service_ops::SERVICE_BACKEND_PREFIX)
        })
    }) {
        let manager = crate::systemd::RealSystemd::connect()
            .map_err(|error| WorkerError::Transaction(error.to_string()))?;
        executor = executor.with_services(crate::executor::ServiceSupport::isolated(
            prepared.roots.clone(),
            prepared.systemd.clone(),
            manager,
            rustix::process::geteuid().as_raw(),
        ));
    }
    executor
        .register_plan(&prepared.plan)
        .map_err(|error| WorkerError::Transaction(error.to_string()))?;

    // The client may be gone; the transaction is durable either way. Finish
    // to a stable boundary - commit, rollback, or recovery-required - and
    // persist it. Never terminate mid-mutation and never start anything the
    // prepared plan does not name.
    let (record, outcome) = coordinator
        .execute(record, &mut executor)
        .map_err(|error| WorkerError::Transaction(error.to_string()))?;
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
                .map_err(|error| WorkerError::Transaction(error.to_string()))?;
            // An uninstall publishes no ledger - the record is removed, not
            // written - so there are no published modes to normalize. The
            // hierarchy normalization below still runs.
            if prepared.action != crate::run::LinuxAction::Uninstall {
                crate::machine::normalize_published_modes(&prepared.state_root, &ledger)
                    .map_err(|error| WorkerError::Transaction(error.to_string()))?;
            }
            crate::machine::normalize_state_modes(
                &prepared.state_root,
                rustix::process::geteuid().as_raw(),
            )
            .map_err(|error| WorkerError::Transaction(error.to_string()))?;
            if prepared.action == crate::run::LinuxAction::Upgrade {
                crate::run::retire_old_generations_for(
                    &prepared.state_root,
                    &prepared.target_plan.app.id,
                    SelectedScope::Machine,
                    &ledger.version,
                )
                .map_err(|error| WorkerError::Transaction(error.to_string()))?;
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
            Err(WorkerError::AuthFailed(_))
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
