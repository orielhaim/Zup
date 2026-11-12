#![forbid(unsafe_code)]

mod operation;
mod session;
mod simulate;
mod telemetry;

pub use operation::{
    ContentAccess, Operation, OperationBody, OperationError, OperationHost, OperationInput,
    OperationSelection, PreparedOperation, RequestOptions, UpdateRequest, attach_maintenance_copy,
    attach_preset_content, choose_install_directory, install_directory_template,
    maintenance_operations, persisted_install_directory, plan_request, prepare_operation,
    resolve_operation, runtime_request,
};

pub use session::{
    BootstrapRequest, CancellationHandle, CompletionOutcome, ExecutionPolicy, FailureKind,
    InstallOutcome, OperationControl, OperationProgress, RecoverySettlement, RuntimeBackend,
    RuntimeControl, RuntimeDeployment, RuntimeEvent, RuntimeFuture, RuntimeOperation,
    RuntimeRequest, RuntimeState, SessionError, SessionLog, TokenProbe, execute, outcome_of,
};
pub use simulate::{SimulatedJob, SimulatedLifecycle, run_simulated};
pub use telemetry::{EventingExecutor, operation_action, operation_phase, operation_result_action};

pub use zup_transaction::RecoveryAssessment;

#[cfg(test)]
mod requirements_fixture {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/deployment.rs"
    ));
}
