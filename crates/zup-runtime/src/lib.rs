#![forbid(unsafe_code)]

mod operation;
mod session;
mod simulate;
mod telemetry;

pub use operation::{
    ContentAccess, Operation, OperationError, OperationHost, OperationInput, OperationSelection,
    PreparedOperation, RequestOptions, attach_maintenance_copy, choose_install_directory,
    install_directory_template, persisted_install_directory, prepare_operation, resolve_operation,
    resolve_scope, runtime_request,
};

pub use session::{
    BootstrapRequest, CancellationHandle, ExecutionPolicy, InstallOutcome, RecoveryStatus,
    RuntimeBackend, RuntimeControl, RuntimeEvent, RuntimeFuture, RuntimePayloadSource,
    RuntimeRequest, RuntimeSession, RuntimeState, SessionError, SessionLog, TokenProbe,
    discover_recovery, outcome_of, run_install, run_install_control,
    run_install_control_with_policy, run_local_install,
};
pub use simulate::{SimulatedJob, SimulatedLifecycle, run_simulated};
pub use telemetry::{EventingExecutor, operation_action, operation_work};

pub use uuid::Uuid;
pub use zup_bundle::PayloadSource;
pub use zup_protocol::SessionId;
