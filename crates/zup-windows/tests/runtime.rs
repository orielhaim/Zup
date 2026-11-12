#![cfg(windows)]

mod requirements_fixture {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/deployment.rs"
    ));
}
use std::path::Path;
use zup_core::TargetPath;
use zup_plan::{DeploymentEffects, compile_deployment};

use tempfile::TempDir;
use zup_core::{
    App, AppId, NonEmptyString, Privilege, RelativePath, ResourceKey, SelectedScope, Target,
    hash_reader,
};
use zup_plan::{DesiredState, LifecycleAction};

use zup_plan::{FileOperation, FileOperationKind, FilePrecondition};
use zup_runtime::{ExecutionPolicy, InstallOutcome, RuntimeRequest, SessionError};
use zup_transaction::{FilesystemTransactionStore, ReconciliationState, TransactionStore};
use zup_windows::{InstallationRecordStore, PayloadOverlayIdentity, WindowsRuntimeBackend};

fn temp_root() -> TempDir {
    let base = directories::BaseDirs::new().unwrap();
    tempfile::tempdir_in(base.data_local_dir()).unwrap()
}

fn digest(bytes: &[u8]) -> zup_core::Sha256Digest {
    hash_reader(bytes).unwrap().1
}

fn target_path(path: &Path) -> TargetPath {
    TargetPath::new(
        Target::parse("windows-x64").unwrap(),
        path.to_string_lossy(),
    )
    .unwrap()
}

fn target() -> Target {
    Target::parse("windows-x64").unwrap()
}

fn file_input(root: &TempDir, scope: SelectedScope) -> DeploymentEffects {
    let destination = root.path().join("install").join("app.exe");
    let source = RelativePath::new("app.exe").unwrap();
    let mut input = DeploymentEffects::new(
        zup_core::InstallationId::new_v7(),
        requirements_fixture::requirements(target(), scope, "com.zup.windows-runtime", "1.0.0"),
    );
    input.install_directory = Some(target_path(&root.path().join("install")));
    input.files.push(FileOperation {
        replacement: zup_core::FixedReplacement::Atomic,
        key: ResourceKey::File {
            id: zup_core::FileResourceId::Payload {
                id: zup_core::PayloadId::File {
                    declaration: zup_core::ResourceId::new("fixture-0").unwrap(),
                },
            },
        },
        source_relative: source,
        destination: target_path(&destination),
        precondition: FilePrecondition::Absent,
        expected_sha256: digest(b"installed"),
        expected_size: 9,
        privilege: if scope == SelectedScope::Machine {
            Privilege::System
        } else {
            Privilege::User
        },
        kind: FileOperationKind::Create,
        executable: false,

        conflict: None,
    });
    input
}

async fn run(
    backend: &WindowsRuntimeBackend,
    request: RuntimeRequest,
    policy: ExecutionPolicy,
) -> Result<InstallOutcome, SessionError> {
    let control = zup_runtime::OperationControl::new(policy);
    let store = FilesystemTransactionStore::private_at(&request.state_root);
    let outcome = zup_runtime::execute(backend, request, &control).await;
    if !matches!(outcome, Ok(InstallOutcome::Committed)) {
        for id in store.list_ids().unwrap_or_default() {
            if let Ok(record) = store.load(&id) {
                eprintln!(
                    "failure: {:?}; recovery: {:?}; compensation: {:?}",
                    record.failure, record.recovery_failure, record.rollback_failure
                );
            }
        }
    }
    outcome
}

fn request(root: &TempDir, scope: SelectedScope) -> (RuntimeRequest, WindowsRuntimeBackend) {
    let state_root = root.path().join("state");
    let payload_root = root.path().join("payload");
    std::fs::create_dir_all(&payload_root).unwrap();
    std::fs::write(payload_root.join("app.exe"), b"installed").unwrap();
    let app_id = AppId::new("com.zup.windows-runtime").unwrap();
    let mut input = file_input(root, scope);
    if let Some(prior) = InstallationRecordStore::new(&state_root)
        .load(&app_id, scope)
        .unwrap()
    {
        input.installation = prior.id;
        input.observed = Some(prior.current.selection());
        input.observed_transaction = Some(prior.current.transaction);
    }
    let plan = compile_deployment(&input).unwrap();
    let request = RuntimeRequest {
        operation: zup_runtime::RuntimeOperation::Deployment(Box::new(
            zup_runtime::RuntimeDeployment {
                plan: plan.clone(),

                intent: zup_plan::DeploymentIntent {
                    deployment: zup_plan::DeploymentSelection::from_plan(&plan),
                    action: if plan.transition.is_remove() {
                        zup_plan::LifecycleAction::Uninstall
                    } else {
                        zup_plan::LifecycleAction::Install
                    },
                    enable: Vec::new(),
                    disable: Vec::new(),
                    install_directory: None,
                },
                bootstrap: None,
                release: None,
            },
        )),
        target: target(),
        app_id,
        app_version: "1.0.0".parse().unwrap(),
        scope,
        state_root,
    };
    let backend = WindowsRuntimeBackend::from_path(payload_root, None).unwrap();
    (request, backend)
}

#[tokio::test]
async fn local_backend_commits_files_and_publishes_ledger() {
    let root = temp_root();
    let (request, backend) = request(&root, SelectedScope::User);
    let destination = root.path().join("install").join("app.exe");
    let outcome = run(&backend, request.clone(), ExecutionPolicy::Interactive)
        .await
        .unwrap();
    assert_eq!(outcome, InstallOutcome::Committed);
    assert_eq!(std::fs::read(destination).unwrap(), b"installed");
    let ledger = InstallationRecordStore::new(&request.state_root)
        .load(&request.app_id, SelectedScope::User)
        .unwrap()
        .unwrap();
    assert_eq!(ledger.current.version.to_string(), "1.0.0");
    let journal = FilesystemTransactionStore::private_at(&request.state_root)
        .load(&ledger.current.transaction)
        .unwrap();
    assert_eq!(journal.reconciliation, ReconciliationState::Complete);
}

#[tokio::test]
async fn recovery_uses_the_protected_journal_without_a_frontend_plan() {
    let root = temp_root();
    let (mut request, backend) = request(&root, SelectedScope::User);
    let plan = request
        .operation
        .deployment()
        .map(|deployment| &deployment.plan)
        .unwrap()
        .clone();
    let store = FilesystemTransactionStore::private_at(&request.state_root);
    let pending = zup_transaction::TransactionCoordinator::new(store.clone())
        .begin(
            request.app_id.clone(),
            request.scope,
            request.app_version.clone(),
            plan,
        )
        .unwrap()
        .preserve();
    request.operation = zup_runtime::RuntimeOperation::Recovery {
        transaction: pending.transaction_id,
    };
    let mut foreign = request.clone();
    foreign.app_id = AppId::new("com.zup.different-application").unwrap();
    assert!(matches!(
        run(&backend, foreign, ExecutionPolicy::Interactive).await,
        Err(SessionError::Protocol(_))
    ));
    assert_eq!(store.load(&pending.transaction_id).unwrap(), pending);
    assert_eq!(
        run(&backend, request, ExecutionPolicy::Interactive)
            .await
            .unwrap(),
        InstallOutcome::Recovered {
            settlement: zup_runtime::RecoverySettlement::RolledBack
        }
    );
    assert!(!root.path().join("install/app.exe").exists());
}

#[tokio::test]
async fn generated_overlay_is_consumed_and_cleaned_by_adapter() {
    let root = temp_root();
    let (mut request, _) = request(&root, SelectedScope::User);
    zup_storage::ensure_private_dir(&request.state_root).unwrap();
    let source = RelativePath::new("__zup_plugins__/generated.exe").unwrap();
    let zup_runtime::RuntimeOperation::Deployment(deployment) = &mut request.operation else {
        unreachable!()
    };
    let plan = &mut deployment.plan;
    for node in &mut plan.nodes {
        if matches!(
            node.kind,
            zup_plan::NodeKind::StageFile { .. } | zup_plan::NodeKind::FileMutation { .. }
        ) {
            node.meta.source_relative = Some(source.clone());
        }
    }
    let identity = PayloadOverlayIdentity::from_transaction(
        request.app_id.clone(),
        request.app_version.clone(),
        request.scope,
        plan,
    )
    .unwrap();
    let base = zup_windows::payload_overlay_base_root(&request.state_root, request.scope).unwrap();
    let overlay = identity.path_under(&base).unwrap();
    std::fs::create_dir_all(overlay.join("__zup_plugins__")).unwrap();
    std::fs::write(overlay.join("__zup_plugins__/generated.exe"), b"installed").unwrap();
    let payload_root = root.path().join("payload");
    let backend = WindowsRuntimeBackend::from_path(payload_root, Some(overlay.clone())).unwrap();
    let outcome = run(&backend, request, ExecutionPolicy::Interactive)
        .await
        .unwrap();
    assert_eq!(outcome, InstallOutcome::Committed);
    assert!(!overlay.exists());
}

#[tokio::test]
async fn noninteractive_machine_scope_reports_authorization_required() {
    if zup_windows::is_process_elevated().unwrap() {
        return;
    }
    let root = temp_root();
    let (request, backend) = request(&root, SelectedScope::Machine);
    let error = run(&backend, request, ExecutionPolicy::NonInteractive)
        .await
        .unwrap_err();
    assert!(matches!(error, SessionError::AuthorizationRequired));
}

#[tokio::test]
async fn uninstall_preserves_authoritative_history_and_supports_reinstallation() {
    let root = temp_root();
    let (request, backend) = request(&root, SelectedScope::User);
    let app_id = request.app_id.clone();
    let app_version = request.app_version.clone();
    let state_root = request.state_root.clone();
    let destination = root.path().join("install").join("app.exe");
    let payload_root = root.path().join("payload");
    let outcome = run(&backend, request, ExecutionPolicy::Interactive)
        .await
        .unwrap();
    assert_eq!(outcome, InstallOutcome::Committed);
    let ledger = InstallationRecordStore::new(&state_root)
        .load(&app_id, SelectedScope::User)
        .unwrap()
        .unwrap();
    let uninstall = zup_windows::plan_target_lifecycle(
        LifecycleAction::Uninstall,
        &app_id,
        SelectedScope::User,
        None,
        &state_root,
    )
    .unwrap();
    let request = RuntimeRequest {
        operation: zup_runtime::RuntimeOperation::Deployment(Box::new(
            zup_runtime::RuntimeDeployment {
                plan: uninstall.clone(),

                intent: zup_plan::DeploymentIntent {
                    deployment: zup_plan::DeploymentSelection::from_plan(&uninstall),
                    action: zup_plan::LifecycleAction::Uninstall,
                    enable: Vec::new(),
                    disable: Vec::new(),
                    install_directory: None,
                },
                bootstrap: None,
                release: None,
            },
        )),
        target: target(),
        app_id,
        app_version,
        scope: SelectedScope::User,
        state_root: state_root.clone(),
    };
    let backend = WindowsRuntimeBackend::from_path(payload_root, None).unwrap();
    let outcome = run(&backend, request, ExecutionPolicy::Interactive)
        .await
        .unwrap();
    assert_eq!(outcome, InstallOutcome::Committed);
    assert!(!destination.exists());
    let journals = FilesystemTransactionStore::private_at(&state_root);
    let remaining = journals.list_ids().unwrap();
    assert!(remaining.contains(&ledger.current.transaction));
    let removed = InstallationRecordStore::new(&state_root)
        .load(&ledger.app_id, SelectedScope::User)
        .unwrap()
        .unwrap();
    assert_eq!(removed.state, zup_plan::InstallationState::Removed);
    let uninstall = journals.load(&removed.current.transaction).unwrap();
    assert!(uninstall.plan.transition.is_remove());
    assert_eq!(uninstall.reconciliation, ReconciliationState::Complete);
    InstallationRecordStore::new(&state_root)
        .repair_committed(&ledger.app_id, SelectedScope::User)
        .unwrap();
    let (reinstall, backend) = self::request(&root, SelectedScope::User);
    assert_eq!(
        run(&backend, reinstall, ExecutionPolicy::Interactive)
            .await
            .unwrap(),
        InstallOutcome::Committed
    );
    let ledgers = InstallationRecordStore::new(&state_root);
    let newer = ledgers
        .load(&ledger.app_id, SelectedScope::User)
        .unwrap()
        .unwrap();
    let reconciled = ledgers
        .reconcile_installation(&uninstall, SelectedScope::User)
        .unwrap();
    assert_eq!(reconciled.current.transaction, newer.current.transaction);
    assert_eq!(std::fs::read(&destination).unwrap(), b"installed");
    assert_eq!(
        ledgers
            .load(&ledger.app_id, SelectedScope::User)
            .unwrap()
            .unwrap(),
        newer
    );
}

#[tokio::test]
async fn forged_installation_record_cannot_authorize_uninstall() {
    let root = temp_root();
    let (request, backend) = request(&root, SelectedScope::User);
    let app_id = request.app_id.clone();
    let app_version = request.app_version.clone();
    let state_root = request.state_root.clone();
    let destination = root.path().join("install").join("app.exe");
    let payload_root = root.path().join("payload");
    let outcome = run(&backend, request, ExecutionPolicy::Interactive)
        .await
        .unwrap();
    assert_eq!(outcome, InstallOutcome::Committed);
    let uninstall = zup_windows::plan_target_lifecycle(
        LifecycleAction::Uninstall,
        &app_id,
        SelectedScope::User,
        None,
        &state_root,
    )
    .unwrap();
    let store = InstallationRecordStore::new(&state_root);
    let mut ledger = store.load(&app_id, SelectedScope::User).unwrap().unwrap();
    ledger.current.transaction = "ffffffff-ffff-ffff-ffff-ffffffffffff".parse().unwrap();
    let forged = ledger.encode().unwrap();
    std::fs::write(store.path_for(&app_id, SelectedScope::User), &forged).unwrap();
    let request = RuntimeRequest {
        operation: zup_runtime::RuntimeOperation::Deployment(Box::new(
            zup_runtime::RuntimeDeployment {
                plan: uninstall.clone(),

                intent: zup_plan::DeploymentIntent {
                    deployment: zup_plan::DeploymentSelection::from_plan(&uninstall),
                    action: zup_plan::LifecycleAction::Uninstall,
                    enable: Vec::new(),
                    disable: Vec::new(),
                    install_directory: None,
                },
                bootstrap: None,
                release: None,
            },
        )),
        target: target(),
        app_id,
        app_version,
        scope: SelectedScope::User,
        state_root: state_root.clone(),
    };
    let backend = WindowsRuntimeBackend::from_path(payload_root, None).unwrap();
    let outcome = run(&backend, request, ExecutionPolicy::Interactive)
        .await
        .unwrap();
    assert!(matches!(outcome, InstallOutcome::Failed(_)), "{outcome:?}");
    assert_eq!(std::fs::read(&destination).unwrap(), b"installed");
    assert!(
        InstallationRecordStore::new(&state_root)
            .load(&ledger.app_id, SelectedScope::User)
            .is_err()
    );
    assert_eq!(
        std::fs::read(store.path_for(&ledger.app_id, SelectedScope::User)).unwrap(),
        forged
    );
}

#[rstest::rstest]
#[case::versioned(false)]
#[case::hybrid(true)]
#[tokio::test]
async fn apps_features_registration_is_opaque_and_round_trips(#[case] fixed: bool) {
    let root = temp_root();
    let state_root = root.path().join("state");
    let payload_root = root.path().join("payload");
    let install_directory = root.path().join("install");
    std::fs::create_dir_all(&payload_root).unwrap();
    std::fs::write(payload_root.join("maintenance.bin"), b"maintenance").unwrap();
    std::fs::write(payload_root.join("fixed.dat"), b"fixed data").unwrap();
    let app_id = AppId::new(format!(
        "com.zup.apps-roundtrip-{}",
        uuid::Uuid::now_v7().simple()
    ))
    .unwrap();
    let app_id_text = app_id.as_str().to_owned();
    let version = semver::Version::parse("1.0.0").unwrap();
    let target_triple = target();
    let mut target_plan = DesiredState {
        generation_readers: Default::default(),
        data: Vec::new(),
        requirements: (),
        maintenance: None,
        installation: zup_core::InstallationId::new_v7(),
        generation: None,
        app: App {
            id: app_id.clone(),
            name: NonEmptyString::new("Apps Round Trip").unwrap(),
            version: version.clone(),
            publisher: None,
            main: None,
            description: None,
        },
        target: target_triple,
        scope: SelectedScope::User,
        install_directory: target_path(&install_directory),
        selected_components: Vec::new(),
        prerequisites: Vec::new(),
        files: if fixed {
            vec![zup_plan::DesiredFile {
                key: ResourceKey::File {
                    id: zup_core::FileResourceId::Payload {
                        id: zup_core::PayloadId::File {
                            declaration: zup_core::ResourceId::new("fixture-1").unwrap(),
                        },
                    },
                },
                source_relative: RelativePath::new("fixed.dat").unwrap(),
                placement: zup_core::PayloadPlacement::Fixed {
                    path: target_path(&install_directory.join("fixed.dat")),
                    replacement: zup_core::FixedReplacement::Atomic,
                },
                size: 10,
                sha256: digest(b"fixed data"),
                privilege: Privilege::User,
                executable: false,
            }]
        } else {
            Vec::new()
        },
        launchers: Vec::new(),
        path_entries: Vec::new(),
        services: Vec::new(),
        protocols: Vec::new(),
        file_associations: Vec::new(),

        preset: None,
    }
    .bind_fixed()
    .expect("fixed deployment requirements");
    zup_runtime::attach_maintenance_copy(
        &mut target_plan,
        &zup_runtime::ContentAccess {
            installer: &payload_root.join("maintenance.bin"),
            state_root: &state_root,
            scope: SelectedScope::User,
            action: LifecycleAction::Install,
            prior: None,
            maintenance: None,
        },
        "maintenance.bin",
    )
    .unwrap();
    let maintenance = std::path::PathBuf::from(
        target_plan
            .maintenance
            .as_ref()
            .unwrap()
            .maintenance_runtime()
            .unwrap()
            .as_str(),
    );
    let plan = zup_windows::plan_target_lifecycle(
        LifecycleAction::Install,
        &app_id,
        SelectedScope::User,
        Some(&target_plan),
        &state_root,
    )
    .unwrap();
    assert!(
        plan.nodes
            .iter()
            .any(|node| { matches!(node.kind, zup_plan::NodeKind::BackendOperation { .. }) })
    );
    let backend =
        zup_windows::WindowsRuntimeBackend::from_path(payload_root.clone(), None).unwrap();
    let request = RuntimeRequest {
        operation: zup_runtime::RuntimeOperation::Deployment(Box::new(
            zup_runtime::RuntimeDeployment {
                plan: plan.clone(),

                intent: zup_plan::DeploymentIntent {
                    deployment: zup_plan::DeploymentSelection::from_plan(&plan),
                    action: if plan.transition.is_remove() {
                        zup_plan::LifecycleAction::Uninstall
                    } else {
                        zup_plan::LifecycleAction::Install
                    },
                    enable: Vec::new(),
                    disable: Vec::new(),
                    install_directory: None,
                },
                bootstrap: None,
                release: None,
            },
        )),
        target: target_triple,
        app_id: app_id.clone(),
        app_version: version.clone(),
        scope: SelectedScope::User,
        state_root: state_root.clone(),
    };
    let outcome = run(&backend, request, ExecutionPolicy::Interactive)
        .await
        .unwrap();
    assert_eq!(outcome, InstallOutcome::Committed);
    let registration =
        zup_windows::inspect_uninstall_registration(SelectedScope::User, app_id.as_str())
            .unwrap()
            .unwrap();
    assert!(matches!(
        registration.values.get("DisplayName"),
        Some(zup_windows::AppsFeaturesValue::String(value)) if value == "Apps Round Trip"
    ));
    let uninstall = zup_windows::plan_target_lifecycle(
        LifecycleAction::Uninstall,
        &app_id,
        SelectedScope::User,
        None,
        &state_root,
    )
    .unwrap();
    let request = RuntimeRequest {
        operation: zup_runtime::RuntimeOperation::Deployment(Box::new(
            zup_runtime::RuntimeDeployment {
                plan: uninstall.clone(),

                intent: zup_plan::DeploymentIntent {
                    deployment: zup_plan::DeploymentSelection::from_plan(&uninstall),
                    action: zup_plan::LifecycleAction::Uninstall,
                    enable: Vec::new(),
                    disable: Vec::new(),
                    install_directory: None,
                },
                bootstrap: None,
                release: None,
            },
        )),
        target: target_triple,
        app_id,
        app_version: version,
        scope: SelectedScope::User,
        state_root: state_root.clone(),
    };
    let outcome = run(&backend, request, ExecutionPolicy::Interactive)
        .await
        .unwrap();
    assert_eq!(outcome, InstallOutcome::Committed);
    assert!(
        zup_windows::inspect_uninstall_registration(SelectedScope::User, &app_id_text)
            .unwrap()
            .is_none()
    );
    assert!(
        maintenance.exists(),
        "retired generations remain until safe reclamation is established"
    );
}

#[tokio::test]
async fn recovery_of_an_untouched_transaction_needs_no_installer_or_payload() {
    let root = temp_root();
    let (mut request, _) = request(&root, SelectedScope::User);
    let store = FilesystemTransactionStore::private_at(&request.state_root);
    let plan = request.operation.deployment().unwrap().plan.clone();
    let pending = zup_transaction::TransactionCoordinator::new(store.clone())
        .begin(
            request.app_id.clone(),
            request.scope,
            request.app_version.clone(),
            plan,
        )
        .unwrap()
        .preserve();
    std::fs::remove_dir_all(root.path().join("payload")).unwrap();
    let backend = WindowsRuntimeBackend::for_recovery();
    request.app_version = "0.9.0".parse().unwrap();
    request.operation = zup_runtime::RuntimeOperation::Recovery {
        transaction: pending.transaction_id,
    };
    assert_eq!(
        run(&backend, request, ExecutionPolicy::Interactive)
            .await
            .unwrap(),
        InstallOutcome::Recovered {
            settlement: zup_runtime::RecoverySettlement::RolledBack
        }
    );
}
