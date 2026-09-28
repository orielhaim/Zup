//! Durable transaction store (trait + filesystem implementation).
//!
//! Each transaction is one full-snapshot `transaction.json`, written by
//! [`write_record`] as a temp sibling that is flushed and then renamed over the
//! destination. Readers therefore see the previous complete record or the new
//! complete one, never a partial write. A crash can leave a hidden
//! `.transaction.json.*` temp file behind; nothing reads it.
//!
//! Durability: the record's bytes are flushed before the rename. On Unix the
//! directory is flushed too; on Windows the rename itself is only as durable as
//! NTFS's metadata journal makes it, because a directory cannot be opened for
//! `FlushFileBuffers`.
//!
//! Writers serialize on `transaction.lock`, so the revision check in
//! [`TransactionStore::compare_and_swap`] and the replace that follows it are
//! one step for every cooperating writer.

use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use atomic_write_file::AtomicWriteFile;

use crate::id::TransactionId;
use crate::record::{CorruptReason, StoreError, TransactionRecord};

/// How many times [`TransactionStore::update`] re-reads and re-applies after
/// losing a compare-and-swap.
///
/// `update` reads outside the lock, so a coordinator and a recovery pass that
/// are live at once can both read one revision; the loser re-reads and
/// converges. A transaction journals O(plan size) writes with a handful of
/// actors, so exhausting this budget means a real stall, not contention.
const MAX_UPDATE_ATTEMPTS: u32 = 8;

/// Optimistic-concurrency durable store for transaction records.
pub trait TransactionStore {
    fn create(&self, record: &TransactionRecord) -> Result<(), StoreError>;
    fn load(&self, id: &TransactionId) -> Result<TransactionRecord, StoreError>;
    fn compare_and_swap(
        &self,
        expected_revision: u64,
        updated: &TransactionRecord,
    ) -> Result<(), StoreError>;

    /// Read-modify-write one record as a single durable step, and return what
    /// was committed.
    ///
    /// `change` is applied to the freshest committed state and swapped in under
    /// the revision it was read at, retrying against re-read state when another
    /// actor won the race (see [`MAX_UPDATE_ATTEMPTS`]).
    ///
    /// `change` must be a function of the state it is handed: it is re-run
    /// against a newer revision on every retry. It must leave `revision` and
    /// `updated_at` alone; the store owns both.
    fn update(
        &self,
        id: &TransactionId,
        change: &mut dyn FnMut(&mut TransactionRecord) -> Result<(), StoreError>,
    ) -> Result<TransactionRecord, StoreError> {
        for _ in 0..MAX_UPDATE_ATTEMPTS {
            let mut next = self.load(id)?;
            let expected = next.revision;
            change(&mut next)?;
            next.revision = expected;
            next.touch();
            match self.compare_and_swap(expected, &next) {
                Ok(()) => return Ok(next),
                Err(StoreError::RevisionConflict { .. }) => continue,
                Err(error) => return Err(error),
            }
        }
        Err(StoreError::UpdateExhausted {
            id: id.to_string(),
            attempts: MAX_UPDATE_ATTEMPTS,
        })
    }
}

/// Filesystem-backed store rooted at a caller-supplied state root.
pub struct FilesystemTransactionStore {
    root: PathBuf,
}

impl FilesystemTransactionStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn directory(&self, id: &TransactionId) -> PathBuf {
        self.root.join("transactions").join(id.to_string())
    }

    fn record_path(&self, id: &TransactionId) -> PathBuf {
        self.directory(id).join("transaction.json")
    }

    /// Hold the per-transaction writer lock until the returned file drops.
    fn lock(&self, id: &TransactionId) -> Result<File, StoreError> {
        let directory = self.directory(id);
        std::fs::create_dir_all(&directory).map_err(|source| StoreError::Io {
            path: directory.clone(),
            source,
        })?;
        let path = directory.join("transaction.lock");
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .map_err(|source| StoreError::Io {
                path: path.clone(),
                source,
            })?;
        file.lock()
            .map_err(|source| StoreError::Io { path, source })?;
        Ok(file)
    }
}

fn write_record(path: &Path, record: &TransactionRecord) -> Result<(), StoreError> {
    let bytes = serde_json::to_vec_pretty(record).map_err(StoreError::Serialize)?;
    let io = |source| StoreError::Io {
        path: path.to_path_buf(),
        source,
    };
    let mut file = AtomicWriteFile::open(path).map_err(io)?;
    file.write_all(&bytes).map_err(io)?;
    file.commit().map_err(io)
}

impl TransactionStore for FilesystemTransactionStore {
    fn create(&self, record: &TransactionRecord) -> Result<(), StoreError> {
        record.validate()?;
        let id = record.transaction_id;
        let _lock = self.lock(&id)?;
        let path = self.record_path(&id);
        if path.exists() {
            return Err(StoreError::AlreadyExists { id: id.to_string() });
        }
        write_record(&path, record)
    }

    fn load(&self, id: &TransactionId) -> Result<TransactionRecord, StoreError> {
        let path = self.record_path(id);
        let bytes = std::fs::read(&path).map_err(|source| {
            if source.kind() == std::io::ErrorKind::NotFound {
                StoreError::Corrupt(CorruptReason::Missing)
            } else {
                StoreError::Io { path, source }
            }
        })?;
        let record: TransactionRecord = serde_json::from_slice(&bytes)
            .map_err(|e| CorruptReason::InvalidJson(e.to_string()))?;
        record.validate()?;
        Ok(record)
    }

    fn compare_and_swap(
        &self,
        expected_revision: u64,
        updated: &TransactionRecord,
    ) -> Result<(), StoreError> {
        updated.validate()?;
        if updated.revision != expected_revision.saturating_add(1) {
            return Err(CorruptReason::RevisionMismatch {
                expected: expected_revision,
                found: updated.revision,
            }
            .into());
        }
        let id = updated.transaction_id;
        let _lock = self.lock(&id)?;
        if self.load(&id)?.revision != expected_revision {
            return Err(StoreError::RevisionConflict {
                expected: expected_revision,
            });
        }
        write_record(&self.record_path(&id), updated)
    }
}
