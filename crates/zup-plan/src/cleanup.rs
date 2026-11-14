use serde::{Deserialize, Serialize};
use zup_core::{
    FixedFileInstance, GenerationReaders, MaterializationSpec, Privilege, ResourceKey,
    Sha256Digest, Target, TransactionId,
};

use crate::{InstallationRecord, OperationId, RetentionChange, RetiredDeploymentSelection};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum GenerationRetirement {
    Snapshot {
        selection: RetiredDeploymentSelection,
    },
    Compensated {
        transaction: TransactionId,
        plan_hash: Sha256Digest,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GenerationCleanup {
    pub source: GenerationRetirement,
    pub materialization: MaterializationSpec,
    pub creator: TransactionId,
    pub tree: zup_core::MaterializationTree,
    pub seal: Sha256Digest,
    pub readers: GenerationReaders,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixedCleanup {
    pub key: ResourceKey,
    pub path: zup_core::TargetPath,
    pub retained_by: TransactionId,
    pub operation: OperationId,
    pub instance: FixedFileInstance,
    pub size: u64,
    pub sha256: Sha256Digest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CleanupCandidate {
    Generation(GenerationCleanup),
    Fixed(FixedCleanup),
    History(crate::HistoryCleanup),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CleanupOperation {
    pub candidate: CleanupCandidate,
    pub privilege: Privilege,
}

impl CleanupOperation {
    pub fn id(&self) -> OperationId {
        let identity = match &self.candidate {
            CleanupCandidate::Generation(generation) => {
                format!("generation:{}", generation.materialization.id())
            }
            CleanupCandidate::Fixed(file) => format!(
                "fixed:{}",
                zup_core::hash_bytes(
                    &serde_json::to_vec(&(file.retained_by, &file.operation, file.instance))
                        .expect("fixed cleanup identity serializes")
                )
            ),
            CleanupCandidate::History(history) => format!(
                "history:{}:{}",
                history.source,
                match &history.object {
                    crate::HistoryObject::Journal { .. } => "journal".to_owned(),
                    crate::HistoryObject::File { target, .. } => zup_core::hash_bytes(
                        &serde_json::to_vec(target).expect("history target serializes")
                    )
                    .to_hex(),
                }
            ),
        };
        OperationId::new(format!("collect:{identity}"))
    }

    pub fn fingerprint(&self) -> Sha256Digest {
        let mut bytes = b"zup:cleanup:1\0".to_vec();
        bytes.extend(serde_json::to_vec(self).expect("cleanup operation serializes"));
        zup_core::hash_bytes(&bytes)
    }

    pub fn validate(&self, target: Target) -> Result<(), String> {
        match &self.candidate {
            CleanupCandidate::History(history) => history.validate(target),
            CleanupCandidate::Generation(generation)
                if generation.materialization.descriptor().target() == target
                    && generation.tree.root.operating_system() == target.operating_system() =>
            {
                generation
                    .tree
                    .validate(generation.materialization.descriptor())
            }
            CleanupCandidate::Fixed(file)
                if matches!(file.key, ResourceKey::File { .. })
                    && file.path.target() == &target
                    && file.key.canonical_for(&target) == file.key
                    && file.instance.object.operating_system() == target.operating_system() =>
            {
                Ok(())
            }
            _ => {
                Err("cleanup candidate differs from its native target or resource identity".into())
            }
        }
    }

    pub fn validate_owned(
        &self,
        record: &InstallationRecord,
        retiring: Option<&RetentionChange>,
    ) -> Result<(), String> {
        self.validate(record.target)?;
        if self.privilege != record.scope.authorization() {
            return Err("cleanup authority differs from its installation".into());
        }
        let retained = |snapshot: &crate::DeploymentSnapshot| {
            retiring.is_none_or(|selection| {
                !selection
                    .retire
                    .iter()
                    .any(|selected| selected.revision == snapshot.revision)
            })
        };
        match &self.candidate {
            CleanupCandidate::History(history) => {
                let mut projected = record.clone();
                if let Some(selection) = retiring {
                    projected
                        .retired_deployments
                        .retain(|retired| !selection.forget.contains(&(&retired.snapshot).into()));
                }
                history.validate_owned(&projected)
            }
            CleanupCandidate::Generation(generation) => {
                if let GenerationRetirement::Snapshot { selection } = &generation.source {
                    let source = record
                        .retired_deployments
                        .iter()
                        .map(|retired| &retired.snapshot)
                        .chain(
                            record
                                .retained
                                .iter()
                                .filter(|snapshot| !retained(snapshot)),
                        )
                        .find(|snapshot| RetiredDeploymentSelection::from(*snapshot) == *selection)
                        .ok_or("generation cleanup has no authenticated retired snapshot")?;
                    if !source
                        .materialization
                        .iter()
                        .chain(&source.maintenance)
                        .any(|spec| spec == &generation.materialization)
                    {
                        return Err("generation cleanup differs from its retired snapshot".into());
                    }
                }
                let contains = |snapshot: &crate::DeploymentSnapshot| {
                    snapshot
                        .materialization
                        .iter()
                        .chain(&snapshot.maintenance)
                        .any(|spec| spec.id() == generation.materialization.id())
                };
                if generation.materialization.installation() != record.id
                    || contains(&record.current)
                    || record
                        .retained
                        .iter()
                        .filter(|snapshot| retained(snapshot))
                        .any(contains)
                {
                    return Err(
                        "generation cleanup would remove active, retained, or mismatched content"
                            .into(),
                    );
                }
                if let GenerationRetirement::Compensated { transaction, .. } = &generation.source {
                    if *transaction != generation.creator
                        || record
                            .retired_deployments
                            .iter()
                            .any(|retired| contains(&retired.snapshot))
                        || record.retained.iter().any(contains)
                    {
                        return Err(
                            "compensated generation remains rooted by installation history".into(),
                        );
                    }
                    return Ok(());
                }
                let untracked = record
                    .retired_deployments
                    .iter()
                    .map(|retired| &retired.snapshot)
                    .chain(
                        record
                            .retained
                            .iter()
                            .filter(|snapshot| !retained(snapshot)),
                    )
                    .any(|snapshot| {
                        snapshot.readers_for(&generation.materialization)
                            == Some(GenerationReaders::Untracked)
                    });
                if generation.readers
                    != if untracked {
                        GenerationReaders::Untracked
                    } else {
                        GenerationReaders::NativeExecutables
                    }
                {
                    return Err("cleanup cannot strengthen the recorded reader contracts".into());
                }
                Ok(())
            }
            CleanupCandidate::Fixed(file) => {
                let owns = |snapshot: &crate::DeploymentSnapshot| {
                    snapshot.resources.values().any(|resource| matches!(resource, crate::OwnedResource::File { instance, .. } if instance.object == file.instance.object))
                };
                if owns(&record.current)
                    || record
                        .retained
                        .iter()
                        .filter(|snapshot| retained(snapshot))
                        .any(owns)
                {
                    return Err(
                        "fixed cleanup would remove an active or retained native instance".into(),
                    );
                }
                if !record.retired_deployments.iter().map(|retired| &retired.snapshot)
                    .chain(record.retained.iter().filter(|snapshot| !retained(snapshot)))
                    .any(|snapshot| matches!(snapshot.resources.get(&file.key), Some(crate::OwnedResource::File { instance, size, sha256, .. }) if instance == &file.instance && *size == file.size && *sha256 == file.sha256)) {
                    return Err("fixed cleanup has no authenticated retired native instance".into());
                }
                Ok(())
            }
        }
    }
}
