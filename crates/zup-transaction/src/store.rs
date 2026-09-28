//! Durable transaction store (trait + filesystem implementation).
//!
//! `fs_transaction` is used **only** for journal persistence — never for
//! application payload mutation.
//!
//! # Why this store has no recovery pass
//!
//! `fs_transaction` ships a `recover` entry point and nothing here calls it,
//! which reads like a missing crash-recovery path. It is not one, and the reason
//! is worth stating because it is an obligation rather than an accident.
//!
//! Every apply this store performs is a change set of **one** op. `fs_transaction`
//! documents that a set of one is already indivisible — a `write_atomic` is
//! all-or-nothing by construction, and a lone rename or unlink is atomic by the
//! filesystem's own guarantee — so it skips the journal rather than write, flush
//! and then delete a second file to restate a promise the single op already
//! carries. Nothing is journaled, so there is nothing to recover.
//!
//! That is a constraint, not a freedom. **A change set of several ops would start
//! writing a journal, and this store would then owe a recovery pass it does not
//! perform.** `tests/store.rs::a_store_write_leaves_no_journal_to_recover` asserts
//! both halves — no journal exists after a create and a swap, and `recover`
//! confirms there is nothing to roll forward — so a change set that grows fails
//! there and names the obligation instead of being discovered on a user's machine.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use fs_transaction::exec::block_on;
use fs_transaction::{ChangeSet, Error as FsTxError};
use jiff::Timestamp;

use crate::id::TransactionId;
use crate::journal_fs::JournalFs;
use crate::record::{CorruptReason, StoreError, TransactionRecord};

/// How many times [`TransactionStore::update`] re-reads and re-applies after
/// losing a compare-and-swap.
///
/// Losing one is ordinary contention, not corruption: every actor that journals
/// progress for a transaction reads the record, decides, and writes it back, so
/// two of those windows overlap whenever a coordinator and a recovery pass are
/// live at once. The loser simply started from a revision that is no longer
/// current, and re-reading converges as long as both writers keep making
/// progress. A transaction journals O(plan size) writes with a handful of
/// actors, so a handful of attempts is ample; exhausting the budget means a
/// writer is rewriting the record faster than the other can be scheduled, which
/// is a real stall rather than something to retry forever.
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
    /// `change` is applied to the freshest committed state and the result is
    /// swapped in under the revision it was read at, retrying against re-read
    /// state when another actor won the race (see [`MAX_UPDATE_ATTEMPTS`]).
    /// A lost swap is therefore never a failure, and a caller never has to
    /// reason about a revision it read before someone else wrote.
    ///
    /// `change` must be a function of the state it is handed. It is re-run
    /// against a newer revision on every retry, so a closure that also captures
    /// state read earlier would reintroduce the lost update this path exists to
    /// prevent. It must leave `revision` and `updated_at` alone: the store owns
    /// both.
    fn update(
        &self,
        id: &TransactionId,
        change: &mut dyn FnMut(&mut TransactionRecord) -> Result<(), StoreError>,
    ) -> Result<TransactionRecord, StoreError> {
        for _ in 0..MAX_UPDATE_ATTEMPTS {
            let mut next = self.load(id)?;
            let expected = next.revision;
            change(&mut next)?;
            next.revision = expected.saturating_add(1);
            next.updated_at = Timestamp::now();
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

    /// The state root this store writes under.
    ///
    /// Exposed because a test that wants to read a durable record — or copy the
    /// store to observe one — needs to name the same root, and inventing a second
    /// spelling of it is how a test ends up reading a different directory than the
    /// one under test.
    pub fn root(&self) -> &Path {
        &self.root
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
