//! Test-only privileged-worker surface.
//!
//! Available only with the `test-support` feature, which production binaries
//! never enable. Everything here takes explicit isolated roots: no
//! environment variable and no IPC message selects them, and the real worker
//! always enforces [`MachineRoots::production`]. These helpers drive the
//! same worker code the privileged path serves - the transport differs, the
//! verification does not.

use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use zup_protocol::{SessionId, WireEnvelope};

use crate::machine::MachineRoots;
use crate::run::{LinuxAction, LinuxOutcome, LinuxRunError, LinuxRunRequest};

/// Isolated machine roots under a caller-held base, for machine-scope tests.
///
/// Nothing here touches the host's `/opt` or `/var/lib/zup`: every root
/// lives under `base`, which the test owns (usually a temporary directory
/// that vanishes on drop).
pub struct MachineTestRoots {
    /// The isolated roots to pass explicitly.
    pub roots: MachineRoots,
    /// The isolated state root, for request overrides.
    pub state: PathBuf,
}

impl MachineTestRoots {
    /// Create the isolated tree under `base`.
    pub fn isolate_in(base: &Path) -> Self {
        let programs = base.join("opt");
        let state = base.join("var").join("lib").join("zup");
        let shared_data = base.join("var").join("opt");
        for directory in [&programs, &shared_data, state.parent().expect("a parent")] {
            std::fs::create_dir_all(directory).expect("an isolated root");
        }
        let roots = MachineRoots::new(programs, state.clone(), shared_data);
        Self { roots, state }
    }
}

/// Run one machine-scope lifecycle through the loopback worker with isolated
/// roots: the same plan, path, and policy validation as the privileged
/// path, with no `pkexec` hop and no host mutation.
pub fn run_machine_isolated(
    installer: &PathBuf,
    roots: &MachineRoots,
    state: &PathBuf,
    action: LinuxAction,
    install_dir_override: Option<PathBuf>,
) -> Result<LinuxOutcome, LinuxRunError> {
    crate::elevate::run_machine_loopback_for_test(
        &LinuxRunRequest {
            installer: installer.clone(),
            scope: zup_core::SelectedScope::Machine,
            state_root: Some(state.clone()),
            action,
            install_dir_override,
        },
        roots,
    )
}

/// Serve one worker session on `stream` with isolated roots, for protocol
/// attack tests: malformed frames, replays, substitutions, and peer games
/// speak to the real session driver.
pub fn serve_worker_isolated(
    stream: &mut UnixStream,
    roots: &MachineRoots,
    invoking_uid: u32,
    session: SessionId,
    worker_exe: &PathBuf,
) -> Result<String, crate::worker::WorkerError> {
    crate::worker::serve_session(
        stream,
        crate::worker::WorkerContext {
            roots: roots.clone(),
            invoking_uid,
            session,
            worker_exe: worker_exe.clone(),
            carrier_pin: None,
        },
    )
}

/// Drive the real client handshake on `stream` with an expected plan, for
/// client-side verification tests: hello, capability, session, and digest
/// binding against an isolated worker.
pub fn drive_client_isolated(
    stream: &mut UnixStream,
    session: SessionId,
    intent: &zup_protocol::PrepareOperation,
    expected_digest: &str,
    expected_target: &zup_core::TargetTriple,
) -> Result<crate::run::LinuxOutcome, crate::run::LinuxRunError> {
    crate::elevate::drive_client(stream, session, intent, expected_digest, expected_target)
}

/// Plan one machine operation and return the intent plus the compiled
/// plan, for tests that journal an interrupted transaction.
///
/// The intent is what the client sends; the plan is what the worker must
/// independently reconstruct. Recovery tests journal the plan, then prove
/// the worker replays it before accepting new work.
pub fn plan_for_test(
    installer: &Path,
    roots: &MachineRoots,
    state: &Path,
    action: LinuxAction,
    install_dir_override: Option<PathBuf>,
) -> (
    zup_protocol::PrepareOperation,
    zup_transaction::TransactionPlan,
) {
    let request = crate::run::LinuxRunRequest {
        installer: installer.to_path_buf(),
        scope: zup_core::SelectedScope::Machine,
        state_root: Some(state.to_path_buf()),
        action,
        install_dir_override,
    };
    let (intent, expected) = crate::elevate::plan_expected(
        &request,
        &state.to_path_buf(),
        roots,
        rustix::process::geteuid().as_raw(),
    )
    .expect("the fixture plans");
    (intent, expected.plan)
}

/// Validate a rendezvous path the way the worker does: derived, then
/// proven. For socket-replacement attack tests.
pub fn validate_rendezvous_for_test(
    socket: &std::path::Path,
    invoking_uid: u32,
) -> Result<(), crate::worker::WorkerError> {
    crate::worker::validate_rendezvous(socket, invoking_uid)
}

/// Send one envelope on a test stream.
pub fn send_envelope_on(
    stream: &mut UnixStream,
    envelope: &WireEnvelope,
) -> Result<(), crate::socket::SocketError> {
    crate::socket::send_envelope(stream, envelope)
}

/// Receive one envelope on a test stream.
pub fn recv_envelope_on(
    stream: &mut UnixStream,
    timeout: Duration,
) -> Result<WireEnvelope, crate::socket::SocketError> {
    crate::socket::recv_envelope(stream, timeout)
}
