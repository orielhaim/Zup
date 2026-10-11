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

pub const PRESET_PROTOCOL_VERSION: u32 = 1;
