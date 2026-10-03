//! Versioned frames and the rules a frame has to satisfy.

use serde::{Deserialize, Serialize};

use crate::{
    HostHello, PRESET_PROTOCOL_VERSION, Action, Capabilities, Configuration, PresetHello,
    SessionId, Snapshot, WireError,
};

/// The largest frame a peer will accept, in bytes.
///
/// A length prefix is attacker-controlled, so the bound is checked before the
/// body is read rather than after. Application assets never travel this way: a
/// logo is materialized as a file the preset reads, not as bytes in a snapshot.
pub const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;

/// Everything that crosses the connection, in one versioned frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope {
    pub version: u32,
    pub session: SessionId,
    /// Strictly increasing per sender.
    pub sequence: u64,
    pub message: Message,
}

/// The closed vocabulary of UI messages.
///
/// There is no generic "call this method with these arguments" variant. Every
/// message is something both peers can name, so a peer that receives one it
/// cannot follow refuses the session rather than improvising.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum Message {
    /// Preset → host, first.
    PresetHello(PresetHello),
    /// Host → preset, accepting or refusing the session.
    HostHello(HostHello),
    /// Host → preset, the application's settings and materialized assets.
    ///
    /// Sent with the first snapshot, and again whenever a settings or asset
    /// change reaches the host. A preset re-reads it rather than guessing what
    /// the application changed.
    Configuration(Box<Configuration>),
    /// Host → preset, the complete presentation state.
    Snapshot(Box<Snapshot>),
    /// Preset → host, one installer operation.
    Action(Action),
    /// Either direction, ending the session.
    Closed,
}

impl Message {
    /// Which side is allowed to send this.
    ///
    /// One rule, checked by one function, rather than a second copy of the
    /// message list to keep in step with this one. A session that receives a
    /// message from the wrong side has a peer that does not follow the
    /// protocol, and refusing it is the only safe answer.
    pub fn sender(&self) -> PeerRole {
        match self {
            Self::PresetHello(_) | Self::Action(_) => PeerRole::Preset,
            Self::HostHello(_) | Self::Configuration(_) | Self::Snapshot(_) => PeerRole::Host,
            Self::Closed => PeerRole::Either,
        }
    }
}

/// Which side of a session a peer is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PeerRole {
    Host,
    Preset,
    /// A message both sides may send.
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

/// Whether `role` may send `message`.
pub fn sender_is_allowed(role: PeerRole, message: &Message) -> bool {
    matches!(message.sender(), PeerRole::Either) || message.sender() == role
}

/// Whether a preset's requirements are met by what a host provides.
pub fn negotiate(provided: &Capabilities, required: &Capabilities) -> Result<(), WireError> {
    let missing = provided.missing(required);
    if missing.is_empty() {
        return Ok(());
    }
    Err(WireError::MissingCapability(missing.join(", ")))
}

/// Serialize a frame, refusing one larger than [`MAX_FRAME_BYTES`].
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

/// Deserialize a frame, refusing a version this build does not speak.
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

/// The wire name of a message, for a refusal that could not name a variant.
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

/// Per-sender sequence checking.
///
/// A retransmitted or reordered frame means the transport is not the one this
/// protocol assumes, and the session is refused rather than continued: acting on
/// a stale action after a newer snapshot is how an uninstall confirmation ends
/// up applying to the wrong state.
#[derive(Debug, Default, Clone)]
pub struct SequenceTracker {
    last: Option<u64>,
}

impl SequenceTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Accept `sequence` if it is strictly greater than the last accepted value.
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
