//! The versioned contract between a zup installer host and a UI preset.
//!
//! A preset is presentation code. It never sees a transaction, a plan, an
//! elevation decision, or a runtime event: it receives [`UiSnapshot`] values
//! describing what the installer is doing and sends [`UiAction`] values
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
//! [`UI_PROTOCOL_VERSION`] is the wire version and is separate from this crate's
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

pub use action::UiAction;
pub use compatibility::{HostOffers, Incompatible};
pub use configuration::{
    ConfigurationError, MAX_ASSET_PATH_BYTES, MAX_ASSETS, MAX_SETTINGS_BYTES, UiConfiguration,
};
pub use description::{
    DESCRIBE_FLAG, DescribeError, MAX_DESCRIBE_BYTES, MAX_TARGETS, PresetDescription,
};
pub use error::UiWireError;
pub use identifiers::{ComponentId, ComponentIdError, InstallScope};
pub use presentation::{
    ChangeGroup, ChangeKind, DiagnosticKind, DiagnosticPresentation, InstallationHealth,
    OperationPhase, PlanPreview, PlannedChange, ProgressPresentation, RequirementPresentation,
    RequirementStatus, ResourceCategory, UpdatePresentation, UpdateState, format_bytes,
};
pub use session::{
    HostHello, UiCapabilities, UiCapability, UiHello, UiSessionId, UnknownCapability,
};
pub use session_machine::{Identity, Session, SessionProgress, SessionState};
pub use snapshot::{
    ComponentOption, InstallOptions, MaintenanceState, ProductIdentity, UiSnapshot, UiState,
    UiSurface,
};
pub use wire::{
    MAX_FRAME_BYTES, SequenceTracker, UiEnvelope, UiMessage, UiPeerRole, decode, encode,
    message_name, negotiate, sender_is_allowed,
};

/// The UI wire protocol version.
///
/// Its own counter, unrelated to this crate's version and to the engine's
/// runtime protocol. A message whose shape or semantics a peer cannot follow is
/// refused rather than guessed at.
pub const UI_PROTOCOL_VERSION: u32 = 1;
