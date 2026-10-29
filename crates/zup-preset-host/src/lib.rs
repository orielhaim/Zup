#![deny(unsafe_code)]

pub mod convert;
pub mod process;
mod stage;
mod state;
pub mod surface;

#[cfg(test)]
mod tests;

pub use process::{PresetProcess, PresetReader, SessionError, launch};
pub use stage::{PresetStage, StageError, confirm_bytes};
pub use state::{ActionRefusal, HostDecision, HostState, Launchable, Selection};
