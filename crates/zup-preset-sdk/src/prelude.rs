//! What a preset normally imports.
//!
//! The SDK's own vocabulary, the state and actions a preset reads and writes,
//! and the GPUI types a preset has to name to implement
//! [`Preset`](crate::Preset) and to hold a session.
//!
//! GPUI itself is one more import away - `use zup_preset_sdk::gpui::*;` - rather
//! than a glob here, because GPUI ships a `test` attribute macro and a glob would
//! put it in scope beside Rust's own `#[test]` in every test module a preset
//! writes.

pub use crate::{
    ActionSender, ApplicationAssets, AssetRef, NoSettings, Preset, PresetAssets, PresetContext,
    PresetSettings, Session, SessionState,
};

/// The GPUI stack.
///
/// Also re-exported at the crate root as `zup_preset_sdk::gpui_kit`; a preset that
/// prefers to spell it out can.
pub use gpui_kit as gpui;

/// The GPUI names a preset's own signatures need.
pub use gpui_kit::{App, AppContext, AsyncApp, Context, Entity, IntoElement, Render, Subscription};

/// What the host published, and what a preset asks for.
///
/// The state a preset renders and the intent it sends. These are the two halves
/// of a preset's whole reach into the installer, and they are what a view names
/// in its own signatures - the fields of a [`Snapshot`](Snapshot), the variants
/// of an [`Action`](Action).
///
/// The rest of the host's vocabulary is deliberately absent. A preset that
/// cannot name a plan preview, a diagnostic kind or a change group cannot write
/// UI that depends on them, which is the point: what it draws is what it was
/// given, and the host decides what that is.
pub use zup_preset_protocol::{
    Action, ComponentGroupOption, ComponentId, ComponentOption, ComponentProminence,
    InstallerState, MaintenanceState, OperationKind, PlanStatus, ProductIdentity,
    SelectionRequirement, Snapshot, Surface,
};

/// What the host told the session it can do.
///
/// A preset states what it cannot present without, and a host refuses one whose
/// requirements it cannot meet before the preset is launched.
pub use zup_preset_protocol::{Capabilities, Capability};

/// Who the application is installing for.
pub use zup_preset_protocol::InstallScope;

/// How much room something takes on disk, formatted for a person.
pub use zup_preset_protocol::format_bytes;

/// The host's own vocabulary, for a preset that presents more than the state.
///
/// Not in the prelude, because most of it describes what the host is *doing*
/// rather than what a person chose, and a preset that renders a diagnostic or a
/// plan preview is a preset that has decided its window is a debug view. Reached
/// deliberately:
///
/// ```ignore
/// use zup_preset_sdk::host::{DiagnosticKind, PlanPreview, ResourceCategory};
/// ```
///
/// Available to any preset, and named here rather than left implicit, so that
/// reaching for it is a decision with a name attached.
pub use zup_preset_protocol as host;
