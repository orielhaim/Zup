use std::sync::{Arc, Mutex};

use futures_channel::mpsc::UnboundedReceiver;
use zup_preset_ipc::{Channel as Transport, Sender};
use zup_preset_protocol::{
    Action, Capabilities, Configuration, HostHello, Message, Session, SessionId, SessionProgress,
    Snapshot, WireError,
};

pub use zup_preset_ipc::{Bootstrap, Error as TransportError};

pub struct Identity {
    pub name: String,
    pub version: String,
    pub required_capabilities: Capabilities,
}

pub struct Opened {
    pub channel: Channel,
    pub host: HostHello,
    pub configuration: Configuration,
    pub snapshot: Snapshot,
}

pub struct Channel {
    incoming: UnboundedReceiver<Result<Message, TransportError>>,
    requester: Requester,
}

impl Channel {
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
            .name("zup-preset-session".into())
            .spawn(move || read(transport, rules, messages))
            .map_err(TransportError::Transport)?;

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

    pub fn into_parts(
        self,
    ) -> (
        UnboundedReceiver<Result<Message, TransportError>>,
        Requester,
    ) {
        (self.incoming, self.requester)
    }
}

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

impl super::session::ActionSender for Requester {
    fn send(&self, action: Action) {
        Requester::send(self, action);
    }
}

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
            return;
        }
    }
    let _ = messages.unbounded_send(Err(TransportError::Transport(std::io::Error::other(
        "the host closed the session",
    ))));
}

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
