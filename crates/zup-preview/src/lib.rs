//! The preview runtime: what a zup preset is shown against, and what happens
//! when what it is shown changes.
//!
//! Two things in this repository are a host, and this is the part of them that
//! is neither an installer nor a machine. `zup preset dev` runs it while a preset
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
//! The engine behind an operation is [`zup_runtime::run_simulated`]: the same
//! stages a real lifecycle walks, on a clock, reporting the same events. It
//! installs nothing. A control still cannot mutate the machine, because the
//! engine's only product is those events.

#![deny(unsafe_code)]

pub mod catalog;
mod controls;
mod machine;
mod session;
mod simulator;
mod state;
mod watch;

pub use controls::{COMMANDS, Command, Components, Effect, apply};
pub use machine::{Footprint, Machine, Scenario, Surface};
pub use session::{ControlOutcome, Driver, Event, Runtime, StartError, default_scenario, serve};
pub use simulator::{Candidate, Simulator, StageError};
pub use state::{PROJECT_DIRECTORY, StateDirectory};
pub use watch::{Seen, Watcher};
