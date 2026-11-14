#![cfg(windows)]
mod requirements_fixture {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/deployment.rs"
    ));
}
use zup_core::TransactionId;
use zup_plan::RemovalKind;
use zup_plan::{DeploymentEffects, FileRemoval, NodeKind, compile_deployment};

use std::path::{Path, PathBuf};

use tempfile::TempDir;
use zup_bundle::DirectoryPayloadSource;
use zup_core::{Sha256Digest, Target, hash_reader};
use zup_transaction::{LockScope, ResourceLock};
use zup_windows::{WindowsFileExecutor, to_host_path};

fn owned_instance(path: &zup_core::TargetPath) -> zup_core::FixedFileInstance {
    let zup_plan::ObservedFileState::File { object, .. } =
        zup_windows::inspect_file(&to_host_path(path).unwrap()).unwrap()
    else {
        panic!("owned file fixture is absent")
    };
    zup_core::FixedFileInstance {
        origin: TransactionId::new_v7(),
        object,
    }
}

fn target_path(path: impl AsRef<Path>) -> zup_core::TargetPath {
    zup_core::TargetPath::new(
        Target::parse("windows-x64").unwrap(),
        path.as_ref().to_string_lossy(),
    )
    .unwrap()
}

#[test]
fn owned_file_removal_reconciles_and_rolls_back_without_touching_drift() {
    use zup_core::{Privilege, ResourceKey, SelectedScope};
    use zup_transaction::ReconcileResult;

    let (_dir, _payload, target_root, source) = setup();
    let destination = target_path(target_root.join("owned.bin"));
    std::fs::write(to_host_path(&destination).unwrap(), b"owned").unwrap();
    let key = ResourceKey::File {
        id: zup_core::FileResourceId::Payload {
            id: zup_core::PayloadId::File {
                declaration: zup_core::ResourceId::new("fixture-0").unwrap(),
            },
        },
    };
    let mut input = DeploymentEffects::new(
        zup_core::InstallationId::new_v7(),
        requirements_fixture::requirements(
            Target::WINDOWS_X64,
            zup_core::SelectedScope::User,
            "com.zup.crash-removal",
            "1.0.0",
        ),
    );
    input.install_directory = Some(target_path(&target_root));
    input.transition = zup_plan::DeploymentTransition::Remove;
    input.retired_keys.push(key.clone());
    input.removals.push(FileRemoval {
        instance: owned_instance(&destination),
        key: key.clone(),
        kind: RemovalKind::RemoveOwned,
        scope: SelectedScope::User,
        privilege: Privilege::User,
        destination: destination.clone(),
        sha256: digest(b"owned"),
        size: 5,
        created_directories: vec![],
    });
    let plan = compile_deployment(&input).unwrap();
    let node = plan
        .nodes
        .iter()
        .find(|node| matches!(node.kind, NodeKind::FileRemoval { .. }))
        .unwrap();
    let exec = WindowsFileExecutor::new(
        source,
        zup_core::TransactionId::new_v7(),
        Target::parse("windows-x64").unwrap().executable_suffix(),
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
    exec.rollback_transaction_receipt(node, &receipt).unwrap();
    exec.rollback_transaction_receipt(node, &receipt)
        .expect("restoration survives a crash before journal advancement");
    assert_eq!(
        std::fs::read(to_host_path(&destination).unwrap()).unwrap(),
        b"owned"
    );
    let receipt = exec.apply_owned_file_removal(node).unwrap();
    std::fs::write(to_host_path(&destination).unwrap(), b"user data").unwrap();
    assert!(exec.rollback_transaction_receipt(node, &receipt).is_err());
    assert_eq!(
        std::fs::read(to_host_path(&destination).unwrap()).unwrap(),
        b"user data"
    );
}

#[test]
fn removal_recovery_reconstructs_receipt_without_inventing_prior_ownership() {
    use zup_core::{AppId, Privilege, ResourceKey, SelectedScope};
    use zup_transaction::{
        FilesystemTransactionStore, NodeState, OperationExecutor, ReconcileResult,
        TransactionOutcome, TransactionPhase, TransactionRecord, TransactionStore, recover,
    };

    struct RemovalExecutor(WindowsFileExecutor<DirectoryPayloadSource>);
    impl OperationExecutor for RemovalExecutor {
        type Error = String;
        fn prepare(&mut self, _node: &zup_plan::DeploymentNode) -> Result<(), Self::Error> {
            Ok(())
        }
        fn apply(
            &mut self,
            node: &zup_plan::DeploymentNode,
            _transaction: &zup_transaction::TransactionRecord,
        ) -> Result<zup_transaction::OperationReceipt, Self::Error> {
            if matches!(node.kind, zup_plan::NodeKind::Barrier) {
                return Ok(zup_transaction::OperationReceipt::Control);
            }
            self.0
                .apply_owned_file_removal(node)
                .map_err(|error| error.to_string())
        }
        fn verify(
            &mut self,
            node: &zup_plan::DeploymentNode,
            receipt: &zup_transaction::OperationReceipt,
        ) -> Result<(), Self::Error> {
            zup_windows::verify_installed_file(node, receipt).map_err(|error| error.to_string())
        }
        fn rollback(
            &mut self,
            node: &zup_plan::DeploymentNode,
            receipt: &zup_transaction::OperationReceipt,
        ) -> Result<(), Self::Error> {
            self.0
                .rollback_transaction_receipt(node, receipt)
                .map_err(|error| error.to_string())
        }
        fn reconcile(
            &mut self,
            node: &zup_plan::DeploymentNode,
            _: Option<&zup_transaction::OperationReceipt>,
            _transaction: &zup_transaction::TransactionRecord,
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
        id: zup_core::FileResourceId::Payload {
            id: zup_core::PayloadId::File {
                declaration: zup_core::ResourceId::new("fixture-1").unwrap(),
            },
        },
    };
    let mut input = DeploymentEffects::new(
        zup_core::InstallationId::new_v7(),
        requirements_fixture::requirements(
            Target::WINDOWS_X64,
            zup_core::SelectedScope::User,
            "com.zup.crash-removal",
            "1.0.0",
        ),
    );
    input.install_directory = Some(target_path(&target_root));
    input.transition = zup_plan::DeploymentTransition::Remove;
    input.retired_keys.push(key.clone());
    input.removals.push(FileRemoval {
        instance: owned_instance(&destination),
        key,
        kind: RemovalKind::RemoveOwned,
        scope: SelectedScope::User,
        privilege: Privilege::User,
        destination: destination.clone(),
        sha256: digest(b"owned"),
        size: 5,
        created_directories: vec![],
    });
    let plan = compile_deployment(&input).unwrap();
    let store = FilesystemTransactionStore::private_at(dir.path().join("state"));
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
        record.transaction_id,
        Target::parse("windows-x64").unwrap().executable_suffix(),
    ));
    record.phase = TransactionPhase::Applying;
    let reservation = record
        .plan
        .nodes
        .iter()
        .find(|node| matches!(node.kind, NodeKind::ReserveInstallation))
        .unwrap()
        .id
        .clone();
    record.nodes.insert(reservation.clone(), NodeState::Running);
    let revision = record.revision;
    record.touch();
    store.compare_and_swap(revision, &record).unwrap();
    record.reservation = Some(store.reserve_installation(&record).unwrap());
    record.nodes.insert(
        reservation,
        NodeState::Applied {
            receipt: Box::new(zup_transaction::OperationReceipt::Control),
        },
    );
    record.nodes.insert(node.id.clone(), NodeState::Running);
    let revision = record.revision;
    record.touch();
    store.compare_and_swap(revision, &record).unwrap();
    executor.0.apply_owned_file_removal(&node).unwrap();
    assert!(!to_host_path(&destination).unwrap().exists());
    let (record, outcome) = recover(
        record,
        &store,
        &mut executor,
        &zup_windows::InstallationRecordStore::new(dir.path().join("state")),
    )
    .unwrap();
    assert_eq!(outcome, TransactionOutcome::RecoveryRequired);
    assert!(
        matches!(
            record.receipt(&node.id),
            Some(zup_transaction::OperationReceipt::RemoveFile { .. })
        ),
        "the reconciled removal is journaled with its receipt"
    );
    assert!(
        matches!(record.nodes.get(&node.id), Some(NodeState::Verified { .. })),
        "the recovered removal is verified before publication"
    );
    assert!(!to_host_path(&destination).unwrap().exists());
}

fn digest(bytes: &[u8]) -> Sha256Digest {
    hash_reader(bytes).unwrap().1
}

fn setup() -> (TempDir, PathBuf, PathBuf, DirectoryPayloadSource) {
    let dir = tempfile::tempdir_in(std::env::var_os("LOCALAPPDATA").unwrap()).unwrap();
    let payload_root = dir.path().join("payload");
    let target_root = dir.path().join("target");
    let work_root = dir.path().join("work");
    std::fs::create_dir_all(&payload_root).unwrap();
    std::fs::create_dir_all(&target_root).unwrap();
    std::fs::create_dir_all(&work_root).unwrap();
    let src = DirectoryPayloadSource::new(&payload_root);
    (dir, payload_root, target_root, src)
}

#[test]
fn installation_lock_exclusive() {
    let dir = tempfile::tempdir_in(std::env::var_os("LOCALAPPDATA").unwrap()).unwrap();
    let key = ResourceLock::lock_key("com.acme.acme", "user");
    let first = ResourceLock::try_acquire(dir.path(), &key)
        .unwrap()
        .expect("first lock");
    let second = ResourceLock::try_acquire(dir.path(), &key).unwrap();
    assert!(second.is_none(), "second must be busy");
    drop(first);
    let third = ResourceLock::try_acquire(dir.path(), &key).unwrap();
    assert!(third.is_some(), "lock released on drop");
}

#[test]
fn lock_identity_separates_installations_and_joins_the_ones_that_are_one() {
    let acme = ResourceLock::lock_key("com.acme.desktop", "user");
    let other = ResourceLock::lock_key("com.other.desktop", "user");
    let machine = ResourceLock::lock_key("com.acme.desktop", "machine");
    assert_ne!(acme, other, "two applications are two installations");
    assert_ne!(
        acme, machine,
        "a user install and a machine install are separate installations with \
         separate ledgers, separate directories and separate uninstall entries"
    );

    let bootstrap = ResourceLock::key_for("com.acme.desktop", "user", LockScope::Bootstrap);
    let lifecycle = ResourceLock::key_for("com.acme.desktop", "user", LockScope::Lifecycle);
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
        ResourceLock::key_for("com.other.desktop", "user", LockScope::Bootstrap) != bootstrap,
        "two applications are two installations, bootstrap phase included"
    );

    let awkward = ResourceLock::lock_key("com.acme.desktop/../../etc", "user");
    assert!(!awkward.contains('/'), "{awkward}");
    assert!(!awkward.contains(".."), "{awkward}");
}

#[test]
fn a_released_lock_keeps_its_file() {
    let dir = tempfile::tempdir_in(std::env::var_os("LOCALAPPDATA").unwrap()).unwrap();
    let key = ResourceLock::lock_key("com.acme.desktop", "user");
    let path = dir.path().join("locks").join(format!("{key}.lock"));
    let held = ResourceLock::try_acquire(dir.path(), &key)
        .unwrap()
        .expect("first lock");
    drop(held);
    assert!(path.exists(), "releasing the lock does not delete its file");
    let again = ResourceLock::try_acquire(dir.path(), &key)
        .unwrap()
        .expect("the same file locks again");
    drop(again);
}
