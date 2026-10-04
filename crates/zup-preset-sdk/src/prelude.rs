//! What a preset normally imports.
//!
//! The SDK's own vocabulary, the state a preset reads and the intent it sends,
//! and the GPUI types a preset's own signatures have to name.
//!
//! GPUI itself is one more import away - `use zup_sdk::preset::prelude::gpui::*;` -
//! rather than a glob here, because GPUI ships a `test` attribute macro and a
//! glob would put it in scope beside Rust's own `#[test]` in every test module a
//! preset writes.

pub use crate::{
    ActionSender, ApplicationAssets, AssetRef, NoSettings, Preset, PresetContext, PresetSettings,
    Session, SessionState,
};

/// The GPUI stack, and the names a preset's signatures need.
pub use gpui_kit::{self as gpui, App, AppContext, AsyncApp, Context, Entity, IntoElement, Render, Subscription};

/// What a person chose, and what the installer is offering them.
///
/// This is the whole of a preset's reach into the installer: the fields of a
/// [`Snapshot`], the variants of an [`Action`], and the options those variants
/// carry. The rest of the host's vocabulary is deliberately absent, so a preset
/// that cannot name a plan preview or a diagnostic kind cannot write a window
/// that depends on one - what it draws is what it was given, and the host
/// decides what that is. A preset that presents more than the state reaches
/// [`crate::presentation`].
pub use zup_preset_protocol::{
    Action, ComponentGroupOption, ComponentId, ComponentOption, ComponentProminence,
    InstallScope, InstallerState, MaintenanceState, OperationKind, ProductIdentity,
    SelectionRequirement, Snapshot, Surface, format_bytes,
};

/// What the host said it can do.
///
/// A preset states what it cannot present without, and a host refuses one whose
/// requirements it cannot meet before the preset is launched.
pub use zup_preset_protocol::{Capabilities, Capability};
