use crate::{
    Capabilities, Configuration, Envelope, HostHello, Message, PRESET_PROTOCOL_VERSION, PeerRole,
    PresetHello, ProductIdentity, SessionId, Snapshot, WireError, negotiate, sender_is_allowed,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Handshake {
    Opening,
    Awaiting { role: PeerRole },
    Completed,
    Live { snapshot: Box<Snapshot> },
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionProgress {
    Send(Envelope),
    Act(crate::Action),
    Done,
}

pub struct Session {
    id: SessionId,
    role: PeerRole,
    state: Handshake,
    inbound: crate::SequenceTracker,
    outbound: u64,
    declared: Capabilities,
    identity: Identity,
    configuration: Configuration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Identity {
    Host(Box<ProductIdentity>),
    Preset { name: String, version: String },
}

impl Session {
    pub fn host(id: SessionId, capabilities: Capabilities, product: ProductIdentity) -> Self {
        Self::new(
            id,
            PeerRole::Host,
            capabilities,
            Identity::Host(Box::new(product)),
        )
    }

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

    pub fn identity(&self) -> &Identity {
        &self.identity
    }

    pub fn is_closed(&self) -> bool {
        matches!(self.state, Handshake::Closed)
    }

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
