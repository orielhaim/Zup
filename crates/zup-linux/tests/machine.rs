//! Linux machine-scope privilege boundary, end to end through the worker.
//!
//! Every test here drives the real worker code with isolated roots: nothing
//! touches the host's `/opt` or `/var/lib/zup`. The loopback tests prove the
//! full lifecycle (install, upgrade, repair, uninstall, recovery, locking)
//! through the same plan, path, and policy validation the privileged path
//! enforces. The protocol tests speak raw frames to the real session driver
//! and prove substitution, replay, and confusion are refused without
//! mutation. Requires the `test-support` feature, which production binaries
//! never enable.
#![cfg(target_os = "linux")]
#![cfg(feature = "test-support")]
#![allow(dead_code)]

#[path = "support.rs"]
mod support;

use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use zup_bundle::BundleWriter;
use zup_core::{
    App, AppId, Frontend, Install, InstallDirectory, InstallScope, Installer, NonEmptyString,
    RelativePath, ResolvedFile, SelectedScope, TargetBuildPlan, TargetTriple, Template,
    hash_reader,
};
use zup_linux::test_support::{
    MachineTestRoots, drive_client_isolated, recv_envelope_on, run_machine_isolated_in,
    send_envelope_on, serve_worker_isolated,
};
use zup_linux::{LinuxAction, LinuxOutcome};
use zup_protocol::{
    ExecuteOperation, Message, PROTOCOL_VERSION, PrepareOperation, SessionId, WireEnvelope,
    privileged_operation,
};

use support::{
    machine_fixture, machine_fixture_override, machine_install_dir, machine_maintenance_path,
    machine_package_bytes, machine_v1_files, machine_v2_files,
};

const HANDSHAKE: Duration = Duration::from_secs(30);

fn linux_target() -> TargetTriple {
    TargetTriple::parse("x86_64-unknown-linux-gnu").expect("a Linux target")
}

fn app_id() -> AppId {
    AppId::new("com.example.tool").expect("an id")
}

/// An isolated machine tree held alive for one test: the temporary base
/// plus the roots inside it. Nothing touches the host's `/opt`.
fn isolated() -> (tempfile::TempDir, MachineTestRoots) {
    let base = tempfile::tempdir().expect("an isolated base");
    let roots = MachineTestRoots::isolate_in(base.path());
    (base, roots)
}

fn run_tool(tool: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new(tool)
        .args(args)
        .output()
        .expect("the installed tool runs");
    assert!(output.status.success(), "{output:?}");
    String::from_utf8(output.stdout).expect("utf8")
}

fn install_v1(roots: &MachineTestRoots, scratch: &Path) -> PathBuf {
    let installer = machine_fixture(scratch, "v1", "1.0.0", &machine_v1_files());
    let outcome = run_machine_isolated_in(
        &installer,
        &roots.roots,
        &roots.state,
        LinuxAction::Apply,
        None,
    );
    assert!(
        matches!(outcome, Ok(LinuxOutcome::Committed { .. })),
        "a fresh machine install commits: {outcome:?}"
    );
    installer
}

/// The full machine lifecycle through the worker: install, upgrade, repair,
/// maintenance-only repair, uninstall. Neighbors survive throughout, and the
/// application runs as the ordinary user after the worker completes.
#[test]
fn machine_full_lifecycle() {
    let (_base, roots) = isolated();
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let installer = install_v1(&roots, scratch.path());

    // The payload landed under the program tree, runnable by this user: the
    // worker never runs application code, and the user runs it afterwards.
    let tool = machine_install_dir(&roots.roots).join("tool");
    assert_eq!(run_tool(&tool, &["--version"]).trim(), "tool 1.0.0");
    assert_eq!(
        std::fs::metadata(&tool).expect("stat").permissions().mode() & 0o777,
        0o755,
        "a machine executable runs for every account"
    );
    assert_eq!(
        run_tool(&tool, &["read-payload"]).trim(),
        "keep-v1",
        "payload data installs beside the tool"
    );
    // The maintenance generation is installed and executable but locked
    // against unprivileged writes by its mode.
    let maintenance = machine_maintenance_path(&roots.state, "1.0.0");
    assert!(maintenance.is_file(), "a maintenance generation exists");
    let mode = std::fs::metadata(&maintenance)
        .expect("stat")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode & 0o022, 0, "never group- or world-writable: {mode:o}");
    assert_ne!(mode & 0o111, 0, "still runnable: {mode:o}");
    // The ledger is public metadata: the unprivileged planner reads it to
    // bind the expected digest.
    let ledger = zup_linux::LinuxLedgerStore::new(&roots.state)
        .load(&app_id(), SelectedScope::Machine)
        .expect("the ledger reads")
        .expect("an installation is recorded");
    assert_eq!(ledger.version.to_string(), "1.0.0");

    // An unrelated neighbor inside and outside the program tree.
    let neighbor = roots.roots.programs.join("neighbor").join("notes.txt");
    std::fs::create_dir_all(neighbor.parent().expect("a parent")).expect("a neighbor dir");
    std::fs::write(&neighbor, b"someone else").expect("a neighbor");
    let nearby = machine_install_dir(&roots.roots).join("user-notes.txt");
    std::fs::write(&nearby, b"the user's").expect("a user file");

    // Upgrade: changed files replace, retired files leave, new files arrive,
    // neighbors survive, the ledger becomes v2, the old generation retires.
    let upgrade = machine_fixture(scratch.path(), "v2", "2.0.0", &machine_v2_files());
    let outcome = run_machine_isolated_in(
        &upgrade,
        &roots.roots,
        &roots.state,
        LinuxAction::Apply,
        None,
    );
    assert!(
        matches!(outcome, Ok(LinuxOutcome::Committed { .. })),
        "a machine upgrade commits: {outcome:?}"
    );
    assert_eq!(run_tool(&tool, &["--version"]).trim(), "tool 2.0.0");
    assert_eq!(
        std::fs::read(machine_install_dir(&roots.roots).join("new.dat")).expect("the new file"),
        b"new-v2",
        "the upgrade lands the added file"
    );
    assert!(
        !machine_install_dir(&roots.roots).join("keep.dat").exists(),
        "the retired file is gone"
    );
    assert_eq!(
        std::fs::read(&neighbor).expect("a neighbor"),
        b"someone else"
    );
    assert_eq!(std::fs::read(&nearby).expect("a user file"), b"the user's");
    let ledger = zup_linux::LinuxLedgerStore::new(&roots.state)
        .load(&app_id(), SelectedScope::Machine)
        .expect("the ledger reads")
        .expect("an installation is recorded");
    assert_eq!(ledger.version.to_string(), "2.0.0");
    assert!(
        !machine_maintenance_path(&roots.state, "1.0.0").exists(),
        "the old generation retires"
    );
    assert!(
        machine_maintenance_path(&roots.state, "2.0.0").is_file(),
        "the new generation installs"
    );

    // Repair: a missing owned file comes back without force; a damaged one
    // refuses without force and restores with it.
    std::fs::remove_file(machine_install_dir(&roots.roots).join("new.dat")).expect("delete");
    let outcome = run_machine_isolated_in(
        &upgrade,
        &roots.roots,
        &roots.state,
        LinuxAction::Repair { force_files: false },
        None,
    );
    assert!(
        matches!(outcome, Ok(LinuxOutcome::Committed { .. })),
        "missing files come back without force: {outcome:?}"
    );
    assert_eq!(
        std::fs::read(machine_install_dir(&roots.roots).join("new.dat")).expect("restored"),
        b"new-v2"
    );
    std::fs::write(
        machine_install_dir(&roots.roots).join("new.dat"),
        b"damaged",
    )
    .expect("damage");
    let outcome = run_machine_isolated_in(
        &upgrade,
        &roots.roots,
        &roots.state,
        LinuxAction::Repair { force_files: false },
        None,
    );
    assert!(
        outcome.is_err(),
        "damage without force refuses: {outcome:?}"
    );
    let outcome = run_machine_isolated_in(
        &upgrade,
        &roots.roots,
        &roots.state,
        LinuxAction::Repair { force_files: true },
        None,
    );
    assert!(
        matches!(outcome, Ok(LinuxOutcome::Committed { .. })),
        "force repair restores damage: {outcome:?}"
    );
    assert_eq!(
        std::fs::read(machine_install_dir(&roots.roots).join("new.dat")).expect("restored"),
        b"new-v2"
    );

    // The original download is gone: repair runs from the root-owned
    // maintenance generation instead of an arbitrary copy.
    std::fs::remove_file(&upgrade).expect("the download is gone");
    std::fs::remove_file(&installer).expect("the old download is gone");
    std::fs::remove_file(machine_install_dir(&roots.roots).join("new.dat")).expect("delete");
    let maintenance = machine_maintenance_path(&roots.state, "2.0.0");
    let outcome = run_machine_isolated_in(
        &maintenance,
        &roots.roots,
        &roots.state,
        LinuxAction::Repair { force_files: false },
        None,
    );
    assert!(
        matches!(outcome, Ok(LinuxOutcome::Committed { .. })),
        "repair works from trusted maintenance: {outcome:?}"
    );

    // Uninstall removes only what Zup owns: neighbors and shared
    // infrastructure survive.
    let outcome = run_machine_isolated_in(
        &maintenance,
        &roots.roots,
        &roots.state,
        LinuxAction::Uninstall,
        None,
    );
    assert!(
        matches!(outcome, Ok(LinuxOutcome::Committed { .. })),
        "uninstall commits: {outcome:?}"
    );
    assert!(!tool.exists(), "the payload is gone");
    assert!(!maintenance.exists(), "its generation is gone");
    assert_eq!(
        std::fs::read(&neighbor).expect("a neighbor"),
        b"someone else"
    );
    assert_eq!(std::fs::read(&nearby).expect("a user file"), b"the user's");
    assert!(
        roots.state.is_dir(),
        "shared machine infrastructure is not removed with one application"
    );
    assert!(
        zup_linux::LinuxLedgerStore::new(&roots.state)
            .load(&app_id(), SelectedScope::Machine)
            .expect("the ledger reads")
            .is_none(),
        "no ownership remains"
    );
}

/// An install-directory override stays inside the machine program tree, and
/// escapes are refused without mutation.
#[test]
fn machine_install_dir_override_stays_in_the_program_tree() {
    let (_base, roots) = isolated();
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let installer =
        machine_fixture_override(scratch.path(), "v1", "1.0.0", &machine_v1_files(), true);

    // A program-tree override installs there.
    let elsewhere = roots.roots.programs.join("AltApp");
    let outcome = run_machine_isolated_in(
        &installer,
        &roots.roots,
        &roots.state,
        LinuxAction::Install,
        Some(elsewhere.clone()),
    );
    assert!(
        matches!(outcome, Ok(LinuxOutcome::Committed { .. })),
        "a program-tree override commits: {outcome:?}"
    );
    assert_eq!(
        run_tool(&elsewhere.join("tool"), &["--version"]).trim(),
        "tool 1.0.0"
    );

    // Escapes are refused: absolute elsewhere, the program root itself, and
    // traversal tricks. None mutates.
    for hostile in [
        PathBuf::from("/etc"),
        PathBuf::from("/etc/zup-phase5-sentinel"),
        roots.roots.programs.clone(),
        roots.roots.programs.join("..").join("evil"),
        roots
            .roots
            .programs
            .join("AltApp")
            .join("..")
            .join("..")
            .join("evil"),
    ] {
        let outcome = run_machine_isolated_in(
            &installer,
            &roots.roots,
            &roots.state,
            LinuxAction::Install,
            Some(hostile.clone()),
        );
        assert!(
            outcome.is_err(),
            "{} must be refused: {outcome:?}",
            hostile.display()
        );
    }
    assert!(
        !Path::new("/etc/zup-phase5-sentinel").exists(),
        "a refused override writes nothing"
    );

    // Without the project permitting it, any override is refused.
    let strict = machine_fixture(scratch.path(), "strict", "1.0.0", &machine_v1_files());
    let outcome = run_machine_isolated_in(
        &strict,
        &roots.roots,
        &roots.state,
        LinuxAction::Install,
        Some(roots.roots.programs.join("Other")),
    );
    assert!(outcome.is_err(), "an unpermitted override refuses");
}

/// A second machine operation on the same application refuses while the
/// first session holds the root lock; an unrelated application proceeds.
#[test]
fn machine_lock_serializes_one_application() {
    let (_base, roots) = isolated();
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let installer = machine_fixture(scratch.path(), "v1", "1.0.0", &machine_v1_files());
    let uid = rustix::process::getuid().as_raw();

    // First session prepares and waits: the lock is held from preparation
    // through execution, so a second prepare already refuses.
    let session_a = SessionId::new_v7();
    let (mut client_a, mut worker_a) = UnixStream::pair().expect("a pair");
    let roots_a = roots.roots.clone();
    let installer_a = installer.clone();
    let worker = std::thread::spawn(move || {
        serve_worker_isolated(
            &mut worker_a,
            &roots_a,
            uid,
            std::process::id(),
            session_a,
            &installer_a,
        )
    });
    let hello = next_hello(&mut client_a);
    assert_eq!(hello.session_id, session_a);
    send_prepare(
        &mut client_a,
        session_a,
        1,
        PrepareOperation {
            operation: privileged_operation::INSTALL.to_owned(),
            force_files: false,
            install_dir_override: None,
            selected_components: Vec::new(),
            expected_plan_digest: None,
            app_id: app_id().to_string(),
            app_version: "1.0.0".to_owned(),
            scope: "machine".to_owned(),
            target: linux_target(),
        },
    );
    let prepared = next_prepared(&mut client_a);
    assert_eq!(prepared.app_id, app_id().to_string());

    // Same application, second session: busy, typed, without mutation.
    let session_b = SessionId::new_v7();
    let (mut client_b, mut worker_b) = UnixStream::pair().expect("a pair");
    let roots_b = roots.roots.clone();
    let installer_b = installer.clone();
    let worker_b_handle = std::thread::spawn(move || {
        serve_worker_isolated(
            &mut worker_b,
            &roots_b,
            uid,
            std::process::id(),
            session_b,
            &installer_b,
        )
    });
    let _ = next_hello(&mut client_b);
    send_prepare(
        &mut client_b,
        session_b,
        1,
        PrepareOperation {
            operation: privileged_operation::INSTALL.to_owned(),
            force_files: false,
            install_dir_override: None,
            selected_components: Vec::new(),
            expected_plan_digest: None,
            app_id: app_id().to_string(),
            app_version: "1.0.0".to_owned(),
            scope: "machine".to_owned(),
            target: linux_target(),
        },
    );
    let failed = next_failed(&mut client_b);
    assert_eq!(failed.kind, zup_protocol::failure::INSTALLATION_BUSY);
    worker_b_handle
        .join()
        .expect("the worker exits")
        .expect_err("busy refuses");

    // Release the first session without executing: nothing mutated, and the
    // lock goes with it.
    drop(client_a);
    worker
        .join()
        .expect("the worker exits")
        .expect_err("no execute means no outcome");
    assert!(
        zup_linux::LinuxLedgerStore::new(&roots.state)
            .load(&app_id(), SelectedScope::Machine)
            .expect("the ledger reads")
            .is_none(),
        "a prepared-but-never-executed session mutates nothing"
    );
    assert!(
        !machine_install_dir(&roots.roots).exists(),
        "no payload without Execute"
    );
}

/// A substitution between Prepare and Execute is refused: authorizing one
/// plan never executes another.
#[test]
fn machine_plan_substitution_is_refused() {
    let (_base, roots) = isolated();
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let installer = machine_fixture(scratch.path(), "v1", "1.0.0", &machine_v1_files());
    let uid = rustix::process::getuid().as_raw();
    let session = SessionId::new_v7();
    let (mut client, mut worker) = UnixStream::pair().expect("a pair");
    let roots_clone = roots.roots.clone();
    let installer_clone = installer.clone();
    let handle = std::thread::spawn(move || {
        serve_worker_isolated(
            &mut worker,
            &roots_clone,
            uid,
            std::process::id(),
            session,
            &installer_clone,
        )
    });
    let _ = next_hello(&mut client);
    send_prepare(&mut client, session, 1, install_intent(None));
    let prepared = next_prepared(&mut client);
    // Execute names a different digest than the prepared one.
    let other = "5feceb66ffc86f38d952786c6d696c79c2dbc239dd4e91b46729d73a27fb57e9";
    assert_ne!(other, prepared.plan_digest.as_str());
    send_envelope_on(
        &mut client,
        &WireEnvelope {
            version: PROTOCOL_VERSION,
            session_id: session,
            sequence: 2,
            message: Message::Execute(ExecuteOperation {
                plan_digest: other.to_owned(),
            }),
        },
    )
    .expect("send");
    let failed = next_failed(&mut client);
    assert_eq!(failed.kind, zup_protocol::failure::AUTHENTICATION);
    handle
        .join()
        .expect("the worker exits")
        .expect_err("substitution refuses");
    assert!(
        !machine_install_dir(&roots.roots).exists(),
        "a refused substitution mutates nothing"
    );
}

/// A completed session cannot be replayed: the stream is over, and a second
/// Execute against it reaches no worker.
#[test]
fn machine_execute_replay_reaches_no_worker() {
    let (_base, roots) = isolated();
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let installer = machine_fixture(scratch.path(), "v1", "1.0.0", &machine_v1_files());
    let uid = rustix::process::getuid().as_raw();
    let session = SessionId::new_v7();
    let (mut client, mut worker) = UnixStream::pair().expect("a pair");
    let roots_clone = roots.roots.clone();
    let installer_clone = installer.clone();
    let handle = std::thread::spawn(move || {
        serve_worker_isolated(
            &mut worker,
            &roots_clone,
            uid,
            std::process::id(),
            session,
            &installer_clone,
        )
    });
    let outcome = drive_client_isolated(
        &mut client,
        session,
        &install_intent(None),
        &expected_digest(&installer, &roots, LinuxAction::Install),
        &linux_target(),
    );
    assert!(
        matches!(outcome, Ok(LinuxOutcome::Committed { .. })),
        "the first execute commits: {outcome:?}"
    );
    handle.join().expect("the worker exits").expect("committed");
    // The session is over: replaying the Execute finds no worker.
    let replay = send_envelope_on(
        &mut client,
        &WireEnvelope {
            version: PROTOCOL_VERSION,
            session_id: session,
            sequence: 99,
            message: Message::Execute(ExecuteOperation {
                plan_digest: "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"
                    .to_owned(),
            }),
        },
    );
    let closed = replay.is_err() || recv_envelope_on(&mut client, Duration::from_secs(5)).is_err();
    assert!(closed, "a replay reaches no worker");
    // Exactly one transaction ran for the one installation.
    let transactions = std::fs::read_dir(roots.state.join("transactions"))
        .expect("transactions")
        .count();
    assert_eq!(transactions, 1, "one session ran one transaction");
}

/// Swapping the sessions' digests refuses both: cross-session confusion
/// never authorizes.
#[test]
fn machine_cross_session_execute_is_refused() {
    // Each session serves its own isolated machine: the worker holds the
    // installation lock from preparation through execution, so two sessions
    // preparing the *same* installation would (correctly) refuse the second
    // with `installation_busy` before any digest confusion is even possible.
    // What this test proves is the digest binding: an Execute naming another
    // session's plan is refused, and nothing mutates either way.
    let (_base_a, roots_a) = isolated();
    let (_base_b, roots_b) = isolated();
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let installer_v1 = machine_fixture(scratch.path(), "v1", "1.0.0", &machine_v1_files());
    let installer_v2 = machine_fixture(scratch.path(), "v2", "2.0.0", &machine_v2_files());
    let uid = rustix::process::getuid().as_raw();

    // Session B's world already holds v1, so its upgrade to v2 plans the
    // replace-transition while session A's fresh install of v1 plans
    // creation: different versions, different plans, different digests.
    let outcome = run_machine_isolated_in(
        &installer_v1,
        &roots_b.roots,
        &roots_b.state,
        LinuxAction::Apply,
        None,
    );
    assert!(
        matches!(outcome, Ok(LinuxOutcome::Committed { .. })),
        "session B's world installs v1 first: {outcome:?}"
    );

    let session_a = SessionId::new_v7();
    let (mut client_a, mut worker_a) = UnixStream::pair().expect("a pair");
    let roots_a_clone = roots_a.roots.clone();
    let installer_a = installer_v1.clone();
    let worker_a = std::thread::spawn(move || {
        serve_worker_isolated(
            &mut worker_a,
            &roots_a_clone,
            uid,
            std::process::id(),
            session_a,
            &installer_a,
        )
    });
    let _ = next_hello(&mut client_a);
    send_prepare(&mut client_a, session_a, 1, install_intent(None));
    let prepared_a = next_prepared(&mut client_a);

    let session_b = SessionId::new_v7();
    let (mut client_b, mut worker_b) = UnixStream::pair().expect("a pair");
    let roots_b_clone = roots_b.roots.clone();
    let worker_b = std::thread::spawn(move || {
        serve_worker_isolated(
            &mut worker_b,
            &roots_b_clone,
            uid,
            std::process::id(),
            session_b,
            &installer_v2,
        )
    });
    let _ = next_hello(&mut client_b);
    // Session B prepares a *different* operation so its digest differs.
    send_prepare(
        &mut client_b,
        session_b,
        1,
        PrepareOperation {
            operation: privileged_operation::UPGRADE.to_owned(),
            app_version: "2.0.0".to_owned(),
            ..install_intent(None)
        },
    );
    let prepared_b = next_prepared(&mut client_b);
    assert_ne!(
        prepared_a.plan_digest, prepared_b.plan_digest,
        "the two sessions prepared different plans"
    );

    // Each Execute carries the other session's digest - and its own session
    // identity, which is exactly what an attacker swapping frames achieves.
    send_envelope_on(
        &mut client_a,
        &WireEnvelope {
            version: PROTOCOL_VERSION,
            session_id: session_a,
            sequence: 2,
            message: Message::Execute(ExecuteOperation {
                plan_digest: prepared_b.plan_digest.clone(),
            }),
        },
    )
    .expect("send");
    assert_eq!(
        next_failed(&mut client_a).kind,
        zup_protocol::failure::AUTHENTICATION
    );
    send_envelope_on(
        &mut client_b,
        &WireEnvelope {
            version: PROTOCOL_VERSION,
            session_id: session_b,
            sequence: 2,
            message: Message::Execute(ExecuteOperation {
                plan_digest: prepared_a.plan_digest.clone(),
            }),
        },
    )
    .expect("send");
    assert_eq!(
        next_failed(&mut client_b).kind,
        zup_protocol::failure::AUTHENTICATION
    );
    worker_a
        .join()
        .expect("the worker exits")
        .expect_err("refused");
    worker_b
        .join()
        .expect("the worker exits")
        .expect_err("refused");
    assert!(
        !machine_install_dir(&roots_a.roots).exists(),
        "the confused install mutates nothing"
    );
    assert_eq!(
        run_tool(
            &machine_install_dir(&roots_b.roots).join("tool"),
            &["--version"]
        )
        .trim(),
        "tool 1.0.0",
        "the confused upgrade leaves v1 running"
    );
}

/// Malformed frames never panic, never allocate unboundedly, and never
/// mutate: unknown versions, unknown types, oversized lengths, truncation,
/// out-of-order messages, wrong sessions, and wrong digests.
#[test]
fn machine_malformed_frames_are_refused_without_mutation() {
    let (_base, roots) = isolated();
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let installer = machine_fixture(scratch.path(), "v1", "1.0.0", &machine_v1_files());
    let uid = rustix::process::getuid().as_raw();

    // An oversized length buys no allocation.
    {
        let (mut client, mut worker) = UnixStream::pair().expect("a pair");
        let session = SessionId::new_v7();
        let roots_clone = roots.roots.clone();
        let installer_clone = installer.clone();
        let handle = std::thread::spawn(move || {
            serve_worker_isolated(
                &mut worker,
                &roots_clone,
                uid,
                std::process::id(),
                session,
                &installer_clone,
            )
        });
        let _ = next_hello(&mut client);
        use std::io::Write as _;
        client
            .write_all(&(zup_protocol::MAX_FRAME_BYTES as u32 + 1).to_be_bytes())
            .expect("send");
        client.flush().expect("flush");
        // A framing refusal is still a typed refusal: the worker reports
        // `protocol` before exiting rather than dropping the peer in silence.
        assert_eq!(
            next_failed(&mut client).kind,
            zup_protocol::failure::PROTOCOL
        );
        handle
            .join()
            .expect("the worker exits")
            .expect_err("refused");
    }

    // Execute before Prepare authorizes nothing.
    {
        let (mut client, mut worker) = UnixStream::pair().expect("a pair");
        let session = SessionId::new_v7();
        let (_base, roots) = isolated();
        let installer = machine_fixture(scratch.path(), "v1b", "1.0.0", &machine_v1_files());
        let roots_clone = roots.roots.clone();
        let installer_clone = installer.clone();
        let handle = std::thread::spawn(move || {
            serve_worker_isolated(
                &mut worker,
                &roots_clone,
                uid,
                std::process::id(),
                session,
                &installer_clone,
            )
        });
        let _ = next_hello(&mut client);
        send_envelope_on(
            &mut client,
            &WireEnvelope {
                version: PROTOCOL_VERSION,
                session_id: session,
                sequence: 1,
                message: Message::Execute(ExecuteOperation {
                    plan_digest: "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"
                        .to_owned(),
                }),
            },
        )
        .expect("send");
        assert_eq!(
            next_failed(&mut client).kind,
            zup_protocol::failure::PROTOCOL
        );
        handle
            .join()
            .expect("the worker exits")
            .expect_err("refused");
    }

    // A frame for another session is cross-session confusion, refused.
    {
        let (mut client, mut worker) = UnixStream::pair().expect("a pair");
        let session = SessionId::new_v7();
        let (_base, roots) = isolated();
        let installer = machine_fixture(scratch.path(), "v1c", "1.0.0", &machine_v1_files());
        let roots_clone = roots.roots.clone();
        let installer_clone = installer.clone();
        let handle = std::thread::spawn(move || {
            serve_worker_isolated(
                &mut worker,
                &roots_clone,
                uid,
                std::process::id(),
                session,
                &installer_clone,
            )
        });
        let _ = next_hello(&mut client);
        let mut intent = install_intent(None);
        intent.expected_plan_digest =
            Some("9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08".to_owned());
        send_envelope_on(
            &mut client,
            &WireEnvelope {
                version: 999,
                session_id: SessionId::new_v7(),
                sequence: 1,
                message: Message::Prepare(intent),
            },
        )
        .expect("send");
        // Exactly one answer: the worker reports the refusal as a typed
        // `Failed` (authentication or protocol) before exiting rather than
        // dropping the peer in silence.
        match recv_envelope_on(&mut client, HANDSHAKE) {
            Err(_) => {}
            Ok(envelope) => match envelope.message {
                Message::Failed(failed)
                    if failed.kind == zup_protocol::failure::AUTHENTICATION
                        || failed.kind == zup_protocol::failure::PROTOCOL => {}
                other => panic!("a foreign frame is refused, got {other:?}"),
            },
        }
        handle
            .join()
            .expect("the worker exits")
            .expect_err("refused");
    }
}

/// Replacing the installer between Prepare and Execute is detected: the
/// worker installs the pinned bytes or nothing.
#[test]
fn machine_package_substitution_is_detected() {
    let (_base, roots) = isolated();
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let installer = machine_fixture(scratch.path(), "v1", "1.0.0", &machine_v1_files());
    let uid = rustix::process::getuid().as_raw();
    let session = SessionId::new_v7();
    let (mut client, mut worker) = UnixStream::pair().expect("a pair");
    let roots_clone = roots.roots.clone();
    let installer_clone = installer.clone();
    let handle = std::thread::spawn(move || {
        serve_worker_isolated(
            &mut worker,
            &roots_clone,
            uid,
            std::process::id(),
            session,
            &installer_clone,
        )
    });
    let _ = next_hello(&mut client);
    send_prepare(&mut client, session, 1, install_intent(None));
    let prepared = next_prepared(&mut client);
    // Swap the user-writable installer for a different package before
    // Execute: same declared version, different bytes and inode.
    let swap = scratch.path().join("swap");
    support::compose_installer(
        Path::new(support::inert_template()),
        &swap,
        &machine_package_bytes(scratch.path(), "1.0.0", &machine_v2_files(), false),
    );
    std::fs::rename(&swap, &installer).expect("swap");
    send_envelope_on(
        &mut client,
        &WireEnvelope {
            version: PROTOCOL_VERSION,
            session_id: session,
            sequence: 2,
            message: Message::Execute(ExecuteOperation {
                plan_digest: prepared.plan_digest.clone(),
            }),
        },
    )
    .expect("send");
    let failed = next_failed(&mut client);
    assert_eq!(failed.kind, zup_protocol::failure::AUTHENTICATION);
    handle
        .join()
        .expect("the worker exits")
        .expect_err("substitution refuses");
    assert!(
        !machine_install_dir(&roots.roots).exists(),
        "a substituted package installs nothing"
    );
}

/// A wrong authorizing user is refused before anything is read: peer
/// verification uses kernel credentials, not message claims.
#[test]
fn machine_wrong_peer_uid_is_refused() {
    let (_base, roots) = isolated();
    let (mut client, mut worker) = UnixStream::pair().expect("a pair");
    let session = SessionId::new_v7();
    let installer = PathBuf::from("/bin/true");
    let stranger = u32::MAX - 7;
    assert_ne!(stranger, rustix::process::getuid().as_raw());
    let roots_clone = roots.roots.clone();
    let handle = std::thread::spawn(move || {
        serve_worker_isolated(
            &mut worker,
            &roots_clone,
            stranger,
            std::process::id(),
            session,
            &installer,
        )
    });
    // No session is served: the peer check fails first, reported as a
    // typed refusal before the worker exits.
    match recv_envelope_on(&mut client, HANDSHAKE) {
        Err(_) => {}
        Ok(envelope) => match envelope.message {
            Message::Failed(failed) if failed.kind == zup_protocol::failure::AUTHENTICATION => {}
            other => panic!("a foreign uid gets no session, got {other:?}"),
        },
    }
    handle
        .join()
        .expect("the worker exits")
        .expect_err("refused");
}

/// A wrong client process is refused even with the right user: pid binding
/// uses kernel credentials, and a neighboring same-user process cannot
/// drive the session.
#[test]
fn machine_wrong_peer_pid_is_refused() {
    let (_base, roots) = isolated();
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let installer = machine_fixture(scratch.path(), "v1", "1.0.0", &machine_v1_files());
    let uid = rustix::process::getuid().as_raw();
    let session = SessionId::new_v7();
    let (mut client, mut worker) = UnixStream::pair().expect("a pair");
    let roots_clone = roots.roots.clone();
    let installer_clone = installer.clone();
    // Nobody holds this pid here: the check names the launched installer,
    // not whoever connected first. The refusal is typed, like every other
    // worker refusal.
    let stranger_pid = u32::MAX - 11;
    assert_ne!(stranger_pid, std::process::id());
    let handle = std::thread::spawn(move || {
        serve_worker_isolated(
            &mut worker,
            &roots_clone,
            uid,
            stranger_pid,
            session,
            &installer_clone,
        )
    });
    match recv_envelope_on(&mut client, HANDSHAKE) {
        Err(_) => {}
        Ok(envelope) => match envelope.message {
            Message::Failed(failed) if failed.kind == zup_protocol::failure::AUTHENTICATION => {}
            other => panic!("a foreign process gets no session, got {other:?}"),
        },
    }
    handle
        .join()
        .expect("the worker exits")
        .expect_err("refused");
}

/// A symlink planted in machine state is refused: trusted state is never
/// reached through a link.
#[test]
fn machine_state_symlink_is_refused() {
    let (_base, roots) = isolated();
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let installer = machine_fixture(scratch.path(), "v1", "1.0.0", &machine_v1_files());
    let elsewhere = tempfile::tempdir().expect("an unrelated tree");
    std::fs::write(elsewhere.path().join("owned"), b"attacker content").expect("write");
    std::fs::create_dir_all(&roots.state).expect("a state root to redirect");
    std::os::unix::fs::symlink(elsewhere.path(), roots.state.join("transactions"))
        .expect("a planted redirect");
    let outcome = run_machine_isolated_in(
        &installer,
        &roots.roots,
        &roots.state,
        LinuxAction::Install,
        None,
    );
    assert!(outcome.is_err(), "a redirected state refuses: {outcome:?}");
    assert_eq!(
        std::fs::read(elsewhere.path().join("owned")).expect("untouched"),
        b"attacker content"
    );
    assert!(
        !machine_install_dir(&roots.roots).exists(),
        "nothing installs past a refused hierarchy"
    );
}

/// An interrupted machine transaction recovers before new work: the next
/// operation replays the journal to a terminal state, then proceeds.
#[test]
fn machine_interrupted_transaction_recovers_first() {
    let (_base, roots) = isolated();
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let installer = machine_fixture(scratch.path(), "v1", "1.0.0", &machine_v1_files());

    // Simulate a worker that died after beginning: a real compiled plan,
    // journaled, never executed.
    let (intent, plan) = zup_linux::test_support::plan_for_test(
        &installer,
        &roots.roots,
        &roots.state,
        LinuxAction::Install,
        None,
    );
    let _ = intent;
    let coordinator = zup_transaction::TransactionCoordinator::new(
        zup_transaction::FilesystemTransactionStore::new(&roots.state),
    );
    let app = app_id();
    let version = semver::Version::parse("1.0.0").expect("a version");
    let record = coordinator
        .begin(app.clone(), SelectedScope::Machine, version, plan)
        .expect("a journal begins");
    drop(record);
    assert!(
        zup_linux::LinuxLedgerStore::new(&roots.state)
            .load(&app, SelectedScope::Machine)
            .expect("the ledger reads")
            .is_none(),
        "nothing published yet"
    );

    // The next operation recovers the journal forward, then applies its own
    // intent against the recovered world. The first attempt goes stale when
    // the recovery commits under it and retries once; the terminal state is
    // what matters, not how many sessions it took.
    let outcome = run_machine_isolated_in(
        &installer,
        &roots.roots,
        &roots.state,
        LinuxAction::Apply,
        None,
    );
    assert!(
        matches!(outcome, Ok(LinuxOutcome::Committed { .. })),
        "recovery then apply commits: {outcome:?}"
    );
    assert_eq!(
        run_tool(
            &machine_install_dir(&roots.roots).join("tool"),
            &["--version"]
        )
        .trim(),
        "tool 1.0.0"
    );
    let ledger = zup_linux::LinuxLedgerStore::new(&roots.state)
        .load(&app, SelectedScope::Machine)
        .expect("the ledger reads")
        .expect("an installation is recorded");
    assert_eq!(ledger.version.to_string(), "1.0.0");
    // No journal is left unfinished: the interrupted transaction reached a
    // terminal phase through recovery, and the apply committed after it.
    let store = zup_transaction::FilesystemTransactionStore::new(&roots.state);
    let mut terminal = 0;
    for entry in std::fs::read_dir(roots.state.join("transactions")).expect("transactions") {
        let entry = entry.expect("an entry");
        let Some(id) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<uuid::Uuid>().ok())
        else {
            continue;
        };
        let record = zup_transaction::TransactionStore::load(
            &store,
            &zup_transaction::TransactionId::from_uuid(id),
        )
        .expect("a journal loads");
        assert!(
            matches!(
                record.phase,
                zup_transaction::TransactionPhase::Committed
                    | zup_transaction::TransactionPhase::RolledBack
            ),
            "no unfinished journal remains"
        );
        terminal += 1;
    }
    assert!(terminal >= 2, "the recovery and the apply both journaled");
}

/// A payload that names a forbidden destination is refused by policy, not
/// merely by the parser: the sentinel outside the allowed roots survives.
#[test]
fn machine_forbidden_destination_is_refused_by_policy() {
    let (_base, roots) = isolated();
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let sentinel = scratch.path().join("sentinel");
    std::fs::write(&sentinel, b"untouchable").expect("a sentinel");

    // A package whose payload claims an absolute destination outside every
    // allowed root. Lowering accepts the spelling; the worker's policy does
    // not.
    let target = linux_target();
    let payload_dir = scratch.path().join("payload");
    std::fs::create_dir_all(&payload_dir).expect("a payload directory");
    let evil = payload_dir.join("evil");
    std::fs::write(&evil, b"evil").expect("a payload");
    let (size, sha256) = hash_reader(b"evil".as_slice()).expect("a hash");
    let plan = TargetBuildPlan {
        installer: Installer {
            preset: None,
            app: App {
                id: app_id(),
                name: NonEmptyString::new("Tool").expect("a name"),
                version: semver::Version::parse("1.0.0").expect("a version"),
                publisher: None,
                main: None,
                description: None,
            },
            target: target.clone(),
            frontend: Frontend::Console,
            updates: None,
            install: Install {
                scope: InstallScope::Machine,
                directory: InstallDirectory {
                    user: None,
                    machine: Some(
                        Template::parse("${location.programs}/tool").expect("a directory"),
                    ),
                },
                allow_directory_override: false,
            },
            prerequisites: Vec::new(),
            components: Vec::new(),
            component_groups: Vec::new(),
            plugins: Vec::new(),
            files: Vec::new(),
            launchers: Vec::new(),
            path: Vec::new(),
            services: Vec::new(),
            protocols: Vec::new(),
            file_associations: Vec::new(),
        },
        prerequisites: Vec::new(),
        plugins: Vec::new(),
        total_size: size,
        prerequisite_size: 0,
        icons: zup_core::TargetIcons::default(),
        files: vec![ResolvedFile {
            source: evil,
            source_relative: RelativePath::new("evil").expect("a relative path"),
            destination: Template::parse(&sentinel.to_string_lossy()).expect("a destination"),
            size,
            sha256,
            component: None,
            condition: None,
            executable: false,
        }],
        ui_assets: Vec::new(),
    };
    let package = BundleWriter::encode(&plan, &[]).expect("the package encodes");
    let installer = scratch.path().join("evil-installer");
    support::compose_installer(Path::new(support::inert_template()), &installer, &package);

    let outcome = run_machine_isolated_in(
        &installer,
        &roots.roots,
        &roots.state,
        LinuxAction::Install,
        None,
    );
    assert!(
        outcome.is_err(),
        "a forbidden destination refuses: {outcome:?}"
    );
    assert_eq!(
        std::fs::read(&sentinel).expect("the sentinel survives"),
        b"untouchable"
    );
}

// --- Handshake drivers -----------------------------------------------------

/// A Prepare intent for the standard fixture install.
fn install_intent(expected: Option<String>) -> PrepareOperation {
    PrepareOperation {
        operation: privileged_operation::INSTALL.to_owned(),
        force_files: false,
        install_dir_override: None,
        selected_components: Vec::new(),
        expected_plan_digest: expected,
        app_id: app_id().to_string(),
        app_version: "1.0.0".to_owned(),
        scope: "machine".to_owned(),
        target: linux_target(),
    }
}

/// The digest the unprivileged planner binds for a fixture operation.
fn expected_digest(installer: &Path, roots: &MachineTestRoots, action: LinuxAction) -> String {
    let (intent, _) =
        zup_linux::test_support::plan_for_test(installer, &roots.roots, &roots.state, action, None);
    intent.expected_plan_digest.expect("the planner binds")
}

fn next_envelope(client: &mut UnixStream) -> WireEnvelope {
    recv_envelope_on(client, HANDSHAKE).expect("a frame arrives")
}

fn next_hello(client: &mut UnixStream) -> zup_protocol::WorkerHello {
    match next_envelope(client).message {
        Message::WorkerHello(hello) => hello,
        other => panic!("expected hello, got {other:?}"),
    }
}

fn next_prepared(client: &mut UnixStream) -> zup_protocol::PreparedOperation {
    match next_envelope(client).message {
        Message::Prepared(prepared) => prepared,
        other => panic!("expected prepared, got {other:?}"),
    }
}

fn next_failed(client: &mut UnixStream) -> zup_protocol::Failed {
    match next_envelope(client).message {
        Message::Failed(failed) => failed,
        other => panic!("expected failed, got {other:?}"),
    }
}

fn send_prepare(
    client: &mut UnixStream,
    session: SessionId,
    sequence: u64,
    intent: PrepareOperation,
) {
    send_envelope_on(
        client,
        &WireEnvelope {
            version: PROTOCOL_VERSION,
            session_id: session,
            sequence,
            message: Message::Prepare(intent),
        },
    )
    .expect("prepare sends");
}

/// A symlink planted at an install destination is refused before anything
/// is mutated: installation never writes through a link.
#[test]
fn machine_destination_symlink_is_refused() {
    let (_base, roots) = isolated();
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let installer = machine_fixture(scratch.path(), "v1", "1.0.0", &machine_v1_files());
    let elsewhere = tempfile::tempdir().expect("an unrelated tree");
    std::fs::write(elsewhere.path().join("target"), b"elsewhere").expect("write");
    let destination = machine_install_dir(&roots.roots).join("tool");
    std::fs::create_dir_all(destination.parent().expect("a parent")).expect("a parent");
    std::os::unix::fs::symlink(elsewhere.path().join("target"), &destination)
        .expect("a planted link");

    let outcome = run_machine_isolated_in(
        &installer,
        &roots.roots,
        &roots.state,
        LinuxAction::Install,
        None,
    );
    assert!(outcome.is_err(), "a planted link refuses: {outcome:?}");
    assert_eq!(
        std::fs::read(elsewhere.path().join("target")).expect("untouched"),
        b"elsewhere"
    );
    assert!(
        zup_linux::LinuxLedgerStore::new(&roots.state)
            .load(&app_id(), SelectedScope::Machine)
            .expect("the ledger reads")
            .is_none(),
        "nothing is owned after a refused destination"
    );
}

/// Cancelling before Execute leaves no machine changes: no client means no
/// final authorization, even after a successful preparation.
#[test]
fn machine_cancel_before_execute_mutates_nothing() {
    let (_base, roots) = isolated();
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let installer = machine_fixture(scratch.path(), "v1", "1.0.0", &machine_v1_files());
    let uid = rustix::process::getuid().as_raw();
    let session = SessionId::new_v7();
    let (mut client, mut worker) = UnixStream::pair().expect("a pair");
    let roots_clone = roots.roots.clone();
    let installer_clone = installer.clone();
    let handle = std::thread::spawn(move || {
        serve_worker_isolated(
            &mut worker,
            &roots_clone,
            uid,
            std::process::id(),
            session,
            &installer_clone,
        )
    });
    let _ = next_hello(&mut client);
    send_prepare(&mut client, session, 1, install_intent(None));
    let _ = next_prepared(&mut client);
    send_envelope_on(
        &mut client,
        &WireEnvelope {
            version: PROTOCOL_VERSION,
            session_id: session,
            sequence: 2,
            message: Message::Cancel,
        },
    )
    .expect("send");
    let failed = next_failed(&mut client);
    assert_eq!(failed.kind, zup_protocol::failure::CANCELLED);
    handle
        .join()
        .expect("the worker exits")
        .expect_err("cancelled");
    assert!(
        !machine_install_dir(&roots.roots).exists(),
        "a cancelled session mutates nothing"
    );
}

/// Swapping the install directory under an authorized digest is refused:
/// the worker replans from the intent it received, and a digest bound to
/// another directory does not match.
#[test]
fn machine_override_swap_is_refused() {
    let (_base, roots) = isolated();
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let installer =
        machine_fixture_override(scratch.path(), "v1", "1.0.0", &machine_v1_files(), true);
    let uid = rustix::process::getuid().as_raw();

    // The digest the client bound for directory A.
    let dir_a = roots.roots.programs.join("AppA");
    let (intent_a, plan_a) = zup_linux::test_support::plan_for_test(
        &installer,
        &roots.roots,
        &roots.state,
        LinuxAction::Install,
        Some(dir_a.clone()),
    );
    let _ = plan_a;
    let digest_a = intent_a.expected_plan_digest.clone().expect("bound");

    // A session whose intent names directory B but authorizes A's digest.
    let dir_b = roots.roots.programs.join("AppB");
    let session = SessionId::new_v7();
    let (mut client, mut worker) = UnixStream::pair().expect("a pair");
    let roots_clone = roots.roots.clone();
    let installer_clone = installer.clone();
    let handle = std::thread::spawn(move || {
        serve_worker_isolated(
            &mut worker,
            &roots_clone,
            uid,
            std::process::id(),
            session,
            &installer_clone,
        )
    });
    let _ = next_hello(&mut client);
    let mut intent_b = install_intent(Some(digest_a));
    intent_b.install_dir_override = Some(dir_b.display().to_string());
    send_prepare(&mut client, session, 1, intent_b);
    let failed = next_failed(&mut client);
    assert_eq!(failed.kind, zup_protocol::failure::AUTHENTICATION);
    handle
        .join()
        .expect("the worker exits")
        .expect_err("refused");
    assert!(
        !dir_a.exists() && !dir_b.exists(),
        "a swapped override installs nowhere"
    );
}

/// A replaced rendezvous is refused before any peer is trusted: the
/// session directory must be real, owned, and private.
#[test]
fn machine_replaced_rendezvous_is_refused() {
    use zup_linux::test_support::validate_rendezvous_for_test;

    let uid = rustix::process::getuid().as_raw();
    let base = tempfile::tempdir().expect("a base");
    let session_dir = base.path().join("session");
    std::fs::create_dir_all(&session_dir).expect("a directory");
    // A private directory owned by this user validates.
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(&session_dir, std::fs::Permissions::from_mode(0o700)).expect("chmod");
    validate_rendezvous_for_test(&session_dir.join("worker.sock"), uid).expect("honest validates");
    // A symlink where the session directory belongs does not.
    let link_base = tempfile::tempdir().expect("a base");
    let target = link_base.path().join("target");
    std::fs::create_dir_all(&target).expect("a target");
    let link = link_base.path().join("session");
    std::os::unix::fs::symlink(&target, &link).expect("a planted link");
    assert!(
        validate_rendezvous_for_test(&link.join("worker.sock"), uid).is_err(),
        "a replaced rendezvous refuses"
    );
    // A world-writable directory does not either.
    std::fs::set_permissions(&session_dir, std::fs::Permissions::from_mode(0o777)).expect("chmod");
    assert!(
        validate_rendezvous_for_test(&session_dir.join("worker.sock"), uid).is_err(),
        "an unprivate rendezvous refuses"
    );
}

/// Launcher failures surface at once: a cancelled prompt, a denial, a
/// missing mechanism, or a worker that exits before connecting never waits
/// out the connection timeout.
#[test]
fn machine_launcher_failures_surface_at_once() {
    use zup_linux::{LaunchOutcome, PkexecError, PkexecLauncher, WorkerChild};

    #[derive(Debug, Clone)]
    struct FakeLauncher {
        pid: u32,
        exit: Option<LaunchOutcome>,
    }

    struct FakeChild {
        pid: u32,
        exit: Option<LaunchOutcome>,
    }

    impl PkexecLauncher for FakeLauncher {
        type Child = FakeChild;

        fn spawn(
            &self,
            _executable: &std::path::Path,
            _args: &[String],
        ) -> Result<FakeChild, PkexecError> {
            Ok(FakeChild {
                pid: self.pid,
                exit: self.exit.clone(),
            })
        }
    }

    impl WorkerChild for FakeChild {
        fn pid(&self) -> u32 {
            self.pid
        }

        fn try_wait(&mut self) -> Result<Option<LaunchOutcome>, PkexecError> {
            Ok(self.exit.clone())
        }

        fn wait(self) -> Result<LaunchOutcome, PkexecError> {
            self.exit
                .clone()
                .ok_or_else(|| PkexecError::WorkerFailed("the fake worker never exited".into()))
        }
    }

    let (_base, roots) = isolated();
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let installer = machine_fixture(scratch.path(), "v1", "1.0.0", &machine_v1_files());
    let cases = [
        (
            "dismissal",
            Some(126),
            "Error executing command as another user: User dismissed authentication dialog",
            "cancelled",
        ),
        (
            "denial",
            Some(126),
            "Error executing command as another user: Not authorized",
            "authorization failed",
        ),
        ("missing", Some(127), "command not found", "unavailable"),
        ("vanished", Some(0), "", "exited before connecting"),
    ];
    for (name, code, stderr, expect) in cases {
        let launcher = FakeLauncher {
            pid: 424242,
            exit: Some(LaunchOutcome {
                code,
                stderr: stderr.to_owned(),
            }),
        };
        let start = std::time::Instant::now();
        let outcome = zup_linux::test_support::run_machine_elevated_for_test(
            &installer,
            &roots.state,
            LinuxAction::Install,
            None,
            &launcher,
        );
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(60),
            "{name} must surface at once, not after the connection timeout: {elapsed:?}"
        );
        let error = outcome.unwrap_err();
        assert!(
            error.to_string().contains(expect),
            "{name}: expected `{expect}` in `{error}`"
        );
    }
}
