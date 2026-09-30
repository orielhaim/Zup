//! The preview runtime: what a zup preset is shown against, and what happens
//! when what it is shown changes.
//!
//! Two things in this repository are a host, and this is the part of them that
//! is neither an installer nor a machine. `zup ui dev` runs it while a preset
//! author edits Rust; `zup preview` runs it while an application author edits
//! `zup.toml`. Below their sources they are the same process doing the same
//! thing, because a preset author who developed against a different one would
//! meet this one instead.
//!
//! What lives here is deliberately everything that is *not* about the world being
//! previewed: the state machine and the child, the session that keeps them in
//! agreement, the controls, the debounced watcher, and where the copies go. A
//! driver supplies a source and a change vocabulary and nothing else.
//!
//! There is no engine behind any of it, and nothing here can install, update,
//! repair, remove, elevate, register or write anything outside its own state
//! directory. That is not a limitation of the simulation - it is what makes a
//! preview safe to leave open while editing an application, and a control causes
//! the event an engine would have caused rather than performing one, so there is
//! no path from a button to a mutation.

#![deny(unsafe_code)]

mod controls;
mod session;
mod simulator;
mod state;
mod watch;

pub use controls::{COMMANDS, Command, Components, Effect, apply};
pub use session::{ControlOutcome, Driver, Event, Runtime, StartError, default_scenario, serve};
pub use simulator::{Candidate, Scenario, Simulator, StageError, Surface};
pub use state::{PROJECT_DIRECTORY, StateDirectory};
pub use watch::{Seen, Watcher};
