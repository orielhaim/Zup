#![cfg(target_os = "linux")]
mod requirements_fixture {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/deployment.rs"
    ));
}
use zup_plan::RemovalKind;
use zup_plan::{FileRemoval, OperationId};

use rstest::rstest;
use zup_core::{SelectedScope, Sha256Digest, hash_reader};
use zup_linux::{
    EntryKind, ExecError, LinuxFileExecutor, OwnedDirectory, PathError, STATE_DIRECTORY_MODE,
    STATE_FILE_MODE,
};
use zup_plan::FilePrecondition;
use zup_transaction::OperationReceipt;

use std::path::{Path, PathBuf};

fn digest(bytes: &[u8]) -> Sha256Digest {
    hash_reader(bytes).expect("bytes hash").1
}

fn file_key() -> zup_core::ResourceKey {
    zup_core::ResourceKey::File {
        id: zup_core::FileResourceId::Payload {
            id: zup_core::PayloadId::File {
                declaration: zup_core::ResourceId::new("program").unwrap(),
            },
        },
    }
}

fn stage_id() -> OperationId {
    OperationId::stage_file(&file_key())
}

struct TransactionContext {
    record: zup_transaction::TransactionRecord,
    store: zup_transaction::FilesystemTransactionStore,
    _directory: tempfile::TempDir,
}

impl std::ops::Deref for TransactionContext {
    type Target = zup_transaction::TransactionRecord;
    fn deref(&self) -> &Self::Target {
        &self.record
    }
}

impl TransactionContext {
    fn new(plan: zup_plan::DeploymentPlan) -> Self {
        use zup_transaction::{NodeState, OperationReceipt, TransactionStore};
        let directory = tempfile::tempdir().unwrap();
        let store =
            zup_transaction::FilesystemTransactionStore::private_at(directory.path().join("state"));
        let mut record = zup_transaction::TransactionRecord::new(
            zup_core::TransactionId::new_v7(),
            plan.requirements.app.id.clone(),
            SelectedScope::User,
            "1.0.0".parse().unwrap(),
            plan,
        );
        store.create(&record).unwrap();
        let reservation = OperationId::new(OperationId::RESERVE);
        record.nodes.insert(
            OperationId::new(OperationId::BEGIN),
            NodeState::Applied {
                receipt: Box::new(OperationReceipt::Control),
            },
        );
        record.nodes.insert(reservation.clone(), NodeState::Running);
        let revision = record.revision;
        record.touch();
        store.compare_and_swap(revision, &record).unwrap();
        record.reservation = Some(store.reserve_installation(&record).unwrap());
        record.nodes.insert(
            reservation,
            NodeState::Applied {
                receipt: Box::new(OperationReceipt::Control),
            },
        );
        let revision = record.revision;
        record.touch();
        store.compare_and_swap(revision, &record).unwrap();
        Self {
            record,
            store,
            _directory: directory,
        }
    }

    fn persist(&mut self, id: OperationId, state: zup_transaction::NodeState) {
        use zup_transaction::TransactionStore;
        let revision = self.record.revision;
        self.record.nodes.insert(id, state);
        self.record.touch();
        self.store.compare_and_swap(revision, &self.record).unwrap();
        self.record = self.store.load(&self.record.transaction_id).unwrap();
    }
}

fn stage(
    executor: &mut LinuxFileExecutor,
    transaction: &mut TransactionContext,
    bytes: &[u8],
) -> Result<OperationReceipt, ExecError> {
    transaction.persist(stage_id(), zup_transaction::NodeState::Running);
    let receipt = executor.stage(&stage_id(), bytes)?;
    transaction.persist(
        stage_id(),
        zup_transaction::NodeState::Applied {
            receipt: Box::new(receipt.clone()),
        },
    );
    Ok(receipt)
}

fn executor_for(
    destination: &Path,
    bytes: &[u8],
    executable: bool,
    precondition: FilePrecondition,
) -> (LinuxFileExecutor, TransactionContext) {
    let kind = match precondition {
        FilePrecondition::Absent => zup_plan::FileOperationKind::Create,
        FilePrecondition::Exact { .. } => zup_plan::FileOperationKind::Replace,
    };
    executor_for_kind(destination, bytes, executable, precondition, kind)
}

fn executor_for_kind(
    destination: &Path,
    bytes: &[u8],
    executable: bool,
    precondition: FilePrecondition,
    kind: zup_plan::FileOperationKind,
) -> (LinuxFileExecutor, TransactionContext) {
    let target = zup_core::Target::host().unwrap();
    let mut effects = zup_plan::DeploymentEffects::new(
        zup_core::InstallationId::new_v7(),
        requirements_fixture::requirements(
            target,
            zup_core::SelectedScope::User,
            "example.application",
            "1.0.0",
        ),
    );
    effects.install_directory = Some(
        zup_core::TargetPath::new(target, destination.parent().unwrap().to_string_lossy()).unwrap(),
    );
    effects.files.push(zup_plan::FileOperation {
        key: file_key(),
        replacement: zup_core::FixedReplacement::Atomic,
        kind,
        destination: zup_core::TargetPath::new(target, destination.to_string_lossy()).unwrap(),
        source_relative: zup_core::RelativePath::new("payload.bin").unwrap(),
        precondition,
        expected_size: bytes.len() as u64,
        expected_sha256: digest(bytes),
        privilege: zup_core::Privilege::User,
        executable,
        conflict: None,
    });
    let transaction = TransactionContext::new(zup_plan::compile_deployment(&effects).unwrap());
    let mut executor = LinuxFileExecutor::new();
    executor.register_plan(&transaction.plan).unwrap();
    executor.set_transaction_id(transaction.transaction_id);
    (executor, transaction)
}

fn mutation(transaction: &TransactionContext) -> zup_plan::DeploymentNode {
    transaction
        .plan
        .nodes
        .iter()
        .find(|node| matches!(node.kind, zup_plan::NodeKind::FileMutation { .. }))
        .unwrap()
        .clone()
}

fn removal_node(
    executor: &mut LinuxFileExecutor,
    destination: &Path,
    bytes: &[u8],
) -> (zup_plan::DeploymentNode, TransactionContext) {
    let target = zup_core::Target::host().unwrap();
    let destination = zup_core::TargetPath::new(target, destination.to_string_lossy()).unwrap();
    let mut effects = zup_plan::DeploymentEffects::new(
        zup_core::InstallationId::new_v7(),
        requirements_fixture::requirements(
            target,
            zup_core::SelectedScope::User,
            "example.application",
            "1.0.0",
        ),
    );
    effects.install_directory = destination.parent();
    effects.transition = zup_plan::DeploymentTransition::Remove;
    effects.retired_keys.push(file_key());
    effects.removals.push(FileRemoval {
        instance: zup_core::FixedFileInstance {
            origin: zup_core::TransactionId::new_v7(),
            object: file_object(Path::new(destination.as_str())),
        },
        key: file_key(),
        kind: RemovalKind::RemoveOwned,
        scope: SelectedScope::User,
        privilege: zup_core::Privilege::User,
        destination,
        sha256: digest(bytes),
        size: bytes.len() as u64,
        created_directories: Vec::new(),
    });
    let transaction = TransactionContext::new(zup_plan::compile_deployment(&effects).unwrap());
    executor.set_transaction_id(transaction.transaction_id);
    executor.register_plan(&transaction.plan).unwrap();
    let node = transaction
        .plan
        .nodes
        .iter()
        .find(|node| matches!(node.kind, zup_plan::NodeKind::FileRemoval { .. }))
        .unwrap()
        .clone();
    (node, transaction)
}

fn mode_of(path: &Path) -> u32 {
    std::fs::symlink_metadata(path)
        .expect("stat")
        .permissions()
        .mode()
        & 0o777
}

use std::os::unix::fs::PermissionsExt as _;

#[test]
fn a_create_publishes_the_staged_bytes_and_reports_them() {
    let root = tempfile::tempdir().expect("a temp directory");
    let destination = root.path().join("install").join("app.dat");
    let (mut executor, mut transaction) = executor_for(
        &destination,
        b"the payload",
        false,
        FilePrecondition::Absent,
    );
    stage(&mut executor, &mut transaction, b"the payload").expect("stage");

    let receipt = executor
        .apply(&mutation(&transaction), &transaction)
        .expect("create");

    assert_eq!(std::fs::read(&destination).expect("read"), b"the payload");
    let OperationReceipt::CreateFile {
        installed_sha256,
        installed_size,
        executable,
        created_directories,
        ..
    } = &receipt
    else {
        panic!("a create reports a create receipt: {receipt:?}");
    };
    assert_eq!(*installed_size, 11);
    assert_eq!(*installed_sha256, digest(b"the payload"));
    assert!(!executable);
    assert_eq!(
        created_directories.len(),
        1,
        "the install directory is recorded so an uninstall knows what zup made"
    );
    assert!(executor.verify(&mutation(&transaction), &receipt).is_ok());
}

#[rstest]
#[case::declared(true, 0o744)]
#[case::not_declared(false, 0o644)]
fn executable_intent_becomes_the_mode_the_receipt_claims(
    #[case] declared: bool,
    #[case] expected: u32,
) {
    let root = tempfile::tempdir().expect("a temp directory");
    let destination = root.path().join("install").join("tool");
    let (mut executor, mut transaction) = executor_for(
        &destination,
        b"#!/bin/sh\n",
        declared,
        FilePrecondition::Absent,
    );
    stage(&mut executor, &mut transaction, b"#!/bin/sh\n").expect("stage");

    let receipt = executor
        .apply(&mutation(&transaction), &transaction)
        .expect("create");

    assert_eq!(mode_of(&destination), expected);
    assert!(
        matches!(&receipt, OperationReceipt::CreateFile { executable, .. } if *executable == declared),
        "the receipt reports the intent it honoured: {receipt:?}"
    );
}

#[test]
fn a_create_refuses_a_destination_that_is_already_there() {
    let root = tempfile::tempdir().expect("a temp directory");
    let destination = root.path().join("install").join("app.dat");
    let parent = OwnedDirectory::create(&root.path().join("install"), STATE_DIRECTORY_MODE)
        .expect("create install");
    parent
        .write_durable("app.dat", b"someone else's file", STATE_FILE_MODE)
        .expect("plant");

    let (mut executor, mut transaction) = executor_for(
        &destination,
        b"the payload",
        false,
        FilePrecondition::Absent,
    );
    stage(&mut executor, &mut transaction, b"the payload").expect("stage");

    let error = executor
        .apply(&mutation(&transaction), &transaction)
        .expect_err("a create over an existing file is refused");
    assert!(
        matches!(error, ExecError::Path(PathError::AlreadyExists { .. })),
        "the refusal names the precondition, not a generic I/O failure: {error}"
    );
    assert_eq!(
        std::fs::read(&destination).expect("read"),
        b"someone else's file",
        "the file that was already there is untouched"
    );
}

#[test]
fn a_replace_backs_up_what_it_replaced() {
    let root = tempfile::tempdir().expect("a temp directory");
    let destination = root.path().join("install").join("app.dat");
    std::fs::create_dir_all(root.path().join("install")).expect("install");
    std::fs::write(&destination, b"v1").expect("write v1");

    let (mut executor, mut transaction) = executor_for(
        &destination,
        b"v2",
        false,
        FilePrecondition::Exact {
            object: file_object(&destination),
            size: 2,
            sha256: digest(b"v1"),
        },
    );
    stage(&mut executor, &mut transaction, b"v2").expect("stage");

    let receipt = executor
        .apply(&mutation(&transaction), &transaction)
        .expect("replace");

    assert_eq!(std::fs::read(&destination).expect("read"), b"v2");
    let OperationReceipt::ReplaceFile {
        backup_path,
        previous_sha256,
        ..
    } = &receipt
    else {
        panic!("a replace reports a replace receipt: {receipt:?}");
    };
    assert_eq!(previous_sha256, &digest(b"v1"));
    assert_eq!(
        std::fs::read(backup_path.as_str()).expect("the backup is readable"),
        b"v1",
        "the backup holds the previous contents, not the new ones"
    );
}

#[test]
fn repaired_content_is_not_replaced_with_the_displaced_modified_instance() {
    let root = tempfile::tempdir().unwrap();
    let destination = root.path().join("install/app.dat");
    std::fs::create_dir(destination.parent().unwrap()).unwrap();
    std::fs::write(&destination, b"modified").unwrap();
    let old_object = file_object(&destination);
    let (mut executor, mut transaction) = executor_for_kind(
        &destination,
        b"authenticated",
        false,
        FilePrecondition::Exact {
            object: old_object,
            size: 8,
            sha256: digest(b"modified"),
        },
        zup_plan::FileOperationKind::RepairOwned,
    );
    stage(&mut executor, &mut transaction, b"authenticated").unwrap();
    let node = mutation(&transaction);
    let receipt = executor.apply(&node, &transaction).unwrap();
    let OperationReceipt::ReplaceFile { backup_path, .. } = &receipt else {
        panic!("replace receipt")
    };
    assert!(executor.rollback(&node, &receipt).is_err());
    assert_eq!(std::fs::read(&destination).unwrap(), b"authenticated");
    assert_eq!(std::fs::read(backup_path).unwrap(), b"modified");
    assert_eq!(file_object(Path::new(backup_path)), old_object);
}

#[rstest]
#[case(0o646)]
#[case(0o4755)]
fn rollback_refuses_a_backup_with_unsafe_permissions_before_restoring_it(#[case] mode: u32) {
    let root = tempfile::tempdir().unwrap();
    let destination = root.path().join("install/app.dat");
    std::fs::create_dir(destination.parent().unwrap()).unwrap();
    std::fs::write(&destination, b"old").unwrap();
    let (mut executor, mut transaction) = executor_for(
        &destination,
        b"new",
        false,
        FilePrecondition::Exact {
            object: file_object(&destination),
            size: 3,
            sha256: digest(b"old"),
        },
    );
    stage(&mut executor, &mut transaction, b"new").unwrap();
    let node = mutation(&transaction);
    let receipt = executor.apply(&node, &transaction).unwrap();
    let OperationReceipt::ReplaceFile { backup_path, .. } = &receipt else {
        panic!("replace receipt")
    };
    std::fs::set_permissions(backup_path, std::fs::Permissions::from_mode(mode)).unwrap();
    assert!(executor.rollback(&node, &receipt).is_err());
    assert_eq!(std::fs::read(&destination).unwrap(), b"new");
    assert_eq!(std::fs::read(backup_path).unwrap(), b"old");
    std::fs::set_permissions(backup_path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

#[test]
#[ignore = "requires root in an isolated verification environment"]
fn matching_native_identity_and_bytes_do_not_authorize_a_foreign_owner() {
    assert!(rustix::process::geteuid().is_root());
    let root = tempfile::tempdir().unwrap();
    let destination = root.path().join("install/app.dat");
    std::fs::create_dir(destination.parent().unwrap()).unwrap();
    std::fs::write(&destination, b"old").unwrap();
    let object = file_object(&destination);
    let (mut executor, mut transaction) = executor_for(
        &destination,
        b"new",
        false,
        FilePrecondition::Exact {
            object,
            size: 3,
            sha256: digest(b"old"),
        },
    );
    stage(&mut executor, &mut transaction, b"new").unwrap();
    let file = std::fs::File::open(&destination).unwrap();
    rustix::fs::fchown(&file, Some(rustix::process::Uid::from_raw(65534)), None).unwrap();
    assert_eq!(file_object(&destination), object);
    assert!(
        executor
            .apply(&mutation(&transaction), &transaction)
            .is_err()
    );
    assert_eq!(std::fs::read(&destination).unwrap(), b"old");
    rustix::fs::fchown(&file, Some(rustix::process::Uid::ROOT), None).unwrap();
}

#[rstest::rstest]
#[case(false)]
#[case(true)]
fn a_replace_refuses_to_overwrite_a_file_that_changed(#[case] replace_object: bool) {
    let root = tempfile::tempdir().expect("a temp directory");
    let destination = root.path().join("install").join("app.dat");
    std::fs::create_dir_all(root.path().join("install")).expect("install");
    std::fs::write(&destination, b"v1").expect("write");
    let object = file_object(&destination);
    if replace_object {
        std::fs::rename(&destination, root.path().join("retained")).unwrap();
        std::fs::write(&destination, b"v1").unwrap();
    } else {
        std::fs::write(&destination, b"a user's edit").unwrap();
    }

    let (mut executor, mut transaction) = executor_for(
        &destination,
        b"v2",
        false,
        FilePrecondition::Exact {
            object,
            size: 2,
            sha256: digest(b"v1"),
        },
    );
    stage(&mut executor, &mut transaction, b"v2").expect("stage");

    let error = executor
        .apply(&mutation(&transaction), &transaction)
        .expect_err("a replace over changed bytes is refused");
    assert!(matches!(error, ExecError::PlanDrift { .. }), "{error}");
    assert_eq!(
        std::fs::read(&destination).expect("read"),
        if replace_object {
            b"v1".as_slice()
        } else {
            b"a user's edit".as_slice()
        },
        "the user's edit is not overwritten"
    );
}

#[test]
fn a_destination_parent_replaced_by_a_symlink_is_refused() {
    let root = tempfile::tempdir().expect("a temp directory");
    let elsewhere = tempfile::tempdir().expect("an unrelated tree");
    let victim = elsewhere.path().join("victim");
    std::fs::write(&victim, b"someone else's file").expect("write");

    let install = root.path().join("install");
    std::fs::create_dir_all(&install).expect("install");
    let destination = install.join("app.dat");
    let (mut executor, mut transaction) = executor_for(
        &destination,
        b"the payload",
        false,
        FilePrecondition::Absent,
    );
    stage(&mut executor, &mut transaction, b"the payload").expect("stage");

    std::fs::remove_dir_all(&install).expect("remove the staged sibling");
    std::os::unix::fs::symlink(elsewhere.path(), &install).expect("symlink");

    let outcome = executor.apply(&mutation(&transaction), &transaction);
    assert!(
        outcome.is_err(),
        "publishing through a link into another tree must be refused, not followed"
    );
    assert_eq!(
        std::fs::read(&victim).expect("read"),
        b"someone else's file",
        "the file outside the install directory is untouched"
    );
}

#[test]
fn a_destination_that_is_a_symlink_is_refused() {
    let root = tempfile::tempdir().expect("a temp directory");
    let elsewhere = tempfile::tempdir().expect("an unrelated tree");
    std::fs::write(elsewhere.path().join("target"), b"elsewhere").expect("write");

    let destination = root.path().join("app.dat");
    std::os::unix::fs::symlink(elsewhere.path().join("target"), &destination).expect("symlink");

    let (mut executor, mut transaction) = executor_for(
        &destination,
        b"the payload",
        false,
        FilePrecondition::Absent,
    );
    stage(&mut executor, &mut transaction, b"the payload").expect("stage");

    let error = executor
        .apply(&mutation(&transaction), &transaction)
        .expect_err("a link at an owned path is refused");
    assert!(
        matches!(error, ExecError::Path(PathError::AlreadyExists { .. })),
        "a link at an owned path is refused as present, not followed: {error}"
    );
    assert_eq!(
        std::fs::read(elsewhere.path().join("target")).expect("read"),
        b"elsewhere",
        "the link's target is untouched"
    );
}

#[rstest]
#[case::fifo(EntryKind::Special)]
#[case::directory(EntryKind::Directory)]
fn a_destination_that_is_not_a_regular_file_is_refused(#[case] planted: EntryKind) {
    let root = tempfile::tempdir().expect("a temp directory");
    let destination = root.path().join("app.dat");
    match planted {
        EntryKind::Special => {
            let name = std::ffi::CString::new(destination.to_string_lossy().as_bytes())
                .expect("a c string");
            // SAFETY: `mkfifo` is called through `rustix`, which is a safe wrapper,

            let result = rustix::fs::mknodat(
                rustix::fs::CWD,
                &name,
                rustix::fs::FileType::Fifo,
                rustix::fs::Mode::from_bits_truncate(0o644),
                0,
            );
            result.expect("a fifo");
        }
        EntryKind::Directory => std::fs::create_dir(&destination).expect("a directory"),
        _ => unreachable!("the case matrix only plants these two"),
    }

    let (mut executor, mut transaction) = executor_for(
        &destination,
        b"the payload",
        false,
        FilePrecondition::Absent,
    );
    stage(&mut executor, &mut transaction, b"the payload").expect("stage");

    assert!(
        executor
            .apply(&mutation(&transaction), &transaction)
            .is_err(),
        "{planted:?} where a regular file belongs must be refused"
    );
}

#[test]
fn a_removal_unlinks_and_backs_up() {
    let root = tempfile::tempdir().expect("a temp directory");
    let destination = root.path().join("app.dat");
    std::fs::write(&destination, b"the payload").expect("write");

    let mut executor = LinuxFileExecutor::new();

    let (planned, transaction) = removal_node(&mut executor, &destination, b"the payload");
    let receipt = executor.apply(&planned, &transaction).expect("remove");

    assert!(!destination.exists(), "the file is unlinked");
    let OperationReceipt::RemoveFile { backup_path, .. } = &receipt else {
        panic!("a removal reports a removal receipt: {receipt:?}");
    };
    assert_eq!(
        std::fs::read(backup_path.as_str()).expect("the backup is readable"),
        b"the payload",
        "the journal can put the file back"
    );
}

#[test]
fn rolling_back_a_create_removes_only_what_the_create_made() {
    let root = tempfile::tempdir().expect("a temp directory");
    let destination = root.path().join("install").join("nested").join("app.dat");
    let (mut executor, mut transaction) = executor_for(
        &destination,
        b"the payload",
        false,
        FilePrecondition::Absent,
    );
    stage(&mut executor, &mut transaction, b"the payload").expect("stage");

    let receipt = executor
        .apply(&mutation(&transaction), &transaction)
        .expect("create");

    std::fs::write(root.path().join("install").join("user.dat"), b"the user's").expect("write");

    executor
        .rollback(&mutation(&transaction), &receipt)
        .expect("rollback");

    assert!(!destination.exists(), "the payload is gone");
    assert!(
        root.path().join("install").join("user.dat").exists(),
        "an unowned file in a directory zup made is not touched"
    );
    assert!(
        root.path().join("install").exists(),
        "a directory that is no longer empty is not removed"
    );
}

#[test]
fn a_rollback_refuses_to_remove_a_file_that_changed() {
    let root = tempfile::tempdir().expect("a temp directory");
    let destination = root.path().join("install").join("app.dat");
    let (mut executor, mut transaction) = executor_for(
        &destination,
        b"the payload",
        false,
        FilePrecondition::Absent,
    );
    stage(&mut executor, &mut transaction, b"the payload").expect("stage");
    let receipt = executor
        .apply(&mutation(&transaction), &transaction)
        .expect("create");

    std::fs::write(&destination, b"the user's edit").expect("write");

    assert!(
        executor
            .rollback(&mutation(&transaction), &receipt)
            .is_err(),
        "a file that is no longer the one this transaction wrote must not be removed"
    );
    assert_eq!(
        std::fs::read(&destination).expect("read"),
        b"the user's edit"
    );
}

#[test]
fn rolling_back_a_replace_restores_the_previous_contents() {
    let root = tempfile::tempdir().expect("a temp directory");
    let destination = root.path().join("install").join("app.dat");
    std::fs::create_dir_all(root.path().join("install")).expect("install");
    std::fs::write(&destination, b"v1").expect("write v1");

    let (mut executor, mut transaction) = executor_for(
        &destination,
        b"v2",
        false,
        FilePrecondition::Exact {
            object: file_object(&destination),
            size: 2,
            sha256: digest(b"v1"),
        },
    );
    stage(&mut executor, &mut transaction, b"v2").expect("stage");
    let receipt = executor
        .apply(&mutation(&transaction), &transaction)
        .expect("replace");

    executor
        .rollback(&mutation(&transaction), &receipt)
        .expect("rollback");

    assert_eq!(
        std::fs::read(&destination).expect("read"),
        b"v1",
        "the replaced file comes back exactly"
    );
}

#[rstest]
#[case::as_written(true, true)]
#[case::after_the_publish(false, false)]
fn reconcile_sees_the_published_file_as_applied(
    #[case] applied: bool,
    #[case] expect_applied: bool,
) {
    let root = tempfile::tempdir().expect("a temp directory");
    let destination = root.path().join("install").join("app.dat");
    let (mut executor, mut transaction) = executor_for(
        &destination,
        b"the payload",
        false,
        FilePrecondition::Absent,
    );
    stage(&mut executor, &mut transaction, b"the payload").expect("stage");
    let receipt = executor
        .apply(&mutation(&transaction), &transaction)
        .expect("create");
    if !applied {
        std::fs::remove_file(&destination).expect("undo");
    }

    let outcome = executor
        .reconcile(&mutation(&transaction), Some(&receipt), &transaction)
        .expect("reconcile");

    assert_eq!(
        matches!(
            outcome,
            zup_transaction::ReconcileResult::AppliedWithReceipt(_)
        ),
        expect_applied
    );
}

#[test]
fn staged_cleanup_preserves_an_equal_byte_foreign_instance() {
    let root = tempfile::tempdir().expect("a temp directory");
    let destination = root.path().join("install").join("app.dat");
    let (mut executor, mut transaction) = executor_for(
        &destination,
        b"the payload",
        false,
        FilePrecondition::Absent,
    );

    let receipt = stage(&mut executor, &mut transaction, b"the payload").expect("stage");
    let OperationReceipt::StageFile { staged_path, .. } = &receipt else {
        panic!("staging reports a staging receipt: {receipt:?}");
    };

    let staged = PathBuf::from(staged_path);
    assert!(
        staged
            .file_name()
            .expect("a name")
            .to_string_lossy()
            .starts_with("staged-"),
        "the name is derived, not taken from the plan: {staged_path}"
    );
    assert_eq!(std::fs::read(&staged).expect("read"), b"the payload");
    std::fs::rename(&staged, root.path().join("original-stage")).unwrap();
    std::fs::write(&staged, b"the payload").unwrap();
    let stage = zup_plan::DeploymentNode {
        id: stage_id(),
        phase: zup_plan::Phase::Stage,
        kind: zup_plan::NodeKind::StageFile { key: file_key() },
        declaration_order: 0,
        meta: Default::default(),
    };
    assert!(executor.rollback(&stage, &receipt).is_err());
    assert_eq!(std::fs::read(&staged).unwrap(), b"the payload");
}

fn plant_executable(path: &Path, bytes: &[u8]) {
    std::fs::write(path, bytes).expect("write");
    let mut permissions = std::fs::symlink_metadata(path).expect("stat").permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions).expect("chmod");
}

#[test]
fn rolling_back_a_removal_restores_bytes_and_mode() {
    let root = tempfile::tempdir().expect("a temp directory");
    let destination = root.path().join("tool");
    plant_executable(&destination, b"#!/bin/sh\necho v1\n");
    let mut executor = LinuxFileExecutor::new();

    let (planned, transaction) = removal_node(&mut executor, &destination, b"#!/bin/sh\necho v1\n");
    let receipt = executor.apply(&planned, &transaction).expect("remove");
    assert!(!destination.exists(), "the file is unlinked");

    executor.rollback(&planned, &receipt).expect("rollback");

    assert_eq!(
        std::fs::read(&destination).expect("read"),
        b"#!/bin/sh\necho v1\n",
        "the removed file comes back exactly"
    );
    assert_eq!(
        mode_of(&destination),
        0o755,
        "the restored file keeps its executable mode"
    );
}

#[test]
fn rolling_back_a_replace_restores_bytes_and_executable_mode() {
    let root = tempfile::tempdir().expect("a temp directory");
    let destination = root.path().join("install").join("tool");
    std::fs::create_dir_all(root.path().join("install")).expect("install");
    plant_executable(&destination, b"v1");
    let (mut executor, mut transaction) = executor_for(
        &destination,
        b"v2",
        false,
        FilePrecondition::Exact {
            object: file_object(&destination),
            size: 2,
            sha256: digest(b"v1"),
        },
    );
    stage(&mut executor, &mut transaction, b"v2").expect("stage");
    let receipt = executor
        .apply(&mutation(&transaction), &transaction)
        .expect("replace");
    assert_eq!(mode_of(&destination), 0o644);

    executor
        .rollback(&mutation(&transaction), &receipt)
        .expect("rollback");

    assert_eq!(
        std::fs::read(&destination).expect("read"),
        b"v1",
        "the replaced file comes back exactly"
    );
    assert_eq!(
        mode_of(&destination),
        0o755,
        "rollback restores the executable bit, not the payload default"
    );
}

#[test]
fn a_replace_rollback_is_idempotent() {
    let root = tempfile::tempdir().expect("a temp directory");
    let destination = root.path().join("install").join("app.dat");
    std::fs::create_dir_all(root.path().join("install")).expect("install");
    std::fs::write(&destination, b"v1").expect("write v1");
    let (mut executor, mut transaction) = executor_for(
        &destination,
        b"v2",
        false,
        FilePrecondition::Exact {
            object: file_object(&destination),
            size: 2,
            sha256: digest(b"v1"),
        },
    );
    stage(&mut executor, &mut transaction, b"v2").expect("stage");
    let receipt = executor
        .apply(&mutation(&transaction), &transaction)
        .expect("replace");
    let node = mutation(&transaction);

    executor.rollback(&node, &receipt).expect("first rollback");
    executor
        .rollback(&node, &receipt)
        .expect("a repeated rollback sees the restored file and succeeds");

    assert_eq!(std::fs::read(&destination).expect("read"), b"v1");
}

#[test]
fn a_replace_rollback_refuses_a_destination_changed_after_publish() {
    let root = tempfile::tempdir().expect("a temp directory");
    let destination = root.path().join("install").join("app.dat");
    std::fs::create_dir_all(root.path().join("install")).expect("install");
    std::fs::write(&destination, b"v1").expect("write v1");
    let (mut executor, mut transaction) = executor_for(
        &destination,
        b"v2",
        false,
        FilePrecondition::Exact {
            object: file_object(&destination),
            size: 2,
            sha256: digest(b"v1"),
        },
    );
    stage(&mut executor, &mut transaction, b"v2").expect("stage");
    let receipt = executor
        .apply(&mutation(&transaction), &transaction)
        .expect("replace");
    std::fs::write(&destination, b"a user's edit").expect("write");

    assert!(
        executor
            .rollback(&mutation(&transaction), &receipt)
            .is_err(),
        "restoring over an unrelated edit would destroy user data"
    );
    assert_eq!(std::fs::read(&destination).expect("read"), b"a user's edit");
}

#[test]
fn a_removal_rollback_refuses_a_recreated_foreign_file() {
    let root = tempfile::tempdir().expect("a temp directory");
    let destination = root.path().join("app.dat");
    std::fs::write(&destination, b"the payload").expect("write");
    let mut executor = LinuxFileExecutor::new();
    let (planned, transaction) = removal_node(&mut executor, &destination, b"the payload");
    let receipt = executor.apply(&planned, &transaction).expect("remove");
    std::fs::write(&destination, b"someone else's file").expect("write");

    assert!(
        executor.rollback(&planned, &receipt).is_err(),
        "restoring over a foreign file would destroy user data"
    );
    assert_eq!(
        std::fs::read(&destination).expect("read"),
        b"someone else's file"
    );
}

#[rstest]
#[case(false)]
#[case(true)]
fn a_replace_rollback_refuses_a_changed_backup(#[case] replace_instance: bool) {
    let root = tempfile::tempdir().expect("a temp directory");
    let destination = root.path().join("install").join("app.dat");
    std::fs::create_dir_all(root.path().join("install")).expect("install");
    std::fs::write(&destination, b"v1").expect("write v1");
    let (mut executor, mut transaction) = executor_for(
        &destination,
        b"v2",
        false,
        FilePrecondition::Exact {
            object: file_object(&destination),
            size: 2,
            sha256: digest(b"v1"),
        },
    );
    stage(&mut executor, &mut transaction, b"v2").expect("stage");
    let receipt = executor
        .apply(&mutation(&transaction), &transaction)
        .expect("replace");
    let OperationReceipt::ReplaceFile { backup_path, .. } = &receipt else {
        panic!("a replace reports a replace receipt: {receipt:?}");
    };
    if replace_instance {
        std::fs::rename(backup_path, root.path().join("original-backup")).unwrap();
        std::fs::write(backup_path, b"v1").unwrap();
    } else {
        std::fs::write(backup_path, b"garbage").unwrap();
    }

    assert!(
        executor
            .rollback(&mutation(&transaction), &receipt)
            .is_err(),
        "a damaged backup must fail recovery, not install garbage"
    );
    assert_eq!(
        std::fs::read(&destination).expect("read"),
        b"v2",
        "the failed rollback leaves the published file alone"
    );
}

#[test]
fn reconcile_refuses_a_receipt_whose_backup_is_gone() {
    let root = tempfile::tempdir().expect("a temp directory");
    let destination = root.path().join("install").join("app.dat");
    std::fs::create_dir_all(root.path().join("install")).expect("install");
    std::fs::write(&destination, b"v1").expect("write v1");
    let (mut executor, mut transaction) = executor_for(
        &destination,
        b"v2",
        false,
        FilePrecondition::Exact {
            object: file_object(&destination),
            size: 2,
            sha256: digest(b"v1"),
        },
    );
    stage(&mut executor, &mut transaction, b"v2").expect("stage");
    let receipt = executor
        .apply(&mutation(&transaction), &transaction)
        .expect("replace");
    let OperationReceipt::ReplaceFile { backup_path, .. } = &receipt else {
        panic!("a replace reports a replace receipt: {receipt:?}");
    };
    std::fs::remove_file(backup_path).expect("lose the backup");

    assert_eq!(
        executor
            .reconcile(&mutation(&transaction), Some(&receipt), &transaction)
            .expect("reconcile"),
        zup_transaction::ReconcileResult::Ambiguous,
        "an applied file without its backup cannot roll back, so recovery stays ambiguous"
    );
}

#[test]
fn removal_refuses_a_file_modified_after_planning() {
    let root = tempfile::tempdir().expect("a temp directory");
    let destination = root.path().join("app.dat");
    std::fs::write(&destination, b"v1").expect("write v1");
    let mut executor = LinuxFileExecutor::new();
    let (planned, transaction) = removal_node(&mut executor, &destination, b"v1");

    std::fs::write(&destination, b"v2-external").expect("an outside edit");

    let error = executor
        .prepare(&planned)
        .expect_err("preparation sees the outside edit");
    assert!(
        matches!(error, ExecError::PlanDrift { .. }),
        "a changed file is drift, not absence: {error}"
    );
    let error = executor
        .apply(&planned, &transaction)
        .expect_err("the unlink refuses a file it was not authorized to remove");
    assert!(
        matches!(error, ExecError::PlanDrift { .. }),
        "the mutation boundary enforces the removal identity: {error}"
    );
    assert_eq!(
        std::fs::read(&destination).expect("read"),
        b"v2-external",
        "the outside edit survives"
    );
}

#[test]
fn removal_without_identity_is_refused() {
    let root = tempfile::tempdir().expect("a temp directory");
    let destination = root.path().join("app.dat");
    std::fs::write(&destination, b"the payload").expect("write");
    let (mut executor, transaction) = executor_for(
        &destination,
        b"the payload",
        false,
        FilePrecondition::Exact {
            object: file_object(&destination),
            size: 11,
            sha256: digest(b"the payload"),
        },
    );
    let bare = zup_plan::DeploymentNode {
        id: OperationId::resource("file_create", &file_key()),
        phase: zup_plan::Phase::Backend,
        kind: zup_plan::NodeKind::FileRemoval { key: file_key() },
        declaration_order: 0,
        meta: Default::default(),
    };

    assert!(
        matches!(executor.prepare(&bare), Err(ExecError::PlanDrift { .. })),
        "existence alone never authorizes a removal"
    );
    assert!(
        executor.apply(&bare, &transaction).is_err(),
        "the mutation boundary requires the removal identity"
    );
    assert!(
        destination.exists(),
        "nothing is unlinked without an identity"
    );
}

#[test]
fn removal_apply_is_idempotent_after_a_crash() {
    let root = tempfile::tempdir().expect("a temp directory");
    let destination = root.path().join("app.dat");
    std::fs::write(&destination, b"the payload").expect("write");
    let mut executor = LinuxFileExecutor::new();
    let (planned, transaction) = removal_node(&mut executor, &destination, b"the payload");

    let first = executor.apply(&planned, &transaction).expect("remove");
    let second = executor
        .apply(&planned, &transaction)
        .expect("a replay after a crash");

    assert!(
        matches!(
            (&first, &second),
            (
                OperationReceipt::RemoveFile { .. },
                OperationReceipt::RemoveFile { .. }
            )
        ),
        "the replay reports the same removal"
    );
    assert!(!destination.exists());
}

#[test]
fn replace_completes_an_interrupted_exchange() {
    let root = tempfile::tempdir().expect("a temp directory");
    let install = root.path().join("install");
    std::fs::create_dir_all(&install).expect("install");
    let destination = install.join("app.dat");
    std::fs::write(&destination, b"v1").expect("write v1");
    let (mut executor, mut transaction) = executor_for(
        &destination,
        b"v2",
        false,
        FilePrecondition::Exact {
            object: file_object(&destination),
            size: 2,
            sha256: digest(b"v1"),
        },
    );
    stage(&mut executor, &mut transaction, b"v2").expect("stage");
    let staged = std::fs::read_dir(&install)
        .expect("read dir")
        .map(|entry| entry.expect("entry").file_name())
        .find(|name| name.to_string_lossy().starts_with("staged-"))
        .expect("a staged file beside the destination");
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(
            install.join(&staged),
            std::fs::Permissions::from_mode(0o644),
        )
        .expect("the crash lands after the staged file takes its published mode");
    }

    rustix::fs::renameat_with(
        rustix::fs::CWD,
        install.join(staged),
        rustix::fs::CWD,
        &destination,
        rustix::fs::RenameFlags::EXCHANGE,
    )
    .expect("the crash lands between the exchange and the backup rename");

    let outcome = executor
        .reconcile(&mutation(&transaction), None, &transaction)
        .expect("reconcile");
    let OperationReceipt::ReplaceFile { backup_path, .. } = (match outcome {
        zup_transaction::ReconcileResult::AppliedWithReceipt(receipt) => receipt,
        other => panic!("an interrupted exchange completes, not {other:?}"),
    }) else {
        panic!("a completed exchange reports a replace receipt");
    };
    assert_eq!(
        std::fs::read(&destination).expect("read"),
        b"v2",
        "the published file stays published"
    );
    assert_eq!(
        std::fs::read(&backup_path).expect("the backup is reconstructed"),
        b"v1",
        "the displaced entry becomes the durable backup"
    );
}

#[test]
fn machine_install_directories_stay_traversable() {
    let root = tempfile::tempdir().expect("a temp directory");
    let destination = root.path().join("install").join("tool");
    let (mut executor, mut transaction) =
        executor_for(&destination, b"#!/bin/sh\n", true, FilePrecondition::Absent);
    executor = executor.for_machine();
    stage(&mut executor, &mut transaction, b"#!/bin/sh\n").expect("stage");
    executor
        .apply(&mutation(&transaction), &transaction)
        .expect("create");

    assert_eq!(
        mode_of(&root.path().join("install")),
        0o755,
        "a machine install directory is traversable by regular users"
    );
    assert_eq!(mode_of(&destination), 0o755);

    let user_destination = root.path().join("user-install").join("tool");
    let (mut user, mut user_transaction) = executor_for(
        &user_destination,
        b"#!/bin/sh\n",
        true,
        FilePrecondition::Absent,
    );
    stage(&mut user, &mut user_transaction, b"#!/bin/sh\n").expect("stage");
    user.apply(&mutation(&user_transaction), &user_transaction)
        .expect("create");
    assert_eq!(
        mode_of(&root.path().join("user-install")),
        0o700,
        "a user install directory stays private"
    );
}

#[test]
fn denied_ancestry_is_an_error_not_absence() {
    use std::os::unix::fs::PermissionsExt as _;

    if rustix::process::geteuid().as_raw() == 0 {
        return;
    }
    let root = tempfile::tempdir().expect("a temp directory");
    let install = root.path().join("install");
    std::fs::create_dir_all(&install).expect("install");
    let destination = install.join("app.dat");
    let (mut executor, transaction) = executor_for(
        &destination,
        b"the payload",
        false,
        FilePrecondition::Absent,
    );

    std::fs::set_permissions(&install, std::fs::Permissions::from_mode(0o000)).expect("deny");
    let outcome = executor.prepare(&mutation(&transaction));
    std::fs::set_permissions(&install, std::fs::Permissions::from_mode(0o700)).expect("restore");

    assert!(
        outcome.is_err(),
        "a directory that cannot be observed is not reported as absent"
    );
}

#[test]
fn large_payload_streams_with_bounded_memory() {
    use zup_bundle::DirectoryPayloadSource;

    const PAYLOAD_BYTES: u64 = 256 * 1024 * 1024;
    const PEAK_KB: u64 = 200 * 1024;

    let root = tempfile::tempdir().expect("a temp directory");
    let payload_root = root.path().join("payload");
    let install = root.path().join("install");
    std::fs::create_dir_all(&payload_root).expect("payload");
    let chunk = vec![0xA5u8; 1024 * 1024];
    let source_file = payload_root.join("big.bin");
    {
        let mut out = std::fs::File::create(&source_file).expect("create");
        use std::io::Write as _;
        for _ in 0..256 {
            out.write_all(&chunk).expect("write");
        }
        out.sync_all().expect("sync");
    }
    let digest = {
        let file = std::fs::File::open(&source_file).expect("open");
        hash_reader(file).expect("bytes hash").1
    };
    let destination = install.join("big.bin");
    let target = zup_core::Target::parse("linux-x64").expect("a target");
    let host_destination =
        zup_core::TargetPath::new(target, destination.to_string_lossy()).expect("a path");
    let mut effects = zup_plan::DeploymentEffects::new(
        zup_core::InstallationId::new_v7(),
        requirements_fixture::requirements(
            target,
            zup_core::SelectedScope::User,
            "example.application",
            "1.0.0",
        ),
    );
    effects.install_directory =
        Some(zup_core::TargetPath::new(target, install.to_string_lossy()).unwrap());
    effects.files.push(zup_plan::FileOperation {
        key: file_key(),
        replacement: zup_core::FixedReplacement::Atomic,
        kind: zup_plan::FileOperationKind::Create,
        destination: host_destination,
        source_relative: zup_core::RelativePath::new("big.bin").unwrap(),
        precondition: FilePrecondition::Absent,
        expected_sha256: digest,
        expected_size: PAYLOAD_BYTES,
        privilege: zup_core::Privilege::User,
        executable: false,
        conflict: None,
    });
    let transaction = TransactionContext::new(zup_plan::compile_deployment(&effects).unwrap());
    let mut executor =
        LinuxFileExecutor::new().with_payload(DirectoryPayloadSource::new(&payload_root));
    executor.set_transaction_id(transaction.transaction_id);
    executor.register_plan(&transaction.plan).unwrap();
    let stage = transaction
        .plan
        .nodes
        .iter()
        .find(|node| matches!(node.kind, zup_plan::NodeKind::StageFile { .. }))
        .unwrap();

    let receipt = executor
        .apply(stage, &transaction)
        .expect("a large stage applies");
    assert!(matches!(receipt, OperationReceipt::StageFile { size, .. } if size == PAYLOAD_BYTES));

    let peak = peak_kb().expect("a peak measurement");
    assert!(
        peak < PEAK_KB,
        "staging 256 MiB peaks at {peak} KiB, below {PEAK_KB} KiB"
    );
}

fn peak_kb() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    status
        .lines()
        .find_map(|line| line.strip_prefix("VmHWM:"))
        .and_then(|value| value.split_whitespace().next()?.parse().ok())
}

fn file_object(path: &Path) -> zup_core::NativeObjectIdentity {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::symlink_metadata(path).unwrap();
    zup_core::NativeObjectIdentity::Linux {
        device: metadata.dev(),
        inode: metadata.ino(),
    }
}

#[test]
fn rollback_cannot_consume_another_transactions_backup() {
    let root = tempfile::tempdir().unwrap();
    let destination = root.path().join("application.bin");
    std::fs::write(&destination, b"previous").unwrap();
    let (mut executor, mut transaction) = executor_for(
        &destination,
        b"next",
        false,
        FilePrecondition::Exact {
            object: file_object(&destination),
            size: 8,
            sha256: digest(b"previous"),
        },
    );
    stage(&mut executor, &mut transaction, b"next").unwrap();
    let planned = mutation(&transaction);
    let receipt = executor.apply(&planned, &transaction).unwrap();
    let OperationReceipt::ReplaceFile {
        ref backup_path, ..
    } = receipt
    else {
        unreachable!()
    };
    executor.set_transaction_id(zup_core::TransactionId::new_v7());
    assert!(executor.rollback(&planned, &receipt).is_err());
    assert_eq!(std::fs::read(&destination).unwrap(), b"next");
    assert_eq!(std::fs::read(backup_path).unwrap(), b"previous");
}
