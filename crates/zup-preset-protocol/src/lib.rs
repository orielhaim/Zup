//! The versioned contract between a zup installer host and a UI preset.
//!
//! A preset is presentation code. It never sees a transaction, a plan, an
//! elevation decision, or a runtime event: it receives [`Snapshot`] values
//! describing what the installer is doing and sends [`Action`] values
//! describing what a person asked for. The host owns the installation state,
//! validates every action against it, and answers with the next snapshot.
//!
//! This crate is the whole of that contract. It is pure data: no GPUI, no
//! Windows APIs, no Tokio, no installer engine, and no dependency on any other
//! Zup crate, so a preset project outside this repository can depend on it
//! alone. Where the engine has a type that means the same thing - a component
//! id, an install scope - the wire representation lives here, duplicated on
//! purpose. A refactor inside the engine must not become a breaking change for
//! every published preset.
//!
//! [`PRESET_PROTOCOL_VERSION`] is the wire version and is separate from this crate's
//! own version: a documentation fix or a new helper does not change it, and
//! changing the engine does not change it either.

#![forbid(unsafe_code)]

mod action;
mod compatibility;
mod configuration;
mod description;
mod error;
mod identifiers;
mod presentation;
mod session;
mod session_machine;
mod snapshot;
mod wire;

pub use action::Action;
pub use compatibility::{HostOffers, Incompatible};
pub use configuration::{
    Configuration, ConfigurationError, MAX_ASSET_PATH_BYTES, MAX_ASSETS, MAX_SETTINGS_BYTES,
};
pub use description::{
    DESCRIBE_FLAG, DescribeError, MAX_DESCRIBE_BYTES, MAX_TARGETS, PresetDescription,
};
pub use error::WireError;
pub use identifiers::{ComponentId, ComponentIdError, InstallScope};
pub use presentation::{
    ChangeGroup, ChangeKind, DiagnosticKind, DiagnosticPresentation, InstallationHealth,
    OperationPhase, PlanPreview, PlanStatus, PlannedChange, ProgressPresentation,
    RequirementPresentation, RequirementStatus, ResourceCategory, UpdatePresentation, UpdateState,
    format_bytes,
};
pub use session::{Capabilities, Capability, HostHello, PresetHello, SessionId, UnknownCapability};
pub use session_machine::{Handshake, Identity, Session, SessionProgress};
pub use snapshot::{
    ComponentGroupOption, ComponentOption, ComponentProminence, InstallOptions, InstallerState,
    LaunchTarget, MaintenanceState, OperationKind, ProductIdentity, SelectionRequirement, Snapshot,
    Surface,
};
pub use wire::{
    Envelope, MAX_FRAME_BYTES, Message, PeerRole, SequenceTracker, decode, encode, message_name,
    negotiate, sender_is_allowed,
};

/// The UI wire protocol version.
///
/// Its own counter, unrelated to this crate's version and to the engine's
/// runtime protocol. A message whose shape or semantics a peer cannot follow is
/// refused rather than guessed at.
pub const PRESET_PROTOCOL_VERSION: u32 = 1;
