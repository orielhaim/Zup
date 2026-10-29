#![forbid(unsafe_code)]

mod operation;
mod session;
mod simulate;
mod telemetry;

pub use operation::{
    ContentAccess, Operation, OperationError, OperationHost, OperationInput, OperationSelection,
    PreparedOperation, RequestOptions, UpdateRequest, attach_maintenance_copy,
    choose_install_directory, install_directory_template, maintenance_operations,
    persisted_install_directory, plan_request, prepare_operation, resolve_operation,
    runtime_request,
};

pub use session::{
    BootstrapRequest, CancellationHandle, ExecutionPolicy, InstallOutcome, OperationControl,
    OperationProgress, RecoveryStatus, RuntimeBackend, RuntimeControl, RuntimeEvent, RuntimeFuture,
    RuntimeRequest, RuntimeState, SessionError, SessionLog, TokenProbe, discover_recovery, execute,
    outcome_of,
};
pub use simulate::{SimulatedJob, SimulatedLifecycle, run_simulated};
pub use telemetry::{EventingExecutor, operation_action, operation_work};

pub use uuid::Uuid;
pub use zup_protocol::SessionId;
