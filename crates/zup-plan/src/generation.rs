use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use zup_core::{
    ActivationRevision, MaterializationRef, MaterializationSpec, Privilege, ResourceKey, Target,
};

use crate::{
    Dependency, DeploymentPlan, DeploymentPlanError, NodeKind, NodeMeta, OperationId, Phase,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GenerationPreparation {
    Materialize,
    ReuseVerified,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GenerationOperation {
    Materialize,
    Seal,
    Verify,
    Activate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActivationSelection {
    pub revision: ActivationRevision,
    pub materialization: Option<MaterializationRef>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum GenerationPublication {
    Application {
        previous: Option<ActivationSelection>,
        revision: ActivationRevision,
    },
    Maintenance,
}

impl GenerationPublication {
    pub const fn previous(self) -> Option<ActivationSelection> {
        match self {
            Self::Application { previous, .. } => previous,
            Self::Maintenance => None,
        }
    }

    pub const fn selection(
        self,
        materialization: zup_core::MaterializationRef,
    ) -> Option<ActivationSelection> {
        match self {
            Self::Application { revision, .. } => Some(ActivationSelection {
                revision,
                materialization: Some(materialization),
            }),
            Self::Maintenance => None,
        }
    }

    pub const fn namespace(self) -> &'static str {
        match self {
            Self::Application { .. } => zup_core::DEPLOYMENTS_DIRECTORY,
            Self::Maintenance => zup_core::MAINTENANCE_DEPLOYMENTS_DIRECTORY,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GenerationSource {
    pub key: ResourceKey,
    pub source_relative: zup_core::RelativePath,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GenerationIntent {
    pub sources: BTreeMap<zup_core::RelativePath, GenerationSource>,
    pub preparation: GenerationPreparation,
    pub materialization: MaterializationSpec,
    pub publication: GenerationPublication,
    pub privilege: Privilege,
    pub exposures: BTreeSet<ResourceKey>,
}

impl GenerationIntent {
    pub fn validate(&self, target: Target) -> Result<(), DeploymentPlanError> {
        let invalid = |reason: &str| DeploymentPlanError::InvalidNode {
            id: self.materialization.id().to_string(),
            reason: reason.into(),
        };
        if self.materialization.descriptor().target() != target {
            return Err(invalid("generation target does not match deployment"));
        }
        if self.sources.values().any(|source| match self.publication {
            GenerationPublication::Application { .. } => !matches!(
                source.key,
                ResourceKey::File {
                    id: zup_core::FileResourceId::Payload { .. }
                }
            ),
            GenerationPublication::Maintenance => !matches!(
                source.key,
                ResourceKey::File {
                    id: zup_core::FileResourceId::Maintenance { .. }
                }
            ),
        }) {
            return Err(invalid(
                "generation source identity does not match its publication purpose",
            ));
        }
        match self.preparation {
            GenerationPreparation::Materialize => {
                let expected: BTreeSet<_> = self
                    .materialization
                    .descriptor()
                    .files()
                    .iter()
                    .map(|file| &file.path)
                    .collect();
                if self.sources.keys().collect::<BTreeSet<_>>() != expected {
                    return Err(invalid(
                        "generation sources do not cover the declared content",
                    ));
                }
                if self
                    .sources
                    .values()
                    .map(|source| &source.key)
                    .collect::<BTreeSet<_>>()
                    .len()
                    != self.sources.len()
                {
                    return Err(invalid("generation repeats a logical payload identity"));
                }
                for member in self.materialization.descriptor().files() {
                    if let ResourceKey::File {
                        id: zup_core::FileResourceId::Maintenance { artifact },
                    } = &self.sources[&member.path].key
                        && (artifact.member_path(target, member.sha256) != member.path
                            || member.executable
                                != !matches!(artifact, zup_core::MaintenanceArtifact::Asset { .. }))
                    {
                        return Err(invalid(
                            "maintenance artifact identity and content placement disagree",
                        ));
                    }
                }
            }
            GenerationPreparation::ReuseVerified if !self.sources.is_empty() => {
                return Err(invalid(
                    "retained generation must not acquire replacement sources",
                ));
            }
            _ => {}
        }
        if let GenerationPublication::Application { previous, revision } = self.publication {
            let expected = match previous {
                None => ActivationRevision::FIRST,
                Some(previous) => previous
                    .revision
                    .next()
                    .map_err(|_| invalid("activation revision exhausted"))?,
            };
            if revision != expected {
                return Err(invalid(
                    "activation revision does not follow observed state",
                ));
            }
            match self.preparation {
                GenerationPreparation::Materialize
                    if previous
                        .and_then(|previous| previous.materialization)
                        .is_some_and(|previous| previous.id == self.materialization.id()) =>
                {
                    return Err(invalid(
                        "committed materialization cannot be reconstructed in place",
                    ));
                }
                GenerationPreparation::ReuseVerified if previous.is_none() => {
                    return Err(invalid(
                        "retained generation requires an existing installation",
                    ));
                }
                _ => {}
            }
        }
        Ok(())
    }

    pub fn operations(&self) -> &'static [GenerationOperation] {
        use GenerationOperation::*;
        match (self.preparation, self.publication) {
            (GenerationPreparation::Materialize, GenerationPublication::Application { .. }) => {
                &[Materialize, Seal, Activate]
            }
            (GenerationPreparation::ReuseVerified, GenerationPublication::Application { .. }) => {
                &[Verify, Activate]
            }
            (GenerationPreparation::Materialize, GenerationPublication::Maintenance) => {
                &[Materialize, Seal]
            }
            (GenerationPreparation::ReuseVerified, GenerationPublication::Maintenance) => &[Verify],
        }
    }

    pub const fn ready_operation(&self) -> GenerationOperation {
        match self.preparation {
            GenerationPreparation::Materialize => GenerationOperation::Seal,
            GenerationPreparation::ReuseVerified => GenerationOperation::Verify,
        }
    }

    pub fn operation_id(&self, operation: GenerationOperation) -> OperationId {
        let token = match operation {
            GenerationOperation::Materialize => "materialize",
            GenerationOperation::Seal => "seal",
            GenerationOperation::Verify => "verify",
            GenerationOperation::Activate => "activate",
        };
        OperationId::new(format!("generation:{token}:{}", self.materialization.id()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlannedGeneration<P> {
    pub installation: zup_core::InstallationId,
    pub id: zup_core::MaterializationId,
    pub descriptor: zup_core::GenerationDescriptor,
    pub directory: P,
    pub publication: GenerationPublication,
    pub preparation: GenerationPreparation,
    pub sources: BTreeMap<zup_core::RelativePath, GenerationSource>,
}

impl<P> PlannedGeneration<P> {
    pub fn resolve_directory<Q, E>(
        &self,
        resolve: impl FnOnce(&P) -> Result<Q, E>,
    ) -> Result<PlannedGeneration<Q>, E> {
        Ok(PlannedGeneration {
            installation: self.installation,
            id: self.id,
            descriptor: self.descriptor.clone(),
            directory: resolve(&self.directory)?,
            publication: self.publication,
            preparation: self.preparation,
            sources: self.sources.clone(),
        })
    }
}

impl PlannedGeneration<zup_core::TargetPath> {
    pub fn maintenance_runtime(&self) -> Option<zup_core::TargetPath> {
        let member = format!(
            "maintenance{}",
            self.descriptor.target().executable_suffix()
        );
        (self.publication == GenerationPublication::Maintenance
            && self
                .descriptor
                .files()
                .iter()
                .any(|file| file.path.as_str() == member && file.executable))
        .then(|| self.directory.join(member).ok())
        .flatten()
    }

    pub fn bind(
        &self,
        privilege: Privilege,
        exposures: BTreeSet<ResourceKey>,
    ) -> Result<GenerationIntent, zup_core::GenerationError> {
        Ok(GenerationIntent {
            sources: match self.preparation {
                GenerationPreparation::Materialize => self.sources.clone(),
                GenerationPreparation::ReuseVerified => BTreeMap::new(),
            },
            preparation: self.preparation,
            materialization: MaterializationSpec::new(
                self.installation,
                self.id,
                self.directory.clone(),
                self.descriptor.clone(),
            )?,
            publication: self.publication,
            privilege,
            exposures,
        })
    }
}

pub fn validate_maintenance_content(
    materialization: &MaterializationSpec,
    preset: Option<&zup_core::InstalledPreset>,
) -> Result<(), crate::ContractError> {
    use zup_core::MaintenanceArtifact;
    let descriptor = materialization.descriptor();
    let invalid = || {
        crate::ContractError::Capability(
            "maintenance generation does not match its declared runtime and preset".into(),
        )
    };
    let mut expected = BTreeSet::new();
    let runtime = descriptor
        .files()
        .iter()
        .find(|file| {
            file.path == MaintenanceArtifact::Runtime.member_path(descriptor.target(), file.sha256)
                && file.executable
        })
        .ok_or_else(invalid)?;
    expected.insert(runtime.path.clone());
    if let Some(preset) = preset {
        let path = MaintenanceArtifact::Preset.member_path(descriptor.target(), preset.executable);
        if !descriptor
            .files()
            .iter()
            .any(|file| file.path == path && file.sha256 == preset.executable && file.executable)
        {
            return Err(invalid());
        }
        expected.insert(path);
        for asset in &preset.preset.assets {
            let path = MaintenanceArtifact::Asset {
                name: zup_core::RelativePath::new(asset.name.as_str()).map_err(|_| invalid())?,
            }
            .member_path(descriptor.target(), asset.sha256);
            if !descriptor.files().iter().any(|file| {
                file.path == path
                    && file.sha256 == asset.sha256
                    && file.size == asset.size
                    && !file.executable
            }) || !expected.insert(path)
            {
                return Err(invalid());
            }
        }
    }
    if expected.len() != descriptor.files().len() {
        return Err(invalid());
    }
    Ok(())
}

impl GenerationOperation {
    pub const fn phase(self) -> Phase {
        match self {
            Self::Activate => Phase::Activation,
            _ => Phase::GenerationPreparation,
        }
    }
}

pub(crate) fn validate_generation_chain(plan: &DeploymentPlan) -> Result<(), DeploymentPlanError> {
    let mut groups = BTreeMap::<_, Vec<_>>::new();
    for node in &plan.nodes {
        if let NodeKind::Generation { operation, intent } = &node.kind {
            groups
                .entry(intent.materialization.id())
                .or_default()
                .push((node, *operation, intent.as_ref()));
        }
    }
    let mut purposes = std::collections::HashSet::new();
    if plan.preset.is_some()
        && plan.transition.is_deploy()
        && !groups
            .values()
            .any(|nodes| nodes[0].2.publication == GenerationPublication::Maintenance)
    {
        return Err(DeploymentPlanError::InvalidOrder);
    }
    for nodes in groups.values() {
        let intent = nodes[0].2;
        if !purposes.insert(std::mem::discriminant(&intent.publication)) {
            return Err(DeploymentPlanError::InvalidOrder);
        }
        validate_generation_intent(plan, nodes, intent)?;
    }
    Ok(())
}

fn validate_generation_intent(
    plan: &DeploymentPlan,
    nodes: &[(
        &crate::DeploymentNode,
        GenerationOperation,
        &GenerationIntent,
    )],
    intent: &GenerationIntent,
) -> Result<(), DeploymentPlanError> {
    intent.validate(plan.target)?;
    if intent.publication == GenerationPublication::Maintenance {
        validate_maintenance_content(&intent.materialization, plan.preset.as_ref())
            .map_err(|_| DeploymentPlanError::InvalidOrder)?;
    }
    let expected_directory = plan
        .install_directory
        .as_ref()
        .ok_or(DeploymentPlanError::InvalidOrder)?
        .join(format!(
            "{}/{}",
            intent.publication.namespace(),
            intent.materialization.id()
        ))
        .map_err(|_| DeploymentPlanError::InvalidOrder)?;
    if intent.materialization.installation() != plan.installation
        || matches!(intent.publication, GenerationPublication::Application { previous, .. } if previous != plan.observed)
        || !intent
            .materialization
            .directory()
            .equivalent(&expected_directory)
    {
        return Err(DeploymentPlanError::InvalidOrder);
    }
    if nodes.len() != intent.operations().len() {
        return Err(DeploymentPlanError::InvalidOrder);
    }
    let edge =
        |from: OperationId, to: OperationId| plan.dependencies.contains(&Dependency { from, to });
    for operation in intent.operations() {
        let expected_id = intent.operation_id(*operation);
        let Some((node, _, other)) = nodes.iter().find(|(_, action, _)| action == operation) else {
            return Err(DeploymentPlanError::InvalidOrder);
        };
        if *other != intent
            || node.id != expected_id
            || node.phase != operation.phase()
            || node.meta
                != (NodeMeta {
                    privilege: Some(intent.privilege),
                    ..Default::default()
                })
        {
            return Err(DeploymentPlanError::InvalidOrder);
        }
    }
    for pair in intent.operations().windows(2) {
        if !edge(intent.operation_id(pair[0]), intent.operation_id(pair[1])) {
            return Err(DeploymentPlanError::InvalidOrder);
        }
    }
    let first = intent.operation_id(intent.operations()[0]);
    let ready = intent.operation_id(intent.ready_operation());
    let activate = match intent.publication {
        GenerationPublication::Application { .. } => {
            intent.operation_id(GenerationOperation::Activate)
        }
        GenerationPublication::Maintenance => OperationId::new(OperationId::VERIFY),
    };
    for node in &plan.nodes {
        if matches!(node.kind, NodeKind::NativeCache { .. })
            && !edge(node.id.clone(), activate.clone())
        {
            return Err(DeploymentPlanError::InvalidOrder);
        }
    }
    if !edge(OperationId::new(OperationId::COMMIT_INTENT), first)
        || if activate.as_str() == OperationId::VERIFY {
            !edge(ready.clone(), activate.clone())
        } else {
            !edge(activate.clone(), OperationId::new(OperationId::VERIFY))
        }
    {
        return Err(DeploymentPlanError::InvalidOrder);
    }
    for key in &intent.exposures {
        let effects: Vec<_> = plan
            .nodes
            .iter()
            .filter(|node| exposure_key(&node.kind) == Some(key))
            .collect();
        if effects.is_empty() {
            return Err(DeploymentPlanError::UnknownDependency {
                id: format!("{key:?}"),
            });
        }
        for effect in effects {
            if !edge(ready.clone(), effect.id.clone()) || !edge(effect.id.clone(), activate.clone())
            {
                return Err(DeploymentPlanError::InvalidOrder);
            }
        }
    }
    Ok(())
}

pub(crate) fn exposure_key(kind: &NodeKind) -> Option<&ResourceKey> {
    match kind {
        NodeKind::FileMutation { key, .. } | NodeKind::BackendOperation { key, .. } => Some(key),
        NodeKind::NativeService { operation }
            if matches!(
                operation.as_ref(),
                crate::NativeServiceOperation::Configure { .. }
            ) =>
        {
            Some(operation.key())
        }
        NodeKind::ServiceControl { operation }
            if operation.stage == crate::ServiceControlStage::Restore =>
        {
            Some(&operation.key)
        }
        _ => None,
    }
}
