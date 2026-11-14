#![cfg(target_os = "linux")]
mod requirements_fixture {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/deployment.rs"
    ));
}
use zup_plan::RemovalKind;
use zup_plan::{DeploymentEffects, DeploymentNode, FileRemoval, OperationId, compile_deployment};

use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;

use zup_bundle::DirectoryPayloadSource;
use zup_core::TargetPath;
use zup_core::{
    AppId, Privilege, RelativePath, ResourceKey, SelectedScope, Sha256Digest, Target, hash_reader,
};
use zup_linux::{ExecError, LinuxFileExecutor};
use zup_plan::{FileOperation, FileOperationKind, FilePrecondition};
use zup_transaction::{
    FilesystemTransactionStore, OperationExecutor, OperationReceipt, ReconcileResult,
    TransactionCoordinator, TransactionOutcome, recover,
};

fn target() -> Target {
    Target::host().expect("a Linux target")
}

fn digest(bytes: &[u8]) -> Sha256Digest {
    hash_reader(bytes).expect("bytes hash").1
}

fn destination(root: &Path, name: &str) -> TargetPath {
    TargetPath::new(target(), root.join(name).to_string_lossy()).expect("a target path")
}

fn work(
    name: &str,
    bytes: &[u8],
    destination: TargetPath,
    precondition: FilePrecondition,
    executable: bool,
) -> FileOperation {
    FileOperation {
        replacement: zup_core::FixedReplacement::Atomic,
        key: ResourceKey::File {
            id: zup_core::FileResourceId::Payload {
                id: zup_core::PayloadId::TreeMember {
                    declaration: zup_core::ResourceId::new("fixture").unwrap(),
                    member: zup_core::RelativePath::new(name).unwrap(),
                },
            },
        },
        source_relative: RelativePath::new(name).expect("a relative path"),
        destination,
        precondition,
        expected_sha256: digest(bytes),
        expected_size: bytes.len() as u64,
        privilege: Privilege::User,
        executable,
        conflict: None,
        kind: match precondition {
            FilePrecondition::Absent => FileOperationKind::Create,
            FilePrecondition::Exact { .. } => FileOperationKind::Replace,
        },
    }
}

fn effects(install: &Path) -> DeploymentEffects {
    let mut effects = DeploymentEffects::new(
        "00000000-0000-7000-8000-000000000001".parse().unwrap(),
        requirements_fixture::requirements(
            target(),
            zup_core::SelectedScope::User,
            "com.example.tool",
            "1.0.0",
        ),
    );
    effects.install_directory = Some(TargetPath::new(target(), install.to_string_lossy()).unwrap());
    effects
}

fn run(
    input: &DeploymentEffects,
    payload_root: &Path,
    state_root: &Path,
) -> (TransactionOutcome, LinuxFileExecutor) {
    let publisher = zup_linux::LinuxInstallationStore::new(state_root);
    let previous = publisher
        .load(
            &AppId::new("com.example.tool").unwrap(),
            SelectedScope::User,
        )
        .unwrap();
    let mut input = input.clone();
    input.observed = previous.as_ref().map(|record| record.current.selection());
    input.observed_transaction = previous.as_ref().map(|record| record.current.transaction);
    let plan = compile_deployment(&input).expect("a plan compiles");
    let store = FilesystemTransactionStore::private_at(state_root);
    let coordinator = TransactionCoordinator::new(store);
    let record = coordinator
        .begin(
            AppId::new("com.example.tool").expect("an id"),
            SelectedScope::User,
            semver::Version::parse("1.0.0").expect("a version"),
            plan.clone(),
        )
        .expect("a record begins");
    let mut executor =
        LinuxFileExecutor::new().with_payload(DirectoryPayloadSource::new(payload_root));
    executor.register_plan(&plan).expect("a plan registers");
    executor.set_transaction_id(record.transaction_id);
    let outcome = record
        .execute(&mut executor, &publisher)
        .expect("execution reaches a stable outcome")
        .outcome;
    (outcome, executor)
}

#[test]
fn the_coordinator_commits_a_linux_install() {
    let root = tempfile::tempdir().expect("a temp directory");
    let payload = root.path().join("payload");
    let install = root.path().join("install");
    let state = root.path().join("state");
    std::fs::create_dir_all(&payload).expect("payload");
    std::fs::write(payload.join("tool"), b"#!/bin/sh\n").expect("tool");
    std::fs::write(payload.join("data.dat"), b"the data").expect("data");

    let mut input = effects(&install);
    input.files.push(work(
        "tool",
        b"#!/bin/sh\n",
        destination(&install, "tool"),
        FilePrecondition::Absent,
        true,
    ));
    input.files.push(work(
        "data.dat",
        b"the data",
        destination(&install, "data.dat"),
        FilePrecondition::Absent,
        false,
    ));

    let (outcome, _) = run(&input, &payload, &state);
    assert_eq!(outcome, TransactionOutcome::Committed);

    assert_eq!(
        std::fs::read(install.join("tool")).expect("tool installs"),
        b"#!/bin/sh\n"
    );
    assert_eq!(
        std::fs::read(install.join("data.dat")).expect("data installs"),
        b"the data"
    );
    let mode = |name: &str| {
        std::fs::symlink_metadata(install.join(name))
            .expect("stat")
            .permissions()
            .mode()
            & 0o777
    };
    assert_eq!(mode("tool"), 0o744, "declared runnable means runnable");
    assert_eq!(mode("data.dat"), 0o644, "data stays data");
    assert!(
        state.join("transactions").exists(),
        "the journal lives in the state root, not beside the payload"
    );
}

#[rstest::rstest]
#[case::same_instance(false)]
#[case::foreign_equal_bytes(true)]
fn unchanged_fixed_payload_retains_identity_and_publishes_new_contracts(#[case] foreign: bool) {
    let root = tempfile::tempdir().unwrap();
    let payload = root.path().join("payload");
    let install = root.path().join("install");
    let state = root.path().join("state");
    std::fs::create_dir(&payload).unwrap();
    std::fs::write(payload.join("program"), b"owned payload").unwrap();
    let path = destination(&install, "program");
    let mut input = effects(&install);
    input.files.push(work(
        "program",
        b"owned payload",
        path.clone(),
        FilePrecondition::Absent,
        false,
    ));
    assert_eq!(
        run(&input, &payload, &state).0,
        TransactionOutcome::Committed
    );
    let publisher = zup_linux::LinuxInstallationStore::new(&state);
    let app = AppId::new("com.example.tool").unwrap();
    let previous = publisher.load(&app, SelectedScope::User).unwrap().unwrap();
    let original = &previous.current.resources[&input.files[0].key];
    let zup_plan::OwnedResource::File { instance, .. } = original else {
        unreachable!()
    };
    if foreign {
        std::fs::rename(path.as_str(), root.path().join("original-instance")).unwrap();
        std::fs::write(path.as_str(), b"owned payload").unwrap();
    }
    let mut input = effects(&install);
    let mut retained = work(
        "repacked",
        b"owned payload",
        path.clone(),
        FilePrecondition::Exact {
            object: instance.object,
            size: 13,
            sha256: digest(b"owned payload"),
        },
        false,
    );
    retained.key = previous.current.resources.keys().next().unwrap().clone();
    retained.kind = FileOperationKind::NoOp;
    retained.replacement = zup_core::FixedReplacement::Quiescent;
    input.files.push(retained);
    let outcome = run(&input, &root.path().join("unavailable-payload"), &state).0;
    let current = publisher.load(&app, SelectedScope::User).unwrap().unwrap();
    if foreign {
        assert_eq!(outcome, TransactionOutcome::RolledBack);
        assert_eq!(current, previous);
    } else {
        assert_eq!(outcome, TransactionOutcome::Committed);
        let zup_plan::OwnedResource::File {
            instance: retained,
            source_relative,
            replacement,
            ..
        } = &current.current.resources[&input.files[0].key]
        else {
            unreachable!()
        };
        assert_eq!(retained, instance);
        assert_eq!(source_relative.as_str(), "repacked");
        assert_eq!(*replacement, zup_core::FixedReplacement::Quiescent);
        assert!(current.references_transaction(instance.origin));
    }
    assert_eq!(std::fs::read(path.as_str()).unwrap(), b"owned payload");
}

#[test]
fn a_contested_destination_fails_the_transaction_without_publishing_anything() {
    let root = tempfile::tempdir().expect("a temp directory");
    let payload = root.path().join("payload");
    let install = root.path().join("install");
    let state = root.path().join("state");
    std::fs::create_dir_all(&payload).expect("payload");
    std::fs::create_dir_all(&install).expect("install");
    std::fs::write(payload.join("tool"), b"v2").expect("tool");
    std::fs::write(payload.join("other"), b"other").expect("other");
    std::fs::write(install.join("tool"), b"someone else's file").expect("plant");

    let mut input = effects(&install);
    input.files.push(work(
        "tool",
        b"v2",
        destination(&install, "tool"),
        FilePrecondition::Absent,
        false,
    ));
    input.files.push(work(
        "other",
        b"other",
        destination(&install, "other"),
        FilePrecondition::Absent,
        false,
    ));

    let (outcome, _) = run(&input, &payload, &state);
    assert_eq!(outcome, TransactionOutcome::RolledBack);
    assert_eq!(
        std::fs::read(install.join("tool")).expect("read"),
        b"someone else's file",
        "the file that was already there is untouched"
    );
    assert!(
        !install.join("other").exists(),
        "the refused transaction publishes nothing, not even the uncontested file"
    );
}

#[test]
fn the_coordinator_applies_an_upgrade_as_a_replace() {
    let root = tempfile::tempdir().expect("a temp directory");
    let payload = root.path().join("payload");
    let install = root.path().join("install");
    let state = root.path().join("state");
    std::fs::create_dir_all(&payload).expect("payload");
    std::fs::write(payload.join("tool"), b"v1").expect("v1");

    let mut v1 = effects(&install);
    v1.files.push(work(
        "tool",
        b"v1",
        destination(&install, "tool"),
        FilePrecondition::Absent,
        true,
    ));
    let (outcome, _) = run(&v1, &payload, &state);
    assert_eq!(outcome, TransactionOutcome::Committed);

    std::fs::write(payload.join("tool"), b"v2").expect("v2");
    let mut v2 = effects(&install);
    v2.files.push(work(
        "tool",
        b"v2",
        destination(&install, "tool"),
        FilePrecondition::Exact {
            object: {
                use std::os::unix::fs::MetadataExt;
                let metadata = std::fs::symlink_metadata(install.join("tool")).unwrap();
                zup_core::NativeObjectIdentity::Linux {
                    device: metadata.dev(),
                    inode: metadata.ino(),
                }
            },
            size: 2,
            sha256: digest(b"v1"),
        },
        true,
    ));
    let (outcome, _) = run(&v2, &payload, &state);
    assert_eq!(outcome, TransactionOutcome::Committed);
    assert_eq!(
        std::fs::read(install.join("tool")).expect("read"),
        b"v2",
        "the upgrade lands"
    );
    assert_eq!(
        std::fs::symlink_metadata(install.join("tool"))
            .expect("stat")
            .permissions()
            .mode()
            & 0o111,
        0o100,
        "runnable stays runnable across the upgrade"
    );
}

#[test]
fn recovery_replays_a_journal_without_the_original_executor() {
    use zup_transaction::{NodeState, TransactionPhase, TransactionStore as _};

    let root = tempfile::tempdir().expect("a temp directory");
    let payload = root.path().join("payload");
    let install = root.path().join("install");
    let state = root.path().join("state");
    std::fs::create_dir_all(&payload).expect("payload");
    std::fs::write(payload.join("tool"), b"the payload").expect("tool");

    let mut input = effects(&install);
    input.files.push(work(
        "tool",
        b"the payload",
        destination(&install, "tool"),
        FilePrecondition::Absent,
        false,
    ));
    let plan = compile_deployment(&input).expect("a plan compiles");

    let store = FilesystemTransactionStore::private_at(&state);
    let coordinator = TransactionCoordinator::new(store);
    let record = coordinator
        .begin(
            AppId::new("com.example.tool").expect("an id"),
            SelectedScope::User,
            semver::Version::parse("1.0.0").expect("a version"),
            plan.clone(),
        )
        .expect("a record begins");
    let transaction_id = record.transaction_id;
    reserve(
        &FilesystemTransactionStore::private_at(&state),
        transaction_id,
    );
    {
        let mut executor =
            LinuxFileExecutor::new().with_payload(DirectoryPayloadSource::new(&payload));
        executor.register_plan(&plan).expect("a plan registers");
        executor.set_transaction_id(transaction_id);
        let mut staged = Vec::new();
        for node in plan
            .nodes
            .iter()
            .filter(|node| matches!(node.kind, zup_plan::NodeKind::StageFile { .. }))
        {
            let store = FilesystemTransactionStore::private_at(&state);
            let fresh = store.load(&transaction_id).unwrap();
            let record = store
                .transition(
                    &transaction_id,
                    fresh.revision,
                    &[zup_transaction::TransactionChange::Node(
                        node.id.clone(),
                        NodeState::Running,
                    )],
                )
                .unwrap();
            let receipt = executor.apply(node, &record).expect("a stage applies");
            staged.push((node.id.clone(), receipt));
        }
        let store = FilesystemTransactionStore::private_at(&state);
        let record = store
            .update(&transaction_id, &mut |record| {
                for (id, receipt) in staged.drain(..) {
                    record.nodes.insert(
                        id,
                        NodeState::Applied {
                            receipt: Box::new(receipt),
                        },
                    );
                }
                Ok(())
            })
            .unwrap();
        for node in plan
            .nodes
            .iter()
            .filter(|node| matches!(node.kind, zup_plan::NodeKind::FileMutation { .. }))
        {
            let _ = executor.apply(node, &record).expect("a mutation applies");
        }
        let store = FilesystemTransactionStore::private_at(&state);
        store
            .update(&transaction_id, &mut |record| {
                for (id, receipt) in staged.drain(..) {
                    record.nodes.insert(
                        id,
                        NodeState::Applied {
                            receipt: Box::new(receipt),
                        },
                    );
                }
                for node in plan
                    .nodes
                    .iter()
                    .filter(|node| matches!(node.kind, zup_plan::NodeKind::FileMutation { .. }))
                {
                    record.nodes.insert(node.id.clone(), NodeState::Running);
                }
                record.phase = TransactionPhase::Applying;
                Ok(())
            })
            .expect("the interrupted phase persists");
    }

    let store = FilesystemTransactionStore::private_at(&state);
    let record = store.load(&transaction_id).expect("the journal survives");
    let mut executor = LinuxFileExecutor::new().with_payload(DirectoryPayloadSource::new(&payload));
    executor.register_plan(&plan).expect("a plan registers");
    executor.set_transaction_id(record.transaction_id);
    let (recovered, outcome) = recover(
        record,
        &store,
        &mut executor,
        &zup_linux::LinuxInstallationStore::new(&state),
    )
    .expect("recovery runs");
    assert_eq!(outcome, TransactionOutcome::Committed, "{recovered:?}");
    assert_eq!(
        std::fs::read(install.join("tool")).expect("read"),
        b"the payload"
    );
}

fn removal(bytes: &[u8], destination: TargetPath) -> FileRemoval {
    FileRemoval {
        instance: zup_core::FixedFileInstance {
            origin: zup_core::TransactionId::new_v7(),
            object: {
                use std::os::unix::fs::MetadataExt;
                let metadata = std::fs::symlink_metadata(destination.as_str()).unwrap();
                zup_core::NativeObjectIdentity::Linux {
                    device: metadata.dev(),
                    inode: metadata.ino(),
                }
            },
        },
        key: ResourceKey::File {
            id: zup_core::FileResourceId::Payload {
                id: zup_core::PayloadId::File {
                    declaration: zup_core::ResourceId::new(destination.file_name().unwrap())
                        .unwrap(),
                },
            },
        },
        kind: RemovalKind::RemoveOwned,
        scope: SelectedScope::User,
        privilege: Privilege::User,
        destination,
        sha256: digest(bytes),
        size: bytes.len() as u64,
        created_directories: Vec::new(),
    }
}

struct FailAfter<E> {
    inner: E,
    fail: OperationId,
}

impl<E: OperationExecutor<Error = ExecError>> OperationExecutor for FailAfter<E> {
    type Error = ExecError;

    fn prepare(&mut self, operation: &DeploymentNode) -> Result<(), Self::Error> {
        self.inner.prepare(operation)
    }

    fn apply(
        &mut self,
        operation: &DeploymentNode,
        transaction: &zup_transaction::TransactionRecord,
    ) -> Result<OperationReceipt, Self::Error> {
        if operation.id == self.fail {
            return Err(ExecError::Executor("a later operation fails".into()));
        }
        self.inner.apply(operation, transaction)
    }

    fn verify(
        &mut self,
        operation: &DeploymentNode,
        receipt: &OperationReceipt,
    ) -> Result<(), Self::Error> {
        self.inner.verify(operation, receipt)
    }

    fn rollback(
        &mut self,
        operation: &DeploymentNode,
        receipt: &OperationReceipt,
    ) -> Result<(), Self::Error> {
        self.inner.rollback(operation, receipt)
    }

    fn reconcile(
        &mut self,
        operation: &DeploymentNode,
        receipt: Option<&OperationReceipt>,
        transaction: &zup_transaction::TransactionRecord,
    ) -> Result<ReconcileResult, Self::Error> {
        self.inner.reconcile(operation, receipt, transaction)
    }
}

#[test]
fn a_failed_transaction_restores_an_already_removed_file() {
    let root = tempfile::tempdir().expect("a temp directory");
    let install = root.path().join("install");
    let state = root.path().join("state");
    std::fs::create_dir_all(&install).expect("install");
    std::fs::write(install.join("doomed.dat"), b"doomed").expect("doomed");
    std::fs::write(install.join("other.dat"), b"other").expect("other");
    let mut permissions = std::fs::symlink_metadata(install.join("doomed.dat"))
        .expect("stat")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(install.join("doomed.dat"), permissions).expect("chmod");

    let first = removal(b"doomed", destination(&install, "doomed.dat"));
    let second = removal(b"other", destination(&install, "other.dat"));
    let mut input = effects(&install);
    input.retired_keys.push(first.key.clone());
    input.retired_keys.push(second.key.clone());
    input.removals.push(first);
    input.removals.push(second);

    let plan = compile_deployment(&input).expect("a plan compiles");
    let fail = OperationId::resource("remove_file", &input.removals[1].key);
    let store = FilesystemTransactionStore::private_at(&state);
    let coordinator = TransactionCoordinator::new(store);
    let record = coordinator
        .begin(
            AppId::new("com.example.tool").expect("an id"),
            SelectedScope::User,
            semver::Version::parse("1.0.0").expect("a version"),
            plan.clone(),
        )
        .expect("a record begins");
    let mut executor = FailAfter {
        inner: {
            let mut inner = LinuxFileExecutor::new();
            inner.register_plan(&plan).expect("a plan registers");
            inner.set_transaction_id(record.transaction_id);
            inner
        },
        fail,
    };
    let settled = record
        .execute(
            &mut executor,
            &zup_linux::LinuxInstallationStore::new(&state),
        )
        .expect("execution reaches a stable outcome");
    let (record, outcome) = (settled.record, settled.outcome);
    let _ = record;

    assert_eq!(outcome, TransactionOutcome::RolledBack);
    assert_eq!(
        std::fs::read(install.join("doomed.dat")).expect("read"),
        b"doomed",
        "the removal that applied before the failure comes back"
    );
    assert_eq!(
        std::fs::symlink_metadata(install.join("doomed.dat"))
            .expect("stat")
            .permissions()
            .mode()
            & 0o777,
        0o755,
        "the restored file keeps its mode"
    );
    assert_eq!(
        std::fs::read(install.join("other.dat")).expect("read"),
        b"other",
        "the operation that failed never unlinked its file"
    );
}

#[test]
fn recovery_commits_an_interrupted_removal() {
    use zup_transaction::{NodeState, TransactionPhase, TransactionStore as _};

    let root = tempfile::tempdir().expect("a temp directory");
    let install = root.path().join("install");
    let state = root.path().join("state");
    std::fs::create_dir_all(&install).expect("install");
    std::fs::write(install.join("doomed.dat"), b"doomed").expect("doomed");

    let removed = removal(b"doomed", destination(&install, "doomed.dat"));
    let mut input = effects(&install);
    input.retired_keys.push(removed.key.clone());
    input.removals.push(removed);
    let plan = compile_deployment(&input).expect("a plan compiles");

    let store = FilesystemTransactionStore::private_at(&state);
    let coordinator = TransactionCoordinator::new(store);
    let record = coordinator
        .begin(
            AppId::new("com.example.tool").expect("an id"),
            SelectedScope::User,
            semver::Version::parse("1.0.0").expect("a version"),
            plan.clone(),
        )
        .expect("a record begins");
    let transaction_id = record.transaction_id;
    reserve(
        &FilesystemTransactionStore::private_at(&state),
        transaction_id,
    );
    {
        let mut executor = LinuxFileExecutor::new();
        executor.register_plan(&plan).expect("a plan registers");
        executor.set_transaction_id(transaction_id);
        let removal = plan
            .nodes
            .iter()
            .find(|node| matches!(node.kind, zup_plan::NodeKind::FileRemoval { .. }))
            .expect("a removal node")
            .clone();
        let _ = executor
            .apply(&removal, &record)
            .expect("a removal applies");
        assert!(
            !install.join("doomed.dat").exists(),
            "the interruption lands after the unlink"
        );
        let store = FilesystemTransactionStore::private_at(&state);
        store
            .update(&transaction_id, &mut |record| {
                record.nodes.insert(removal.id.clone(), NodeState::Running);
                record.phase = TransactionPhase::Applying;
                Ok(())
            })
            .expect("the interrupted phase persists");
    }

    let store = FilesystemTransactionStore::private_at(&state);
    let record = store.load(&transaction_id).expect("the journal survives");
    let mut executor = LinuxFileExecutor::new();
    executor.register_plan(&plan).expect("a plan registers");
    executor.set_transaction_id(record.transaction_id);
    let (recovered, outcome) = recover(
        record,
        &store,
        &mut executor,
        &zup_linux::LinuxInstallationStore::new(&state),
    )
    .expect("recovery runs");
    assert_eq!(outcome, TransactionOutcome::Committed, "{recovered:?}");
    assert!(
        !install.join("doomed.dat").exists(),
        "recovery does not resurrect a removal that already unlinked"
    );
}

fn reserve(store: &impl zup_transaction::TransactionStore, transaction: zup_core::TransactionId) {
    use zup_transaction::{NodeState, TransactionChange as Change};
    let id = OperationId::new(OperationId::RESERVE);
    let record = store.load(&transaction).unwrap();
    let record = store
        .transition(
            &transaction,
            record.revision,
            &[Change::Node(id.clone(), NodeState::Running)],
        )
        .unwrap();
    let reservation = store.reserve_installation(&record).unwrap();
    let mut record = store
        .transition(
            &transaction,
            record.revision,
            &[
                Change::Reservation(Some(reservation)),
                Change::Node(
                    id,
                    NodeState::Applied {
                        receipt: Box::new(OperationReceipt::Control),
                    },
                ),
            ],
        )
        .unwrap();
    for node in record.plan.nodes.clone() {
        if node.kind.is_barrier()
            && matches!(
                node.phase,
                zup_plan::Phase::Begin | zup_plan::Phase::Preflight | zup_plan::Phase::CommitIntent
            )
        {
            record = store
                .transition(
                    &transaction,
                    record.revision,
                    &[Change::Node(node.id.clone(), NodeState::Running)],
                )
                .unwrap();
            record = store
                .transition(
                    &transaction,
                    record.revision,
                    &[Change::Node(
                        node.id,
                        NodeState::Applied {
                            receipt: Box::new(OperationReceipt::Control),
                        },
                    )],
                )
                .unwrap();
        }
    }
}

#[rstest::rstest]
#[case::complete(false)]
#[case::swapped_native_object(true)]
fn explicit_repair_resolution_preserves_the_modified_native_object(#[case] swap: bool) {
    use std::os::unix::fs::MetadataExt;
    use zup_transaction::{RecoveryRequest, TransactionStore};
    let root = tempfile::tempdir().unwrap();
    let payload = root.path().join("payload");
    let install = root.path().join("install");
    let state = root.path().join("state");
    std::fs::create_dir(&payload).unwrap();
    std::fs::write(payload.join("program"), b"authenticated").unwrap();
    let mut input = effects(&install);
    input.files.push(work(
        "program",
        b"authenticated",
        destination(&install, "program"),
        FilePrecondition::Absent,
        false,
    ));
    assert_eq!(
        run(&input, &payload, &state).0,
        TransactionOutcome::Committed
    );
    let publisher = zup_linux::LinuxInstallationStore::new(&state);
    let app = AppId::new("com.example.tool").unwrap();
    let previous = publisher.load(&app, SelectedScope::User).unwrap().unwrap();
    std::fs::write(install.join("program"), b"externally modified").unwrap();
    let metadata = std::fs::symlink_metadata(install.join("program")).unwrap();
    let foreign = zup_core::NativeObjectIdentity::Linux {
        device: metadata.dev(),
        inode: metadata.ino(),
    };
    input.observed = Some(previous.current.selection());
    input.observed_transaction = Some(previous.commit.transaction);
    input.files[0].kind = FileOperationKind::RepairOwned;
    input.files[0].precondition = FilePrecondition::Exact {
        object: foreign,
        size: 19,
        sha256: digest(b"externally modified"),
    };
    let plan = compile_deployment(&input).unwrap();
    let store = FilesystemTransactionStore::private_at(&state);
    let open = TransactionCoordinator::new(store.clone())
        .begin(
            app.clone(),
            SelectedScope::User,
            "1.0.0".parse().unwrap(),
            plan.clone(),
        )
        .unwrap();
    let mut native = LinuxFileExecutor::new().with_payload(DirectoryPayloadSource::new(&payload));
    native.register_plan(&plan).unwrap();
    native.set_transaction_id(open.transaction_id);
    let mut executor = FailAfter {
        inner: native,
        fail: OperationId::new("ctrl:verify"),
    };
    let settled = open.execute(&mut executor, &publisher).unwrap();
    assert_eq!(settled.outcome, TransactionOutcome::RecoveryRequired);
    let repair = settled
        .record
        .plan
        .nodes
        .iter()
        .find(|node| matches!(node.kind, zup_plan::NodeKind::FileMutation { .. }))
        .unwrap();
    let OperationReceipt::ReplaceFile {
        backup_path,
        previous_object,
        ..
    } = settled.record.receipt(&repair.id).unwrap()
    else {
        panic!("replacement receipt");
    };
    assert_eq!(*previous_object, foreign);
    let backup = backup_path.clone();
    let mut native = executor.inner;
    let assessment = zup_transaction::assess_recovery(&settled.record, &mut native).unwrap();
    let decision = assessment.complete_forward.unwrap();
    if swap {
        std::fs::rename(
            install.join("program"),
            root.path().join("original-new-instance"),
        )
        .unwrap();
        std::fs::write(install.join("program"), b"authenticated").unwrap();
    }
    let result = zup_transaction::recover_requested(
        settled.record.clone(),
        &store,
        &mut native,
        &publisher,
        &RecoveryRequest::CompleteForward {
            decision: Box::new(decision),
        },
    );
    if swap {
        assert!(result.is_err());
        assert!(
            store
                .load(&settled.record.transaction_id)
                .unwrap()
                .resolution
                .is_none()
        );
    } else {
        let (record, outcome) = result.unwrap();
        assert_eq!(
            outcome,
            TransactionOutcome::Committed,
            "{:?}",
            record.recovery_failure
        );
        publisher
            .reconcile_installation(&record, SelectedScope::User)
            .unwrap();
        let installed = publisher.load(&app, SelectedScope::User).unwrap().unwrap();
        zup_transaction::verify_published_installation(&record, &installed).unwrap();
        assert!(installed.current.resources.values().all(|resource| !matches!(resource, zup_plan::OwnedResource::File { instance, .. } if instance.object == foreign)));
    }
    assert_eq!(std::fs::read(&backup).unwrap(), b"externally modified");
    let metadata = std::fs::symlink_metadata(backup).unwrap();
    assert_eq!(
        zup_core::NativeObjectIdentity::Linux {
            device: metadata.dev(),
            inode: metadata.ino()
        },
        foreign
    );
    assert_eq!(
        std::fs::read(install.join("program")).unwrap(),
        b"authenticated"
    );
}
