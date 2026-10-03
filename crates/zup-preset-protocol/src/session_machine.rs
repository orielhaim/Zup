//! The rules a session follows, as one state machine both peers run.
//!
//! A handshake that is correct in production and correct in the development
//! simulator is one handshake, not two. The host and the preset disagree about
//! almost everything else - one owns the machine, one draws a window - but the
//! order of the first two messages, the protocol version each states, the
//! session identity both must carry, and which side may speak next are the same
//! facts, so they live here rather than twice.
//!
//! Everything here is a pure function of what has been received. Nothing in
//! this module needs a pipe, a window, or a runtime, which is what lets a test
//! drive a whole session - including every refusal - without one.

use crate::{
    Capabilities, Configuration, Envelope, HostHello, Message, PRESET_PROTOCOL_VERSION, PeerRole,
    PresetHello, ProductIdentity, SessionId, Snapshot, WireError, negotiate, sender_is_allowed,
};

/// Where a connection has reached.
///
/// Named for the handshake rather than for the session, because this is the
/// exchange's own progress and not what a preset draws: the state a preset
/// renders is a [`Snapshot`], and the session it draws it through is the SDK's.
/// A preset never sees this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Handshake {
    /// Nothing has been sent.
    Opening,
    /// This side has said hello and is waiting for the other.
    Awaiting { role: PeerRole },
    /// Both sides have said hello. The preset has no state to draw yet.
    Completed,
    /// The preset has been told what the installer is doing.
    Live { snapshot: Box<Snapshot> },
    /// One side ended the session.
    Closed,
}

/// What a session wants its caller to do next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionProgress {
    /// Write this frame and send it.
    ///
    /// A framed envelope rather than a bare message: the session owns the
    /// outbound sequence, so a caller that framed its own would have to reach
    /// into that counter to agree with it.
    Send(Envelope),
    /// The preset asked for something. Only a host sees this.
    Act(crate::Action),
    /// Nothing more; the session is finished.
    Done,
}

/// One end of one connection, whichever end it is.
///
/// The role is fixed at construction, so a host and a preset running this one
/// type cannot disagree about the handshake: there is only one handshake.
///
/// A session is a *connection*, not an installation. A host that relaunches a
/// preset after a crash opens a new session with a new id, and publishes the
/// current state into it in full: that is what makes "a newly connected UI
/// always receives the latest complete `Snapshot`" true by construction
/// rather than by replay, and it is why the authoritative state lives in the
/// host and not here.
pub struct Session {
    id: SessionId,
    role: PeerRole,
    state: Handshake,
    inbound: crate::SequenceTracker,
    outbound: u64,
    /// What this side offers: a host's capabilities, a preset's requirements.
    declared: Capabilities,
    /// The identity this side stated in its hello.
    identity: Identity,
    /// The current configuration, so a republish does not need the caller to
    /// remember it.
    configuration: Configuration,
}

/// The name and version this side stated in its hello.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Identity {
    Host(Box<ProductIdentity>),
    Preset { name: String, version: String },
}

impl Session {
    /// Open a host session providing `capabilities` for `product`.
    pub fn host(id: SessionId, capabilities: Capabilities, product: ProductIdentity) -> Self {
        Self::new(
            id,
            PeerRole::Host,
            capabilities,
            Identity::Host(Box::new(product)),
        )
    }

    /// Open a preset session requiring `capabilities`.
    pub fn preset(
        id: SessionId,
        capabilities: Capabilities,
        name: String,
        version: String,
    ) -> Self {
        Self::new(
            id,
            PeerRole::Preset,
            capabilities,
            Identity::Preset { name, version },
        )
    }

    fn new(id: SessionId, role: PeerRole, declared: Capabilities, identity: Identity) -> Self {
        Self {
            id,
            role,
            state: Handshake::Opening,
            inbound: crate::SequenceTracker::new(),
            outbound: 0,
            declared,
            identity,
            configuration: Configuration::empty(),
        }
    }

    pub fn id(&self) -> SessionId {
        self.id
    }

    pub fn role(&self) -> PeerRole {
        self.role
    }

    pub fn handshake(&self) -> &Handshake {
        &self.state
    }

    /// The identity this side presented.
    pub fn identity(&self) -> &Identity {
        &self.identity
    }

    /// Whether either side has ended the session.
    pub fn is_closed(&self) -> bool {
        matches!(self.state, Handshake::Closed)
    }

    /// The host's answer to a preset's hello.
    pub fn host_hello(&self) -> HostHello {
        match &self.identity {
            Identity::Host(product) => HostHello::new(
                self.id,
                self.declared.clone(),
                (**product).clone(),
                env!("CARGO_PKG_VERSION"),
            ),
            Identity::Preset { .. } => panic!("a preset session has no host hello to build"),
        }
    }

    /// This side's first message.
    pub fn opening(&mut self) -> Result<Envelope, WireError> {
        let message = match &self.identity {
            Identity::Preset { name, version } => Message::PresetHello(PresetHello {
                protocol_version: PRESET_PROTOCOL_VERSION,
                session: self.id,
                preset: name.clone(),
                preset_version: version.clone(),
                required_capabilities: self.declared.clone(),
            }),
            Identity::Host(_) => Message::HostHello(self.host_hello()),
        };
        self.state = Handshake::Awaiting { role: self.peer() };
        self.frame(message)
    }

    fn peer(&self) -> PeerRole {
        match self.role {
            PeerRole::Host => PeerRole::Preset,
            PeerRole::Preset => PeerRole::Host,
            PeerRole::Either => PeerRole::Either,
        }
    }

    /// Take one frame from the peer and say what to do next.
    pub fn receive(&mut self, envelope: Envelope) -> Result<SessionProgress, WireError> {
        if matches!(self.state, Handshake::Closed) {
            return Err(WireError::SessionClosed);
        }
        if envelope.version != PRESET_PROTOCOL_VERSION {
            return Err(WireError::VersionMismatch {
                expected: PRESET_PROTOCOL_VERSION,
                found: envelope.version,
            });
        }
        if envelope.session != self.id {
            return Err(WireError::SessionMismatch);
        }
        if !sender_is_allowed(self.peer(), &envelope.message) {
            return Err(WireError::UnexpectedMessage {
                expected: self.peer().name(),
                found: crate::message_name(&envelope.message),
            });
        }
        self.inbound.accept(envelope.sequence)?;
        match envelope.message {
            Message::Closed => {
                self.state = Handshake::Closed;
                Ok(SessionProgress::Done)
            }
            Message::PresetHello(hello) => self.greet(hello),
            Message::HostHello(hello) => self.greet_host(hello),
            Message::Configuration(configuration) => {
                configuration.validate().map_err(WireError::Configuration)?;
                self.configuration = *configuration;
                Ok(SessionProgress::Done)
            }
            Message::Snapshot(snapshot) => {
                self.state = Handshake::Live { snapshot };
                Ok(SessionProgress::Done)
            }
            Message::Action(action) => Ok(SessionProgress::Act(action)),
        }
    }

    /// A host receiving a preset's hello.
    ///
    /// The preset opens, so the host greets one. A second hello on the same
    /// connection is refused here rather than re-answered: reconnection is a
    /// new session with a new id, and this frame would already have been
    /// refused for carrying one.
    fn greet(&mut self, hello: PresetHello) -> Result<SessionProgress, WireError> {
        if self.role != PeerRole::Host || !matches!(self.state, Handshake::Opening) {
            return Err(WireError::UnexpectedMessage {
                expected: "an action",
                found: "ui_hello",
            });
        }
        check_version(hello.protocol_version)?;
        if hello.session != self.id {
            return Err(WireError::SessionMismatch);
        }
        negotiate(&self.declared, &hello.required_capabilities)?;
        self.state = Handshake::Completed;
        let answer = self.frame(Message::HostHello(self.host_hello()))?;
        Ok(SessionProgress::Send(answer))
    }

    /// A preset receiving a host's answer.
    fn greet_host(&mut self, hello: HostHello) -> Result<SessionProgress, WireError> {
        if self.role != PeerRole::Preset || !matches!(self.state, Handshake::Awaiting { .. }) {
            return Err(WireError::UnexpectedMessage {
                expected: "a configuration or snapshot",
                found: "host_hello",
            });
        }
        check_version(hello.protocol_version)?;
        if hello.session != self.id {
            return Err(WireError::SessionMismatch);
        }
        negotiate(&hello.capabilities, &self.declared)?;
        self.state = Handshake::Completed;
        Ok(SessionProgress::Done)
    }

    /// Send the current configuration and state to a connected preset.
    pub fn publish(
        &mut self,
        configuration: Configuration,
        snapshot: Box<Snapshot>,
    ) -> Result<Vec<Envelope>, WireError> {
        configuration.validate().map_err(WireError::Configuration)?;
        self.configuration = configuration;
        self.state = Handshake::Live {
            snapshot: snapshot.clone(),
        };
        Ok(vec![
            self.frame(Message::Configuration(Box::new(self.configuration.clone())))?,
            self.frame(Message::Snapshot(snapshot))?,
        ])
    }

    /// Wrap a message in the next frame from this side.
    pub fn frame(&mut self, message: Message) -> Result<Envelope, WireError> {
        if !sender_is_allowed(self.role, &message) {
            return Err(WireError::UnexpectedMessage {
                expected: self.role.name(),
                found: crate::message_name(&message),
            });
        }
        self.outbound += 1;
        Ok(Envelope {
            version: PRESET_PROTOCOL_VERSION,
            session: self.id,
            sequence: self.outbound,
            message,
        })
    }
}

fn check_version(found: u32) -> Result<(), WireError> {
    if found != PRESET_PROTOCOL_VERSION {
        return Err(WireError::VersionMismatch {
            expected: PRESET_PROTOCOL_VERSION,
            found,
        });
    }
    Ok(())
}
