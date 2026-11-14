//! A final directory is a journal identity. Creation has one bounded, separate slot.
use crate::{
    CorruptReason, StoreError, TransactionRecord,
    fault_injection::{Boundary, boundary},
};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
};
use zup_core::{NativeObjectIdentity, TransactionId};

pub trait JournalFiles {
    fn read(&self, name: &str) -> Result<Option<Vec<u8>>, StoreError>;
    fn publish(&self, name: &str, bytes: &[u8], create: bool) -> Result<(), StoreError>;
    fn lock_file(&self, name: &str, creation: private_fs::FileCreation)
    -> Result<File, StoreError>;
    fn sync(&self) -> Result<(), StoreError>;
}

pub trait JournalDirectory: JournalFiles + Sized {
    fn open(&self, name: &str) -> Result<Option<Self>, StoreError>;
    fn create(&self, name: &str) -> Result<Self, StoreError>;
    fn object(&self) -> Result<NativeObjectIdentity, StoreError>;
    fn same_mount(&self, other: &Self) -> Result<bool, StoreError>;
    fn names(&self) -> Result<Vec<String>, StoreError>;
    fn move_to(
        &self,
        name: &str,
        destination: &Self,
        target: &str,
        object: NativeObjectIdentity,
    ) -> Result<(), StoreError>;
    fn retire_reservation(&self, bytes: &[u8]) -> Result<(), StoreError>;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JournalIdentity {
    schema: u32,
    transaction: TransactionId,
    directory: NativeObjectIdentity,
    namespace: NativeObjectIdentity,
    lock: NativeObjectIdentity,
}

impl JournalIdentity {
    pub fn validate(
        bytes: &[u8],
        id: TransactionId,
        directory: NativeObjectIdentity,
        lock: NativeObjectIdentity,
        namespace: NativeObjectIdentity,
    ) -> Result<(), StoreError> {
        let seal: Self =
            serde_json::from_slice(bytes).map_err(|e| CorruptReason::InvalidJson(e.to_string()))?;
        if seal.schema != 1
            || seal.transaction != id
            || seal.directory != directory
            || seal.lock != lock
            || seal.namespace != namespace
        {
            return Err(CorruptReason::IdentityMismatch.into());
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Creation {
    identity: JournalIdentity,
    record: TransactionRecord,
}

fn deferred(reason: impl ToString) -> StoreError {
    StoreError::CreationDeferred {
        reason: reason.to_string(),
    }
}
fn io(error: std::io::Error) -> StoreError {
    StoreError::Io {
        path: "journal-creation".into(),
        source: error,
    }
}

pub fn journal_lock<D: JournalDirectory>(
    directory: &D,
    id: TransactionId,
    namespace: NativeObjectIdentity,
) -> Result<File, StoreError> {
    let file = directory.lock_file("transaction.lock", private_fs::FileCreation::Existing)?;
    file.lock().map_err(io)?;
    verify_held_lock(directory, id, namespace, &file)?;
    Ok(file)
}

fn verify_held_lock<D: JournalDirectory>(
    directory: &D,
    id: TransactionId,
    namespace: NativeObjectIdentity,
    mut file: &File,
) -> Result<(), StoreError> {
    file.seek(SeekFrom::Start(0)).map_err(io)?;
    let mut seal = Vec::new();
    file.take(4097).read_to_end(&mut seal).map_err(io)?;
    let object = zup_storage::namespace::identity(file).map_err(io)?;
    JournalIdentity::validate(&seal, id, directory.object()?, object, namespace)?;
    let current = directory.lock_file("transaction.lock", private_fs::FileCreation::Existing)?;
    if zup_storage::namespace::identity(&current).map_err(io)? != object {
        return Err(CorruptReason::IdentityMismatch.into());
    }
    Ok(())
}

pub trait LockedJournal: JournalFiles + Send {}

pub struct JournalLease<D> {
    namespace: D,
    directory: D,
    lock: File,
    id: TransactionId,
}
impl<D: JournalDirectory + Send> JournalLease<D> {
    pub fn open(namespace: D, id: TransactionId) -> Result<Option<Self>, StoreError> {
        let Some(directory) = namespace.open(&id.to_string())? else {
            return Ok(None);
        };
        let lock = journal_lock(&directory, id, namespace.object()?)?;
        Ok(Some(Self {
            namespace,
            directory,
            lock,
            id,
        }))
    }
    fn verify(&self) -> Result<(), StoreError> {
        verify_held_lock(
            &self.directory,
            self.id,
            self.namespace.object()?,
            &self.lock,
        )?;
        let current = self
            .namespace
            .open(&self.id.to_string())?
            .ok_or(CorruptReason::Missing)?;
        if current.object()? != self.directory.object()? {
            return Err(CorruptReason::IdentityMismatch.into());
        }
        Ok(())
    }
}

impl<D: JournalDirectory + Send> LockedJournal for JournalLease<D> {}

impl<D: JournalDirectory + Send> JournalFiles for JournalLease<D> {
    fn read(&self, name: &str) -> Result<Option<Vec<u8>>, StoreError> {
        self.verify()?;
        self.directory.read(name)
    }
    fn publish(&self, name: &str, bytes: &[u8], create: bool) -> Result<(), StoreError> {
        self.verify()?;
        self.directory.publish(name, bytes, create)
    }
    fn lock_file(
        &self,
        name: &str,
        creation: private_fs::FileCreation,
    ) -> Result<File, StoreError> {
        self.verify()?;
        self.directory.lock_file(name, creation)
    }
    fn sync(&self) -> Result<(), StoreError> {
        self.verify()?;
        self.directory.sync()?;
        self.namespace.sync()
    }
}

pub fn create_journal<D: JournalDirectory>(
    creation: &D,
    transactions: &D,
    id: TransactionId,
    bytes: &[u8],
) -> Result<(), StoreError> {
    let record: TransactionRecord =
        serde_json::from_slice(bytes).map_err(|e| CorruptReason::InvalidJson(e.to_string()))?;
    record.validate()?;
    if record.transaction_id != id || !creation.same_mount(transactions)? {
        return Err(CorruptReason::IdentityMismatch.into());
    }
    let coordinator =
        creation.lock_file("creation.lock", private_fs::FileCreation::OpenOrCreate)?;
    coordinator.lock().map_err(io)?;
    if coordinator.metadata().map_err(io)?.len() != 0 {
        return Err(deferred("creation lock contains foreign data"));
    }
    creation.sync()?;
    finish_creation(creation, transactions)?;
    if let Some(directory) = transactions.open(&id.to_string())? {
        let _lock = journal_lock(&directory, id, transactions.object()?)?;
        directory
            .read("transaction.json")?
            .ok_or(CorruptReason::Missing)?;
        return Err(StoreError::AlreadyExists { id: id.to_string() });
    }
    let directory = creation.create("pending")?;
    creation.sync()?;
    boundary(Boundary::JournalNamespaceCreated, &id.to_string());
    let mut lock = directory.lock_file("transaction.lock", private_fs::FileCreation::CreateNew)?;
    let identity = JournalIdentity {
        schema: 1,
        transaction: id,
        directory: directory.object()?,
        namespace: transactions.object()?,
        lock: zup_storage::namespace::identity(&lock).map_err(io)?,
    };
    let seal = serde_json::to_vec(&identity).map_err(StoreError::Serialize)?;
    lock.write_all(&seal).map_err(io)?;
    lock.sync_all().map_err(io)?;
    directory.sync()?;
    boundary(Boundary::JournalLockPublished, &id.to_string());
    let reservation =
        serde_json::to_vec(&Creation { identity, record }).map_err(StoreError::Serialize)?;
    creation.publish("pending.json", &reservation, true)?;
    boundary(Boundary::JournalCreationReserved, &id.to_string());
    drop(lock);
    drop(directory);
    finish_creation(creation, transactions)
}

fn finish_creation<D: JournalDirectory>(creation: &D, transactions: &D) -> Result<(), StoreError> {
    for name in creation.names()? {
        if !matches!(name.as_str(), "creation.lock" | "pending" | "pending.json") {
            return Err(deferred(
                "creation namespace contains an undeclared member; evidence retained",
            ));
        }
    }
    let Some(bytes) = creation.read("pending.json")? else {
        if creation.open("pending")?.is_some() {
            return Err(deferred(
                "unreserved creation slot; native ownership is unproven",
            ));
        }
        return Ok(());
    };
    let pending: Creation = serde_json::from_slice(&bytes)
        .map_err(|e| deferred(format!("invalid creation reservation: {e}")))?;
    pending.record.validate()?;
    let id = pending.record.transaction_id;
    if pending.identity.transaction != id
        || pending.identity.schema != 1
        || pending.identity.namespace != transactions.object()?
    {
        return Err(CorruptReason::IdentityMismatch.into());
    }
    let expected = serde_json::to_vec_pretty(&pending.record).map_err(StoreError::Serialize)?;
    let name = id.to_string();
    if let Some(directory) = creation.open("pending")? {
        if directory.object()? != pending.identity.directory || transactions.open(&name)?.is_some()
        {
            return Err(deferred(
                "creation slot or destination identity is ambiguous",
            ));
        }
        let mut lock = journal_lock(&directory, id, transactions.object()?)?;
        lock.seek(SeekFrom::Start(0)).map_err(io)?;
        let mut seal = Vec::new();
        lock.read_to_end(&mut seal).map_err(io)?;
        JournalIdentity::validate(
            &seal,
            id,
            pending.identity.directory,
            pending.identity.lock,
            transactions.object()?,
        )?;
        for name in directory.names()? {
            if !matches!(
                name.as_str(),
                "transaction.lock" | "transaction.json" | "head.json" | "events-a.log"
            ) {
                return Err(deferred(
                    "creation contains an undeclared member; evidence retained",
                ));
            }
        }
        match directory.read("transaction.json")? {
            Some(found) if found != expected => {
                return Err(deferred("creation record differs from its reservation"));
            }
            Some(_) => {}
            None => directory.publish("transaction.json", &expected, true)?,
        }
        crate::journal_format::initialize(&directory, &expected, &pending.record)?;
        directory.sync()?;
        boundary(Boundary::JournalRecordPublished, &name);
        drop(lock);
        drop(directory);
        // A rename error can mean an uncertain acknowledgement. Only the exact
        // reserved native object at the destination can settle that ambiguity.
        let moved = creation.move_to("pending", transactions, &name, pending.identity.directory);
        if let Err(error) = moved {
            let Some(observed) = transactions.open(&name)? else {
                return Err(error);
            };
            if observed.object()? != pending.identity.directory {
                return Err(error);
            }
        }
        if creation.open("pending")?.is_some() {
            return Err(deferred("publication left an ambiguous source namespace"));
        }
        boundary(Boundary::JournalNamespacePublished, &name);
    }
    let directory = transactions
        .open(&name)?
        .ok_or_else(|| deferred("reserved creation and published namespace both missing"))?;
    if directory.object()? != pending.identity.directory {
        return Err(CorruptReason::IdentityMismatch.into());
    }
    let mut lock = journal_lock(&directory, id, transactions.object()?)?;
    lock.seek(SeekFrom::Start(0)).map_err(io)?;
    let mut seal = Vec::new();
    lock.read_to_end(&mut seal).map_err(io)?;
    if seal != serde_json::to_vec(&pending.identity).map_err(StoreError::Serialize)? {
        return Err(CorruptReason::IdentityMismatch.into());
    }
    let observed = directory
        .read("transaction.json")?
        .ok_or(CorruptReason::Missing)?;
    // Once visible, CAS may already have advanced the record. Never overwrite it.
    let record: TransactionRecord =
        serde_json::from_slice(&observed).map_err(|e| CorruptReason::InvalidJson(e.to_string()))?;
    record.validate()?;
    crate::journal_format::load(&directory, id, &crate::journal_io::Counters::default())?;
    if record != pending.record {
        return Err(CorruptReason::IdentityMismatch.into());
    }
    directory.sync()?;
    boundary(Boundary::JournalParentSyncBefore, &name);
    transactions.sync()?;
    creation.sync()?;
    boundary(Boundary::JournalParentSynced, &name);
    boundary(Boundary::JournalReservationRemoveBefore, &name);
    creation.retire_reservation(&bytes)?;
    boundary(Boundary::JournalCreationRetired, &name);
    Ok(())
}
