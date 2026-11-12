use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use zup_core::SelectedScope;
use zup_core::SessionId;
use zup_protocol::{
    Capabilities, ExecuteOperation, Message, PROTOCOL_VERSION, PrepareOperation, SequenceTracker,
    WireEnvelope,
};

use crate::error::{ExecError, PathError};
use crate::lifecycle::LinuxOutcome;
use crate::locations::MachineRoots;
use crate::pkexec::{PkexecLauncher, WorkerChild, map_launch};
use crate::run::LinuxRunRequest;
use crate::socket::{
    HANDSHAKE_TIMEOUT, Rendezvous, peer_identity, pin_peer, recv_envelope, send_envelope,
};
use zup_protocol::ProgressKind;

const ACCEPT_TIMEOUT: Duration = Duration::from_secs(600);

const EXECUTE_TIMEOUT: Duration = Duration::from_secs(3600);

pub(crate) fn plan_expected(
    request: &LinuxRunRequest,
    state_root: &PathBuf,
    roots: &MachineRoots,
    systemd: &crate::machine::SystemdRoots,
    expected_uid: u32,
) -> Result<(PrepareOperation, zup_plan::DeploymentPlan), ExecError> {
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
    let target = build.installer.target;
    if target.operating_system() != zup_core::TargetOperatingSystem::Linux {
        return Err(PathError::Refused {
            path: target.to_string(),
            reason: "not a Linux target".into(),
        }
        .into());
    }

    crate::machine::resolve_machine_state_root(Some(state_root), roots, &build.installer.app.id)?;
    crate::machine::assess_application_state_root(
        roots,
        &build.installer.app.id,
        expected_uid,
        true,
    )
    .map_err(|error| PathError::Refused {
        path: state_root.display().to_string(),
        reason: error.to_string(),
    })?;
    crate::machine::verify_ledger_trust(state_root, &build.installer.app.id, expected_uid)
        .map_err(|error| PathError::Refused {
            path: state_root.display().to_string(),
            reason: error.to_string(),
        })?;
    let ledger_store = crate::ledger::LinuxInstallationStore::new(state_root);
    let ledger = ledger_store.load(&build.installer.app.id, SelectedScope::Machine)?;
    let selection = request.selection.clone();
    let host = crate::operation_host::LinuxOperationHost {
        installer: request.installer.clone(),
        machine: Some(crate::operation_host::MachineHost {
            roots: roots.clone(),
            systemd: systemd.clone(),
        }),
    };
    let input = zup_runtime::OperationInput {
        build: &build,
        installer: &request.installer,
        scope: SelectedScope::Machine,
        operation: request.action,
        selection,
        prior: ledger.as_ref(),
        state_root,
        cancellation: &zup_plan::NeverCancelled,
    };
    let prepared = zup_runtime::prepare_operation(&input, &host)?;
    ledger_store.validate_plan(
        &build.installer.app.id,
        SelectedScope::Machine,
        &prepared.transaction_plan,
    )?;
    let intent = PrepareOperation {
        operation: zup_protocol::PrivilegedRequest::Deployment {
            intent: Box::new(zup_plan::DeploymentIntent {
                deployment: zup_plan::DeploymentSelection::from_plan(&prepared.transaction_plan),
                action: prepared.action,
                enable: request.selection.enable.clone(),
                disable: request.selection.disable.clone(),
                install_directory: request.selection.install_directory.clone(),
            }),
            expected_requirements: Some(Box::new(prepared.transaction_plan.requirements.clone())),
        },
        app_id: build.installer.app.id.to_string(),
        app_version: build.installer.app.version.to_string(),
        scope: "machine".to_owned(),
        target,
        state_root: state_root.display().to_string(),
    };
    Ok((intent, prepared.transaction_plan))
}

pub(crate) fn drive_elevated(
    worker_exe: &std::path::Path,
    intent: &PrepareOperation,
    target: &zup_core::Target,
    launcher: &impl PkexecLauncher,
    events: Option<&tokio::sync::broadcast::Sender<zup_runtime::RuntimeEvent>>,
    cancellation: Option<&zup_runtime::CancellationHandle>,
) -> Result<LinuxOutcome, ExecError> {
    let session = SessionId::new_v7();
    let invoking = rustix::process::getuid().as_raw();
    let rendezvous = Rendezvous::create(invoking, session).map_err(into_run_error)?;

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
        .spawn(worker_exe, &args)
        .map_err(ExecError::Elevation)?;

    let mut stream = {
        let start = std::time::Instant::now();
        loop {
            if cancellation.is_some_and(|cancel| cancel.is_cancelled()) {
                return Err(ExecError::Cancelled);
            }
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
    if let Some(events) = events {
        let _ = events.send(zup_runtime::RuntimeEvent::WorkerConnected);
    }

    let outcome = drive_client(&mut stream, session, intent, target, events, cancellation);

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
    expected_target: &zup_core::Target,
    events: Option<&tokio::sync::broadcast::Sender<zup_runtime::RuntimeEvent>>,
    cancellation: Option<&zup_runtime::CancellationHandle>,
) -> Result<LinuxOutcome, ExecError> {
    let mut incoming = SequenceTracker::new();
    let mut outgoing: u64 = 0;

    let hello = recv_or_fail(stream, session, &mut incoming, HANDSHAKE_TIMEOUT)?;
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
    let prepare_deadline = std::time::Instant::now() + crate::socket::PREPARE_TIMEOUT;
    let prepared = loop {
        if cancellation.is_some_and(|cancel| cancel.is_cancelled()) {
            send_msg(stream, session, &mut outgoing, Message::Cancel)?;
            return Err(ExecError::Cancelled);
        }
        let remaining = prepare_deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Err(ExecError::Worker("worker preparation timed out".into()));
        }
        let envelope = match recv_msg(
            stream,
            session,
            &mut incoming,
            remaining.min(Duration::from_millis(200)),
        ) {
            Ok(envelope) => envelope,
            Err(RecvFail::Timeout) => continue,
            Err(RecvFail::Failed(error)) => return Err(error),
        };
        match envelope.message {
            Message::Prepared(prepared) => break prepared,
            Message::Progress(report) => {
                emit_progress(&report, events);
                continue;
            }
            Message::Failed(failed) if failed.kind == zup_protocol::WorkerFailure::StalePlan => {
                return Err(ExecError::StalePlan);
            }
            Message::Failed(zup_protocol::Failed {
                kind: zup_protocol::WorkerFailure::RecoveryRequired { transaction },
                message,
            }) => {
                return Err(ExecError::RecoveryRequired {
                    transaction,
                    detail: message,
                });
            }
            Message::Failed(failed) => {
                return Err(ExecError::Worker(format!(
                    "{:?}: {}",
                    failed.kind, failed.message
                )));
            }
            _ => return Err(ExecError::Worker("expected prepared".into())),
        }
    };

    if prepared.app_id != intent.app_id
        || prepared.scope != intent.scope
        || prepared.target != intent.target
        || prepared.operation != intent.operation.name()
    {
        return Err(ExecError::Worker(
            "the worker prepared a different operation".into(),
        ));
    }
    prepared
        .plan
        .validate()
        .map_err(|error| ExecError::Worker(error.to_string()))?;
    if prepared.plan.fingerprint().to_hex() != prepared.plan_digest
        || prepared.plan.target != prepared.target
        || prepared.plan.requirements.app.id.to_string() != prepared.app_id
        || prepared.plan.requirements.scope.to_string() != prepared.scope
    {
        return Err(ExecError::Worker(
            "prepared graph differs from its authenticated response".into(),
        ));
    }
    if matches!(&intent.operation, zup_protocol::PrivilegedRequest::Deployment { expected_requirements: Some(expected), .. } if expected.as_ref() != &prepared.plan.requirements)
    {
        return Err(ExecError::Worker(
            "the worker's plan differs from the confirmed one".into(),
        ));
    }
    tracing::debug!(
        "machine {} {} {} ({} files, plan {})",
        prepared.operation,
        prepared.app_id,
        prepared.app_version,
        prepared.file_count,
        &prepared.plan_digest[..16]
    );

    if cancellation.is_some_and(|cancel| cancel.is_cancelled()) {
        send_msg(stream, session, &mut outgoing, Message::Cancel)?;
        return Err(ExecError::Cancelled);
    }
    if let Some(events) = events {
        let _ = events.send(zup_runtime::RuntimeEvent::PlanPrepared {
            plan: prepared.plan.clone(),
        });
    }
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
    let deadline = std::time::Instant::now() + EXECUTE_TIMEOUT;
    let mut cancel_sent = false;
    loop {
        if cancellation.is_some_and(|cancel| cancel.is_cancelled()) && !cancel_sent {
            send_msg(stream, session, &mut outgoing, Message::Cancel)?;
            cancel_sent = true;
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Err(ExecError::Worker("the worker timed out".into()));
        }
        let slice = if cancellation.is_some() {
            remaining.min(Duration::from_millis(200))
        } else {
            remaining
        };
        let message = match recv_msg(stream, session, &mut incoming, slice) {
            Ok(envelope) => envelope.message,
            Err(RecvFail::Timeout) if cancellation.is_some() => continue,
            Err(RecvFail::Timeout) => {
                return Err(ExecError::Worker("the worker timed out".into()));
            }
            Err(RecvFail::Failed(error)) => return Err(error),
        };
        match message {
            Message::Completed(completed) => {
                return match completed.outcome {
                    zup_protocol::WorkerOutcome::Committed => {
                        Ok(LinuxOutcome::Committed { version })
                    }
                    zup_protocol::WorkerOutcome::RolledBack => {
                        if let (Some(events), Some(message)) = (events, completed.detail) {
                            let _ = events.send(zup_runtime::RuntimeEvent::Failed {
                                kind: zup_runtime::FailureKind::Transaction,
                                message,
                            });
                        }
                        Ok(LinuxOutcome::RolledBack)
                    }
                    zup_protocol::WorkerOutcome::RecoveryAssessed => {
                        Ok(LinuxOutcome::Assessed(Box::new(
                            serde_json::from_str(completed.detail.as_deref().ok_or_else(|| {
                                ExecError::Worker("worker assessment missing".into())
                            })?)
                            .map_err(|error| ExecError::Worker(error.to_string()))?,
                        )))
                    }
                    zup_protocol::WorkerOutcome::Cancelled => Err(ExecError::Cancelled),
                    zup_protocol::WorkerOutcome::RecoveryRequired => {
                        let detail = completed.detail.unwrap_or_else(|| "The transaction requires recovery before another operation can proceed".into());
                        if let Some(events) = events {
                            let _ = events.send(zup_runtime::RuntimeEvent::Failed {
                                kind: zup_runtime::FailureKind::RecoveryRequired,
                                message: detail.clone(),
                            });
                        }
                        Ok(LinuxOutcome::RecoveryRequired {
                            transaction: Some(completed.transaction_id),
                            detail,
                        })
                    }
                    zup_protocol::WorkerOutcome::CommittedPendingReconciliation => {
                        Ok(LinuxOutcome::CommittedPendingReconciliation {
                            detail: completed.detail.unwrap_or_else(|| {
                                format!(
                                    "transaction {} committed; reconciliation remains pending",
                                    completed.transaction_id
                                )
                            }),
                            transaction: completed.transaction_id,
                        })
                    }
                    _ => Err(ExecError::Worker(format!(
                        "unexpected outcome {:?}",
                        completed.outcome
                    ))),
                };
            }
            Message::Failed(failed) if failed.kind == zup_protocol::WorkerFailure::Cancelled => {
                return Err(ExecError::Cancelled);
            }
            Message::Failed(failed) => {
                return Err(ExecError::Worker(format!(
                    "{:?}: {}",
                    failed.kind, failed.message
                )));
            }
            Message::Progress(report) => {
                emit_progress(&report, events);
                continue;
            }
            _ => {}
        }
    }
}

fn recv_or_fail(
    stream: &mut UnixStream,
    session: SessionId,
    incoming: &mut SequenceTracker,
    timeout: Duration,
) -> Result<WireEnvelope, ExecError> {
    recv_msg(stream, session, incoming, timeout).map_err(|error| match error {
        RecvFail::Timeout => ExecError::Worker("the worker timed out".into()),
        RecvFail::Failed(error) => error,
    })
}

fn emit_progress(
    report: &zup_protocol::ProgressReport,
    events: Option<&tokio::sync::broadcast::Sender<zup_runtime::RuntimeEvent>>,
) {
    let Some(events) = events else { return };
    let event = match report.kind {
        ProgressKind::OperationProgress => match (report.completed, report.total) {
            (Some(completed), Some(total)) => zup_runtime::RuntimeEvent::Progress {
                phase: report.phase,
                completed,
                total,
                action: report.detail.clone(),
            },
            _ => zup_runtime::RuntimeEvent::StagingProgress {
                id: "operation".into(),
                detail: report.detail.clone(),
            },
        },
        ProgressKind::OperationStarted => zup_runtime::RuntimeEvent::OperationStarted {
            id: report.detail.clone(),
        },
        ProgressKind::PreflightStarted => zup_runtime::RuntimeEvent::PreflightStarted,
        ProgressKind::ResourceBlocked => zup_runtime::RuntimeEvent::ResourceBlocked {
            detail: report.detail.clone(),
            pids: report.blocked_pids.clone(),
        },
        ProgressKind::StagingStarted => zup_runtime::RuntimeEvent::StagingStarted {
            id: report.detail.clone(),
        },
        ProgressKind::StagingProgress | ProgressKind::Verifying => {
            zup_runtime::RuntimeEvent::StagingProgress {
                id: "staging".into(),
                detail: report.detail.clone(),
            }
        }
        ProgressKind::RollingBack => zup_runtime::RuntimeEvent::RollingBack,
        ProgressKind::Committed => zup_runtime::RuntimeEvent::StagingProgress {
            id: "commit".into(),
            detail: report.detail.clone(),
        },
    };
    let _ = events.send(event);
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

enum RecvFail {
    Timeout,
    Failed(ExecError),
}

fn recv_msg(
    stream: &mut UnixStream,
    session: SessionId,
    incoming: &mut SequenceTracker,
    timeout: Duration,
) -> Result<WireEnvelope, RecvFail> {
    let envelope = match recv_envelope(stream, timeout) {
        Ok(envelope) => envelope,
        Err(crate::error::IpcError::Timeout) => return Err(RecvFail::Timeout),
        Err(error) => return Err(RecvFail::Failed(into_run_error(error))),
    };
    incoming
        .accept(envelope.sequence)
        .map_err(|error| RecvFail::Failed(ExecError::Worker(format!("sequence: {error}"))))?;
    if envelope.session_id != session {
        return Err(RecvFail::Failed(ExecError::Worker(
            "session mismatch".into(),
        )));
    }
    if envelope.version != PROTOCOL_VERSION {
        return Err(RecvFail::Failed(ExecError::Worker(
            "protocol version mismatch".into(),
        )));
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

pub(crate) fn run_machine_loopback(
    request: &LinuxRunRequest,
    roots: &MachineRoots,
    control: &zup_runtime::RuntimeControl,
) -> Result<LinuxOutcome, ExecError> {
    let carrier = crate::carrier::Carrier::open(&request.installer)?;
    let state_root = crate::machine::resolve_machine_state_root(
        request.state_root.as_deref(),
        roots,
        &carrier.package().plan().installer.app.id,
    )?;
    run_machine_loopback_with_on(
        request,
        &state_root,
        roots,
        &crate::machine::SystemdRoots::production(),
        crate::worker::WorkerFault::None,
        Some(control),
    )
}

#[cfg(feature = "test-support")]
pub(crate) fn run_machine_loopback_for_test(
    request: &LinuxRunRequest,
    roots: &MachineRoots,
    systemd: &crate::machine::SystemdRoots,
) -> Result<LinuxOutcome, ExecError> {
    run_machine_loopback_with(request, roots, systemd, crate::worker::WorkerFault::None)
}

#[cfg(feature = "test-support")]
pub(crate) fn run_machine_loopback_faulted(
    request: &LinuxRunRequest,
    roots: &MachineRoots,
    systemd: &crate::machine::SystemdRoots,
    fault: crate::worker::WorkerFault,
) -> Result<LinuxOutcome, ExecError> {
    run_machine_loopback_with(request, roots, systemd, fault)
}

#[cfg(feature = "test-support")]
fn run_machine_loopback_with(
    request: &LinuxRunRequest,
    roots: &MachineRoots,
    systemd: &crate::machine::SystemdRoots,
    fault: crate::worker::WorkerFault,
) -> Result<LinuxOutcome, ExecError> {
    let carrier = crate::carrier::Carrier::open(&request.installer)?;
    let state_root = crate::machine::resolve_machine_state_root(
        request.state_root.as_deref(),
        roots,
        &carrier.package().plan().installer.app.id,
    )?;
    run_machine_loopback_with_on(request, &state_root, roots, systemd, fault, None)
}

fn run_machine_loopback_with_on(
    request: &LinuxRunRequest,
    state_root: &PathBuf,
    roots: &MachineRoots,
    systemd: &crate::machine::SystemdRoots,
    fault: crate::worker::WorkerFault,
    control: Option<&zup_runtime::RuntimeControl>,
) -> Result<LinuxOutcome, ExecError> {
    match run_machine_loopback_with_once(request, state_root, roots, systemd, fault, control) {
        Err(ExecError::StalePlan) => {
            run_machine_loopback_with_once(request, state_root, roots, systemd, fault, control)
        }
        outcome => outcome,
    }
}

fn run_machine_loopback_with_once(
    request: &LinuxRunRequest,
    state_root: &PathBuf,
    roots: &MachineRoots,
    systemd: &crate::machine::SystemdRoots,
    fault: crate::worker::WorkerFault,
    control: Option<&zup_runtime::RuntimeControl>,
) -> Result<LinuxOutcome, ExecError> {
    let (intent, _) = plan_expected(
        request,
        state_root,
        roots,
        systemd,
        rustix::process::geteuid().as_raw(),
    )?;
    let context = crate::worker::WorkerContext {
        roots: roots.clone(),
        systemd: systemd.clone(),
        invoking_uid: rustix::process::geteuid().as_raw(),
        expected_client_pid: std::process::id(),
        session: SessionId::new_v7(),
        worker_exe: request.installer.clone(),
        carrier_pin: None,
        fault,
    };
    drive_loopback(context, &intent, control)
}

fn drive_loopback(
    context: crate::worker::WorkerContext,
    intent: &PrepareOperation,
    control: Option<&zup_runtime::RuntimeControl>,
) -> Result<LinuxOutcome, ExecError> {
    let session = context.session;
    let (mut client, mut worker) = UnixStream::pair()
        .map_err(|source| PathError::io(std::path::Path::new("<loopback>"), source))?;
    let peer = peer_identity(&client).map_err(into_run_error)?;
    if peer.uid != context.invoking_uid {
        return Err(ExecError::Worker("loopback peer mismatch".into()));
    }
    let worker = std::thread::spawn(move || crate::worker::serve_session(&mut worker, context));
    let outcome = drive_client(
        &mut client,
        session,
        intent,
        &intent.target,
        control.map(|control| &control.events),
        control.map(|control| &control.cancellation),
    );
    drop(client);
    let finished = worker
        .join()
        .map_err(|_| ExecError::Worker("the worker thread failed".into()))
        .and_then(|result| result.map_err(into_run_error));
    match (outcome, finished) {
        (Ok(outcome), Ok(_)) => Ok(outcome),
        (Err(error), _) | (Ok(_), Err(error)) => Err(error),
    }
}

fn into_run_error(error: crate::error::IpcError) -> ExecError {
    ExecError::Worker(error.to_string())
}

pub(crate) fn recover_machine(
    installer: &std::path::Path,
    state_root: &std::path::Path,
    id: uuid::Uuid,
    request: &zup_transaction::RecoveryRequest,
    control: &zup_runtime::RuntimeControl,
) -> Result<LinuxOutcome, ExecError> {
    let carrier = crate::carrier::Carrier::open(installer)?;
    let app = &carrier.package().plan().installer.app;
    let intent = PrepareOperation {
        operation: zup_protocol::PrivilegedRequest::Recovery {
            transaction: zup_core::TransactionId::from_uuid(id),
            request: request.clone(),
        },
        app_id: app.id.to_string(),
        app_version: app.version.to_string(),
        scope: "machine".into(),
        target: *carrier.target(),
        state_root: state_root.display().to_string(),
    };
    if rustix::process::geteuid().as_raw() != 0 {
        let _ = control
            .events
            .send(zup_runtime::RuntimeEvent::WaitingForAuthorization);
        return drive_elevated(
            installer,
            &intent,
            carrier.target(),
            &crate::pkexec::SystemPkexec::resolve()?,
            Some(&control.events),
            Some(&control.cancellation),
        );
    }
    let context = crate::worker::WorkerContext {
        roots: MachineRoots::production(),
        systemd: crate::machine::SystemdRoots::production(),
        invoking_uid: 0,
        expected_client_pid: std::process::id(),
        session: SessionId::new_v7(),
        worker_exe: installer.to_path_buf(),
        carrier_pin: None,
        fault: crate::worker::WorkerFault::None,
    };
    drive_loopback(context, &intent, Some(control))
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
