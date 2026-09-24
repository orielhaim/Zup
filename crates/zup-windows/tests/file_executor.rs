//! Windows file executor integration tests (temporary trees only).

use std::path::PathBuf;

use tempfile::TempDir;
use zup_bundle::DirectoryPayloadSource;
use zup_core::{RelativePath, Sha256Digest, hash_reader};
use zup_transaction::OperationId;
use zup_windows::{
    FilePrecondition, InstallationLock, NullProgress, WindowsFileExecutor, apply_node,
    create_durable, move_durable, reconcile_node, volume_root, write_durable,
};

#[test]
fn owned_file_removal_reconciles_and_rolls_back_without_touching_drift() {
    use zup_core::{ResourceKey, SelectedScope};
    use zup_exec::{ExecutionPlan, OwnedResource, RemovalKind, RemovalOperation};
    use zup_platform::TargetPath;
    use zup_transaction::{
        NodeKind, OperationReceipt as TransactionReceipt, ReconcileResult, compile_transaction,
    };

    let (dir, _payload, target_root, source) = setup();
    let destination = TargetPath::new(target_root.join("owned.bin")).unwrap();
    std::fs::write(destination.as_path(), b"owned").unwrap();
    let key = ResourceKey::File {
        destination: destination.to_string(),
    };
    let owned = OwnedResource::File {
        destination: destination.clone(),
        source_relative: RelativePath::new("owned.bin").unwrap(),
        sha256: digest(b"owned"),
        size: 5,
        created_directories: vec![],
    };
    let execution = ExecutionPlan {
        uninstall: true,
        removals: vec![RemovalOperation {
            key: key.clone(),
            kind: RemovalKind::RemoveOwned,
            scope: SelectedScope::User,
            owned,
        }],
        ..Default::default()
    };
    let plan = compile_transaction(&execution).unwrap();
    let node = plan
        .nodes
        .iter()
        .find(|node| matches!(node.kind, NodeKind::OwnedRemoval { .. }))
        .unwrap();
    let exec = WindowsFileExecutor::new(
        source,
        dir.path().join("work"),
        "removal-test".into(),
        Box::new(NullProgress),
    );
    assert_eq!(
        exec.reconcile_owned_file_removal(node).unwrap(),
        ReconcileResult::NotApplied
    );
    let receipt = exec.apply_owned_file_removal(node).unwrap();
    assert!(!destination.as_path().exists());
    assert!(matches!(
        exec.reconcile_owned_file_removal(node).unwrap(),
        ReconcileResult::AppliedWithReceipt(TransactionReceipt::RemoveFile { .. })
    ));
    exec.rollback_transaction_receipt(&receipt).unwrap();
    assert_eq!(std::fs::read(destination.as_path()).unwrap(), b"owned");
    let receipt = exec.apply_owned_file_removal(node).unwrap();
    std::fs::write(destination.as_path(), b"user data").unwrap();
    assert!(exec.rollback_transaction_receipt(&receipt).is_err());
    assert_eq!(std::fs::read(destination.as_path()).unwrap(), b"user data");
}

#[test]
fn coordinator_recovers_crash_after_owned_file_removal_before_receipt() {
    use zup_core::{AppId, ResourceKey, SelectedScope};
    use zup_exec::{ExecutionPlan, OwnedResource, RemovalKind, RemovalOperation};
    use zup_platform::TargetPath;
    use zup_transaction::{
        FilesystemTransactionStore, NodeKind, NodeState, OperationExecutor,
        OperationReceipt as TransactionReceipt, ReconcileResult, TransactionId, TransactionOutcome,
        TransactionPhase, TransactionRecord, TransactionStore, compile_transaction, recover,
    };

    struct RemovalExecutor(WindowsFileExecutor<DirectoryPayloadSource>);
    impl OperationExecutor for RemovalExecutor {
        type Error = String;
        fn apply(
            &mut self,
            node: &zup_transaction::TransactionNode,
        ) -> Result<TransactionReceipt, Self::Error> {
            self.0
                .apply_owned_file_removal(node)
                .map_err(|error| error.to_string())
        }
        fn rollback(
            &mut self,
            _: &zup_transaction::TransactionNode,
            receipt: &TransactionReceipt,
        ) -> Result<(), Self::Error> {
            self.0
                .rollback_transaction_receipt(receipt)
                .map_err(|error| error.to_string())
        }
        fn reconcile(
            &mut self,
            node: &zup_transaction::TransactionNode,
            _: Option<&TransactionReceipt>,
        ) -> Result<ReconcileResult, Self::Error> {
            self.0
                .reconcile_owned_file_removal(node)
                .map_err(|error| error.to_string())
        }
    }
    let (dir, _, target_root, source) = setup();
    let destination = TargetPath::new(target_root.join("app.exe")).unwrap();
    std::fs::write(destination.as_path(), b"owned").unwrap();
    let key = ResourceKey::File {
        destination: destination.to_string(),
    };
    let plan = compile_transaction(&ExecutionPlan {
        uninstall: true,
        removals: vec![RemovalOperation {
            key,
            kind: RemovalKind::RemoveOwned,
            scope: SelectedScope::User,
            owned: OwnedResource::File {
                destination: destination.clone(),
                source_relative: RelativePath::new("app.exe").unwrap(),
                sha256: digest(b"owned"),
                size: 5,
                created_directories: vec![],
            },
        }],
        ..Default::default()
    })
    .unwrap();
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
        .find(|node| matches!(node.kind, NodeKind::OwnedRemoval { .. }))
        .unwrap()
        .clone();
    let mut executor = RemovalExecutor(WindowsFileExecutor::new(
        source,
        dir.path().join("work"),
        record.transaction_id.to_string(),
        Box::new(NullProgress),
    ));
    record.phase = TransactionPhase::Applying;
    record.nodes.insert(node.id.clone(), NodeState::Running);
    let revision = record.revision;
    record.touch();
    store.compare_and_swap(revision, &record).unwrap();
    executor.0.apply_owned_file_removal(&node).unwrap();
    assert!(!destination.as_path().exists());
    let (record, outcome) = recover(record, &store, &mut executor).unwrap();
    assert_eq!(outcome, TransactionOutcome::Committed);
    assert!(
        matches!(record.nodes.get(&node.id), Some(NodeState::Applied { receipt }) if matches!(receipt.as_ref(), TransactionReceipt::RemoveFile { .. }))
    );
    assert!(!destination.as_path().exists());
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
fn durable_write_then_read() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("j.json");
    write_durable(&path, b"{\"a\":1}").expect("write_durable");
    assert_eq!(std::fs::read(&path).unwrap(), b"{\"a\":1}");
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
fn move_durable_publishes() {
    let dir = TempDir::new().unwrap();
    let a = dir.path().join("a.bin");
    let b = dir.path().join("b.bin");
    std::fs::write(&a, b"payload").unwrap();
    move_durable(&a, &b).unwrap();
    assert!(b.exists());
    assert!(!a.exists());
}

#[test]
fn move_durable_accepts_embedded_separator_path() {
    let dir = TempDir::new().unwrap();
    let source = dir.path().join("work/a.bin");
    let destination = dir.path().join("target/b.bin");
    std::fs::create_dir_all(source.parent().unwrap()).unwrap();
    std::fs::create_dir_all(destination.parent().unwrap()).unwrap();
    std::fs::write(&source, b"payload").unwrap();
    move_durable(&source, &destination).unwrap();
    assert_eq!(std::fs::read(destination).unwrap(), b"payload");
}

#[test]
fn volume_root_is_absolute() {
    let dir = TempDir::new().unwrap();
    let v = volume_root(dir.path()).unwrap();
    assert!(v.is_absolute());
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
fn fresh_install_create() {
    let (_dir, payload_root, target_root, src) = setup();
    std::fs::write(payload_root.join("a.bin"), b"hello").unwrap();

    let dest = target_root.join("a.bin");
    let op_id = OperationId::resource(
        "create",
        &zup_core::ResourceKey::File {
            destination: dest.display().to_string(),
        },
    );
    let mut exec = WindowsFileExecutor::new(
        src,
        target_root.join("work"),
        "tx1".into(),
        Box::new(NullProgress),
    );
    exec.note_file(&op_id, FilePrecondition::Absent, digest(b"hello"), 5);
    exec.note_file(
        &stage_op("a"),
        FilePrecondition::Absent,
        digest(b"hello"),
        5,
    );

    // Stage then create via apply_node.
    let stage = zup_transaction::TransactionNode {
        id: stage_op("a"),
        phase: zup_transaction::Phase::Stage,
        kind: zup_transaction::NodeKind::StageFile {
            key: zup_core::ResourceKey::File {
                destination: dest.display().to_string(),
            },
        },
        declaration_order: 1,
        meta: zup_transaction::NodeMeta::default(),
    };
    // Register stage desired state.
    exec.note_file(&stage.id, FilePrecondition::Absent, digest(b"hello"), 5);

    let rel = RelativePath::new("a.bin").unwrap();
    let receipt = apply_node(&mut exec, &stage, &rel, &dest).expect("stage");
    assert!(matches!(
        receipt,
        zup_windows::OperationReceipt::StageFile(_)
    ));

    let create = zup_transaction::TransactionNode {
        id: op_id.clone(),
        phase: zup_transaction::Phase::FileMutation,
        kind: zup_transaction::NodeKind::FileMutation {
            key: zup_core::ResourceKey::File {
                destination: dest.display().to_string(),
            },
            delta: zup_exec::Delta::Create,
        },
        declaration_order: 2,
        meta: zup_transaction::NodeMeta::default(),
    };
    exec.note_file(&create.id, FilePrecondition::Absent, digest(b"hello"), 5);
    let receipt = apply_node(&mut exec, &create, &rel, &dest).expect("create");
    match receipt {
        zup_windows::OperationReceipt::CreateFile(r) => {
            assert_eq!(r.installed_size, 5);
            assert_eq!(r.installed_sha256, digest(b"hello"));
        }
        other => panic!("expected CreateFile receipt, got {other:?}"),
    }
    assert_eq!(std::fs::read(&dest).unwrap(), b"hello");
}

#[test]
fn update_replace_keeps_backup() {
    let (_dir, payload_root, target_root, src) = setup();
    std::fs::write(payload_root.join("a.bin"), b"new!").unwrap();
    let dest = target_root.join("a.bin");
    std::fs::write(&dest, b"old!").unwrap();

    let op_id = OperationId::resource(
        "replace",
        &zup_core::ResourceKey::File {
            destination: dest.display().to_string(),
        },
    );
    let mut exec = WindowsFileExecutor::new(
        src,
        target_root.join("work"),
        "tx2".into(),
        Box::new(NullProgress),
    );
    let rel = RelativePath::new("a.bin").unwrap();

    let stage = zup_transaction::TransactionNode {
        id: stage_op("a"),
        phase: zup_transaction::Phase::Stage,
        kind: zup_transaction::NodeKind::StageFile {
            key: zup_core::ResourceKey::File {
                destination: dest.display().to_string(),
            },
        },
        declaration_order: 1,
        meta: zup_transaction::NodeMeta::default(),
    };
    exec.note_file(&stage.id, FilePrecondition::Absent, digest(b"new!"), 4);
    apply_node(&mut exec, &stage, &rel, &dest).expect("stage");

    let replace = zup_transaction::TransactionNode {
        id: op_id.clone(),
        phase: zup_transaction::Phase::FileMutation,
        kind: zup_transaction::NodeKind::FileMutation {
            key: zup_core::ResourceKey::File {
                destination: dest.display().to_string(),
            },
            delta: zup_exec::Delta::Replace,
        },
        declaration_order: 2,
        meta: zup_transaction::NodeMeta::default(),
    };
    exec.note_file(
        &replace.id,
        FilePrecondition::Exact {
            size: 4,
            sha256: digest(b"old!"),
        },
        digest(b"new!"),
        4,
    );
    let receipt = apply_node(&mut exec, &replace, &rel, &dest).expect("replace");
    match receipt {
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

#[test]
fn plan_drift_create_when_target_appears() {
    let (_dir, payload_root, target_root, src) = setup();
    std::fs::write(payload_root.join("a.bin"), b"x").unwrap();
    let dest = target_root.join("a.bin");
    // Target appears after snapshot.
    std::fs::write(&dest, b"foreign").unwrap();

    let mut exec = WindowsFileExecutor::new(
        src,
        target_root.join("work"),
        "tx3".into(),
        Box::new(NullProgress),
    );
    let rel = RelativePath::new("a.bin").unwrap();
    let op = zup_transaction::TransactionNode {
        id: OperationId::new("create"),
        phase: zup_transaction::Phase::FileMutation,
        kind: zup_transaction::NodeKind::FileMutation {
            key: zup_core::ResourceKey::File {
                destination: dest.display().to_string(),
            },
            delta: zup_exec::Delta::Create,
        },
        declaration_order: 1,
        meta: zup_transaction::NodeMeta::default(),
    };
    exec.note_file(&op.id, FilePrecondition::Absent, digest(b"x"), 1);
    let err = apply_node(&mut exec, &op, &rel, &dest).unwrap_err();
    assert!(matches!(
        err,
        zup_windows::WindowsFileExecutorError::PlanDrift { .. }
    ));
    assert_eq!(
        std::fs::read(&dest).unwrap(),
        b"foreign",
        "must not overwrite"
    );
}

#[test]
fn plan_drift_replace_when_target_changed() {
    let (_dir, payload_root, target_root, src) = setup();
    std::fs::write(payload_root.join("a.bin"), b"new!").unwrap();
    let dest = target_root.join("a.bin");
    std::fs::write(&dest, b"CHANGED").unwrap(); // not the expected old content

    let mut exec = WindowsFileExecutor::new(
        src,
        target_root.join("work"),
        "tx4".into(),
        Box::new(NullProgress),
    );
    let rel = RelativePath::new("a.bin").unwrap();
    let op = zup_transaction::TransactionNode {
        id: OperationId::new("replace"),
        phase: zup_transaction::Phase::FileMutation,
        kind: zup_transaction::NodeKind::FileMutation {
            key: zup_core::ResourceKey::File {
                destination: dest.display().to_string(),
            },
            delta: zup_exec::Delta::Replace,
        },
        declaration_order: 1,
        meta: zup_transaction::NodeMeta::default(),
    };
    exec.note_file(
        &op.id,
        FilePrecondition::Exact {
            size: 4,
            sha256: digest(b"old!"),
        },
        digest(b"new!"),
        4,
    );
    let err = apply_node(&mut exec, &op, &rel, &dest).unwrap_err();
    assert!(matches!(
        err,
        zup_windows::WindowsFileExecutorError::PlanDrift { .. }
    ));
    assert_eq!(std::fs::read(&dest).unwrap(), b"CHANGED");
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
fn large_file_staged_streaming() {
    let (_dir, payload_root, target_root, src) = setup();
    let big = vec![0xA5u8; 3 * 1024 * 1024];
    std::fs::write(payload_root.join("big.bin"), &big).unwrap();
    let dest = target_root.join("big.bin");

    let mut exec = WindowsFileExecutor::new(
        src,
        target_root.join("work"),
        "txbig".into(),
        Box::new(NullProgress),
    );
    let rel = RelativePath::new("big.bin").unwrap();
    let stage = zup_transaction::TransactionNode {
        id: stage_op("big"),
        phase: zup_transaction::Phase::Stage,
        kind: zup_transaction::NodeKind::StageFile {
            key: zup_core::ResourceKey::File {
                destination: dest.display().to_string(),
            },
        },
        declaration_order: 1,
        meta: zup_transaction::NodeMeta::default(),
    };
    exec.note_file(
        &stage.id,
        FilePrecondition::Absent,
        digest(&big),
        big.len() as u64,
    );
    apply_node(&mut exec, &stage, &rel, &dest).expect("stage large");
}
