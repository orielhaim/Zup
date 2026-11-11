mod controller;
mod host;
mod runner;

pub use controller::run;
pub use host::{
    HostDecision, HostState, OperationContext, PresetLaunch, ScopedInstallation, Selection,
    SessionDiscovery, SessionHost, SessionLaunch, UpdatePreparation, automation_events,
    initial_state, is_terminal, select_scope, state_name,
};
pub use runner::{
    CompletionKind, OperationCompletion, OperationLaunch, SessionOutcome, complete_operation,
    reconcile_operation, run_operation,
};
