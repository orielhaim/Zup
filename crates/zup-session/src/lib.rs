mod controller;
mod host;
mod runner;

pub use controller::run;
pub use host::{
    HostDecision, HostState, OperationContext, PresetLaunch, ScopedInstallation, Selection,
    SessionDiscovery, SessionHost, SessionLaunch, automation_events, initial_state, is_terminal,
    process_outcome, select_scope, state_name,
};
pub use runner::{
    OperationCompletion, OperationLaunch, Reporter, complete_operation, run_operation,
};
