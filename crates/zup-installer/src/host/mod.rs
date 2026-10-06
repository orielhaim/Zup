//! The installer host, and the preset process it drives.
//!
//! The host owns every fact about the installation: what is installed, what an
//! action means, whether the machine will allow it, and what the next
//! `Snapshot` says. The preset owns none of that. It is a GPUI application
//! that draws what it is told and asks for what it wants, over a pipe, in
//! another process, and the only thing that can change the state is a validated
//! action arriving here.
//!
//! That is a stronger statement than "the UI is separate". It means a preset
//! that crashes loses a window and nothing else, that the engine keeps owning a
//! transaction after the window closes, and that the same protocol, the same
//! state machine, and the same test suite serve this host and the development
//! simulator.
//!
//! What is left here is the part that is about *this* machine: the embedded
//! package, the installed content store, the engine, and the elevation it needs.
//! The state machine and the session are in `zup-preset-host`, so that `zup preset dev`
//! runs the same ones.

pub mod preset;
mod session;

pub use preset::{Composed, PresetError, Source, materialize};
pub use session::{
    Launch, Opening, Placement, PresetSource, opening, surface, surface_from_arguments,
    uninstall_confirmation,
};

// The host half of the preset protocol, re-exported so a caller that already names
// this module does not have to learn a second one. `zup preset dev` names
// `zup_preset_host` directly; this is a convenience for the installer's own callers,
// not a second path.
pub use zup_preset_host::{
    ActionRefusal, HostDecision, HostState, Selection, capabilities, component, components,
    diagnostic, engine_scope, install_options, maintenance_state, plan, product, scope, scopes,
};
