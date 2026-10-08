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
