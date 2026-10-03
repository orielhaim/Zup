//! The host side of the UI protocol, with no engine behind it.
//!
//! Two things in this repository are called a host and they are the same thing:
//! the process that owns a machine's presentation state and speaks
//! `zup-preset-protocol` to a preset. One runs inside an installer, driving a real
//! engine. The other runs inside `zup preset dev`, driving a simulation of one. They
//! share this crate so that a preset developed against the simulator meets the
//! same state machine, the same refusals, and the same progress a preset installed
//! from a release would.
//!
//! What a host owns is a [`HostState`], and what it is given is engine events.
//! Nothing here reaches an engine, a filesystem, or a platform: a host that had
//! to know how to install something in order to answer `Action::Install` would
//! be a host whose answers could not be established without installing something.

#![deny(unsafe_code)]

pub mod convert;
pub mod process;
mod state;
pub mod surface;

#[cfg(test)]
mod tests;

pub use convert::{component, components, diagnostic, engine_scope, phase, plan, scope};
pub use process::{PresetProcess, PresetReader, SessionError, launch};
pub use state::{ActionRefusal, HostDecision, HostState, Launchable, Selection};
pub use surface::{
    capabilities, components as surface_components, default_scope, install_options, launchers,
    maintenance_state, persisted_location, product, scopes,
};
