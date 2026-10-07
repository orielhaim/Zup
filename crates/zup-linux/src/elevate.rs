//! The unprivileged side of a machine operation.
//!
//! The model is propose, verify, then authorize - never validate-then-trust:
//!
//! ```text
//! plan locally from the same inputs (expected digest)
//! create a private per-session rendezvous
//! launch the privileged worker through pkexec (or loop back when root)
//! verify the worker (uid 0, the launched pid)
//! send intent + expected digest (Prepare)
//! compare the worker's reconstructed digest with the expected one
//! authorize exactly that digest (Execute)
//! ```
//!
//! No mutation follows from anything before Execute, and Execute names the
//! exact digest both sides computed. A substitution anywhere in between
//! refuses the session instead of installing something nobody confirmed.

use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use zup_core::SelectedScope;
use zup_protocol::{
    Capabilities, ExecuteOperation, Message, PROTOCOL_VERSION, PrepareOperation, SequenceTracker,
    SessionId, WireEnvelope,
};

use crate::machine::MachineRoots;
use crate::pkexec::{PkexecLauncher, SystemPkexec, WorkerChild, map_launch};
use crate::run::{LinuxAction, LinuxOutcome, LinuxRunError, LinuxRunRequest};
use crate::socket::{
    HANDSHAKE_TIMEOUT, Rendezvous, peer_identity, pin_peer, recv_envelope, send_envelope,
};

/// How long to wait for the worker to connect after launch.
///
/// Deliberately generous: authorization timing belongs to `pkexec`/polkit,
/// and racing the administrator entering a password would turn every slow
/// authentication into a protocol failure.
const ACCEPT_TIMEOUT: Duration = Duration::from_secs(600);
/// How long to wait for the final Execute's answer once sent. The install
/// itself may take arbitrarily long; the *answer* framing must not hang
/// forever, so the framing timeout applies per frame while progress flows.
const EXECUTE_TIMEOUT: Duration = Duration::from_secs(3600);

/// Run one machine-scope lifecycle.
///
/// When this process is already uid 0, the same worker path serves the
/// session over a loopback socket pair with no `pkexec` hop - and with the
/// exact same plan, path, and policy validation. "Already root" never takes
/// a weaker shortcut.
pub fn run_machine(request: &LinuxRunRequest) -> Result<LinuxOutcome, LinuxRunError> {
    if request.scope != SelectedScope::Machine {
        return Err(LinuxRunError::RefusedPath {
            path: "<scope>".into(),
            reason: "the machine runner serves machine scope only".into(),
        });
    }
    if rustix::process::geteuid().as_raw() == 0 {
        return run_machine_loopback(request, &MachineRoots::production());
    }
    let launcher = SystemPkexec::resolve().map_err(LinuxRunError::Elevation)?;
    run_machine_elevated(request, &launcher)
}

/// The expected plan: the digest the unprivileged process computed itself,
/// plus the target it must match.
#[derive(Debug, Clone)]
pub(crate) struct ExpectedPlan {
    pub digest: String,
    pub target: zup_core::TargetTriple,
    /// The compiled plan itself, for tests that journal an interrupted
    /// transaction. Present only under `test` or `test-support`: production
    /// planning binds the digest, never the plan object.
    #[cfg(feature = "test-support")]
    pub plan: zup_transaction::TransactionPlan,
}

/// Plan the machine transaction locally, read-only, to bind it.
///
/// Nothing here mutates: no journal begins, no lock is taken, no file is
/// staged. The worker repeats the same planning from the same inputs after
/// authorization, and the two digests must be equal before Execute.
///
/// `roots` are the machine roots both sides enforce: production on the real
/// path, isolated roots in tests. They are a parameter, never environment,
/// so an unprivileged caller cannot redirect them.
///
/// `expected_uid` anchors trust: the state hierarchy and the ledger must be
/// owned by it and private to it before anything is planned from them. `0`
/// on the real path; the test account's own uid in isolated runs. A ledger
/// the invoking user could have written is refused rather than bound.
pub(crate) fn plan_expected(
    request: &LinuxRunRequest,
    state_root: &PathBuf,
    roots: &MachineRoots,
    expected_uid: u32,
) -> Result<(PrepareOperation, ExpectedPlan), LinuxRunError> {
    let carrier = crate::carrier::Carrier::open(&request.installer)?;
    let mut targets = carrier.package().build_plan()?.targets;
    if targets.len() != 1 {
        return Err(LinuxRunError::MultipleTargets {
            count: targets.len(),
        });
    }
    let build = targets.remove(0);
    if !build.installer.install.scope.allows_machine() {
        return Err(LinuxRunError::RefusedPath {
            path: build.installer.app.id.to_string(),
            reason: "the package does not declare machine scope".into(),
        });
    }
    if !build.installer.plugins.is_empty() {
        return Err(LinuxRunError::RefusedPath {
            path: build.installer.app.id.to_string(),
            reason: "machine scope refuses projects that need plugin execution".into(),
        });
    }
    if !build.installer.prerequisites.is_empty() {
        return Err(LinuxRunError::RefusedPath {
            path: build.installer.app.id.to_string(),
            reason: "machine scope runs no prerequisite installers".into(),
        });
    }
    let target = build.installer.target.clone();
    if target.operating_system() != zup_core::TargetOperatingSystem::Linux {
        return Err(LinuxRunError::RefusedPath {
            path: target.to_string(),
            reason: "not a Linux target".into(),
        });
    }

    let mut plan_request = zup_plan::PlanRequest::new(target.clone(), SelectedScope::Machine);
    let mut override_text: Option<String> = None;
    if let Some(directory) = &request.install_dir_override {
        crate::machine::authorize_machine_install_directory(directory, roots).map_err(|error| {
            LinuxRunError::RefusedPath {
                path: directory.display().to_string(),
                reason: error.to_string(),
            }
        })?;
        plan_request.install_directory = Some(
            crate::machine::host_to_install_template(directory, roots, &target).map_err(
                |error| LinuxRunError::RefusedPath {
                    path: directory.display().to_string(),
                    reason: error.to_string(),
                },
            )?,
        );
        override_text = Some(directory.display().to_string());
    }
    // The installer's own directory choices, if the project permits them,
    // are validated against the machine program tree before planning.
    // (The installer CLI resolves `--install-dir` into the request; the
    // worker revalidates the same host path independently.)
    let install = zup_plan::plan(
        &zup_plan::BuildPlan {
            targets: vec![build],
        },
        &plan_request,
    )?;
    let mut target_plan = crate::resolve::resolve_target_with(
        &install,
        &crate::locations::LinuxInstallLocationResolver::with_machine_roots(roots.clone()),
    )?;
    crate::run::attach_maintenance_copy_for(
        &mut target_plan,
        &request.installer,
        state_root,
        SelectedScope::Machine,
    )?;
    crate::capabilities::validate_target_plan(&target_plan)?;
    // Trust before reading: a hierarchy or ledger the invoking user could
    // have written plans a digest nobody should authorize.
    crate::machine::verify_machine_hierarchy(state_root, expected_uid).map_err(|error| {
        LinuxRunError::RefusedPath {
            path: state_root.display().to_string(),
            reason: error.to_string(),
        }
    })?;
    crate::machine::verify_ledger_trust(state_root, &target_plan.app.id, expected_uid).map_err(
        |error| LinuxRunError::RefusedPath {
            path: state_root.display().to_string(),
            reason: error.to_string(),
        },
    )?;
    let ledger_store = crate::ledger::LinuxLedgerStore::new(state_root);
    let ledger = ledger_store.load(&target_plan.app.id, SelectedScope::Machine)?;
    let action =
        crate::run::resolve_action(request.action, ledger.as_ref(), &target_plan.app.version)?;
    let snapshot = crate::snapshot::snapshot_target(&target_plan);
    let owned_matches = crate::run::inspect_owned_matches_for(ledger.as_ref());
    let execution = zup_exec::plan_lifecycle(
        action,
        (action != zup_exec::LifecycleAction::Uninstall).then_some(&target_plan),
        Some(&snapshot),
        ledger.as_ref(),
        &owned_matches,
    )?;
    let input = crate::input::compile_execution_plan(&execution, &target_plan)?;
    let plan = zup_transaction::compile_transaction(&input)?;
    ledger_store.validate_plan(
        &target_plan.app.id,
        SelectedScope::Machine,
        &target_plan.app.version,
        &plan,
    )?;
    let digest = plan.fingerprint().to_hex();
    let operation = match request.action {
        LinuxAction::Install => zup_protocol::privileged_operation::INSTALL,
        LinuxAction::Upgrade => zup_protocol::privileged_operation::UPGRADE,
        LinuxAction::Repair { .. } => zup_protocol::privileged_operation::REPAIR,
        LinuxAction::Uninstall => zup_protocol::privileged_operation::UNINSTALL,
        LinuxAction::Apply => zup_protocol::privileged_operation::APPLY,
    };
    let intent = PrepareOperation {
        operation: operation.to_owned(),
        force_files: matches!(request.action, LinuxAction::Repair { force_files } if force_files),
        install_dir_override: override_text,
        selected_components: Vec::new(),
        expected_plan_digest: Some(digest.clone()),
        app_id: target_plan.app.id.to_string(),
        app_version: target_plan.app.version.to_string(),
        scope: "machine".to_owned(),
        target: target.clone(),
    };
    Ok((
        intent,
        ExpectedPlan {
            digest,
            target: target.clone(),
            #[cfg(feature = "test-support")]
            plan,
        },
    ))
}

/// Effective machine state root for planning: the explicit test root, or
/// the production root without creating it.
fn planning_state_root(request: &LinuxRunRequest) -> PathBuf {
    request
        .state_root
        .clone()
        .unwrap_or_else(|| MachineRoots::production().state)
}

/// Drive one elevated machine operation through `pkexec` and the worker.
///
/// A plan that went stale while the worker repaired state retries once
/// against the repaired world, with a fresh plan and a fresh session.
/// Anything else fails as it fails: retries never reuse an authorization.
fn run_machine_elevated(
    request: &LinuxRunRequest,
    launcher: &impl PkexecLauncher,
) -> Result<LinuxOutcome, LinuxRunError> {
    match run_machine_elevated_once(request, launcher) {
        Err(LinuxRunError::StalePlan) => run_machine_elevated_once(request, launcher),
        outcome => outcome,
    }
}

/// One elevated attempt: plan, launch, handshake, execute.
fn run_machine_elevated_once(
    request: &LinuxRunRequest,
    launcher: &impl PkexecLauncher,
) -> Result<LinuxOutcome, LinuxRunError> {
    let roots = MachineRoots::production();
    let state_root = planning_state_root(request);
    let (intent, expected) = plan_expected(request, &state_root, &roots, 0)?;
    let session = SessionId::new_v7();
    let invoking = rustix::process::getuid().as_raw();
    let rendezvous = Rendezvous::create(invoking, session).map_err(into_run_error)?;

    let worker_exe = std::env::current_exe().map_err(|source| LinuxRunError::Io {
        path: "<executable>".into(),
        source,
    })?;
    let args = vec![
        "__privileged-worker".to_owned(),
        "--session".to_owned(),
        session.0.to_string(),
    ];
    let spawned = launcher
        .spawn(&worker_exe, &args)
        .map_err(LinuxRunError::Elevation)?;
    let mut stream = rendezvous.accept(ACCEPT_TIMEOUT).map_err(into_run_error)?;
    // The worker is the process just launched, now root: peer uid 0 and
    // the exact launched pid, pinned against reuse.
    let peer = peer_identity(&stream).map_err(into_run_error)?;
    if peer.uid != 0 {
        return Err(LinuxRunError::Worker("the worker is not privileged".into()));
    }
    if peer.pid != spawned.pid() {
        return Err(LinuxRunError::Worker(
            "the connected worker is not the launched process".into(),
        ));
    }
    let _pin = pin_peer(peer.pid, peer.uid).map_err(into_run_error)?;

    let outcome = drive_client(
        &mut stream,
        session,
        &intent,
        &expected.digest,
        &expected.target,
    );
    // The protocol decides, and the launcher result corroborates: a clean
    // protocol ending with a failed launch is still a failure, and a clean
    // launch with a broken protocol is not a success.
    let launch_result = spawned
        .wait()
        .map_err(LinuxRunError::Elevation)
        .and_then(|launch| map_launch(&launch).map_err(LinuxRunError::Elevation));
    match (outcome, launch_result) {
        (Ok(outcome), Ok(())) => Ok(outcome),
        (Ok(_), Err(error)) => Err(error),
        (Err(error), _) => Err(error),
    }
}

/// The client half of the handshake over a connected stream.
///
/// `pub(crate)` so the `test-support` surface can drive the real client
/// against an isolated worker without `pkexec`: the handshake is identical,
/// only the transport is a test socket pair.
pub(crate) fn drive_client(
    stream: &mut UnixStream,
    session: SessionId,
    intent: &PrepareOperation,
    expected_digest: &str,
    expected_target: &zup_core::TargetTriple,
) -> Result<LinuxOutcome, LinuxRunError> {
    let mut incoming = SequenceTracker::new();
    let mut outgoing: u64 = 0;

    // The worker speaks first: it must be the privileged worker for this
    // session before anything is proposed.
    let hello = recv_msg(stream, session, &mut incoming, HANDSHAKE_TIMEOUT)?;
    let worker_pid = match hello.message {
        Message::WorkerHello(hello) => {
            if hello.session_id != session || hello.target != *expected_target {
                return Err(LinuxRunError::Worker("worker hello mismatch".into()));
            }
            check_capabilities(&hello.capabilities)?;
            hello.worker_pid
        }
        _ => return Err(LinuxRunError::Worker("expected worker hello".into())),
    };
    let _ = worker_pid;

    send_msg(
        stream,
        session,
        &mut outgoing,
        Message::Prepare(intent.clone()),
    )?;
    let prepared = loop {
        match recv_msg(stream, session, &mut incoming, HANDSHAKE_TIMEOUT)?.message {
            Message::Prepared(prepared) => break prepared,
            Message::Progress(report) => {
                println!("{} ", report.detail);
                continue;
            }
            Message::Failed(failed) if failed.kind == zup_protocol::failure::STALE_PLAN => {
                return Err(LinuxRunError::StalePlan);
            }
            Message::Failed(failed) => {
                return Err(LinuxRunError::Worker(format!(
                    "{}: {}",
                    failed.kind, failed.message
                )));
            }
            _ => return Err(LinuxRunError::Worker("expected prepared".into())),
        }
    };
    // The binding: the worker's reconstructed digest must equal what this
    // process planned and showed. Authorizing any other digest would let
    // the UI confirm one plan while root executes another.
    if prepared.plan_digest.to_lowercase() != expected_digest.to_lowercase() {
        return Err(LinuxRunError::Worker(
            "the worker's plan differs from the confirmed one".into(),
        ));
    }
    println!(
        "machine {} {} {} ({} files, plan {})",
        prepared.operation,
        prepared.app_id,
        prepared.app_version,
        prepared.file_count,
        &prepared.plan_digest[..16]
    );

    send_msg(
        stream,
        session,
        &mut outgoing,
        Message::Execute(ExecuteOperation {
            plan_digest: prepared.plan_digest.clone(),
        }),
    )?;
    let version: semver::Version = prepared
        .app_version
        .parse()
        .map_err(|_| LinuxRunError::Worker("the worker prepared an unparsable version".into()))?;
    loop {
        match recv_msg(stream, session, &mut incoming, EXECUTE_TIMEOUT)?.message {
            Message::Completed(completed) => {
                return match completed.outcome.as_str() {
                    "committed" => Ok(LinuxOutcome::Committed { version }),
                    "rolled_back" => Ok(LinuxOutcome::RolledBack),
                    "recovery_required" => Ok(LinuxOutcome::RecoveryRequired {
                        transaction: completed.transaction_id.to_string(),
                    }),
                    _ => Err(LinuxRunError::Worker(format!(
                        "unknown outcome {}",
                        completed.outcome
                    ))),
                };
            }
            Message::Failed(failed) => {
                return Err(LinuxRunError::Worker(format!(
                    "{}: {}",
                    failed.kind, failed.message
                )));
            }
            Message::Progress(report) => {
                println!("{} ", report.detail);
                continue;
            }
            _ => {}
        }
    }
}

/// Send one client envelope, advancing the outgoing sequence.
fn send_msg(
    stream: &mut UnixStream,
    session: SessionId,
    outgoing: &mut u64,
    message: Message,
) -> Result<(), LinuxRunError> {
    let envelope = WireEnvelope {
        version: PROTOCOL_VERSION,
        session_id: session,
        sequence: *outgoing,
        message,
    };
    send_envelope(stream, &envelope).map_err(into_run_error)?;
    *outgoing = outgoing.saturating_add(1);
    Ok(())
}

/// Receive one client envelope: session, version, and sequence bound.
fn recv_msg(
    stream: &mut UnixStream,
    session: SessionId,
    incoming: &mut SequenceTracker,
    timeout: Duration,
) -> Result<WireEnvelope, LinuxRunError> {
    let envelope = recv_envelope(stream, timeout).map_err(into_run_error)?;
    incoming
        .accept(envelope.sequence)
        .map_err(|error| LinuxRunError::Worker(format!("sequence: {error}")))?;
    if envelope.session_id != session {
        return Err(LinuxRunError::Worker("session mismatch".into()));
    }
    if envelope.version != PROTOCOL_VERSION {
        return Err(LinuxRunError::Worker("protocol version mismatch".into()));
    }
    Ok(envelope)
}

/// The worker must speak the lifecycle the client plans through.
fn check_capabilities(capabilities: &Capabilities) -> Result<(), LinuxRunError> {
    if !capabilities.file_transactions_v1 || !capabilities.lifecycle_v1 {
        return Err(LinuxRunError::Worker(
            "the worker lacks the file-transaction capability".into(),
        ));
    }
    Ok(())
}

/// Already-root loopback: the same worker path serves the session over an
/// in-process socket pair, with the exact same plan, path, and policy
/// validation - and no `pkexec` hop.
fn run_machine_loopback(
    request: &LinuxRunRequest,
    roots: &MachineRoots,
) -> Result<LinuxOutcome, LinuxRunError> {
    let state_root = request
        .state_root
        .clone()
        .unwrap_or_else(|| roots.state.clone());
    run_machine_loopback_on(request, &state_root, roots)
}

/// Loopback with an explicit state root, for isolated tests.
///
/// `pub(crate)` and reachable only through the `test-support` module:
/// production always resolves the state root the same way the real worker
/// does, and no IPC message selects it.
#[cfg(feature = "test-support")]
pub(crate) fn run_machine_loopback_for_test(
    request: &LinuxRunRequest,
    roots: &MachineRoots,
) -> Result<LinuxOutcome, LinuxRunError> {
    run_machine_loopback(request, roots)
}

/// One loopback session: plan, serve, drive, join.
///
/// Like the elevated path, a stale plan retries once against the repaired
/// world with a fresh session.
fn run_machine_loopback_on(
    request: &LinuxRunRequest,
    state_root: &PathBuf,
    roots: &MachineRoots,
) -> Result<LinuxOutcome, LinuxRunError> {
    match run_machine_loopback_once(request, state_root, roots) {
        Err(LinuxRunError::StalePlan) => run_machine_loopback_once(request, state_root, roots),
        outcome => outcome,
    }
}

/// One loopback attempt: plan, serve, drive, join.
fn run_machine_loopback_once(
    request: &LinuxRunRequest,
    state_root: &PathBuf,
    roots: &MachineRoots,
) -> Result<LinuxOutcome, LinuxRunError> {
    let (intent, expected) = plan_expected(
        request,
        state_root,
        roots,
        rustix::process::geteuid().as_raw(),
    )?;
    let session = SessionId::new_v7();
    let (mut client, mut worker) = UnixStream::pair().map_err(|source| LinuxRunError::Io {
        path: "<loopback>".into(),
        source,
    })?;
    let context = crate::worker::WorkerContext {
        roots: roots.clone(),
        invoking_uid: rustix::process::geteuid().as_raw(),
        session,
        worker_exe: request.installer.clone(),
        carrier_pin: None,
    };
    let worker_thread =
        std::thread::spawn(move || crate::worker::serve_session(&mut worker, context));
    let outcome = drive_client_loopback(
        &mut client,
        session,
        &intent,
        &expected.digest,
        &expected.target,
    );
    match worker_thread.join() {
        Ok(Ok(_)) => outcome,
        Ok(Err(error)) => Err(LinuxRunError::Worker(error.to_string())),
        Err(_) => Err(LinuxRunError::Worker("the worker thread failed".into())),
    }
}

/// The client half without pkexec: peer checks against the loopback worker.
///
/// The loopback worker is this process, already root: uid 0 and a live
/// pinned peer are still verified, because "already root" skips the hop,
/// never the checks.
fn drive_client_loopback(
    stream: &mut UnixStream,
    session: SessionId,
    intent: &PrepareOperation,
    expected_digest: &str,
    expected_target: &zup_core::TargetTriple,
) -> Result<LinuxOutcome, LinuxRunError> {
    let peer = peer_identity(stream).map_err(into_run_error)?;
    if peer.uid != rustix::process::geteuid().as_raw() {
        return Err(LinuxRunError::Worker("loopback peer mismatch".into()));
    }
    drive_client(stream, session, intent, expected_digest, expected_target)
}

fn into_run_error(error: crate::socket::SocketError) -> LinuxRunError {
    LinuxRunError::Worker(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_require_the_transaction_set() {
        assert!(
            check_capabilities(&Capabilities {
                file_transactions_v1: true,
                backend_operations_v1: false,
                lifecycle_v1: true,
                prerequisite_bootstrap_v1: false,
            })
            .is_ok()
        );
        assert!(check_capabilities(&Capabilities::default()).is_err());
    }
}
