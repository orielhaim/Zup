use std::path::PathBuf;

use rstest::rstest;
use zup_core::{SelectedScope, Target};
use zup_runtime::{
    ExecutionPolicy, InstallOutcome, OperationControl, RuntimeBackend, RuntimeControl,
    RuntimeEvent, RuntimeFuture, RuntimeRequest, SessionError, execute,
};
use zup_transaction::{TransactionInput, compile_transaction};

struct FakeBackend;

impl RuntimeBackend for FakeBackend {
    fn execute<'a>(
        &'a self,
        _request: RuntimeRequest,
        control: RuntimeControl,
    ) -> RuntimeFuture<'a, Result<InstallOutcome, SessionError>> {
        let cancellation = control.cancellation;
        let events = control.events;
        let policy = control.policy;
        Box::pin(async move {
            let _ = events.send(RuntimeEvent::StateChanged {
                state: zup_runtime::RuntimeState::Preparing,
            });
            if cancellation.is_cancelled() {
                return Ok(InstallOutcome::Cancelled);
            }
            if policy == ExecutionPolicy::NonInteractive {
                return Err(SessionError::AuthorizationRequired);
            }
            let _ = events.send(RuntimeEvent::Progress {
                completed: 1,
                total: 1,
                action: "Finished".into(),
            });
            Ok(InstallOutcome::Committed)
        })
    }
}

fn request() -> RuntimeRequest {
    RuntimeRequest {
        target: Target::parse("windows-x64").unwrap(),
        app_id: zup_core::AppId::new("com.example.runtime").unwrap(),
        app_version: "1.0.0".parse().unwrap(),
        scope: SelectedScope::User,
        transaction_plan: compile_transaction(&TransactionInput::new(
            Target::parse("windows-x64").unwrap(),
        ))
        .unwrap(),
        state_root: PathBuf::from("state"),
        work_root: PathBuf::from("work"),
        recovery_id: None,
        bootstrap: None,
        release: None,
    }
}

#[derive(Clone, Copy)]
enum Expectation {
    Committed,
    Cancelled,
    AuthorizationRequired,
    PlanInvalid,
}

#[rstest]
#[case::committed(ExecutionPolicy::Interactive, false, false, Expectation::Committed)]
#[case::cancelled_before_the_backend(
    ExecutionPolicy::Interactive,
    true,
    false,
    Expectation::Cancelled
)]
#[case::noninteractive_authorization(
    ExecutionPolicy::NonInteractive,
    false,
    false,
    Expectation::AuthorizationRequired
)]
#[case::target_mismatch(ExecutionPolicy::Interactive, false, true, Expectation::PlanInvalid)]
#[tokio::test]
async fn the_operation_outcome_is_independent_of_progress_delivery(
    #[case] policy: ExecutionPolicy,
    #[case] cancel: bool,
    #[case] mismatch: bool,
    #[case] expected: Expectation,
) {
    let backend = FakeBackend;
    let mut request = request();
    if mismatch {
        request.target = Target::parse("arm64-pc-windows-msvc").unwrap();
    }
    let control = if cancel {
        let cancellation = zup_runtime::CancellationHandle::new();
        cancellation.cancel();
        OperationControl::with_cancellation(cancellation, policy)
    } else {
        OperationControl::new(policy)
    };
    let _ignored = control.subscribe();
    let result = execute(&backend, request, &control).await;
    match expected {
        Expectation::Committed => {
            assert_eq!(result.unwrap(), InstallOutcome::Committed);
            assert_eq!(control.progress().completed, 1);
            assert!(control.progress().state.is_some());
        }
        Expectation::Cancelled => assert!(matches!(result, Ok(InstallOutcome::Cancelled))),
        Expectation::AuthorizationRequired => {
            assert!(matches!(result, Err(SessionError::AuthorizationRequired)))
        }
        Expectation::PlanInvalid => assert!(matches!(result, Err(SessionError::PlanInvalid(_)))),
    }
    assert!(!ExecutionPolicy::NonInteractive.allows_authorization());
    assert!(ExecutionPolicy::Interactive.allows_authorization());
}
