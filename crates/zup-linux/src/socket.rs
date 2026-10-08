use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::error::IpcError;
use zup_protocol::{MAX_FRAME_BYTES, SessionId, WireEnvelope, decode_payload, encode_payload};

pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

pub const FRAME_TIMEOUT: Duration = Duration::from_secs(30);

const LENGTH_WIDTH: usize = 4;

#[derive(Debug)]
pub struct Rendezvous {
    directory: PathBuf,
    socket: PathBuf,
    listener: UnixListener,
}

impl Rendezvous {
    pub fn create(invoking_uid: u32, session: SessionId) -> Result<Self, IpcError> {
        let directory = match validated_xdg(invoking_uid) {
            Some(dir) => {
                let directory = dir
                    .join("zup")
                    .join("privileged")
                    .join(session.0.to_string());
                create_private_dir_all(&directory)?;
                directory
            }

            None => fallback_base()?,
        };
        let socket = directory.join("worker.sock");

        let _ = std::fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket).map_err(|source| IpcError::Io {
            path: socket.display().to_string(),
            source,
        })?;
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).map_err(
            |source| IpcError::Io {
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

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    pub fn accept(&self, timeout: Duration) -> Result<UnixStream, IpcError> {
        self.listener
            .set_nonblocking(true)
            .map_err(|source| IpcError::Io {
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
                    return Err(IpcError::Io {
                        path: self.socket.display().to_string(),
                        source,
                    });
                }
            }
            if start.elapsed() >= timeout {
                return Err(IpcError::Timeout);
            }
        }
    }

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

pub fn connect(socket: &Path, timeout: Duration) -> Result<UnixStream, IpcError> {
    let start = std::time::Instant::now();
    loop {
        match UnixStream::connect(socket) {
            Ok(stream) => return Ok(stream),
            Err(source) if start.elapsed() < timeout => {
                if source.kind() == std::io::ErrorKind::NotFound {
                    std::thread::sleep(Duration::from_millis(25));
                    continue;
                }
                return Err(IpcError::Io {
                    path: socket.display().to_string(),
                    source,
                });
            }
            Err(source) => {
                return Err(IpcError::Io {
                    path: socket.display().to_string(),
                    source,
                });
            }
        }
    }
}

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

fn fallback_base() -> Result<PathBuf, IpcError> {
    let root = std::env::temp_dir();
    for _ in 0..16 {
        let name = format!("zup-priv-{}", SessionId::new_v7().0.as_simple());
        let base = root.join(name);
        match std::fs::create_dir(&base) {
            Ok(()) => {
                std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700)).map_err(
                    |source| IpcError::Io {
                        path: base.display().to_string(),
                        source,
                    },
                )?;
                return Ok(base);
            }
            Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(source) => {
                return Err(IpcError::Io {
                    path: base.display().to_string(),
                    source,
                });
            }
        }
    }
    Err(IpcError::NoRuntime(
        "could not create a private fallback runtime directory".to_owned(),
    ))
}

fn create_private_dir_all(path: &Path) -> Result<(), IpcError> {
    let mut missing: Vec<&Path> = Vec::new();
    let mut cursor = path;
    loop {
        match std::fs::symlink_metadata(cursor) {
            Ok(metadata) => {
                if !metadata.is_dir() || metadata.file_type().is_symlink() {
                    return Err(IpcError::Io {
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
                        return Err(IpcError::NoRuntime(
                            "no usable runtime directory".to_owned(),
                        ));
                    }
                }
            }
            Err(source) => {
                return Err(IpcError::Io {
                    path: cursor.display().to_string(),
                    source,
                });
            }
        }
    }
    for level in missing.iter().rev() {
        std::fs::create_dir(level).map_err(|source| IpcError::Io {
            path: level.display().to_string(),
            source,
        })?;
        std::fs::set_permissions(level, std::fs::Permissions::from_mode(0o700)).map_err(
            |source| IpcError::Io {
                path: level.display().to_string(),
                source,
            },
        )?;
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerIdentity {
    pub pid: u32,

    pub uid: u32,

    pub gid: u32,
}

pub fn peer_identity(stream: &UnixStream) -> Result<PeerIdentity, IpcError> {
    use std::os::fd::AsFd as _;
    let cred = rustix::net::sockopt::socket_peercred(stream.as_fd()).map_err(|error| {
        IpcError::PeerAuth(format!("peer credentials are unavailable: {error}"))
    })?;
    Ok(PeerIdentity {
        pid: cred.pid.as_raw_pid() as u32,
        uid: cred.uid.as_raw(),
        gid: cred.gid.as_raw(),
    })
}

#[derive(Debug)]
pub enum PeerPin {
    PidFd(PidFdPin),

    Legacy(LegacyPin),
}

#[derive(Debug)]
pub struct PidFdPin {
    fd: rustix::fd::OwnedFd,
}

#[derive(Debug, Clone)]
pub struct LegacyPin {
    pid: u32,
    uid: u32,
    start_time: u64,
}

pub fn pin_peer(pid: u32, uid: u32) -> Result<PeerPin, IpcError> {
    let raw = rustix::process::Pid::from_raw(pid as i32)
        .ok_or_else(|| IpcError::PeerAuth(format!("peer pid {pid} is not a process id")))?;
    match rustix::process::pidfd_open(raw, rustix::process::PidfdFlags::NONBLOCK) {
        Ok(fd) => Ok(PeerPin::PidFd(PidFdPin { fd })),
        Err(_) => {
            let start_time = process_start_time(pid)
                .ok_or_else(|| IpcError::PeerAuth(format!("peer pid {pid} has no start time")))?;
            Ok(PeerPin::Legacy(LegacyPin {
                pid,
                uid,
                start_time,
            }))
        }
    }
}

pub fn peer_alive(pin: &PeerPin) -> bool {
    match pin {
        PeerPin::PidFd(pinned) => {
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

fn process_start_time(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = stat.rfind(')')?;
    let fields: Vec<&str> = stat[after_comm + 1..].split_whitespace().collect();

    fields.get(19)?.parse::<u64>().ok()
}

fn process_uid(pid: u32) -> Option<u32> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    for line in status.lines() {
        if let Some(values) = line.strip_prefix("Uid:") {
            return values.split_whitespace().next()?.parse::<u32>().ok();
        }
    }
    None
}

pub fn send_envelope(stream: &mut UnixStream, envelope: &WireEnvelope) -> Result<(), IpcError> {
    let bytes = encode_payload(envelope).map_err(|error| IpcError::Framing(error.to_string()))?;
    let length = u32::try_from(bytes.len())
        .map_err(|_| IpcError::Framing("frame exceeds u32 range".to_owned()))?;
    stream
        .write_all(&length.to_be_bytes())
        .and_then(|()| stream.write_all(&bytes))
        .and_then(|()| stream.flush())
        .map_err(|source| IpcError::Io {
            path: "<send>".to_owned(),
            source,
        })
}

pub fn recv_envelope(stream: &mut UnixStream, timeout: Duration) -> Result<WireEnvelope, IpcError> {
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|source| IpcError::Io {
            path: "<recv>".to_owned(),
            source,
        })?;
    let mut length = [0u8; LENGTH_WIDTH];
    read_exact(stream, &mut length)?;
    let length = u32::from_be_bytes(length) as usize;
    if length == 0 || length > MAX_FRAME_BYTES {
        return Err(IpcError::Framing(format!(
            "frame length {length} is outside the bound {MAX_FRAME_BYTES}"
        )));
    }
    let mut bytes = vec![0u8; length];
    read_exact(stream, &mut bytes)?;
    decode_payload(&bytes).map_err(|error| IpcError::Framing(error.to_string()))
}

fn read_exact(stream: &mut UnixStream, mut buffer: &mut [u8]) -> Result<(), IpcError> {
    while !buffer.is_empty() {
        match stream.read(buffer) {
            Ok(0) => return Err(IpcError::Framing("peer closed mid-frame".to_owned())),
            Ok(consumed) => buffer = &mut buffer[consumed..],
            Err(source) => {
                if source.kind() == std::io::ErrorKind::TimedOut {
                    return Err(IpcError::Timeout);
                }
                return Err(IpcError::Io {
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
            Err(IpcError::Framing(_))
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
            Err(IpcError::Framing(_))
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
