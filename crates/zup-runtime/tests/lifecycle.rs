use tempfile::TempDir;
use zup_core::{
    App, AppId, FileExtension, FileTypeId, NonEmptyString, Privilege, ProtocolScheme, RelativePath,
    ResourceKey, SelectedScope, hash_reader,
};
use zup_exec::LifecycleAction;
use zup_platform::{
    CommandSpec, TargetFile, TargetFileType, TargetPath, TargetPathEntry, TargetPlan,
    TargetPlanSummary, TargetProtocol,
};
use zup_runtime::{InstallOutcome, RuntimeRequest, run_local_install};
use zup_windows::{InstallLedgerStore, plan_target_lifecycle};

#[tokio::test]
async fn interrupted_uninstall_recovers_through_runtime_and_publishes_ledger() {
    use zup_bundle::DirectoryPayloadSource;
    use zup_transaction::{
        FilesystemTransactionStore, NodeKind, NodeState, TransactionId, TransactionPhase,
        TransactionRecord, TransactionStore, compile_transaction,
    };
    use zup_windows::{NullProgress, WindowsFileExecutor};

    let root = TempDir::new().unwrap();
    let payload = root.path().join("payload");
    std::fs::create_dir_all(&payload).unwrap();
    std::fs::write(payload.join("app.exe"), b"owned").unwrap();
    let desired = target(&root, "1.0.0", &[("app.exe", b"owned")]);
    assert_eq!(
        transition(&root, LifecycleAction::Install, Some(&desired)).await,
        InstallOutcome::Committed
    );
    let state_root = root.path().join("state");
    let execution = plan_target_lifecycle(
        LifecycleAction::Uninstall,
        &desired.app.id,
        SelectedScope::User,
        None,
        &state_root,
    )
    .unwrap();
    let plan = compile_transaction(&execution).unwrap();
    let store = FilesystemTransactionStore::new(&state_root);
    let mut record = TransactionRecord::new(
        TransactionId::new_v7(),
        desired.app.id.clone(),
        SelectedScope::User,
        desired.app.version.clone(),
        plan,
    );
    store.create(&record).unwrap();
    let node = record
        .plan
        .nodes
        .iter()
        .find(|node| matches!(node.kind, NodeKind::OwnedRemoval { .. }))
        .unwrap()
        .clone();
    record.phase = TransactionPhase::Applying;
    record.nodes.insert(node.id.clone(), NodeState::Running);
    let revision = record.revision;
    record.touch();
    store.compare_and_swap(revision, &record).unwrap();
    WindowsFileExecutor::new(
        DirectoryPayloadSource::new(&payload),
        root.path().join("work"),
        record.transaction_id.to_string(),
        Box::new(NullProgress),
    )
    .apply_owned_file_removal(&node)
    .unwrap();
    assert!(!root.path().join("install/app.exe").exists());

    let result = run_local_install(RuntimeRequest {
        app_id: desired.app.id.clone(),
        app_version: desired.app.version.clone(),
        scope: SelectedScope::User,
        execution_plan: Default::default(),
        state_root: state_root.clone(),
        work_root: root.path().join("work"),
        payload_root: payload,
        payload_overlay_root: None,
        payload_overlay_base_root: None,
        recovery_id: Some(record.transaction_id),
        bootstrap: None,
    })
    .await
    .unwrap()
    .0;
    assert_eq!(result, InstallOutcome::Committed);
    assert!(
        InstallLedgerStore::new(&state_root)
            .load(&desired.app.id, SelectedScope::User)
            .unwrap()
            .is_none()
    );
}

fn target(root: &TempDir, version: &str, files: &[(&str, &[u8])]) -> TargetPlan {
    let install = root.path().join("install");
    TargetPlan {
        app: App {
            id: AppId::new("com.zup.lifecycle-e2e").unwrap(),
            name: NonEmptyString::new("Lifecycle").unwrap(),
            version: version.parse().unwrap(),
            publisher: None,
            main: None,
            description: None,
        },
        scope: SelectedScope::User,
        install_directory: TargetPath::new(install.clone()).unwrap(),
        selected_components: vec![],
        prerequisites: vec![],
        files: files
            .iter()
            .map(|(name, bytes)| {
                let path = TargetPath::new(install.join(name)).unwrap();
                TargetFile {
                    key: ResourceKey::File {
                        destination: path.to_string(),
                    },
                    source_relative: RelativePath::new(name).unwrap(),
                    destination: path,
                    size: bytes.len() as u64,
                    sha256: hash_reader(*bytes).unwrap().1,
                    privilege: Privilege::User,
                }
            })
            .collect(),
        shortcuts: vec![],
        path_entries: vec![],
        services: vec![],
        protocols: vec![],
        file_types: vec![],
        summary: TargetPlanSummary {
            file_count: files.len(),
            install_bytes: files.iter().map(|(_, bytes)| bytes.len() as u64).sum(),
            resource_count: 0,
            requires_elevation: false,
            selected_component_count: 0,
            prerequisite_count: 0,
            download_bytes: 0,
        },
    }
}

async fn transition(
    root: &TempDir,
    action: LifecycleAction,
    target: Option<&TargetPlan>,
) -> InstallOutcome {
    let state_root = root.path().join("state");
    let app_id = AppId::new("com.zup.lifecycle-e2e").unwrap();
    let ledger = InstallLedgerStore::new(&state_root)
        .load(&app_id, SelectedScope::User)
        .unwrap();
    let execution =
        plan_target_lifecycle(action, &app_id, SelectedScope::User, target, &state_root).unwrap();
    let version = target.map_or_else(
        || ledger.unwrap().version,
        |target| target.app.version.clone(),
    );
    let request = RuntimeRequest {
        app_id,
        app_version: version,
        scope: SelectedScope::User,
        execution_plan: execution,
        state_root,
        work_root: root.path().join("work"),
        payload_root: root.path().join("payload"),
        payload_overlay_root: None,
        payload_overlay_base_root: None,
        recovery_id: None,
        bootstrap: None,
    };
    run_local_install(request).await.unwrap().0
}

#[tokio::test]
async fn fresh_install_upgrade_and_uninstall_preserve_user_data() {
    let root = TempDir::new().unwrap();
    let payload = root.path().join("payload");
    std::fs::create_dir_all(&payload).unwrap();
    std::fs::write(payload.join("a.exe"), b"old-a").unwrap();
    std::fs::write(payload.join("b.exe"), b"old-b").unwrap();
    let first = target(&root, "1.0.0", &[("a.exe", b"old-a"), ("b.exe", b"old-b")]);
    assert_eq!(
        transition(&root, LifecycleAction::Install, Some(&first)).await,
        InstallOutcome::Committed
    );
    let install = root.path().join("install");
    std::fs::write(install.join("notes.txt"), b"user data").unwrap();

    std::fs::write(payload.join("a.exe"), b"new-a").unwrap();
    std::fs::write(payload.join("c.exe"), b"new-c").unwrap();
    let second = target(&root, "2.0.0", &[("a.exe", b"new-a"), ("c.exe", b"new-c")]);
    assert_eq!(
        transition(&root, LifecycleAction::Upgrade, Some(&second)).await,
        InstallOutcome::Committed
    );
    assert_eq!(std::fs::read(install.join("a.exe")).unwrap(), b"new-a");
    assert!(!install.join("b.exe").exists());
    assert_eq!(
        std::fs::read(install.join("notes.txt")).unwrap(),
        b"user data"
    );
    let ledger = InstallLedgerStore::new(root.path().join("state"))
        .load(&first.app.id, SelectedScope::User)
        .unwrap()
        .unwrap();
    assert_eq!(ledger.version.to_string(), "2.0.0");
    assert_eq!(ledger.resources.len(), 2);

    assert_eq!(
        transition(&root, LifecycleAction::Uninstall, None).await,
        InstallOutcome::Committed
    );
    assert!(!install.join("a.exe").exists());
    assert!(!install.join("c.exe").exists());
    assert_eq!(
        std::fs::read(install.join("notes.txt")).unwrap(),
        b"user data"
    );
    assert!(
        InstallLedgerStore::new(root.path().join("state"))
            .load(&first.app.id, SelectedScope::User)
            .unwrap()
            .is_none()
    );
    assert!(
        std::fs::read_dir(&install)
            .unwrap()
            .all(|entry| entry.unwrap().file_name() == "notes.txt")
    );
}

#[tokio::test]
async fn modify_adds_and_deselects_component_resources() {
    let root = TempDir::new().unwrap();
    let payload = root.path().join("payload");
    std::fs::create_dir_all(&payload).unwrap();
    std::fs::write(payload.join("core.exe"), b"core").unwrap();
    std::fs::write(payload.join("optional.dat"), b"optional").unwrap();
    let mut first = target(&root, "1.0.0", &[("core.exe", b"core")]);
    first.selected_components = vec![zup_core::ComponentId::new("core").unwrap()];
    assert_eq!(
        transition(&root, LifecycleAction::Install, Some(&first)).await,
        InstallOutcome::Committed
    );
    let mut added = target(
        &root,
        "1.0.0",
        &[("core.exe", b"core"), ("optional.dat", b"optional")],
    );
    added.selected_components = vec![
        zup_core::ComponentId::new("core").unwrap(),
        zup_core::ComponentId::new("optional").unwrap(),
    ];
    assert_eq!(
        transition(&root, LifecycleAction::Modify, Some(&added)).await,
        InstallOutcome::Committed
    );
    assert!(root.path().join("install/optional.dat").exists());
    let ledger = InstallLedgerStore::new(root.path().join("state"))
        .load(&first.app.id, SelectedScope::User)
        .unwrap()
        .unwrap();
    assert_eq!(ledger.selected_components, added.selected_components);
    assert_eq!(
        transition(&root, LifecycleAction::Modify, Some(&first)).await,
        InstallOutcome::Committed
    );
    assert!(!root.path().join("install/optional.dat").exists());
    let ledger = InstallLedgerStore::new(root.path().join("state"))
        .load(&first.app.id, SelectedScope::User)
        .unwrap()
        .unwrap();
    assert_eq!(ledger.selected_components, first.selected_components);
}

#[tokio::test]
async fn repair_restores_missing_file_and_requires_force_for_changed_digest() {
    let root = TempDir::new().unwrap();
    let payload = root.path().join("payload");
    std::fs::create_dir_all(&payload).unwrap();
    std::fs::write(payload.join("app.exe"), b"owned").unwrap();
    let target = target(&root, "1.0.0", &[("app.exe", b"owned")]);
    assert_eq!(
        transition(&root, LifecycleAction::Install, Some(&target)).await,
        InstallOutcome::Committed
    );
    let path = root.path().join("install/app.exe");
    std::fs::remove_file(&path).unwrap();
    assert_eq!(
        transition(
            &root,
            LifecycleAction::Repair { force_files: false },
            Some(&target)
        )
        .await,
        InstallOutcome::Committed
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"owned");
    std::fs::write(&path, b"user edit").unwrap();
    let execution = plan_target_lifecycle(
        LifecycleAction::Repair { force_files: false },
        &target.app.id,
        SelectedScope::User,
        Some(&target),
        &root.path().join("state"),
    )
    .unwrap();
    assert_eq!(execution.files[0].kind, zup_exec::FileOperationKind::Drift);
    assert_eq!(std::fs::read(&path).unwrap(), b"user edit");
    assert_eq!(
        transition(
            &root,
            LifecycleAction::Repair { force_files: true },
            Some(&target)
        )
        .await,
        InstallOutcome::Committed
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"owned");
}

#[tokio::test]
async fn uninstall_leaves_externally_modified_owned_file() {
    let root = TempDir::new().unwrap();
    let payload = root.path().join("payload");
    std::fs::create_dir_all(&payload).unwrap();
    std::fs::write(payload.join("app.exe"), b"owned").unwrap();
    let desired = target(&root, "1.0.0", &[("app.exe", b"owned")]);
    assert_eq!(
        transition(&root, LifecycleAction::Install, Some(&desired)).await,
        InstallOutcome::Committed
    );
    let path = root.path().join("install/app.exe");
    std::fs::write(&path, b"user edit").unwrap();
    let plan = plan_target_lifecycle(
        LifecycleAction::Uninstall,
        &desired.app.id,
        SelectedScope::User,
        None,
        &root.path().join("state"),
    )
    .unwrap();
    assert_eq!(plan.removals.len(), 1);
    assert_eq!(plan.removals[0].kind, zup_exec::RemovalKind::Drift);
    assert_eq!(
        transition(&root, LifecycleAction::Uninstall, None).await,
        InstallOutcome::Committed
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"user edit");
    assert!(
        InstallLedgerStore::new(root.path().join("state"))
            .load(&desired.app.id, SelectedScope::User)
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn failed_upgrade_rolls_back_and_keeps_previous_ledger() {
    let root = TempDir::new().unwrap();
    let payload = root.path().join("payload");
    std::fs::create_dir_all(&payload).unwrap();
    std::fs::write(payload.join("app.exe"), b"version-one").unwrap();
    let first = target(&root, "1.0.0", &[("app.exe", b"version-one")]);
    assert_eq!(
        transition(&root, LifecycleAction::Install, Some(&first)).await,
        InstallOutcome::Committed
    );
    std::fs::write(payload.join("app.exe"), b"version-two").unwrap();
    std::fs::write(payload.join("later.dat"), b"later").unwrap();
    let second = target(
        &root,
        "2.0.0",
        &[("app.exe", b"version-two"), ("later.dat", b"later")],
    );
    let state_root = root.path().join("state");
    let execution = plan_target_lifecycle(
        LifecycleAction::Upgrade,
        &first.app.id,
        SelectedScope::User,
        Some(&second),
        &state_root,
    )
    .unwrap();
    std::fs::create_dir(root.path().join("install/later.dat")).unwrap();
    let outcome = run_local_install(RuntimeRequest {
        app_id: first.app.id.clone(),
        app_version: second.app.version.clone(),
        scope: SelectedScope::User,
        execution_plan: execution,
        state_root: state_root.clone(),
        work_root: root.path().join("work"),
        payload_root: payload,
        payload_overlay_root: None,
        payload_overlay_base_root: None,
        recovery_id: None,
        bootstrap: None,
    })
    .await
    .unwrap()
    .0;
    assert_eq!(outcome, InstallOutcome::RolledBack);
    assert_eq!(
        std::fs::read(root.path().join("install/app.exe")).unwrap(),
        b"version-one"
    );
    assert!(root.path().join("install/later.dat").is_dir());
    let ledger = InstallLedgerStore::new(&state_root)
        .load(&first.app.id, SelectedScope::User)
        .unwrap()
        .unwrap();
    assert_eq!(ledger.version.to_string(), "1.0.0");
    assert_eq!(ledger.resources.len(), 1);
}

struct RegistryCleanup {
    scheme: String,
    id: String,
    extension: String,
    path_entry: String,
}

impl Drop for RegistryCleanup {
    fn drop(&mut self) {
        use windows_registry::{CURRENT_USER, Type};
        if let Ok(classes) = CURRENT_USER.open("Software\\Classes") {
            for key in [&self.scheme, &self.id, &self.extension] {
                let _ = classes.remove_tree(key);
            }
        }
        if let Ok(file_exts) =
            CURRENT_USER.open(r"Software\Microsoft\Windows\CurrentVersion\Explorer\FileExts")
        {
            let _ = file_exts.remove_tree(&self.extension);
        }
        if let Ok(environment) = CURRENT_USER.open("Environment")
            && let Ok(value) = environment.get_value("Path")
        {
            let ty = value.ty();
            if matches!(ty, Type::String | Type::ExpandString)
                && let Ok(raw) = String::try_from(value)
                && raw.split(';').any(|entry| entry == self.path_entry)
            {
                let kept = raw
                    .split(';')
                    .filter(|entry| *entry != self.path_entry)
                    .collect::<Vec<_>>()
                    .join(";");
                if ty == Type::String {
                    let _ = environment.set_string("Path", &kept);
                } else {
                    let _ = environment.set_expand_string("Path", &kept);
                }
            }
        }
    }
}

#[tokio::test]
async fn uninstall_restores_managed_registry_resources_and_preserves_user_choice() {
    use windows_registry::CURRENT_USER;
    let root = TempDir::new().unwrap();
    let payload = root.path().join("payload");
    std::fs::create_dir_all(&payload).unwrap();
    std::fs::write(payload.join("app.exe"), b"app").unwrap();
    let suffix = uuid::Uuid::now_v7().simple().to_string();
    let scheme = format!("zup-test-{suffix}");
    let id = format!("Zup.Test.{suffix}");
    let extension = format!(".zup{suffix}");
    let path_entry = root.path().join("install/bin").display().to_string();
    let _cleanup = RegistryCleanup {
        scheme: scheme.clone(),
        id: id.clone(),
        extension: extension.clone(),
        path_entry: path_entry.clone(),
    };
    let before_path =
        zup_windows::read_path_value(&zup_windows::WindowsRegistryReader, SelectedScope::User)
            .unwrap();
    let mut desired = target(&root, "1.0.0", &[("app.exe", b"app")]);
    let command = CommandSpec::new(desired.files[0].destination.clone(), vec!["%1".into()]);
    let scheme_id = ProtocolScheme::new(&scheme).unwrap();
    desired.protocols.push(TargetProtocol {
        key: ResourceKey::Protocol {
            scheme: scheme_id.clone(),
        },
        scheme: scheme_id,
        command: command.clone(),
        scope: SelectedScope::User,
        privilege: Privilege::User,
    });
    let file_type_id = FileTypeId::new(&id).unwrap();
    desired.file_types.push(TargetFileType {
        key: ResourceKey::FileType {
            id: file_type_id.clone(),
        },
        extension: FileExtension::new(&extension).unwrap(),
        id: file_type_id,
        description: Some("Zup Test".into()),
        command,
        scope: SelectedScope::User,
        privilege: Privilege::User,
    });
    desired.path_entries.push(TargetPathEntry {
        key: ResourceKey::PathEntry {
            value: path_entry.clone(),
        },
        value: TargetPath::new(root.path().join("install/bin")).unwrap(),
        scope: SelectedScope::User,
        privilege: Privilege::User,
    });
    desired.summary.resource_count = 3;
    let choices = CURRENT_USER
        .create(r"Software\Microsoft\Windows\CurrentVersion\Explorer\FileExts")
        .unwrap();
    choices
        .create(&extension)
        .unwrap()
        .create("UserChoice")
        .unwrap()
        .set_string("ProgId", "Foreign.Document")
        .unwrap();
    assert_eq!(
        transition(&root, LifecycleAction::Install, Some(&desired)).await,
        InstallOutcome::Committed
    );
    let classes = CURRENT_USER.open("Software\\Classes").unwrap();
    assert!(classes.open(&scheme).is_ok());
    assert!(classes.open(&id).is_ok());
    assert!(classes.open(&extension).is_ok());
    assert!(
        zup_windows::read_path_value(&zup_windows::WindowsRegistryReader, SelectedScope::User)
            .unwrap()
            .unwrap()
            .contains(&path_entry)
    );
    assert_eq!(
        choices
            .open(&extension)
            .unwrap()
            .open("UserChoice")
            .unwrap()
            .get_string("ProgId")
            .unwrap(),
        "Foreign.Document"
    );

    assert_eq!(
        transition(&root, LifecycleAction::Uninstall, None).await,
        InstallOutcome::Committed
    );
    assert!(classes.open(&scheme).is_err());
    assert!(classes.open(&id).is_err());
    assert!(classes.open(&extension).is_err());
    let after_path =
        zup_windows::read_path_value(&zup_windows::WindowsRegistryReader, SelectedScope::User)
            .unwrap();
    assert_eq!(after_path, before_path);
    assert_eq!(
        choices
            .open(&extension)
            .unwrap()
            .open("UserChoice")
            .unwrap()
            .get_string("ProgId")
            .unwrap(),
        "Foreign.Document"
    );
}
