#![cfg(all(windows, feature = "test-launcher"))]

use std::path::{Path, PathBuf};
use std::time::Duration;

use tempfile::TempDir;
use zup_core::{Privilege, RelativePath, ResourceKey, Target, hash_reader};
use zup_plan::TargetPath;
use zup_plan::{FileOperation, FileOperationKind, FilePrecondition};
use zup_protocol::{
    ExecuteTransaction, Message, PROTOCOL_VERSION, ParentHello, SessionId, WireEnvelope,
};
use zup_transaction::{TransactionInput, TransactionPlan, compile_transaction};
use zup_windows::launch_worker_for_test;
use zup_windows::{
    PipeServer, UserSid, WorkerBootstrap, format_bootstrap, frame_server,
    payload_overlay_base_root, plan_hash_hex, verify_client_pid,
};

fn digest(bytes: &[u8]) -> zup_core::Sha256Digest {
    hash_reader(bytes).unwrap().1
}

fn target() -> Target {
    Target::host().expect("host target")
}

fn target_path(path: &Path) -> TargetPath {
    TargetPath::new(target(), path.to_string_lossy()).unwrap()
}

fn file_input(destination: &Path, source: &RelativePath, bytes: &[u8]) -> TransactionInput {
    let mut input = TransactionInput::new(target());
    input.files.push(FileOperation {
        key: ResourceKey::File {
            destination: destination.to_string_lossy().into_owned(),
        },
        source_relative: source.clone(),
        destination: target_path(destination),
        precondition: FilePrecondition::Absent,
        expected_sha256: digest(bytes),
        expected_size: bytes.len() as u64,
        privilege: Privilege::User,
        kind: FileOperationKind::Create,
        executable: false,

        conflict: None,
    });
    input
}

struct Worker {
    plan: Option<TransactionPlan>,
    authority: zup_core::SelectedScope,
    install_directory: PathBuf,
    version: String,
    payload_root: PathBuf,
    state_root: PathBuf,
    work_root: PathBuf,
    recovery_id: Option<uuid::Uuid>,
    release: Option<zup_core::ReleaseIdentity>,
    payload_overlay_root: Option<PathBuf>,
}

fn app_id(work_root: &Path) -> zup_core::AppId {
    zup_core::AppId::new(format!(
        "com.zup.worker-{}",
        &digest(work_root.to_string_lossy().as_bytes()).to_hex()[..16]
    ))
    .unwrap()
}

fn worker_with_bundle(work_root: &Path, version: &str, payload_root: &Path) -> PathBuf {
    std::fs::create_dir_all(work_root.join("dist")).unwrap();
    for entry in std::fs::read_dir(payload_root).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_file() {
            std::fs::copy(entry.path(), work_root.join("dist").join(entry.file_name())).unwrap();
        }
    }
    let manifest = format!(
        r#"
schema = 1
frontend = "console"
[app]
id = "{app_id}"
name = "Acme App"
version = "{version}"
[build]
[build.targets.default]
target = "{target}"
source = {{ directory = "dist" }}
[install]
scope = "user"
allow_directory_override = true
[install.directory]
user = "${{location.user_data}}/AcmeApp"
[[files]]
source = "**/*"
destination = "${{install}}"
allow_empty = true
"#,
        target = target(),
        app_id = app_id(work_root),
    );
    let parsed = zup_manifest::parse(&manifest).unwrap();
    let installer = zup_manifest::parse_and_compile(&manifest, "default").unwrap();
    let config = zup_manifest::select_targets(
        &parsed,
        &["default"],
        &zup_manifest::TargetOverrides::default(),
    )
    .unwrap()
    .into_iter()
    .next()
    .unwrap();
    let mut build = zup_build::materialize(
        &work_root.join("zup.toml"),
        &parsed,
        vec![(config, installer)],
        zup_build::Writes::None,
    )
    .unwrap();
    let plan = build.targets.pop().unwrap();
    let package = work_root.join("worker.zup");
    std::fs::write(
        &package,
        zup_bundle::BundleWriter::encode(&plan, &[]).unwrap(),
    )
    .unwrap();
    let worker = work_root.join("worker.exe");
    zup_windows::embed_bundle_file(
        &PathBuf::from(env!("CARGO_BIN_EXE_zup-test-worker")),
        &worker,
        &package,
        None,
    )
    .unwrap();
    worker
}

async fn execute_with_test_worker(
    worker: &Worker,
) -> Result<zup_protocol::WorkerOutcome, zup_protocol::WorkerFailure> {
    let Worker {
        plan,
        authority,
        install_directory,
        version,
        payload_root,
        state_root,
        work_root,
        recovery_id,
        release,
        payload_overlay_root,
    } = worker;
    let zup_bin = worker_with_bundle(work_root, &worker.version, payload_root);
    let bundle = zup_windows::EmbeddedBundle::open(&zup_bin).unwrap();
    let intent = zup_plan::DeploymentIntent {
        action: zup_plan::LifecycleAction::Install,
        enable: vec![],
        disable: vec![],
        install_directory: Some(install_directory.clone()),
    };
    let plan = plan.clone().unwrap_or_else(|| {
        let build = bundle.build_plan().unwrap();
        let host = zup_windows::WindowsOperationHost {
            app_id: app_id(work_root),
            scope: zup_core::SelectedScope::User,
            frontend: zup_core::Frontend::Console,
            state_root: state_root.clone(),
            content: zup_windows::WindowsContent::Embedded(&bundle),
            acquire_prerequisites: false,
        };
        zup_runtime::prepare_operation(
            &zup_runtime::OperationInput {
                build: &build.targets[0],
                installer: &zup_bin,
                scope: zup_core::SelectedScope::User,
                operation: zup_runtime::Operation::Install,
                selection: zup_runtime::OperationSelection {
                    enable: vec![],
                    disable: vec![],
                    install_directory: Some(install_directory.clone()),
                },
                prior: None,
                state_root,
                cancellation: &zup_plan::NeverCancelled,
            },
            &host,
        )
        .unwrap()
        .transaction_plan
    });
    let supplied_plan = worker.plan.is_some();
    let recovery_id = *recovery_id;
    let payload_root = payload_root.as_path();
    let state_root = state_root.as_path();
    let work_root = work_root.as_path();
    let payload_overlay_root = payload_overlay_root.as_deref();
    let plan = if let Some(id) = recovery_id {
        use zup_transaction::TransactionStore;
        zup_transaction::FilesystemTransactionStore::private_at(state_root)
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
        target: plan.target,
        expected_plan_hash: plan_hash.clone(),
    };
    let worker = launch_worker_for_test(&zup_bin, &format_bootstrap(&bootstrap), *authority)
        .expect("spawn worker");
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
                target: plan.target,
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
                content: None,
                intent: Some(intent),
                target: plan.target,
                plan_json,
                plan_hash,
                app_id: app_id(work_root).to_string(),
                app_version: version.into(),
                scope: "user".into(),
                payload_root: if supplied_plan {
                    payload_root
                } else {
                    &zup_bin
                }
                .display()
                .to_string(),
                payload_overlay_root: payload_overlay_root.map(|path| path.display().to_string()),
                payload_overlay_base_root: payload_overlay_root.map(|_| {
                    payload_overlay_base_root(state_root, zup_core::SelectedScope::User)
                        .unwrap()
                        .display()
                        .to_string()
                }),
                state_root: state_root.display().to_string(),
                recovery_id,
                recovery_intent: None,
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
            Message::Completed(completed) => return Ok(completed.outcome),
            Message::Failed(failed) => return Err(failed.kind),
            _ => {}
        }
    }
}

#[rstest::rstest]
#[case(zup_core::SelectedScope::User)]
#[case(zup_core::SelectedScope::Machine)]
#[tokio::test]
async fn a_worker_executes_only_within_its_authority_domain(
    #[case] authority: zup_core::SelectedScope,
) {
    let dir = TempDir::new().unwrap();
    let payload_root = dir.path().join("payload");
    let target_root = dir.path().join("target");
    let state_root = dir.path().join("state");
    let work_root = dir.path().join("work");
    for path in [&payload_root, &target_root, &state_root, &work_root] {
        std::fs::create_dir_all(path).unwrap();
    }
    std::fs::write(payload_root.join("App.exe"), b"hello-app").unwrap();
    let registration = app_id(&work_root);
    let destination = target_root.join("App.exe");
    let outcome = execute_with_test_worker(&Worker {
        plan: None,
        authority,
        install_directory: target_root,
        version: "1.0.0".to_owned(),
        payload_root,
        state_root,
        work_root,
        recovery_id: None,
        release: None,
        payload_overlay_root: None,
    })
    .await;
    if authority == zup_core::SelectedScope::Machine {
        assert_eq!(outcome, Err(zup_protocol::WorkerFailure::Authentication));
        assert!(!destination.exists());
    } else {
        assert_eq!(outcome, Ok(zup_protocol::WorkerOutcome::Committed));
        assert_eq!(std::fs::read(destination).unwrap(), b"hello-app");
        windows_registry::CURRENT_USER
            .remove_tree(format!(
                "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\{registration}"
            ))
            .unwrap();
    }
}

#[tokio::test]
async fn authenticated_worker_rejects_an_overlay_not_declared_by_its_content() {
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
        app_id(&work_root),
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
            plan: Some(plan),
            authority: zup_core::SelectedScope::User,
            install_directory: target_root,
            version: "1.0.0".to_owned(),
            payload_root,
            state_root,
            work_root,
            recovery_id: None,
            release: None,
            payload_overlay_root: Some(overlay.clone()),
        })
        .await,
        Err(zup_protocol::WorkerFailure::StalePlan)
    );
    assert!(!destination.exists());
}
