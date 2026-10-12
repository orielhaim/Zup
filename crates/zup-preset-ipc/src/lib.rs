use std::time::Duration;

use ipc_channel::ipc::{IpcBytesReceiver, IpcBytesSender, IpcOneShotServer, IpcSender};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use zup_preset_protocol::{Envelope, MAX_FRAME_BYTES, decode, encode};

pub const ACCEPT_TIMEOUT: Duration = Duration::from_secs(30);

pub const GREETING_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("the preset's transport could not be created: {0}")]
    Bootstrap(#[source] std::io::Error),

    #[error("the preset did not collect its endpoint in time")]
    AcceptTimeout,

    #[error("the preset collected its endpoint and then said nothing")]
    GreetingTimeout,

    #[error("the preset's transport failed: {0}")]
    Transport(#[source] std::io::Error),

    #[error("the peer sent a message this transport cannot carry: {0}")]
    Protocol(#[from] zup_preset_protocol::WireError),

    #[error(
        "this program is a zup installer preset; it is launched by an installer, not run directly"
    )]
    NotLaunchedByAHost,
}

impl Error {
    pub fn peer_gone(&self) -> bool {
        let Self::Transport(error) = self else {
            return false;
        };
        if matches!(
            error.kind(),
            std::io::ErrorKind::BrokenPipe
                | std::io::ErrorKind::NotConnected
                | std::io::ErrorKind::ConnectionAborted
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::UnexpectedEof
        ) {
            return true;
        }
        error
            .raw_os_error()
            .is_some_and(|code| PEER_GONE_CODES.contains(&code))
    }
}

const PEER_GONE_CODES: &[i32] = &[0x8007_00E8_u32 as i32, 109, 233];

fn other(error: impl std::fmt::Display) -> Error {
    Error::Transport(std::io::Error::other(error.to_string()))
}

pub struct Endpoint {
    server: Option<IpcOneShotServer<Collected>>,
    name: String,
}

impl Endpoint {
    pub fn create() -> Result<Self, Error> {
        let (server, name) = IpcOneShotServer::<Collected>::new().map_err(Error::Bootstrap)?;
        Ok(Self {
            server: Some(server),
            name,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn accept(mut self) -> Result<Channel, Error> {
        let server = self.server.take().expect("a live endpoint");
        let (collected, _wait) = one_shot(server);
        let payload = match collected.recv_timeout(ACCEPT_TIMEOUT) {
            Ok(payload) => payload.map_err(other)?,
            Err(_) => return Err(Error::AcceptTimeout),
        };
        Ok(Channel {
            sender: Sender {
                session: payload.session,
                outbound: payload.write_snapshots,
            },
            inbound: payload.read_actions,
        })
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct Collected {
    session: Uuid,
    read_actions: IpcBytesReceiver,
    write_snapshots: IpcBytesSender,
}

fn one_shot<T: serde::Serialize + serde::de::DeserializeOwned + Send + 'static>(
    server: IpcOneShotServer<T>,
) -> (
    std::sync::mpsc::Receiver<Result<T, String>>,
    std::thread::JoinHandle<()>,
) {
    let (delivered, collected) = std::sync::mpsc::sync_channel(1);
    let handle = std::thread::Builder::new()
        .name("zup-preset-bootstrap".into())
        .spawn(move || {
            let (_, payload) = match server.accept() {
                Ok(pair) => pair,
                Err(error) => {
                    let _ = delivered.send(Err(error.to_string()));
                    return;
                }
            };
            let _ = delivered.send(Ok(payload));
        })
        .expect("start the bootstrap waiter");
    (collected, handle)
}

pub struct Bootstrap {
    name: String,
}

impl Bootstrap {
    pub const FLAG: &'static str = "--zup-endpoint";

    pub fn from_arguments<I>(arguments: I) -> Result<Self, Error>
    where
        I: IntoIterator<Item = String>,
    {
        arguments
            .into_iter()
            .filter_map(|argument| {
                argument
                    .strip_prefix(Self::FLAG)
                    .and_then(|value| value.strip_prefix('='))
                    .map(str::to_owned)
            })
            .next()
            .map(|name| Self { name })
            .ok_or(Error::NotLaunchedByAHost)
    }

    pub fn to_argument(name: &str) -> String {
        format!("{}={name}", Self::FLAG)
    }

    pub fn collect(&self) -> Result<Channel, Error> {
        let sender: IpcSender<Collected> =
            IpcSender::connect(self.name.clone()).map_err(Error::Transport)?;
        let (outbound, read_actions) =
            ipc_channel::ipc::bytes_channel().map_err(Error::Bootstrap)?;
        let (write_snapshots, inbound) =
            ipc_channel::ipc::bytes_channel().map_err(Error::Bootstrap)?;
        let session = Uuid::now_v7();
        sender
            .send(Collected {
                session,
                read_actions,
                write_snapshots,
            })
            .map_err(other)?;
        Ok(Channel {
            sender: Sender { session, outbound },
            inbound,
        })
    }
}

pub struct Channel {
    sender: Sender,
    inbound: IpcBytesReceiver,
}

impl Channel {
    pub fn session(&self) -> Uuid {
        self.sender.session
    }

    pub fn sender(&self) -> &Sender {
        &self.sender
    }

    pub fn recv(&self) -> Result<Envelope, Error> {
        self.bounded(MAX_FRAME_BYTES)
    }

    pub fn recv_within(self, budget: Duration) -> Result<(Self, Envelope), Error> {
        let Channel { sender, inbound } = self;
        let (read, waiting) = std::sync::mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("zup-preset-greeting".into())
            .spawn(move || {
                let received = inbound.recv();
                let _ = read.send((inbound, received));
            })
            .map_err(Error::Transport)?;
        let (inbound, body) = match waiting.recv_timeout(budget) {
            Ok(received) => received,
            Err(_) => return Err(Error::GreetingTimeout),
        };
        let channel = Self { sender, inbound };
        let body = body.map_err(other)?;
        if body.len() > MAX_FRAME_BYTES {
            return Err(Error::Protocol(
                zup_preset_protocol::WireError::FrameTooLarge {
                    max: MAX_FRAME_BYTES,
                },
            ));
        }
        let envelope = decode(&body)?;
        Ok((channel, envelope))
    }

    fn bounded(&self, max: usize) -> Result<Envelope, Error> {
        let body = self.inbound.recv().map_err(other)?;
        if body.len() > max {
            return Err(Error::Protocol(
                zup_preset_protocol::WireError::FrameTooLarge { max },
            ));
        }
        Ok(decode(&body)?)
    }
}

impl std::fmt::Debug for Channel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Channel")
            .field("session", &self.sender.session)
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
pub struct Sender {
    session: Uuid,
    outbound: IpcBytesSender,
}

impl Sender {
    pub fn session(&self) -> Uuid {
        self.session
    }

    pub fn send(&self, envelope: &Envelope) -> Result<(), Error> {
        let body = encode(envelope)?;
        if body.len() > MAX_FRAME_BYTES {
            return Err(Error::Protocol(
                zup_preset_protocol::WireError::FrameTooLarge {
                    max: MAX_FRAME_BYTES,
                },
            ));
        }
        self.outbound.send(&body).map_err(Error::Transport)
    }
}

impl std::fmt::Debug for Sender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sender")
            .field("session", &self.session)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_host_gives_up_on_a_preset_that_never_arrives() {
        let (server, _name) = IpcOneShotServer::<Collected>::new().expect("create an endpoint");
        let (collected, _wait) = one_shot(server);
        assert!(matches!(
            collected.recv_timeout(Duration::from_millis(50)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));
    }

    #[test]
    fn a_peer_that_stops_after_collecting_is_given_up_on() {
        let (writer, reader) = ipc_channel::ipc::bytes_channel().expect("a channel");
        let (outbound, _unread) = ipc_channel::ipc::bytes_channel().expect("a channel");
        let channel = Channel {
            sender: Sender {
                session: Uuid::now_v7(),
                outbound,
            },
            inbound: reader,
        };
        let error = channel
            .recv_within(Duration::from_millis(100))
            .expect_err("a peer that says nothing is given up on");
        assert!(
            matches!(error, Error::GreetingTimeout),
            "and it says the greeting never arrived rather than reporting a broken transport: \
             {error}"
        );
        drop(writer);
    }

    #[test]
    fn a_frame_inside_the_budget_is_read_and_the_channel_survives() {
        let session = Uuid::now_v7();
        let hello = Envelope {
            version: zup_preset_protocol::PRESET_PROTOCOL_VERSION,
            session: zup_preset_protocol::SessionId(session),
            sequence: 1,
            message: zup_preset_protocol::Message::PresetHello(zup_preset_protocol::PresetHello {
                protocol_version: zup_preset_protocol::PRESET_PROTOCOL_VERSION,
                session: zup_preset_protocol::SessionId(session),
                preset: "bounded".into(),
                preset_version: "0.0.0".into(),
                required_capabilities: Default::default(),
            }),
        };

        let (writer, reader) = ipc_channel::ipc::bytes_channel().expect("a channel");
        let (outbound, _unread) = ipc_channel::ipc::bytes_channel().expect("a channel");
        writer
            .send(&encode(&hello).expect("an envelope encodes"))
            .expect("write");
        let channel = Channel {
            sender: Sender { session, outbound },
            inbound: reader,
        };

        let (channel, read) = channel
            .recv_within(Duration::from_secs(30))
            .expect("a frame inside the budget is read");
        assert_eq!(read, hello);
        assert_eq!(
            channel.session(),
            session,
            "and the channel comes back, because greeting is one read rather than the only one"
        );
    }

    #[test]
    fn a_gone_peer_is_told_from_a_transport_that_failed() {
        let (writer, reader) = ipc_channel::ipc::bytes_channel().expect("a channel");
        let (outbound, unread) = ipc_channel::ipc::bytes_channel().expect("a channel");
        drop(unread);
        let channel = Channel {
            sender: Sender {
                session: Uuid::now_v7(),
                outbound,
            },
            inbound: reader,
        };
        let closing = Envelope {
            version: zup_preset_protocol::PRESET_PROTOCOL_VERSION,
            session: zup_preset_protocol::SessionId(Uuid::now_v7()),
            sequence: 1,
            message: zup_preset_protocol::Message::Closed,
        };
        let gone = channel
            .sender
            .send(&closing)
            .expect_err("a write onto a peer that has gone fails");
        assert!(
            gone.peer_gone(),
            "and it says the peer is gone rather than that something broke: {gone}"
        );

        let refused = Error::Protocol(zup_preset_protocol::WireError::FrameTooLarge { max: 16 });
        assert!(
            !refused.peer_gone(),
            "and a peer that sent something unreadable is still a peer: {refused}"
        );
        let broken = Error::Transport(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
        assert!(
            !broken.peer_gone(),
            "as is a pipe this host was not allowed to write to: {broken}"
        );
        drop(writer);
    }

    #[test]
    fn a_dropped_peer_ends_the_session() {
        let (writer, reader) = ipc_channel::ipc::bytes_channel().expect("a channel");
        let (outbound, _unread) = ipc_channel::ipc::bytes_channel().expect("a channel");
        drop(writer);
        let channel = Channel {
            sender: Sender {
                session: Uuid::now_v7(),
                outbound,
            },
            inbound: reader,
        };
        assert!(
            channel.recv().is_err(),
            "a reader whose writer is gone reads as the end of the session"
        );
    }

    #[test]
    fn an_oversized_frame_is_refused_without_being_decoded() {
        let session = Uuid::now_v7();
        let closing = Envelope {
            version: zup_preset_protocol::PRESET_PROTOCOL_VERSION,
            session: zup_preset_protocol::SessionId(session),
            sequence: 1,
            message: zup_preset_protocol::Message::Closed,
        };

        let (writer, reader) = ipc_channel::ipc::bytes_channel().expect("a channel");
        let (outbound, _unread) = ipc_channel::ipc::bytes_channel().expect("a channel");
        let channel = Channel {
            sender: Sender { session, outbound },
            inbound: reader,
        };
        writer
            .send(&encode(&closing).expect("an envelope encodes"))
            .expect("write");
        assert_eq!(
            channel
                .bounded(MAX_FRAME_BYTES)
                .expect("a frame within the bound is read"),
            closing
        );

        let (writer, reader) = ipc_channel::ipc::bytes_channel().expect("a channel");
        let (outbound, _unread) = ipc_channel::ipc::bytes_channel().expect("a channel");
        let channel = Channel {
            sender: Sender { session, outbound },
            inbound: reader,
        };
        writer
            .send(&[0u8; 64])
            .expect("a peer that ignores the bound can still write");
        assert!(matches!(
            channel.bounded(16),
            Err(Error::Protocol(
                zup_preset_protocol::WireError::FrameTooLarge { max: 16 }
            ))
        ));
    }
}
