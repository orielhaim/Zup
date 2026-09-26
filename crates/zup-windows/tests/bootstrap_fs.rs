#![cfg(windows)]

//! The Windows bootstrap filesystem adapter: publication through the Windows
//! durable primitives, and reparse-point refusal for quarantine and bootstrap
//! state.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use semver::Version;
use tempfile::TempDir;
use zup_bootstrap::{
    BootstrapFileSystem, BootstrapKey, BootstrapPlan, BootstrapState, BootstrapStateStore,
    BootstrapStoreError, FilesystemBootstrapStateStore, Quarantine, QuarantineError,
};
use zup_core::{AppId, PrerequisiteId, SelectedScope, Sha256Digest, TargetTriple, hash_reader};
use zup_windows::{WindowsBootstrapFileSystem, windows_bootstrap_file_system};

const PAYLOAD: &[u8] = b"runtime";

fn digest(bytes: &[u8]) -> Sha256Digest {
    hash_reader(bytes).unwrap().1
}

fn bootstrap_state() -> BootstrapState {
    BootstrapState::new(
        &BootstrapPlan::new(
            BootstrapKey {
                app_id: AppId::new("com.example.bootstrap").unwrap(),
                app_version: Version::new(1, 0, 0),
                scope: SelectedScope::User,
                target: TargetTriple::parse("x86_64-pc-windows-msvc").unwrap(),
            },
            Vec::new(),
        )
        .unwrap(),
    )
}

fn runtime_id() -> PrerequisiteId {
    PrerequisiteId::new("runtime").unwrap()
}

/// A directory junction: a reparse point that, unlike a symlink, needs no
/// elevated privilege to create, so the adapter's reparse check is exercised on
/// a stock host. Dropping it deletes the reparse point, never its target.
struct Junction {
    path: PathBuf,
}

impl Junction {
    fn new(link: &Path, target: &Path) -> Self {
        fs::create_dir_all(target).unwrap();
        let status = Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .stdout(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "mklink /J failed for {}", link.display());
        Self {
            path: link.to_path_buf(),
        }
    }
}

impl Drop for Junction {
    fn drop(&mut self) {
        let _ = fs::remove_dir(&self.path);
    }
}

#[derive(Default)]
struct Observed {
    publishes: Mutex<Vec<(PathBuf, PathBuf)>>,
    link_checks: Mutex<usize>,
}

impl Observed {
    fn publishes(&self) -> Vec<(PathBuf, PathBuf)> {
        self.publishes.lock().unwrap().clone()
    }

    fn link_checks(&self) -> usize {
        *self.link_checks.lock().unwrap()
    }
}

/// Wraps the Windows adapter so a test can see that the injected adapter, not
/// `std::fs`, is what quarantine and the state store call.
struct ObservedWindowsFileSystem {
    observed: Arc<Observed>,
}

impl ObservedWindowsFileSystem {
    fn observed() -> (Arc<Observed>, Arc<dyn BootstrapFileSystem>) {
        let observed = Arc::new(Observed::default());
        let adapter = Self {
            observed: observed.clone(),
        };
        (observed, Arc::new(adapter))
    }
}

impl BootstrapFileSystem for ObservedWindowsFileSystem {
    fn publish_replace(&self, from: &Path, to: &Path) -> Result<(), io::Error> {
        self.observed
            .publishes
            .lock()
            .unwrap()
            .push((from.to_path_buf(), to.to_path_buf()));
        WindowsBootstrapFileSystem.publish_replace(from, to)
    }

    fn is_link(&self, path: &Path) -> Result<bool, io::Error> {
        *self.observed.link_checks.lock().unwrap() += 1;
        WindowsBootstrapFileSystem.is_link(path)
    }
}

#[test]
fn windows_adapter_reports_reparse_points_and_plain_entries() {
    let root = TempDir::new().unwrap();
    let regular = root.path().join("regular.exe");
    fs::write(&regular, PAYLOAD).unwrap();
    let _junction = Junction::new(&root.path().join("linked"), &root.path().join("elsewhere"));
    let adapter = WindowsBootstrapFileSystem::new();

    assert!(!adapter.is_link(&regular).unwrap());
    assert!(
        !adapter.is_link(&root.path().join("missing.exe")).unwrap(),
        "a path that does not exist is not a link"
    );
    assert!(
        adapter.is_link(&root.path().join("linked")).unwrap(),
        "a directory junction carries FILE_ATTRIBUTE_REPARSE_POINT"
    );
}

#[test]
fn windows_adapter_publishes_and_reports_failure() {
    let root = TempDir::new().unwrap();
    let adapter = WindowsBootstrapFileSystem::new();
    let published = root.path().join("published.exe");
    let staged = root.path().join("staged.exe");
    fs::write(&published, b"old").unwrap();
    fs::write(&staged, PAYLOAD).unwrap();

    adapter.publish_replace(&staged, &published).unwrap();

    assert_eq!(fs::read(&published).unwrap(), PAYLOAD);
    assert!(!staged.exists(), "publication consumes the staged file");

    let absent = root.path().join("absent.partial");
    let destination = root.path().join("never-created.exe");
    assert!(
        adapter.publish_replace(&absent, &destination).is_err(),
        "a failed publication must surface instead of leaving a partial result"
    );
    assert!(!destination.exists());
}

#[test]
fn quarantine_publishes_durably_through_the_windows_adapter() {
    let root = TempDir::new().unwrap();
    let (observed, adapter) = ObservedWindowsFileSystem::observed();
    let quarantine = Quarantine::with_file_system(root.path(), adapter).unwrap();
    let reservation = quarantine
        .reserve(&runtime_id(), "runtime.exe", Some(PAYLOAD.len() as u64))
        .unwrap();

    let artifact = quarantine
        .stage_bytes(&reservation, PAYLOAD, digest(PAYLOAD))
        .unwrap();

    assert_eq!(
        observed.publishes(),
        vec![(
            reservation.partial_path.clone(),
            reservation.final_path.clone()
        )],
        "quarantine must publish through the injected Windows adapter"
    );
    assert_eq!(fs::read(&reservation.final_path).unwrap(), PAYLOAD);
    assert!(
        !reservation.partial_path.exists(),
        "a durable publish leaves no staging file behind"
    );
    assert!(observed.link_checks() > 0);
    quarantine.verify(&artifact).unwrap();
}

#[test]
fn quarantine_refuses_a_reparse_point_inside_its_root() {
    let root = TempDir::new().unwrap();
    let quarantine =
        Quarantine::with_file_system(root.path(), windows_bootstrap_file_system()).unwrap();
    let id = runtime_id();
    let outside = root.path().join("outside");
    let _junction = Junction::new(&root.path().join(id.as_str()), &outside);

    let error = match quarantine.reserve(&id, "runtime.exe", Some(PAYLOAD.len() as u64)) {
        Ok(_) => panic!("a reparse point in the quarantine root must be refused"),
        Err(error) => error,
    };

    assert!(matches!(error, QuarantineError::OutsideRoot));
    assert_eq!(
        fs::read_dir(&outside).unwrap().count(),
        0,
        "nothing may be written through a reparse point"
    );
}

#[test]
fn state_store_publishes_durably_through_the_windows_adapter() {
    let root = TempDir::new().unwrap();
    let (observed, adapter) = ObservedWindowsFileSystem::observed();
    let store = FilesystemBootstrapStateStore::with_file_system(root.path(), adapter);
    let state = bootstrap_state();

    store.create(&state).unwrap();
    assert_eq!(store.load(state.id).unwrap(), state);
    let mut updated = store.load(state.id).unwrap();
    updated.revision = 1;
    store.compare_and_swap(0, &updated).unwrap();
    assert_eq!(store.load(state.id).unwrap().revision, 1);
    assert!(matches!(
        store.compare_and_swap(0, &updated),
        Err(BootstrapStoreError::RevisionConflict)
    ));

    assert_eq!(
        observed.publishes().len(),
        2,
        "create and swap each publish once"
    );
    for (from, to) in observed.publishes() {
        assert!(!from.exists(), "each publish consumes its staging file");
        assert!(to.is_file());
        assert!(!WindowsBootstrapFileSystem::new().is_link(&to).unwrap());
    }
}

#[test]
fn state_store_refuses_a_reparse_point_root() {
    let root = TempDir::new().unwrap();
    let outside = root.path().join("outside");
    let link = root.path().join("state");
    let _junction = Junction::new(&link, &outside);
    let store =
        FilesystemBootstrapStateStore::with_file_system(&link, windows_bootstrap_file_system());
    let state = bootstrap_state();

    assert!(matches!(
        store.create(&state),
        Err(BootstrapStoreError::Invalid)
    ));
    assert_eq!(
        fs::read_dir(&outside).unwrap().count(),
        0,
        "no bootstrap state may be written through a reparse point"
    );
}
