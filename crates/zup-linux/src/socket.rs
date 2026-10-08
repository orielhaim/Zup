//! Per-session private Unix IPC for the privileged worker.
//!
//! No TCP, no world-accessible socket, no predictable pathname. Each
//! elevation attempt creates one private rendezvous directory under the
//! invoking user's runtime area, binds one socket in it, serves one worker
//! lifetime through it, and removes it afterwards.
//!
//! Both peers verify each other with kernel evidence, never with claims
//! inside protocol messages:
//!
//! - the worker verifies the client: peer uid must equal the uid `pkexec`
//!   reports as the authorizing user, and the peer pid is pinned with a
//!   pidfd so PID reuse cannot substitute a new process for the
//!   authenticated one;
//! - the client verifies the worker: peer uid must be `0`, and the worker
//!   pid must equal the process the client launched.
//!
//! A bare PID is never the whole identity. Where the kernel provides pidfd
//! the pin is a pidfd held for the session; where it does not, the fallback
//! is pid plus process start time plus uid plus the connected socket's own
//! lifetime, and the fallback is explicit rather than silent.

use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use zup_protocol::{MAX_FRAME_BYTES, SessionId, WireEnvelope, decode_payload, encode_payload};

/// How long the handshake may take: connection, hello, prepare. Authentication
/// timing itself belongs to `pkexec`/polkit and is not raced here.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
/// How long one frame may take once the handshake is done.
pub const FRAME_TIMEOUT: Duration = Duration::from_secs(30);
/// Length prefix width: one big-endian `u32` ahead of every frame.
const LENGTH_WIDTH: usize = 4;

/// Why the rendezvous could not be created or served.
#[derive(Debug, thiserror::Error)]
pub enum SocketError {
    #[error("no usable runtime directory: {0}")]
    NoRuntime(String),

    #[error("rendezvous I/O at `{path}`: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("peer authentication failed: {0}")]
    AuthFailed(String),

    #[error("protocol framing failed: {0}")]
    Framing(String),

    #[error("handshake timed out")]
    Timeout,
}

/// One private rendezvous: the directory, the socket in it, and the listener.
#[derive(Debug)]
pub struct Rendezvous {
    directory: PathBuf,
    socket: PathBuf,
    listener: UnixListener,
}

impl Rendezvous {
    /// Create the rendezvous for one session under the invoking user's
    /// runtime area.
    ///
    /// Prefers `$XDG_RUNTIME_DIR/zup/privileged/<session>/` after validating
    /// the runtime directory itself; otherwise the freshly created
    /// unpredictable fallback directory itself is the session directory.
    /// The fallback name already carries the session's randomness, so no
    /// further nesting is added - nesting would push the socket pathname
    /// past the `SUN_LEN` bound. Never a caller-supplied pathname, and never
    /// a predictable shared socket.
    pub fn create(invoking_uid: u32, session: SessionId) -> Result<Self, SocketError> {
        let directory = match validated_xdg(invoking_uid) {
            Some(dir) => {
                let directory = dir
                    .join("zup")
                    .join("privileged")
                    .join(session.0.to_string());
                create_private_dir_all(&directory)?;
                directory
            }
            // The fallback base is freshly created, unpredictable, and
            // private: it already is a per-session directory, so the socket
            // lives directly in it.
            None => fallback_base()?,
        };
        let socket = directory.join("worker.sock");
        // A stale socket from a crashed run must not be served: it names a
        // session that is over.
        let _ = std::fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket).map_err(|source| SocketError::Io {
            path: socket.display().to_string(),
            source,
        })?;
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).map_err(
            |source| SocketError::Io {
                path: socket.display().to_string(),
                source,
            },
        )?;
        Ok(Self {
            directory,
            socket,
            listener,
        })
    }

    /// Where the worker connects.
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// Accept the one worker connection, with a bounded wait.
    pub fn accept(&self, timeout: Duration) -> Result<UnixStream, SocketError> {
        self.listener
            .set_nonblocking(true)
            .map_err(|source| SocketError::Io {
                path: self.socket.display().to_string(),
                source,
            })?;
        let start = std::time::Instant::now();
        loop {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    let _ = self.listener.set_nonblocking(false);
                    return Ok(stream);
                }
                Err(source)
                    if source.kind() == std::io::ErrorKind::WouldBlock
                        && start.elapsed() < timeout =>
                {
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(source) => {
                    return Err(SocketError::Io {
                        path: self.socket.display().to_string(),
                        source,
                    });
                }
            }
            if start.elapsed() >= timeout {
                return Err(SocketError::Timeout);
            }
        }
    }

    /// Remove the rendezvous directory. Best effort after the session ends;
    /// a leftover is a private empty directory, not a listening socket.
    pub fn remove(&self) {
        let _ = std::fs::remove_file(&self.socket);
        let _ = std::fs::remove_dir(&self.directory);
    }
}

impl Drop for Rendezvous {
    fn drop(&mut self) {
        self.remove();
    }
}

/// Connect to a rendezvous socket with a bounded wait.
pub fn connect(socket: &Path, timeout: Duration) -> Result<UnixStream, SocketError> {
    let start = std::time::Instant::now();
    loop {
        match UnixStream::connect(socket) {
            Ok(stream) => return Ok(stream),
            Err(source) if start.elapsed() < timeout => {
                if source.kind() == std::io::ErrorKind::NotFound {
                    std::thread::sleep(Duration::from_millis(25));
                    continue;
                }
                return Err(SocketError::Io {
                    path: socket.display().to_string(),
                    source,
                });
            }
            Err(source) => {
                return Err(SocketError::Io {
                    path: socket.display().to_string(),
                    source,
                });
            }
        }
    }
}

/// The validated `$XDG_RUNTIME_DIR`, if one is usable.
///
/// Validation is ownership and type, not just spelling: the variable is
/// untrusted user environment, and a runtime directory owned by someone else
/// or passing through a link is not a private area.
///
/// The result is only ever used by the client to *create* its own rendezvous;
/// the worker never derives this path. The client passes the created socket
/// pathname to the worker explicitly (as a command-line argument), so a
/// sanitized `pkexec` environment - or an independently generated fallback
/// directory - cannot desynchronize the two ends.
fn validated_xdg(invoking_uid: u32) -> Option<PathBuf> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from)?;
    if !dir.is_absolute() {
        return None;
    }
    let Ok(metadata) = std::fs::symlink_metadata(&dir) else {
        return None;
    };
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return None;
    }
    use std::os::unix::fs::MetadataExt as _;
    if metadata.uid() != invoking_uid || metadata.permissions().mode() & 0o022 != 0 {
        return None;
    }
    Some(dir)
}

/// A fresh unpredictable directory under the system temporary area.
///
/// `0700` and created exclusively: the session identity feeding the name
/// carries the operating system's randomness (a v7 uuid), so the name is
/// not guessable and a pre-existing entry is a collision that fails rather
/// than a directory that is reused.
fn fallback_base() -> Result<PathBuf, SocketError> {
    let root = std::env::temp_dir();
    for _ in 0..16 {
        let name = format!("zup-priv-{}", SessionId::new_v7().0.as_simple());
        let base = root.join(name);
        match std::fs::create_dir(&base) {
            Ok(()) => {
                std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700)).map_err(
                    |source| SocketError::Io {
                        path: base.display().to_string(),
                        source,
                    },
                )?;
                return Ok(base);
            }
            Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(source) => {
                return Err(SocketError::Io {
                    path: base.display().to_string(),
                    source,
                });
            }
        }
    }
    Err(SocketError::NoRuntime(
        "could not create a private fallback runtime directory".to_owned(),
    ))
}

/// Create a directory hierarchy whose every created level is private.
fn create_private_dir_all(path: &Path) -> Result<(), SocketError> {
    let mut missing: Vec<&Path> = Vec::new();
    let mut cursor = path;
    loop {
        match std::fs::symlink_metadata(cursor) {
            Ok(metadata) => {
                if !metadata.is_dir() || metadata.file_type().is_symlink() {
                    return Err(SocketError::Io {
                        path: cursor.display().to_string(),
                        source: std::io::Error::other("not a real directory"),
                    });
                }
                break;
            }
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                missing.push(cursor);
                match cursor.parent() {
                    Some(parent) if !parent.as_os_str().is_empty() => cursor = parent,
                    _ => {
                        return Err(SocketError::NoRuntime(
                            "no usable runtime directory".to_owned(),
                        ));
                    }
                }
            }
            Err(source) => {
                return Err(SocketError::Io {
                    path: cursor.display().to_string(),
                    source,
                });
            }
        }
    }
    for level in missing.iter().rev() {
        std::fs::create_dir(level).map_err(|source| SocketError::Io {
            path: level.display().to_string(),
            source,
        })?;
        std::fs::set_permissions(level, std::fs::Permissions::from_mode(0o700)).map_err(
            |source| SocketError::Io {
                path: level.display().to_string(),
                source,
            },
        )?;
    }
    Ok(())
}

/// Kernel peer credentials of a connected socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerIdentity {
    /// The peer process id, at connect time.
    pub pid: u32,
    /// The peer user id.
    pub uid: u32,
    /// The peer group id.
    pub gid: u32,
}

/// Read the kernel's peer credentials for `stream`.
///
/// `SO_PEERCRED` is evidence, not a claim: the kernel reports who holds the
/// other end, and nothing the peer sent can change the answer.
pub fn peer_identity(stream: &UnixStream) -> Result<PeerIdentity, SocketError> {
    use std::os::fd::AsFd as _;
    let cred = rustix::net::sockopt::socket_peercred(stream.as_fd()).map_err(|error| {
        SocketError::AuthFailed(format!("peer credentials are unavailable: {error}"))
    })?;
    Ok(PeerIdentity {
        pid: cred.pid.as_raw_pid() as u32,
        uid: cred.uid.as_raw(),
        gid: cred.gid.as_raw(),
    })
}

/// A pinned peer process: identity that survives PID reuse.
///
/// The pidfd, held open for the session, refers to the process rather than
/// to the number: if the peer dies, operations on it fail, and a new
/// process reusing the number is a different process the pin never names.
#[derive(Debug)]
pub enum PeerPin {
    /// A pidfd held for the session. The strong case.
    PidFd(PidFdPin),
    /// PID plus start time plus uid, with the socket lifetime as the outer
    /// bound. Only when the kernel has no pidfd; explicit, documented, and
    /// still checked on every use rather than once at handshake.
    Legacy(LegacyPin),
}

/// A held pidfd plus the process it was pinned from.
///
/// The descriptor is the identity: it names the process, not the number,
///
/// so PID reuse cannot substitute a new process for the authenticated one.
#[derive(Debug)]
pub struct PidFdPin {
    fd: rustix::fd::OwnedFd,
}

/// PID plus process start time plus uid: the documented fallback.
#[derive(Debug, Clone)]
pub struct LegacyPin {
    pid: u32,
    uid: u32,
    start_time: u64,
}

/// Pin `pid`/`uid` for the session.
///
/// Prefers a pidfd; falls back to pid plus start time only when the kernel
/// has no pidfd to give, and says so in the return value rather than
/// silently.
pub fn pin_peer(pid: u32, uid: u32) -> Result<PeerPin, SocketError> {
    let raw = rustix::process::Pid::from_raw(pid as i32)
        .ok_or_else(|| SocketError::AuthFailed(format!("peer pid {pid} is not a process id")))?;
    match rustix::process::pidfd_open(raw, rustix::process::PidfdFlags::NONBLOCK) {
        Ok(fd) => Ok(PeerPin::PidFd(PidFdPin { fd })),
        Err(_) => {
            let start_time = process_start_time(pid).ok_or_else(|| {
                SocketError::AuthFailed(format!("peer pid {pid} has no start time"))
            })?;
            Ok(PeerPin::Legacy(LegacyPin {
                pid,
                uid,
                start_time,
            }))
        }
    }
}

/// Whether the pinned peer is still the authenticated process.
///
/// A pidfd names the process, so liveness is a poll on the pin: a dead peer
/// fails, and a reused PID is a different process this pin never named. The
/// legacy pin re-reads the start time and the uid, so a reused PID whose
/// start time differs fails too. Checked wherever the session acts on the
/// peer still being there - before Execute runs, not only at handshake.
pub fn peer_alive(pin: &PeerPin) -> bool {
    match pin {
        PeerPin::PidFd(pinned) => {
            // A pidfd is pollable: it reports readable when the process it
            // names exits. A zero-timeout poll that reports nothing ready
            // means the peer is still running; anything else - readable, or
            // an error - means the pin no longer names a live process, and
            // the pin cannot name anyone else.
            let mut waiting = [rustix::event::PollFd::new(
                &pinned.fd,
                rustix::event::PollFlags::IN,
            )];
            let immediate = rustix::event::Timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            rustix::event::poll(&mut waiting, Some(&immediate)).is_ok_and(|ready| ready == 0)
        }
        PeerPin::Legacy(pinned) => {
            process_start_time(pinned.pid).is_some_and(|start| start == pinned.start_time)
                && process_uid(pinned.pid).is_some_and(|uid| uid == pinned.uid)
        }
    }
}

/// A process's start time (jiffies since boot), or `None` when unreadable.
///
/// Parsed after the final `)` of the comm field, because the process name
/// itself may hold spaces and parentheses. Unreadable means unprovable,
/// which the caller treats as failure rather than as absence.
fn process_start_time(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = stat.rfind(')')?;
    let fields: Vec<&str> = stat[after_comm + 1..].split_whitespace().collect();
    // starttime is field 22 overall; field 3 (state) is index 0 here.
    fields.get(19)?.parse::<u64>().ok()
}

/// A process's real uid, or `None` when unreadable.
fn process_uid(pid: u32) -> Option<u32> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    for line in status.lines() {
        if let Some(values) = line.strip_prefix("Uid:") {
            return values.split_whitespace().next()?.parse::<u32>().ok();
        }
    }
    None
}

/// Send one envelope as a length-delimited frame.
pub fn send_envelope(stream: &mut UnixStream, envelope: &WireEnvelope) -> Result<(), SocketError> {
    let bytes =
        encode_payload(envelope).map_err(|error| SocketError::Framing(error.to_string()))?;
    let length = u32::try_from(bytes.len())
        .map_err(|_| SocketError::Framing("frame exceeds u32 range".to_owned()))?;
    stream
        .write_all(&length.to_be_bytes())
        .and_then(|()| stream.write_all(&bytes))
        .and_then(|()| stream.flush())
        .map_err(|source| SocketError::Io {
            path: "<send>".to_owned(),
            source,
        })
}

/// Receive one envelope: bounded length first, payload second.
///
/// The length is validated before any payload-sized buffer exists, so a
/// hostile prefix buys no allocation.
pub fn recv_envelope(
    stream: &mut UnixStream,
    timeout: Duration,
) -> Result<WireEnvelope, SocketError> {
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|source| SocketError::Io {
            path: "<recv>".to_owned(),
            source,
        })?;
    let mut length = [0u8; LENGTH_WIDTH];
    read_exact(stream, &mut length)?;
    let length = u32::from_be_bytes(length) as usize;
    if length == 0 || length > MAX_FRAME_BYTES {
        return Err(SocketError::Framing(format!(
            "frame length {length} is outside the bound {MAX_FRAME_BYTES}"
        )));
    }
    let mut bytes = vec![0u8; length];
    read_exact(stream, &mut bytes)?;
    decode_payload(&bytes).map_err(|error| SocketError::Framing(error.to_string()))
}

fn read_exact(stream: &mut UnixStream, mut buffer: &mut [u8]) -> Result<(), SocketError> {
    while !buffer.is_empty() {
        match stream.read(buffer) {
            Ok(0) => return Err(SocketError::Framing("peer closed mid-frame".to_owned())),
            Ok(consumed) => buffer = &mut buffer[consumed..],
            Err(source) => {
                if source.kind() == std::io::ErrorKind::TimedOut {
                    return Err(SocketError::Timeout);
                }
                return Err(SocketError::Io {
                    path: "<recv>".to_owned(),
                    source,
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use zup_protocol::{Message, PROTOCOL_VERSION};

    fn envelope(session: SessionId, sequence: u64) -> WireEnvelope {
        WireEnvelope {
            version: PROTOCOL_VERSION,
            session_id: session,
            sequence,
            message: Message::Ping,
        }
    }

    #[test]
    fn frames_round_trip_over_a_socketpair() {
        let (mut first, mut second) = UnixStream::pair().expect("a pair");
        let session = SessionId::new_v7();
        send_envelope(&mut first, &envelope(session, 7)).expect("send");
        let received = recv_envelope(&mut second, FRAME_TIMEOUT).expect("recv");
        assert_eq!(received.sequence, 7);
        assert_eq!(received.session_id, session);
    }

    #[test]
    fn an_oversized_length_buys_no_allocation() {
        let (mut first, mut second) = UnixStream::pair().expect("a pair");
        let hostile = (MAX_FRAME_BYTES as u32 + 1).to_be_bytes();
        first.write_all(&hostile).expect("send");
        first.flush().expect("flush");
        assert!(matches!(
            recv_envelope(&mut second, FRAME_TIMEOUT),
            Err(SocketError::Framing(_))
        ));
    }

    #[test]
    fn truncation_is_a_framing_error_not_a_hang() {
        let (mut first, mut second) = UnixStream::pair().expect("a pair");
        first.write_all(&16u32.to_be_bytes()).expect("send");
        first.write_all(b"half").expect("send");
        drop(first);
        assert!(matches!(
            recv_envelope(&mut second, FRAME_TIMEOUT),
            Err(SocketError::Framing(_))
        ));
    }

    #[test]
    fn peer_credentials_name_this_process() {
        let (first, _) = UnixStream::pair().expect("a pair");
        let identity = peer_identity(&first).expect("credentials");
        assert_eq!(identity.pid, std::process::id());
        assert_eq!(identity.uid, rustix::process::getuid().as_raw());
    }

    #[test]
    fn a_session_pins_itself_and_stays_alive() {
        let pinned =
            pin_peer(std::process::id(), rustix::process::getuid().as_raw()).expect("self pins");
        assert!(peer_alive(&pinned), "this process is alive");
    }

    #[test]
    fn a_dead_peer_is_not_alive() {
        let mut child = std::process::Command::new("/bin/true")
            .spawn()
            .expect("a short child");
        let pid = child.id();
        let uid = rustix::process::getuid().as_raw();
        let pinned = pin_peer(pid, uid).expect("a child pins");
        assert!(peer_alive(&pinned));
        child.wait().expect("reap");
        // Give the kernel a moment to report the exit through the pin.
        for _ in 0..50 {
            if !peer_alive(&pinned) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("a reaped child must stop being alive");
    }

    #[test]
    fn pinning_a_pid_that_is_not_there_fails() {
        assert!(pin_peer(u32::MAX / 2, 0).is_err());
    }

    #[test]
    fn rendezvous_lifecycle_is_private_and_removed() {
        let session = SessionId::new_v7();
        let uid = rustix::process::getuid().as_raw();
        let rendezvous = Rendezvous::create(uid, session).expect("a rendezvous");
        assert!(rendezvous.socket().exists());
        let mode = std::fs::symlink_metadata(rendezvous.directory.as_path())
            .expect("stat")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700, "the session directory is private");
        let path = rendezvous.socket().to_path_buf();
        drop(rendezvous);
        assert!(!path.exists(), "the socket leaves with the session");
    }
}
