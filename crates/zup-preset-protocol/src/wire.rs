use serde::{Deserialize, Serialize};

use crate::{
    Action, Capabilities, Configuration, HostHello, PRESET_PROTOCOL_VERSION, PresetHello,
    SessionId, Snapshot, WireError,
};

pub const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope {
    pub version: u32,
    pub session: SessionId,
    pub sequence: u64,
    pub message: Message,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum Message {
    PresetHello(PresetHello),
    HostHello(HostHello),
    Configuration(Box<Configuration>),
    Snapshot(Box<Snapshot>),
    Action(Action),
    Closed,
}

impl Message {
    pub fn sender(&self) -> PeerRole {
        match self {
            Self::PresetHello(_) | Self::Action(_) => PeerRole::Preset,
            Self::HostHello(_) | Self::Configuration(_) | Self::Snapshot(_) => PeerRole::Host,
            Self::Closed => PeerRole::Either,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PeerRole {
    Host,
    Preset,
    Either,
}

impl PeerRole {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Host => "host",
            Self::Preset => "preset",
            Self::Either => "either",
        }
    }
}

pub fn sender_is_allowed(role: PeerRole, message: &Message) -> bool {
    matches!(message.sender(), PeerRole::Either) || message.sender() == role
}

pub fn negotiate(provided: &Capabilities, required: &Capabilities) -> Result<(), WireError> {
    let missing = provided.missing(required);
    if missing.is_empty() {
        return Ok(());
    }
    Err(WireError::MissingCapability(missing.join(", ")))
}

pub fn encode(envelope: &Envelope) -> Result<Vec<u8>, WireError> {
    let bytes =
        serde_json::to_vec(envelope).map_err(|error| WireError::Malformed(error.to_string()))?;
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(WireError::FrameTooLarge {
            max: MAX_FRAME_BYTES,
        });
    }
    Ok(bytes)
}

pub fn decode(bytes: &[u8]) -> Result<Envelope, WireError> {
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(WireError::FrameTooLarge {
            max: MAX_FRAME_BYTES,
        });
    }
    let envelope: Envelope =
        serde_json::from_slice(bytes).map_err(|error| WireError::Malformed(error.to_string()))?;
    if envelope.version != PRESET_PROTOCOL_VERSION {
        return Err(WireError::VersionMismatch {
            expected: PRESET_PROTOCOL_VERSION,
            found: envelope.version,
        });
    }
    Ok(envelope)
}

pub fn message_name(message: &Message) -> &'static str {
    match message {
        Message::PresetHello(_) => "ui_hello",
        Message::HostHello(_) => "host_hello",
        Message::Configuration(_) => "configuration",
        Message::Snapshot(_) => "snapshot",
        Message::Action(_) => "action",
        Message::Closed => "closed",
    }
}

#[derive(Debug, Default, Clone)]
pub struct SequenceTracker {
    last: Option<u64>,
}

impl SequenceTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn accept(&mut self, sequence: u64) -> Result<(), WireError> {
        match self.last {
            None => {
                self.last = Some(sequence);
                Ok(())
            }
            Some(previous) if sequence > previous => {
                self.last = Some(sequence);
                Ok(())
            }
            Some(previous) if sequence == previous => {
                Err(WireError::DuplicateSequence { sequence })
            }
            Some(previous) => Err(WireError::SequenceRegression {
                previous,
                next: sequence,
            }),
        }
    }
}
