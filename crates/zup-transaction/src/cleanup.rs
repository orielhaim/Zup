use crate::{OperationReceipt, TransactionPhase, TransactionRecord, TransactionStore};
use zup_plan::{CleanupCandidate, CleanupOperation, DeploymentNode, InstallationRecord, NodeKind};

pub fn authenticate_cleanup<'a>(
    store: &impl TransactionStore,
    installation: &InstallationRecord,
    transaction: &TransactionRecord,
    node: &'a DeploymentNode,
) -> Result<&'a CleanupOperation, String> {
    transaction.validate().map_err(|error| error.to_string())?;
    if transaction.phase != TransactionPhase::Committed
        || !transaction.plan.nodes.contains(node)
        || installation.commit.transaction != transaction.transaction_id
    {
        return Err("cleanup requires the currently committed protected deployment".into());
    }
    crate::verify_installation_evidence(installation, transaction)?;
    let NodeKind::Cleanup { operation } = &node.kind else {
        return Err("cleanup effect required".into());
    };
    operation.validate_owned(installation, None)?;
    match &operation.candidate {
        CleanupCandidate::History(_) => {}
        CleanupCandidate::Fixed(file) => {
            let source = store
                .load(&file.retained_by)
                .map_err(|error| error.to_string())?;
            if source.phase != TransactionPhase::Committed
                || source.app_id != transaction.app_id
                || source.scope != transaction.scope
                || source.target != transaction.target
                || source.plan.installation != transaction.plan.installation
            {
                return Err("retired file source journal differs from its installation".into());
            }
            let effect = source
                .plan
                .nodes
                .iter()
                .find(|node| node.id == file.operation)
                .ok_or("retired file source operation is missing")?;
            let matches = match (&effect.kind, source.receipt(&effect.id)) {
                (
                    NodeKind::FileMutation { key, .. },
                    Some(OperationReceipt::ReplaceFile {
                        previous_object,
                        previous_sha256,
                        previous_size,
                        backup_path,
                        ..
                    }),
                ) => {
                    key == &file.key
                        && previous_object == &file.instance.object
                        && previous_sha256 == &file.sha256
                        && previous_size == &file.size
                        && zup_core::TargetPath::new(transaction.target, backup_path)
                            .is_ok_and(|path| path.equivalent(&file.path))
                }
                (
                    NodeKind::FileRemoval { key },
                    Some(OperationReceipt::RemoveFile {
                        object,
                        sha256,
                        size,
                        backup_path,
                        ..
                    }),
                ) => {
                    key == &file.key
                        && object == &file.instance.object
                        && sha256 == &file.sha256
                        && size == &file.size
                        && backup_path.equivalent(&file.path)
                }
                _ => false,
            };
            if !matches {
                return Err(
                    "cleanup path or native instance differs from its protected retirement receipt"
                        .into(),
                );
            }
        }
        CleanupCandidate::Generation(generation) => {
            let source = store
                .load(&generation.creator)
                .map_err(|error| error.to_string())?;
            let provenance = match &generation.source {
                zup_plan::GenerationRetirement::Snapshot { .. } => {
                    source.phase == TransactionPhase::Committed
                }
                zup_plan::GenerationRetirement::Compensated {
                    transaction,
                    plan_hash,
                } => {
                    source.phase == TransactionPhase::RolledBack
                        && source.transaction_id == *transaction
                        && source.plan_hash == *plan_hash
                }
            };
            if matches!(
                &generation.source,
                zup_plan::GenerationRetirement::Compensated { .. }
            ) {
                let intent = source
                    .plan
                    .nodes
                    .iter()
                    .find_map(|node| match &node.kind {
                        NodeKind::Generation {
                            operation: zup_plan::GenerationOperation::Materialize,
                            intent,
                        } if intent.materialization == generation.materialization => Some(intent),
                        _ => None,
                    })
                    .ok_or("compensated generation has no original materialization intent")?;
                let readers = if matches!(
                    intent.publication,
                    zup_plan::GenerationPublication::Maintenance
                ) && source.plan.preset.is_none()
                {
                    zup_core::GenerationReaders::NativeExecutables
                } else {
                    source.plan.requirements.generation_readers
                };
                if generation.readers != readers {
                    return Err(
                        "cleanup cannot strengthen compensated generation reader contract".into(),
                    );
                }
            }
            if !provenance || source.app_id != transaction.app_id
                || source.scope != transaction.scope || source.target != transaction.target
                || source.plan.installation != transaction.plan.installation
                || !source.plan.nodes.iter().any(|node| matches!(&node.kind,
                    NodeKind::Generation { operation: zup_plan::GenerationOperation::Materialize, intent }
                    if intent.materialization == generation.materialization))
                || !source.plan.nodes.iter().any(|node| matches!((&node.kind, source.receipt(&node.id)),
                    (NodeKind::Generation { operation: zup_plan::GenerationOperation::Seal, intent }, Some(OperationReceipt::Generation(receipt)))
                    if intent.materialization == generation.materialization
                        && matches!(receipt.as_ref(), crate::GenerationReceipt::Sealed { observed, seal, tree }
                            if observed.object == generation.tree.root && *tree == generation.tree && *seal == generation.seal))) {
                return Err("generation cleanup differs from its protected creation and sealing evidence".into());
            }
        }
    }
    Ok(operation)
}

pub fn cleanup_candidates(
    store: &crate::FilesystemTransactionStore,
    installation: &InstallationRecord,
    retiring: Option<&zup_plan::RetentionChange>,
) -> Result<Vec<CleanupOperation>, String> {
    observe_cleanup(store, installation, retiring, None)
}

fn observe_cleanup(
    store: &crate::FilesystemTransactionStore,
    installation: &InstallationRecord,
    retiring: Option<&zup_plan::RetentionChange>,
    publishing: Option<zup_core::TransactionId>,
) -> Result<Vec<CleanupOperation>, String> {
    installation.validate().map_err(|error| error.to_string())?;
    if let Some(selection) = retiring {
        selection.validate_owned(installation)?;
    }
    let mut journals = Vec::new();
    let mut compensated = Vec::new();
    for id in store.list_ids().map_err(|error| error.to_string())? {
        let record = store.load(&id).map_err(|error| error.to_string())?;
        if publishing == Some(record.transaction_id) {
            continue;
        }
        if record.app_id == installation.app_id
            && record.scope == installation.scope
            && record.target == installation.target
            && record.plan.installation == installation.id
        {
            if record.phase != TransactionPhase::Committed
                && record.phase != TransactionPhase::RolledBack
                || record.phase == TransactionPhase::Committed && !record.cleanup_complete()
            {
                return Err(format!(
                    "transaction {} requires recovery before cleanup planning",
                    record.transaction_id
                ));
            }
            if record.phase == TransactionPhase::Committed {
                journals.push(record);
            } else {
                compensated.push(record);
            }
        }
    }
    let retired = installation
        .retired_deployments
        .iter()
        .map(|retired| &retired.snapshot)
        .chain(installation.retained.iter().filter(|snapshot| {
            retiring.is_some_and(|selection| {
                selection
                    .retire
                    .iter()
                    .any(|selected| selected.revision == snapshot.revision)
            })
        }))
        .collect::<Vec<_>>();
    let live = std::iter::once(&installation.current)
        .chain(
            installation
                .retained
                .iter()
                .filter(|snapshot| !retired.contains(snapshot)),
        )
        .collect::<Vec<_>>();
    let mut candidates = std::collections::BTreeMap::new();
    for snapshot in &retired {
        for spec in snapshot.materialization.iter().chain(&snapshot.maintenance) {
            if live.iter().any(|snapshot| {
                snapshot
                    .materialization
                    .iter()
                    .chain(&snapshot.maintenance)
                    .any(|live| live.id() == spec.id())
            }) {
                continue;
            }
            let readers = if retired.iter().any(|snapshot| {
                snapshot.readers_for(spec) == Some(zup_core::GenerationReaders::Untracked)
            }) {
                zup_core::GenerationReaders::Untracked
            } else {
                zup_core::GenerationReaders::NativeExecutables
            };
            let (creator, tree, seal) = journals
                .iter()
                .find_map(|record| {
                    record.plan.nodes.iter().find_map(|node| {
                        match (&node.kind, record.receipt(&node.id)) {
                            (
                                NodeKind::Generation {
                                    operation: zup_plan::GenerationOperation::Seal,
                                    intent,
                                },
                                Some(OperationReceipt::Generation(receipt)),
                            ) if &intent.materialization == spec => match receipt.as_ref() {
                                crate::GenerationReceipt::Sealed { tree, seal, .. } => {
                                    Some((record.transaction_id, tree.clone(), *seal))
                                }
                                _ => None,
                            },
                            _ => None,
                        }
                    })
                })
                .ok_or("retired generation is missing protected sealing evidence")?;
            let operation = CleanupOperation {
                privilege: installation.scope.authorization(),
                candidate: CleanupCandidate::Generation(zup_plan::GenerationCleanup {
                    source: zup_plan::GenerationRetirement::Snapshot {
                        selection: (*snapshot).into(),
                    },
                    materialization: spec.clone(),
                    creator,
                    tree,
                    seal,
                    readers,
                }),
            };
            operation.validate_owned(installation, retiring)?;
            candidates.entry(operation.id()).or_insert(operation);
        }
        for (key, owned) in &snapshot.resources {
            let zup_plan::OwnedResource::File {
                instance,
                size,
                sha256,
                ..
            } = owned
            else {
                continue;
            };
            if live.iter().any(|snapshot| snapshot.resources.values().any(|owned| matches!(owned, zup_plan::OwnedResource::File { instance: live, .. } if live.object == instance.object))) { continue; }
            let mut source_found = false;
            for record in &journals {
                for node in &record.plan.nodes {
                    let path = match (&node.kind, record.receipt(&node.id)) {
                        (
                            NodeKind::FileMutation { key: candidate, .. },
                            Some(OperationReceipt::ReplaceFile {
                                previous_object,
                                previous_size,
                                previous_sha256,
                                backup_path,
                                ..
                            }),
                        ) if candidate == key
                            && previous_object == &instance.object
                            && previous_size == size
                            && previous_sha256 == sha256 =>
                        {
                            Some(
                                zup_core::TargetPath::new(record.target, backup_path)
                                    .map_err(|error| error.to_string())?,
                            )
                        }
                        (
                            NodeKind::FileRemoval { key: candidate },
                            Some(OperationReceipt::RemoveFile {
                                object,
                                size: previous_size,
                                sha256: previous_sha256,
                                backup_path,
                                ..
                            }),
                        ) if candidate == key
                            && object == &instance.object
                            && previous_size == size
                            && previous_sha256 == sha256 =>
                        {
                            Some(backup_path.clone())
                        }
                        _ => None,
                    };
                    if let Some(path) = path {
                        source_found = true;
                        let operation = CleanupOperation {
                            privilege: installation.scope.authorization(),
                            candidate: CleanupCandidate::Fixed(zup_plan::FixedCleanup {
                                key: key.clone(),
                                path,
                                retained_by: record.transaction_id,
                                operation: node.id.clone(),
                                instance: *instance,
                                size: *size,
                                sha256: *sha256,
                            }),
                        };
                        operation.validate_owned(installation, retiring)?;
                        candidates.entry(operation.id()).or_insert(operation);
                    }
                }
            }
            if !source_found {
                return Err("retired fixed instance is missing its protected displacement receipt; ownership must be retained".into());
            }
        }
    }
    for source in &compensated {
        for node in &source.plan.nodes {
            let (
                NodeKind::Generation {
                    operation: zup_plan::GenerationOperation::Seal,
                    intent,
                },
                Some(OperationReceipt::Generation(receipt)),
            ) = (&node.kind, source.receipt(&node.id))
            else {
                continue;
            };
            let crate::GenerationReceipt::Sealed { tree, seal, .. } = receipt.as_ref() else {
                continue;
            };
            let rooted = std::iter::once(&installation.current)
                .chain(&installation.retained)
                .chain(
                    installation
                        .retired_deployments
                        .iter()
                        .map(|retired| &retired.snapshot),
                )
                .any(|snapshot| {
                    snapshot
                        .materialization
                        .iter()
                        .chain(&snapshot.maintenance)
                        .any(|spec| spec.id() == intent.materialization.id())
                });
            if rooted {
                continue;
            }
            let readers = if matches!(
                intent.publication,
                zup_plan::GenerationPublication::Maintenance
            ) && source.plan.preset.is_none()
            {
                zup_core::GenerationReaders::NativeExecutables
            } else {
                source.plan.requirements.generation_readers
            };
            let operation = CleanupOperation {
                privilege: installation.scope.authorization(),
                candidate: CleanupCandidate::Generation(zup_plan::GenerationCleanup {
                    source: zup_plan::GenerationRetirement::Compensated {
                        transaction: source.transaction_id,
                        plan_hash: source.plan_hash,
                    },
                    materialization: intent.materialization.clone(),
                    creator: source.transaction_id,
                    tree: tree.clone(),
                    seal: *seal,
                    readers,
                }),
            };
            operation.validate_owned(installation, retiring)?;
            candidates.entry(operation.id()).or_insert(operation);
        }
    }
    candidates.retain(|_, candidate| {
        !journals.iter().any(|journal| {
            journal.plan.nodes.iter().any(|node| {
                let NodeKind::Cleanup { operation } = &node.kind else {
                    return false;
                };
                let Some(OperationReceipt::Cleanup {
                    completion:
                        crate::CleanupCompletion::Removed | crate::CleanupCompletion::AlreadyAbsent,
                    intent,
                }) = journal.receipt(&node.id)
                else {
                    return false;
                };
                if *intent != operation.fingerprint() {
                    return false;
                }
                match (&candidate.candidate, &operation.candidate) {
                    (
                        CleanupCandidate::Generation(candidate),
                        CleanupCandidate::Generation(collected),
                    ) => {
                        candidate.materialization == collected.materialization
                            && candidate.tree == collected.tree
                            && candidate.seal == collected.seal
                    }
                    (CleanupCandidate::Fixed(candidate), CleanupCandidate::Fixed(collected)) => {
                        candidate == collected
                    }
                    _ => false,
                }
            })
        })
    });
    Ok(candidates.into_values().collect())
}

pub fn collected_deployments(
    installation: &InstallationRecord,
    remaining: &[CleanupOperation],
) -> Vec<zup_plan::RetiredDeploymentSelection> {
    installation.retired_deployments.iter().filter(|retired| !remaining.iter().any(|operation| match &operation.candidate {
        CleanupCandidate::Generation(generation) => retired.snapshot.materialization.iter().chain(&retired.snapshot.maintenance).any(|spec| spec.id() == generation.materialization.id()),
        CleanupCandidate::Fixed(file) => retired.snapshot.resources.values().any(|resource| matches!(resource, zup_plan::OwnedResource::File { instance, .. } if instance.object == file.instance.object)),
        CleanupCandidate::History(_) => false,
    })).map(|retired| (&retired.snapshot).into()).collect()
}

pub fn validate_retention(
    store: &crate::FilesystemTransactionStore,
    installation: &InstallationRecord,
    selection: &zup_plan::RetentionChange,
    publishing: &TransactionRecord,
) -> Result<(), String> {
    if publishing.phase != TransactionPhase::Applying || publishing.plan.installation != installation.id
        || publishing.plan.observed_transaction != Some(installation.commit.transaction)
        || !publishing.plan.nodes.iter().any(|node| matches!(&node.kind, NodeKind::Retention { selection: expected } if expected.as_ref() == selection))
        || store.load(&publishing.transaction_id).map_err(|error| error.to_string())? != *publishing {
        return Err("retention publication lacks its protected current transaction".into());
    }
    selection.validate_owned(installation)?;
    if !selection.forget.is_empty() {
        let remaining =
            observe_cleanup(store, installation, None, Some(publishing.transaction_id))?;
        let collected = collected_deployments(installation, &remaining);
        if selection
            .forget
            .iter()
            .any(|selected| !collected.contains(selected))
        {
            return Err("retired ownership cannot be forgotten before all unreferenced payloads have verified collection receipts".into());
        }
    }
    Ok(())
}
