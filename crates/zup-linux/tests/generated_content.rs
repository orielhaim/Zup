#![cfg(all(target_os = "linux", feature = "test-support"))]

use std::collections::BTreeMap;
use std::path::Path;

use zup_core::{RelativePath, ResourceKey, SelectedScope, Sha256Digest, Target};
use zup_linux::test_support::{compose_fixture, v1_files};
use zup_platform::TargetPath;
use zup_runtime::{
    ExecutionPolicy, InstallOutcome, OperationControl, RuntimeControl, RuntimeRequest,
};
use zup_transaction::{
    FileDelta, FilePrecondition, FileWork, FilesystemTransactionStore, InstallationLock,
    TransactionCoordinator, TransactionInput, compile_transaction,
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

fn stage(root: &Path, name: &str, bytes: &[u8]) {
    let mut generated = BTreeMap::new();
    generated.insert(name.into(), bytes.to_vec());
    zup_linux::stage_generated(root, &generated).expect("stage");
}

#[test]
fn a_later_operation_cannot_replace_an_unsettled_transactions_inputs() {
    let scratch = tempfile::tempdir().expect("scratch");
    let root = scratch.path();
    let original = b"[Desktop Entry]\nName=Original\n";
    let source = "__zup_generated__/applications/app.desktop";
    let plan = plan_for(original);
    stage(root, source, original);
    let (size, sha) = digest(original);
    let object = root.join("generated").join("objects").join(sha.to_hex());
    let open = TransactionCoordinator::new(FilesystemTransactionStore::new(root))
        .begin(
            zup_core::AppId::new("com.example.generated").expect("id"),
            SelectedScope::User,
            semver::Version::new(1, 0, 0),
            plan.clone(),
        )
        .expect("journal");
    let id = open.record().transaction_id;
    drop(open);

    stage(root, source, b"[Desktop Entry]\nName=Replacement\n");
    assert_eq!(std::fs::read(&object).expect("original object"), original);
    let loaded = zup_linux::generated_for_plan(root, &plan).expect("recovery bytes");
    assert_eq!(loaded.get(source).expect("source"), original);
    zup_linux::reap_generated(root).expect("reap");
    assert_eq!(
        std::fs::read(&object).expect("retained"),
        original,
        "an unsettled journal keeps its inputs"
    );
    assert!(
        !root
            .join("generated")
            .join(format!("{}.json", "user"))
            .exists(),
        "content is not published as a shared generation file"
    );

    std::fs::remove_dir_all(root.join("transactions").join(id.to_string())).expect("journal");
    zup_linux::reap_generated(root).expect("reap settled");
    assert!(!object.exists(), "content with no owner is removed");
    let _ = size;
}

#[test]
fn a_partial_object_is_rejected_and_a_completed_stage_replaces_it() {
    let scratch = tempfile::tempdir().expect("scratch");
    let root = scratch.path();
    let bytes = b"[Desktop Entry]\nName=Complete\n";
    let plan = plan_for(bytes);
    let (_, sha) = digest(bytes);
    let object = root.join("generated").join("objects").join(sha.to_hex());
    std::fs::create_dir_all(object.parent().expect("parent")).expect("dir");
    std::fs::write(&object, b"partial").expect("partial");
    let error = zup_linux::generated_for_plan(root, &plan).expect_err("partial content");
    assert!(error.to_string().contains("does not match"), "{error}");
    stage(root, "__zup_generated__/applications/app.desktop", bytes);
    let loaded = zup_linux::generated_for_plan(root, &plan).expect("repaired");
    assert_eq!(
        loaded
            .get("__zup_generated__/applications/app.desktop")
            .expect("source")
            .as_slice(),
        bytes
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
    assert!(!state.join("generated").exists());
    let _ = &mut backend;
}
