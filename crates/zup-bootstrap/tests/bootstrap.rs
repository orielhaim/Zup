use std::sync::{Arc, Mutex};

use semver::Version;
use tempfile::TempDir;
use zup_bootstrap::{
    BootstrapError, BootstrapId, BootstrapOperation, BootstrapOperationState, BootstrapOutcome,
    BootstrapState, BootstrapStateStore, DetectionResult, FilesystemBootstrapStateStore,
    PrerequisiteProvider, PrerequisiteSatisfier, ProviderOutcome, ProviderRequest, Quarantine,
    QuarantineError, execute_operation, recover,
};
use zup_core::{PrerequisiteId, RelativePath, TargetTriple};

mod common;
use common::{digest, embedded, plan};
use rstest::rstest;

struct FakeSatisfier {
    satisfied: Arc<Mutex<bool>>,
}

impl PrerequisiteSatisfier for FakeSatisfier {
    fn satisfy(&self, _operation: &BootstrapOperation) -> Result<DetectionResult, BootstrapError> {
        if *self.satisfied.lock().unwrap() {
            Ok(DetectionResult::Satisfied {
                version: Some(Version::new(14, 0, 0)),
                evidence: "fixture".into(),
            })
        } else {
            Ok(DetectionResult::Missing)
        }
    }
}

struct FakeProvider {
    satisfied: Arc<Mutex<bool>>,
    outcome: ProviderOutcome,
    satisfy_on_success: bool,
}

impl PrerequisiteProvider for FakeProvider {
    fn execute(&self, _request: &ProviderRequest) -> Result<ProviderOutcome, BootstrapError> {
        if self.satisfy_on_success && matches!(self.outcome, ProviderOutcome::Succeeded) {
            *self.satisfied.lock().unwrap() = true;
        }
        Ok(self.outcome)
    }
}

/// The three ways an operation ends, decided by what the provider reported and whether
/// the requirement is actually satisfied afterwards. The last one is the rule that
/// matters: a provider that exits 0 without satisfying anything must not be able to
/// report success, because a bootstrap that believes it is ready installs against a
/// runtime that is not there.
#[rstest]
#[case::already_satisfied(true, ProviderOutcome::Succeeded, Expected::Ready)]
#[case::provider_succeeded_but_nothing_satisfies(
    false,
    ProviderOutcome::Succeeded,
    Expected::FailsClosed
)]
#[case::reboot_requested(false, ProviderOutcome::RebootRequired { exit_code: 3010 }, Expected::Reboot)]
fn an_operation_ends_in_exactly_one_of_three_states(
    #[case] already_satisfied: bool,
    #[case] outcome: ProviderOutcome,
    #[case] expected: Expected,
) {
    let plan = plan(embedded(b"runtime"));
    let satisfied = Arc::new(Mutex::new(already_satisfied));
    let satisfier = FakeSatisfier {
        satisfied: satisfied.clone(),
    };
    let provider = FakeProvider {
        satisfied,
        outcome,
        satisfy_on_success: already_satisfied,
    };
    let mut state = BootstrapState::new(&plan);
    let operation = &plan.operations[0];
    let root = TempDir::new().unwrap();
    let executable = root.path().join("runtime.exe");
    std::fs::write(&executable, b"runtime").unwrap();

    let result = execute_operation(
        &plan, operation, &satisfier, &provider, executable, &mut state,
    );
    match expected {
        Expected::Ready => {
            assert_eq!(result.unwrap(), BootstrapOutcome::Ready);
            assert!(matches!(
                state.operation_mut(&operation.id),
                Some(BootstrapOperationState::Satisfied { .. })
            ));
        }
        Expected::FailsClosed => assert!(
            matches!(result, Err(BootstrapError::RequirementStillUnsatisfied)),
            "{result:?}"
        ),
        Expected::Reboot => {
            assert_eq!(
                result.unwrap(),
                BootstrapOutcome::RebootRequired {
                    exit_code: 3010,
                    prerequisite_id: PrerequisiteId::new("runtime").unwrap(),
                }
            );
            assert!(
                state.reboot_required,
                "the exit code has to outlive the process that produced it"
            );
        }
    }
}

/// How an operation is allowed to end. Named so the three cases above read as a table
/// and so a new `BootstrapOutcome` cannot be added without someone deciding which of
/// these it belongs to.
#[derive(Debug, Clone, Copy)]
enum Expected {
    Ready,
    FailsClosed,
    Reboot,
}

#[test]
fn assessment_does_not_trust_stale_satisfied_state() {
    let plan = plan(embedded(b"runtime"));
    let satisfied = Arc::new(Mutex::new(true));
    let satisfier = FakeSatisfier {
        satisfied: satisfied.clone(),
    };
    let mut state = BootstrapState::new(&plan);
    zup_bootstrap::assess(&plan, &satisfier, &mut state).unwrap();
    assert!(state.remaining.is_empty());
    *satisfied.lock().unwrap() = false;
    let remaining = zup_bootstrap::assess(&plan, &satisfier, &mut state).unwrap();
    assert_eq!(remaining, vec![PrerequisiteId::new("runtime").unwrap()]);
}

#[test]
fn bound_plan_rejects_wrong_artifact_paths_and_identities() {
    let plan = plan(embedded(b"runtime"));
    let id = BootstrapId::for_plan(&plan);
    let mut artifacts = std::collections::BTreeMap::new();
    artifacts.insert(
        PrerequisiteId::new("runtime").unwrap(),
        zup_bootstrap::QuarantinedArtifact {
            relative_path: RelativePath::new("other/runtime.exe").unwrap(),
            size: 7,
            sha256: digest(b"runtime"),
        },
    );
    assert!(matches!(
        zup_bootstrap::BoundBootstrapPlan::with_id(id, plan.clone(), artifacts),
        Err(BootstrapError::ArtifactMismatch(_))
    ));
    assert!(matches!(
        zup_bootstrap::BoundBootstrapPlan::with_id(BootstrapId::new(), plan, Default::default(),),
        Err(BootstrapError::InvalidState(_))
    ));
}

#[test]
fn failed_bootstrap_is_not_silently_retried() {
    let plan = plan(embedded(b"runtime"));
    let satisfied = Arc::new(Mutex::new(false));
    let satisfier = FakeSatisfier { satisfied };
    let mut state = BootstrapState::new(&plan);
    state
        .mark(
            &PrerequisiteId::new("runtime").unwrap(),
            BootstrapOperationState::Failed {
                code: "ambiguous_external_process".into(),
                message: "unknown".into(),
            },
        )
        .unwrap();
    state.recompute();
    assert!(matches!(
        zup_bootstrap::assess(&plan, &satisfier, &mut state),
        Err(BootstrapError::RecoveryRequired(_))
    ));
}

#[test]
fn quarantine_rejects_digest_mismatch_and_truncation() {
    let root = TempDir::new().unwrap();
    let quarantine = Quarantine::new(root.path()).unwrap();
    let id = PrerequisiteId::new("runtime").unwrap();
    let reservation = quarantine.reserve(&id, "runtime.exe", Some(8)).unwrap();
    let error = quarantine
        .stage_bytes(&reservation, b"short", digest(b"runtime"))
        .unwrap_err();
    assert!(matches!(error, QuarantineError::SizeMismatch { .. }));
    assert!(!reservation.final_path.exists());
    let reservation = quarantine.reserve(&id, "runtime.exe", Some(7)).unwrap();
    let error = quarantine
        .stage_bytes(&reservation, b"runtime", digest(b"different"))
        .unwrap_err();
    assert!(matches!(error, QuarantineError::DigestMismatch));
    assert!(!reservation.final_path.exists());
    // A refused artifact leaves no staged bytes behind for a later run to find.
    assert!(!reservation.partial_path.exists());
}

#[test]
fn state_store_checks_integrity_and_resume_identity() {
    let plan = plan(embedded(b"runtime"));
    let state = BootstrapState::new(&plan);
    let root = TempDir::new().unwrap();
    let store = FilesystemBootstrapStateStore::new(root.path());
    store.create(&state).unwrap();
    let loaded = store.load(state.id).unwrap();
    assert_eq!(loaded, state);
    let mut updated = loaded.clone();
    updated.revision = 1;
    store.compare_and_swap(0, &updated).unwrap();
    assert!(matches!(
        store.compare_and_swap(0, &updated),
        Err(zup_bootstrap::BootstrapStoreError::RevisionConflict)
    ));
    let path = root
        .path()
        .join("bootstrap")
        .join(state.id.as_uuid().to_string())
        .join("state.json");
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[0] ^= 1;
    std::fs::write(path, bytes).unwrap();
    assert!(store.load(state.id).is_err());
}

/// The target is part of a bootstrap's identity, so state written for one machine is
/// not resumable on another - neither through the recovery entry point nor through the
/// store's own resume query.
#[test]
fn state_from_another_target_is_neither_resumed_nor_recovered() {
    let first = plan(embedded(b"runtime"));
    let mut second = first.clone();
    second.key.target = TargetTriple::parse("arm64-pc-windows-msvc").unwrap();
    assert_ne!(first.fingerprint(), second.fingerprint());
    assert_ne!(
        BootstrapId::for_plan(&first),
        BootstrapId::for_plan(&second)
    );

    let mut state = BootstrapState::new(&first);
    let satisfied = Arc::new(Mutex::new(true));
    let error = recover(&second, &FakeSatisfier { satisfied }, &mut state).unwrap_err();
    assert!(matches!(error, BootstrapError::TargetMismatch));

    let root = TempDir::new().unwrap();
    let store = FilesystemBootstrapStateStore::new(root.path());
    store.create(&state).unwrap();
    assert!(
        store
            .find_resumable(&first.key.app_id, first.key.scope, &second.key.target)
            .unwrap()
            .is_empty(),
        "a store must not offer another machine's bootstrap as resumable"
    );
    assert_eq!(
        store
            .find_resumable(&first.key.app_id, first.key.scope, &first.key.target)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn crash_recovery_re_detects_running_operation() {
    let plan = plan(embedded(b"runtime"));
    let satisfied = Arc::new(Mutex::new(false));
    let satisfier = FakeSatisfier { satisfied };
    let mut state = BootstrapState::new(&plan);
    state
        .mark(&plan.operations[0].id, BootstrapOperationState::Running)
        .unwrap();
    assert_eq!(
        recover(&plan, &satisfier, &mut state).unwrap(),
        BootstrapOutcome::RecoveryRequired
    );
    assert_eq!(state.phase, zup_bootstrap::BootstrapPhase::RecoveryRequired);
}
