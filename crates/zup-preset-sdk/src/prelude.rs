//! What a preset normally imports.
//!
//! The SDK's own vocabulary, plus the GPUI types a preset has to name to
//! implement [`Preset`](crate::Preset) and to hold a session. GPUI itself is
//! one more import away - `use zup_preset_sdk::gpui::*;` - rather than a glob here,
//! because GPUI ships a `test` attribute macro and a glob would put it in scope
//! beside Rust's own `#[test]` in every test module a preset writes.

pub use crate::{
    ApplicationAssets, AssetRef, NoSettings, Preset, PresetAssets, PresetContext, PresetSettings,
    SessionState, Session,
};

/// The GPUI stack.
///
/// Also re-exported at the crate root as `zup_preset_sdk::gpui_kit`; a preset that
/// prefers to spell it out can.
pub use gpui_kit as gpui;

/// The GPUI names a preset's own signatures need.
pub use gpui_kit::{App, AppContext, AsyncApp, Context, Entity, IntoElement, Render, Subscription};

pub use zup_preset_protocol::{
    ChangeGroup, ChangeKind, ComponentGroupOption, ComponentId, ComponentOption,
    ComponentProminence, DiagnosticKind, DiagnosticPresentation, HostHello, InstallOptions,
    InstallScope, InstallationHealth, LaunchTarget, MaintenanceState, OperationKind,
    OperationPhase, PlanPreview, PlanStatus, PlannedChange, ProductIdentity,
    RequirementPresentation, RequirementStatus, ResourceCategory, SelectionRequirement, Action,
    Capabilities, Capability, Configuration, Snapshot, InstallerState, Surface,
    UpdatePresentation, UpdateState, format_bytes,
};
