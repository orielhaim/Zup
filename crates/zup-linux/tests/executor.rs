#![cfg(target_os = "linux")]

use rstest::rstest;
use zup_core::{SelectedScope, Sha256Digest, hash_reader};
use zup_linux::{
    EntryKind, ExecError, FileIntent, FileWork, LinuxFileExecutor, OwnedDirectory, PathError,
    STATE_DIRECTORY_MODE, STATE_FILE_MODE,
};
use zup_transaction::{
    FilePrecondition, FileRemoval, FileRemovalKind, OperationId, OperationReceipt,
};

use std::path::{Path, PathBuf};

fn digest(bytes: &[u8]) -> Sha256Digest {
    hash_reader(bytes).expect("bytes hash").1
}

fn intent(bytes: &[u8], executable: bool) -> FileIntent {
    FileIntent {
        sha256: digest(bytes),
        size: bytes.len() as u64,
        executable,
    }
}

fn id(n: u32) -> OperationId {
    OperationId::new(format!("op-{n}"))
}

fn file_key() -> zup_core::ResourceKey {
    zup_core::ResourceKey::File {
        destination: String::from("/tmp/whatever"),
    }
}

fn stage_id() -> OperationId {
    OperationId::stage_file(&file_key())
}

fn executor_for(
    destination: &Path,
    bytes: &[u8],
    executable: bool,
    precondition: FilePrecondition,
) -> LinuxFileExecutor {
    let mut executor = LinuxFileExecutor::new();
    executor.set_transaction_id(zup_transaction::TransactionId::new_v7());
    let work = FileWork {
        destination: zup_platform::TargetPath::new(
            zup_core::Target::parse("linux-x64").expect("a target"),
            destination.to_string_lossy(),
        )
        .expect("a target path"),
        host_path: destination.to_path_buf(),
        intent: intent(bytes, executable),
        precondition,
    };
    executor.register(&id(1), work.clone());
    executor.register(&stage_id(), work);
    executor
}

fn node(delta: zup_transaction::FileDelta) -> zup_transaction::TransactionNode {
    zup_transaction::TransactionNode {
        id: id(1),
        phase: zup_transaction::Phase::FileMutation,
        kind: zup_transaction::NodeKind::FileMutation {
            key: file_key(),
            delta,
        },
        declaration_order: 0,
        meta: Default::default(),
    }
}

fn removal_node(destination: &Path, bytes: &[u8]) -> zup_transaction::TransactionNode {
    let target = zup_core::Target::parse("linux-x64").expect("a target");
    let destination = zup_platform::TargetPath::new(target, destination.to_string_lossy())
        .expect("a target path");
    let removal = FileRemoval {
        key: file_key(),
        kind: FileRemovalKind::RemoveOwned,
        scope: SelectedScope::User,
        privilege: zup_core::Privilege::User,
        destination: destination.clone(),
        sha256: digest(bytes),
        size: bytes.len() as u64,
        created_directories: Vec::new(),
    };
    zup_transaction::TransactionNode {
        id: id(1),
        phase: zup_transaction::Phase::FileMutation,
        kind: zup_transaction::NodeKind::FileRemoval { key: file_key() },
        declaration_order: 0,
        meta: zup_transaction::NodeMeta {
            privilege: Some(zup_core::Privilege::User),
            removal: Some(removal),
            ..Default::default()
        },
    }
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
    let mut executor = executor_for(
        &destination,
        b"the payload",
        false,
        FilePrecondition::Absent,
    );
    executor.stage(&stage_id(), b"the payload").expect("stage");

    let receipt = executor
        .apply(&node(zup_transaction::FileDelta::Create))
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
    assert_eq!(*installed_sha256, digest(b"the payload").to_hex());
    assert!(!executable);
    assert_eq!(
        created_directories.len(),
        1,
        "the install directory is recorded so an uninstall knows what zup made"
    );
    assert!(
        executor
            .verify(&node(zup_transaction::FileDelta::Create), &receipt)
            .is_ok()
    );
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
    let mut executor = executor_for(
        &destination,
        b"#!/bin/sh\n",
        declared,
        FilePrecondition::Absent,
    );
    executor.stage(&stage_id(), b"#!/bin/sh\n").expect("stage");

    let receipt = executor
        .apply(&node(zup_transaction::FileDelta::Create))
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

    let mut executor = executor_for(
        &destination,
        b"the payload",
        false,
        FilePrecondition::Absent,
    );
    executor.stage(&stage_id(), b"the payload").expect("stage");

    let error = executor
        .apply(&node(zup_transaction::FileDelta::Create))
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

    let mut executor = executor_for(
        &destination,
        b"v2",
        false,
        FilePrecondition::Exact {
            size: 2,
            sha256: digest(b"v1"),
        },
    );
    executor.stage(&stage_id(), b"v2").expect("stage");

    let receipt = executor
        .apply(&node(zup_transaction::FileDelta::Replace))
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
    assert_eq!(previous_sha256, &digest(b"v1").to_hex());
    assert_eq!(
        std::fs::read(backup_path.as_str()).expect("the backup is readable"),
        b"v1",
        "the backup holds the previous contents, not the new ones"
    );
}

#[test]
fn a_replace_refuses_to_overwrite_a_file_that_changed() {
    let root = tempfile::tempdir().expect("a temp directory");
    let destination = root.path().join("install").join("app.dat");
    std::fs::create_dir_all(root.path().join("install")).expect("install");
    std::fs::write(&destination, b"a user's edit").expect("write");

    let mut executor = executor_for(
        &destination,
        b"v2",
        false,
        FilePrecondition::Exact {
            size: 2,
            sha256: digest(b"v1"),
        },
    );
    executor.stage(&stage_id(), b"v2").expect("stage");

    let error = executor
        .apply(&node(zup_transaction::FileDelta::Replace))
        .expect_err("a replace over changed bytes is refused");
    assert!(matches!(error, ExecError::PlanDrift { .. }), "{error}");
    assert_eq!(
        std::fs::read(&destination).expect("read"),
        b"a user's edit",
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
    let mut executor = executor_for(
        &destination,
        b"the payload",
        false,
        FilePrecondition::Absent,
    );
    executor.stage(&stage_id(), b"the payload").expect("stage");

    std::fs::remove_dir_all(&install).expect("remove the staged sibling");
    std::os::unix::fs::symlink(elsewhere.path(), &install).expect("symlink");

    let outcome = executor.apply(&node(zup_transaction::FileDelta::Create));
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

    let mut executor = executor_for(
        &destination,
        b"the payload",
        false,
        FilePrecondition::Absent,
    );
    executor.stage(&stage_id(), b"the payload").expect("stage");

    let error = executor
        .apply(&node(zup_transaction::FileDelta::Create))
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

    let mut executor = executor_for(
        &destination,
        b"the payload",
        false,
        FilePrecondition::Absent,
    );
    executor.stage(&stage_id(), b"the payload").expect("stage");

    assert!(
        executor
            .apply(&node(zup_transaction::FileDelta::Create))
            .is_err(),
        "{planted:?} where a regular file belongs must be refused"
    );
}

#[test]
fn a_removal_unlinks_and_backs_up() {
    let root = tempfile::tempdir().expect("a temp directory");
    let destination = root.path().join("app.dat");
    std::fs::write(&destination, b"the payload").expect("write");

    let mut executor = executor_for(
        &destination,
        b"the payload",
        false,
        FilePrecondition::Exact {
            size: 11,
            sha256: digest(b"the payload"),
        },
    );

    let receipt = executor
        .apply(&removal_node(&destination, b"the payload"))
        .expect("remove");

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
    let mut executor = executor_for(
        &destination,
        b"the payload",
        false,
        FilePrecondition::Absent,
    );
    executor.stage(&stage_id(), b"the payload").expect("stage");

    let receipt = executor
        .apply(&node(zup_transaction::FileDelta::Create))
        .expect("create");

    std::fs::write(root.path().join("install").join("user.dat"), b"the user's").expect("write");

    executor
        .rollback(&node(zup_transaction::FileDelta::Create), &receipt)
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
    let mut executor = executor_for(
        &destination,
        b"the payload",
        false,
        FilePrecondition::Absent,
    );
    executor.stage(&stage_id(), b"the payload").expect("stage");
    let receipt = executor
        .apply(&node(zup_transaction::FileDelta::Create))
        .expect("create");

    std::fs::write(&destination, b"the user's edit").expect("write");

    assert!(
        executor
            .rollback(&node(zup_transaction::FileDelta::Create), &receipt)
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

    let mut executor = executor_for(
        &destination,
        b"v2",
        false,
        FilePrecondition::Exact {
            size: 2,
            sha256: digest(b"v1"),
        },
    );
    executor.stage(&stage_id(), b"v2").expect("stage");
    let receipt = executor
        .apply(&node(zup_transaction::FileDelta::Replace))
        .expect("replace");

    executor
        .rollback(&node(zup_transaction::FileDelta::Replace), &receipt)
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
    let mut executor = executor_for(
        &destination,
        b"the payload",
        false,
        FilePrecondition::Absent,
    );
    executor.stage(&stage_id(), b"the payload").expect("stage");
    let receipt = executor
        .apply(&node(zup_transaction::FileDelta::Create))
        .expect("create");
    if !applied {
        std::fs::remove_file(&destination).expect("undo");
    }

    let outcome = executor
        .reconcile(&node(zup_transaction::FileDelta::Create), Some(&receipt))
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
fn a_staged_file_is_named_for_its_operation() {
    let root = tempfile::tempdir().expect("a temp directory");
    let destination = root.path().join("install").join("app.dat");
    let mut executor = executor_for(
        &destination,
        b"the payload",
        false,
        FilePrecondition::Absent,
    );

    let receipt = executor.stage(&stage_id(), b"the payload").expect("stage");
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
    let mut executor = executor_for(
        &destination,
        b"#!/bin/sh\necho v1\n",
        true,
        FilePrecondition::Exact {
            size: 18,
            sha256: digest(b"#!/bin/sh\necho v1\n"),
        },
    );

    let receipt = executor
        .apply(&removal_node(&destination, b"#!/bin/sh\necho v1\n"))
        .expect("remove");
    assert!(!destination.exists(), "the file is unlinked");

    executor
        .rollback(
            &removal_node(&destination, b"#!/bin/sh\necho v1\n"),
            &receipt,
        )
        .expect("rollback");

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
    let mut executor = executor_for(
        &destination,
        b"v2",
        false,
        FilePrecondition::Exact {
            size: 2,
            sha256: digest(b"v1"),
        },
    );
    executor.stage(&stage_id(), b"v2").expect("stage");
    let receipt = executor
        .apply(&node(zup_transaction::FileDelta::Replace))
        .expect("replace");
    assert_eq!(mode_of(&destination), 0o644);

    executor
        .rollback(&node(zup_transaction::FileDelta::Replace), &receipt)
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
    let mut executor = executor_for(
        &destination,
        b"v2",
        false,
        FilePrecondition::Exact {
            size: 2,
            sha256: digest(b"v1"),
        },
    );
    executor.stage(&stage_id(), b"v2").expect("stage");
    let receipt = executor
        .apply(&node(zup_transaction::FileDelta::Replace))
        .expect("replace");
    let node = node(zup_transaction::FileDelta::Replace);

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
    let mut executor = executor_for(
        &destination,
        b"v2",
        false,
        FilePrecondition::Exact {
            size: 2,
            sha256: digest(b"v1"),
        },
    );
    executor.stage(&stage_id(), b"v2").expect("stage");
    let receipt = executor
        .apply(&node(zup_transaction::FileDelta::Replace))
        .expect("replace");
    std::fs::write(&destination, b"a user's edit").expect("write");

    assert!(
        executor
            .rollback(&node(zup_transaction::FileDelta::Replace), &receipt)
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
    let mut executor = executor_for(
        &destination,
        b"the payload",
        false,
        FilePrecondition::Exact {
            size: 11,
            sha256: digest(b"the payload"),
        },
    );
    let receipt = executor
        .apply(&removal_node(&destination, b"the payload"))
        .expect("remove");
    std::fs::write(&destination, b"someone else's file").expect("write");

    assert!(
        executor
            .rollback(&removal_node(&destination, b"the payload"), &receipt)
            .is_err(),
        "restoring over a foreign file would destroy user data"
    );
    assert_eq!(
        std::fs::read(&destination).expect("read"),
        b"someone else's file"
    );
}

#[test]
fn a_replace_rollback_refuses_a_damaged_backup() {
    let root = tempfile::tempdir().expect("a temp directory");
    let destination = root.path().join("install").join("app.dat");
    std::fs::create_dir_all(root.path().join("install")).expect("install");
    std::fs::write(&destination, b"v1").expect("write v1");
    let mut executor = executor_for(
        &destination,
        b"v2",
        false,
        FilePrecondition::Exact {
            size: 2,
            sha256: digest(b"v1"),
        },
    );
    executor.stage(&stage_id(), b"v2").expect("stage");
    let receipt = executor
        .apply(&node(zup_transaction::FileDelta::Replace))
        .expect("replace");
    let OperationReceipt::ReplaceFile { backup_path, .. } = &receipt else {
        panic!("a replace reports a replace receipt: {receipt:?}");
    };
    std::fs::write(backup_path, b"garbage").expect("damage the backup");

    assert!(
        executor
            .rollback(&node(zup_transaction::FileDelta::Replace), &receipt)
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
    let mut executor = executor_for(
        &destination,
        b"v2",
        false,
        FilePrecondition::Exact {
            size: 2,
            sha256: digest(b"v1"),
        },
    );
    executor.stage(&stage_id(), b"v2").expect("stage");
    let receipt = executor
        .apply(&node(zup_transaction::FileDelta::Replace))
        .expect("replace");
    let OperationReceipt::ReplaceFile { backup_path, .. } = &receipt else {
        panic!("a replace reports a replace receipt: {receipt:?}");
    };
    std::fs::remove_file(backup_path).expect("lose the backup");

    assert_eq!(
        executor
            .reconcile(&node(zup_transaction::FileDelta::Replace), Some(&receipt))
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
    let mut executor = executor_for(
        &destination,
        b"v1",
        false,
        FilePrecondition::Exact {
            size: 2,
            sha256: digest(b"v1"),
        },
    );
    let planned = removal_node(&destination, b"v1");

    std::fs::write(&destination, b"v2-external").expect("an outside edit");

    let error = executor
        .prepare(&planned)
        .expect_err("preparation sees the outside edit");
    assert!(
        matches!(error, ExecError::PlanDrift { .. }),
        "a changed file is drift, not absence: {error}"
    );
    let error = executor
        .apply(&planned)
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
    let mut executor = executor_for(
        &destination,
        b"the payload",
        false,
        FilePrecondition::Exact {
            size: 11,
            sha256: digest(b"the payload"),
        },
    );
    let bare = zup_transaction::TransactionNode {
        id: id(1),
        phase: zup_transaction::Phase::Backend,
        kind: zup_transaction::NodeKind::FileRemoval { key: file_key() },
        declaration_order: 0,
        meta: Default::default(),
    };

    assert!(
        matches!(executor.prepare(&bare), Err(ExecError::PlanDrift { .. })),
        "existence alone never authorizes a removal"
    );
    assert!(
        matches!(executor.apply(&bare), Err(ExecError::PlanDrift { .. })),
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
    let mut executor = executor_for(
        &destination,
        b"the payload",
        false,
        FilePrecondition::Exact {
            size: 11,
            sha256: digest(b"the payload"),
        },
    );
    let planned = removal_node(&destination, b"the payload");

    let first = executor.apply(&planned).expect("remove");
    let second = executor.apply(&planned).expect("a replay after a crash");

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
    let mut executor = executor_for(
        &destination,
        b"v2",
        false,
        FilePrecondition::Exact {
            size: 2,
            sha256: digest(b"v1"),
        },
    );
    executor.stage(&stage_id(), b"v2").expect("stage");
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
        .reconcile(&node(zup_transaction::FileDelta::Replace), None)
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
    let mut executor =
        executor_for(&destination, b"#!/bin/sh\n", true, FilePrecondition::Absent).for_machine();
    executor.stage(&stage_id(), b"#!/bin/sh\n").expect("stage");
    executor
        .apply(&node(zup_transaction::FileDelta::Create))
        .expect("create");

    assert_eq!(
        mode_of(&root.path().join("install")),
        0o755,
        "a machine install directory is traversable by regular users"
    );
    assert_eq!(mode_of(&destination), 0o755);

    let user_destination = root.path().join("user-install").join("tool");
    let mut user = executor_for(
        &user_destination,
        b"#!/bin/sh\n",
        true,
        FilePrecondition::Absent,
    );
    user.stage(&stage_id(), b"#!/bin/sh\n").expect("stage");
    user.apply(&node(zup_transaction::FileDelta::Create))
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
    let mut executor = executor_for(
        &destination,
        b"the payload",
        false,
        FilePrecondition::Absent,
    );

    std::fs::set_permissions(&install, std::fs::Permissions::from_mode(0o000)).expect("deny");
    let outcome = executor.prepare(&node(zup_transaction::FileDelta::Create));
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
        zup_platform::TargetPath::new(target, destination.to_string_lossy()).expect("a path");
    let stage_key = zup_core::ResourceKey::File {
        destination: host_destination.to_string(),
    };
    let stage_operation = OperationId::stage_file(&stage_key);
    let mut executor =
        LinuxFileExecutor::new().with_payload(DirectoryPayloadSource::new(&payload_root));
    executor.register(
        &stage_operation,
        FileWork {
            destination: host_destination,
            host_path: destination.clone(),
            intent: FileIntent {
                sha256: digest,
                size: PAYLOAD_BYTES,
                executable: false,
            },
            precondition: FilePrecondition::Absent,
        },
    );
    let stage = zup_transaction::TransactionNode {
        id: stage_operation,
        phase: zup_transaction::Phase::Stage,
        kind: zup_transaction::NodeKind::StageFile { key: stage_key },
        declaration_order: 0,
        meta: zup_transaction::NodeMeta {
            source_relative: Some(zup_core::RelativePath::new("big.bin").expect("a relative path")),
            file_precondition: Some(FilePrecondition::Absent),
            expected_sha256: Some(digest),
            expected_size: Some(PAYLOAD_BYTES),
            privilege: Some(zup_core::Privilege::User),
            executable: Some(false),
            ..Default::default()
        },
    };

    let receipt = executor.apply(&stage).expect("a large stage applies");
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
