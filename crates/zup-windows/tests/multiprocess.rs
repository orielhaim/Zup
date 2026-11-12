#![cfg(all(windows, feature = "test-launcher"))]

mod requirements_fixture {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/deployment.rs"
    ));
}
use std::path::{Path, PathBuf};
use std::time::Duration;
use zup_plan::{DeploymentEffects, DeploymentPlan, compile_deployment};

use zup_core::SessionId;
use zup_core::TargetPath;
use zup_core::{Privilege, RelativePath, ResourceKey, Target, hash_reader};
use zup_plan::{FileOperation, FileOperationKind, FilePrecondition};
use zup_protocol::{
    ExecuteOperation, Message, PROTOCOL_VERSION, ParentHello, PrepareTransaction, WireEnvelope,
};

use zup_windows::launch_worker_for_test;
use zup_windows::{
    PipeServer, UserSid, WorkerBootstrap, format_bootstrap, frame_server,
    payload_overlay_base_root, verify_client_pid,
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

fn file_input(destination: &Path, source: &RelativePath, bytes: &[u8]) -> DeploymentEffects {
    let mut input = DeploymentEffects::new(
        zup_core::InstallationId::new_v7(),
        requirements_fixture::requirements(
            target(),
            zup_core::SelectedScope::User,
            "example.application",
            "1.0.0",
        ),
    );
    input.install_directory = Some(target_path(destination.parent().unwrap()));
    input.files.push(FileOperation {
        replacement: zup_core::FixedReplacement::Atomic,
        key: ResourceKey::File {
            id: zup_core::FileResourceId::Payload {
                id: zup_core::PayloadId::File {
                    declaration: zup_core::ResourceId::new("fixture-0").unwrap(),
                },
            },
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
    plan: Option<DeploymentPlan>,
    action: zup_plan::LifecycleAction,
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
id = "application"
source = "**/*"
placement = {{ kind = "fixed", directory = "${{install}}", replacement = "quiescent" }}
allow_empty = true
"#,
        target = target(),
        app_id = app_id(work_root),
    );
    let parsed = zup_manifest::parse(&manifest).unwrap();
    let installer = zup_manifest::parse_and_compile(&manifest, "default").unwrap();
    let config =
        zup_manifest::select_targets(&parsed, &["default"], &zup_core::TargetOverrides::default())
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

#[derive(Clone, Copy)]
enum WorkerExecution {
    Execute,
    CancelPrepared,
}

async fn execute_with_test_worker(
    worker: &Worker,
    execution: WorkerExecution,
) -> Result<zup_protocol::WorkerOutcome, zup_protocol::WorkerFailure> {
    let executable = worker_with_bundle(&worker.work_root, &worker.version, &worker.payload_root);
    execute_with_carrier(worker, execution, &executable).await
}

async fn execute_with_carrier(
    worker: &Worker,
    execution: WorkerExecution,
    executable: &Path,
) -> Result<zup_protocol::WorkerOutcome, zup_protocol::WorkerFailure> {
    let Worker {
        plan,
        action,
        authority,
        install_directory,
        version: _,
        payload_root,
        state_root,
        work_root,
        recovery_id,
        release,
        payload_overlay_root,
    } = worker;
    let zup_bin = executable.to_path_buf();
    let bundle = zup_windows::EmbeddedBundle::open(&zup_bin).unwrap();
    let plan = plan
        .clone()
        .or_else(|| {
            recovery_id.map(|id| {
                use zup_transaction::TransactionStore;
                zup_transaction::FilesystemTransactionStore::private_at(state_root)
                    .load(&zup_core::TransactionId::from_uuid(id))
                    .unwrap()
                    .plan
            })
        })
        .unwrap_or_else(|| {
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
                    operation: (*action).into(),
                    selection: zup_runtime::OperationSelection {
                        deployment: None,
                        enable: vec![],
                        disable: vec![],
                        install_directory: if matches!(
                            action,
                            zup_plan::LifecycleAction::Rollback { .. }
                                | zup_plan::LifecycleAction::Cleanup { .. }
                        ) {
                            None
                        } else {
                            Some(install_directory.clone())
                        },
                    },
                    prior: zup_windows::InstallationRecordStore::new(state_root)
                        .load(&app_id(work_root), zup_core::SelectedScope::User)
                        .unwrap()
                        .as_ref(),
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
            .load(&zup_core::TransactionId::from_uuid(id))
            .expect("recovery journal")
            .plan
    } else {
        plan.clone()
    };
    let intent = zup_plan::DeploymentIntent {
        deployment: zup_plan::DeploymentSelection::from_plan(&plan),
        action: *action,
        enable: vec![],
        disable: vec![],
        install_directory: if matches!(
            action,
            zup_plan::LifecycleAction::Rollback { .. } | zup_plan::LifecycleAction::Cleanup { .. }
        ) {
            None
        } else {
            Some(install_directory.clone())
        },
    };
    let operation = match recovery_id {
        Some(id) => zup_protocol::PreparedTransactionRequest::Recovery {
            transaction: zup_core::TransactionId::from_uuid(id),
        },
        None => zup_protocol::PreparedTransactionRequest::Deployment {
            plan: Box::new(plan.clone()),
            intent: Box::new(intent),
        },
    };
    let prepare = PrepareTransaction {
        content: None,
        operation,
        target: plan.target,
        app_id: app_id(work_root).to_string(),
        app_version: worker.version.clone(),
        scope: "user".into(),
        payload_root: if recovery_id.is_some() {
            Path::new("")
        } else if supplied_plan {
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
        release: if recovery_id.is_some() {
            None
        } else {
            release.clone()
        },
    };
    let plan_hash = prepare.digest().unwrap();
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
        expected_request_digest: plan_hash.clone(),
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
                expected_request_digest: plan_hash.clone(),
            }),
        })
        .await
        .expect("parent hello");
    writer
        .send(&WireEnvelope {
            version: PROTOCOL_VERSION,
            session_id,
            sequence: 2,
            message: Message::PrepareTransaction(Box::new(prepare)),
        })
        .await
        .expect("execute");
    loop {
        let message = tokio::time::timeout(Duration::from_secs(10), reader.recv())
            .await
            .expect("worker response timeout")
            .expect("worker response");
        match message.message {
            Message::Prepared(prepared) => {
                prepared.plan.validate().unwrap();
                assert_eq!(prepared.plan.fingerprint().to_hex(), prepared.plan_digest);
                if recovery_id.is_none() {
                    assert_eq!(prepared.plan.requirements, plan.requirements);
                    assert_eq!(prepared.plan.install_directory, plan.install_directory);
                }
                writer
                    .send(&WireEnvelope {
                        version: PROTOCOL_VERSION,
                        session_id,
                        sequence: 3,
                        message: match execution {
                            WorkerExecution::Execute => Message::Execute(ExecuteOperation {
                                plan_digest: prepared.plan_digest,
                            }),
                            WorkerExecution::CancelPrepared => {
                                assert!(
                                    zup_transaction::FilesystemTransactionStore::private_at(
                                        state_root
                                    )
                                    .list_ids()
                                    .unwrap()
                                    .is_empty()
                                );
                                assert!(!install_directory.join("App.exe").exists());
                                Message::Cancel
                            }
                        },
                    })
                    .await
                    .unwrap();
            }
            Message::Completed(completed) => {
                if matches!(
                    completed.outcome,
                    zup_protocol::WorkerOutcome::RolledBack
                        | zup_protocol::WorkerOutcome::RecoveryRequired
                ) {
                    use zup_transaction::TransactionStore;
                    let store = zup_transaction::FilesystemTransactionStore::private_at(state_root);
                    if let Ok(record) = store.load(&zup_core::TransactionId::from_uuid(
                        completed.transaction_id,
                    )) {
                        eprintln!(
                            "worker failure: {:?}; recovery: {:?}; compensation: {:?}",
                            record.failure, record.recovery_failure, record.rollback_failure
                        );
                    }
                }
                drop(reader);
                drop(writer);
                worker.wait_for_exit(Duration::from_secs(10)).unwrap();
                return Ok(completed.outcome);
            }
            Message::Failed(failed) => {
                drop(reader);
                drop(writer);
                worker.wait_for_exit(Duration::from_secs(10)).unwrap();
                return Err(failed.kind);
            }
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
    let dir = tempfile::tempdir_in(directories::BaseDirs::new().unwrap().data_local_dir()).unwrap();
    let payload_root = dir.path().join("payload");
    let target_root = dir.path().join("target");
    let state_root = dir.path().join("state");
    let work_root = dir.path().join("work");
    for path in [&payload_root, &target_root, &state_root, &work_root] {
        private_fs::PrivateDir::ensure(path).unwrap();
    }
    std::fs::write(payload_root.join("App.exe"), b"hello-app").unwrap();
    let registration = app_id(&work_root);
    let destination = target_root.join("App.exe");
    let outcome = execute_with_test_worker(
        &Worker {
            plan: None,
            action: zup_plan::LifecycleAction::Install,
            authority,
            install_directory: target_root,
            version: "1.0.0".to_owned(),
            payload_root,
            state_root,
            work_root,
            recovery_id: None,
            release: None,
            payload_overlay_root: None,
        },
        WorkerExecution::Execute,
    )
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
    let root =
        tempfile::tempdir_in(directories::BaseDirs::new().unwrap().data_local_dir()).unwrap();
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
    let plan = compile_deployment(&file_input(&destination, &source, bytes)).unwrap();
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
        execute_with_test_worker(
            &Worker {
                plan: Some(plan),
                action: zup_plan::LifecycleAction::Install,
                authority: zup_core::SelectedScope::User,
                install_directory: target_root,
                version: "1.0.0".to_owned(),
                payload_root,
                state_root,
                work_root,
                recovery_id: None,
                release: None,
                payload_overlay_root: Some(overlay.clone()),
            },
            WorkerExecution::Execute
        )
        .await,
        Err(zup_protocol::WorkerFailure::StalePlan)
    );
    assert!(!destination.exists());
}

#[tokio::test]
async fn cancelling_prepared_worker_publishes_no_journal_or_application() {
    let root =
        tempfile::tempdir_in(directories::BaseDirs::new().unwrap().data_local_dir()).unwrap();
    let payload = root.path().join("payload");
    let state = root.path().join("state");
    let work = root.path().join("work");
    let application = root.path().join("application");
    for path in [&payload, &state, &work, &application] {
        private_fs::PrivateDir::ensure(path).unwrap();
    }
    std::fs::write(payload.join("App.exe"), b"hello-app").unwrap();
    let result = execute_with_test_worker(
        &Worker {
            plan: None,
            action: zup_plan::LifecycleAction::Install,
            authority: zup_core::SelectedScope::User,
            install_directory: application.clone(),
            version: "1.0.0".into(),
            payload_root: payload,
            state_root: state.clone(),
            work_root: work,
            recovery_id: None,
            release: None,
            payload_overlay_root: None,
        },
        WorkerExecution::CancelPrepared,
    )
    .await;
    assert_eq!(result, Err(zup_protocol::WorkerFailure::Cancelled));
    assert!(
        zup_transaction::FilesystemTransactionStore::private_at(&state)
            .list_ids()
            .unwrap()
            .is_empty()
    );
    assert!(!application.join("App.exe").exists());
}

#[tokio::test]
async fn current_worker_rolls_back_owned_fixed_content_without_the_old_package() {
    let root =
        tempfile::tempdir_in(directories::BaseDirs::new().unwrap().data_local_dir()).unwrap();
    let payload = root.path().join("payload");
    let state = root.path().join("state");
    let work = root.path().join("work");
    let application = root.path().join("application");
    for path in [&payload, &state, &work, &application] {
        private_fs::PrivateDir::ensure(path).unwrap();
    }
    let mut worker = Worker {
        plan: None,
        action: zup_plan::LifecycleAction::Install,
        authority: zup_core::SelectedScope::User,
        install_directory: application.clone(),
        version: "1.0.0".into(),
        payload_root: payload.clone(),
        state_root: state.clone(),
        work_root: work.clone(),
        recovery_id: None,
        release: None,
        payload_overlay_root: None,
    };
    std::fs::write(payload.join("App.exe"), b"release-a").unwrap();
    assert_eq!(
        execute_with_test_worker(&worker, WorkerExecution::Execute).await,
        Ok(zup_protocol::WorkerOutcome::Committed)
    );
    let publisher = zup_windows::InstallationRecordStore::new(&state);
    let original = publisher
        .load(&app_id(&work), zup_core::SelectedScope::User)
        .unwrap()
        .unwrap();
    worker.version = "2.0.0".into();
    worker.action = zup_plan::LifecycleAction::Upgrade;
    std::fs::write(payload.join("App.exe"), b"release-b").unwrap();
    assert_eq!(
        execute_with_test_worker(&worker, WorkerExecution::Execute).await,
        Ok(zup_protocol::WorkerOutcome::Committed)
    );
    assert_eq!(
        std::fs::read(application.join("App.exe")).unwrap(),
        b"release-b"
    );
    worker.action = zup_plan::LifecycleAction::Rollback {
        revision: original.current.revision,
    };
    assert_eq!(
        execute_with_test_worker(&worker, WorkerExecution::Execute).await,
        Ok(zup_protocol::WorkerOutcome::Committed)
    );
    let restored = publisher
        .load(&app_id(&work), zup_core::SelectedScope::User)
        .unwrap()
        .unwrap();
    assert_eq!(restored.current.requirements, original.current.requirements);
    assert_eq!(
        std::fs::read(application.join("App.exe")).unwrap(),
        b"release-a"
    );
    assert_eq!(
        std::fs::read(payload.join("App.exe")).unwrap(),
        b"release-b"
    );
    worker.action = zup_plan::LifecycleAction::Uninstall;
    assert_eq!(
        execute_with_test_worker(&worker, WorkerExecution::Execute).await,
        Ok(zup_protocol::WorkerOutcome::Committed)
    );
    worker.action = zup_plan::LifecycleAction::Cleanup { keep: 0 };
    for _ in 0..4 {
        assert_eq!(
            execute_with_test_worker(&worker, WorkerExecution::Execute).await,
            Ok(zup_protocol::WorkerOutcome::Committed)
        );
    }
    assert!(!application.join("App.exe").exists());
}

#[cfg(feature = "test-faults")]
#[rstest::rstest]
#[case("apply_returned", "stage-file:")]
#[case("apply_returned", "generation:materialize:")]
#[case("apply_returned", "generation:seal:")]
#[case("installation_publication_requested", "")]
#[tokio::test]
async fn original_v1_carrier_recovers_v2_without_original_source(
    #[case] point: &str,
    #[case] operation: &str,
) {
    const CHILD: &str = "ZUP_SOURCE_FREE_WORKER_ROOT";
    let child_root = std::env::var_os(CHILD).map(PathBuf::from);
    let temporary = child_root.is_none().then(|| {
        tempfile::tempdir_in(directories::BaseDirs::new().unwrap().data_local_dir()).unwrap()
    });
    let root = child_root
        .clone()
        .unwrap_or_else(|| temporary.as_ref().unwrap().path().to_path_buf());
    let payload = root.join("payload");
    let state = root.join("state");
    let work = root.join("work");
    let application = root.join("application");
    let mut request = Worker {
        plan: None,
        action: zup_plan::LifecycleAction::Install,
        authority: zup_core::SelectedScope::User,
        install_directory: application.clone(),
        version: "1.0.0".into(),
        payload_root: payload.clone(),
        state_root: state.clone(),
        work_root: work.clone(),
        recovery_id: None,
        release: None,
        payload_overlay_root: None,
    };
    if child_root.is_some() {
        request.version = "2.0.0".into();
        request.action = zup_plan::LifecycleAction::Upgrade;
        let _ = execute_with_test_worker(&request, WorkerExecution::Execute).await;
        panic!("the injected worker interruption must prevent completion");
    }
    for path in [&payload, &state, &work, &application] {
        private_fs::PrivateDir::ensure(path).unwrap();
    }
    std::fs::write(payload.join("App.exe"), b"release-a").unwrap();
    assert_eq!(
        execute_with_test_worker(&request, WorkerExecution::Execute).await,
        Ok(zup_protocol::WorkerOutcome::Committed)
    );
    let carrier = root.join("v1-maintenance.exe");
    std::fs::copy(work.join("worker.exe"), &carrier).unwrap();
    std::fs::write(payload.join("App.exe"), b"release-b").unwrap();
    let test_name = std::thread::current().name().unwrap().to_owned();
    let result = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &test_name, "--nocapture"])
        .env(CHILD, &root)
        .env("ZUP_TEST_FAULT_POINT", point)
        .env("ZUP_TEST_FAULT_OPERATION", operation)
        .env("ZUP_TEST_FAULT_ACTION", "crash")
        .output()
        .unwrap();
    assert!(!result.status.success());
    use zup_transaction::TransactionStore;
    let store = zup_transaction::FilesystemTransactionStore::private_at(&state);
    let pending = store
        .list_ids()
        .unwrap()
        .into_iter()
        .map(|id| store.load(&id).unwrap())
        .find(|record| record.app_version.to_string() == "2.0.0")
        .unwrap_or_else(|| {
            panic!(
                "no v2 transaction: {}",
                String::from_utf8_lossy(&result.stderr)
            )
        });
    assert!(
        pending
            .nodes
            .values()
            .any(|node| !matches!(node, zup_transaction::NodeState::Pending))
    );
    let retained = state
        .join("transactions")
        .join(pending.transaction_id.to_string())
        .join("recovery");
    assert!(retained.join("source.json").exists());
    std::fs::remove_dir_all(&payload).unwrap();
    std::fs::remove_dir_all(&work).unwrap();
    request.recovery_id = Some(pending.transaction_id.as_uuid());
    let outcome = execute_with_carrier(&request, WorkerExecution::Execute, &carrier).await;
    if operation == "stage-file:" {
        assert_eq!(outcome, Ok(zup_protocol::WorkerOutcome::RecoveryRequired));
        assert!(retained.join("source.json").exists());
        assert_eq!(
            std::fs::read(application.join("App.exe")).unwrap(),
            b"release-a"
        );
    } else {
        assert_eq!(outcome, Ok(zup_protocol::WorkerOutcome::Committed));
        assert_eq!(
            std::fs::read(application.join("App.exe")).unwrap(),
            b"release-b"
        );
        assert!(!retained.exists());
    }
    windows_registry::CURRENT_USER
        .remove_tree(format!(
            "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\{}",
            app_id(&work)
        ))
        .unwrap();
}

#[tokio::test]
async fn authenticated_worker_refuses_unverified_release_provenance() {
    let root =
        tempfile::tempdir_in(directories::BaseDirs::new().unwrap().data_local_dir()).unwrap();
    let payload = root.path().join("payload");
    let state = root.path().join("state");
    let work = root.path().join("work");
    let application = root.path().join("application");
    for path in [&payload, &state, &work, &application] {
        private_fs::PrivateDir::ensure(path).unwrap();
    }
    std::fs::write(payload.join("App.exe"), b"payload").unwrap();
    let forged = zup_core::ReleaseIdentity {
        app_id: app_id(&work),
        release: digest(b"release"),
        document: digest(b"document"),
        catalog: digest(b"catalog"),
        variant: "application".into(),
        manifest: digest(b"manifest"),
        runtime: Some(digest(b"runtime")),
        version: "1.0.0".into(),
        target: target(),
        channel: "stable".into(),
        repository: "https://untrusted.invalid/".into(),
        pin: zup_core::ReleasePin::Channel {
            channel: "stable".into(),
        },
        trust_anchor: digest(b"untrusted root"),
        frontend: "console".into(),
        components: vec![],
    };
    let request = Worker {
        plan: None,
        action: zup_plan::LifecycleAction::Install,
        authority: zup_core::SelectedScope::User,
        install_directory: application.clone(),
        version: "1.0.0".into(),
        payload_root: payload,
        state_root: state.clone(),
        work_root: work,
        recovery_id: None,
        release: Some(forged),
        payload_overlay_root: None,
    };
    assert_eq!(
        execute_with_test_worker(&request, WorkerExecution::Execute).await,
        Err(zup_protocol::WorkerFailure::Authentication)
    );
    assert!(
        zup_transaction::FilesystemTransactionStore::private_at(&state)
            .list_ids()
            .unwrap()
            .is_empty()
    );
    assert!(!application.join("App.exe").exists());
}
