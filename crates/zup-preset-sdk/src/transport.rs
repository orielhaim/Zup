//! The preset's end of a session.
//!
//! All of the process transport is [`zup_preset_ipc`]. What is here is the part a
//! preset cares about: the handshake, the state the host opened the session
//! with, and the thread that keeps the session current while the window draws.
//!
//! There is no async runtime and no platform code in a preset's life. A preset
//! draws and answers; one background thread reads the host and hands what it
//! said to the application, and a click is framed and written straight back
//! through the same channel.

use std::sync::{Arc, Mutex};

use futures_channel::mpsc::UnboundedReceiver;
use zup_preset_ipc::{Channel as Transport, Sender};
use zup_preset_protocol::{
    HostHello, Session, SessionProgress, Action, Capabilities, Configuration, Message,
    SessionId, Snapshot, WireError,
};

pub use zup_preset_ipc::{Bootstrap, Error as TransportError};

/// What a preset calls itself.
///
/// The host checks these against the record it keeps of the presets it ships, so
/// a preset whose executable disagrees with the package that described it is
/// refused rather than trusted.
pub struct Identity {
    pub name: String,
    pub version: String,
    pub required_capabilities: Capabilities,
}

/// A connected session, and the state the host opened it with.
pub struct Opened {
    pub channel: Channel,
    pub host: HostHello,
    pub configuration: Configuration,
    pub snapshot: Snapshot,
}

/// A connected session: messages from the host, and a way to ask for something.
///
/// The two halves are the preset's entire reach into the installer. There is no
/// third thing to call.
pub struct Channel {
    /// The host's messages, for the task that draws.
    incoming: UnboundedReceiver<Result<Message, TransportError>>,
    /// A handle the UI thread keeps, so a click can ask for something.
    requester: Requester,
}

impl Channel {
    /// Connect, complete the opening exchange, and start reading.
    ///
    /// All of it, here, rather than spread across the call site: a preset cannot
    /// draw anything without a configuration and a snapshot, and a caller that
    /// had to remember the order would eventually get it wrong in a way that
    /// only shows up as a window that never appears.
    pub fn open(bootstrap: &Bootstrap, identity: Identity) -> Result<Opened, TransportError> {
        let transport = bootstrap.collect()?;
        let mut session = Session::preset(
            SessionId(transport.session()),
            identity.required_capabilities,
            identity.name,
            identity.version,
        );
        let host = greet(&transport, &mut session)?;
        let (configuration, snapshot) = opening(&transport, &mut session)?;

        let rules = Arc::new(Mutex::new(session));
        let requester = Requester {
            sender: transport.sender().clone(),
            session: Arc::clone(&rules),
        };
        let (messages, incoming) = futures_channel::mpsc::unbounded();
        std::thread::Builder::new()
            .name("zup-ui-session".into())
            .spawn(move || read(transport, rules, messages))
            .map_err(|error| TransportError::Transport(error.to_string()))?;

        Ok(Opened {
            channel: Channel {
                incoming,
                requester,
            },
            host,
            configuration,
            snapshot,
        })
    }

    /// The reading half and a handle for asking.
    ///
    /// The reader moves into the preset's background thread and the requester
    /// stays with the UI thread, because a drawing task is `Send` and a channel
    /// receiver is not something two threads should share.
    pub fn into_parts(
        self,
    ) -> (
        UnboundedReceiver<Result<Message, TransportError>>,
        Requester,
    ) {
        (self.incoming, self.requester)
    }

    /// A handle a UI thread can use to ask for something.
    pub fn requests(&self) -> Requester {
        self.requester.clone()
    }
}

/// One way to ask the host for something, from any thread.
///
/// The envelope is framed here rather than on a queue, so a click is on the wire
/// when it happens and the session's outbound counter has exactly one writer.
#[derive(Clone)]
pub struct Requester {
    sender: Sender,
    session: Arc<Mutex<Session>>,
}

impl Requester {
    pub fn send(&self, action: Action) {
        let framed = self
            .session
            .lock()
            .expect("the session")
            .frame(Message::Action(action));
        if let Ok(envelope) = framed {
            let _ = self.sender.send(&envelope);
        }
    }
}

impl crate::session::ActionSender for Requester {
    fn send(&self, action: Action) {
        Requester::send(self, action);
    }
}

/// Read what the host says, for as long as it says anything.
fn read(
    transport: Transport,
    session: Arc<Mutex<Session>>,
    messages: futures_channel::mpsc::UnboundedSender<Result<Message, TransportError>>,
) {
    while let Ok(envelope) = transport.recv() {
        let message = envelope.message.clone();
        if session
            .lock()
            .expect("the session")
            .receive(envelope)
            .is_err()
        {
            break;
        }
        if messages.unbounded_send(Ok(message)).is_err() {
            // The preset's UI thread is gone, so there is nobody left to show a
            // state to.
            return;
        }
    }
    let _ = messages.unbounded_send(Err(TransportError::Transport(
        "the host closed the session".into(),
    )));
}

/// Say hello and read the host's answer.
fn greet(transport: &Transport, session: &mut Session) -> Result<HostHello, TransportError> {
    let opening = session.opening()?;
    transport.sender().send(&opening)?;
    let answer = transport.recv()?;
    let host = match &answer.message {
        Message::HostHello(host) => host.clone(),
        found => return Err(unexpected("a host hello", found)),
    };
    match session.receive(answer)? {
        SessionProgress::Done => Ok(host),
        _ => Err(TransportError::Protocol(WireError::UnexpectedMessage {
            expected: "a host hello",
            found: "a progress step",
        })),
    }
}

/// The configuration and the first state, in the order the host sends them.
fn opening(
    transport: &Transport,
    session: &mut Session,
) -> Result<(Configuration, Snapshot), TransportError> {
    let mut configuration = None;
    let mut snapshot = None;
    while configuration.is_none() || snapshot.is_none() {
        let envelope = transport.recv()?;
        let message = envelope.message.clone();
        session.receive(envelope)?;
        match message {
            Message::Configuration(value) => configuration = Some(*value),
            Message::Snapshot(value) => snapshot = Some(*value),
            found => return Err(unexpected("a configuration and a state", &found)),
        }
    }
    Ok((
        configuration.expect("a configuration"),
        snapshot.expect("a snapshot"),
    ))
}

fn unexpected(expected: &'static str, found: &Message) -> TransportError {
    TransportError::Protocol(WireError::UnexpectedMessage {
        expected,
        found: zup_preset_protocol::message_name(found),
    })
}
