use std::path::{Path, PathBuf};

use tempfile::TempDir;
use zup_bundle::DirectoryPayloadSource;
use zup_core::{RelativePath, Sha256Digest, TargetTriple, hash_reader};
use zup_transaction::{
    FileDelta, FilePrecondition, InstallationLock, LockScope, OperationId, TransactionInput,
    compile_transaction,
};
use zup_windows::{
    NullProgress, WindowsFileExecutor, apply_node, create_durable, reconcile_node, to_host_path,
};

fn target_path(path: impl AsRef<Path>) -> zup_platform::TargetPath {
    zup_platform::TargetPath::new(
        TargetTriple::parse("x86_64-pc-windows-msvc").unwrap(),
        path.as_ref().to_string_lossy(),
    )
    .unwrap()
}

#[test]
fn owned_file_removal_reconciles_and_rolls_back_without_touching_drift() {
    use zup_core::{Privilege, ResourceKey, SelectedScope};
    use zup_transaction::{FileRemoval, FileRemovalKind, NodeKind, ReconcileResult};

    let (dir, _payload, target_root, source) = setup();
    let destination = target_path(target_root.join("owned.bin"));
    std::fs::write(to_host_path(&destination).unwrap(), b"owned").unwrap();
    let key = ResourceKey::File {
        destination: destination.to_string(),
    };
    let mut input = TransactionInput::new(TargetTriple::parse("x86_64-pc-windows-msvc").unwrap());
    input.uninstall = true;
    input.retired_keys.push(key.clone());
    input.removals.push(FileRemoval {
        key: key.clone(),
        kind: FileRemovalKind::RemoveOwned,
        scope: SelectedScope::User,
        privilege: Privilege::User,
        destination: destination.clone(),
        sha256: digest(b"owned"),
        size: 5,
        created_directories: vec![],
    });
    let plan = compile_transaction(&input).unwrap();
    let node = plan
        .nodes
        .iter()
        .find(|node| matches!(node.kind, NodeKind::FileRemoval { .. }))
        .unwrap();
    let exec = WindowsFileExecutor::new(
        source,
        dir.path().join("work"),
        "removal-test".into(),
        TargetTriple::parse("x86_64-pc-windows-msvc")
            .unwrap()
            .executable_suffix(),
        Box::new(NullProgress),
    );
    assert_eq!(
        exec.reconcile_owned_file_removal(node).unwrap(),
        ReconcileResult::NotApplied
    );
    let receipt = exec.apply_owned_file_removal(node).unwrap();
    assert!(!to_host_path(&destination).unwrap().exists());
    assert!(matches!(
        exec.reconcile_owned_file_removal(node).unwrap(),
        ReconcileResult::AppliedWithReceipt(zup_transaction::OperationReceipt::RemoveFile { .. })
    ));
    exec.rollback_transaction_receipt(&receipt).unwrap();
    assert_eq!(
        std::fs::read(to_host_path(&destination).unwrap()).unwrap(),
        b"owned"
    );
    let receipt = exec.apply_owned_file_removal(node).unwrap();
    std::fs::write(to_host_path(&destination).unwrap(), b"user data").unwrap();
    assert!(exec.rollback_transaction_receipt(&receipt).is_err());
    assert_eq!(
        std::fs::read(to_host_path(&destination).unwrap()).unwrap(),
        b"user data"
    );
}

#[test]
fn coordinator_recovers_crash_after_owned_file_removal_before_receipt() {
    use zup_core::{AppId, Privilege, ResourceKey, SelectedScope};
    use zup_transaction::{
        FileRemoval, FileRemovalKind, FilesystemTransactionStore, NodeKind, NodeState,
        OperationExecutor, ReconcileResult, TransactionId, TransactionOutcome, TransactionPhase,
        TransactionRecord, TransactionStore, recover,
    };

    struct RemovalExecutor(WindowsFileExecutor<DirectoryPayloadSource>);
    impl OperationExecutor for RemovalExecutor {
        type Error = String;
        fn prepare(&mut self, _node: &zup_transaction::TransactionNode) -> Result<(), Self::Error> {
            Ok(())
        }
        fn apply(
            &mut self,
            node: &zup_transaction::TransactionNode,
        ) -> Result<zup_transaction::OperationReceipt, Self::Error> {
            if matches!(node.kind, zup_transaction::NodeKind::Barrier) {
                return Ok(zup_transaction::OperationReceipt::Control);
            }
            self.0
                .apply_owned_file_removal(node)
                .map_err(|error| error.to_string())
        }
        fn verify(
            &mut self,
            _node: &zup_transaction::TransactionNode,
            receipt: &zup_transaction::OperationReceipt,
        ) -> Result<(), Self::Error> {
            zup_windows::verify_installed_file(receipt).map_err(|error| error.to_string())
        }
        fn rollback(
            &mut self,
            _: &zup_transaction::TransactionNode,
            receipt: &zup_transaction::OperationReceipt,
        ) -> Result<(), Self::Error> {
            self.0
                .rollback_transaction_receipt(receipt)
                .map_err(|error| error.to_string())
        }
        fn reconcile(
            &mut self,
            node: &zup_transaction::TransactionNode,
            _: Option<&zup_transaction::OperationReceipt>,
        ) -> Result<ReconcileResult, Self::Error> {
            self.0
                .reconcile_owned_file_removal(node)
                .map_err(|error| error.to_string())
        }
    }
    let (dir, _, target_root, source) = setup();
    let destination = target_path(target_root.join("app.exe"));
    std::fs::write(to_host_path(&destination).unwrap(), b"owned").unwrap();
    let key = ResourceKey::File {
        destination: destination.to_string(),
    };
    let mut input = TransactionInput::new(TargetTriple::parse("x86_64-pc-windows-msvc").unwrap());
    input.uninstall = true;
    input.retired_keys.push(key.clone());
    input.removals.push(FileRemoval {
        key,
        kind: FileRemovalKind::RemoveOwned,
        scope: SelectedScope::User,
        privilege: Privilege::User,
        destination: destination.clone(),
        sha256: digest(b"owned"),
        size: 5,
        created_directories: vec![],
    });
    let plan = compile_transaction(&input).unwrap();
    let store = FilesystemTransactionStore::new(dir.path());
    let mut record = TransactionRecord::new(
        TransactionId::new_v7(),
        AppId::new("com.zup.crash-removal").unwrap(),
        SelectedScope::User,
        "1.0.0".parse().unwrap(),
        plan,
    );
    store.create(&record).unwrap();
    let node = record
        .plan
        .nodes
        .iter()
        .find(|node| matches!(node.kind, NodeKind::FileRemoval { .. }))
        .unwrap()
        .clone();
    let mut executor = RemovalExecutor(WindowsFileExecutor::new(
        source,
        dir.path().join("work"),
        record.transaction_id.to_string(),
        TargetTriple::parse("x86_64-pc-windows-msvc")
            .unwrap()
            .executable_suffix(),
        Box::new(NullProgress),
    ));
    record.phase = TransactionPhase::Applying;
    record.nodes.insert(node.id.clone(), NodeState::Running);
    let revision = record.revision;
    record.touch();
    store.compare_and_swap(revision, &record).unwrap();
    executor.0.apply_owned_file_removal(&node).unwrap();
    assert!(!to_host_path(&destination).unwrap().exists());
    let (record, outcome) = recover(record, &store, &mut executor).unwrap();
    assert_eq!(outcome, TransactionOutcome::Committed);
    assert!(
        matches!(
            record.receipt(&node.id),
            Some(zup_transaction::OperationReceipt::RemoveFile { .. })
        ),
        "the reconciled removal is journaled with its receipt"
    );
    assert!(
        matches!(record.nodes.get(&node.id), Some(NodeState::Verified { .. })),
        "a committed removal is verified"
    );
    assert!(!to_host_path(&destination).unwrap().exists());
}

fn digest(bytes: &[u8]) -> Sha256Digest {
    hash_reader(bytes).unwrap().1
}

fn setup() -> (TempDir, PathBuf, PathBuf, DirectoryPayloadSource) {
    let dir = TempDir::new().unwrap();
    let payload_root = dir.path().join("payload");
    let target_root = dir.path().join("target");
    let work_root = dir.path().join("work");
    std::fs::create_dir_all(&payload_root).unwrap();
    std::fs::create_dir_all(&target_root).unwrap();
    std::fs::create_dir_all(&work_root).unwrap();
    let src = DirectoryPayloadSource::new(&payload_root);
    (dir, payload_root, target_root, src)
}

fn stage_op(id: &str) -> OperationId {
    OperationId::new(format!(
        "stage-file:file:{{\"file\":{{\"destination\":\"{id}\"}}}}"
    ))
}

#[test]
fn create_durable_refuses_overwrite() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("f.bin");
    create_durable(&path, b"first").unwrap();
    let err = create_durable(&path, b"second").unwrap_err();
    assert!(matches!(err, zup_windows::DurableError::Win32 { .. }));
    assert_eq!(std::fs::read(&path).unwrap(), b"first");
}

#[test]
fn installation_lock_exclusive() {
    let dir = TempDir::new().unwrap();
    let key = InstallationLock::lock_key("com.acme.acme", "user");
    let first = InstallationLock::try_acquire(dir.path(), &key)
        .unwrap()
        .expect("first lock");
    let second = InstallationLock::try_acquire(dir.path(), &key).unwrap();
    assert!(second.is_none(), "second must be busy");
    drop(first);
    let third = InstallationLock::try_acquire(dir.path(), &key).unwrap();
    assert!(third.is_some(), "lock released on drop");
}

#[test]
fn lock_identity_separates_installations_and_joins_the_ones_that_are_one() {
    let acme = InstallationLock::lock_key("com.acme.desktop", "user");
    let other = InstallationLock::lock_key("com.other.desktop", "user");
    let machine = InstallationLock::lock_key("com.acme.desktop", "machine");
    assert_ne!(acme, other, "two applications are two installations");
    assert_ne!(
        acme, machine,
        "a user install and a machine install are separate installations with \
         separate ledgers, separate directories and separate uninstall entries"
    );

    let bootstrap = InstallationLock::key_for("com.acme.desktop", "user", LockScope::Bootstrap);
    let lifecycle = InstallationLock::key_for("com.acme.desktop", "user", LockScope::Lifecycle);
    assert_eq!(
        lifecycle, acme,
        "one spelling of the lifecycle key, not two"
    );
    assert_ne!(
        bootstrap, lifecycle,
        "a bootstrap and a transaction must not hold the same lock"
    );
    assert!(
        bootstrap.starts_with("zup-bootstrap-"),
        "a bootstrap key says so, because a parent and a worker read each other's: {bootstrap}"
    );
    assert!(
        InstallationLock::key_for("com.other.desktop", "user", LockScope::Bootstrap) != bootstrap,
        "two applications are two installations, bootstrap phase included"
    );

    let awkward = InstallationLock::lock_key("com.acme.desktop/../../etc", "user");
    assert!(!awkward.contains('/'), "{awkward}");
    assert!(!awkward.contains(".."), "{awkward}");
}

#[test]
fn a_lock_marker_is_removed_only_when_nobody_holds_it() {
    let dir = TempDir::new().unwrap();
    let key = InstallationLock::lock_key("com.acme.desktop", "user");
    let held = InstallationLock::try_acquire(dir.path(), &key)
        .unwrap()
        .expect("first lock");
    InstallationLock::remove_if_unheld(dir.path(), &key).unwrap();
    assert!(
        dir.path().join(format!("{key}.lock")).exists(),
        "a held lock's marker is not somebody else's to delete"
    );
    drop(held);
    InstallationLock::remove_if_unheld(dir.path(), &key).unwrap();
    assert!(!dir.path().join(format!("{key}.lock")).exists());

    InstallationLock::remove_if_unheld(dir.path(), &key).unwrap();
}

#[test]
fn a_file_mutation_produces_a_receipt_that_names_what_it_replaced() {
    let rel = RelativePath::new("a.bin").unwrap();

    let (_dir, payload_root, target_root, src) = setup();
    std::fs::write(payload_root.join("a.bin"), b"hello").unwrap();
    let dest = target_root.join("a.bin");
    let mut exec = WindowsFileExecutor::new(
        src,
        target_root.join("work"),
        "tx1".into(),
        TargetTriple::parse("x86_64-pc-windows-msvc")
            .unwrap()
            .executable_suffix(),
        Box::new(NullProgress),
    );
    let stage = stage_node(&dest);
    exec.note_file(&stage.id, FilePrecondition::Absent, digest(b"hello"), 5);
    apply_node(&mut exec, &stage, &rel, &dest).expect("stage");
    let create = mutation_node("create", FileDelta::Create, &dest);
    exec.note_file(&create.id, FilePrecondition::Absent, digest(b"hello"), 5);
    match apply_node(&mut exec, &create, &rel, &dest).expect("create") {
        zup_windows::OperationReceipt::CreateFile(r) => {
            assert_eq!(r.installed_size, 5);
            assert_eq!(r.installed_sha256, digest(b"hello"));
        }
        other => panic!("expected CreateFile receipt, got {other:?}"),
    }
    assert_eq!(std::fs::read(&dest).unwrap(), b"hello");

    let (_dir, payload_root, target_root, src) = setup();
    std::fs::write(payload_root.join("a.bin"), b"new!").unwrap();
    let dest = target_root.join("a.bin");
    std::fs::write(&dest, b"old!").unwrap();
    let mut exec = WindowsFileExecutor::new(
        src,
        target_root.join("work"),
        "tx2".into(),
        TargetTriple::parse("x86_64-pc-windows-msvc")
            .unwrap()
            .executable_suffix(),
        Box::new(NullProgress),
    );
    let stage = stage_node(&dest);
    exec.note_file(&stage.id, FilePrecondition::Absent, digest(b"new!"), 4);
    apply_node(&mut exec, &stage, &rel, &dest).expect("stage");
    let replace = mutation_node("replace", FileDelta::Replace, &dest);
    exec.note_file(
        &replace.id,
        FilePrecondition::Exact {
            size: 4,
            sha256: digest(b"old!"),
        },
        digest(b"new!"),
        4,
    );
    match apply_node(&mut exec, &replace, &rel, &dest).expect("replace") {
        zup_windows::OperationReceipt::ReplaceFile(r) => {
            assert_eq!(r.previous_sha256, digest(b"old!"));
            assert_eq!(r.new_sha256, digest(b"new!"));
            let backup = std::path::PathBuf::from(&r.backup_path);
            assert_eq!(std::fs::read(backup).unwrap(), b"old!");
        }
        other => panic!("expected ReplaceFile receipt, got {other:?}"),
    }
    assert_eq!(std::fs::read(&dest).unwrap(), b"new!");
}

fn stage_node(dest: &Path) -> zup_transaction::TransactionNode {
    zup_transaction::TransactionNode {
        id: stage_op("a"),
        phase: zup_transaction::Phase::Stage,
        kind: zup_transaction::NodeKind::StageFile {
            key: zup_core::ResourceKey::File {
                destination: dest.display().to_string(),
            },
        },
        declaration_order: 1,
        meta: zup_transaction::NodeMeta::default(),
    }
}

fn mutation_node(verb: &str, delta: FileDelta, dest: &Path) -> zup_transaction::TransactionNode {
    let key = zup_core::ResourceKey::File {
        destination: dest.display().to_string(),
    };
    zup_transaction::TransactionNode {
        id: OperationId::resource(verb, &key),
        phase: zup_transaction::Phase::FileMutation,
        kind: zup_transaction::NodeKind::FileMutation { key, delta },
        declaration_order: 2,
        meta: zup_transaction::NodeMeta::default(),
    }
}

#[test]
fn a_target_that_changed_after_the_plan_is_refused_and_left_alone() {
    for (delta, on_disk, expected) in [
        (
            FileDelta::Create,
            b"foreign".as_slice(),
            b"foreign".as_slice(),
        ),
        (
            FileDelta::Replace,
            b"CHANGED".as_slice(),
            b"CHANGED".as_slice(),
        ),
    ] {
        let (_dir, payload_root, target_root, src) = setup();
        std::fs::write(payload_root.join("a.bin"), b"new!").unwrap();
        let dest = target_root.join("a.bin");
        std::fs::write(&dest, on_disk).unwrap();

        let mut exec = WindowsFileExecutor::new(
            src,
            target_root.join("work"),
            "tx-drift".into(),
            TargetTriple::parse("x86_64-pc-windows-msvc")
                .unwrap()
                .executable_suffix(),
            Box::new(NullProgress),
        );
        let rel = RelativePath::new("a.bin").unwrap();
        let op = mutation_node("drift", delta, &dest);

        exec.note_file(
            &op.id,
            FilePrecondition::Exact {
                size: 4,
                sha256: digest(b"old!"),
            },
            digest(b"new!"),
            4,
        );
        let error = apply_node(&mut exec, &op, &rel, &dest).unwrap_err();
        assert!(matches!(
            error,
            zup_windows::WindowsFileExecutorError::PlanDrift { .. }
        ));
        assert_eq!(
            std::fs::read(&dest).unwrap(),
            expected,
            "must not overwrite"
        );
    }
}

#[test]
fn reconcile_absent_and_applied() {
    let dir = TempDir::new().unwrap();
    let dest = dir.path().join("f.bin");
    let op = zup_transaction::TransactionNode {
        id: OperationId::new("create"),
        phase: zup_transaction::Phase::FileMutation,
        kind: zup_transaction::NodeKind::Barrier,
        declaration_order: 0,
        meta: zup_transaction::NodeMeta::default(),
    };
    assert_eq!(
        reconcile_node(&op, &dest, (digest(b"x"), 1)),
        zup_transaction::ReconcileResult::NotApplied
    );
    std::fs::write(&dest, b"x").unwrap();
    assert_eq!(
        reconcile_node(&op, &dest, (digest(b"x"), 1)),
        zup_transaction::ReconcileResult::Applied
    );
    std::fs::write(&dest, b"y").unwrap();
    assert_eq!(
        reconcile_node(&op, &dest, (digest(b"x"), 1)),
        zup_transaction::ReconcileResult::Ambiguous
    );
}

#[test]
fn verification_follows_every_part_of_a_receipt() {
    use zup_windows::verify_installed_file;

    let (_dir, payload_root, target_root, src) = setup();
    std::fs::write(payload_root.join("a.bin"), b"hello").unwrap();
    let dest = target_root.join("a.bin");
    let mut exec = WindowsFileExecutor::new(
        src,
        target_root.join("work"),
        "tx-verify".into(),
        TargetTriple::parse("x86_64-pc-windows-msvc")
            .unwrap()
            .executable_suffix(),
        Box::new(NullProgress),
    );
    let rel = RelativePath::new("a.bin").unwrap();
    let stage = stage_node(&dest);
    exec.note_file(&stage.id, FilePrecondition::Absent, digest(b"hello"), 5);
    let stage_receipt = zup_windows::transaction_receipt(
        apply_node(&mut exec, &stage, &rel, &dest).expect("stage"),
    );
    let zup_transaction::OperationReceipt::StageFile { staged_path, .. } = &stage_receipt else {
        panic!("expected a stage receipt");
    };
    let staged_path = PathBuf::from(staged_path);
    verify_installed_file(&stage_receipt).expect("staged payload matches");

    let create = mutation_node("create", FileDelta::Create, &dest);
    exec.note_file(&create.id, FilePrecondition::Absent, digest(b"hello"), 5);
    let created = zup_windows::transaction_receipt(
        apply_node(&mut exec, &create, &rel, &dest).expect("create"),
    );
    verify_installed_file(&created).expect("installed file matches its receipt");
    assert!(
        verify_installed_file(&stage_receipt).is_err(),
        "a published payload is not still staged"
    );
    assert!(!staged_path.exists());
    std::fs::write(&dest, b"HELLO").unwrap();
    assert!(matches!(
        verify_installed_file(&created).unwrap_err(),
        zup_windows::WindowsFileExecutorError::Verification { .. }
    ));
    std::fs::remove_file(&dest).unwrap();
    assert!(
        verify_installed_file(&created).is_err(),
        "a missing installed file is not verified"
    );

    let (_dir, payload_root, target_root, src) = setup();
    std::fs::write(payload_root.join("a.bin"), b"new!").unwrap();
    let dest = target_root.join("a.bin");
    std::fs::write(&dest, b"old!").unwrap();
    let mut exec = WindowsFileExecutor::new(
        src,
        target_root.join("work"),
        "tx-verify-replace".into(),
        TargetTriple::parse("x86_64-pc-windows-msvc")
            .unwrap()
            .executable_suffix(),
        Box::new(NullProgress),
    );
    let stage = stage_node(&dest);
    exec.note_file(&stage.id, FilePrecondition::Absent, digest(b"new!"), 4);
    apply_node(&mut exec, &stage, &rel, &dest).expect("stage");
    let replace = mutation_node("replace", FileDelta::Replace, &dest);
    exec.note_file(
        &replace.id,
        FilePrecondition::Exact {
            size: 4,
            sha256: digest(b"old!"),
        },
        digest(b"new!"),
        4,
    );
    let replaced = zup_windows::transaction_receipt(
        apply_node(&mut exec, &replace, &rel, &dest).expect("replace"),
    );
    verify_installed_file(&replaced).expect("replace matches its receipt");

    let zup_transaction::OperationReceipt::ReplaceFile { backup_path, .. } = &replaced else {
        panic!("expected a replace receipt");
    };
    let backup = std::path::PathBuf::from(backup_path);
    std::fs::write(&backup, b"zzzz").unwrap();
    assert!(
        verify_installed_file(&replaced).is_err(),
        "a changed backup fails verification"
    );
    std::fs::write(&backup, b"old!").unwrap();
    std::fs::write(&dest, b"nope").unwrap();
    assert!(
        verify_installed_file(&replaced).is_err(),
        "a changed installed file fails verification"
    );
}

#[test]
fn removal_verification_requires_absent_destination_and_intact_backup() {
    use zup_core::{Privilege, ResourceKey, SelectedScope};
    use zup_transaction::{FileRemoval, FileRemovalKind, NodeKind};
    use zup_windows::verify_installed_file;

    let (dir, _payload, target_root, source) = setup();
    let destination = target_path(target_root.join("owned.bin"));
    std::fs::write(to_host_path(&destination).unwrap(), b"owned").unwrap();
    let key = ResourceKey::File {
        destination: destination.to_string(),
    };
    let mut input = TransactionInput::new(TargetTriple::parse("x86_64-pc-windows-msvc").unwrap());
    input.uninstall = true;
    input.retired_keys.push(key.clone());
    input.removals.push(FileRemoval {
        key: key.clone(),
        kind: FileRemovalKind::RemoveOwned,
        scope: SelectedScope::User,
        privilege: Privilege::User,
        destination: destination.clone(),
        sha256: digest(b"owned"),
        size: 5,
        created_directories: vec![],
    });
    let plan = compile_transaction(&input).unwrap();
    let node = plan
        .nodes
        .iter()
        .find(|node| matches!(node.kind, NodeKind::FileRemoval { .. }))
        .unwrap();
    let exec = WindowsFileExecutor::new(
        source,
        dir.path().join("work"),
        "removal-verify".into(),
        TargetTriple::parse("x86_64-pc-windows-msvc")
            .unwrap()
            .executable_suffix(),
        Box::new(NullProgress),
    );
    let receipt = exec.apply_owned_file_removal(node).unwrap();
    verify_installed_file(&receipt).expect("removal matches its receipt");

    std::fs::write(to_host_path(&destination).unwrap(), b"owned").unwrap();
    assert!(
        verify_installed_file(&receipt).is_err(),
        "a reappeared destination fails verification"
    );
    std::fs::remove_file(to_host_path(&destination).unwrap()).unwrap();

    let zup_transaction::OperationReceipt::RemoveFile { backup_path, .. } = &receipt else {
        panic!("expected a removal receipt");
    };
    std::fs::write(to_host_path(backup_path).unwrap(), b"zzz").unwrap();
    assert!(
        verify_installed_file(&receipt).is_err(),
        "a changed backup fails verification"
    );
}
