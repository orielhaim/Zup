use std::collections::BTreeSet;

use zup_core::MaterializationId;
use zup_plan::{CleanupCandidate, HistoryFileTarget, InstallationRecord, NodeKind};

use crate::{
    FilesystemTransactionStore, OperationReceipt, ReconciliationState, TransactionPhase,
    TransactionRecord,
};

struct HistorySource {
    record: TransactionRecord,
    materializations: Vec<MaterializationId>,
    sidecars: Vec<HistoryFileTarget>,
}

fn reclaimable_history(
    store: &FilesystemTransactionStore,
    installation: &InstallationRecord,
    retention: Option<&zup_plan::RetentionChange>,
) -> Result<Vec<HistorySource>, String> {
    let mut projected = installation.clone();
    if let Some(retention) = retention {
        retention.validate_owned(installation)?;
        projected
            .retired_deployments
            .retain(|retired| !retention.forget.contains(&(&retired.snapshot).into()));
    }
    let records = crate::settled_records(
        store,
        &installation.app_id,
        installation.scope,
        Some(installation),
    )
    .map_err(|error| error.to_string())?;
    let rooted = |id| {
        projected
            .retained
            .iter()
            .chain(std::iter::once(&projected.current))
            .chain(
                projected
                    .retired_deployments
                    .iter()
                    .map(|retired| &retired.snapshot),
            )
            .any(|snapshot| {
                snapshot
                    .materialization
                    .iter()
                    .chain(&snapshot.maintenance)
                    .any(|spec| spec.id() == id)
            })
    };
    let collected = |predicate: &dyn Fn(&CleanupCandidate) -> bool| {
        records.iter().any(|record| {
        record.phase == TransactionPhase::Committed && record.cleanup_complete() && record.plan.nodes.iter().any(|node| {
            let NodeKind::Cleanup { operation } = &node.kind else { return false; };
            predicate(&operation.candidate) && matches!(record.receipt(&node.id), Some(OperationReceipt::Cleanup {
                intent, completion: crate::CleanupCompletion::Removed | crate::CleanupCompletion::AlreadyAbsent,
            }) if *intent == operation.fingerprint())
        })
    })
    };
    let mut result = Vec::new();
    for source in &records {
        if source.plan.installation != installation.id
            || source.target != installation.target
            || !matches!(
                source.phase,
                TransactionPhase::Committed | TransactionPhase::RolledBack
            )
            || source.phase == TransactionPhase::Committed
                && source.reconciliation != ReconciliationState::Complete
            || projected.references_transaction(source.transaction_id)
            || source
                .reservation
                .as_ref()
                .is_some_and(|reservation| reservation.origin == source.transaction_id)
        {
            continue;
        }
        let mut materializations = BTreeSet::new();
        let mut sidecars = BTreeSet::new();
        let mut eligible = true;
        for node in &source.plan.nodes {
            match (&node.kind, source.receipt(&node.id)) {
                (
                    NodeKind::Generation {
                        operation: zup_plan::GenerationOperation::Materialize,
                        intent,
                    },
                    _,
                ) => {
                    materializations.insert(intent.materialization.id());
                    sidecars.insert(HistoryFileTarget::Materialization {
                        id: intent.materialization.id(),
                    });
                    let seal = source.plan.nodes.iter().find_map(|node| {
                        match (&node.kind, source.receipt(&node.id)) {
                            (
                                NodeKind::Generation {
                                    operation: zup_plan::GenerationOperation::Seal,
                                    intent: sealed,
                                },
                                Some(OperationReceipt::Generation(receipt)),
                            ) if sealed.materialization == intent.materialization => {
                                match receipt.as_ref() {
                                    crate::GenerationReceipt::Sealed { observed, seal, .. } => {
                                        Some((observed.object, *seal))
                                    }
                                    _ => None,
                                }
                            }
                            _ => None,
                        }
                    });
                    eligible &= seal.is_some_and(|(object, seal)| collected(&|candidate| matches!(candidate, CleanupCandidate::Generation(generation)
                        if generation.materialization == intent.materialization && generation.creator == source.transaction_id && generation.tree.root == object && generation.seal == seal)));
                }
                (NodeKind::PersistentDirectory { operation }, _)
                    if operation.stage == zup_plan::PersistentDirectoryStage::Prepare =>
                {
                    sidecars.insert(HistoryFileTarget::Persistent {
                        resource: operation.intent.directory.id.clone(),
                    });
                }
                (NodeKind::Cleanup { operation }, receipt) => {
                    if matches!(
                        receipt,
                        Some(OperationReceipt::Cleanup {
                            completion: crate::CleanupCompletion::Removed
                                | crate::CleanupCompletion::AlreadyAbsent,
                            ..
                        })
                    ) {
                        let origin = match &operation.candidate {
                            CleanupCandidate::Generation(generation) => Some(generation.creator),
                            CleanupCandidate::Fixed(file) => Some(file.retained_by),
                            CleanupCandidate::History(_) => None,
                        };
                        eligible &= origin.is_none_or(|origin| {
                            !records.iter().any(|record| record.transaction_id == origin)
                        });
                    }
                    if let CleanupCandidate::Generation(generation) = &operation.candidate {
                        if !matches!(
                            receipt,
                            Some(OperationReceipt::Cleanup {
                                completion: crate::CleanupCompletion::Deferred {
                                    reason: crate::CleanupDeferral::UntrackedReaders
                                },
                                ..
                            })
                        ) {
                            materializations.insert(generation.materialization.id());
                        }
                        sidecars.insert(HistoryFileTarget::Collection {
                            intent: operation.fingerprint(),
                        });
                    }
                }
                (
                    _,
                    Some(OperationReceipt::ReplaceFile {
                        previous_object,
                        previous_size,
                        previous_sha256,
                        ..
                    }),
                ) => {
                    eligible &= collected(&|candidate| {
                        matches!(candidate, CleanupCandidate::Fixed(file)
                        if file.retained_by == source.transaction_id && file.operation == node.id && file.instance.object == *previous_object
                            && file.size == *previous_size && file.sha256 == *previous_sha256)
                    });
                }
                (
                    _,
                    Some(OperationReceipt::RemoveFile {
                        object,
                        size,
                        sha256,
                        ..
                    }),
                ) => {
                    eligible &= collected(&|candidate| {
                        matches!(candidate, CleanupCandidate::Fixed(file)
                        if file.retained_by == source.transaction_id && file.operation == node.id && file.instance.object == *object
                            && file.size == *size && file.sha256 == *sha256)
                    });
                }
                _ => {}
            }
        }
        if eligible && !materializations.iter().copied().any(rooted) {
            result.push(HistorySource {
                record: source.clone(),
                materializations: materializations.into_iter().collect(),
                sidecars: sidecars.into_iter().collect(),
            });
        }
    }
    Ok(result)
}

#[derive(Clone, Copy)]
pub enum StateNamespace {
    Transactions,
    History,
    Materializations(zup_core::InstallationId),
    Data,
    Collections,
}

pub trait HistoryFilesystem {
    type Directory;
    fn namespace(
        &self,
        namespace: StateNamespace,
        create: bool,
    ) -> Result<Option<Self::Directory>, String>;
    fn open_directory(
        &self,
        parent: &Self::Directory,
        name: &str,
    ) -> Result<Option<Self::Directory>, String>;
    fn object(directory: &Self::Directory) -> Result<zup_core::NativeObjectIdentity, String>;
    fn names(directory: &Self::Directory) -> Result<Vec<String>, String>;
    fn read_file(
        directory: &Self::Directory,
        name: &str,
    ) -> Result<Option<(zup_plan::MetadataFile, Vec<u8>)>, String>;
    fn move_directory(
        &self,
        parent: &Self::Directory,
        name: &str,
        destination: &Self::Directory,
        object: zup_core::NativeObjectIdentity,
    ) -> Result<(), String>;
    fn remove_file(
        directory: &Self::Directory,
        name: &str,
        expected: &zup_plan::MetadataFile,
    ) -> Result<(), String>;
    fn remove_directory(
        parent: &Self::Directory,
        name: &str,
        object: zup_core::NativeObjectIdentity,
    ) -> Result<(), String>;
}

pub struct HistoryStorage<F>(pub F);

impl<F: HistoryFilesystem> HistoryStorage<F> {
    fn sidecar(
        &self,
        installation: zup_core::InstallationId,
        source: zup_core::TransactionId,
        target: &HistoryFileTarget,
    ) -> Result<Option<(F::Directory, String)>, String> {
        let namespace = match target {
            HistoryFileTarget::Materialization { .. } => {
                StateNamespace::Materializations(installation)
            }
            HistoryFileTarget::Persistent { .. } => StateNamespace::Data,
            HistoryFileTarget::Collection { .. } => StateNamespace::Collections,
        };
        let Some(parent) = self.0.namespace(namespace, false)? else {
            return Ok(None);
        };
        Ok(Some((
            parent,
            target
                .path(installation, source)
                .as_str()
                .rsplit('/')
                .next()
                .expect("metadata path has a file name")
                .to_owned(),
        )))
    }

    pub fn candidates(
        &self,
        store: &FilesystemTransactionStore,
        installation: &InstallationRecord,
        retention: Option<&zup_plan::RetentionChange>,
    ) -> Result<Vec<zup_plan::HistoryCleanup>, String> {
        use zup_plan::{HistoryCleanup, HistoryObject, JournalMember};
        let mut result = Vec::new();
        let Some(transactions) = self.0.namespace(StateNamespace::Transactions, false)? else {
            return Ok(result);
        };
        'source: for source in reclaimable_history(store, installation, retention)? {
            let directory = self
                .0
                .open_directory(&transactions, &source.record.transaction_id.to_string())?
                .ok_or("settled history disappeared during observation")?;
            let mut members = std::collections::BTreeMap::new();
            for name in F::names(&directory)? {
                let member = match name.as_str() {
                    "transaction.json" => JournalMember::Record,
                    "transaction.lock" => JournalMember::Lock,
                    _ => return Err("history journal contains an undeclared entry".into()),
                };
                let (observed, bytes) =
                    F::read_file(&directory, &name)?.ok_or("history journal member disappeared")?;
                match member {
                    JournalMember::Record => {
                        let record: TransactionRecord = serde_json::from_slice(&bytes)
                            .map_err(|error| format!("history journal is invalid: {error}"))?;
                        if record != source.record {
                            return Err("history journal changed during observation".into());
                        }
                    }
                    JournalMember::Lock if !bytes.is_empty() => {
                        return Err("history lock contains unexpected data".into());
                    }
                    JournalMember::Lock => {}
                }
                members.insert(member, observed);
            }
            if !members.contains_key(&JournalMember::Record) {
                return Err("history journal has no protected record".into());
            }
            let mut observed_files = Vec::new();
            for target in &source.sidecars {
                let Some((parent, name)) =
                    self.sidecar(installation.id, source.record.transaction_id, target)?
                else {
                    continue;
                };
                let Some((observed, bytes)) = F::read_file(&parent, &name)? else {
                    continue;
                };
                if !validate_sidecar(&source.record, target, &bytes)? {
                    continue 'source;
                }
                observed_files.push(HistoryCleanup {
                    installation: installation.id,
                    source: source.record.transaction_id,
                    materializations: source.materializations.clone(),
                    object: HistoryObject::File {
                        target: target.clone(),
                        observed,
                    },
                });
            }
            result.extend(observed_files);
            result.push(HistoryCleanup {
                installation: installation.id,
                source: source.record.transaction_id,
                materializations: source.materializations,
                object: HistoryObject::Journal {
                    plan_hash: source.record.plan_hash,
                    object: F::object(&directory)?,
                    members,
                    sidecars: source.sidecars,
                },
            });
        }
        Ok(result)
    }

    pub fn collect(
        &self,
        history: &zup_plan::HistoryCleanup,
    ) -> Result<crate::CleanupCompletion, String> {
        use crate::CleanupCompletion;
        use zup_plan::HistoryObject;
        match &history.object {
            HistoryObject::File { target, observed } => {
                let Some((parent, name)) =
                    self.sidecar(history.installation, history.source, target)?
                else {
                    return Ok(CleanupCompletion::AlreadyAbsent);
                };
                let Some((current, _)) = F::read_file(&parent, &name)? else {
                    return Ok(CleanupCompletion::AlreadyAbsent);
                };
                if current != *observed {
                    return Err(
                        "history metadata file differs from its protected native instance".into(),
                    );
                }
                F::remove_file(&parent, &name, observed)?;
                crate::fault_injection::boundary(
                    crate::fault_injection::Boundary::HistoryMemberRemoved,
                    &format!("history:{}", history.source),
                );
                if F::read_file(&parent, &name)?.is_some() {
                    return Err("history metadata remains after removal".into());
                }
            }
            HistoryObject::Journal {
                object,
                members,
                sidecars,
                ..
            } => {
                let transactions = self
                    .0
                    .namespace(StateNamespace::Transactions, false)?
                    .ok_or("history transactions namespace is missing")?;
                let name = history.source.to_string();
                let source = self.0.open_directory(&transactions, &name)?;
                let archive = self.0.namespace(StateNamespace::History, false)?;
                let archived = archive
                    .as_ref()
                    .map(|parent| self.0.open_directory(parent, &name))
                    .transpose()?
                    .flatten();
                if source.is_some() && archived.is_some() {
                    return Err(
                        "history journal occupies both transaction and archive names".into(),
                    );
                }
                if source.is_none() && archived.is_none() {
                    return Ok(CleanupCompletion::AlreadyAbsent);
                }
                for target in sidecars {
                    if let Some((parent, name)) =
                        self.sidecar(history.installation, history.source, target)?
                        && F::read_file(&parent, &name)?.is_some()
                    {
                        if archived.is_some() {
                            return Err("metadata reappeared after journal archival".into());
                        }
                        return Ok(CleanupCompletion::Deferred {
                            reason: crate::CleanupDeferral::ReferencedMetadata,
                        });
                    }
                }
                drop(archived);
                let archive = match archive {
                    Some(archive) => archive,
                    None => self
                        .0
                        .namespace(StateNamespace::History, true)?
                        .ok_or("history archive namespace is unavailable")?,
                };
                if let Some(source) = source {
                    verify_journal::<F>(&source, *object, members, false)?;
                    self.0
                        .move_directory(&transactions, &name, &archive, *object)?;
                    crate::fault_injection::boundary(
                        crate::fault_injection::Boundary::HistoryArchived,
                        &format!("history:{}", history.source),
                    );
                }
                let directory = self
                    .0
                    .open_directory(&archive, &name)?
                    .ok_or("archived journal is missing")?;
                verify_journal::<F>(&directory, *object, members, true)?;
                for (member, observed) in members {
                    if F::read_file(&directory, member.name())?.is_some() {
                        F::remove_file(&directory, member.name(), observed)?;
                        crate::fault_injection::boundary(
                            crate::fault_injection::Boundary::HistoryJournalMemberRemoved,
                            &format!("history:{}", history.source),
                        );
                    }
                }
                drop(directory);
                F::remove_directory(&archive, &name, *object)?;
                crate::fault_injection::boundary(
                    crate::fault_injection::Boundary::HistoryDirectoryRemoved,
                    &format!("history:{}", history.source),
                );
                if self.0.open_directory(&archive, &name)?.is_some()
                    || self.0.open_directory(&transactions, &name)?.is_some()
                {
                    return Err("history journal remains after removal".into());
                }
            }
        }
        Ok(CleanupCompletion::Removed)
    }

    pub fn verify(
        &self,
        history: &zup_plan::HistoryCleanup,
        completion: crate::CleanupCompletion,
    ) -> Result<(), String> {
        use zup_plan::HistoryObject;
        match (&history.object, completion) {
            (
                HistoryObject::File { target, .. },
                crate::CleanupCompletion::Removed | crate::CleanupCompletion::AlreadyAbsent,
            ) => {
                if let Some((parent, name)) =
                    self.sidecar(history.installation, history.source, target)?
                    && F::read_file(&parent, &name)?.is_some()
                {
                    return Err("collected metadata file remains present".into());
                }
            }
            (
                HistoryObject::Journal { .. },
                crate::CleanupCompletion::Removed | crate::CleanupCompletion::AlreadyAbsent,
            ) => {
                for namespace in [StateNamespace::Transactions, StateNamespace::History] {
                    if let Some(parent) = self.0.namespace(namespace, false)?
                        && self
                            .0
                            .open_directory(&parent, &history.source.to_string())?
                            .is_some()
                    {
                        return Err("collected history journal remains present".into());
                    }
                }
            }
            (
                HistoryObject::Journal {
                    object, members, ..
                },
                crate::CleanupCompletion::Deferred {
                    reason: crate::CleanupDeferral::ReferencedMetadata,
                },
            ) => {
                let transactions = self
                    .0
                    .namespace(StateNamespace::Transactions, false)?
                    .ok_or("history transactions namespace is missing")?;
                let source = self
                    .0
                    .open_directory(&transactions, &history.source.to_string())?
                    .ok_or("deferred history journal is missing")?;
                verify_journal::<F>(&source, *object, members, false)?;
                if let Some(archive) = self.0.namespace(StateNamespace::History, false)?
                    && self
                        .0
                        .open_directory(&archive, &history.source.to_string())?
                        .is_some()
                {
                    return Err("deferred journal was unexpectedly archived".into());
                }
            }
            _ => return Err("history cleanup has an incompatible completion receipt".into()),
        }
        Ok(())
    }
}

fn verify_journal<F: HistoryFilesystem>(
    directory: &F::Directory,
    object: zup_core::NativeObjectIdentity,
    members: &std::collections::BTreeMap<zup_plan::JournalMember, zup_plan::MetadataFile>,
    partial: bool,
) -> Result<(), String> {
    if F::object(directory)? != object {
        return Err("history journal directory identity changed".into());
    }
    let names = F::names(directory)?;
    if !partial && names.len() != members.len() {
        return Err("history journal membership changed".into());
    }
    for name in names {
        let expected = members
            .iter()
            .find(|(member, _)| member.name() == name)
            .map(|(_, observed)| observed)
            .ok_or("history journal contains an undeclared entry")?;
        let (observed, _) =
            F::read_file(directory, &name)?.ok_or("history journal member disappeared")?;
        if observed != *expected {
            return Err("history journal member identity or content changed".into());
        }
    }
    Ok(())
}

fn validate_sidecar(
    source: &TransactionRecord,
    target: &HistoryFileTarget,
    bytes: &[u8],
) -> Result<bool, String> {
    match target {
        HistoryFileTarget::Materialization { id } => {
            let record: crate::MaterializationRecord =
                serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
            record.validate()?;
            if !record.retired_preparations.is_empty() {
                return Ok(false);
            }
            if record.transaction != source.transaction_id || record.specification.installation() != source.plan.installation
                || record.specification.id() != *id || !matches!(&record.state, crate::MaterializationState::Sealed { .. })
                || !source.plan.nodes.iter().any(|node| matches!((&node.kind, source.receipt(&node.id)),
                    (NodeKind::Generation { operation: zup_plan::GenerationOperation::Seal, intent }, Some(OperationReceipt::Generation(receipt)))
                    if intent.materialization == record.specification && matches!(receipt.as_ref(), crate::GenerationReceipt::Sealed { observed, seal, .. }
                        if observed.object == record.object && record.fingerprint().is_ok_and(|fingerprint| fingerprint == *seal)))) {
                return Err("materialization metadata differs from its protected creator".into());
            }
        }
        HistoryFileTarget::Persistent { resource } => {
            let record: crate::PersistentPreparation =
                serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
            if record.transaction != source.transaction_id || record.installation != source.plan.installation
                || record.operation.intent.directory.id != *resource
                || !source.plan.nodes.iter().any(|node| matches!(&node.kind, NodeKind::PersistentDirectory { operation } if operation.as_ref() == &record.operation)) {
                return Err("persistent metadata differs from its protected preparation".into());
            }
        }
        HistoryFileTarget::Collection { intent } => {
            if source.plan.nodes.iter().any(|node| matches!(&node.kind, NodeKind::Cleanup { operation }
                if operation.fingerprint() == *intent && matches!(source.receipt(&node.id), Some(OperationReceipt::Cleanup {
                    completion: crate::CleanupCompletion::Deferred { reason: crate::CleanupDeferral::UntrackedReaders }, .. })))) {
                return Err("untracked-reader deferral cannot own collection metadata".into());
            }
            let record: crate::GenerationCollection =
                serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
            if record.schema != 1 || record.transaction != source.transaction_id || record.intent != *intent
                || !source.plan.nodes.iter().any(|node| matches!(&node.kind, NodeKind::Cleanup { operation }
                    if operation.fingerprint() == *intent && matches!(&operation.candidate, CleanupCandidate::Generation(generation) if generation.tree.root == record.tree.root))) {
                return Err("collection metadata differs from its protected operation".into());
            }
        }
    }
    Ok(true)
}
