use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use zup_bundle::DirectoryPayloadSource;
use zup_core::{RelativePath, SelectedScope, TargetTriple, hash_reader};
use zup_runtime::{
    CancellationHandle, ExecutionPolicy, InstallOutcome, RuntimeBackend, RuntimeControl,
    RuntimeEvent, RuntimeFuture, RuntimePayloadSource, RuntimeRequest, SessionError, run_install,
    run_install_control, run_install_control_with_policy,
};
use zup_transaction::{TransactionInput, compile_transaction};

struct TestDir(PathBuf);

impl TestDir {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("zup-runtime-{}", zup_runtime::Uuid::now_v7()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct FakeBackend {
    source: RuntimePayloadSource,
    calls: Arc<AtomicUsize>,
}

impl RuntimeBackend for FakeBackend {
    fn payload_source(&self, _request: &RuntimeRequest) -> RuntimePayloadSource {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.source.clone()
    }

    fn execute<'a>(
        &'a self,
        _request: RuntimeRequest,
        control: RuntimeControl,
    ) -> RuntimeFuture<'a, Result<InstallOutcome, SessionError>> {
        let calls = self.calls.clone();
        let cancellation = control.cancellation;
        let events = control.events;
        let policy = control.policy;
        Box::pin(async move {
            calls.fetch_add(1, Ordering::SeqCst);
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
        target: TargetTriple::parse("x86_64-pc-windows-msvc").unwrap(),
        app_id: zup_core::AppId::new("com.example.runtime").unwrap(),
        app_version: "1.0.0".parse().unwrap(),
        scope: SelectedScope::User,
        transaction_plan: compile_transaction(&TransactionInput::new(
            TargetTriple::parse("x86_64-pc-windows-msvc").unwrap(),
        ))
        .unwrap(),
        state_root: PathBuf::new(),
        work_root: PathBuf::new(),
        recovery_id: None,
        bootstrap: None,
    }
}

fn backend() -> (TestDir, FakeBackend) {
    let root = TestDir::new();
    let source = DirectoryPayloadSource::new(root.path());
    (
        root,
        FakeBackend {
            source: Arc::new(source),
            calls: Arc::new(AtomicUsize::new(0)),
        },
    )
}

#[tokio::test]
async fn runtime_forwards_control_and_reports_local_outcome() {
    let (_root, backend) = backend();
    let (events, _) = tokio::sync::broadcast::channel(32);
    let mut receiver = events.subscribe();
    let outcome = run_install_control(&backend, request(), CancellationHandle::new(), events)
        .await
        .unwrap();
    assert_eq!(outcome, InstallOutcome::Committed);
    let forwarded = std::iter::from_fn(|| receiver.try_recv().ok()).collect::<Vec<_>>();
    assert!(forwarded.iter().any(|event| matches!(
        event,
        RuntimeEvent::Progress {
            completed: 1,
            total: 1,
            ..
        }
    )));
    assert!(forwarded.iter().any(|event| matches!(
        event,
        RuntimeEvent::Completed { outcome } if outcome == "committed"
    )));
}

#[tokio::test]
async fn runtime_preserves_cancellation_and_authorization_policy() {
    let (_root, backend) = backend();
    let cancellation = CancellationHandle::new();
    cancellation.cancel();
    let (events, _) = tokio::sync::broadcast::channel(16);
    let mut receiver = events.subscribe();
    let result = run_install_control_with_policy(
        &backend,
        request(),
        cancellation,
        events,
        ExecutionPolicy::Interactive,
    )
    .await;
    assert!(matches!(result, Ok(InstallOutcome::Cancelled)));
    assert!(
        std::iter::from_fn(|| receiver.try_recv().ok()).any(|event| matches!(
            event,
            RuntimeEvent::Completed { outcome } if outcome == "cancelled"
        ))
    );

    let (events, _) = tokio::sync::broadcast::channel(16);
    let error = run_install_control_with_policy(
        &backend,
        request(),
        CancellationHandle::new(),
        events,
        ExecutionPolicy::NonInteractive,
    )
    .await
    .unwrap_err();
    assert!(matches!(error, SessionError::AuthorizationRequired));
    assert!(!ExecutionPolicy::NonInteractive.allows_authorization());
    assert!(ExecutionPolicy::Interactive.allows_authorization());
}

#[tokio::test]
async fn runtime_rejects_request_target_mismatch_before_backend() {
    let (_root, backend) = backend();
    let mut request = request();
    request.target = TargetTriple::parse("arm64-pc-windows-msvc").unwrap();
    let (events, _) = tokio::sync::broadcast::channel(8);
    let error = run_install_control(&backend, request, CancellationHandle::new(), events)
        .await
        .unwrap_err();
    assert!(matches!(error, SessionError::PlanInvalid(_)));
}

#[tokio::test]
async fn runtime_creates_a_session_and_exposes_a_verified_payload_source() {
    let root = TestDir::new();
    std::fs::write(root.path().join("payload.bin"), b"payload").unwrap();
    let source = DirectoryPayloadSource::new(root.path());
    let calls = Arc::new(AtomicUsize::new(0));
    let backend = FakeBackend {
        source: Arc::new(source),
        calls: calls.clone(),
    };
    let mut request = request();
    request.state_root = root.path().join("state");
    request.work_root = root.path().join("work");
    let (outcome, session) = run_install(&backend, request.clone()).await.unwrap();
    assert_eq!(outcome, InstallOutcome::Committed);
    assert!(!session.session_id.to_string().is_empty());
    assert!(calls.load(Ordering::SeqCst) >= 2);
    let relative = RelativePath::new("payload.bin").unwrap();
    let (size, digest) = hash_reader(&b"payload"[..]).unwrap();
    let mut bytes = Vec::new();
    use std::io::Read;
    backend
        .payload_source(&request)
        .open(&relative, &digest, size)
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    assert_eq!(bytes, b"payload");
}

#[test]
fn architecture_boundary_has_no_platform_adapter_dependency() {
    let manifest = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"))
        .unwrap()
        .to_ascii_lowercase();
    let adapter = b"zup-windows"
        .iter()
        .map(|byte| *byte as char)
        .collect::<String>();
    let forbidden_package = b"windows-registry"
        .iter()
        .map(|byte| *byte as char)
        .collect::<String>();
    assert!(!manifest.contains(&adapter));
    assert!(!manifest.contains(&forbidden_package));
}
