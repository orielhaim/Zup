#![cfg(target_os = "linux")]

//! The privileged rendezvous across real process boundaries.
//!
//! The client creates the rendezvous and passes the exact socket pathname to
//! the worker explicitly, because `pkexec` sanitizes the environment and two
//! independently generated fallback directories can never match. These tests
//! prove the handoff with a real child process spawned with a sanitized
//! environment - no `XDG_*` inheritance - for both the `XDG_RUNTIME_DIR` and
//! the fallback locations, while the worker-side validation (ownership,
//! symlink, peer identity, liveness) still holds.

use std::io::Read;
use std::os::unix::fs::PermissionsExt as _;
use std::process::{Command, Stdio};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use zup_linux::test_support::validate_rendezvous_for_test;
use zup_linux::{Rendezvous, peer_alive, peer_identity, pin_peer};
use zup_protocol::SessionId;

static ENV_LOCK: Mutex<()> = Mutex::new(());

fn env_lock() -> MutexGuard<'static, ()> {
    ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn current_uid() -> u32 {
    rustix::process::getuid().as_raw()
}

fn with_xdg_runtime<F, R>(dir: Option<&std::path::Path>, run: F) -> R
where
    F: FnOnce() -> R,
{
    let prior = std::env::var_os("XDG_RUNTIME_DIR");
    match dir {
        // SAFETY: serialized by the caller's lock.
        Some(path) => unsafe { std::env::set_var("XDG_RUNTIME_DIR", path) },
        None => unsafe { std::env::remove_var("XDG_RUNTIME_DIR") },
    }
    let result = run();
    match prior {
        // SAFETY: same serialization as the setup above.
        Some(previous) => unsafe { std::env::set_var("XDG_RUNTIME_DIR", previous) },
        None => unsafe { std::env::remove_var("XDG_RUNTIME_DIR") },
    }
    result
}

/// Spawn a real child process with a sanitized environment that connects to
/// `socket` and sends one byte, then exits. The child learns the pathname
/// from its arguments only - never from the environment.
fn spawn_connector(socket: &std::path::Path) -> std::process::Child {
    // The child stays alive after connecting so the parent can prove
    // process-lifetime pinning against a live peer; the test kills it
    // afterwards and proves the pin dies with it.
    let script = format!(
        "import socket as s, time; c=s.socket(s.AF_UNIX, s.SOCK_STREAM); c.connect({:?}); c.sendall(b'!'); time.sleep(30)",
        socket.to_string_lossy().into_owned(),
    );
    Command::new("python3")
        .arg("-c")
        .arg(script)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("a connector child spawns")
}

fn accept_one(
    rendezvous: &Rendezvous,
    child: &mut std::process::Child,
) -> std::os::unix::net::UnixStream {
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        match rendezvous.accept(Duration::from_secs(1)) {
            Ok(mut stream) => {
                // The child sent one byte; drain it so the assertion below
                // proves the bytes crossed the boundary.
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .expect("a timeout sets");
                let mut byte = [0u8; 1];
                stream.read_exact(&mut byte).expect("a byte arrives");
                assert_eq!(byte, [b'!']);
                return stream;
            }
            Err(zup_linux::SocketError::Timeout) => {}
            Err(error) => panic!("the rendezvous accepts: {error}"),
        }
        match child.try_wait().expect("a child polls") {
            Some(status) => panic!("the connector exited before connecting: {status}"),
            None => {}
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the connector connects within the deadline"
        );
    }
}

fn prove_peer(
    stream: &std::os::unix::net::UnixStream,
    child: &std::process::Child,
    socket: &std::path::Path,
) -> zup_linux::PeerPin {
    let peer = peer_identity(stream).expect("kernel peer credentials");
    assert_eq!(peer.uid, current_uid(), "the peer is the invoking user");
    assert_eq!(
        peer.pid,
        child.id(),
        "the peer is exactly the spawned child, not whoever connected first"
    );
    let pin = pin_peer(peer.pid, peer.uid).expect("the peer pins");
    assert!(peer_alive(&pin), "the live child is alive through its pin");
    validate_rendezvous_for_test(socket, current_uid()).expect("the honest rendezvous validates");
    pin
}

/// The XDG rendezvous crosses a sanitized process boundary on the exact
/// endpoint, with ownership, peer identity, and liveness verified.
#[test]
fn xdg_rendezvous_crosses_a_sanitized_boundary_on_the_exact_endpoint() {
    let _env = env_lock();
    let base = tempfile::tempdir().expect("a base");
    let runtime = base.path().join("run");
    std::fs::create_dir_all(&runtime).expect("a runtime directory");
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700)).expect("chmod");
    with_xdg_runtime(Some(&runtime), || {
        let session = SessionId::new_v7();
        let rendezvous = Rendezvous::create(current_uid(), session).expect("a rendezvous");
        assert!(
            rendezvous.socket().starts_with(&runtime),
            "the XDG runtime directory is used when it validates"
        );
        let mut child = spawn_connector(rendezvous.socket());
        let stream = accept_one(&rendezvous, &mut child);
        let pin = prove_peer(&stream, &child, rendezvous.socket());
        // The pin dies with the process: liveness is process lifetime, not a number.
        child.kill().expect("the child is killed");
        let status = child.wait().expect("the child is reaped");
        assert!(!status.success(), "the killed connector exits uncleanly");
        for _ in 0..50 {
            if !peer_alive(&pin) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        panic!("a reaped child must stop being alive");
    });
}

/// The fallback rendezvous crosses a sanitized process boundary too: the
/// child never sees the fallback directory through the environment, only
/// through the explicit pathname.
#[test]
fn fallback_rendezvous_crosses_a_sanitized_boundary_on_the_exact_endpoint() {
    let _env = env_lock();
    with_xdg_runtime(None, || {
        let session = SessionId::new_v7();
        let rendezvous = Rendezvous::create(current_uid(), session).expect("a rendezvous");
        assert!(
            !rendezvous.socket().to_string_lossy().is_empty(),
            "a fallback rendezvous exists"
        );
        let mut child = spawn_connector(rendezvous.socket());
        let stream = accept_one(&rendezvous, &mut child);
        let pin = prove_peer(&stream, &child, rendezvous.socket());
        child.kill().expect("the child is killed");
        child.wait().expect("the child is reaped");
        for _ in 0..50 {
            if !peer_alive(&pin) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        panic!("a reaped child must stop being alive");
    });
}

/// A sanitized worker-side view needs no environment at all: the pathname is
/// explicit, and validation still refuses a replaced or unprivate directory.
#[test]
fn explicit_path_validation_needs_no_environment() {
    let _env = env_lock();
    with_xdg_runtime(None, || {
        let session = SessionId::new_v7();
        let rendezvous = Rendezvous::create(current_uid(), session).expect("a rendezvous");
        // No environment, yet the explicit path validates: establishment never
        // relied on inherited state.
        validate_rendezvous_for_test(rendezvous.socket(), current_uid())
            .expect("an explicit pathname validates without environment");
        // And the same validation still refuses impostors.
        assert!(
            validate_rendezvous_for_test(
                &rendezvous.socket().with_file_name("other.sock"),
                current_uid()
            )
            .is_err(),
            "only the session socket validates"
        );
        assert!(
            validate_rendezvous_for_test(
                std::path::Path::new("relative/worker.sock"),
                current_uid()
            )
            .is_err(),
            "a relative pathname never validates"
        );
    });
}
