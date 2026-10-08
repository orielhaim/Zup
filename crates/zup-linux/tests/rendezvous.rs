#![cfg(target_os = "linux")]
#![cfg(feature = "test-support")]

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

fn spawn_connector(socket: &std::path::Path) -> std::process::Child {
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
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .expect("a timeout sets");
                let mut byte = [0u8; 1];
                stream.read_exact(&mut byte).expect("a byte arrives");
                assert_eq!(byte, [b'!']);
                return stream;
            }
            Err(zup_linux::IpcError::Timeout) => {}
            Err(error) => panic!("the rendezvous accepts: {error}"),
        }
        if let Some(status) = child.try_wait().expect("a child polls") {
            panic!("the connector exited before connecting: {status}");
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

#[test]
fn explicit_path_validation_needs_no_environment() {
    let _env = env_lock();
    with_xdg_runtime(None, || {
        let session = SessionId::new_v7();
        let rendezvous = Rendezvous::create(current_uid(), session).expect("a rendezvous");

        validate_rendezvous_for_test(rendezvous.socket(), current_uid())
            .expect("an explicit pathname validates without environment");

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
