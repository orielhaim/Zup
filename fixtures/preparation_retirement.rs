use zup_transaction::{MaterializationRecord, TransactionRecord, TransactionStore};

fn temporary() -> tempfile::TempDir {
    #[cfg(windows)]
    {
        tempfile::tempdir_in(std::env::var_os("LOCALAPPDATA").unwrap()).unwrap()
    }
    #[cfg(not(windows))]
    {
        tempfile::tempdir().unwrap()
    }
}

fn recover_native(root: &Path) -> (TransactionRecord, TransactionOutcome) {
    let state = root.join("state");
    let store = FilesystemTransactionStore::private_at(&state);
    let pending = store.load(&store.list_ids().unwrap()[0]).unwrap();
    #[cfg(windows)]
    let mut executor = Executor::new(&state, &root.join("source"), pending.transaction_id);
    #[cfg(not(windows))]
    let mut executor = {
        let mut executor =
            LinuxFileExecutor::new().with_payload(interrupted_payload::InterruptedPayload(
                zup_bundle::DirectoryPayloadSource::new(root.join("source")),
            ));
        executor.set_state_authority(state.clone(), pending.app_id.clone(), pending.scope);
        executor.set_transaction_id(pending.transaction_id);
        executor.register_plan(&pending.plan).unwrap();
        executor
    };
    #[cfg(windows)]
    let publisher = InstallationRecordStore::new(&state);
    #[cfg(not(windows))]
    let publisher = LinuxInstallationStore::new(&state);
    zup_transaction::recover(pending, &store, &mut executor, &publisher).unwrap()
}

fn child(root: &Path, point: &str, recovering: bool) {
    child_at(root, point, recovering, 1);
}

fn child_at(root: &Path, point: &str, recovering: bool, occurrence: usize) {
    #[cfg(windows)]
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    #[cfg(not(windows))]
    let mut command = {
        let mut command = std::process::Command::new("sh");
        command
            .args(["-c", "ulimit -c 0; exec \"$@\"", "zup-preparation-crash"])
            .arg(std::env::current_exe().unwrap());
        command
    };
    let output = command
        .args([
            "--exact",
            std::thread::current().name().unwrap(),
            "--nocapture",
        ])
        .env("ZUP_PREPARATION_TEST_ROOT", root)
        .env(
            "ZUP_PREPARATION_TEST_RECOVER",
            if recovering { "1" } else { "0" },
        )
        .env("ZUP_TEST_FAULT_POINT", point)
        .env("ZUP_TEST_FAULT_ACTION", "crash")
        .env("ZUP_TEST_FAULT_OCCURRENCE", occurrence.to_string())
        .output()
        .unwrap();
    assert!(!output.status.success(), "fault was not reached");
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains("panicked"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn in_child() -> bool {
    let Some(root) = std::env::var_os("ZUP_PREPARATION_TEST_ROOT") else {
        return false;
    };
    let root = std::path::PathBuf::from(root);
    if std::env::var("ZUP_PREPARATION_TEST_RECOVER").unwrap() == "1" {
        recover_native(&root);
    } else {
        let source = root.join("source");
        std::fs::create_dir_all(source.join("build")).unwrap();
        std::fs::write(source.join("build/asset.bin"), b"recoverable").unwrap();
        deploy(
            &root.join("state"),
            &root.join("application"),
            &source,
            b"recoverable",
            None,
        );
    }
    panic!("fault was not reached");
}

fn records(root: &Path) -> Vec<MaterializationRecord> {
    std::fs::read_dir(root.join("state/materializations"))
        .unwrap()
        .flat_map(|installation| std::fs::read_dir(installation.unwrap().path()).unwrap())
        .map(Result::unwrap)
        .filter(|entry| entry.path().extension().is_some_and(|e| e == "json"))
        .map(|entry| serde_json::from_slice(&std::fs::read(entry.path()).unwrap()).unwrap())
        .collect()
}

#[rstest::rstest]
#[case("preparation_retired")]
#[case("preparation_retirement_before")]
#[case("preparation_delete_intent_before")]
#[case("preparation_delete_intent")]
#[case("preparation_remove_before")]
#[case("preparation_remove_after")]
#[case("preparation_parent_synced")]
#[case("preparation_parent_sync_before")]
#[case("preparation_readback_before")]
#[case("preparation_absence_observed")]
#[case("preparation_compaction_before")]
#[case("preparation_compacted")]
fn retirement_crashes_converge_without_accumulating_preparations(#[case] point: &str) {
    if in_child() {
        return;
    }
    let root = temporary();
    child(root.path(), "materialization_stream_interrupted", false);
    for _ in 0..2 {
        child(root.path(), "materialization_stream_interrupted", true);
    }
    child(root.path(), point, true);
    let foreign = root
        .path()
        .join("application/.zup/deployments/.preparing-foreign");
    std::fs::create_dir(&foreign).unwrap();
    std::fs::write(foreign.join("keep"), b"foreign").unwrap();
    let (record, outcome) = recover_native(root.path());
    assert_eq!(
        outcome,
        TransactionOutcome::Committed,
        "{:?}",
        record.recovery_failure
    );
    let metadata = records(root.path());
    #[cfg(windows)]
    assert_eq!(metadata.len(), 2, "application and maintenance evidence");
    #[cfg(not(windows))]
    assert_eq!(metadata.len(), 1);
    for record in &metadata {
        assert!(record.retired_preparations.is_empty());
        assert!(matches!(
            record.state,
            zup_transaction::MaterializationState::Sealed { .. }
        ));
        let namespace = Path::new(record.specification.directory().as_str())
            .parent()
            .unwrap();
        assert!(std::fs::read_dir(namespace).unwrap().all(|e| {
            let name = e.unwrap().file_name();
            !name.to_string_lossy().starts_with(".preparing-") || name == ".preparing-foreign"
        }));
        let seal = record.fingerprint().unwrap();
        let mut changed = record.clone();
        changed
            .retired_preparations
            .push(zup_transaction::RetiredPreparation {
                directory: zup_core::ResourceId::new(format!(
                    ".preparing-{}",
                    MaterializationId::new_v7()
                ))
                .unwrap(),
                object: match record.object {
                    zup_core::NativeObjectIdentity::Windows { volume, mut file } => {
                        file[0] ^= 0xff;
                        zup_core::NativeObjectIdentity::Windows { volume, file }
                    }
                    zup_core::NativeObjectIdentity::Linux { device, inode } => {
                        zup_core::NativeObjectIdentity::Linux {
                            device,
                            inode: inode.wrapping_add(1),
                        }
                    }
                },
                collection: zup_transaction::PreparationCollection::Retired,
            });
        assert_eq!(changed.fingerprint().unwrap(), seal);
        changed.retired_preparations[0].collection =
            zup_transaction::PreparationCollection::Deleting {
                tree: zup_transaction::PreparationTree {
                    directories: Default::default(),
                    files: Default::default(),
                },
            };
        assert_eq!(changed.fingerprint().unwrap(), seal);
        for file in record.specification.descriptor().files() {
            let bytes = std::fs::read(
                Path::new(record.specification.directory().as_str()).join(file.path.as_str()),
            )
            .unwrap();
            assert_eq!(zup_core::hash_bytes(&bytes), file.sha256);
            assert_eq!(bytes.len() as u64, file.size);
        }
    }
    assert_eq!(std::fs::read(foreign.join("keep")).unwrap(), b"foreign");
}

#[rstest::rstest]
#[case::foreign_member("insert")]
#[case::replaced_member("replace_member")]
#[case::replaced_root("replace_root")]
#[case::replaced_namespace("replace_namespace")]
#[case::missing_intent_root("missing")]
#[case::busy_reader("busy")]
#[case::hardlink("hardlink")]
#[case::changed_access("access")]
#[case::directory_link("link")]
fn retirement_refuses_changed_authority(#[case] mutation: &str) {
    if in_child() {
        return;
    }
    let root = temporary();
    child(root.path(), "materialization_stream_interrupted", false);
    child(root.path(), "preparation_delete_intent", true);
    let metadata = records(root.path()).remove(0);
    let retired = &metadata.retired_preparations[0];
    let path = Path::new(metadata.specification.directory().as_str())
        .parent()
        .unwrap()
        .join(retired.directory.as_str());
    let zup_transaction::PreparationCollection::Deleting { tree } = &retired.collection else {
        panic!("missing delete intent");
    };
    let member = path.join(tree.files.keys().next().unwrap().as_str());
    let saved = root.path().join("preserved");
    #[cfg(windows)]
    let mut reader: Option<std::fs::File> = None;
    #[cfg(not(windows))]
    let reader: Option<std::fs::File> = None;
    match mutation {
        "insert" => std::fs::write(path.join("foreign"), b"foreign").unwrap(),
        "replace_member" => {
            std::fs::rename(&member, &saved).unwrap();
            std::fs::write(&member, b"foreign").unwrap();
        }
        "replace_root" => {
            std::fs::rename(&path, &saved).unwrap();
            std::fs::create_dir(&path).unwrap();
            std::fs::write(path.join("foreign"), b"foreign").unwrap();
        }
        "replace_namespace" => {
            let namespace = path.parent().unwrap();
            std::fs::rename(namespace, &saved).unwrap();
            std::fs::create_dir(namespace).unwrap();
            std::fs::write(namespace.join("keep"), b"foreign").unwrap();
        }
        "missing" => {
            std::fs::rename(&path, &saved).unwrap();
        }
        "busy" => {
            #[cfg(windows)]
            {
                reader = Some(std::fs::File::open(&member).unwrap());
            }
            #[cfg(not(windows))]
            {
                return;
            } // Linux private staging has no published consumer contract.
        }
        "hardlink" => std::fs::hard_link(&member, &saved).unwrap(),
        "access" => {
            #[cfg(windows)]
            assert!(
                std::process::Command::new("icacls")
                    .arg(&path)
                    .args(["/grant", "*S-1-1-0:(RX)"])
                    .output()
                    .unwrap()
                    .status
                    .success()
            );
            #[cfg(not(windows))]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
        "link" => {
            std::fs::create_dir(&saved).unwrap();
            std::fs::write(saved.join("keep"), b"foreign").unwrap();
            #[cfg(windows)]
            assert!(
                std::process::Command::new("cmd")
                    .args(["/c", "mklink", "/J"])
                    .arg(path.join("foreign"))
                    .arg(&saved)
                    .output()
                    .unwrap()
                    .status
                    .success()
            );
            #[cfg(not(windows))]
            std::os::unix::fs::symlink(&saved, path.join("foreign")).unwrap();
        }
        _ => unreachable!(),
    }
    let (record, outcome) = recover_native(root.path());
    if mutation == "missing" {
        assert_eq!(
            outcome,
            TransactionOutcome::Committed,
            "{:?}",
            record.recovery_failure
        );
        assert!(saved.exists(), "never search for a moved root");
        return;
    }
    assert_eq!(outcome, TransactionOutcome::RecoveryRequired);
    if mutation == "replace_namespace" {
        assert_eq!(
            std::fs::read(path.parent().unwrap().join("keep")).unwrap(),
            b"foreign"
        );
        return;
    }
    assert!(path.exists());
    if matches!(mutation, "insert" | "replace_root") {
        assert_eq!(std::fs::read(path.join("foreign")).unwrap(), b"foreign");
    }
    if mutation == "replace_member" {
        assert_eq!(std::fs::read(&member).unwrap(), b"foreign");
    }
    if mutation == "link" {
        assert_eq!(std::fs::read(saved.join("keep")).unwrap(), b"foreign");
    }
    if mutation == "busy" {
        drop(reader);
        assert_eq!(recover_native(root.path()).1, TransactionOutcome::Committed);
    }
}

#[test]
fn sealed_receipts_survive_source_free_retirement_collection() {
    #[cfg(windows)]
    use zup_transaction::OperationExecutor;
    if in_child() {
        return;
    }
    let root = temporary();
    child(root.path(), "materialization_stream_interrupted", false);
    child(root.path(), "preparation_delete_intent", true);
    let mut metadata = records(root.path()).remove(0);
    let retired = metadata.retired_preparations.remove(0);
    let retired_path = Path::new(metadata.specification.directory().as_str())
        .parent()
        .unwrap()
        .join(retired.directory.as_str());
    let record_path = root
        .path()
        .join("state/materializations")
        .join(metadata.specification.installation().to_string())
        .join(format!("{}.json", metadata.specification.id()));
    // Model cleanup deferred until after publication, retaining the exact protected native inventory.
    std::fs::write(&record_path, serde_json::to_vec(&metadata).unwrap()).unwrap();
    let (committed, outcome) = recover_native(root.path());
    assert_eq!(outcome, TransactionOutcome::Committed);
    let mut sealed: MaterializationRecord =
        serde_json::from_slice(&std::fs::read(&record_path).unwrap()).unwrap();
    let seal = sealed.fingerprint().unwrap();
    sealed.retired_preparations.push(retired);
    assert_eq!(sealed.fingerprint().unwrap(), seal);
    std::fs::write(&record_path, serde_json::to_vec(&sealed).unwrap()).unwrap();
    std::fs::remove_dir_all(root.path().join("source")).unwrap();
    let mut executor = native_executor(
        &root.path().join("state"),
        &root.path().join("missing"),
        &committed,
    );
    for node in &committed.plan.nodes {
        if let Some(receipt @ zup_transaction::OperationReceipt::Generation(_)) =
            committed.receipt(&node.id)
        {
            executor.verify(node, receipt).unwrap();
        }
    }
    let cleaned: MaterializationRecord =
        serde_json::from_slice(&std::fs::read(&record_path).unwrap()).unwrap();
    assert!(cleaned.retired_preparations.is_empty());
    assert!(!retired_path.exists());
    assert_eq!(cleaned.fingerprint().unwrap(), seal);
}

#[test]
fn competing_coordinator_cannot_retire_or_collect() {
    if in_child() {
        return;
    }
    let root = temporary();
    child(root.path(), "materialization_stream_interrupted", false);
    let before = records(root.path()).remove(0);
    let state = root.path().join("state");
    #[cfg(windows)]
    let lock = {
        let directory = private_fs::PrivateDir::open(
            state
                .join("materializations")
                .join(before.specification.installation().to_string()),
        )
        .unwrap();
        let lock = zup_storage::open_lock(&directory, "preparations.lock").unwrap();
        lock.try_lock().unwrap();
        lock
    };
    #[cfg(not(windows))]
    let lock = zup_transaction::ResourceLock::try_acquire(
        &state,
        &format!("preparations-{}", before.specification.installation()),
    )
    .unwrap()
    .unwrap();
    let (record, outcome) = recover_native(root.path());
    assert_eq!(outcome, TransactionOutcome::RecoveryRequired);
    assert!(
        record
            .recovery_failure
            .unwrap()
            .message
            .contains("coordinator busy")
    );
    assert_eq!(records(root.path()).remove(0), before);
    drop(lock);
    assert_eq!(recover_native(root.path()).1, TransactionOutcome::Committed);
}

#[cfg(windows)]
#[test]
fn maintenance_retirement_preserves_the_already_sealed_application() {
    if in_child() {
        return;
    }
    let root = temporary();
    child_at(root.path(), "materialization_stream_interrupted", false, 4);
    let application = records(root.path())
        .into_iter()
        .find(|r| {
            matches!(
                r.state,
                zup_transaction::MaterializationState::Sealed { .. }
            )
        })
        .unwrap();
    let seal = application.fingerprint().unwrap();
    for _ in 0..2 {
        child(root.path(), "materialization_stream_interrupted", true);
    }
    child(root.path(), "preparation_remove_after", true);
    assert_eq!(recover_native(root.path()).1, TransactionOutcome::Committed);
    for metadata in records(root.path()) {
        assert!(metadata.retired_preparations.is_empty());
        if metadata.specification == application.specification {
            assert_eq!(metadata.fingerprint().unwrap(), seal);
            assert_eq!(
                std::fs::read(
                    Path::new(metadata.specification.directory().as_str()).join("lib/data.txt")
                )
                .unwrap(),
                b"recoverable"
            );
        }
    }
}

#[rstest::rstest]
#[case("preparation_remove_before", 2)]
#[case("preparation_remove_before", 3)]
#[case("preparation_remove_after", 2)]
#[case("preparation_remove_after", 3)]
fn directory_and_root_deletion_crashes_are_recoverable(
    #[case] point: &str,
    #[case] occurrence: usize,
) {
    if in_child() {
        return;
    }
    let root = temporary();
    child(root.path(), "materialization_stream_interrupted", false);
    child_at(root.path(), point, true, occurrence);
    assert_eq!(recover_native(root.path()).1, TransactionOutcome::Committed);
    assert!(
        records(root.path())
            .iter()
            .all(|r| r.retired_preparations.is_empty())
    );
}

#[rstest::rstest]
#[case::reservation("materialization_directory_created")]
#[case::before_delete_intent("preparation_retired")]
fn unreceipted_creation_and_missing_retirement_are_not_adopted(#[case] point: &str) {
    if in_child() {
        return;
    }
    let root = temporary();
    if point == "preparation_retired" {
        child(root.path(), "materialization_stream_interrupted", false);
        child(root.path(), point, true);
    } else {
        child(root.path(), point, false);
    }
    let metadata = records(root.path()).remove(0);
    let namespace = Path::new(metadata.specification.directory().as_str())
        .parent()
        .unwrap();
    let saved = root.path().join("keep");
    if point == "preparation_retired" {
        std::fs::rename(
            namespace.join(metadata.retired_preparations[0].directory.as_str()),
            &saved,
        )
        .unwrap();
    } else {
        let name = metadata.state.directory().unwrap();
        std::fs::write(namespace.join(name.as_str()).join("foreign"), b"foreign").unwrap();
    }
    assert_eq!(
        recover_native(root.path()).1,
        TransactionOutcome::RecoveryRequired
    );
    if point == "preparation_retired" {
        assert!(saved.exists());
    } else {
        assert_eq!(
            std::fs::read(
                namespace
                    .join(metadata.state.directory().unwrap().as_str())
                    .join("foreign")
            )
            .unwrap(),
            b"foreign"
        );
    }
}
