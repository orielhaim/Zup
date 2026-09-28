//! The bootstrap filesystem seam: the portable default stands alone, and
//! injected adapters are what quarantine and the state store actually consult
//! for publication and link policy.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tempfile::TempDir;
use zup_bootstrap::{
    BootstrapFileSystem, BootstrapState, BootstrapStateStore, BootstrapStoreError,
    FilesystemBootstrapStateStore, PortableBootstrapFileSystem, Quarantine, QuarantineError,
};
use zup_core::PrerequisiteId;

mod common;
use common::{digest, embedded, plan};

#[derive(Debug, PartialEq, Eq)]
enum Call {
    Publish { from: PathBuf, to: PathBuf },
    IsLink { path: PathBuf, answer: bool },
}

/// Records every seam call and can be told to fail, so a test can prove which
/// adapter did the work and what happens when it refuses.
struct RecordingFileSystem {
    calls: Arc<Mutex<Vec<Call>>>,
    publish_failure: Option<String>,
    link_everything: bool,
}

impl RecordingFileSystem {
    fn new() -> Self {
        Self {
            calls: Arc::new(Mutex::new(Vec::new())),
            publish_failure: None,
            link_everything: false,
        }
    }

    fn refusing_publish(message: &str) -> Self {
        Self {
            publish_failure: Some(message.to_owned()),
            ..Self::new()
        }
    }

    fn refusing_links() -> Self {
        Self {
            link_everything: true,
            ..Self::new()
        }
    }

    fn publishes(&self) -> Vec<(PathBuf, PathBuf)> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter_map(|call| match call {
                Call::Publish { from, to } => Some((from.clone(), to.clone())),
                Call::IsLink { .. } => None,
            })
            .collect()
    }
}

impl BootstrapFileSystem for RecordingFileSystem {
    fn publish_replace(&self, from: &Path, to: &Path) -> Result<(), io::Error> {
        self.calls.lock().unwrap().push(Call::Publish {
            from: from.to_path_buf(),
            to: to.to_path_buf(),
        });
        match &self.publish_failure {
            Some(message) => Err(io::Error::other(message.clone())),
            None => PortableBootstrapFileSystem.publish_replace(from, to),
        }
    }

    fn is_link(&self, path: &Path) -> Result<bool, io::Error> {
        let detected = PortableBootstrapFileSystem.is_link(path)?;
        // `refusing_links` claims every existing path is a link, which is what a
        // host with a stricter link policy reports.
        let answer = detected || (self.link_everything && path.symlink_metadata().is_ok());
        self.calls.lock().unwrap().push(Call::IsLink {
            path: path.to_path_buf(),
            answer,
        });
        Ok(answer)
    }
}

fn runtime_id() -> PrerequisiteId {
    PrerequisiteId::new("runtime").unwrap()
}

/// `Quarantine` holds a trait object, so it is not `Debug`; match the error
/// instead of `unwrap_err` when the success type is not printable.
fn quarantine_root_error(result: Result<Quarantine, QuarantineError>) -> QuarantineError {
    match result {
        Ok(_) => panic!("a link at the quarantine root must be refused"),
        Err(error) => error,
    }
}

#[test]
fn portable_adapter_publishes_by_replacing_and_never_leaks_a_partial() {
    let root = TempDir::new().unwrap();
    let quarantine = Quarantine::new(root.path()).unwrap();
    let id = runtime_id();

    let first = quarantine.reserve(&id, "runtime.exe", Some(7)).unwrap();
    let artifact = quarantine
        .stage_bytes(&first, b"runtime", digest(b"runtime"))
        .unwrap();
    let published = quarantine.resolve(&artifact.relative_path).unwrap();
    assert_eq!(std::fs::read(&published).unwrap(), b"runtime");
    assert!(
        !first.partial_path.exists(),
        "partial must not survive publication"
    );
    quarantine.verify(&artifact).unwrap();

    // Publication replaces: a second, different artifact lands at the same path.
    let second = quarantine.reserve(&id, "runtime.exe", Some(6)).unwrap();
    assert_eq!(second.final_path, first.final_path);
    let replaced = quarantine
        .stage_bytes(&second, b"second", digest(b"second"))
        .unwrap();
    assert_eq!(
        std::fs::read(&second.final_path).unwrap(),
        b"second",
        "the published artifact must be the staged bytes, not the first one"
    );
    assert!(!second.partial_path.exists());
    assert!(matches!(
        quarantine.verify(&artifact),
        Err(QuarantineError::DigestMismatch)
    ));
    quarantine.verify(&replaced).unwrap();
}

/// Publication goes through the injected adapter and consults it about links first. The
/// adapter's refusal has to reach the caller as the adapter's own error, attributed to
/// the path it refused, and the artifact must not be left published.
#[test]
fn injected_adapter_failure_replaces_quarantine_publication() {
    let root = TempDir::new().unwrap();
    let file_system = Arc::new(RecordingFileSystem::refusing_publish(
        "seam refused publication",
    ));
    let calls = file_system.calls.clone();
    let quarantine = Quarantine::with_file_system(root.path(), file_system.clone()).unwrap();
    let reservation = quarantine
        .reserve(&runtime_id(), "runtime.exe", Some(7))
        .unwrap();

    let error = quarantine
        .stage_bytes(&reservation, b"runtime", digest(b"runtime"))
        .unwrap_err();

    assert!(
        matches!(&error, QuarantineError::Io { path, source }
            if *path == reservation.final_path
                && source.to_string().contains("seam refused publication")),
        "the adapter's failure must reach the caller, got {error:?}"
    );
    assert!(!reservation.final_path.exists());
    assert!(
        calls.lock().unwrap().iter().any(|call| matches!(
            call,
            Call::IsLink { path, .. } if *path == reservation.partial_path
        )),
        "quarantine must ask the adapter about links before it publishes anything"
    );
}

#[test]
fn injected_link_policy_guards_quarantine_root() {
    let root = TempDir::new().unwrap();
    let error = quarantine_root_error(Quarantine::with_file_system(
        root.path(),
        Arc::new(RecordingFileSystem::refusing_links()),
    ));

    assert!(
        matches!(error, QuarantineError::OutsideRoot),
        "a link at the quarantine root must be refused, got {error:?}"
    );
}

/// Every state write goes through the injected adapter, from a temporary sibling that the
/// rename consumes, and the compare-and-swap refuses a revision that is not the one the
/// caller read. The failure-injection sibling proves the adapter is actually consulted;
/// this proves what it is asked to do when it answers.
#[test]
fn state_publication_and_cas_go_through_the_injected_adapter() {
    let root = TempDir::new().unwrap();
    let file_system = Arc::new(RecordingFileSystem::new());
    let store = FilesystemBootstrapStateStore::with_file_system(root.path(), file_system.clone());
    let state = BootstrapState::new(&plan(embedded(b"runtime")));

    store.create(&state).unwrap();
    let mut updated = store.load(state.id).unwrap();
    updated.revision = 1;
    store.compare_and_swap(0, &updated).unwrap();

    let publishes = file_system.publishes();
    assert_eq!(publishes.len(), 2, "create and swap each publish once");
    for (from, to) in &publishes {
        assert_eq!(
            to,
            &store
                .root()
                .join("bootstrap")
                .join(state.id.as_uuid().to_string())
                .join("state.json")
        );
        assert!(
            from.parent() == to.parent() && !from.exists(),
            "state must be published from a temporary sibling that is consumed by the rename"
        );
    }
    assert!(matches!(
        store.compare_and_swap(0, &updated),
        Err(BootstrapStoreError::RevisionConflict)
    ));
}

#[test]
fn injected_adapter_failure_replaces_state_publication() {
    let root = TempDir::new().unwrap();
    let store = FilesystemBootstrapStateStore::with_file_system(
        root.path(),
        Arc::new(RecordingFileSystem::refusing_publish(
            "seam refused publication",
        )),
    );
    let state = BootstrapState::new(&plan(embedded(b"runtime")));

    let error = store.create(&state).unwrap_err();

    assert!(
        matches!(&error, BootstrapStoreError::Io { source, .. }
            if source.to_string().contains("seam refused publication")),
        "the adapter's failure must reach the caller, got {error:?}"
    );
    assert!(matches!(
        store.load(state.id),
        Err(BootstrapStoreError::Missing)
    ));
}

#[test]
fn injected_link_policy_guards_state_before_any_write() {
    let root = TempDir::new().unwrap();
    let store = FilesystemBootstrapStateStore::with_file_system(
        root.path(),
        Arc::new(RecordingFileSystem::refusing_links()),
    );
    let state = BootstrapState::new(&plan(embedded(b"runtime")));

    assert!(matches!(
        store.create(&state),
        Err(BootstrapStoreError::Invalid)
    ));
    assert!(
        !store.root().join("bootstrap").exists(),
        "a refused link must stop the store before it creates anything"
    );
}
