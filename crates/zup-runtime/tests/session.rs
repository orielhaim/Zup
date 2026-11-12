mod requirements_fixture {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/deployment.rs"
    ));
}
use std::path::PathBuf;
use zup_plan::{DeploymentEffects, compile_deployment};

use rstest::rstest;
use zup_core::{SelectedScope, Target};
use zup_runtime::{
    ExecutionPolicy, InstallOutcome, OperationControl, RuntimeBackend, RuntimeControl,
    RuntimeEvent, RuntimeFuture, RuntimeRequest, SessionError, execute,
};

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
                phase: zup_core::OperationPhase::Files,
                completed: 1,
                total: 1,
                action: "Finished".into(),
            });
            Ok(InstallOutcome::Committed)
        })
    }
}

fn request() -> RuntimeRequest {
    let target = Target::WINDOWS_X64;
    let mut effects = DeploymentEffects::new(
        zup_core::InstallationId::new_v7(),
        requirements_fixture::requirements(
            target,
            zup_core::SelectedScope::User,
            "example.application",
            "1.0.0",
        ),
    );
    effects.install_directory = Some(zup_core::TargetPath::new(target, r"C:\example").unwrap());
    let plan = compile_deployment(&effects).unwrap();
    RuntimeRequest {
        operation: zup_runtime::RuntimeOperation::Deployment(Box::new(
            zup_runtime::RuntimeDeployment {
                plan: plan.clone(),
                intent: zup_plan::DeploymentIntent {
                    deployment: zup_plan::DeploymentSelection::from_plan(&plan),
                    action: zup_plan::LifecycleAction::Install,
                    enable: Vec::new(),
                    disable: Vec::new(),
                    install_directory: None,
                },
                bootstrap: None,
                release: None,
            },
        )),
        target: Target::parse("windows-x64").unwrap(),
        app_id: zup_core::AppId::new("com.example.runtime").unwrap(),
        app_version: "1.0.0".parse().unwrap(),
        scope: SelectedScope::User,
        state_root: PathBuf::from("state"),
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

struct SettledBackend(InstallOutcome);

impl RuntimeBackend for SettledBackend {
    fn execute<'a>(
        &'a self,
        _request: RuntimeRequest,
        _control: RuntimeControl,
    ) -> RuntimeFuture<'a, Result<InstallOutcome, SessionError>> {
        Box::pin(async { Ok(self.0.clone()) })
    }
}

#[rstest]
#[case::committed(InstallOutcome::Committed, zup_runtime::RecoverySettlement::Committed)]
#[case::compensated(
    InstallOutcome::RolledBack,
    zup_runtime::RecoverySettlement::RolledBack
)]
#[tokio::test]
async fn recovery_reports_successful_settlement_without_changing_deployment_failures(
    #[case] settlement: InstallOutcome,
    #[case] expected: zup_runtime::RecoverySettlement,
) {
    let backend = SettledBackend(settlement.clone());
    let control = OperationControl::new(ExecutionPolicy::NonInteractive);
    assert_eq!(
        execute(&backend, request(), &control).await.unwrap(),
        settlement
    );
    let mut recovery = request();
    recovery.operation = zup_runtime::RuntimeOperation::Recovery {
        transaction: zup_core::TransactionId::new_v7(),
        request: Default::default(),
    };
    assert_eq!(
        execute(&backend, recovery, &control).await.unwrap(),
        InstallOutcome::Recovered {
            settlement: expected
        },
    );
}
