mod requirements_fixture {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/deployment.rs"
    ));
}
use std::sync::Mutex;

use zup_core::{AppId, InstallationId, SelectedScope, Target, TargetPath};
use zup_plan::{DeploymentEffects, DeploymentNode, InstallationRecord, compile_deployment};
use zup_transaction::{
    FilesystemTransactionStore, InstallationPublisher, OperationExecutor, OperationReceipt,
    PublicationState, ReconcileResult, TransactionCoordinator, TransactionOutcome,
    TransactionRecord, TransactionStore, prepare_installation_record, recover,
};

#[derive(Default)]
struct Executor {
    rollback_calls: usize,
}

impl OperationExecutor for Executor {
    type Error = String;

    fn prepare(&mut self, _: &DeploymentNode) -> Result<(), String> {
        Ok(())
    }
    fn apply(
        &mut self,
        _: &DeploymentNode,
        _transaction: &zup_transaction::TransactionRecord,
    ) -> Result<OperationReceipt, String> {
        Ok(OperationReceipt::Control)
    }
    fn verify(&mut self, _: &DeploymentNode, _: &OperationReceipt) -> Result<(), String> {
        Ok(())
    }
    fn rollback(&mut self, _: &DeploymentNode, _: &OperationReceipt) -> Result<(), String> {
        self.rollback_calls += 1;
        Ok(())
    }
    fn reconcile(
        &mut self,
        _: &DeploymentNode,
        _: Option<&OperationReceipt>,
        _transaction: &zup_transaction::TransactionRecord,
    ) -> Result<ReconcileResult, String> {
        Ok(ReconcileResult::NotApplied)
    }
}

#[derive(Default)]
struct Publisher {
    installation: Mutex<Option<InstallationRecord>>,
    lose_acknowledgment: Mutex<bool>,
    publication_attempts: Mutex<usize>,
}

impl InstallationPublisher for Publisher {
    type Error = String;

    fn prepare(&self, transaction: &TransactionRecord) -> Result<InstallationRecord, String> {
        prepare_installation_record(transaction, self.installation.lock().unwrap().as_ref())
            .map_err(|error| error.to_string())
    }

    fn publish(&self, transaction: &TransactionRecord) -> Result<InstallationRecord, String> {
        assert!(!matches!(
            transaction.publication,
            PublicationState::NotRequested
        ));
        *self.publication_attempts.lock().unwrap() += 1;
        let mut durable = self.installation.lock().unwrap();
        let installation = prepare_installation_record(transaction, durable.as_ref())
            .map_err(|error| error.to_string())?;
        *durable = Some(installation.clone());
        if std::mem::take(&mut *self.lose_acknowledgment.lock().unwrap()) {
            return Err("publication acknowledgment lost".into());
        }
        Ok(installation)
    }
}

#[rstest::rstest]
#[case(false)]
#[case(true)]
fn publication_with_lost_acknowledgment_recovers_without_compensating(
    #[case] altered_ownership: bool,
) {
    let root = tempfile::tempdir().unwrap();
    let store = FilesystemTransactionStore::private_at(root.path().join("state"));
    let target = Target::LINUX_X64;
    let mut effects = DeploymentEffects::new(
        InstallationId::new_v7(),
        requirements_fixture::requirements(
            target,
            zup_core::SelectedScope::User,
            "example.publication",
            "1.0.0",
        ),
    );
    effects.install_directory = Some(TargetPath::new(target, "/opt/example").unwrap());
    let open = TransactionCoordinator::new(store.clone())
        .begin(
            AppId::new("example.publication").unwrap(),
            SelectedScope::User,
            "1.0.0".parse().unwrap(),
            compile_deployment(&effects).unwrap(),
        )
        .unwrap();
    let publisher = Publisher::default();
    *publisher.lose_acknowledgment.lock().unwrap() = true;
    let mut executor = Executor::default();
    let interrupted = open.execute(&mut executor, &publisher).unwrap();
    assert_eq!(interrupted.outcome, TransactionOutcome::RecoveryRequired);
    assert_eq!(executor.rollback_calls, 0);
    let published = publisher.installation.lock().unwrap().clone().unwrap();
    let uncertain = store.load(&interrupted.record.transaction_id).unwrap();
    let mut different_contract = published.clone();
    different_contract.current.requirements.app.description = Some("changed contract".into());
    different_contract.validate().unwrap();
    assert!(
        zup_transaction::verify_published_installation(&uncertain, &different_contract).is_err()
    );
    assert!(matches!(
        uncertain.publication,
        PublicationState::Requested { .. }
    ));
    let uncertain = if altered_ownership {
        let mut altered = published.clone();
        let path = altered.directory.join("foreign.txt").unwrap();
        altered.current.resources.insert(
            zup_core::ResourceKey::File {
                id: zup_core::FileResourceId::Payload {
                    id: zup_core::PayloadId::File {
                        declaration: zup_core::ResourceId::new("fixture-0").unwrap(),
                    },
                },
            },
            zup_plan::OwnedResource::File {
                replacement: zup_core::FixedReplacement::Atomic,
                instance: zup_core::FixedFileInstance {
                    origin: altered.current.transaction,
                    object: zup_core::NativeObjectIdentity::Linux {
                        device: 1,
                        inode: 1,
                    },
                },
                destination: path,
                source_relative: zup_core::RelativePath::new("foreign.txt").unwrap(),
                sha256: zup_core::hash_bytes(b"foreign"),
                size: 7,
                executable: false,
                created_directories: vec![],
                privilege: zup_core::Privilege::User,
            },
        );
        altered.validate().unwrap();
        assert!(zup_transaction::verify_installation_evidence(&altered, &uncertain).is_err());
        *publisher.installation.lock().unwrap() = Some(altered);
        let (blocked, outcome) = recover(uncertain, &store, &mut executor, &publisher).unwrap();
        assert_eq!(outcome, TransactionOutcome::RecoveryRequired);
        assert_eq!(*publisher.publication_attempts.lock().unwrap(), 1);
        assert_eq!(executor.rollback_calls, 0);
        *publisher.installation.lock().unwrap() = Some(published.clone());
        blocked
    } else {
        uncertain
    };
    let (committed, outcome) = recover(uncertain, &store, &mut executor, &publisher).unwrap();
    assert_eq!(outcome, TransactionOutcome::Committed);
    assert_eq!(executor.rollback_calls, 0);
    assert_eq!(
        publisher.installation.lock().unwrap().as_ref(),
        Some(&published)
    );
    assert!(matches!(
        committed.publication,
        PublicationState::Published { .. }
    ));
    committed.validate().unwrap();
}

struct FilePublisher {
    directory: std::path::PathBuf,
    terminate_after_write: bool,
}

impl InstallationPublisher for FilePublisher {
    type Error = String;

    fn prepare(&self, transaction: &TransactionRecord) -> Result<InstallationRecord, String> {
        zup_storage::ensure_private_dir(&self.directory).map_err(|error| error.to_string())?;
        let path = self.directory.join("installation.json");
        let previous = match zup_storage::read_private(&path) {
            Ok(bytes) => {
                Some(InstallationRecord::decode(&bytes).map_err(|error| error.to_string())?)
            }
            Err(private_fs::Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                None
            }
            Err(error) => return Err(error.to_string()),
        };
        prepare_installation_record(transaction, previous.as_ref())
            .map_err(|error| error.to_string())
    }

    fn publish(&self, transaction: &TransactionRecord) -> Result<InstallationRecord, String> {
        let path = self.directory.join("installation.json");
        let installation = self.prepare(transaction)?;
        let bytes = installation.encode().map_err(|error| error.to_string())?;
        zup_storage::publish_private(&path, &bytes).map_err(|error| error.to_string())?;
        if self.terminate_after_write {
            std::process::exit(86);
        }
        let readback = zup_storage::read_private(&path).map_err(|error| error.to_string())?;
        InstallationRecord::decode(&readback).map_err(|error| error.to_string())
    }
}

#[test]
fn publication_process_crash_requires_no_compensation() {
    const CHILD_ROOT: &str = "ZUP_PUBLICATION_CRASH_TEST_ROOT";
    if let Some(root) = std::env::var_os(CHILD_ROOT) {
        let root = std::path::PathBuf::from(root);
        let store = FilesystemTransactionStore::private_at(root.join("state"));
        let mut effects = DeploymentEffects::new(
            InstallationId::new_v7(),
            requirements_fixture::requirements(
                Target::LINUX_X64,
                zup_core::SelectedScope::User,
                "example.publication",
                "1.0.0",
            ),
        );
        effects.install_directory =
            Some(TargetPath::new(Target::LINUX_X64, "/opt/example").unwrap());
        let open = TransactionCoordinator::new(store)
            .begin(
                AppId::new("example.publication").unwrap(),
                SelectedScope::User,
                "1.0.0".parse().unwrap(),
                compile_deployment(&effects).unwrap(),
            )
            .unwrap();
        open.execute(
            &mut Executor::default(),
            &FilePublisher {
                directory: root.join("installations"),
                terminate_after_write: true,
            },
        )
        .unwrap();
        panic!("publication must terminate the child process");
    }
    let root = tempfile::tempdir().unwrap();
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "publication_process_crash_requires_no_compensation",
            "--nocapture",
        ])
        .env(CHILD_ROOT, root.path())
        .output()
        .unwrap();
    assert_eq!(
        child.status.code(),
        Some(86),
        "{}",
        String::from_utf8_lossy(&child.stderr)
    );
    let store = FilesystemTransactionStore::private_at(root.path().join("state"));
    let transactions = store.list_ids().unwrap();
    assert_eq!(transactions.len(), 1);
    let pending = store.load(&transactions[0]).unwrap();
    assert!(matches!(
        pending.publication,
        PublicationState::Requested { .. }
    ));
    let path = root.path().join("installations").join("installation.json");
    let published = zup_storage::read_private(&path).unwrap();
    let mut executor = Executor::default();
    let (committed, outcome) = recover(
        pending,
        &store,
        &mut executor,
        &FilePublisher {
            directory: root.path().join("installations"),
            terminate_after_write: false,
        },
    )
    .unwrap();
    assert_eq!(outcome, TransactionOutcome::Committed);
    assert_eq!(executor.rollback_calls, 0);
    assert_eq!(zup_storage::read_private(&path).unwrap(), published);
    committed.validate().unwrap();
}

#[test]
fn competing_applications_cannot_reserve_the_same_installation_identity() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    let store = FilesystemTransactionStore::private_at(&state);
    let installation = InstallationId::new_v7();
    let gate = std::sync::Arc::new(std::sync::Barrier::new(2));
    let transactions = ["example.first", "example.second"].map(|app| {
        let mut effects = DeploymentEffects::new(
            installation,
            requirements_fixture::requirements(
                Target::LINUX_X64,
                zup_core::SelectedScope::User,
                app,
                "1.0.0",
            ),
        );
        effects.install_directory =
            Some(TargetPath::new(Target::LINUX_X64, "/opt/example").unwrap());
        let transaction = TransactionCoordinator::new(store.clone())
            .begin(
                AppId::new(app).unwrap(),
                SelectedScope::User,
                "1.0.0".parse().unwrap(),
                compile_deployment(&effects).unwrap(),
            )
            .unwrap();
        let gate = gate.clone();
        std::thread::spawn(move || {
            gate.wait();
            transaction
                .execute(&mut Executor::default(), &Publisher::default())
                .unwrap()
        })
    });
    let settled = transactions.map(|thread| thread.join().unwrap());
    assert_eq!(
        settled
            .iter()
            .filter(|settled| settled.outcome == TransactionOutcome::Committed)
            .count(),
        1
    );
    assert_eq!(
        settled
            .iter()
            .filter(|settled| settled.outcome == TransactionOutcome::RolledBack)
            .count(),
        1
    );
    let winner = settled
        .iter()
        .find(|settled| settled.outcome == TransactionOutcome::Committed)
        .unwrap();
    let loser = settled
        .iter()
        .find(|settled| settled.outcome == TransactionOutcome::RolledBack)
        .unwrap();
    let reservation = store.reserve_installation(&winner.record).unwrap();
    assert_eq!(reservation.origin, winner.record.transaction_id);
    assert_eq!(reservation.app_id, winner.record.app_id);
    assert!(loser.record.reservation.is_none());
    assert!(loser.record.failure.is_some());
}

#[test]
fn missing_established_reservation_requires_recovery_without_recreating_ownership() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    let store = FilesystemTransactionStore::private_at(&state);
    let mut effects = DeploymentEffects::new(
        InstallationId::new_v7(),
        requirements_fixture::requirements(
            Target::LINUX_X64,
            zup_core::SelectedScope::User,
            "example.reservation",
            "1.0.0",
        ),
    );
    effects.install_directory = Some(TargetPath::new(Target::LINUX_X64, "/opt/example").unwrap());
    let publisher = Publisher::default();
    let app = AppId::new("example.reservation").unwrap();
    let first = TransactionCoordinator::new(store.clone())
        .begin(
            app.clone(),
            SelectedScope::User,
            "1.0.0".parse().unwrap(),
            compile_deployment(&effects).unwrap(),
        )
        .unwrap()
        .execute(&mut Executor::default(), &publisher)
        .unwrap();
    assert_eq!(first.outcome, TransactionOutcome::Committed);
    private_fs::PrivateDir::open(&state)
        .unwrap()
        .open_dir("identities")
        .unwrap()
        .remove_file(
            format!("{}.json", effects.installation),
            private_fs::Durability::Full,
        )
        .unwrap();
    effects.observed = publisher
        .installation
        .lock()
        .unwrap()
        .as_ref()
        .map(|installed| installed.current.selection());
    effects.observed_transaction = Some(first.record.transaction_id);
    let mut executor = Executor::default();
    effects.requirements.app.version = "2.0.0".parse().unwrap();
    let next = TransactionCoordinator::new(store)
        .begin(
            app,
            SelectedScope::User,
            "2.0.0".parse().unwrap(),
            compile_deployment(&effects).unwrap(),
        )
        .unwrap()
        .execute(&mut executor, &publisher)
        .unwrap();
    assert_eq!(next.outcome, TransactionOutcome::RecoveryRequired);
    assert_eq!(executor.rollback_calls, 0);
    assert!(
        !state
            .join("identities")
            .join(format!("{}.json", effects.installation))
            .exists()
    );
    assert_eq!(*publisher.publication_attempts.lock().unwrap(), 1);
}

#[cfg(feature = "test-faults")]
#[test]
fn reservation_crash_preserves_the_owner_without_claiming_installation_success() {
    const CHILD_ROOT: &str = "ZUP_RESERVATION_CRASH_TEST_ROOT";
    if let Some(root) = std::env::var_os(CHILD_ROOT) {
        let state = std::path::PathBuf::from(root).join("state");
        let mut effects = DeploymentEffects::new(
            InstallationId::new_v7(),
            requirements_fixture::requirements(
                Target::LINUX_X64,
                zup_core::SelectedScope::User,
                "example.reservation",
                "1.0.0",
            ),
        );
        effects.install_directory =
            Some(TargetPath::new(Target::LINUX_X64, "/opt/example").unwrap());
        TransactionCoordinator::new(FilesystemTransactionStore::private_at(state))
            .begin(
                AppId::new("example.reservation").unwrap(),
                SelectedScope::User,
                "1.0.0".parse().unwrap(),
                compile_deployment(&effects).unwrap(),
            )
            .unwrap()
            .execute(&mut Executor::default(), &Publisher::default())
            .unwrap();
        panic!("reservation must terminate the child");
    }
    let root = tempfile::tempdir().unwrap();
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "reservation_crash_preserves_the_owner_without_claiming_installation_success",
            "--nocapture",
        ])
        .env(CHILD_ROOT, root.path())
        .env("ZUP_TEST_FAULT_POINT", "installation_reserved")
        .env("ZUP_TEST_FAULT_ACTION", "crash")
        .output()
        .unwrap();
    assert!(!child.status.success());
    let store = FilesystemTransactionStore::private_at(root.path().join("state"));
    let ids = store.list_ids().unwrap();
    assert_eq!(ids.len(), 1);
    let pending = store.load(&ids[0]).unwrap();
    assert!(pending.reservation.is_none());
    let reservation = store.reserve_installation(&pending).unwrap();
    assert_eq!(reservation.origin, pending.transaction_id);
    let publisher = Publisher::default();
    let (settled, outcome) =
        recover(pending, &store, &mut Executor::default(), &publisher).unwrap();
    assert_eq!(outcome, TransactionOutcome::RolledBack);
    assert_eq!(settled.reservation, Some(reservation));
    assert!(publisher.installation.lock().unwrap().is_none());
}
