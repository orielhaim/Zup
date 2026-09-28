//! Windows file executor integration tests (temporary trees only).

use std::path::{Path, PathBuf};

use tempfile::TempDir;
use zup_bundle::DirectoryPayloadSource;
use zup_core::{RelativePath, Sha256Digest, TargetTriple, hash_reader};
use zup_transaction::{
    FileDelta, FilePrecondition, OperationId, TransactionInput, compile_transaction,
};
use zup_windows::{
    InstallationLock, LockScope, NullProgress, WindowsFileExecutor, apply_node, create_durable,
    move_durable, reconcile_node, to_host_path, volume_root, write_durable,
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

/// The lock's identity is the installation, not the operation or the machine
/// layout. Everything here is a case where getting it wrong means either two
/// processes mutating one ledger, or two unrelated installs refusing to run at
/// the same time.
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

    // A bootstrap and a transaction on the same installation are the same
    // authority, but a parent that is staging prerequisites and a worker that is
    // installing files are different moments of one operation, and the key has
    // to say so without the two ever colliding.
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

    // The key has to survive a character an application id may legally contain
    // but a file name may not.
    let awkward = InstallationLock::lock_key("com.acme.desktop/../../etc", "user");
    assert!(!awkward.contains('/'), "{awkward}");
    assert!(!awkward.contains(".."), "{awkward}");
}

/// The refusal names the state root, not the lock file.
///
/// A message containing `%LOCALAPPDATA%\zup\zup-install-com_acme_desktop-user.lock`
/// sends a user to look at a file that means nothing to them; the directory it
/// lives in is the thing they can act on.
#[test]
fn a_lock_refusal_names_a_directory_and_not_an_implementation_detail() {
    let dir = TempDir::new().unwrap();
    // A directory where the lock file goes: opening it cannot succeed, and the
    // message has to be about the directory a user can look at.
    let key = InstallationLock::lock_key("com.acme.desktop", "machine");
    std::fs::create_dir(dir.path().join(format!("{key}.lock"))).unwrap();
    let error = InstallationLock::try_acquire(dir.path(), &key).expect_err("not a file");
    let message = error.to_string();
    assert!(!message.contains(".lock"), "{message}");
    assert!(
        message.contains(&dir.path().display().to_string()),
        "{message}"
    );
}

/// An uninstalled installation leaves no lock behind, and a reinstall starts
/// clean. This is the residue a repeat-lifecycle test would otherwise leave
/// behind for the next run to trip over.
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
    // And removing again is not an error: uninstall is idempotent.
    InstallationLock::remove_if_unheld(dir.path(), &key).unwrap();
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
            delta: FileDelta::Create,
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
            delta: FileDelta::Replace,
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
            delta: FileDelta::Create,
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
            delta: FileDelta::Replace,
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

#[test]
fn installed_file_verification_follows_the_receipt() {
    use zup_windows::verify_installed_file;

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
        "tx-verify".into(),
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
    exec.note_file(&stage.id, FilePrecondition::Absent, digest(b"hello"), 5);
    let stage_receipt = apply_node(&mut exec, &stage, &rel, &dest).expect("stage");
    let zup_windows::OperationReceipt::StageFile(staged) = &stage_receipt else {
        panic!("expected a stage receipt");
    };
    let staged_path = staged.staged_path.clone();
    let staged_receipt = zup_windows::transaction_receipt(stage_receipt);
    verify_installed_file(&staged_receipt).expect("staged payload matches");

    let create = zup_transaction::TransactionNode {
        id: op_id.clone(),
        phase: zup_transaction::Phase::FileMutation,
        kind: zup_transaction::NodeKind::FileMutation {
            key: zup_core::ResourceKey::File {
                destination: dest.display().to_string(),
            },
            delta: FileDelta::Create,
        },
        declaration_order: 2,
        meta: zup_transaction::NodeMeta::default(),
    };
    exec.note_file(&create.id, FilePrecondition::Absent, digest(b"hello"), 5);
    let receipt = apply_node(&mut exec, &create, &rel, &dest).expect("create");
    let receipt = zup_windows::transaction_receipt(receipt);
    let zup_transaction::OperationReceipt::CreateFile {
        destination,
        installed_sha256,
        installed_size,
        ..
    } = &receipt
    else {
        panic!("expected a create receipt");
    };
    verify_installed_file(&receipt).expect("installed file matches its receipt");

    // The published file is the installed state, so the staged copy is gone
    // and staging is no longer verifiable — verification covers the mutation.
    assert!(
        verify_installed_file(&staged_receipt).is_err(),
        "a published payload is not still staged"
    );
    assert!(!std::path::Path::new(&staged_path).exists());

    // Anything other than the recorded bytes fails verification.
    std::fs::write(&dest, b"HELLO").unwrap();
    let error = verify_installed_file(&receipt).unwrap_err();
    assert!(matches!(
        error,
        zup_windows::WindowsFileExecutorError::Verification { .. }
    ));
    std::fs::remove_file(&dest).unwrap();
    assert!(
        verify_installed_file(&receipt).is_err(),
        "a missing installed file is not verified"
    );
    let _ = (destination, installed_sha256, installed_size);
}

#[test]
fn replaced_file_verification_checks_installed_and_backup() {
    use zup_windows::verify_installed_file;

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
        "tx-verify-replace".into(),
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
        id: op_id,
        phase: zup_transaction::Phase::FileMutation,
        kind: zup_transaction::NodeKind::FileMutation {
            key: zup_core::ResourceKey::File {
                destination: dest.display().to_string(),
            },
            delta: FileDelta::Replace,
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
    let receipt = zup_windows::transaction_receipt(receipt);
    verify_installed_file(&receipt).expect("replace matches its receipt");

    let zup_transaction::OperationReceipt::ReplaceFile { backup_path, .. } = &receipt else {
        panic!("expected a replace receipt");
    };
    let backup = std::path::PathBuf::from(backup_path);
    std::fs::write(&backup, b"zzzz").unwrap();
    assert!(
        verify_installed_file(&receipt).is_err(),
        "a changed backup fails verification"
    );
    std::fs::write(&backup, b"old!").unwrap();
    std::fs::write(&dest, b"nope").unwrap();
    assert!(
        verify_installed_file(&receipt).is_err(),
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
        Box::new(NullProgress),
    );
    let receipt = exec.apply_owned_file_removal(node).unwrap();
    verify_installed_file(&receipt).expect("removal matches its receipt");

    // A file that reappeared at the destination is no longer removed.
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

#[test]
fn backend_receipts_are_not_file_verifications() {
    use zup_core::ResourceKey;
    use zup_windows::verify_installed_file;

    let error = verify_installed_file(&zup_transaction::OperationReceipt::Control)
        .expect_err("a barrier receipt names no file state");
    assert!(matches!(
        error,
        zup_windows::WindowsFileExecutorError::Unsupported { .. }
    ));
    let error = verify_installed_file(&zup_transaction::OperationReceipt::Backend {
        key: ResourceKey::Backend {
            id: zup_core::BackendResourceId::new("fake.backend").unwrap(),
        },
        payload: b"opaque".to_vec(),
    })
    .expect_err("an opaque backend receipt is not a file receipt");
    assert!(matches!(
        error,
        zup_windows::WindowsFileExecutorError::Unsupported { .. }
    ));
}
