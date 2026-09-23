//! Durable transaction store (trait + filesystem implementation).
//!
//! `fs_transaction` is used **only** for journal persistence — never for
//! application payload mutation.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use fs_transaction::exec::block_on;
use fs_transaction::{ChangeSet, Error as FsTxError};

use crate::id::TransactionId;
use crate::journal_fs::JournalFs;
use crate::record::{CorruptReason, StoreError, TransactionRecord};

/// Optimistic-concurrency durable store for transaction records.
pub trait TransactionStore {
    fn create(&self, record: &TransactionRecord) -> Result<(), StoreError>;
    fn load(&self, id: &TransactionId) -> Result<TransactionRecord, StoreError>;
    fn compare_and_swap(
        &self,
        expected_revision: u64,
        updated: &TransactionRecord,
    ) -> Result<(), StoreError>;
}

const JOURNAL_FILE: &str = "transaction.json";
const TRANSACTIONS_DIR: &str = "transactions";
const JOURNAL_LOCK: &str = "transaction.lock";

/// Filesystem-backed store rooted at a caller-supplied state root.
pub struct FilesystemTransactionStore {
    root: PathBuf,
}

impl FilesystemTransactionStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn journal_dir(&self, id: &TransactionId) -> PathBuf {
        self.root
            .join(TRANSACTIONS_DIR)
            .join(id.as_uuid().to_string())
    }

    fn journal_path(&self, id: &TransactionId) -> PathBuf {
        self.journal_dir(id).join(JOURNAL_FILE)
    }

    fn lock(&self, id: &TransactionId) -> Result<File, StoreError> {
        let directory = self.journal_dir(id);
        std::fs::create_dir_all(&directory).map_err(|source| StoreError::Io {
            path: directory.clone(),
            source,
        })?;
        let path = directory.join(JOURNAL_LOCK);
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

    fn journal_rel(id: &TransactionId) -> PathBuf {
        Path::new(TRANSACTIONS_DIR)
            .join(id.as_uuid().to_string())
            .join(JOURNAL_FILE)
    }

    fn read_journal(&self, id: &TransactionId) -> Result<Vec<u8>, StoreError> {
        let path = self.journal_path(id);
        std::fs::read(&path).map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                StoreError::Corrupt(CorruptReason::Missing)
            } else {
                StoreError::Io {
                    path: path.clone(),
                    source: err,
                }
            }
        })
    }

    fn parse(bytes: &[u8]) -> Result<TransactionRecord, StoreError> {
        let record: TransactionRecord = serde_json::from_slice(bytes)
            .map_err(|e| StoreError::Corrupt(CorruptReason::InvalidJson(e.to_string())))?;
        record.validate()?;
        Ok(record)
    }

    fn serialize(record: &TransactionRecord) -> Result<Vec<u8>, StoreError> {
        serde_json::to_vec_pretty(record).map_err(StoreError::Serialize)
    }
}

fn is_drifted(err: &FsTxError) -> bool {
    matches!(err, FsTxError::Drifted(_))
}

impl TransactionStore for FilesystemTransactionStore {
    fn create(&self, record: &TransactionRecord) -> Result<(), StoreError> {
        record.validate()?;
        let id = record.transaction_id;
        let _lock = self.lock(&id)?;
        if self.journal_path(&id).exists() {
            return Err(StoreError::AlreadyExists { id: id.to_string() });
        }
        let bytes = Self::serialize(record)?;
        let rel = Self::journal_rel(&id);
        let mut change = ChangeSet::new();
        change.expect_absent(&rel);
        change.write(&rel, bytes);
        block_on(change.apply(&JournalFs, &self.root)).map_err(|e| {
            if is_drifted(&e) {
                StoreError::AlreadyExists { id: id.to_string() }
            } else {
                StoreError::Persistence(e.to_string())
            }
        })
    }

    fn load(&self, id: &TransactionId) -> Result<TransactionRecord, StoreError> {
        let bytes = self.read_journal(id)?;
        Self::parse(&bytes)
    }

    fn compare_and_swap(
        &self,
        expected_revision: u64,
        updated: &TransactionRecord,
    ) -> Result<(), StoreError> {
        updated.validate()?;
        let id = updated.transaction_id;
        let _lock = self.lock(&id)?;
        if updated.revision != expected_revision.saturating_add(1) {
            return Err(StoreError::Corrupt(CorruptReason::RevisionMismatch {
                expected: expected_revision,
                found: updated.revision,
            }));
        }
        let previous = self.read_journal(&id)?;
        let previous_record = Self::parse(&previous)?;
        if previous_record.revision != expected_revision {
            return Err(StoreError::RevisionConflict {
                expected: expected_revision,
            });
        }

        let bytes = Self::serialize(updated)?;
        let rel = Self::journal_rel(&id);
        let mut change = ChangeSet::new();
        change.expect(&rel, previous);
        change.write(&rel, bytes);
        block_on(change.apply(&JournalFs, &self.root)).map_err(|e| {
            if is_drifted(&e) {
                StoreError::RevisionConflict {
                    expected: expected_revision,
                }
            } else {
                StoreError::Persistence(e.to_string())
            }
        })
    }
}
