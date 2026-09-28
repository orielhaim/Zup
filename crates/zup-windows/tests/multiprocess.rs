#![cfg(feature = "test-launcher")]

use std::path::{Path, PathBuf};
use std::time::Duration;

use tempfile::TempDir;
use zup_core::{Privilege, RelativePath, ResourceKey, TargetTriple, hash_reader};
use zup_platform::TargetPath;
use zup_protocol::{
    ExecuteTransaction, Message, PROTOCOL_VERSION, ParentHello, SessionId, WireEnvelope,
};
use zup_transaction::{
    FileDelta, FilePrecondition, FileWork, TransactionInput, TransactionPlan, compile_transaction,
};
use zup_windows::launch_worker_for_test;
use zup_windows::{
    PipeServer, UserSid, WorkerBootstrap, format_bootstrap, frame_server,
    payload_overlay_base_root, plan_hash_hex, verify_client_pid,
};

fn digest(bytes: &[u8]) -> zup_core::Sha256Digest {
    hash_reader(bytes).unwrap().1
}

fn target() -> TargetTriple {
    TargetTriple::parse("x86_64-pc-windows-msvc").unwrap()
}

fn target_path(path: &Path) -> TargetPath {
    TargetPath::new(target(), path.to_string_lossy()).unwrap()
}

fn file_input(destination: &Path, source: &RelativePath, bytes: &[u8]) -> TransactionInput {
    let mut input = TransactionInput::new(target());
    input.files.push(FileWork {
        key: ResourceKey::File {
            destination: destination.to_string_lossy().into_owned(),
        },
        source_relative: source.clone(),
        destination: target_path(destination),
        precondition: FilePrecondition::Absent,
        expected_sha256: digest(bytes),
        expected_size: bytes.len() as u64,
        privilege: Privilege::User,
        delta: FileDelta::Create,
    });
    input
}

/// The one worker invocation these tests share.
///
/// A struct rather than eight positional arguments, because a test helper whose
/// arguments are four paths and two options is a test helper whose call sites
/// cannot be read.
struct Worker {
    plan: TransactionPlan,
    version: String,
    payload_root: PathBuf,
    state_root: PathBuf,
    work_root: PathBuf,
    recovery_id: Option<uuid::Uuid>,
    release: Option<zup_core::ReleaseIdentity>,
    payload_overlay_root: Option<PathBuf>,
}

async fn execute_with_test_worker(worker: &Worker) -> String {
    let Worker {
        plan,
        version,
        payload_root,
        state_root,
        work_root,
        recovery_id,
        release,
        payload_overlay_root,
    } = worker;
    let plan = plan.clone();
    let recovery_id = *recovery_id;
    let payload_root = payload_root.as_path();
    let state_root = state_root.as_path();
    let work_root = work_root.as_path();
    let payload_overlay_root = payload_overlay_root.as_deref();
    let plan = if let Some(id) = recovery_id {
        use zup_transaction::TransactionStore;
        zup_transaction::FilesystemTransactionStore::new(state_root)
            .load(&zup_transaction::TransactionId::from_uuid(id))
            .expect("recovery journal")
            .plan
    } else {
        plan.clone()
    };
    let plan_json = serde_json::to_string(&plan).unwrap();
    let plan_hash = plan_hash_hex(&plan_json);
    let session_id = SessionId::new_v7();
    let pipe_name = zup_windows::pipe_name(&session_id.to_string());
    let mut server = PipeServer::create(&pipe_name).expect("create pipe");
    let bootstrap = WorkerBootstrap {
        protocol_version: PROTOCOL_VERSION,
        session_id,
        pipe_name: pipe_name.clone(),
        expected_parent_pid: std::process::id(),
        expected_parent_sid: UserSid::current().unwrap().display().to_owned(),
        target: plan.target.clone(),
        expected_plan_hash: plan_hash.clone(),
    };
    let zup_bin = PathBuf::from(env!("CARGO_BIN_EXE_zup-test-worker"));
    assert!(zup_bin.exists(), "zup binary not found at {zup_bin:?}");
    let worker =
        launch_worker_for_test(&zup_bin, &format_bootstrap(&bootstrap)).expect("spawn worker");
    tokio::time::timeout(Duration::from_secs(5), server.connect())
        .await
        .expect("worker connect timeout")
        .expect("worker connect");
    assert!(matches!(
        verify_client_pid(server.as_raw() as isize, worker.pid().wrapping_add(1)),
        Err(zup_windows::TransportError::WorkerPidMismatch { .. })
    ));
    verify_client_pid(server.as_raw() as isize, worker.pid()).expect("exact worker PID");
    let (mut reader, mut writer) = frame_server(server.into_inner().expect("into inner"));
    let hello = tokio::time::timeout(Duration::from_secs(5), reader.recv())
        .await
        .expect("hello timeout")
        .expect("hello");
    match hello.message {
        Message::WorkerHello(hello) => {
            assert_eq!(hello.protocol_version, PROTOCOL_VERSION);
            assert_eq!(hello.session_id, session_id);
            assert!(hello.capabilities.backend_operations_v1);
        }
        other => panic!("expected WorkerHello, got {other:?}"),
    }
    writer
        .send(&WireEnvelope {
            version: PROTOCOL_VERSION,
            session_id,
            sequence: 1,
            message: Message::ParentHello(ParentHello {
                protocol_version: PROTOCOL_VERSION,
                session_id,
                target: plan.target.clone(),
                transaction_id: uuid::Uuid::now_v7(),
                expected_plan_hash: plan_hash.clone(),
            }),
        })
        .await
        .expect("parent hello");
    writer
        .send(&WireEnvelope {
            version: PROTOCOL_VERSION,
            session_id,
            sequence: 2,
            message: Message::ExecuteTransaction(Box::new(ExecuteTransaction {
                target: plan.target.clone(),
                plan_json,
                plan_hash,
                app_id: "com.acme.app".into(),
                app_version: version.into(),
                scope: "user".into(),
                payload_root: payload_root.display().to_string(),
                payload_overlay_root: payload_overlay_root.map(|path| path.display().to_string()),
                payload_overlay_base_root: payload_overlay_root.map(|_| {
                    payload_overlay_base_root(state_root, zup_core::SelectedScope::User)
                        .unwrap()
                        .display()
                        .to_string()
                }),
                state_root: state_root.display().to_string(),
                work_root: work_root.display().to_string(),
                recovery_id,
                release: release.clone(),
            })),
        })
        .await
        .expect("execute");
    loop {
        let message = tokio::time::timeout(Duration::from_secs(10), reader.recv())
            .await
            .expect("worker response timeout")
            .expect("worker response");
        match message.message {
            Message::Completed(completed) => return completed.outcome,
            Message::Failed(failed) => panic!("worker failed: {}: {}", failed.kind, failed.message),
            _ => {}
        }
    }
}

#[tokio::test]
async fn multi_process_named_pipe_handshake_and_execute() {
    let dir = TempDir::new().unwrap();
    let payload_root = dir.path().join("payload");
    let target_root = dir.path().join("target");
    let state_root = dir.path().join("state");
    let work_root = dir.path().join("work");
    for path in [&payload_root, &target_root, &state_root, &work_root] {
        std::fs::create_dir_all(path).unwrap();
    }
    std::fs::write(payload_root.join("App.exe"), b"hello-app").unwrap();
    let destination = target_root.join("App.exe");
    let plan = compile_transaction(&file_input(
        &destination,
        &RelativePath::new("App.exe").unwrap(),
        b"hello-app",
    ))
    .unwrap();
    assert_eq!(
        execute_with_test_worker(&Worker {
            plan,
            version: "1.0.0".to_owned(),
            payload_root,
            state_root,
            work_root,
            recovery_id: None,
            release: None,
            payload_overlay_root: None,
        })
        .await,
        "committed"
    );
    assert_eq!(std::fs::read(destination).unwrap(), b"hello-app");
}

#[tokio::test]
async fn authenticated_worker_consumes_generated_overlay() {
    let root = TempDir::new().unwrap();
    let payload_root = root.path().join("payload");
    let state_root = root.path().join("state");
    let work_root = root.path().join("work");
    let target_root = root.path().join("target");
    for path in [&payload_root, &state_root, &work_root, &target_root] {
        std::fs::create_dir_all(path).unwrap();
    }
    let source = RelativePath::new("__zup_plugins__/generated.bin").unwrap();
    let bytes = b"plugin-generated";
    let destination = target_root.join("generated.bin");
    let plan = compile_transaction(&file_input(&destination, &source, bytes)).unwrap();
    let identity = zup_windows::PayloadOverlayIdentity::from_transaction(
        zup_core::AppId::new("com.acme.app").unwrap(),
        "1.0.0".parse().unwrap(),
        zup_core::SelectedScope::User,
        &plan,
    )
    .unwrap();
    let overlay_base =
        payload_overlay_base_root(&state_root, zup_core::SelectedScope::User).unwrap();
    let overlay = identity.path_under(&overlay_base).unwrap();
    let overlay_file = overlay.join(source.as_str());
    std::fs::create_dir_all(overlay_file.parent().unwrap()).unwrap();
    std::fs::write(&overlay_file, bytes).unwrap();
    assert_eq!(
        execute_with_test_worker(&Worker {
            plan,
            version: "1.0.0".to_owned(),
            payload_root,
            state_root,
            work_root,
            recovery_id: None,
            release: None,
            payload_overlay_root: Some(overlay.clone()),
        })
        .await,
        "committed"
    );
    assert_eq!(std::fs::read(destination).unwrap(), bytes);
}
