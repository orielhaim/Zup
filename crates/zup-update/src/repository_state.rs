use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, Read};
use std::sync::{Arc, Mutex};

use private_fs::{PrivateDir, Publication};
use serde::{Deserialize, Serialize};
use tough::{MetadataChange, MetadataStore};
use zup_core::{AppId, Sha256Digest};

use crate::TrustContext;

pub const STATE_FILE: &str = "repository.json";
pub const MAX_STATE_BYTES: u64 = crate::MAX_DOCUMENT_BYTES;

/// A protected record capability retaining its exclusive repository lease.
pub trait RepositoryStateFile: Send + Sync {
    fn read(&self) -> io::Result<Option<Vec<u8>>>;
    fn publish(&self, bytes: &[u8]) -> io::Result<()>;
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    schema: u32,
    app_id: AppId,
    trust_anchor: Sha256Digest,
    revision: u64,
    metadata: BTreeMap<String, String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    record: Record,
    fingerprint: Sha256Digest,
}

impl Record {
    fn encode(&self) -> io::Result<Vec<u8>> {
        let fingerprint = self.fingerprint()?;
        let bytes = serde_json::to_vec(&Envelope {
            record: self.clone(),
            fingerprint,
        })?;
        if bytes.len() as u64 > MAX_STATE_BYTES {
            return Err(invalid("repository trust state exceeds its storage limit"));
        }
        Ok(bytes)
    }

    fn fingerprint(&self) -> io::Result<Sha256Digest> {
        use sha2::{Digest, Sha256};
        let mut hash = Sha256::new();
        hash.update(b"zup:repository-state:1\0");
        hash.update(serde_json::to_vec(self)?);
        Ok(Sha256Digest::from_hasher(hash))
    }

    fn decode(bytes: &[u8], expected: &Record) -> io::Result<Self> {
        if bytes.len() as u64 > MAX_STATE_BYTES {
            return Err(invalid("repository trust state exceeds its storage limit"));
        }
        let envelope: Envelope = serde_json::from_slice(bytes)?;
        let record = envelope.record;
        if record.schema != 1
            || record.app_id != expected.app_id
            || record.trust_anchor != expected.trust_anchor
            || record.fingerprint()? != envelope.fingerprint
        {
            return Err(invalid(
                "repository trust state identity or integrity differs",
            ));
        }
        Ok(record)
    }
}

enum State {
    Committed(Record),
    Uncertain,
}

struct RepositoryState<F> {
    file: F,
    state: Mutex<State>,
}

impl<F> std::fmt::Debug for RepositoryState<F> {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        output
            .debug_struct("RepositoryState")
            .finish_non_exhaustive()
    }
}

pub fn open<F: RepositoryStateFile + 'static>(
    context: &TrustContext,
    file: F,
) -> io::Result<Arc<dyn MetadataStore>> {
    let expected = Record {
        schema: 1,
        app_id: context.trust.app_id.clone(),
        trust_anchor: context.trust.trust_anchor().map_err(invalid)?,
        revision: 0,
        metadata: BTreeMap::new(),
    };
    let record = match file.read()? {
        Some(bytes) => Record::decode(&bytes, &expected)?,
        None => expected,
    };
    Ok(Arc::new(RepositoryState {
        file,
        state: Mutex::new(State::Committed(record)),
    }))
}

impl<F: RepositoryStateFile> MetadataStore for RepositoryState<F> {
    fn bytes(&self, name: &str) -> io::Result<Option<Vec<u8>>> {
        let state = self.state.lock().map_err(|_| uncertain())?;
        let State::Committed(record) = &*state else {
            return Err(uncertain());
        };
        record
            .metadata
            .get(name)
            .map(|bytes| zup_core::base64_decode(bytes).map_err(invalid))
            .transpose()
    }

    fn apply(&self, changes: &[MetadataChange]) -> io::Result<()> {
        let mut state = self.state.lock().map_err(|_| uncertain())?;
        let State::Committed(current) = &*state else {
            return Err(uncertain());
        };
        match self.file.read()? {
            Some(bytes) if Record::decode(&bytes, current)? == *current => {}
            None if current.revision == 0 && current.metadata.is_empty() => {}
            _ => {
                return Err(invalid(
                    "repository trust state changed while its lease was held",
                ));
            }
        }
        let mut next = current.clone();
        for change in changes {
            match change {
                MetadataChange::Write { name, bytes } => {
                    next.metadata
                        .insert(name.clone(), zup_core::base64_encode(bytes));
                }
                MetadataChange::Remove { name } => {
                    next.metadata.remove(name);
                }
            }
        }
        if next.metadata == current.metadata {
            return Ok(());
        }
        next.revision = next
            .revision
            .checked_add(1)
            .ok_or_else(|| invalid("repository trust revision exhausted"))?;
        let bytes = next.encode()?;
        *state = State::Uncertain;
        self.file.publish(&bytes)?;
        let found = self.file.read()?.ok_or_else(uncertain)?;
        if Record::decode(&found, &next)? != next {
            return Err(uncertain());
        }
        *state = State::Committed(next);
        Ok(())
    }
}

struct PrivateStateFile {
    directory: PrivateDir,
    _lease: File,
}

pub(crate) fn open_private(context: &TrustContext) -> io::Result<Arc<dyn MetadataStore>> {
    let path = context.datastore_dir().map_err(io::Error::other)?;
    let directory = zup_storage::ensure_private_dir(&path).map_err(io::Error::other)?;
    let lease = zup_storage::open_lock(&directory, "repository.lock").map_err(io::Error::other)?;
    match lease.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "another update is using this repository trust state",
            ));
        }
        Err(std::fs::TryLockError::Error(error)) => return Err(error),
    }
    open(
        context,
        PrivateStateFile {
            directory,
            _lease: lease,
        },
    )
}

impl RepositoryStateFile for PrivateStateFile {
    fn read(&self) -> io::Result<Option<Vec<u8>>> {
        let mut file = match self.directory.open_file(STATE_FILE) {
            Ok(file) => file,
            Err(private_fs::Error::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(None);
            }
            Err(error) => return Err(io::Error::other(error)),
        };
        let mut bytes = Vec::new();
        (&mut file)
            .take(MAX_STATE_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_STATE_BYTES {
            return Err(invalid("repository trust state exceeds its storage limit"));
        }
        file.verify().map_err(io::Error::other)?;
        self.directory.sync_all().map_err(io::Error::other)?;
        Ok(Some(bytes))
    }

    fn publish(&self, bytes: &[u8]) -> io::Result<()> {
        zup_storage::publish_full(&self.directory, STATE_FILE, bytes, Publication::Replace)
            .map_err(io::Error::other)
    }
}

fn invalid(message: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_string())
}

fn uncertain() -> io::Error {
    io::Error::other("repository publication requires a fresh observation before continuing")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn context(path: &std::path::Path) -> TrustContext {
        TrustContext::from_update_config(
            &zup_core::UpdateConfig {
                repository: "https://example.test/releases".into(),
                channel: "stable".into(),
                trusted_root: b"fixture trust anchor".to_vec(),
            },
            AppId::new("com.example.repository-state").unwrap(),
            zup_core::InstallScope::User,
            path,
        )
    }

    struct InterruptedFile {
        inner: PrivateStateFile,
        armed: Arc<AtomicBool>,
        after: bool,
    }

    impl RepositoryStateFile for InterruptedFile {
        fn read(&self) -> io::Result<Option<Vec<u8>>> {
            self.inner.read()
        }
        fn publish(&self, bytes: &[u8]) -> io::Result<()> {
            let fail = self.armed.swap(false, Ordering::SeqCst);
            if fail && !self.after {
                return Err(io::Error::other("interrupted before publication"));
            }
            self.inner.publish(bytes)?;
            if fail {
                return Err(io::Error::other("completion receipt unavailable"));
            }
            Ok(())
        }
    }

    fn write(name: &str, bytes: &[u8]) -> MetadataChange {
        MetadataChange::Write {
            name: name.into(),
            bytes: bytes.to_vec(),
        }
    }

    #[rstest::rstest]
    #[case(false)]
    #[case(true)]
    fn interrupted_batch_is_reobserved_before_further_metadata_can_be_used(#[case] after: bool) {
        let root = tempfile::tempdir().unwrap();
        let context = context(root.path());
        let directory = zup_storage::ensure_private_dir(&context.datastore_dir().unwrap()).unwrap();
        let lease = zup_storage::open_lock(&directory, "repository.lock").unwrap();
        lease.try_lock().unwrap();
        let armed = Arc::new(AtomicBool::new(false));
        let store = open(
            &context,
            InterruptedFile {
                inner: PrivateStateFile {
                    directory,
                    _lease: lease,
                },
                armed: armed.clone(),
                after,
            },
        )
        .unwrap();
        store
            .apply(&[
                write("root.json", b"root one"),
                write("timestamp.json", b"timestamp five"),
                write("snapshot.json", b"snapshot five"),
            ])
            .unwrap();
        armed.store(true, Ordering::SeqCst);
        assert!(
            store
                .apply(&[
                    write("root.json", b"root two"),
                    MetadataChange::Remove {
                        name: "timestamp.json".into()
                    },
                    MetadataChange::Remove {
                        name: "snapshot.json".into()
                    }
                ])
                .is_err()
        );
        assert!(store.bytes("root.json").is_err());
        assert!(
            store
                .apply(&[write("targets.json", b"unverified continuation")])
                .is_err()
        );
        drop(store);
        let recovered = open_private(&context).unwrap();
        assert_eq!(
            recovered.bytes("root.json").unwrap().unwrap(),
            if after { b"root two" } else { b"root one" }
        );
        assert_eq!(
            recovered.bytes("timestamp.json").unwrap(),
            if after {
                None
            } else {
                Some(b"timestamp five".to_vec())
            }
        );
        assert_eq!(
            recovered.bytes("snapshot.json").unwrap(),
            if after {
                None
            } else {
                Some(b"snapshot five".to_vec())
            }
        );
    }

    #[test]
    fn repository_lease_excludes_other_processes_until_the_last_reader_drops() {
        const CHILD: &str = "ZUP_REPOSITORY_LEASE_CHILD";
        if let Some(path) = std::env::var_os(CHILD) {
            let error = open_private(&context(std::path::Path::new(&path))).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let context = context(root.path());
        let store = open_private(&context).unwrap();
        let reader = Arc::clone(&store);
        drop(store);
        let test = std::thread::current().name().unwrap().to_owned();
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &test])
            .env(CHILD, root.path())
            .output()
            .unwrap();
        assert!(
            child.status.success(),
            "{}",
            String::from_utf8_lossy(&child.stderr)
        );
        drop(reader);
        assert!(open_private(&context).is_ok());
    }
    #[tokio::test]
    async fn corrupt_clock_evidence_refuses_refresh_before_transport() {
        let root = tempfile::tempdir().unwrap();
        let context = context(root.path());
        let store = open_private(&context).unwrap();
        store
            .apply(&[write("latest_known_time.json", b"invalid clock evidence")])
            .unwrap();
        let resolver = crate::ReleaseResolver::new(
            context,
            zup_acquire::HostProfile::native(),
            zup_acquire::CachePolicy::Auto,
            zup_acquire::ProgressSink::discard(),
        )
        .unwrap()
        .with_metadata_store(store);
        let error = resolver.resolve().await.unwrap_err();
        assert!(matches!(error, crate::UpdateError::Tuf(error)
            if matches!(*error, tough::error::Error::DatastoreDeserialize { .. })));
    }
    #[tokio::test]
    async fn verified_key_rotation_retires_prior_metadata_floors_in_the_native_store() {
        let root = tempfile::tempdir().unwrap();
        let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../third-party/tough/tests/data/rotated-root");
        let initial = std::fs::read(fixtures.join("1.root.json")).unwrap();
        let mut context = context(root.path());
        context.trust.trusted_root = zup_core::base64_encode(&initial);
        let store = open_private(&context).unwrap();
        let inflated = |name: &str| {
            let mut metadata: serde_json::Value = serde_json::from_slice(
                &std::fs::read(fixtures.join(if name == "snapshot.json" {
                    "1.snapshot.json"
                } else {
                    name
                }))
                .unwrap(),
            )
            .unwrap();
            metadata["signed"]["version"] = serde_json::json!(999999);
            serde_json::to_vec(&metadata).unwrap()
        };
        store
            .apply(&[
                write("timestamp.json", &inflated("timestamp.json")),
                write("snapshot.json", &inflated("snapshot.json")),
            ])
            .unwrap();
        let metadata = url::Url::from_directory_path(&fixtures).unwrap();
        let targets = url::Url::from_directory_path(fixtures.join("targets")).unwrap();
        let repository = tough::RepositoryLoader::new(&initial, metadata, targets)
            .metadata_store(store.clone())
            .load()
            .await
            .unwrap();
        assert_eq!(repository.root().signed.version.get(), 2);
        for name in ["timestamp.json", "snapshot.json"] {
            let bytes = store.bytes(name).unwrap().unwrap();
            let metadata: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert!(metadata["signed"]["version"].as_u64().unwrap() < 999999);
        }
    }
    #[derive(Debug)]
    struct PausedStore {
        inner: Arc<dyn MetadataStore>,
        started: std::sync::mpsc::SyncSender<()>,
        release: Mutex<std::sync::mpsc::Receiver<()>>,
    }

    impl MetadataStore for PausedStore {
        fn bytes(&self, name: &str) -> io::Result<Option<Vec<u8>>> {
            self.inner.bytes(name)
        }
        fn apply(&self, changes: &[MetadataChange]) -> io::Result<()> {
            self.started.send(()).map_err(io::Error::other)?;
            self.release
                .lock()
                .unwrap()
                .recv_timeout(std::time::Duration::from_secs(10))
                .map_err(io::Error::other)?;
            self.inner.apply(changes)
        }
    }

    #[tokio::test]
    async fn cancelled_refresh_retains_the_lease_until_started_publication_finishes() {
        let root = tempfile::tempdir().unwrap();
        let context = context(root.path());
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let store = Arc::new(PausedStore {
            inner: open_private(&context).unwrap(),
            started: started_tx,
            release: Mutex::new(release_rx),
        });
        let resolver = crate::ReleaseResolver::new(
            context.clone(),
            zup_acquire::HostProfile::native(),
            zup_acquire::CachePolicy::Auto,
            zup_acquire::ProgressSink::discard(),
        )
        .unwrap()
        .with_metadata_store(store);
        let task = tokio::spawn(async move { resolver.resolve().await });
        tokio::task::spawn_blocking(move || {
            started_rx
                .recv_timeout(std::time::Duration::from_secs(10))
                .unwrap()
        })
        .await
        .unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(
            open_private(&context).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        release_tx.send(()).unwrap();
        let reopened = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                match open_private(&context) {
                    Ok(store) => break store,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        tokio::task::yield_now().await
                    }
                    Err(error) => panic!("{error}"),
                }
            }
        })
        .await
        .unwrap();
        assert!(reopened.bytes("latest_known_time.json").unwrap().is_some());
    }
}
