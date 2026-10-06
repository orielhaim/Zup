#![cfg(target_os = "linux")]

//! The real coordinator driving the Linux executor, end to end.
//!
//! The unit tests in `executor.rs` drive the executor by hand; these prove the
//! portable [`TransactionCoordinator`] accepts it as an [`OperationExecutor`]
//! and runs the full prepare / stage / apply / verify / commit protocol against
//! a real filesystem journal. A hand-driven executor that the coordinator
//! cannot drive would be a demonstration, not a backend.

use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;

use zup_bundle::DirectoryPayloadSource;
use zup_core::{
    AppId, Privilege, RelativePath, ResourceKey, SelectedScope, Sha256Digest, TargetTriple,
    hash_reader,
};
use zup_linux::LinuxFileExecutor;
use zup_platform::TargetPath;
use zup_transaction::{
    FileDelta, FilePrecondition, FileWork, FilesystemTransactionStore, TransactionCoordinator,
    TransactionInput, TransactionOutcome, compile_transaction, recover,
};

fn target() -> TargetTriple {
    TargetTriple::parse("x86_64-unknown-linux-gnu").expect("a Linux target")
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
) -> FileWork {
    FileWork {
        key: ResourceKey::File {
            destination: destination.to_string(),
        },
        source_relative: RelativePath::new(name).expect("a relative path"),
        destination,
        precondition,
        expected_sha256: digest(bytes),
        expected_size: bytes.len() as u64,
        privilege: Privilege::User,
        executable,
        delta: match precondition {
            FilePrecondition::Absent => FileDelta::Create,
            FilePrecondition::Exact { .. } => FileDelta::Replace,
        },
    }
}

/// Run one transaction to a stable outcome through the real coordinator.
fn run(
    input: &TransactionInput,
    payload_root: &Path,
    state_root: &Path,
) -> (TransactionOutcome, LinuxFileExecutor) {
    let plan = compile_transaction(input).expect("a plan compiles");
    let store = FilesystemTransactionStore::new(state_root);
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
    let (_, outcome) = coordinator
        .execute(record, &mut executor)
        .expect("execution reaches a stable outcome");
    (outcome, executor)
}

/// A two-file install commits through the coordinator, and the executable bit
/// lands where the intent said.
#[test]
fn the_coordinator_commits_a_linux_install() {
    let root = tempfile::tempdir().expect("a temp directory");
    let payload = root.path().join("payload");
    let install = root.path().join("install");
    let state = root.path().join("state");
    std::fs::create_dir_all(&payload).expect("payload");
    std::fs::write(payload.join("tool"), b"#!/bin/sh\n").expect("tool");
    std::fs::write(payload.join("data.dat"), b"the data").expect("data");

    let mut input = TransactionInput::new(target());
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

/// A contested destination fails the whole transaction, and the engine is
/// honest about what it cannot prove: a create refused by the kernel leaves
/// nothing behind, but the coordinator settles a failed apply through the same
/// reconciliation a crash would get, and foreign bytes at the destination are
/// ambiguous there. The outcome is recovery-required rather than rolled-back,
/// which is the same answer the Windows backend gives for the same collision.
/// What matters is what the machine holds: the file that was already there is
/// untouched, and the uncontested file is never published.
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

    let mut input = TransactionInput::new(target());
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
    assert_eq!(outcome, TransactionOutcome::RecoveryRequired);
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

/// An upgrade is a replace with an exact precondition: the old bytes must be
/// what the plan says they are, and the backup holds them afterwards.
#[test]
fn the_coordinator_applies_an_upgrade_as_a_replace() {
    let root = tempfile::tempdir().expect("a temp directory");
    let payload = root.path().join("payload");
    let install = root.path().join("install");
    let state = root.path().join("state");
    std::fs::create_dir_all(&payload).expect("payload");
    std::fs::write(payload.join("tool"), b"v1").expect("v1");

    let mut v1 = TransactionInput::new(target());
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
    let mut v2 = TransactionInput::new(target());
    v2.files.push(work(
        "tool",
        b"v2",
        destination(&install, "tool"),
        FilePrecondition::Exact {
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

/// Recovery replays from the journal alone: a record left mid-apply by a dead
/// process reconciles the published file as applied and reaches commit without
/// the original executor, payload handles, or registrations.
#[test]
fn recovery_replays_a_journal_without_the_original_executor() {
    use zup_transaction::{NodeState, TransactionPhase, TransactionStore as _};

    let root = tempfile::tempdir().expect("a temp directory");
    let payload = root.path().join("payload");
    let install = root.path().join("install");
    let state = root.path().join("state");
    std::fs::create_dir_all(&payload).expect("payload");
    std::fs::write(payload.join("tool"), b"the payload").expect("tool");

    let mut input = TransactionInput::new(target());
    input.files.push(work(
        "tool",
        b"the payload",
        destination(&install, "tool"),
        FilePrecondition::Absent,
        false,
    ));
    let plan = compile_transaction(&input).expect("a plan compiles");

    // Drive the plan by hand up to and including the publish, journaling each
    // receipt the way the coordinator would - then stop cold. This is the crash:
    // staging nodes applied, mutation nodes caught running, phase Applying.
    let store = FilesystemTransactionStore::new(&state);
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
    {
        let mut executor =
            LinuxFileExecutor::new().with_payload(DirectoryPayloadSource::new(&payload));
        executor.register_plan(&plan).expect("a plan registers");
        let mut staged = Vec::new();
        for node in plan
            .nodes
            .iter()
            .filter(|node| matches!(node.kind, zup_transaction::NodeKind::StageFile { .. }))
        {
            let receipt = executor.apply(node).expect("a stage applies");
            staged.push((node.id.clone(), receipt));
        }
        for node in plan
            .nodes
            .iter()
            .filter(|node| matches!(node.kind, zup_transaction::NodeKind::FileMutation { .. }))
        {
            // The mutation publishes - then the process dies before journaling it.
            let _ = executor.apply(node).expect("a mutation applies");
        }
        let store = FilesystemTransactionStore::new(&state);
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
                for node in plan.nodes.iter().filter(|node| {
                    matches!(node.kind, zup_transaction::NodeKind::FileMutation { .. })
                }) {
                    record.nodes.insert(node.id.clone(), NodeState::Running);
                }
                record.phase = TransactionPhase::Applying;
                Ok(())
            })
            .expect("the interrupted phase persists");
        // The executor drops here. So does the process, in the real story.
    }

    // A new process, a new executor, no registrations carried over: only the
    // journal on disk.
    let store = FilesystemTransactionStore::new(&state);
    let record = store.load(&transaction_id).expect("the journal survives");
    let mut executor = LinuxFileExecutor::new().with_payload(DirectoryPayloadSource::new(&payload));
    executor.register_plan(&plan).expect("a plan registers");
    let (_, outcome) = recover(record, &store, &mut executor).expect("recovery runs");
    assert_eq!(outcome, TransactionOutcome::Committed);
    assert_eq!(
        std::fs::read(install.join("tool")).expect("read"),
        b"the payload"
    );
}
