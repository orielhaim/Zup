//! What a preset normally imports.
//!
//! The SDK's own vocabulary, plus the GPUI types a preset has to name to
//! implement [`Preset`](crate::Preset) and to hold a session. GPUI itself is
//! one more import away - `use zup_ui_sdk::gpui::*;` - rather than a glob here,
//! because GPUI ships a `test` attribute macro and a glob would put it in scope
//! beside Rust's own `#[test]` in every test module a preset writes.

pub use crate::{
    ApplicationAssets, AssetRef, NoSettings, Preset, PresetAssets, PresetContext, PresetSettings,
    SessionState, UiSession,
};

/// The GPUI stack.
///
/// Also re-exported at the crate root as `zup_ui_sdk::gpui_kit`; a preset that
/// prefers to spell it out can.
pub use gpui_kit as gpui;

/// The GPUI names a preset's own signatures need.
pub use gpui_kit::{App, AppContext, AsyncApp, Context, Entity, IntoElement, Render, Subscription};

pub use zup_ui_protocol::{
    ChangeGroup, ChangeKind, ComponentId, ComponentOption, DiagnosticKind, DiagnosticPresentation,
    HostHello, InstallOptions, InstallScope, InstallationHealth, MaintenanceState, OperationPhase,
    PlanPreview, PlannedChange, ProductIdentity, RequirementPresentation, RequirementStatus,
    ResourceCategory, UiAction, UiCapabilities, UiCapability, UiConfiguration, UiSnapshot, UiState,
    UiSurface, UpdatePresentation, UpdateState, format_bytes,
};
