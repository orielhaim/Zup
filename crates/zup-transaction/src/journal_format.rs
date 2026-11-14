use crate::{
    CorruptReason, JournalFiles, NodeState, StoreError, TransactionPhase, TransactionRecord,
    fault_injection::{Boundary, boundary},
    journal_io::Counters,
};
use serde::{Deserialize, Serialize};
use std::io::{Read, Seek, SeekFrom, Write};
use zup_core::{NativeObjectIdentity, Sha256Digest, TransactionId, hash_bytes};
use zup_plan::OperationId;

const FORMAT: u32 = 4;
const MAX_FRAME: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Head {
    format: u32,
    transaction: TransactionId,
    intent: Sha256Digest,
    slot: Slot,
    checkpoint: Option<Sha256Digest>,
    base: u64,
    revision: u64,
    length: u64,
    tail: Sha256Digest,
    log: NativeObjectIdentity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Slot {
    A,
    B,
}

impl Slot {
    fn other(self) -> Self {
        match self {
            Self::A => Self::B,
            Self::B => Self::A,
        }
    }
}

impl Head {
    fn log_name(&self) -> &'static str {
        match self.slot {
            Slot::A => "events-a.log",
            Slot::B => "events-b.log",
        }
    }
    fn checkpoint_name(&self) -> &'static str {
        match self.slot {
            Slot::A => "checkpoint-a.json",
            Slot::B => "checkpoint-b.json",
        }
    }
}

/// An atomic group of typed changes. The revision and timestamp belong to the commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "field",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum TransactionChange {
    Node(OperationId, NodeState),
    Phase(TransactionPhase),
    Reservation(Option<crate::InstallationReservation>),
    Publication(crate::PublicationState),
    Resolution(Option<crate::RecoveryDecision>),
    Failure(Option<crate::TransactionFailure>),
    RollbackFailure(Option<crate::TransactionFailure>),
    RecoveryFailure(Option<crate::TransactionFailure>),
    Reconciliation(crate::ReconciliationState),
    Release(Option<Box<zup_core::ReleaseIdentity>>),
}

impl TransactionChange {
    pub(crate) fn apply(&self, record: &mut TransactionRecord) -> Result<(), StoreError> {
        match self {
            Self::Node(id, state) => {
                let before = record
                    .nodes
                    .get(id)
                    .ok_or(CorruptReason::NodeStateMismatch)?;
                if before != state {
                    before
                        .clone()
                        .transition(state.clone())
                        .map_err(|_| CorruptReason::InvalidPhase)?;
                }
                record.nodes.insert(id.clone(), state.clone());
            }
            Self::Phase(phase) => {
                if record.phase != *phase {
                    record
                        .phase
                        .transition(*phase)
                        .map_err(|_| CorruptReason::InvalidPhase)?;
                }
                record.phase = *phase;
            }
            Self::Reservation(value) => record.reservation = value.clone(),
            Self::Publication(value) => record.publication = *value,
            Self::Resolution(value) => record.resolution = value.clone(),
            Self::Failure(value) => record.failure = value.clone(),
            Self::RollbackFailure(value) => record.rollback_failure = value.clone(),
            Self::RecoveryFailure(value) => record.recovery_failure = value.clone(),
            Self::Reconciliation(value) => record.reconciliation = value.clone(),
            Self::Release(value) => record.release = value.as_deref().cloned(),
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Event {
    revision: u64,
    updated_at: jiff::Timestamp,
    changes: Vec<TransactionChange>,
}

// Checkpoints contain only mutable state. The authenticated intent never moves.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Checkpoint {
    transaction: TransactionId,
    intent: Sha256Digest,
    revision: u64,
    updated_at: jiff::Timestamp,
    state: MutableState,
}

pub(crate) struct Loaded {
    pub head: Head,
    pub record: TransactionRecord,
    pub head_bytes: Vec<u8>,
    pub budget: u64,
    stamps: Vec<(&'static str, Stamp)>,
}

#[derive(PartialEq, Eq)]
struct Stamp {
    object: NativeObjectIdentity,
    size: u64,
    modified: std::time::SystemTime,
}

fn stamp<D: JournalFiles + ?Sized>(directory: &D, name: &str) -> Result<Stamp, StoreError> {
    let file = directory.lock_file(name, private_fs::FileCreation::Existing)?;
    let metadata = file.metadata().map_err(io_error)?;
    Ok(Stamp {
        object: zup_storage::namespace::identity(&file).map_err(io_error)?,
        size: metadata.len(),
        modified: metadata.modified().map_err(io_error)?,
    })
}

fn stamps<D: JournalFiles + ?Sized>(
    directory: &D,
    head: &Head,
) -> Result<Vec<(&'static str, Stamp)>, StoreError> {
    let mut names = vec!["transaction.json", head.log_name()];
    if head.checkpoint.is_some() {
        names.push(head.checkpoint_name());
    }
    names
        .into_iter()
        .map(|name| Ok((name, stamp(directory, name)?)))
        .collect()
}

impl Loaded {
    pub fn unchanged<D: JournalFiles + ?Sized>(&self, directory: &D) -> Result<bool, StoreError> {
        for (name, expected) in &self.stamps {
            if stamp(directory, name)? != *expected {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

fn invalid() -> StoreError {
    CorruptReason::InvalidHistory.into()
}

fn io_error(source: std::io::Error) -> StoreError {
    StoreError::Io {
        path: "transaction-journal".into(),
        source,
    }
}

fn encode(value: &impl Serialize) -> Result<Vec<u8>, StoreError> {
    serde_json::to_vec(value).map_err(StoreError::Serialize)
}

fn decode<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, StoreError> {
    serde_json::from_slice(bytes).map_err(|_| invalid())
}

fn chain(previous: Sha256Digest, bytes: &[u8]) -> Sha256Digest {
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    hash.update(b"zup/journal/event/4\0");
    hash.update(previous.as_bytes());
    hash.update((bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
    Sha256Digest::from_hasher(hash)
}

fn log_file<D: JournalFiles + ?Sized>(
    directory: &D,
    head: &Head,
) -> Result<std::fs::File, StoreError> {
    let file = directory.lock_file(head.log_name(), private_fs::FileCreation::Existing)?;
    if zup_storage::namespace::identity(&file).map_err(io_error)? != head.log {
        return Err(CorruptReason::IdentityMismatch.into());
    }
    Ok(file)
}

fn read<D: JournalFiles + ?Sized>(
    directory: &D,
    name: &str,
    io: &Counters,
) -> Result<Vec<u8>, StoreError> {
    let bytes = directory.read(name)?.ok_or(CorruptReason::Missing)?;
    io.read(bytes.len());
    Ok(bytes)
}

fn publish<D: JournalFiles + ?Sized>(
    directory: &D,
    name: &str,
    bytes: &[u8],
    io: &Counters,
) -> Result<(), StoreError> {
    io.write(bytes.len());
    directory.publish(name, bytes, false)
}

pub(crate) fn initialize<D: JournalFiles + ?Sized>(
    directory: &D,
    intent: &[u8],
    record: &TransactionRecord,
) -> Result<(), StoreError> {
    let io = Counters::default();
    if let Some(bytes) = directory.read("head.json")? {
        let loaded = load(directory, record.transaction_id, &io)?;
        if loaded.record != *record || loaded.head_bytes != bytes {
            return Err(invalid());
        }
        return Ok(());
    }
    if let Some(bytes) = directory.read("events-a.log")? {
        if !bytes.is_empty() {
            return Err(invalid());
        }
    } else {
        directory.publish("events-a.log", &[], true)?;
    }
    boundary(
        Boundary::JournalInitialLogPublished,
        &record.transaction_id.to_string(),
    );
    let log = directory.lock_file("events-a.log", private_fs::FileCreation::Existing)?;
    let head = Head {
        format: FORMAT,
        transaction: record.transaction_id,
        intent: hash_bytes(intent),
        slot: Slot::A,
        checkpoint: None,
        base: record.revision,
        revision: record.revision,
        length: 0,
        tail: hash_bytes(intent),
        log: zup_storage::namespace::identity(&log).map_err(io_error)?,
    };
    directory.publish("head.json", &encode(&head)?, true)?;
    boundary(
        Boundary::JournalInitialHeadPublished,
        &record.transaction_id.to_string(),
    );
    Ok(())
}

pub(crate) fn read_head<D: JournalFiles + ?Sized>(
    directory: &D,
    io: &Counters,
) -> Result<Vec<u8>, StoreError> {
    read(directory, "head.json", io)
}

pub(crate) fn load<D: JournalFiles + ?Sized>(
    directory: &D,
    id: TransactionId,
    io: &Counters,
) -> Result<Loaded, StoreError> {
    let head_bytes = read_head(directory, io)?;
    load_head(directory, id, head_bytes, io)
}

pub(crate) fn load_head<D: JournalFiles + ?Sized>(
    directory: &D,
    id: TransactionId,
    head_bytes: Vec<u8>,
    io: &Counters,
) -> Result<Loaded, StoreError> {
    let head: Head = decode(&head_bytes)?;
    if head.format != FORMAT || head.transaction != id || head.base > head.revision {
        return Err(invalid());
    }
    let intent = read(directory, "transaction.json", io)?;
    if hash_bytes(&intent) != head.intent {
        return Err(invalid());
    }
    let mut record: TransactionRecord = decode(&intent)?;
    record.validate()?;
    if record.transaction_id != id || head.base < record.revision {
        return Err(CorruptReason::IdentityMismatch.into());
    }
    let mut tail = head.intent;
    let mut budget = intent.len() as u64;
    if let Some(expected) = head.checkpoint {
        let bytes = read(directory, head.checkpoint_name(), io)?;
        if hash_bytes(&bytes) != expected {
            return Err(invalid());
        }
        let checkpoint: Checkpoint = decode(&bytes)?;
        if checkpoint.transaction != id
            || checkpoint.intent != head.intent
            || checkpoint.revision != head.base
        {
            return Err(invalid());
        }
        // A checkpoint is a complete state, not a sequence of transitions from Prepared.
        checkpoint.state.restore(&mut record);
        record.revision = checkpoint.revision;
        record.updated_at = checkpoint.updated_at;
        record.validate_state()?;
        tail = expected;
        budget = budget.max(bytes.len() as u64);
    } else if head.base != record.revision {
        return Err(invalid());
    }
    let mut file = log_file(directory, &head)?;
    if file.metadata().map_err(io_error)?.len() < head.length {
        return Err(invalid());
    }
    let mut remaining = head.length;
    while remaining != 0 {
        if remaining < 8 {
            return Err(invalid());
        }
        let mut size = [0; 8];
        file.read_exact(&mut size).map_err(io_error)?;
        let size = u64::from_le_bytes(size);
        remaining -= 8;
        if size > MAX_FRAME || size > remaining {
            return Err(invalid());
        }
        let mut bytes = vec![0; usize::try_from(size).map_err(|_| invalid())?];
        file.read_exact(&mut bytes).map_err(io_error)?;
        io.read(bytes.len() + 8);
        let event: Event = decode(&bytes)?;
        if record.revision.checked_add(1) != Some(event.revision) {
            return Err(invalid());
        }
        for change in event.changes {
            change.apply(&mut record)?;
        }
        record.revision = event.revision;
        record.updated_at = event.updated_at;
        record.validate_state()?;
        tail = chain(tail, &bytes);
        remaining -= size;
    }
    if tail != head.tail || record.revision != head.revision {
        return Err(invalid());
    }
    // A visible head after an uncertain publication must be flushed before effects resume.
    file.sync_all().map_err(io_error)?;
    directory.sync()?;
    let stamps = stamps(directory, &head)?;
    for (_, stamp) in &stamps {
        io.file_size(stamp.size);
    }
    Ok(Loaded {
        head,
        record,
        head_bytes,
        budget,
        stamps,
    })
}

pub(crate) fn changes(
    before: &TransactionRecord,
    after: &TransactionRecord,
) -> Result<Vec<TransactionChange>, StoreError> {
    if before.schema != after.schema
        || before.transaction_id != after.transaction_id
        || before.app_id != after.app_id
        || before.scope != after.scope
        || before.target != after.target
        || before.app_version != after.app_version
        || before.created_at != after.created_at
        || before.plan_hash != after.plan_hash
        || before.plan != after.plan
        || before.nodes.len() != after.nodes.len()
    {
        return Err(CorruptReason::IdentityMismatch.into());
    }
    let mut changes = Vec::new();
    for (id, value) in &after.nodes {
        if before.nodes.get(id) != Some(value) {
            changes.push(TransactionChange::Node(id.clone(), value.clone()));
        }
    }
    macro_rules! field {
        ($field:ident, $variant:ident) => {
            if before.$field != after.$field {
                changes.push(TransactionChange::$variant(after.$field.clone()));
            }
        };
    }
    field!(phase, Phase);
    field!(reservation, Reservation);
    field!(publication, Publication);
    field!(resolution, Resolution);
    field!(failure, Failure);
    field!(rollback_failure, RollbackFailure);
    field!(recovery_failure, RecoveryFailure);
    field!(reconciliation, Reconciliation);
    if before.release != after.release {
        changes.push(TransactionChange::Release(
            after.release.clone().map(Box::new),
        ));
    }
    Ok(changes)
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MutableState {
    nodes: std::collections::BTreeMap<OperationId, NodeState>,
    phase: TransactionPhase,
    reservation: Option<crate::InstallationReservation>,
    publication: crate::PublicationState,
    resolution: Option<crate::RecoveryDecision>,
    failure: Option<crate::TransactionFailure>,
    rollback_failure: Option<crate::TransactionFailure>,
    recovery_failure: Option<crate::TransactionFailure>,
    reconciliation: crate::ReconciliationState,
    release: Option<zup_core::ReleaseIdentity>,
}

impl MutableState {
    fn capture(record: &TransactionRecord) -> Self {
        Self {
            nodes: record.nodes.clone(),
            phase: record.phase,
            reservation: record.reservation.clone(),
            publication: record.publication,
            resolution: record.resolution.clone(),
            failure: record.failure.clone(),
            rollback_failure: record.rollback_failure.clone(),
            recovery_failure: record.recovery_failure.clone(),
            reconciliation: record.reconciliation.clone(),
            release: record.release.clone(),
        }
    }
    fn restore(self, record: &mut TransactionRecord) {
        record.nodes = self.nodes;
        record.phase = self.phase;
        record.reservation = self.reservation;
        record.publication = self.publication;
        record.resolution = self.resolution;
        record.failure = self.failure;
        record.rollback_failure = self.rollback_failure;
        record.recovery_failure = self.recovery_failure;
        record.reconciliation = self.reconciliation;
        record.release = self.release;
    }
}

pub(crate) fn commit<D: JournalFiles + ?Sized>(
    directory: &D,
    current: &mut Loaded,
    changes: Vec<TransactionChange>,
    io: &Counters,
) -> Result<(), StoreError> {
    let next = &current.record;
    let id = next.transaction_id.to_string();
    let event = encode(&Event {
        revision: next.revision,
        updated_at: next.updated_at,
        changes,
    })?;
    if event.len() as u64 > MAX_FRAME {
        return Err(invalid());
    }
    let mut head = current.head.clone();
    let mut budget = current.budget;
    if head.length + event.len() as u64 + 8 > budget.max(64 * 1024) * 2 {
        boundary(Boundary::JournalCheckpointBefore, &id);
        head.slot = head.slot.other();
        let bytes = encode(&Checkpoint {
            transaction: next.transaction_id,
            intent: head.intent,
            revision: next.revision,
            updated_at: next.updated_at,
            state: MutableState::capture(next),
        })?;
        publish(directory, head.checkpoint_name(), &bytes, io)?;
        boundary(Boundary::JournalCheckpointWritten, &id);
        publish(directory, head.log_name(), &[], io)?;
        let file = directory.lock_file(head.log_name(), private_fs::FileCreation::Existing)?;
        head.log = zup_storage::namespace::identity(&file).map_err(io_error)?;
        head.base = next.revision;
        head.length = 0;
        head.checkpoint = Some(hash_bytes(&bytes));
        head.tail = hash_bytes(&bytes);
        budget = budget.max(bytes.len() as u64);
        boundary(Boundary::JournalCheckpointReady, &id);
    } else {
        let mut file = log_file(directory, &head)?;
        if file.metadata().map_err(io_error)?.len() < head.length {
            return Err(invalid());
        }
        file.set_len(head.length).map_err(io_error)?;
        file.seek(SeekFrom::Start(head.length)).map_err(io_error)?;
        file.write_all(&(event.len() as u64).to_le_bytes())
            .map_err(io_error)?;
        boundary(Boundary::JournalFrameHeaderWritten, &id);
        file.write_all(&event).map_err(io_error)?;
        io.write(event.len() + 8);
        boundary(Boundary::JournalFrameWritten, &id);
        file.sync_all().map_err(io_error)?;
        boundary(Boundary::JournalFrameSynced, &id);
        let _verified = log_file(directory, &head)?;
        head.length += event.len() as u64 + 8;
        head.tail = chain(head.tail, &event);
    }
    head.revision = next.revision;
    let head_bytes = encode(&head)?;
    boundary(Boundary::JournalHeadBefore, &id);
    publish(directory, "head.json", &head_bytes, io)?;
    boundary(Boundary::JournalHeadPublished, &id);
    if head.checkpoint.is_some() && head.base == next.revision {
        boundary(Boundary::JournalCheckpointPublished, &id);
    }
    let stamps = stamps(directory, &head)?;
    for (_, stamp) in &stamps {
        io.file_size(stamp.size);
    }
    current.head = head;
    current.head_bytes = head_bytes;
    current.budget = budget;
    current.stamps = stamps;
    Ok(())
}
