use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use zup_core::SelectedScope;
use zup_protocol::{
    Capabilities, ExecuteOperation, Message, PROTOCOL_VERSION, PrepareOperation, SequenceTracker,
    SessionId, WireEnvelope,
};

use crate::error::{ExecError, PathError};
use crate::machine::MachineRoots;
use crate::pkexec::{PkexecLauncher, SystemPkexec, WorkerChild, map_launch};
use crate::run::{LinuxAction, LinuxOutcome, LinuxRunRequest};
use crate::socket::{
    HANDSHAKE_TIMEOUT, Rendezvous, peer_identity, pin_peer, recv_envelope, send_envelope,
};

const ACCEPT_TIMEOUT: Duration = Duration::from_secs(600);

const EXECUTE_TIMEOUT: Duration = Duration::from_secs(3600);

pub fn run_machine(request: &LinuxRunRequest) -> Result<LinuxOutcome, ExecError> {
    if request.scope != SelectedScope::Machine {
        return Err(PathError::Refused {
            path: "<scope>".into(),
            reason: "the machine runner serves machine scope only".into(),
        }
        .into());
    }
    if rustix::process::geteuid().as_raw() == 0 {
        return run_machine_loopback(request, &MachineRoots::production());
    }
    let launcher = SystemPkexec::resolve().map_err(ExecError::Elevation)?;
    run_machine_elevated(request, &launcher)
}

#[derive(Debug, Clone)]
pub(crate) struct ExpectedPlan {
    pub digest: String,
    pub target: zup_core::TargetTriple,

    #[cfg(feature = "test-support")]
    pub plan: zup_transaction::TransactionPlan,
}

pub(crate) fn plan_expected(
    request: &LinuxRunRequest,
    state_root: &PathBuf,
    roots: &MachineRoots,
    systemd: &crate::machine::SystemdRoots,
    expected_uid: u32,
) -> Result<(PrepareOperation, ExpectedPlan), ExecError> {
    let carrier = crate::carrier::Carrier::open(&request.installer)?;
    let mut targets = carrier.package().build_plan()?.targets;
    if targets.len() != 1 {
        return Err(ExecError::MultipleTargets {
            count: targets.len(),
        });
    }
    let build = targets.remove(0);
    if !build.installer.install.scope.allows_machine() {
        return Err(PathError::Refused {
            path: build.installer.app.id.to_string(),
            reason: "the package does not declare machine scope".into(),
        }
        .into());
    }
    if !build.installer.plugins.is_empty() {
        return Err(PathError::Refused {
            path: build.installer.app.id.to_string(),
            reason: "machine scope refuses projects that need plugin execution".into(),
        }
        .into());
    }
    if !build.installer.prerequisites.is_empty() {
        return Err(PathError::Refused {
            path: build.installer.app.id.to_string(),
            reason: "machine scope runs no prerequisite installers".into(),
        }
        .into());
    }
    let target = build.installer.target.clone();
    if target.operating_system() != zup_core::TargetOperatingSystem::Linux {
        return Err(PathError::Refused {
            path: target.to_string(),
            reason: "not a Linux target".into(),
        }
        .into());
    }

    let mut plan_request = zup_plan::PlanRequest::new(target.clone(), SelectedScope::Machine);
    let mut override_text: Option<String> = None;
    if let Some(directory) = &request.install_dir_override {
        crate::machine::authorize_machine_install_directory(directory, roots).map_err(|error| {
            PathError::Refused {
                path: directory.display().to_string(),
                reason: error.to_string(),
            }
        })?;
        plan_request.install_directory = Some(
            crate::machine::host_to_install_template(directory, roots, &target).map_err(
                |error| PathError::Refused {
                    path: directory.display().to_string(),
                    reason: error.to_string(),
                },
            )?,
        );
        override_text = Some(directory.display().to_string());
    }

    let install = zup_plan::plan(
        &zup_plan::BuildPlan {
            targets: vec![build],
        },
        &plan_request,
    )?;
    let mut target_plan = crate::resolve::resolve_target(
        &install,
        &crate::locations::LinuxInstallLocationResolver::with_machine_roots(roots.clone()),
    )?;
    crate::run::attach_maintenance_copy_for(
        &mut target_plan,
        &request.installer,
        state_root,
        SelectedScope::Machine,
    )?;

    crate::machine::verify_machine_hierarchy(state_root, expected_uid).map_err(|error| {
        PathError::Refused {
            path: state_root.display().to_string(),
            reason: error.to_string(),
        }
    })?;
    crate::machine::verify_ledger_trust(state_root, &target_plan.app.id, expected_uid).map_err(
        |error| PathError::Refused {
            path: state_root.display().to_string(),
            reason: error.to_string(),
        },
    )?;
    let ledger_store = crate::ledger::LinuxLedgerStore::new(state_root);
    let ledger = ledger_store.load(&target_plan.app.id, SelectedScope::Machine)?;
    let action =
        crate::run::resolve_action(request.action, ledger.as_ref(), &target_plan.app.version)?;
    let mut snapshot = crate::executor::snapshot_target(&target_plan);

    let needs_manager = crate::input::requires_service_manager(&target_plan, ledger.as_ref());
    let mut manager = if needs_manager {
        Some(
            crate::systemd::RealSystemd::connect()
                .map_err(|error| ExecError::Executor(format!("systemd is unavailable: {error}")))?,
        )
    } else {
        None
    };
    if let Some(manager) = manager.as_mut() {
        snapshot.services = crate::input::snapshot_services(&target_plan, manager, systemd)
            .map_err(|error| ExecError::Executor(format!("service snapshot: {error}")))?;
    }
    let owned_matches = crate::run::inspect_owned_matches_for(ledger.as_ref());
    let execution = zup_exec::plan_lifecycle(
        action,
        (action != zup_exec::LifecycleAction::Uninstall).then_some(&target_plan),
        Some(&snapshot),
        ledger.as_ref(),
        &owned_matches,
    )?;
    let force_services = matches!(request.action, LinuxAction::Repair { force_files: true });
    let input = if !needs_manager {
        crate::input::compile_execution_plan(&execution, &target_plan)?
    } else {
        let manager = manager.as_mut().ok_or_else(|| {
            ExecError::Executor("a service transaction without a systemd manager".into())
        })?;
        crate::input::compile_machine_execution_plan(
            &execution,
            &target_plan,
            crate::input::ServiceCompilation {
                roots,
                systemd,
                manager,
                force_services,
            },
        )?
    };
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

fn planning_state_root(request: &LinuxRunRequest) -> PathBuf {
    request
        .state_root
        .clone()
        .unwrap_or_else(|| MachineRoots::production().state)
}

pub(crate) fn run_machine_elevated(
    request: &LinuxRunRequest,
    launcher: &impl PkexecLauncher,
) -> Result<LinuxOutcome, ExecError> {
    match run_machine_elevated_once(request, launcher) {
        Err(ExecError::StalePlan) => run_machine_elevated_once(request, launcher),
        outcome => outcome,
    }
}

pub(crate) fn run_machine_elevated_once(
    request: &LinuxRunRequest,
    launcher: &impl PkexecLauncher,
) -> Result<LinuxOutcome, ExecError> {
    let roots = MachineRoots::production();
    let state_root = planning_state_root(request);
    let systemd = crate::machine::SystemdRoots::production();
    let (intent, expected) = plan_expected(request, &state_root, &roots, &systemd, 0)?;
    let session = SessionId::new_v7();
    let invoking = rustix::process::getuid().as_raw();
    let rendezvous = Rendezvous::create(invoking, session).map_err(into_run_error)?;

    let worker_exe = std::env::current_exe().map_err(|source| PathError::Io {
        path: "<executable>".into(),
        source,
    })?;

    let args = vec![
        "__privileged-worker".to_owned(),
        "--session".to_owned(),
        session.0.to_string(),
        "--client-pid".to_owned(),
        std::process::id().to_string(),
        "--socket-path".to_owned(),
        rendezvous.socket().display().to_string(),
    ];
    let mut spawned = launcher
        .spawn(&worker_exe, &args)
        .map_err(ExecError::Elevation)?;

    let mut stream = {
        let start = std::time::Instant::now();
        loop {
            match rendezvous.accept(Duration::from_secs(1)) {
                Ok(stream) => {
                    let peer = peer_identity(&stream).map_err(into_run_error)?;
                    if peer.uid == 0 && peer.pid == spawned.pid() {
                        break stream;
                    }
                    drop(stream);
                }
                Err(crate::error::IpcError::Timeout) => {}
                Err(error) => return Err(into_run_error(error)),
            }
            match spawned.try_wait().map_err(ExecError::Elevation)? {
                Some(launch) => {
                    map_launch(&launch).map_err(ExecError::Elevation)?;
                    return Err(ExecError::Worker(
                        "the worker exited before connecting".into(),
                    ));
                }
                None => {
                    if start.elapsed() >= ACCEPT_TIMEOUT {
                        return Err(ExecError::Worker("the worker did not connect".into()));
                    }
                }
            }
        }
    };

    let peer = peer_identity(&stream).map_err(into_run_error)?;
    let _pin = pin_peer(peer.pid, peer.uid).map_err(into_run_error)?;

    let outcome = drive_client(
        &mut stream,
        session,
        &intent,
        &expected.digest,
        &expected.target,
    );

    let launch_result = spawned
        .wait()
        .map_err(ExecError::Elevation)
        .and_then(|launch| map_launch(&launch).map_err(ExecError::Elevation));
    match (outcome, launch_result) {
        (Ok(outcome), Ok(())) => Ok(outcome),
        (Ok(_), Err(error)) => Err(error),
        (Err(error), _) => Err(error),
    }
}

pub(crate) fn drive_client(
    stream: &mut UnixStream,
    session: SessionId,
    intent: &PrepareOperation,
    expected_digest: &str,
    expected_target: &zup_core::TargetTriple,
) -> Result<LinuxOutcome, ExecError> {
    let mut incoming = SequenceTracker::new();
    let mut outgoing: u64 = 0;

    let hello = recv_msg(stream, session, &mut incoming, HANDSHAKE_TIMEOUT)?;
    let worker_pid = match hello.message {
        Message::WorkerHello(hello) => {
            if hello.session_id != session || hello.target != *expected_target {
                return Err(ExecError::Worker("worker hello mismatch".into()));
            }
            check_capabilities(&hello.capabilities)?;
            hello.worker_pid
        }
        _ => return Err(ExecError::Worker("expected worker hello".into())),
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
                return Err(ExecError::StalePlan);
            }
            Message::Failed(failed) => {
                return Err(ExecError::Worker(format!(
                    "{}: {}",
                    failed.kind, failed.message
                )));
            }
            _ => return Err(ExecError::Worker("expected prepared".into())),
        }
    };

    if prepared.plan_digest.to_lowercase() != expected_digest.to_lowercase() {
        return Err(ExecError::Worker(
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
        .map_err(|_| ExecError::Worker("the worker prepared an unparsable version".into()))?;
    loop {
        match recv_msg(stream, session, &mut incoming, EXECUTE_TIMEOUT)?.message {
            Message::Completed(completed) => {
                return match completed.outcome.as_str() {
                    "committed" => Ok(LinuxOutcome::Committed { version }),
                    "rolled_back" => Ok(LinuxOutcome::RolledBack),
                    "recovery_required" => Ok(LinuxOutcome::RecoveryRequired {
                        transaction: completed.transaction_id.to_string(),
                    }),
                    _ => Err(ExecError::Worker(format!(
                        "unknown outcome {}",
                        completed.outcome
                    ))),
                };
            }
            Message::Failed(failed) => {
                return Err(ExecError::Worker(format!(
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

fn send_msg(
    stream: &mut UnixStream,
    session: SessionId,
    outgoing: &mut u64,
    message: Message,
) -> Result<(), ExecError> {
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

fn recv_msg(
    stream: &mut UnixStream,
    session: SessionId,
    incoming: &mut SequenceTracker,
    timeout: Duration,
) -> Result<WireEnvelope, ExecError> {
    let envelope = recv_envelope(stream, timeout).map_err(into_run_error)?;
    incoming
        .accept(envelope.sequence)
        .map_err(|error| ExecError::Worker(format!("sequence: {error}")))?;
    if envelope.session_id != session {
        return Err(ExecError::Worker("session mismatch".into()));
    }
    if envelope.version != PROTOCOL_VERSION {
        return Err(ExecError::Worker("protocol version mismatch".into()));
    }
    Ok(envelope)
}

fn check_capabilities(capabilities: &Capabilities) -> Result<(), ExecError> {
    if !capabilities.file_transactions_v1 || !capabilities.lifecycle_v1 {
        return Err(ExecError::Worker(
            "the worker lacks the file-transaction capability".into(),
        ));
    }
    Ok(())
}

fn run_machine_loopback(
    request: &LinuxRunRequest,
    roots: &MachineRoots,
) -> Result<LinuxOutcome, ExecError> {
    let state_root = request
        .state_root
        .clone()
        .unwrap_or_else(|| roots.state.clone());
    run_machine_loopback_on(request, &state_root, roots)
}

#[cfg(feature = "test-support")]
pub(crate) fn run_machine_loopback_for_test(
    request: &LinuxRunRequest,
    roots: &MachineRoots,
    systemd: &crate::machine::SystemdRoots,
) -> Result<LinuxOutcome, ExecError> {
    run_machine_loopback_with(request, roots, systemd)
}

fn run_machine_loopback_on(
    request: &LinuxRunRequest,
    state_root: &PathBuf,
    roots: &MachineRoots,
) -> Result<LinuxOutcome, ExecError> {
    run_machine_loopback_with_on(
        request,
        state_root,
        roots,
        &crate::machine::SystemdRoots::production(),
    )
}

#[cfg(feature = "test-support")]
fn run_machine_loopback_with(
    request: &LinuxRunRequest,
    roots: &MachineRoots,
    systemd: &crate::machine::SystemdRoots,
) -> Result<LinuxOutcome, ExecError> {
    let state_root = request
        .state_root
        .clone()
        .unwrap_or_else(|| roots.state.clone());
    run_machine_loopback_with_on(request, &state_root, roots, systemd)
}

fn run_machine_loopback_with_on(
    request: &LinuxRunRequest,
    state_root: &PathBuf,
    roots: &MachineRoots,
    systemd: &crate::machine::SystemdRoots,
) -> Result<LinuxOutcome, ExecError> {
    match run_machine_loopback_with_once(request, state_root, roots, systemd) {
        Err(ExecError::StalePlan) => {
            run_machine_loopback_with_once(request, state_root, roots, systemd)
        }
        outcome => outcome,
    }
}

fn run_machine_loopback_with_once(
    request: &LinuxRunRequest,
    state_root: &PathBuf,
    roots: &MachineRoots,
    systemd: &crate::machine::SystemdRoots,
) -> Result<LinuxOutcome, ExecError> {
    let (intent, expected) = plan_expected(
        request,
        state_root,
        roots,
        systemd,
        rustix::process::geteuid().as_raw(),
    )?;
    let session = SessionId::new_v7();
    let (mut client, mut worker) = UnixStream::pair().map_err(|source| PathError::Io {
        path: "<loopback>".into(),
        source,
    })?;
    let context = crate::worker::WorkerContext {
        roots: roots.clone(),
        systemd: systemd.clone(),
        invoking_uid: rustix::process::geteuid().as_raw(),

        expected_client_pid: std::process::id(),
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
        Ok(Err(error)) => Err(ExecError::Worker(error.to_string())),
        Err(_) => Err(ExecError::Worker("the worker thread failed".into())),
    }
}

fn drive_client_loopback(
    stream: &mut UnixStream,
    session: SessionId,
    intent: &PrepareOperation,
    expected_digest: &str,
    expected_target: &zup_core::TargetTriple,
) -> Result<LinuxOutcome, ExecError> {
    let peer = peer_identity(stream).map_err(into_run_error)?;
    if peer.uid != rustix::process::geteuid().as_raw() {
        return Err(ExecError::Worker("loopback peer mismatch".into()));
    }
    drive_client(stream, session, intent, expected_digest, expected_target)
}

fn into_run_error(error: crate::error::IpcError) -> ExecError {
    ExecError::Worker(error.to_string())
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
