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
    HostHello, ProductIdentity, UI_PROTOCOL_VERSION, UiCapabilities, UiConfiguration, UiEnvelope,
    UiHello, UiMessage, UiPeerRole, UiSessionId, UiSnapshot, UiWireError, negotiate,
    sender_is_allowed,
};

/// Where a session has reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionState {
    /// Nothing has been sent.
    Opening,
    /// This side has said hello and is waiting for the other.
    Awaiting { role: UiPeerRole },
    /// Both sides have said hello. The preset has no state to draw yet.
    Handshaken,
    /// The preset has been told what the installer is doing.
    Live { snapshot: Box<UiSnapshot> },
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
    Send(UiEnvelope),
    /// The preset asked for something. Only a host sees this.
    Act(crate::UiAction),
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
/// always receives the latest complete `UiSnapshot`" true by construction
/// rather than by replay, and it is why the authoritative state lives in the
/// host and not here.
pub struct Session {
    id: UiSessionId,
    role: UiPeerRole,
    state: SessionState,
    inbound: crate::SequenceTracker,
    outbound: u64,
    /// What this side offers: a host's capabilities, a preset's requirements.
    declared: UiCapabilities,
    /// The identity this side stated in its hello.
    identity: Identity,
    /// The current configuration, so a republish does not need the caller to
    /// remember it.
    configuration: UiConfiguration,
}

/// The name and version this side stated in its hello.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Identity {
    Host(Box<ProductIdentity>),
    Preset { name: String, version: String },
}

impl Session {
    /// Open a host session providing `capabilities` for `product`.
    pub fn host(id: UiSessionId, capabilities: UiCapabilities, product: ProductIdentity) -> Self {
        Self::new(
            id,
            UiPeerRole::Host,
            capabilities,
            Identity::Host(Box::new(product)),
        )
    }

    /// Open a preset session requiring `capabilities`.
    pub fn preset(
        id: UiSessionId,
        capabilities: UiCapabilities,
        name: String,
        version: String,
    ) -> Self {
        Self::new(
            id,
            UiPeerRole::Preset,
            capabilities,
            Identity::Preset { name, version },
        )
    }

    fn new(
        id: UiSessionId,
        role: UiPeerRole,
        declared: UiCapabilities,
        identity: Identity,
    ) -> Self {
        Self {
            id,
            role,
            state: SessionState::Opening,
            inbound: crate::SequenceTracker::new(),
            outbound: 0,
            declared,
            identity,
            configuration: UiConfiguration::empty(),
        }
    }

    pub fn id(&self) -> UiSessionId {
        self.id
    }

    pub fn role(&self) -> UiPeerRole {
        self.role
    }

    pub fn state(&self) -> &SessionState {
        &self.state
    }

    /// The identity this side presented.
    pub fn identity(&self) -> &Identity {
        &self.identity
    }

    /// Whether either side has ended the session.
    pub fn is_closed(&self) -> bool {
        matches!(self.state, SessionState::Closed)
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
    pub fn opening(&mut self) -> Result<UiEnvelope, UiWireError> {
        let message = match &self.identity {
            Identity::Preset { name, version } => UiMessage::UiHello(UiHello {
                protocol_version: UI_PROTOCOL_VERSION,
                session: self.id,
                preset: name.clone(),
                preset_version: version.clone(),
                required_capabilities: self.declared.clone(),
            }),
            Identity::Host(_) => UiMessage::HostHello(self.host_hello()),
        };
        self.state = SessionState::Awaiting { role: self.peer() };
        self.frame(message)
    }

    fn peer(&self) -> UiPeerRole {
        match self.role {
            UiPeerRole::Host => UiPeerRole::Preset,
            UiPeerRole::Preset => UiPeerRole::Host,
            UiPeerRole::Either => UiPeerRole::Either,
        }
    }

    /// Take one frame from the peer and say what to do next.
    pub fn receive(&mut self, envelope: UiEnvelope) -> Result<SessionProgress, UiWireError> {
        if matches!(self.state, SessionState::Closed) {
            return Err(UiWireError::SessionClosed);
        }
        if envelope.version != UI_PROTOCOL_VERSION {
            return Err(UiWireError::VersionMismatch {
                expected: UI_PROTOCOL_VERSION,
                found: envelope.version,
            });
        }
        if envelope.session != self.id {
            return Err(UiWireError::SessionMismatch);
        }
        if !sender_is_allowed(self.peer(), &envelope.message) {
            return Err(UiWireError::UnexpectedMessage {
                expected: self.peer().name(),
                found: crate::message_name(&envelope.message),
            });
        }
        self.inbound.accept(envelope.sequence)?;
        match envelope.message {
            UiMessage::Closed => {
                self.state = SessionState::Closed;
                Ok(SessionProgress::Done)
            }
            UiMessage::UiHello(hello) => self.greet(hello),
            UiMessage::HostHello(hello) => self.greet_host(hello),
            UiMessage::Configuration(configuration) => {
                configuration
                    .validate()
                    .map_err(UiWireError::Configuration)?;
                self.configuration = *configuration;
                Ok(SessionProgress::Done)
            }
            UiMessage::Snapshot(snapshot) => {
                self.state = SessionState::Live { snapshot };
                Ok(SessionProgress::Done)
            }
            UiMessage::Action(action) => Ok(SessionProgress::Act(action)),
        }
    }

    /// A host receiving a preset's hello.
    ///
    /// The preset opens, so the host greets one. A second hello on the same
    /// connection is refused here rather than re-answered: reconnection is a
    /// new session with a new id, and this frame would already have been
    /// refused for carrying one.
    fn greet(&mut self, hello: UiHello) -> Result<SessionProgress, UiWireError> {
        if self.role != UiPeerRole::Host || !matches!(self.state, SessionState::Opening) {
            return Err(UiWireError::UnexpectedMessage {
                expected: "an action",
                found: "ui_hello",
            });
        }
        check_version(hello.protocol_version)?;
        if hello.session != self.id {
            return Err(UiWireError::SessionMismatch);
        }
        negotiate(&self.declared, &hello.required_capabilities)?;
        self.state = SessionState::Handshaken;
        let answer = self.frame(UiMessage::HostHello(self.host_hello()))?;
        Ok(SessionProgress::Send(answer))
    }

    /// A preset receiving a host's answer.
    fn greet_host(&mut self, hello: HostHello) -> Result<SessionProgress, UiWireError> {
        if self.role != UiPeerRole::Preset || !matches!(self.state, SessionState::Awaiting { .. }) {
            return Err(UiWireError::UnexpectedMessage {
                expected: "a configuration or snapshot",
                found: "host_hello",
            });
        }
        check_version(hello.protocol_version)?;
        if hello.session != self.id {
            return Err(UiWireError::SessionMismatch);
        }
        negotiate(&hello.capabilities, &self.declared)?;
        self.state = SessionState::Handshaken;
        Ok(SessionProgress::Done)
    }

    /// Send the current configuration and state to a connected preset.
    pub fn publish(
        &mut self,
        configuration: UiConfiguration,
        snapshot: Box<UiSnapshot>,
    ) -> Result<Vec<UiEnvelope>, UiWireError> {
        configuration
            .validate()
            .map_err(UiWireError::Configuration)?;
        self.configuration = configuration;
        self.state = SessionState::Live {
            snapshot: snapshot.clone(),
        };
        Ok(vec![
            self.frame(UiMessage::Configuration(Box::new(
                self.configuration.clone(),
            )))?,
            self.frame(UiMessage::Snapshot(snapshot))?,
        ])
    }

    /// Wrap a message in the next frame from this side.
    pub fn frame(&mut self, message: UiMessage) -> Result<UiEnvelope, UiWireError> {
        if !sender_is_allowed(self.role, &message) {
            return Err(UiWireError::UnexpectedMessage {
                expected: self.role.name(),
                found: crate::message_name(&message),
            });
        }
        self.outbound += 1;
        Ok(UiEnvelope {
            version: UI_PROTOCOL_VERSION,
            session: self.id,
            sequence: self.outbound,
            message,
        })
    }
}

fn check_version(found: u32) -> Result<(), UiWireError> {
    if found != UI_PROTOCOL_VERSION {
        return Err(UiWireError::VersionMismatch {
            expected: UI_PROTOCOL_VERSION,
            found,
        });
    }
    Ok(())
}
