#![forbid(unsafe_code)]

mod session;
mod simulate;

pub use session::{
    BootstrapRequest, CancellationHandle, ExecutionPolicy, InstallOutcome, RecoveryStatus,
    RuntimeBackend, RuntimeControl, RuntimeEvent, RuntimeFuture, RuntimePayloadSource,
    RuntimeRequest, RuntimeSession, RuntimeState, SessionError, SessionLog, TokenProbe,
    discover_recovery, run_install, run_install_control, run_install_control_with_policy,
    run_local_install,
};
pub use simulate::{SimulatedJob, SimulatedLifecycle, run_simulated};

pub use uuid::Uuid;
pub use zup_bundle::PayloadSource;
pub use zup_protocol::SessionId;
