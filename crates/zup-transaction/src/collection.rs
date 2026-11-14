use crate::{CleanupCompletion, CleanupDeferral};
use serde::{Deserialize, Serialize};
use zup_core::{MaterializationTree, Sha256Digest, TransactionId};
use zup_plan::{CleanupOperation, GenerationCleanup};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CollectionState {
    Prepared,
    Deleting,
    Complete,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GenerationCollection {
    pub schema: u32,
    pub transaction: TransactionId,
    pub intent: Sha256Digest,
    pub tree: MaterializationTree,
    pub state: CollectionState,
}

pub enum CollectionFence<F> {
    Ready(F),
    Deferred(CleanupDeferral),
}

pub trait GenerationCollector {
    type Coordinator;
    type Fence;
    fn coordinate(&self, generation: &GenerationCleanup) -> Result<Self::Coordinator, String>;
    fn load(
        &self,
        transaction: TransactionId,
        intent: Sha256Digest,
    ) -> Result<Option<GenerationCollection>, String>;
    fn save(&self, record: &GenerationCollection, create: bool) -> Result<(), String>;
    fn inspect(
        &self,
        generation: &GenerationCleanup,
        previous: Option<&GenerationCollection>,
    ) -> Result<Option<MaterializationTree>, String>;
    fn fence(
        &self,
        generation: &GenerationCleanup,
        tree: &MaterializationTree,
    ) -> Result<CollectionFence<Self::Fence>, String>;
    fn remove(
        &self,
        generation: &GenerationCleanup,
        tree: &MaterializationTree,
        fence: Self::Fence,
    ) -> Result<(), String>;
}

pub fn collect_generation(
    filesystem: &impl GenerationCollector,
    operation: &CleanupOperation,
    generation: &GenerationCleanup,
    transaction: TransactionId,
) -> Result<CleanupCompletion, String> {
    let _coordinator = filesystem.coordinate(generation)?;
    if generation.readers == zup_core::GenerationReaders::Untracked {
        return Ok(CleanupCompletion::Deferred {
            reason: CleanupDeferral::UntrackedReaders,
        });
    }
    let intent = operation.fingerprint();
    let previous = filesystem.load(transaction, intent)?;
    if let Some(previous) = &previous
        && (previous.schema != 1
            || previous.transaction != transaction
            || previous.intent != intent
            || previous.tree != generation.tree)
    {
        return Err("generation collection evidence differs from its protected intent".into());
    }
    let current = filesystem.inspect(generation, previous.as_ref())?;
    let mut record = match previous {
        Some(record) => record,
        None if current.is_none() => {
            let record = GenerationCollection {
                schema: 1,
                transaction,
                intent,
                tree: generation.tree.clone(),
                state: CollectionState::Complete,
            };
            filesystem.save(&record, true)?;
            if filesystem.inspect(generation, Some(&record))?.is_some() {
                return Err("absent generation reappeared during collection observation".into());
            }
            return Ok(CleanupCompletion::AlreadyAbsent);
        }
        None => {
            let tree = current
                .clone()
                .ok_or("retired generation disappeared before collection preparation")?;
            if tree != generation.tree {
                return Err("generation member identities differ from their protected seal".into());
            }
            let record = GenerationCollection {
                schema: 1,
                transaction,
                intent,
                tree,
                state: CollectionState::Prepared,
            };
            filesystem.save(&record, true)?;
            crate::fault_injection::boundary(
                crate::fault_injection::Boundary::CollectionPrepared,
                operation.id().as_str(),
            );
            record
        }
    };
    let Some(tree) = current else {
        if record.state == CollectionState::Prepared {
            return Err("prepared generation disappeared before deletion intent".into());
        }
        record.state = CollectionState::Complete;
        filesystem.save(&record, false)?;
        return Ok(CleanupCompletion::AlreadyAbsent);
    };
    if record.state == CollectionState::Complete {
        return Err("collected generation name is occupied".into());
    }
    match filesystem.fence(generation, &tree)? {
        CollectionFence::Deferred(reason) if record.state == CollectionState::Prepared => Ok(CleanupCompletion::Deferred { reason }),
        CollectionFence::Deferred(_) => Err("interrupted generation collection is blocked by a native consumer; recovery remains pending".into()),
        CollectionFence::Ready(fence) => {
            record.state = CollectionState::Deleting;
            filesystem.save(&record, false)?;
            crate::fault_injection::boundary(crate::fault_injection::Boundary::CollectionDeleting, operation.id().as_str());
            filesystem.remove(generation, &tree, fence)?;
            if filesystem.inspect(generation, Some(&record))?.is_some() { return Err("generation namespace is occupied after collection".into()); }
            record.state = CollectionState::Complete;
            filesystem.save(&record, false)?;
            crate::fault_injection::boundary(crate::fault_injection::Boundary::CollectionCompleted, operation.id().as_str());
            Ok(CleanupCompletion::Removed)
        }
    }
}
pub fn verify_generation_collection(
    filesystem: &impl GenerationCollector,
    operation: &CleanupOperation,
    generation: &GenerationCleanup,
    transaction: TransactionId,
    completion: CleanupCompletion,
) -> Result<(), String> {
    let _coordinator = filesystem.coordinate(generation)?;
    let record = filesystem.load(transaction, operation.fingerprint())?;
    match completion {
        CleanupCompletion::Removed | CleanupCompletion::AlreadyAbsent => {
            let record = record.ok_or("generation collection completion evidence is missing")?;
            if record.schema != 1
                || record.transaction != transaction
                || record.intent != operation.fingerprint()
                || record.tree != generation.tree
                || record.state != CollectionState::Complete
                || filesystem.inspect(generation, Some(&record))?.is_some()
            {
                return Err("generation collection completion differs from native state".into());
            }
        }
        CleanupCompletion::Deferred { reason } => {
            if reason == CleanupDeferral::UntrackedReaders
                && generation.readers != zup_core::GenerationReaders::Untracked
                || record.is_some_and(|record| {
                    record.state != CollectionState::Prepared || record.tree != generation.tree
                })
            {
                return Err(
                    "generation collection deferral conflicts with irreversible deletion".into(),
                );
            }
        }
    }
    Ok(())
}
