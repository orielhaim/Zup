//! The process transport a zup installer preset and its host speak over.
//!
//! A preset is a separate process. Something has to carry frames between the
//! two, and this is it: the bootstrap that finds the host, the two byte
//! channels that follow, and the encoding of one
//! [`UiEnvelope`](zup_ui_protocol::UiEnvelope) per message.
//!
//! # What this crate is responsible for
//!
//! - **OS process IPC.** `ipc-channel` owns that. It is Servo's cross-platform
//!   implementation, and using it is the reason a preset runs from the same
//!   source on Windows, macOS, and Linux.
//! - **The bootstrap.** The host creates a one-shot server, gives the preset one
//!   name on its command line, and the preset collects it. The single message
//!   that carries is the session id and the preset's ends of the two channels.
//! - **A duplex channel**, its lifetime, and its errors.
//! - **Framing.** One encoded envelope per message, bounded in both directions.
//!
//! # What it is not responsible for
//!
//! The wire format is [`zup_ui_protocol`]'s and stays there. `ipc-channel` moves
//! opaque bytes and never learns what an envelope is. That is what keeps
//! protocol compatibility a decision Zup makes rather than one that follows
//! from a dependency's serde representation.
//!
//! # On security
//!
//! A preset is a child process the host launches, and it is native code that is
//! not sandboxed. This transport therefore does not claim to be a privilege
//! boundary and does not borrow the elevated worker's apparatus: there is no
//! name derived from a session to guess, no ACL to get wrong, and no peer
//! identity to verify, because the endpoint the preset collects is created after
//! the preset is launched and exists for exactly one client.
//!
//! The protections that do apply are the protocol's own, enforced above this
//! crate: version validation, session identity, monotonic sequence numbers,
//! bounded message size, a bounded bootstrap wait, and the host's validation of
//! every action against the state it owns. A preset that sends nonsense is
//! refused; a preset that is not talking to its own host has no endpoint to
//! reach.

use std::time::Duration;

use ipc_channel::ipc::{IpcBytesReceiver, IpcBytesSender, IpcOneShotServer, IpcSender};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use zup_ui_protocol::{MAX_FRAME_BYTES, UiEnvelope, decode, encode};

/// How long a host waits for the preset to collect its endpoint.
///
/// The endpoint exists before the preset is launched, so this covers process
/// start and scheduling rather than a negotiation. It is still bounded: a child
/// that never arrives must not hang an installer.
pub const ACCEPT_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a host waits for the preset's first frame.
///
/// Distinct from [`ACCEPT_TIMEOUT`] because it bounds a different wait, and
/// because a preset that got this far and then stopped is a different failure
/// from one that never started. Without it a preset that collects its endpoint
/// and then says nothing holds the host on an `recv` that will never return - and
/// an installer waiting on a window that is never coming is the one outcome the
/// bound exists to prevent.
pub const GREETING_TIMEOUT: Duration = Duration::from_secs(30);

/// What went wrong between a host and a preset.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("the preset's transport could not be created: {0}")]
    Bootstrap(#[source] std::io::Error),

    #[error("the preset did not collect its endpoint in time")]
    AcceptTimeout,

    #[error("the preset collected its endpoint and then said nothing")]
    GreetingTimeout,

    #[error("the preset's transport failed: {0}")]
    Transport(String),

    #[error("the peer sent a message this transport cannot carry: {0}")]
    Protocol(#[from] zup_ui_protocol::UiWireError),

    #[error(
        "this program is a zup installer preset; it is launched by an installer, not run directly"
    )]
    NotLaunchedByAHost,
}

/// What a host has created, before a preset has collected it.
pub struct Endpoint {
    server: Option<IpcOneShotServer<Collected>>,
    name: String,
}

impl Endpoint {
    /// Create the endpoint a preset will collect.
    pub fn create() -> Result<Self, Error> {
        let (server, name) = IpcOneShotServer::<Collected>::new().map_err(Error::Bootstrap)?;
        Ok(Self {
            server: Some(server),
            name,
        })
    }

    /// The name to hand the preset.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Wait for the preset to collect it, and return the session it carries.
    ///
    /// Takes `self` because an endpoint is single-use by construction: a second
    /// process must not be able to collect a name the first one already used.
    pub fn accept(mut self) -> Result<Channel, Error> {
        let server = self.server.take().expect("a live endpoint");
        let (collected, _wait) = one_shot(server);
        let payload = match collected.recv_timeout(ACCEPT_TIMEOUT) {
            Ok(payload) => payload.map_err(Error::Transport)?,
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

/// The bootstrap payload: the preset's ends of the two channels.
///
/// The host's own ends never leave the host process, so a preset cannot reach a
/// direction it was not given.
#[derive(Debug, Serialize, Deserialize)]
struct Collected {
    session: Uuid,
    /// The host reads actions here.
    read_actions: IpcBytesReceiver,
    /// The host writes snapshots here.
    write_snapshots: IpcBytesSender,
}

/// Wait for a one-shot client on a thread, so the wait itself can be bounded.
///
/// `IpcOneShotServer::accept` blocks for as long as the client takes. The thread
/// is left behind when it is the one that times out: the only thing it holds is
/// a name nothing else can use, and the installer is on its way out.
fn one_shot<T: serde::Serialize + serde::de::DeserializeOwned + Send + 'static>(
    server: IpcOneShotServer<T>,
) -> (
    std::sync::mpsc::Receiver<Result<T, String>>,
    std::thread::JoinHandle<()>,
) {
    let (delivered, collected) = std::sync::mpsc::sync_channel(1);
    let handle = std::thread::Builder::new()
        .name("zup-ui-bootstrap".into())
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

/// How a preset finds its host.
///
/// One name, passed on the command line. Everything else - the session, the
/// settings, the assets - arrives as protocol messages, so a preset's identity
/// cannot depend on how it was started and the command line stays readable by
/// every process on the machine without being worth reading.
pub struct Bootstrap {
    name: String,
}

impl Bootstrap {
    /// The flag a host passes the endpoint name under.
    pub const FLAG: &'static str = "--zup-endpoint";

    /// Read the endpoint name from the arguments a host passed.
    ///
    /// Arguments that are not this one are ignored, because a host may pass more
    /// and a preset must not refuse to start over one it does not recognise.
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

    /// The argument a host launches a preset with.
    pub fn to_argument(name: &str) -> String {
        format!("{}={name}", Self::FLAG)
    }

    /// Collect the endpoint the host created, and return the session it carries.
    ///
    /// The preset keeps the sending end of the channel it writes on and the
    /// receiving end of the channel it reads on, and hands the host the other
    /// end of each. Each side therefore holds exactly the half it needs and
    /// cannot reach a direction it was not given.
    pub fn collect(&self) -> Result<Channel, Error> {
        let sender: IpcSender<Collected> = IpcSender::connect(self.name.clone())
            .map_err(|error| Error::Transport(error.to_string()))?;
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
            .map_err(|error| Error::Transport(error.to_string()))?;
        Ok(Channel {
            sender: Sender { session, outbound },
            inbound,
        })
    }
}

/// One end of a preset session: what the host said, and a way to ask for
/// something.
///
/// Both peers are the same type. The host keeps the sender of the channel it
/// writes on and the receiver of the channel it reads; the preset receives the
/// complementary pair. One type means one place where a frame is encoded, and a
/// host and a preset cannot disagree about the transport because there is only
/// one transport.
///
/// The reading half is not cloneable and deliberately is not: one reader per
/// session is what keeps the order of the host's messages meaningful, and a
/// second reader would make the sequence numbers describe two interleavings
/// instead of one. A peer that writes from several threads takes
/// [`Channel::sender`].
pub struct Channel {
    sender: Sender,
    inbound: IpcBytesReceiver,
}

impl Channel {
    /// The session both sides carry, so a frame from another one is refused
    /// rather than acted on.
    pub fn session(&self) -> Uuid {
        self.sender.session
    }

    /// The writing half, for a peer that asks for something from another
    /// thread.
    pub fn sender(&self) -> &Sender {
        &self.sender
    }

    /// Read one envelope, or the end of the session.
    pub fn recv(&self) -> Result<UiEnvelope, Error> {
        self.bounded(MAX_FRAME_BYTES)
    }

    /// Read one envelope, or give up after `budget`, handing the channel back.
    ///
    /// The one read a host must not block on forever: a preset that has collected
    /// its endpoint and then gone quiet. Everything after the greeting is a read a
    /// host is willing to sit on, because the preset is the thing the person is
    /// looking at and it may take as long as it likes to speak.
    ///
    /// Takes and returns `self` because the bound is a thread: `ipc-channel`
    /// offers a blocking receive and a non-blocking one, and no receive with a
    /// deadline, so the wait that must end goes on a thread and the wait that need
    /// not does not. On a timeout the thread is left holding the channel - the
    /// same trade [`Endpoint::accept`] makes - and the caller is failing the
    /// handshake, which ends the preset and with it any reason to read again.
    pub fn recv_within(self, budget: Duration) -> Result<(Self, UiEnvelope), Error> {
        let Channel { sender, inbound } = self;
        // The receiver goes to the thread and comes back with the frame it read,
        // because a session that has greeted is a session that still has to be
        // read from.
        let (read, waiting) = std::sync::mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("zup-ui-greeting".into())
            .spawn(move || {
                let received = inbound.recv();
                let _ = read.send((inbound, received));
            })
            .map_err(|error| Error::Transport(error.to_string()))?;
        let (inbound, body) = match waiting.recv_timeout(budget) {
            Ok(received) => received,
            Err(_) => return Err(Error::GreetingTimeout),
        };
        let channel = Self { sender, inbound };
        let body = body.map_err(|error| Error::Transport(error.to_string()))?;
        if body.len() > MAX_FRAME_BYTES {
            return Err(Error::Protocol(
                zup_ui_protocol::UiWireError::FrameTooLarge {
                    max: MAX_FRAME_BYTES,
                },
            ));
        }
        let envelope = decode(&body)?;
        Ok((channel, envelope))
    }

    /// Read one envelope, refusing anything longer than `max`.
    ///
    /// The bound is checked before the body is decoded rather than after, so a
    /// peer that announces more than this transport will move is refused at the
    /// point where believing it would cost a parse. The bytes themselves have
    /// already crossed the operating system's boundary by this point, so this is
    /// a bound on what is interpreted rather than on what is transferred.
    fn bounded(&self, max: usize) -> Result<UiEnvelope, Error> {
        let body = self
            .inbound
            .recv()
            .map_err(|error| Error::Transport(error.to_string()))?;
        if body.len() > max {
            return Err(Error::Protocol(
                zup_ui_protocol::UiWireError::FrameTooLarge { max },
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

/// The writing half of a session: a way to ask the host for something, from any
/// thread.
///
/// A session has one reading half and as many writing halves as the peer needs,
/// because a click can come from a drawing task, a timer, or a background worker
/// and none of them should have to know which.
#[derive(Clone)]
pub struct Sender {
    session: Uuid,
    outbound: IpcBytesSender,
}

impl Sender {
    /// The session both sides carry.
    pub fn session(&self) -> Uuid {
        self.session
    }

    /// Write one envelope.
    pub fn send(&self, envelope: &UiEnvelope) -> Result<(), Error> {
        let body = encode(envelope)?;
        if body.len() > MAX_FRAME_BYTES {
            return Err(Error::Protocol(
                zup_ui_protocol::UiWireError::FrameTooLarge {
                    max: MAX_FRAME_BYTES,
                },
            ));
        }
        self.outbound
            .send(&body)
            .map_err(|error| Error::Transport(error.to_string()))
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

    /// A host that is never collected must give up rather than hang: a person
    /// who started an installer should not be waiting on a window that will
    /// never appear.
    #[test]
    fn a_host_gives_up_on_a_preset_that_never_arrives() {
        let (server, _name) = IpcOneShotServer::<Collected>::new().expect("create an endpoint");
        let (collected, _wait) = one_shot(server);
        assert!(matches!(
            collected.recv_timeout(Duration::from_millis(50)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));
    }

    /// A peer that collects the endpoint and then says nothing must not hold a
    /// host on its greeting.
    ///
    /// The bound is the whole claim. Without it, a preset that got this far and
    /// then stopped is an installer waiting forever on a window that is never
    /// coming, holding a tree it will never release. The writer below outlives the
    /// call on purpose: nothing has ended the session, so the deadline is the only
    /// thing that can.
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

    /// A frame that arrives inside the budget is read normally, and the channel
    /// comes back with it.
    ///
    /// The other half of the bound: a deadline that turned every read into a
    /// failure would satisfy the test above by refusing to talk to presets. The
    /// channel surviving is the part that matters afterwards - a session that has
    /// greeted is a session that still has to be read from.
    #[test]
    fn a_frame_inside_the_budget_is_read_and_the_channel_survives() {
        let session = Uuid::now_v7();
        let hello = UiEnvelope {
            version: zup_ui_protocol::UI_PROTOCOL_VERSION,
            session: zup_ui_protocol::UiSessionId(session),
            sequence: 1,
            message: zup_ui_protocol::UiMessage::UiHello(zup_ui_protocol::UiHello {
                protocol_version: zup_ui_protocol::UI_PROTOCOL_VERSION,
                session: zup_ui_protocol::UiSessionId(session),
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

    /// A preset that has gone is the end of the session, not a hang. This is
    /// what lets a host notice a crashed window rather than waiting on it.
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

    /// A frame longer than the transport will move is refused at the point
    /// where believing it would cost a parse.
    #[test]
    fn an_oversized_frame_is_refused_without_being_decoded() {
        let session = Uuid::now_v7();
        let closing = UiEnvelope {
            version: zup_ui_protocol::UI_PROTOCOL_VERSION,
            session: zup_ui_protocol::UiSessionId(session),
            sequence: 1,
            message: zup_ui_protocol::UiMessage::Closed,
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
                zup_ui_protocol::UiWireError::FrameTooLarge { max: 16 }
            ))
        ));
    }
}
