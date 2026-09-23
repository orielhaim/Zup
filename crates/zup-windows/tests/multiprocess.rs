//! Multi-process named-pipe integration tests (test launcher, no UAC).

#![cfg(feature = "test-launcher")]

use std::path::{Path, PathBuf};
use std::time::Duration;

use tempfile::TempDir;
use zup_core::{
    AppId, RelativePath, ResourceKey, ServiceId, ServiceStart, Sha256Digest, ShortcutLocation,
    hash_reader,
};
use zup_exec::{
    ExecutionPlan, ExecutionSummary, FileOperation, FileOperationKind, FilePrecondition,
    ObservedServiceState, ObservedShortcutState, ServiceOperation, ServiceOperationKind,
    ShortcutOperation, ShortcutOperationKind,
};
use zup_platform::{CommandSpec, TargetPath};
use zup_protocol::{Message, PROTOCOL_VERSION, ParentHello, SessionId, WireEnvelope};
use zup_transaction::compile_transaction;
use zup_windows::launch_worker_for_test;
use zup_windows::{
    InstallLedgerStore, PipeSecurity, PipeServer, UserSid, WorkerBootstrap, format_bootstrap,
    frame_server, plan_hash_hex, verify_client_pid,
};

struct ServiceCleanup(String);
impl Drop for ServiceCleanup {
    fn drop(&mut self) {
        use windows_service::service::ServiceAccess;
        use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
        if let Ok(manager) =
            ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
            && let Ok(service) = manager.open_service(&self.0, ServiceAccess::DELETE)
        {
            let _ = service.delete();
        }
    }
}

fn digest(b: &[u8]) -> Sha256Digest {
    hash_reader(b).unwrap().1
}

fn tpath(s: &str) -> TargetPath {
    TargetPath::new(PathBuf::from(s)).unwrap()
}

async fn execute_with_test_worker(
    execution: &ExecutionPlan,
    version: &str,
    payload_root: &Path,
    state_root: &Path,
    work_root: &Path,
    recovery_id: Option<uuid::Uuid>,
) -> String {
    let plan = if let Some(id) = recovery_id {
        use zup_transaction::TransactionStore;
        zup_transaction::FilesystemTransactionStore::new(state_root)
            .load(&zup_transaction::TransactionId::from_uuid(id))
            .expect("recovery journal")
            .plan
    } else {
        compile_transaction(execution).expect("compile")
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
        Message::WorkerHello(h) => {
            assert_eq!(h.protocol_version, PROTOCOL_VERSION);
            assert_eq!(h.session_id, session_id);
            assert!(h.capabilities.has_shortcut_service_v1());
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
            message: Message::ExecuteTransaction(zup_protocol::ExecuteTransaction {
                plan_json,
                plan_hash,
                app_id: "com.acme.app".into(),
                app_version: version.into(),
                scope: "machine".into(),
                payload_root: payload_root.display().to_string(),
                state_root: state_root.display().to_string(),
                work_root: work_root.display().to_string(),
                recovery_id,
            }),
        })
        .await
        .expect("execute");
    for _ in 0..8 {
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
    panic!("worker did not send Completed");
}

#[tokio::test]
async fn multi_process_named_pipe_handshake_and_execute() {
    let dir = TempDir::new().unwrap();
    let payload_root = dir.path().join("payload");
    let target_root = dir.path().join("target");
    let state_root = dir.path().join("state");
    let work_root = dir.path().join("work");
    std::fs::create_dir_all(&payload_root).unwrap();
    std::fs::create_dir_all(&target_root).unwrap();
    std::fs::create_dir_all(&state_root).unwrap();
    std::fs::create_dir_all(&work_root).unwrap();
    std::fs::write(payload_root.join("App.exe"), b"hello-app").unwrap();

    let dest = target_root.join("App.exe");
    let dest_str = dest.display().to_string();
    let execution = ExecutionPlan {
        selected_components: vec![],
        uninstall: false,
        removals: vec![],
        files: vec![FileOperation {
            key: ResourceKey::File {
                destination: dest_str.clone(),
            },
            kind: FileOperationKind::Create,
            destination: tpath(&dest_str),
            source_relative: RelativePath::new("App.exe").unwrap(),
            precondition: FilePrecondition::Absent,
            expected_sha256: digest(b"hello-app"),
            expected_size: 9,
            conflict: None,
        }],
        shortcuts: vec![],
        path_entries: vec![],
        services: vec![],
        protocols: vec![],
        file_types: vec![],
        external_actions: vec![],
        summary: ExecutionSummary {
            files_create: 1,
            ..Default::default()
        },
    };

    assert_eq!(
        execute_with_test_worker(
            &execution,
            "1.0.0",
            &payload_root,
            &state_root,
            &work_root,
            None
        )
        .await,
        "committed"
    );
    assert_eq!(std::fs::read(dest).unwrap(), b"hello-app");
}

#[tokio::test]
async fn authenticated_machine_worker_reads_embedded_bundle_payload() {
    let root = TempDir::new().unwrap();
    let project = root.path().join("project");
    let payload = project.join("dist");
    std::fs::create_dir_all(&payload).unwrap();
    std::fs::create_dir_all(root.path().join("state")).unwrap();
    std::fs::create_dir_all(root.path().join("work")).unwrap();
    std::fs::create_dir_all(root.path().join("target")).unwrap();
    std::fs::write(payload.join("App.exe"), b"hello-app").unwrap();
    let manifest = r#"
schema = 1
[app]
id = "com.acme.app"
name = "Acme"
version = "1.0.0"
[source]
directory = "dist"
[install]
scope = "machine"
[install.directory]
machine = "${known.program_files}/Acme"
[[files]]
source = "**/*"
destination = "${install}"
"#;
    std::fs::write(project.join("zup.toml"), manifest).unwrap();
    let parsed = zup_manifest::parse(manifest).unwrap();
    let installer = zup_manifest::parse_and_compile(manifest).unwrap();
    let build = zup_build::materialize(&project.join("zup.toml"), &parsed, installer).unwrap();
    let package = zup_bundle::BundleWriter::encode(&build).unwrap();
    let runtime = root.path().join("runtime.exe");
    let setup = root.path().join("Setup.exe");
    std::fs::write(&runtime, b"MZ worker runtime").unwrap();
    zup_bundle::append_bundle_to_executable(&runtime, &setup, &package).unwrap();

    let dest = root.path().join("target/App.exe");
    let dest_str = dest.display().to_string();
    let execution = ExecutionPlan {
        files: vec![FileOperation {
            key: ResourceKey::File {
                destination: dest_str.clone(),
            },
            kind: FileOperationKind::Create,
            destination: tpath(&dest_str),
            source_relative: RelativePath::new("App.exe").unwrap(),
            precondition: FilePrecondition::Absent,
            expected_sha256: digest(b"hello-app"),
            expected_size: 9,
            conflict: None,
        }],
        summary: ExecutionSummary {
            files_create: 1,
            ..Default::default()
        },
        ..Default::default()
    };
    assert_eq!(
        execute_with_test_worker(
            &execution,
            "1.0.0",
            &setup,
            &root.path().join("state"),
            &root.path().join("work"),
            None,
        )
        .await,
        "committed"
    );
    assert_eq!(std::fs::read(dest).unwrap(), b"hello-app");
    let maintenance = root
        .path()
        .join("state/maintenance/com.acme.app/machine/1.0.0/Setup.exe");
    assert!(zup_bundle::EmbeddedBundle::open(maintenance).is_ok());
}

#[tokio::test]
async fn worker_transaction_with_file_shortcut_and_service() {
    let dir = TempDir::new().unwrap();
    let payload_root = dir.path().join("payload");
    let state_root = dir.path().join("state");
    let work_root = dir.path().join("work");
    std::fs::create_dir_all(&payload_root).unwrap();
    std::fs::create_dir_all(&state_root).unwrap();
    std::fs::create_dir_all(&work_root).unwrap();
    std::fs::write(payload_root.join("App.exe"), b"hello-app").unwrap();
    let executable = TargetPath::new(dir.path().join("App.exe")).unwrap();
    let link = TargetPath::new(dir.path().join("App.lnk")).unwrap();
    let name = format!("zup-test-{}", uuid::Uuid::now_v7().simple());
    let _cleanup = ServiceCleanup(name.clone());
    let execution = ExecutionPlan {
        selected_components: vec![],
        uninstall: false,
        removals: vec![],
        files: vec![FileOperation {
            key: ResourceKey::File {
                destination: executable.to_string(),
            },
            kind: FileOperationKind::Create,
            destination: executable.clone(),
            source_relative: RelativePath::new("App.exe").unwrap(),
            precondition: FilePrecondition::Absent,
            expected_sha256: digest(b"hello-app"),
            expected_size: 9,
            conflict: None,
        }],
        shortcuts: vec![ShortcutOperation {
            key: ResourceKey::Shortcut {
                location: ShortcutLocation::Desktop,
                name: "App".into(),
            },
            kind: ShortcutOperationKind::Create,
            link_path: link.clone(),
            target: executable.clone(),
            arguments: vec!["a b".into()],
            working_directory: None,
            previous: ObservedShortcutState::Absent,
            conflict: None,
        }],
        services: vec![ServiceOperation {
            key: ResourceKey::Service {
                id: ServiceId::new(&name).unwrap(),
            },
            kind: ServiceOperationKind::Create,
            id: name.clone(),
            name: name.clone(),
            display_name: "Zup Test Service".into(),
            command: CommandSpec::new(executable.clone(), vec!["a b".into(), "世界".into()]),
            start: ServiceStart::Disabled,
            previous: ObservedServiceState::Absent,
            conflict: None,
        }],
        path_entries: vec![],
        protocols: vec![],
        file_types: vec![],
        external_actions: vec![],
        summary: ExecutionSummary {
            files_create: 1,
            shortcuts_create: 1,
            services_create: 1,
            ..Default::default()
        },
    };
    let outcome = execute_with_test_worker(
        &execution,
        "1.0.0",
        &payload_root,
        &state_root,
        &work_root,
        None,
    )
    .await;
    let ledger = InstallLedgerStore::new(&state_root)
        .load(
            &AppId::new("com.acme.app").unwrap(),
            zup_core::SelectedScope::Machine,
        )
        .unwrap();
    if zup_windows::is_process_elevated().unwrap() {
        assert_eq!(outcome, "committed");
        assert_eq!(std::fs::read(executable.as_path()).unwrap(), b"hello-app");
        assert!(link.as_path().is_file());
        assert_eq!(ledger.as_ref().unwrap().resources.len(), 3);
        let committed_id = ledger
            .as_ref()
            .unwrap()
            .committed_transaction
            .parse()
            .unwrap();
        assert_eq!(
            execute_with_test_worker(
                &execution,
                "1.0.0",
                &payload_root,
                &state_root,
                &work_root,
                Some(committed_id)
            )
            .await,
            "committed"
        );
        use zup_windows::{ServiceReader, ShortcutReader};
        std::fs::write(payload_root.join("App.exe"), b"upgraded-app").unwrap();
        let mut upgrade = execution.clone();
        upgrade.files[0].kind = FileOperationKind::Replace;
        upgrade.files[0].precondition = FilePrecondition::Exact {
            size: 9,
            sha256: digest(b"hello-app"),
        };
        upgrade.files[0].expected_sha256 = digest(b"upgraded-app");
        upgrade.files[0].expected_size = 12;
        upgrade.shortcuts[0].kind = ShortcutOperationKind::UpdateOwned;
        upgrade.shortcuts[0].previous = zup_windows::WindowsShortcutReader
            .read_shortcut(&link)
            .unwrap();
        upgrade.shortcuts[0].arguments = vec!["upgrade".into()];
        upgrade.services[0].kind = ServiceOperationKind::UpdateOwned;
        upgrade.services[0].previous = zup_windows::WindowsServiceReader
            .read_service(&name)
            .unwrap();
        upgrade.services[0].start = ServiceStart::Manual;
        assert_eq!(
            execute_with_test_worker(
                &upgrade,
                "2.0.0",
                &payload_root,
                &state_root,
                &work_root,
                None
            )
            .await,
            "committed"
        );
        assert_eq!(
            std::fs::read(executable.as_path()).unwrap(),
            b"upgraded-app"
        );
        let ledger = InstallLedgerStore::new(&state_root)
            .load(
                &AppId::new("com.acme.app").unwrap(),
                zup_core::SelectedScope::Machine,
            )
            .unwrap()
            .unwrap();
        assert_eq!(ledger.version.to_string(), "2.0.0");
        assert_eq!(
            execute_with_test_worker(
                &execution,
                "1.0.0",
                &payload_root,
                &state_root,
                &work_root,
                Some(committed_id),
            )
            .await,
            "committed"
        );
        assert_eq!(
            InstallLedgerStore::new(&state_root)
                .load(
                    &AppId::new("com.acme.app").unwrap(),
                    zup_core::SelectedScope::Machine,
                )
                .unwrap()
                .unwrap()
                .version
                .to_string(),
            "2.0.0"
        );
        let uninstall = ExecutionPlan {
            uninstall: true,
            removals: ledger
                .resources
                .iter()
                .map(|(key, owned)| zup_exec::RemovalOperation {
                    key: key.clone(),
                    kind: zup_exec::RemovalKind::RemoveOwned,
                    scope: zup_core::SelectedScope::Machine,
                    owned: owned.clone(),
                })
                .collect(),
            ..Default::default()
        };
        assert_eq!(
            execute_with_test_worker(
                &uninstall,
                "2.0.0",
                &payload_root,
                &state_root,
                &work_root,
                None
            )
            .await,
            "committed"
        );
        assert!(!executable.as_path().exists());
        assert!(!link.as_path().exists());
        assert!(matches!(
            zup_windows::WindowsServiceReader
                .read_service(&name)
                .unwrap(),
            ObservedServiceState::Absent
        ));
        assert!(
            InstallLedgerStore::new(&state_root)
                .load(
                    &AppId::new("com.acme.app").unwrap(),
                    zup_core::SelectedScope::Machine
                )
                .unwrap()
                .is_none()
        );
    } else {
        assert_eq!(outcome, "rolled_back");
        assert!(!executable.as_path().exists());
        assert!(!link.as_path().exists());
        assert!(ledger.is_none());
    }
}

#[test]
fn pipe_security_descriptor_policy() {
    let security = PipeSecurity::for_initiating_user().expect("sd");
    assert!(!security.raw().is_null());
}

#[test]
fn uac_identity_model_allows_different_sids() {
    // Over-the-shoulder: worker SID may differ from parent SID.
    let parent = UserSid::current().unwrap();
    assert!(!parent.display().is_empty());
    assert_eq!(
        parent,
        UserSid::for_process(std::process::id()).expect("current process token")
    );
    let _elevated = zup_windows::is_process_elevated().expect("token elevation query");
}
