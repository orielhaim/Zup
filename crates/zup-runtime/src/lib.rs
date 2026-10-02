//! Runtime orchestration for zup installations.
//!
//! Tokio owns asynchronous session orchestration. The synchronous transaction
//! engine runs under `spawn_blocking`.

#![forbid(unsafe_code)]

mod diagnostics;
mod events;
mod session;
mod simulate;

pub use diagnostics::SessionLog;
pub use events::{RuntimeEvent, RuntimeState};
pub use session::{
    BootstrapRequest, CancellationHandle, ExecutionPolicy, InstallOutcome, RecoveryStatus,
    RuntimeBackend, RuntimeControl, RuntimeFuture, RuntimePayloadSource, RuntimeRequest,
    RuntimeSession, SessionError, TokenProbe, discover_recovery, run_install, run_install_control,
    run_install_control_with_policy, run_local_install,
};
pub use simulate::{SimulatedJob, SimulatedLifecycle, run_simulated};

pub use uuid::Uuid;
pub use zup_bundle::PayloadSource;
pub use zup_protocol::SessionId;
