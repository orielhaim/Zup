#![cfg(target_os = "linux")]

use rstest::rstest;
use zup_core::{Sha256Digest, hash_reader};
use zup_linux::{
    EntryKind, ExecError, FileIntent, FileWork, LinuxFileExecutor, OwnedDirectory, PathError,
    STATE_DIRECTORY_MODE, STATE_FILE_MODE,
};
use zup_transaction::{FilePrecondition, OperationId, OperationReceipt};

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

fn removal_node() -> zup_transaction::TransactionNode {
    zup_transaction::TransactionNode {
        id: id(1),
        phase: zup_transaction::Phase::FileMutation,
        kind: zup_transaction::NodeKind::FileRemoval { key: file_key() },
        declaration_order: 0,
        meta: Default::default(),
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
        .apply(&zup_transaction::TransactionNode {
            id: id(1),
            phase: zup_transaction::Phase::FileMutation,
            kind: zup_transaction::NodeKind::FileRemoval {
                key: zup_core::ResourceKey::File {
                    destination: String::from("/tmp/whatever"),
                },
            },
            declaration_order: 0,
            meta: Default::default(),
        })
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

    let receipt = executor.apply(&removal_node()).expect("remove");
    assert!(!destination.exists(), "the file is unlinked");

    executor
        .rollback(&removal_node(), &receipt)
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
    let receipt = executor.apply(&removal_node()).expect("remove");
    std::fs::write(&destination, b"someone else's file").expect("write");

    assert!(
        executor.rollback(&removal_node(), &receipt).is_err(),
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
