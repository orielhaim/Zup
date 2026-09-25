use std::sync::{Arc, Mutex};

use semver::Version;
use tempfile::TempDir;
use zup_bootstrap::{
    BootstrapError, BootstrapId, BootstrapKey, BootstrapOperation, BootstrapOperationState,
    BootstrapOutcome, BootstrapPlan, BootstrapState, BootstrapStateStore, DetectionResult,
    FilesystemBootstrapStateStore, PrerequisiteDetector, PrerequisiteProvider, ProviderOutcome,
    ProviderRequest, Quarantine, QuarantineError, execute_operation, recover,
};
use zup_core::{
    AppId, PrerequisiteArchitecture, PrerequisiteDetector as DetectorSpec, PrerequisiteId,
    PrerequisiteInstaller, PrerequisitePackage, RelativePath, SelectedScope, Sha256Digest,
};

fn digest(bytes: &[u8]) -> Sha256Digest {
    zup_core::hash_reader(bytes).unwrap().1
}

fn plan(package: PrerequisitePackage) -> BootstrapPlan {
    BootstrapPlan::new(
        BootstrapKey {
            app_id: AppId::new("com.example.bootstrap").unwrap(),
            app_version: Version::new(1, 0, 0),
            scope: SelectedScope::User,
        },
        vec![BootstrapOperation {
            id: PrerequisiteId::new("runtime").unwrap(),
            name: "Runtime".into(),
            target: PrerequisiteArchitecture::Current,
            detector: DetectorSpec::VisualCppV14 { version: None },
            package,
            installer: PrerequisiteInstaller::default(),
        }],
    )
    .unwrap()
}

struct FakeDetector {
    satisfied: Arc<Mutex<bool>>,
}

impl PrerequisiteDetector for FakeDetector {
    fn detect(&self, _operation: &BootstrapOperation) -> Result<DetectionResult, BootstrapError> {
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

fn embedded(bytes: &[u8]) -> PrerequisitePackage {
    PrerequisitePackage::Embedded {
        path: RelativePath::new("runtime.exe").unwrap(),
        sha256: digest(bytes),
        size: bytes.len() as u64,
    }
}

#[test]
fn already_satisfied_prerequisite_is_not_executed() {
    let package = embedded(b"runtime");
    let plan = plan(package);
    let satisfied = Arc::new(Mutex::new(true));
    let detector = FakeDetector {
        satisfied: satisfied.clone(),
    };
    let provider = FakeProvider {
        satisfied,
        outcome: ProviderOutcome::Succeeded,
        satisfy_on_success: true,
    };
    let mut state = BootstrapState::new(&plan);
    let operation = &plan.operations[0];
    let root = TempDir::new().unwrap();
    let executable = root.path().join("runtime.exe");
    std::fs::write(&executable, b"runtime").unwrap();
    let outcome = execute_operation(
        &plan, operation, &detector, &provider, executable, &mut state,
    )
    .unwrap();
    assert_eq!(outcome, BootstrapOutcome::Ready);
    assert!(matches!(
        state.operation_mut(&operation.id),
        Some(BootstrapOperationState::Satisfied { .. })
    ));
}

#[test]
fn successful_process_with_unsatisfied_detector_fails_closed() {
    let plan = plan(embedded(b"runtime"));
    let satisfied = Arc::new(Mutex::new(false));
    let detector = FakeDetector {
        satisfied: satisfied.clone(),
    };
    let provider = FakeProvider {
        satisfied,
        outcome: ProviderOutcome::Succeeded,
        satisfy_on_success: false,
    };
    let mut state = BootstrapState::new(&plan);
    let root = TempDir::new().unwrap();
    let executable = root.path().join("runtime.exe");
    std::fs::write(&executable, b"runtime").unwrap();
    let error = execute_operation(
        &plan,
        &plan.operations[0],
        &detector,
        &provider,
        executable,
        &mut state,
    )
    .unwrap_err();
    assert!(matches!(error, BootstrapError::DetectorStillUnsatisfied));
}

#[test]
fn reboot_exit_code_is_durable_and_blocks_completion() {
    let plan = plan(embedded(b"runtime"));
    let satisfied = Arc::new(Mutex::new(false));
    let detector = FakeDetector {
        satisfied: satisfied.clone(),
    };
    let provider = FakeProvider {
        satisfied,
        outcome: ProviderOutcome::RebootRequired { exit_code: 3010 },
        satisfy_on_success: false,
    };
    let mut state = BootstrapState::new(&plan);
    let root = TempDir::new().unwrap();
    let executable = root.path().join("runtime.exe");
    std::fs::write(&executable, b"runtime").unwrap();
    let outcome = execute_operation(
        &plan,
        &plan.operations[0],
        &detector,
        &provider,
        executable,
        &mut state,
    )
    .unwrap();
    assert_eq!(
        outcome,
        BootstrapOutcome::RebootRequired {
            exit_code: 3010,
            prerequisite_id: PrerequisiteId::new("runtime").unwrap(),
        }
    );
    assert!(state.reboot_required);
}

#[test]
fn assessment_does_not_trust_stale_satisfied_state() {
    let plan = plan(embedded(b"runtime"));
    let satisfied = Arc::new(Mutex::new(true));
    let detector = FakeDetector {
        satisfied: satisfied.clone(),
    };
    let mut state = BootstrapState::new(&plan);
    zup_bootstrap::assess(&plan, &detector, &mut state).unwrap();
    assert!(state.remaining.is_empty());
    *satisfied.lock().unwrap() = false;
    let remaining = zup_bootstrap::assess(&plan, &detector, &mut state).unwrap();
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
    let detector = FakeDetector { satisfied };
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
        zup_bootstrap::assess(&plan, &detector, &mut state),
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

#[test]
fn crash_recovery_re_detects_running_operation() {
    let plan = plan(embedded(b"runtime"));
    let satisfied = Arc::new(Mutex::new(false));
    let detector = FakeDetector { satisfied };
    let mut state = BootstrapState::new(&plan);
    state
        .mark(&plan.operations[0].id, BootstrapOperationState::Running)
        .unwrap();
    assert_eq!(
        recover(&plan, &detector, &mut state).unwrap(),
        BootstrapOutcome::RecoveryRequired
    );
    assert_eq!(state.phase, zup_bootstrap::BootstrapPhase::RecoveryRequired);
}
