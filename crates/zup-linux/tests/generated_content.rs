#![cfg(all(target_os = "linux", feature = "test-support"))]

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use zup_core::{RelativePath, ResourceKey, SelectedScope, Sha256Digest, Target};
use zup_linux::test_support::{compose_fixture, v1_files};
use zup_platform::TargetPath;
use zup_runtime::{
    ExecutionPolicy, InstallOutcome, OperationControl, RuntimeControl, RuntimeRequest,
};
use zup_transaction::{
    FileDelta, FilePrecondition, FileWork, FilesystemTransactionStore, InstallationLock,
    TransactionInput, TransactionStore, compile_transaction,
};

fn target() -> Target {
    Target::parse("linux-x64").expect("target")
}

fn digest(bytes: &[u8]) -> (u64, Sha256Digest) {
    zup_core::hash_reader(bytes).expect("hash")
}

fn plan_for(bytes: &[u8]) -> zup_transaction::TransactionPlan {
    let (size, sha256) = digest(bytes);
    let destination = "/tmp/zup-generated-ownership.desktop";
    let mut input = TransactionInput::new(target());
    input.files.push(FileWork {
        key: ResourceKey::File {
            destination: destination.into(),
        },
        source_relative: RelativePath::new("__zup_generated__/applications/app.desktop")
            .expect("source"),
        destination: TargetPath::new(target(), destination).expect("destination"),
        precondition: FilePrecondition::Absent,
        expected_sha256: sha256,
        expected_size: size,
        privilege: zup_core::Privilege::User,
        executable: false,
        delta: FileDelta::Create,
    });
    compile_transaction(&input).expect("plan")
}

fn app() -> zup_core::AppId {
    zup_core::AppId::new("com.example.generated").expect("id")
}

fn lock_for(root: &Path, id: &zup_core::AppId) -> InstallationLock {
    let key = InstallationLock::lock_key(id.as_str(), "user");
    InstallationLock::try_acquire(root, &key)
        .expect("lock")
        .expect("acquired")
}

fn stage_with(root: &Path, lock: &InstallationLock, name: &str, bytes: &[u8]) {
    let mut generated = BTreeMap::new();
    generated.insert(name.into(), bytes.to_vec());
    zup_linux::stage_generated(root, &app(), SelectedScope::User, lock, &generated).expect("stage");
}

fn stage(root: &Path, name: &str, bytes: &[u8]) {
    let held = lock_for(root, &app());
    stage_with(root, &held, name, bytes);
}

fn object(root: &Path, bytes: &[u8]) -> std::path::PathBuf {
    let (_, sha) = digest(bytes);
    root.join("objects")
        .join(InstallationLock::lock_key(app().as_str(), "user"))
        .join(sha.to_hex())
}

#[test]
fn a_later_operation_cannot_replace_an_unsettled_transactions_inputs() {
    let scratch = tempfile::tempdir().expect("scratch");
    let root = scratch.path();
    let original = b"[Desktop Entry]\nName=Original\n";
    let source = "__zup_generated__/applications/app.desktop";
    let plan = plan_for(original);
    stage(root, source, original);
    let object = object(root, original);
    let mut record = zup_transaction::TransactionRecord::new(
        zup_transaction::TransactionId::new_v7(),
        app(),
        SelectedScope::User,
        semver::Version::new(1, 0, 0),
        plan.clone(),
    );
    record.phase = zup_transaction::TransactionPhase::Applying;
    let id = record.transaction_id;
    FilesystemTransactionStore::private_at(root)
        .create(&record)
        .expect("journal");

    stage(root, source, b"[Desktop Entry]\nName=Replacement\n");
    assert_eq!(std::fs::read(&object).expect("original object"), original);
    let held = lock_for(root, &app());
    let loaded = zup_linux::generated_for_plan(root, &app(), SelectedScope::User, &held, &plan)
        .expect("recovery bytes");
    assert_eq!(loaded.get(source).expect("source"), original);
    zup_linux::reap_generated(root, &app(), SelectedScope::User, &held).expect("reap");
    drop(held);
    assert_eq!(
        std::fs::read(&object).expect("retained"),
        original,
        "an unsettled journal keeps its inputs"
    );
    assert!(
        !root.join("generated").exists(),
        "content is not published in the old shared directory"
    );

    std::fs::remove_dir_all(root.join("transactions").join(id.to_string())).expect("journal");
    zup_linux::reap_generated(root, &app(), SelectedScope::User, &lock_for(root, &app()))
        .expect("reap settled");
    assert!(!object.exists(), "content with no owner is removed");
}

#[test]
fn a_corrupt_object_is_left_in_place() {
    let scratch = tempfile::tempdir().expect("scratch");
    let root = scratch.path();
    let bytes = b"[Desktop Entry]\nName=Complete\n";
    let plan = plan_for(bytes);
    let (_, sha) = digest(bytes);
    let key = InstallationLock::lock_key(app().as_str(), "user");
    let directory = private_fs::PrivateDir::ensure(root.join("objects"))
        .expect("objects")
        .ensure_dir(&key)
        .expect("app objects");
    directory
        .write_atomic_durable(sha.to_hex(), b"partial")
        .expect("partial");
    let held = lock_for(root, &app());
    let error = zup_linux::generated_for_plan(root, &app(), SelectedScope::User, &held, &plan)
        .expect_err("partial content");
    assert!(error.to_string().contains("does not match"), "{error}");
    let error = zup_linux::stage_generated(
        root,
        &app(),
        SelectedScope::User,
        &held,
        &BTreeMap::from([(
            "__zup_generated__/applications/app.desktop".into(),
            bytes.to_vec(),
        )]),
    )
    .expect_err("corrupt objects are not replaced");
    assert!(error.to_string().contains("does not match"), "{error}");
    assert_eq!(
        std::fs::read(object(root, bytes)).expect("corrupt object"),
        b"partial"
    );
}

#[test]
fn a_busy_operation_does_not_stage_generated_content() {
    let scratch = tempfile::tempdir().expect("scratch");
    let installer = compose_fixture(scratch.path(), "setup", "1.0.0", &v1_files());
    let state = scratch.path().join("state");
    std::fs::create_dir_all(&state).expect("state");
    let key = InstallationLock::lock_key("com.example.fresh", "user");
    let _held = InstallationLock::try_acquire(&state, &key)
        .expect("lock")
        .expect("acquired");
    let operation = OperationControl::new(ExecutionPolicy::NonInteractive);
    let runtime = RuntimeControl::new(
        operation.cancellation(),
        operation.events(),
        operation.policy(),
    );
    let mut backend = zup_linux::LinuxRuntimeBackend {
        installer,
        operation: zup_runtime::Operation::Install,
        selection: zup_runtime::OperationSelection {
            enable: Vec::new(),
            disable: Vec::new(),
            install_directory: None,
        },
        install_dir_override: None,
        generated: BTreeMap::from([(
            "__zup_generated__/applications/app.desktop".into(),
            b"[Desktop Entry]\n".to_vec(),
        )]),
    };
    let plan = compile_transaction(&TransactionInput::new(target())).expect("plan");
    let report = backend
        .execute_blocking(
            &RuntimeRequest {
                target: plan.target,
                app_id: zup_core::AppId::new("com.example.fresh").expect("id"),
                app_version: semver::Version::new(1, 0, 0),
                scope: SelectedScope::User,
                transaction_plan: plan,
                state_root: state.clone(),
                work_root: state.join("work"),
                recovery_id: None,
                bootstrap: None,
                release: None,
            },
            &runtime,
        )
        .expect("busy");
    assert!(matches!(report.outcome, InstallOutcome::Busy { .. }));
    assert!(!state.join("objects").exists());
    let _ = &mut backend;
}

#[test]
fn concurrent_writers_publish_one_immutable_object() {
    let scratch = tempfile::tempdir().expect("scratch");
    let root = scratch.path();
    let bytes = b"[Desktop Entry]\nName=Shared\n";
    let key = InstallationLock::lock_key(app().as_str(), "user");
    let held = lock_for(root, &app());
    stage_with(root, &held, "payload", bytes);
    let (started, started_rx) = std::sync::mpsc::channel();
    let waiter = std::thread::spawn({
        let root = root.to_path_buf();
        move || {
            assert!(
                InstallationLock::try_acquire(&root, &key)
                    .expect("attempt")
                    .is_none(),
                "the holder excludes a second writer"
            );
            started.send(()).expect("started");
        }
    });
    started_rx.recv().expect("waiter saw the lock");
    drop(held);
    waiter.join().expect("waiter");
    stage(root, "payload", bytes);
    assert_eq!(std::fs::read(object(root, bytes)).expect("object"), bytes);
}

#[test]
fn concurrent_writers_publish_distinct_objects() {
    let scratch = tempfile::tempdir().expect("scratch");
    let root = scratch.path();
    let first = b"first-object";
    let second = b"second-object";
    let other = zup_core::AppId::new("com.example.other").expect("id");
    let barrier = std::sync::Barrier::new(2);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            barrier.wait();
            stage(root, "payload", first);
        });
        scope.spawn(|| {
            let held = lock_for(root, &other);
            barrier.wait();
            let mut generated = BTreeMap::new();
            generated.insert("payload".into(), second.to_vec());
            zup_linux::stage_generated(root, &other, SelectedScope::User, &held, &generated)
                .expect("other stage");
        });
    });
    assert_eq!(std::fs::read(object(root, first)).expect("first"), first);
    let other_object = root
        .join("objects")
        .join(InstallationLock::lock_key(other.as_str(), "user"))
        .join(digest(second).1.to_hex());
    assert_eq!(std::fs::read(other_object).expect("second"), second);
}

#[test]
fn another_application_cannot_collect_staged_objects() {
    let scratch = tempfile::tempdir().expect("scratch");
    let root = scratch.path();
    let bytes = b"owned-by-the-staging-app";
    stage(root, "payload", bytes);
    let other = zup_core::AppId::new("com.example.other").expect("id");
    zup_linux::reap_generated(root, &other, SelectedScope::User, &lock_for(root, &other))
        .expect("other reap");
    assert_eq!(std::fs::read(object(root, bytes)).expect("kept"), bytes);
    zup_linux::reap_generated(root, &app(), SelectedScope::User, &lock_for(root, &app()))
        .expect("owner reap");
    assert!(!object(root, bytes).exists());
}

#[test]
fn a_prepared_journal_does_not_keep_objects_an_applying_journal_does() {
    let scratch = tempfile::tempdir().expect("scratch");
    let root = scratch.path();
    let bytes = b"needed-for-replay";
    stage(root, "payload", bytes);
    let plan = plan_for(bytes);
    let mut prepared = zup_transaction::TransactionRecord::new(
        zup_transaction::TransactionId::new_v7(),
        app(),
        SelectedScope::User,
        semver::Version::new(1, 0, 0),
        plan,
    );
    FilesystemTransactionStore::private_at(root)
        .create(&prepared)
        .expect("prepared");
    zup_linux::reap_generated(root, &app(), SelectedScope::User, &lock_for(root, &app()))
        .expect("reap prepared");
    assert!(
        !object(root, bytes).exists(),
        "rollback of a prepared journal does not read generated bytes"
    );

    stage(root, "payload", bytes);
    prepared.phase = zup_transaction::TransactionPhase::Applying;
    prepared.transaction_id = zup_transaction::TransactionId::new_v7();
    FilesystemTransactionStore::private_at(root)
        .create(&prepared)
        .expect("applying");
    zup_linux::reap_generated(root, &app(), SelectedScope::User, &lock_for(root, &app()))
        .expect("reap applying");
    assert_eq!(std::fs::read(object(root, bytes)).expect("retained"), bytes);
}

#[test]
fn garbage_collection_waits_for_the_installation_lock() {
    let scratch = tempfile::tempdir().expect("scratch");
    let root = scratch.path();
    let bytes = b"staged-under-the-lock";
    let key = InstallationLock::lock_key(app().as_str(), "user");
    let held = InstallationLock::try_acquire(root, &key)
        .expect("lock")
        .expect("held");
    stage_with(root, &held, "payload", bytes);
    let (started, started_rx) = std::sync::mpsc::channel();
    let (observed, observed_rx) = std::sync::mpsc::channel();
    let collector = std::thread::spawn({
        let root = root.to_path_buf();
        move || {
            started.send(()).expect("started");
            assert!(
                InstallationLock::try_acquire(&root, &key)
                    .expect("attempt")
                    .is_none()
            );
            observed.send(()).expect("observed");
        }
    });
    started_rx.recv().expect("collector started");
    assert!(object(root, bytes).exists());
    observed_rx.recv().expect("collector saw the lock");
    drop(held);
    collector.join().expect("collector");
}

#[test]
fn a_world_writable_object_directory_is_refused() {
    let scratch = tempfile::tempdir().expect("scratch");
    let root = scratch.path();
    stage(root, "payload", b"private-bytes");
    let objects = root.join("objects");
    let mut permissions = std::fs::metadata(&objects).expect("stat").permissions();
    permissions.set_mode(0o777);
    std::fs::set_permissions(&objects, permissions).expect("chmod");
    let error = zup_linux::stage_generated(
        root,
        &app(),
        SelectedScope::User,
        &lock_for(root, &app()),
        &BTreeMap::from([("payload".into(), b"private-bytes".to_vec())]),
    )
    .expect_err("loose directory");
    let _ = error;
    assert_eq!(
        std::fs::metadata(&objects)
            .expect("stat")
            .permissions()
            .mode()
            & 0o777,
        0o777
    );
}

#[test]
fn a_symlink_cannot_replace_the_object_directory() {
    let scratch = tempfile::tempdir().expect("scratch");
    let root = scratch.path();
    let elsewhere = scratch.path().join("elsewhere");
    std::fs::create_dir(&elsewhere).expect("dir");
    std::os::unix::fs::symlink(&elsewhere, root.join("objects")).expect("symlink");
    let error = zup_linux::stage_generated(
        root,
        &app(),
        SelectedScope::User,
        &lock_for(root, &app()),
        &BTreeMap::from([("payload".into(), b"bytes".to_vec())]),
    )
    .expect_err("symlink");
    assert!(
        std::fs::read_dir(&elsewhere)
            .expect("elsewhere")
            .next()
            .is_none(),
        "publication must not follow the replacement symlink: {error}"
    );
}
