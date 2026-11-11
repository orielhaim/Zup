use crate::RemovalKind;
use std::collections::{BTreeMap, BTreeSet};

use miette::Diagnostic;
use petgraph::algo::is_cyclic_directed;
use petgraph::graph::DiGraph;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use zup_core::TargetPath;
use zup_core::{
    ComponentId, InstalledPreset, Privilege, RelativePath, ResourceKey, Sha256Digest, Target,
};

use crate::effect_input::{
    BackendOperation, BackendOperationIntent, DeploymentEffects, DeploymentEffectsError,
    FileRemoval,
};
use crate::operation_id::OperationId;
use crate::{FileOperation, FileOperationKind, FilePrecondition};

pub const DEPLOYMENT_PLAN_SCHEMA: u32 = 1;

#[derive(Debug, Error, Diagnostic)]
pub enum DeploymentPlanError {
    #[error("transaction input is invalid: {0}")]
    #[diagnostic(code(zup_plan::invalid_input))]
    InvalidInput(#[from] DeploymentEffectsError),

    #[error("unresolved transaction conflict: {reason}")]
    #[diagnostic(code(zup_plan::plan_conflict))]
    UnresolvedConflict { reason: String },

    #[error("duplicate operation id `{id}`")]
    #[diagnostic(code(zup_plan::duplicate_operation_id))]
    DuplicateOperationId { id: String },

    #[error("dependency references unknown operation `{id}`")]
    #[diagnostic(code(zup_plan::unknown_dependency))]
    UnknownDependency { id: String },

    #[error("operation `{id}` depends on itself")]
    #[diagnostic(code(zup_plan::self_dependency))]
    SelfDependency { id: String },

    #[error("transaction graph contains a cycle")]
    #[diagnostic(code(zup_plan::cycle))]
    Cycle,

    #[error("invalid transaction node `{id}`: {reason}")]
    #[diagnostic(code(zup_plan::invalid_node))]
    InvalidNode { id: String, reason: String },

    #[error("invalid transaction execution or rollback order")]
    #[diagnostic(code(zup_plan::invalid_order))]
    InvalidOrder,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Begin,
    Stage,
    Preflight,
    CommitIntent,
    GenerationPreparation,
    ServiceQuiesce,
    FileMutation,
    Backend,
    ServiceRestore,
    Verify,
    Activation,
    Retention,
    Commit,
    Cleanup,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum NodeKind {
    Cleanup {
        operation: Box<crate::CleanupOperation>,
    },
    Retention {
        selection: Box<crate::RetentionChange>,
    },
    PersistentDirectory {
        operation: Box<crate::PersistentDirectoryOperation>,
    },
    ReserveInstallation,
    PublishInstallation,
    Generation {
        operation: crate::GenerationOperation,
        intent: Box<crate::GenerationIntent>,
    },
    Barrier,
    ServiceControl {
        operation: Box<crate::ServiceControlOperation>,
    },
    NativeService {
        operation: Box<crate::NativeServiceOperation>,
    },
    StageFile {
        key: ResourceKey,
    },
    RetainFile {
        key: ResourceKey,
    },
    NativeCache {
        operation: Box<crate::NativeCacheOperation>,
    },
    FileMutation {
        key: ResourceKey,
        delta: FileOperationKind,
    },
    FileRemoval {
        key: ResourceKey,
    },
    BackendOperation {
        key: ResourceKey,
        intent: BackendOperationIntent,
    },
    BackendRemoval {
        key: ResourceKey,
    },
}

impl NodeKind {
    pub fn is_post_commit(&self) -> bool {
        matches!(self, Self::Cleanup { .. })
    }
    pub fn is_barrier(&self) -> bool {
        matches!(self, Self::Barrier)
    }

    pub fn requires_verification(&self) -> bool {
        matches!(
            self,
            Self::Cleanup { .. }
                | Self::Generation { .. }
                | Self::PersistentDirectory { .. }
                | Self::RetainFile { .. }
                | Self::NativeCache { .. }
                | Self::FileMutation { .. }
                | Self::ServiceControl { .. }
                | Self::NativeService { .. }
                | Self::FileRemoval { .. }
                | Self::BackendOperation { .. }
                | Self::BackendRemoval { .. }
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeploymentNode {
    pub id: OperationId,
    pub phase: Phase,
    pub kind: NodeKind,
    pub declaration_order: u32,
    pub meta: NodeMeta,
}

impl DeploymentNode {
    pub fn file_destination(&self) -> Option<&TargetPath> {
        match &self.kind {
            NodeKind::StageFile { .. }
            | NodeKind::RetainFile { .. }
            | NodeKind::FileMutation { .. } => self.meta.destination.as_ref(),
            NodeKind::FileRemoval { .. } => self
                .meta
                .removal
                .as_ref()
                .map(|removal| &removal.destination),
            _ => None,
        }
    }

    pub fn mutates_file(&self, path: &TargetPath) -> bool {
        matches!(
            self.kind,
            NodeKind::FileMutation { .. } | NodeKind::FileRemoval { .. }
        ) && self
            .file_destination()
            .is_some_and(|destination| destination.equivalent(path))
    }

    pub fn work(&self) -> u64 {
        match &self.kind {
            NodeKind::Generation { operation, intent }
                if *operation != crate::GenerationOperation::Activate =>
            {
                intent.materialization.descriptor().size().max(1)
            }
            NodeKind::StageFile { .. } | NodeKind::FileMutation { .. } => {
                self.meta.expected_size.unwrap_or(1).max(1)
            }
            _ => 1,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct NodeMeta {
    pub destination: Option<TargetPath>,
    pub source_relative: Option<RelativePath>,
    pub file_precondition: Option<FilePrecondition>,
    pub fixed_replacement: Option<zup_core::FixedReplacement>,
    pub expected_sha256: Option<Sha256Digest>,
    pub expected_size: Option<u64>,
    pub privilege: Option<Privilege>,
    pub executable: Option<bool>,
    pub backend: Option<BackendOperation>,
    pub removal: Option<FileRemoval>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dependency {
    pub from: OperationId,
    pub to: OperationId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DeploymentAudit {
    pub files_unchanged: usize,
    pub backend_unchanged: usize,
    pub drifted_removals: usize,
    pub noop_total: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeploymentPlan {
    pub requirements: crate::DeploymentRequirements,
    pub installation: zup_core::InstallationId,
    pub observed: Option<crate::ActivationSelection>,
    pub schema: u32,
    pub target: Target,
    pub selected_components: Vec<ComponentId>,
    pub install_directory: Option<TargetPath>,
    pub transition: crate::DeploymentTransition,
    pub retired_keys: Vec<ResourceKey>,
    pub nodes: Vec<DeploymentNode>,
    pub dependencies: Vec<Dependency>,
    pub audit: DeploymentAudit,
    pub execution_order: Vec<OperationId>,
    pub rollback_order: Vec<OperationId>,
    pub preset: Option<InstalledPreset>,
    /// Committed transaction this plan was prepared against.
    /// `None` means no installation existed.
    pub observed_transaction: Option<zup_core::TransactionId>,
}

impl DeploymentPlan {
    pub fn fingerprint(&self) -> Sha256Digest {
        let json = serde_json::to_vec(self).expect("transaction plan serializes");
        let mut hasher = Sha256::new();
        hasher.update(&json);
        Sha256Digest::from_hasher(hasher)
    }

    /// Authorization is read from each node, never from a scope, so a
    pub fn requires_authorization(&self) -> bool {
        self.nodes
            .iter()
            .any(|node| node.meta.privilege == Some(Privilege::System))
    }

    pub fn total_work(&self) -> u64 {
        self.nodes
            .iter()
            .fold(0u64, |total, node| total.saturating_add(node.work()))
            .max(1)
    }

    pub fn validate(&self) -> Result<(), DeploymentPlanError> {
        self.requirements
            .validate()
            .map_err(|error| DeploymentPlanError::InvalidNode {
                id: "requirements".into(),
                reason: error.to_string(),
            })?;
        if self.requirements.target != self.target
            || self.requirements.components != self.selected_components
            || self.transition.is_deploy() && self.requirements.preset != self.preset
        {
            return Err(DeploymentPlanError::InvalidNode {
                id: "requirements".into(),
                reason: "deployment requirements differ from execution metadata".into(),
            });
        }
        if self.install_directory.is_none() {
            return Err(DeploymentPlanError::InvalidNode {
                id: "install_directory".into(),
                reason: "deployment requires an installation directory".into(),
            });
        }
        if self.schema != DEPLOYMENT_PLAN_SCHEMA {
            return Err(DeploymentPlanError::InvalidNode {
                id: "schema".into(),
                reason: format!("unsupported transaction plan schema {}", self.schema),
            });
        }
        if self
            .install_directory
            .as_ref()
            .is_some_and(|directory| directory.target() != &self.target)
        {
            return Err(DeploymentPlanError::InvalidNode {
                id: "install_directory".into(),
                reason: "target does not match the transaction".into(),
            });
        }
        let mut ids = BTreeSet::new();
        let mut native_services = BTreeMap::new();
        let mut retired = BTreeSet::new();
        for key in &self.retired_keys {
            if !retired.insert(key) {
                return Err(DeploymentPlanError::InvalidNode {
                    id: format!("{key:?}"),
                    reason: "retired key is duplicated".into(),
                });
            }
        }
        if self.transition.is_maintain()
            && (self.observed.is_none()
                || self.nodes.iter().any(|node| {
                    !matches!(
                        node.kind,
                        NodeKind::Barrier
                            | NodeKind::ReserveInstallation
                            | NodeKind::PublishInstallation
                            | NodeKind::Retention { .. }
                            | NodeKind::Cleanup { .. }
                    )
                }))
        {
            return Err(DeploymentPlanError::InvalidOrder);
        }
        crate::generation::validate_generation_chain(self)?;
        for (index, node) in self.nodes.iter().enumerate() {
            if !ids.insert(node.id.clone()) {
                return Err(DeploymentPlanError::DuplicateOperationId {
                    id: node.id.to_string(),
                });
            }
            if node.declaration_order != (index as u32).saturating_add(1) {
                return Err(DeploymentPlanError::InvalidNode {
                    id: node.id.to_string(),
                    reason: "declaration order is not contiguous".into(),
                });
            }
            let invalid = |reason: &str| DeploymentPlanError::InvalidNode {
                id: node.id.to_string(),
                reason: reason.into(),
            };
            let namespace = self
                .install_directory
                .as_ref()
                .expect("validated installation directory")
                .join(".zup")
                .map_err(|_| invalid("installation has no valid deployment namespace"))?;
            if node
                .file_destination()
                .is_some_and(|path| path.starts_with(&namespace))
            {
                return Err(invalid(
                    "fixed files cannot occupy deployment metadata or generations",
                ));
            }
            match &node.kind {
                NodeKind::Cleanup { operation } => {
                    operation
                        .validate(self.target)
                        .map_err(|reason| invalid(&reason))?;
                    if node.id != operation.id()
                        || node.phase != Phase::Cleanup
                        || operation.privilege != self.requirements.scope.authorization()
                        || node.meta
                            != (NodeMeta {
                                privilege: Some(operation.privilege),
                                ..Default::default()
                            })
                        || matches!(&operation.candidate, crate::CleanupCandidate::Generation(generation) if generation.materialization.installation() != self.installation)
                        || matches!(&operation.candidate, crate::CleanupCandidate::History(history) if history.installation != self.installation)
                    {
                        return Err(invalid(
                            "cleanup metadata differs from its installation authority",
                        ));
                    }
                }
                NodeKind::Retention { selection } => {
                    selection.validate().map_err(|reason| invalid(&reason))?;
                    if node.id.as_str() != OperationId::RETENTION
                        || node.phase != Phase::Retention
                        || self.observed.is_none()
                        || node.meta
                            != (NodeMeta {
                                privilege: Some(self.requirements.scope.authorization()),
                                ..Default::default()
                            })
                        || selection
                            .retire
                            .iter()
                            .chain(&selection.forget)
                            .any(|snapshot| {
                                self.observed
                                    .is_none_or(|observed| snapshot.revision >= observed.revision)
                            })
                    {
                        return Err(invalid(
                            "retention is not bound to earlier recorded revisions",
                        ));
                    }
                }
                NodeKind::PersistentDirectory { operation } => {
                    operation
                        .validate(self.target, self.requirements.scope)
                        .map_err(|reason| invalid(&reason))?;
                    let path =
                        operation.intent.binding.path(self.target).map_err(|_| {
                            invalid("persistent directory has no valid target path")
                        })?;
                    if path.starts_with(&namespace) || namespace.starts_with(&path) {
                        return Err(invalid(
                            "persistent data cannot contain or occupy the deployment namespace",
                        ));
                    }
                    if operation.intent.installation != self.installation
                        || node.id != operation.id()
                        || node.phase != Phase::FileMutation
                        || node.meta
                            != (NodeMeta {
                                privilege: Some(operation.intent.privilege),
                                ..Default::default()
                            })
                    {
                        return Err(invalid(
                            "persistent directory effect metadata is inconsistent",
                        ));
                    }
                }
                NodeKind::ReserveInstallation => {
                    if node.id.as_str() != OperationId::RESERVE
                        || node.phase != Phase::Begin
                        || node.meta
                            != (NodeMeta {
                                privilege: Some(self.requirements.scope.authorization()),
                                ..Default::default()
                            })
                    {
                        return Err(invalid("installation reservation metadata is inconsistent"));
                    }
                }
                NodeKind::PublishInstallation => {
                    if node.id.as_str() != OperationId::COMMIT
                        || node.phase != Phase::Commit
                        || node.meta
                            != (NodeMeta {
                                privilege: Some(self.requirements.scope.authorization()),
                                ..Default::default()
                            })
                    {
                        return Err(invalid("installation publication metadata is inconsistent"));
                    }
                }
                NodeKind::Generation { intent, .. } => {
                    intent.validate(self.target)?;
                }
                NodeKind::NativeCache { operation } => {
                    operation.validate(self.target)?;
                    operation.source_nodes(&self.nodes)?;
                    if node.id != operation.id()
                        || node.phase != Phase::Backend
                        || node.meta
                            != (NodeMeta {
                                privilege: Some(Privilege::User),
                                ..Default::default()
                            })
                    {
                        return Err(invalid("native cache metadata is inconsistent"));
                    }
                }
                NodeKind::ServiceControl { operation } => {
                    operation.validate().map_err(|reason| invalid(&reason))?;
                    if !operation
                        .registration
                        .belongs_to(self.installation, self.target)
                    {
                        return Err(invalid(
                            "service ownership does not match the installation and platform",
                        ));
                    }
                    crate::validate_runtime_preservation(self, operation, &[])
                        .map_err(|reason| invalid(&reason))?;
                    let phase = match operation.stage {
                        crate::ServiceControlStage::Quiesce => Phase::ServiceQuiesce,
                        crate::ServiceControlStage::Restore => Phase::ServiceRestore,
                    };
                    let token = match operation.stage {
                        crate::ServiceControlStage::Quiesce => "service_quiesce",
                        crate::ServiceControlStage::Restore => "service_restore",
                    };
                    if node.id != OperationId::resource(token, &operation.key) {
                        return Err(invalid("service control identity is not canonical"));
                    }
                    if node.phase != phase
                        || node.meta
                            != (NodeMeta {
                                privilege: Some(operation.privilege),
                                ..Default::default()
                            })
                    {
                        return Err(invalid("service control metadata is inconsistent"));
                    }
                    if !matches!(&operation.configuration, crate::ServiceState::Registration { command, .. } if command.executable.target() == &self.target)
                        || !matches!(&operation.owned_configuration, crate::ServiceState::Absent)
                            && !matches!(&operation.owned_configuration, crate::ServiceState::Registration { command, .. } if command.executable.target() == &self.target)
                    {
                        return Err(invalid("service control target mismatch"));
                    }
                }
                NodeKind::NativeService { operation } => {
                    operation.validate(self.installation, self.target)?;
                    if native_services
                        .insert(operation.key(), (node, operation.as_ref()))
                        .is_some()
                        || matches!(
                            operation.as_ref(),
                            crate::NativeServiceOperation::Remove { .. }
                        ) && !retired.contains(operation.key())
                    {
                        return Err(invalid(
                            "native service ownership is duplicated or not retired",
                        ));
                    }
                    if node.id != operation.id()
                        || node.phase != Phase::Backend
                        || node.meta
                            != (NodeMeta {
                                privilege: Some(operation.privilege()),
                                ..Default::default()
                            })
                    {
                        return Err(invalid("native service metadata is inconsistent"));
                    }
                }
                NodeKind::Barrier => {
                    let expected = match node.id.as_str() {
                        OperationId::BEGIN => Phase::Begin,
                        OperationId::PREFLIGHT => Phase::Preflight,
                        OperationId::COMMIT_INTENT => Phase::CommitIntent,
                        OperationId::VERIFY => Phase::Verify,
                        _ => return Err(invalid("unknown transaction barrier")),
                    };
                    if node.phase != expected || node.meta != NodeMeta::default() {
                        return Err(invalid("transaction barrier metadata is inconsistent"));
                    }
                }
                NodeKind::StageFile { .. }
                | NodeKind::RetainFile { .. }
                | NodeKind::FileMutation { .. } => {
                    if node
                        .meta
                        .destination
                        .as_ref()
                        .is_none_or(|path| path.target() != &self.target)
                        || node.meta.source_relative.is_none()
                        || node.meta.file_precondition.is_none()
                        || node.meta.fixed_replacement.is_none()
                        || matches!(node.meta.file_precondition, Some(FilePrecondition::Exact { object, .. })
                            if object.operating_system() != self.target.operating_system())
                        || node.meta.expected_sha256.is_none()
                        || node.meta.expected_size.is_none()
                        || node.meta.executable.is_none()
                        || node.meta.privilege.is_none()
                    {
                        return Err(invalid("file metadata is incomplete"));
                    }
                    let destination = node
                        .meta
                        .destination
                        .as_ref()
                        .expect("validated file destination");
                    let key = match &node.kind {
                        NodeKind::StageFile { key }
                        | NodeKind::RetainFile { key }
                        | NodeKind::FileMutation { key, .. } => key,
                        _ => unreachable!(),
                    };
                    let identity_matches = match key {
                        ResourceKey::File { id: zup_core::FileResourceId::Payload { .. } } | ResourceKey::File { id: zup_core::FileResourceId::Plugin { .. } } => true,
                        ResourceKey::File { id: zup_core::FileResourceId::Native { id: zup_core::NativeFileId::ServiceSource { id } } } => self.target.operating_system() == zup_core::TargetOperatingSystem::Linux
                            && self.nodes.iter().any(|node| matches!(&node.kind, NodeKind::NativeService { operation }
                                if operation.source_key().as_ref() == Some(key) && operation.key() == &ResourceKey::Service { id: id.clone() })),
                        ResourceKey::File { id: zup_core::FileResourceId::Native { .. } }
                            => self.target.operating_system() == zup_core::TargetOperatingSystem::Linux,
                        _ => false,
                    };
                    if !identity_matches {
                        return Err(invalid("fixed resource identity and placement disagree"));
                    }
                    if self
                        .install_directory
                        .as_ref()
                        .and_then(|root| root.join(".zup").ok())
                        .is_some_and(|reserved| destination.starts_with(&reserved))
                        || self.nodes.iter().any(|node| {
                            matches!(&node.kind, NodeKind::Generation { intent, .. }
                            if destination.starts_with(intent.materialization.directory()))
                        })
                    {
                        return Err(invalid(
                            "generation storage cannot be written by a fixed file effect",
                        ));
                    }
                    match &node.kind {
                        NodeKind::RetainFile { key } => {
                            if node.phase != Phase::Stage
                                || node.id != OperationId::resource("file_retain", key)
                                || self.transition.is_remove() || retired.contains(&key)
                                || !matches!(node.meta.file_precondition, Some(FilePrecondition::Exact { size, sha256, .. })
                                    if node.meta.expected_size == Some(size) && node.meta.expected_sha256 == Some(sha256))
                            {
                                return Err(invalid("retained file must name the exact previously owned content"));
                            }
                        }
                        NodeKind::StageFile { key } => {
                            if node.phase != Phase::Stage || node.id != OperationId::stage_file(key) {
                                return Err(invalid("staging identity is not canonical"));
                            }
                        }
                        NodeKind::FileMutation { key, delta } => {
                            if node.phase != Phase::FileMutation
                                || !matches!(delta, FileOperationKind::Create | FileOperationKind::Replace | FileOperationKind::RestoreOwned | FileOperationKind::RepairOwned)
                                || node.id != OperationId::resource(file_delta_token(*delta), key)
                                || !self.nodes.iter().any(|stage| matches!(&stage.kind, NodeKind::StageFile { key: staged } if staged == key) && stage.meta == node.meta)
                            {
                                return Err(invalid("file mutation has no matching staged intent"));
                            }
                        }
                        _ => unreachable!(),
                    }
                }
                NodeKind::FileRemoval { key } => {
                    let Some(removal) = &node.meta.removal else {
                        return Err(invalid("file removal metadata is missing"));
                    };
                    if removal.key != *key
                        || removal.instance.object.operating_system()
                            != self.target.operating_system()
                        || !retired.contains(&key)
                        || removal.destination.target() != &self.target
                        || removal
                            .created_directories
                            .iter()
                            .any(|directory| directory.target() != &self.target)
                        || node.meta.privilege != Some(removal.privilege)
                        || node.meta.backend.is_some()
                    {
                        return Err(invalid("file removal metadata is inconsistent"));
                    }
                }
                NodeKind::BackendOperation { key, intent } => {
                    let Some(operation) = &node.meta.backend else {
                        return Err(invalid("backend operation metadata is missing"));
                    };
                    operation.validate()?;
                    if operation.key != *key
                        || node.meta.privilege != Some(operation.privilege)
                        || operation.intent != *intent
                        || *intent != BackendOperationIntent::Apply
                        || node.meta.removal.is_some()
                    {
                        return Err(invalid("backend operation metadata is inconsistent"));
                    }
                }
                NodeKind::BackendRemoval { key } => {
                    let Some(operation) = &node.meta.backend else {
                        return Err(invalid("backend removal metadata is missing"));
                    };
                    operation.validate()?;
                    if operation.key != *key
                        || node.meta.privilege != Some(operation.privilege)
                        || operation.intent != BackendOperationIntent::Remove
                        || !retired.contains(&key)
                        || node.meta.removal.is_some()
                    {
                        return Err(invalid("backend removal metadata is inconsistent"));
                    }
                }
            }
        }
        for node in &self.nodes {
            let key = match &node.kind {
                NodeKind::StageFile { key }
                | NodeKind::RetainFile { key }
                | NodeKind::FileMutation { key, .. }
                | NodeKind::FileRemoval { key } => key,
                _ => continue,
            };
            if matches!(
                key,
                ResourceKey::File {
                    id: zup_core::FileResourceId::Native {
                        id: zup_core::NativeFileId::ServiceSource { .. }
                    }
                }
            ) && !native_services
                .values()
                .any(|(_, operation)| operation.source_key().as_ref() == Some(key))
            {
                return Err(DeploymentPlanError::InvalidNode {
                    id: node.id.to_string(),
                    reason: "service source file has no native registration effect".into(),
                });
            }
        }
        let barrier = |id: &str| {
            self.nodes
                .iter()
                .find(|node| {
                    node.id.as_str() == id
                        && (node.kind.is_barrier()
                            || matches!(node.kind, NodeKind::PublishInstallation))
                })
                .map(|node| &node.id)
                .ok_or(DeploymentPlanError::InvalidOrder)
        };
        let begin = barrier(OperationId::BEGIN)?;
        let reservation = self
            .nodes
            .iter()
            .find(|node| matches!(node.kind, NodeKind::ReserveInstallation))
            .ok_or(DeploymentPlanError::InvalidOrder)?;
        let preflight = barrier(OperationId::PREFLIGHT)?;
        let intent = barrier(OperationId::COMMIT_INTENT)?;
        let verify = barrier(OperationId::VERIFY)?;
        let commit = barrier(OperationId::COMMIT)?;
        let edge = |from: &OperationId, to: &OperationId| {
            self.dependencies
                .iter()
                .any(|edge| &edge.from == from && &edge.to == to)
        };
        for dependency in persistent_dependencies(&self.nodes, &self.requirements, self.transition)?
        {
            if !edge(&dependency.from, &dependency.to) {
                return Err(DeploymentPlanError::InvalidOrder);
            }
        }
        for dependency in history_dependencies(&self.nodes)? {
            if !edge(&dependency.from, &dependency.to) {
                return Err(DeploymentPlanError::InvalidOrder);
            }
        }
        for node in &self.nodes {
            if matches!(node.kind, NodeKind::Retention { .. })
                && (!edge(intent, &node.id) || !edge(&node.id, verify))
            {
                return Err(DeploymentPlanError::InvalidOrder);
            }
        }
        for node in &self.nodes {
            if node.kind.is_post_commit()
                && (!edge(commit, &node.id)
                    || self.dependencies.iter().any(|dependency| {
                        dependency.from == node.id
                            && self.nodes.iter().any(|successor| {
                                successor.id == dependency.to && !successor.kind.is_post_commit()
                            })
                    }))
            {
                return Err(DeploymentPlanError::InvalidOrder);
            }
        }
        if !edge(begin, &reservation.id)
            || self
                .nodes
                .iter()
                .filter(|node| node.id != *begin && node.id != reservation.id)
                .any(|node| !edge(&reservation.id, &node.id))
        {
            return Err(DeploymentPlanError::InvalidOrder);
        }
        if !edge(preflight, intent) || !edge(verify, commit) {
            return Err(DeploymentPlanError::InvalidOrder);
        }
        let stages: Vec<_> = self
            .nodes
            .iter()
            .filter(|node| {
                matches!(
                    node.kind,
                    NodeKind::StageFile { .. } | NodeKind::RetainFile { .. }
                )
            })
            .collect();
        if stages.is_empty() && !edge(begin, preflight) {
            return Err(DeploymentPlanError::InvalidOrder);
        }
        for stage in stages {
            if stage.phase != Phase::Stage || !edge(begin, &stage.id) || !edge(&stage.id, preflight)
            {
                return Err(DeploymentPlanError::InvalidOrder);
            }
        }
        for node in &self.nodes {
            if let NodeKind::NativeCache { operation } = &node.kind {
                for source in operation.source_nodes(&self.nodes)? {
                    if !edge(&source.id, &node.id) {
                        return Err(DeploymentPlanError::InvalidOrder);
                    }
                }
            }
            if matches!(
                node.kind,
                NodeKind::FileMutation { .. }
                    | NodeKind::FileRemoval { .. }
                    | NodeKind::NativeCache { .. }
                    | NodeKind::NativeService { .. }
                    | NodeKind::BackendOperation { .. }
                    | NodeKind::BackendRemoval { .. }
            ) && (!edge(intent, &node.id) || !edge(&node.id, verify))
            {
                return Err(DeploymentPlanError::InvalidOrder);
            }
            let NodeKind::ServiceControl { operation } = &node.kind else {
                continue;
            };
            if !edge(&OperationId::new(OperationId::COMMIT_INTENT), &node.id)
                || !edge(&node.id, &OperationId::new(OperationId::VERIFY))
            {
                return Err(DeploymentPlanError::InvalidOrder);
            }
            for mutation in self.nodes.iter().filter(|node| operation.guards_node(node)) {
                let ordered = match operation.stage {
                    crate::ServiceControlStage::Quiesce => edge(&node.id, &mutation.id),
                    crate::ServiceControlStage::Restore => edge(&mutation.id, &node.id),
                };
                if !ordered {
                    return Err(DeploymentPlanError::InvalidOrder);
                }
            }
            for dependency in &operation.dependencies {
                let dependency = self.nodes.iter().find(|other| matches!(&other.kind, NodeKind::ServiceControl { operation: other } if other.key == *dependency && other.stage == operation.stage)).ok_or(DeploymentPlanError::InvalidOrder)?;
                let ordered = match operation.stage {
                    crate::ServiceControlStage::Quiesce => edge(&node.id, &dependency.id),
                    crate::ServiceControlStage::Restore => edge(&dependency.id, &node.id),
                };
                if !ordered {
                    return Err(DeploymentPlanError::InvalidOrder);
                }
            }
            if operation.stage == crate::ServiceControlStage::Restore
                && let Some(stop) = self.nodes.iter().find(|other| matches!(&other.kind, NodeKind::ServiceControl { operation: other } if other.key == operation.key && other.stage == crate::ServiceControlStage::Quiesce))
                && !edge(&stop.id, &node.id) {
                return Err(DeploymentPlanError::InvalidOrder);
            }
        }
        for (node, operation) in native_services.values() {
            if let Some(source) = operation.source_node(&self.nodes)?
                && !edge(&source.id, &node.id)
            {
                return Err(DeploymentPlanError::InvalidOrder);
            }
            for id in operation.contract().requires_services {
                let key = ResourceKey::Service { id }.canonical_for(&self.target);
                let (dependency_node, dependency) = native_services
                    .get(&key)
                    .ok_or(DeploymentPlanError::InvalidOrder)?;
                let (from, to) = match operation {
                    crate::NativeServiceOperation::Remove { .. } => (&node.id, &dependency_node.id),
                    crate::NativeServiceOperation::Configure { .. } => {
                        if matches!(dependency, crate::NativeServiceOperation::Remove { .. }) {
                            return Err(DeploymentPlanError::InvalidOrder);
                        }
                        (&dependency_node.id, &node.id)
                    }
                };
                if !edge(from, to) {
                    return Err(DeploymentPlanError::InvalidOrder);
                }
            }
        }
        let mut graph = DiGraph::new();
        let mut index = BTreeMap::new();
        for node in &self.nodes {
            index.insert(node.id.clone(), graph.add_node(()));
        }
        for edge in &self.dependencies {
            if edge.from == edge.to {
                return Err(DeploymentPlanError::SelfDependency {
                    id: edge.from.to_string(),
                });
            }
            if !ids.contains(&edge.from) || !ids.contains(&edge.to) {
                return Err(DeploymentPlanError::UnknownDependency {
                    id: edge.from.to_string(),
                });
            }
            graph.add_edge(index[&edge.from], index[&edge.to], ());
        }
        if is_cyclic_directed(&graph) {
            return Err(DeploymentPlanError::Cycle);
        }
        for dependency in service_payload_dependencies(&self.nodes)? {
            if !edge(&dependency.from, &dependency.to) {
                return Err(DeploymentPlanError::InvalidOrder);
            }
        }
        Self::validate_order(&self.execution_order, &ids, &self.dependencies, false)?;
        let compensation = compensation_dependencies(&self.nodes, &self.dependencies)?;
        validate_graph(&self.nodes, &compensation)?;
        Self::validate_order(&self.rollback_order, &ids, &compensation, false)?;
        Ok(())
    }

    fn validate_order(
        order: &[OperationId],
        ids: &BTreeSet<OperationId>,
        dependencies: &[Dependency],
        reverse: bool,
    ) -> Result<(), DeploymentPlanError> {
        if order.len() != ids.len() {
            return Err(DeploymentPlanError::InvalidOrder);
        }
        let mut seen = BTreeSet::new();
        let mut positions = BTreeMap::new();
        for (position, id) in order.iter().enumerate() {
            if !ids.contains(id) || !seen.insert(id.clone()) {
                return Err(DeploymentPlanError::InvalidOrder);
            }
            positions.insert(id.clone(), position);
        }
        for dependency in dependencies {
            let from = positions[&dependency.from];
            let to = positions[&dependency.to];
            if (!reverse && from >= to) || (reverse && from <= to) {
                return Err(DeploymentPlanError::InvalidOrder);
            }
        }
        Ok(())
    }

    pub fn to_dot(&self) -> String {
        let mut output = String::from("digraph transaction {\n");
        for node in &self.nodes {
            output.push_str(&format!(
                "  \"{}\" [label=\"{}\\n{:?}\"];\n",
                node.id, node.id, node.phase
            ));
        }
        for edge in &self.dependencies {
            output.push_str(&format!("  \"{}\" -> \"{}\";\n", edge.from, edge.to));
        }
        output.push_str("}\n");
        output
    }
}

pub fn compile_deployment(
    input: &DeploymentEffects,
) -> Result<DeploymentPlan, DeploymentPlanError> {
    input.validate()?;
    reject_conflicts(input)?;

    let mut nodes = Vec::new();
    let mut audit = DeploymentAudit::default();
    let mut order = 0u32;
    let push = |node: DeploymentNode, order: &mut u32, nodes: &mut Vec<DeploymentNode>| {
        *order = order.saturating_add(1);
        let mut node = node;
        node.declaration_order = *order;
        nodes.push(node);
    };

    push(
        barrier(OperationId::BEGIN, input.requirements.scope.authorization()),
        &mut order,
        &mut nodes,
    );
    push(
        DeploymentNode {
            id: OperationId::new(OperationId::RESERVE),
            phase: Phase::Begin,
            kind: NodeKind::ReserveInstallation,
            declaration_order: 0,
            meta: NodeMeta {
                privilege: Some(input.requirements.scope.authorization()),
                ..Default::default()
            },
        },
        &mut order,
        &mut nodes,
    );
    let mut stage_ids = Vec::new();
    let mut file_ids = Vec::new();
    let mut file_ids_by_key = BTreeMap::new();
    for file in &input.files {
        match file.kind {
            FileOperationKind::NoOp => {
                audit.files_unchanged += 1;
                audit.noop_total += 1;
                let id = OperationId::resource("file_retain", &file.key);
                push(
                    DeploymentNode {
                        id: id.clone(),
                        phase: Phase::Stage,
                        kind: NodeKind::RetainFile {
                            key: file.key.clone(),
                        },
                        declaration_order: 0,
                        meta: file_meta(file),
                    },
                    &mut order,
                    &mut nodes,
                );
                stage_ids.push(id.clone());
                file_ids_by_key.insert(file.key.clone(), id);
            }
            FileOperationKind::Conflict | FileOperationKind::Drift => {
                return Err(DeploymentPlanError::UnresolvedConflict {
                    reason: format!("file {:?}", file.key),
                });
            }
            FileOperationKind::Create
            | FileOperationKind::Replace
            | FileOperationKind::RestoreOwned
            | FileOperationKind::RepairOwned => {
                let stage_id = OperationId::stage_file(&file.key);
                push(
                    DeploymentNode {
                        id: stage_id.clone(),
                        phase: Phase::Stage,
                        kind: NodeKind::StageFile {
                            key: file.key.clone(),
                        },
                        declaration_order: 0,
                        meta: file_meta(file),
                    },
                    &mut order,
                    &mut nodes,
                );
                stage_ids.push(stage_id);
                let mutation_id = OperationId::resource(file_delta_token(file.kind), &file.key);
                push(
                    DeploymentNode {
                        id: mutation_id.clone(),
                        phase: Phase::FileMutation,
                        kind: NodeKind::FileMutation {
                            key: file.key.clone(),
                            delta: file.kind,
                        },
                        declaration_order: 0,
                        meta: file_meta(file),
                    },
                    &mut order,
                    &mut nodes,
                );
                file_ids_by_key.insert(file.key.clone(), mutation_id.clone());
                file_ids.push(mutation_id);
            }
        }
    }

    push(
        barrier(
            OperationId::PREFLIGHT,
            input.requirements.scope.authorization(),
        ),
        &mut order,
        &mut nodes,
    );
    push(
        barrier(
            OperationId::COMMIT_INTENT,
            input.requirements.scope.authorization(),
        ),
        &mut order,
        &mut nodes,
    );

    let mut backend_ids = BTreeMap::new();
    let mut backend_node_ids = Vec::new();
    let mut backend_removal_ids = std::collections::BTreeSet::new();
    for operation in &input.backend_operations {
        let id = OperationId::resource(
            match operation.intent {
                BackendOperationIntent::Apply => "backend_apply",
                BackendOperationIntent::Remove => "backend_remove",
            },
            &operation.key,
        );
        backend_ids.insert(operation.key.clone(), id.clone());
        if operation.intent == BackendOperationIntent::Remove {
            backend_removal_ids.insert(id.clone());
        }
        let kind = match operation.intent {
            BackendOperationIntent::Apply => NodeKind::BackendOperation {
                key: operation.key.clone(),
                intent: operation.intent,
            },
            BackendOperationIntent::Remove => NodeKind::BackendRemoval {
                key: operation.key.clone(),
            },
        };
        push(
            DeploymentNode {
                id: id.clone(),
                phase: Phase::Backend,
                kind,
                declaration_order: 0,
                meta: NodeMeta {
                    privilege: Some(operation.privilege),
                    backend: Some(operation.clone()),
                    ..Default::default()
                },
            },
            &mut order,
            &mut nodes,
        );
        backend_node_ids.push(id);
    }

    let mut native_service_ids = BTreeMap::new();
    for operation in &input.native_services {
        let id = operation.id();
        native_service_ids.insert(operation.key().clone(), id.clone());
        push(
            DeploymentNode {
                id,
                phase: Phase::Backend,
                kind: NodeKind::NativeService {
                    operation: Box::new(operation.clone()),
                },
                declaration_order: 0,
                meta: NodeMeta {
                    privilege: Some(operation.privilege()),
                    ..Default::default()
                },
            },
            &mut order,
            &mut nodes,
        );
    }

    let mut file_removal_ids = Vec::new();
    let mut removal_ids_by_key = std::collections::BTreeMap::new();
    for removal in &input.removals {
        if removal.kind == RemovalKind::Drift {
            audit.drifted_removals += 1;
            continue;
        }
        let id = OperationId::resource("remove_file", &removal.key);
        push(
            DeploymentNode {
                id: id.clone(),
                phase: Phase::Backend,
                kind: NodeKind::FileRemoval {
                    key: removal.key.clone(),
                },
                declaration_order: 0,
                meta: NodeMeta {
                    privilege: Some(removal.privilege),
                    removal: Some(removal.clone()),
                    ..Default::default()
                },
            },
            &mut order,
            &mut nodes,
        );
        removal_ids_by_key.insert(removal.key.clone(), id.clone());
        file_removal_ids.push(id);
    }

    let mut cache_ids = Vec::new();
    for operation in &input.native_caches {
        let id = operation.id();
        operation.source_nodes(&nodes)?;
        push(
            DeploymentNode {
                id: id.clone(),
                phase: Phase::Backend,
                kind: NodeKind::NativeCache {
                    operation: Box::new(operation.clone()),
                },
                declaration_order: 0,
                meta: NodeMeta {
                    privilege: Some(Privilege::User),
                    ..Default::default()
                },
            },
            &mut order,
            &mut nodes,
        );
        cache_ids.push(id);
    }

    push(
        barrier(
            OperationId::VERIFY,
            input.requirements.scope.authorization(),
        ),
        &mut order,
        &mut nodes,
    );
    push(
        barrier(
            OperationId::COMMIT,
            input.requirements.scope.authorization(),
        ),
        &mut order,
        &mut nodes,
    );

    let mut edges = Vec::new();
    let begin = OperationId::new(OperationId::BEGIN);
    let preflight = OperationId::new(OperationId::PREFLIGHT);
    let commit_intent = OperationId::new(OperationId::COMMIT_INTENT);
    let verify = OperationId::new(OperationId::VERIFY);
    let commit = OperationId::new(OperationId::COMMIT);
    for stage in &stage_ids {
        edges.push(Dependency {
            from: begin.clone(),
            to: stage.clone(),
        });
        edges.push(Dependency {
            from: stage.clone(),
            to: preflight.clone(),
        });
    }
    if stage_ids.is_empty() {
        edges.push(Dependency {
            from: begin,
            to: preflight.clone(),
        });
    }
    edges.push(Dependency {
        from: preflight,
        to: commit_intent.clone(),
    });
    for file in &file_ids {
        edges.push(Dependency {
            from: commit_intent.clone(),
            to: file.clone(),
        });
    }
    for backend in &backend_node_ids {
        edges.push(Dependency {
            from: commit_intent.clone(),
            to: backend.clone(),
        });
        for file in &file_ids {
            edges.push(Dependency {
                from: file.clone(),
                to: backend.clone(),
            });
        }
    }
    for (index, operation) in input.backend_operations.iter().enumerate() {
        let to = &backend_node_ids[index];
        for dependency in &operation.dependencies {
            let from = backend_ids
                .get(dependency)
                .or_else(|| file_ids_by_key.get(dependency))
                .or_else(|| removal_ids_by_key.get(dependency));
            let from = from.ok_or_else(|| DeploymentPlanError::UnknownDependency {
                id: format!("{:?}", dependency),
            })?;
            edges.push(Dependency {
                from: from.clone(),
                to: to.clone(),
            });
        }
    }
    for operation in &input.native_services {
        let id = &native_service_ids[operation.key()];
        edges.push(Dependency {
            from: commit_intent.clone(),
            to: id.clone(),
        });
        for file in &file_ids {
            edges.push(Dependency {
                from: file.clone(),
                to: id.clone(),
            });
        }
        for dependency in operation.contract().requires_services {
            let dependency = &native_service_ids
                [&ResourceKey::Service { id: dependency }.canonical_for(&input.target)];
            let (from, to) = if matches!(operation, crate::NativeServiceOperation::Remove { .. }) {
                (id.clone(), dependency.clone())
            } else {
                (dependency.clone(), id.clone())
            };
            edges.push(Dependency { from, to });
        }
    }
    for file in &file_removal_ids {
        for predecessor in file_ids.iter().chain(
            backend_node_ids
                .iter()
                .filter(|id| backend_removal_ids.contains(*id)),
        ) {
            edges.push(Dependency {
                from: predecessor.clone(),
                to: file.clone(),
            });
        }
        edges.push(Dependency {
            from: commit_intent.clone(),
            to: file.clone(),
        });
    }
    let tails = file_ids
        .iter()
        .chain(backend_node_ids.iter())
        .chain(native_service_ids.values())
        .chain(cache_ids.iter())
        .chain(file_removal_ids.iter());
    for tail in tails {
        edges.push(Dependency {
            from: tail.clone(),
            to: verify.clone(),
        });
    }
    for node in &nodes {
        if let NodeKind::NativeService { operation } = &node.kind
            && let Some(source) = operation.source_node(&nodes)?
        {
            edges.push(Dependency {
                from: source.id.clone(),
                to: node.id.clone(),
            });
        }
        if let NodeKind::NativeCache { operation } = &node.kind {
            edges.push(Dependency {
                from: commit_intent.clone(),
                to: node.id.clone(),
            });
            for source in operation.source_nodes(&nodes)? {
                edges.push(Dependency {
                    from: source.id.clone(),
                    to: node.id.clone(),
                });
            }
        }
    }
    if file_ids.is_empty()
        && backend_node_ids.is_empty()
        && native_service_ids.is_empty()
        && file_removal_ids.is_empty()
        && cache_ids.is_empty()
    {
        edges.push(Dependency {
            from: commit_intent,
            to: verify.clone(),
        });
    }
    edges.push(Dependency {
        from: verify,
        to: commit,
    });

    let mut control_ids = BTreeMap::new();
    for operation in &input.service_controls {
        operation
            .validate()
            .map_err(|reason| DeploymentPlanError::InvalidNode {
                id: format!("{:?}", operation.key),
                reason,
            })?;
        let (token, phase) = match operation.stage {
            crate::ServiceControlStage::Quiesce => ("service_quiesce", Phase::ServiceQuiesce),
            crate::ServiceControlStage::Restore => ("service_restore", Phase::ServiceRestore),
        };
        let id = OperationId::resource(token, &operation.key);
        if control_ids
            .insert((operation.key.clone(), token), id.clone())
            .is_some()
        {
            return Err(DeploymentPlanError::DuplicateOperationId { id: id.to_string() });
        }
        push(
            DeploymentNode {
                id: id.clone(),
                phase,
                kind: NodeKind::ServiceControl {
                    operation: Box::new(operation.clone()),
                },
                declaration_order: 0,
                meta: NodeMeta {
                    privilege: Some(operation.privilege),
                    ..Default::default()
                },
            },
            &mut order,
            &mut nodes,
        );
        edges.push(Dependency {
            from: OperationId::new(OperationId::COMMIT_INTENT),
            to: id.clone(),
        });
        edges.push(Dependency {
            from: id.clone(),
            to: OperationId::new(OperationId::VERIFY),
        });
        for mutation in nodes.iter().filter(|node| operation.guards_node(node)) {
            let mutation = &mutation.id;
            let (from, to) = match operation.stage {
                crate::ServiceControlStage::Quiesce => (id.clone(), mutation.clone()),
                crate::ServiceControlStage::Restore => (mutation.clone(), id.clone()),
            };
            edges.push(Dependency { from, to });
        }
    }
    for operation in &input.service_controls {
        let token = match operation.stage {
            crate::ServiceControlStage::Quiesce => "service_quiesce",
            crate::ServiceControlStage::Restore => "service_restore",
        };
        let id = &control_ids[&(operation.key.clone(), token)];
        if token == "service_restore"
            && let Some(stop) = control_ids.get(&(operation.key.clone(), "service_quiesce"))
        {
            edges.push(Dependency {
                from: stop.clone(),
                to: id.clone(),
            });
        }
        for dependency in &operation.dependencies {
            let other = control_ids
                .get(&(dependency.clone(), token))
                .ok_or_else(|| DeploymentPlanError::UnknownDependency {
                    id: format!("{dependency:?}"),
                })?;
            let (from, to) = match operation.stage {
                crate::ServiceControlStage::Quiesce => (id.clone(), other.clone()),
                crate::ServiceControlStage::Restore => (other.clone(), id.clone()),
            };
            edges.push(Dependency { from, to });
        }
    }

    for intent in &input.generations {
        for operation in intent.operations() {
            push(
                DeploymentNode {
                    id: intent.operation_id(*operation),
                    phase: operation.phase(),
                    kind: NodeKind::Generation {
                        operation: *operation,
                        intent: Box::new(intent.clone()),
                    },
                    declaration_order: 0,
                    meta: NodeMeta {
                        privilege: Some(intent.privilege),
                        ..Default::default()
                    },
                },
                &mut order,
                &mut nodes,
            );
        }
        edges.push(Dependency {
            from: OperationId::new(OperationId::COMMIT_INTENT),
            to: intent.operation_id(intent.operations()[0]),
        });
        for pair in intent.operations().windows(2) {
            edges.push(Dependency {
                from: intent.operation_id(pair[0]),
                to: intent.operation_id(pair[1]),
            });
        }
        let ready = intent.operation_id(intent.ready_operation());
        let activate = match intent.publication {
            crate::GenerationPublication::Application { .. } => {
                intent.operation_id(crate::GenerationOperation::Activate)
            }
            crate::GenerationPublication::Maintenance => OperationId::new(OperationId::VERIFY),
        };
        for cache in &cache_ids {
            edges.push(Dependency {
                from: cache.clone(),
                to: activate.clone(),
            });
        }
        for key in &intent.exposures {
            let effects: Vec<_> = nodes
                .iter()
                .filter(|node| crate::generation::exposure_key(&node.kind) == Some(key))
                .collect();
            if effects.is_empty() {
                return Err(DeploymentPlanError::UnknownDependency {
                    id: format!("{key:?}"),
                });
            }
            for effect in effects {
                edges.push(Dependency {
                    from: ready.clone(),
                    to: effect.id.clone(),
                });
                edges.push(Dependency {
                    from: effect.id.clone(),
                    to: activate.clone(),
                });
            }
        }
        edges.push(Dependency {
            from: if activate.as_str() == OperationId::VERIFY {
                ready
            } else {
                activate
            },
            to: OperationId::new(OperationId::VERIFY),
        });
    }
    for operation in &input.data {
        let id = operation.id();
        push(
            DeploymentNode {
                id: id.clone(),
                phase: Phase::FileMutation,
                kind: NodeKind::PersistentDirectory {
                    operation: Box::new(operation.clone()),
                },
                declaration_order: 0,
                meta: NodeMeta {
                    privilege: Some(operation.intent.privilege),
                    ..Default::default()
                },
            },
            &mut order,
            &mut nodes,
        );
    }
    edges.extend(persistent_dependencies(
        &nodes,
        &input.requirements,
        input.transition,
    )?);
    if let Some(selection) = &input.retention {
        let id = OperationId::new(OperationId::RETENTION);
        push(
            DeploymentNode {
                id: id.clone(),
                phase: Phase::Retention,
                kind: NodeKind::Retention {
                    selection: Box::new(selection.clone()),
                },
                declaration_order: 0,
                meta: NodeMeta {
                    privilege: Some(input.requirements.scope.authorization()),
                    ..Default::default()
                },
            },
            &mut order,
            &mut nodes,
        );
        edges.push(Dependency {
            from: OperationId::new(OperationId::COMMIT_INTENT),
            to: id.clone(),
        });
        edges.push(Dependency {
            from: id,
            to: OperationId::new(OperationId::VERIFY),
        });
    }
    for operation in &input.cleanup {
        let id = operation.id();
        push(
            DeploymentNode {
                id: id.clone(),
                phase: Phase::Cleanup,
                kind: NodeKind::Cleanup {
                    operation: Box::new(operation.clone()),
                },
                declaration_order: 0,
                meta: NodeMeta {
                    privilege: Some(operation.privilege),
                    ..Default::default()
                },
            },
            &mut order,
            &mut nodes,
        );
        edges.push(Dependency {
            from: OperationId::new(OperationId::COMMIT),
            to: id,
        });
    }
    let reserve = OperationId::new(OperationId::RESERVE);
    edges.push(Dependency {
        from: OperationId::new(OperationId::BEGIN),
        to: reserve.clone(),
    });
    for node in &nodes {
        if node.id.as_str() != OperationId::BEGIN && node.id != reserve {
            edges.push(Dependency {
                from: reserve.clone(),
                to: node.id.clone(),
            });
        }
    }
    edges.extend(service_payload_dependencies(&nodes)?);
    edges.extend(history_dependencies(&nodes)?);
    validate_graph(&nodes, &edges)?;
    let execution_order = topological_ids(&nodes, &edges, false)?;
    let compensation = compensation_dependencies(&nodes, &edges)?;
    let rollback_order = topological_ids(&nodes, &compensation, true)?;
    let plan = DeploymentPlan {
        requirements: input.requirements.clone(),
        installation: input.installation,
        observed: input.observed,
        schema: DEPLOYMENT_PLAN_SCHEMA,
        target: input.target,
        selected_components: input.selected_components.clone(),
        install_directory: input.install_directory.clone(),
        transition: input.transition,
        retired_keys: input.retired_keys.clone(),
        nodes,
        dependencies: edges,
        audit,
        execution_order,
        rollback_order,
        preset: input.preset.clone(),
        observed_transaction: input.observed_transaction,
    };
    plan.validate()?;
    Ok(plan)
}

fn persistent_dependencies(
    nodes: &[DeploymentNode],
    requirements: &crate::DeploymentRequirements,
    transition: crate::DeploymentTransition,
) -> Result<Vec<Dependency>, DeploymentPlanError> {
    use crate::PersistentDirectoryStage::{Access, Prepare};
    let mut pairs = BTreeMap::new();
    for node in nodes {
        let NodeKind::PersistentDirectory { operation } = &node.kind else {
            continue;
        };
        let required = requirements
            .data
            .iter()
            .find(|data| data.id == operation.intent.directory.id);
        if !transition.is_deploy()
            || required.is_none_or(|required| {
                !operation
                    .intent
                    .matches_requirement(required, requirements.target)
            })
            || matches!(&operation.intent.binding, crate::PersistentBinding::SystemdState(binding) if binding.application != requirements.app.id)
        {
            return Err(DeploymentPlanError::InvalidOrder);
        }
        let pair = pairs
            .entry(&operation.intent.directory.id)
            .or_insert((None, None));
        let slot = match operation.stage {
            Prepare => &mut pair.0,
            Access => &mut pair.1,
        };
        if slot.replace((node, operation)).is_some() {
            return Err(DeploymentPlanError::InvalidOrder);
        }
    }
    if transition.is_deploy() && pairs.len() != requirements.data.len() {
        return Err(DeploymentPlanError::InvalidOrder);
    }
    let mut roots: Vec<zup_core::TargetPath> = Vec::new();
    for (prepare, _) in pairs.values() {
        let Some((_, operation)) = prepare else {
            return Err(DeploymentPlanError::InvalidOrder);
        };
        for id in operation.intent.directory.location.services() {
            let native = nodes
                .iter()
                .filter_map(|node| match &node.kind {
                    NodeKind::NativeService { operation } => match operation.as_ref() {
                        crate::NativeServiceOperation::Configure { service, .. }
                            if service.key
                                == (zup_core::ResourceKey::Service { id: id.clone() })
                                    .canonical_for(&requirements.target) =>
                        {
                            Some(service)
                        }
                        _ => None,
                    },
                    _ => None,
                })
                .collect::<Vec<_>>();
            if native.len() != 1 {
                return Err(DeploymentPlanError::InvalidOrder);
            }
            let native = native[0];
            let matches = match &operation.intent.binding {
                crate::PersistentBinding::SystemdState(binding) => {
                    binding.unit == native.name && native.state_directories.contains(&binding.name)
                }
                crate::PersistentBinding::Filesystem(binding) => binding
                    .writers
                    .iter()
                    .find(|writer| &writer.service == id)
                    .is_some_and(|writer| match (&writer.principal, &native.execution) {
                        (
                            crate::ServiceDataPrincipal::WindowsService { name },
                            zup_core::ServiceExecution::WindowsJob { .. },
                        ) => name == &native.name,
                        (
                            crate::ServiceDataPrincipal::LinuxUser { name, .. },
                            zup_core::ServiceExecution::SystemdUser { user, .. }
                            | zup_core::ServiceExecution::OpenrcUser { user, .. },
                        ) => name == user,
                        _ => false,
                    }),
            };
            if !matches {
                return Err(DeploymentPlanError::InvalidOrder);
            }
        }
        let path = operation
            .intent
            .binding
            .path(requirements.target)
            .map_err(|_| DeploymentPlanError::InvalidOrder)?;
        if roots
            .iter()
            .any(|root| root.starts_with(&path) || path.starts_with(root))
        {
            return Err(DeploymentPlanError::UnresolvedConflict {
                reason: "persistent data bindings overlap".into(),
            });
        }
        for node in nodes {
            let resource = match &node.kind {
                NodeKind::Generation { intent, .. } => Some(intent.materialization.directory()),
                NodeKind::FileMutation { .. }
                | NodeKind::RetainFile { .. }
                | NodeKind::FileRemoval { .. } => node.meta.destination.as_ref(),
                _ => None,
            };
            if resource
                .is_some_and(|resource| resource.starts_with(&path) || path.starts_with(resource))
            {
                return Err(DeploymentPlanError::UnresolvedConflict {
                    reason: "application payload and persistent data bindings overlap".into(),
                });
            }
        }
        roots.push(path);
    }
    let mut edges = Vec::new();
    let mut add = |from: OperationId, to: OperationId| edges.push(Dependency { from, to });
    for (_, (prepare, access)) in pairs {
        let (Some((prepare, first)), Some((access, second))) = (prepare, access) else {
            return Err(DeploymentPlanError::InvalidOrder);
        };
        if first.intent != second.intent {
            return Err(DeploymentPlanError::InvalidOrder);
        }
        add(
            OperationId::new(OperationId::COMMIT_INTENT),
            prepare.id.clone(),
        );
        add(prepare.id.clone(), access.id.clone());
        add(access.id.clone(), OperationId::new(OperationId::VERIFY));
        for node in nodes {
            match &node.kind {
                NodeKind::NativeService { operation } => {
                    if let zup_core::ResourceKey::Service { id } = operation.key()
                        && first
                            .intent
                            .directory
                            .location
                            .services()
                            .any(|service| service == id)
                    {
                        add(prepare.id.clone(), node.id.clone());
                        add(node.id.clone(), access.id.clone());
                    }
                }
                NodeKind::ServiceControl { operation }
                    if operation.stage == crate::ServiceControlStage::Restore =>
                {
                    if let zup_core::ResourceKey::Service { id } = &operation.key
                        && first
                            .intent
                            .directory
                            .location
                            .services()
                            .any(|service| service == id)
                    {
                        add(access.id.clone(), node.id.clone());
                    }
                }
                NodeKind::Generation {
                    operation: crate::GenerationOperation::Activate,
                    ..
                }
                | NodeKind::PublishInstallation => add(access.id.clone(), node.id.clone()),
                NodeKind::FileMutation { .. } | NodeKind::BackendOperation { .. } => {
                    add(prepare.id.clone(), node.id.clone());
                }
                _ => {}
            }
        }
    }
    Ok(edges)
}

fn compensation_dependencies(
    nodes: &[DeploymentNode],
    edges: &[Dependency],
) -> Result<Vec<Dependency>, DeploymentPlanError> {
    let mut inputs = BTreeSet::new();
    for node in nodes {
        if let NodeKind::NativeService { operation } = &node.kind
            && let Some(source) = operation.source_node(nodes)?
            && !matches!(source.kind, NodeKind::RetainFile { .. })
        {
            inputs.insert((source.id.clone(), node.id.clone()));
        }
        if let NodeKind::NativeCache { operation } = &node.kind {
            for source in operation.source_nodes(nodes)? {
                if matches!(source.kind, NodeKind::RetainFile { .. }) {
                    continue;
                }
                inputs.insert((source.id.clone(), node.id.clone()));
            }
        }
    }
    Ok(edges
        .iter()
        .map(|edge| {
            if inputs.contains(&(edge.from.clone(), edge.to.clone())) {
                edge.clone()
            } else {
                Dependency {
                    from: edge.to.clone(),
                    to: edge.from.clone(),
                }
            }
        })
        .collect())
}

fn barrier(id: &'static str, privilege: Privilege) -> DeploymentNode {
    DeploymentNode {
        id: OperationId::new(id),
        phase: match id {
            OperationId::BEGIN => Phase::Begin,
            OperationId::PREFLIGHT => Phase::Preflight,
            OperationId::COMMIT_INTENT => Phase::CommitIntent,
            OperationId::VERIFY => Phase::Verify,
            _ => Phase::Commit,
        },
        kind: if id == OperationId::COMMIT {
            NodeKind::PublishInstallation
        } else {
            NodeKind::Barrier
        },
        declaration_order: 0,
        meta: if id == OperationId::COMMIT {
            NodeMeta {
                privilege: Some(privilege),
                ..Default::default()
            }
        } else {
            NodeMeta::default()
        },
    }
}

fn file_meta(file: &FileOperation) -> NodeMeta {
    NodeMeta {
        destination: Some(file.destination.clone()),
        source_relative: Some(file.source_relative.clone()),
        file_precondition: Some(file.precondition),
        fixed_replacement: Some(file.replacement),
        expected_sha256: Some(file.expected_sha256),
        expected_size: Some(file.expected_size),
        privilege: Some(file.privilege),
        executable: Some(file.executable),
        ..Default::default()
    }
}

fn reject_conflicts(input: &DeploymentEffects) -> Result<(), DeploymentPlanError> {
    for file in &input.files {
        if matches!(
            file.kind,
            FileOperationKind::Conflict | FileOperationKind::Drift
        ) {
            return Err(DeploymentPlanError::UnresolvedConflict {
                reason: format!("file {:?}", file.key),
            });
        }
    }
    Ok(())
}

fn file_delta_token(delta: FileOperationKind) -> &'static str {
    match delta {
        FileOperationKind::Create => "file_create",
        FileOperationKind::Replace => "file_replace",
        FileOperationKind::RestoreOwned => "file_restore_owned",
        FileOperationKind::RepairOwned => "file_repair_owned",
        FileOperationKind::NoOp => "file_noop",
        FileOperationKind::Conflict => "file_conflict",
        FileOperationKind::Drift => "file_drift",
    }
}

fn history_dependencies(nodes: &[DeploymentNode]) -> Result<Vec<Dependency>, DeploymentPlanError> {
    let mut dependencies = Vec::new();
    for node in nodes {
        let NodeKind::Cleanup { operation } = &node.kind else {
            continue;
        };
        let crate::CleanupCandidate::History(history) = &operation.candidate else {
            continue;
        };
        let crate::HistoryObject::File { target, .. } = &history.object else {
            continue;
        };
        let journal = nodes.iter().find(|candidate| {
            matches!(&candidate.kind, NodeKind::Cleanup { operation }
                if matches!(&operation.candidate, crate::CleanupCandidate::History(source)
                    if source.installation == history.installation && source.source == history.source
                        && source.materializations == history.materializations
                        && matches!(&source.object, crate::HistoryObject::Journal { sidecars, .. } if sidecars.contains(target))))
        }).ok_or(DeploymentPlanError::InvalidOrder)?;
        dependencies.push(Dependency {
            from: node.id.clone(),
            to: journal.id.clone(),
        });
    }
    Ok(dependencies)
}

fn service_payload_dependencies(
    nodes: &[DeploymentNode],
) -> Result<Vec<Dependency>, DeploymentPlanError> {
    let mut dependencies = Vec::new();
    for node in nodes {
        let NodeKind::NativeService { operation } = &node.kind else {
            continue;
        };
        let source = operation.source_node(nodes)?;
        let definition = source
            .filter(|source| {
                matches!(
                    source.kind,
                    NodeKind::FileMutation { .. } | NodeKind::FileRemoval { .. }
                )
            })
            .unwrap_or(node);
        let contract = operation.contract();
        let (before, after) = operation.configurations();
        let previous = match operation.as_ref() {
            crate::NativeServiceOperation::Configure { previous, .. } => previous.as_deref(),
            crate::NativeServiceOperation::Remove { service, .. } => Some(service.as_ref()),
        };
        for payload in nodes {
            if payload.id == definition.id
                || !matches!(
                    payload.kind,
                    NodeKind::FileMutation { .. } | NodeKind::FileRemoval { .. }
                )
            {
                continue;
            }
            let Some(path) = payload.file_destination() else {
                continue;
            };
            let required = contract.requires_files.iter()
                .chain(previous.into_iter().flat_map(|owned| &owned.requires_files))
                .any(|required| required.equivalent(path))
                || before.iter().chain(std::iter::once(&after)).any(|state|
                    matches!(state, crate::ServiceState::Registration { command, .. } if command.executable.equivalent(path)));
            if required {
                dependencies.push(Dependency {
                    from: payload.id.clone(),
                    to: definition.id.clone(),
                });
            }
        }
    }
    Ok(dependencies)
}

fn validate_graph(
    nodes: &[DeploymentNode],
    dependencies: &[Dependency],
) -> Result<(), DeploymentPlanError> {
    let mut ids = BTreeMap::new();
    for node in nodes {
        if ids.insert(node.id.clone(), node).is_some() {
            return Err(DeploymentPlanError::DuplicateOperationId {
                id: node.id.to_string(),
            });
        }
    }
    for dependency in dependencies {
        if dependency.from == dependency.to {
            return Err(DeploymentPlanError::SelfDependency {
                id: dependency.from.to_string(),
            });
        }
        if !ids.contains_key(&dependency.from) || !ids.contains_key(&dependency.to) {
            return Err(DeploymentPlanError::UnknownDependency {
                id: dependency.from.to_string(),
            });
        }
    }
    let mut graph = DiGraph::new();
    let mut indices = BTreeMap::new();
    for node in nodes {
        indices.insert(node.id.clone(), graph.add_node(()));
    }
    for dependency in dependencies {
        graph.add_edge(indices[&dependency.from], indices[&dependency.to], ());
    }
    if is_cyclic_directed(&graph) {
        return Err(DeploymentPlanError::Cycle);
    }
    Ok(())
}

fn topological_ids(
    nodes: &[DeploymentNode],
    dependencies: &[Dependency],
    compensating: bool,
) -> Result<Vec<OperationId>, DeploymentPlanError> {
    let metadata: BTreeMap<OperationId, &DeploymentNode> =
        nodes.iter().map(|node| (node.id.clone(), node)).collect();
    let mut incoming: BTreeMap<OperationId, usize> =
        nodes.iter().map(|node| (node.id.clone(), 0)).collect();
    let mut outgoing: BTreeMap<OperationId, Vec<OperationId>> = BTreeMap::new();
    for dependency in dependencies {
        let (from, to) = (&dependency.from, &dependency.to);
        *incoming.get_mut(to).ok_or(DeploymentPlanError::Cycle)? += 1;
        outgoing.entry(from.clone()).or_default().push(to.clone());
    }
    let mut ready: Vec<OperationId> = incoming
        .iter()
        .filter(|(_, count)| **count == 0)
        .map(|(id, _)| id.clone())
        .collect();
    let mut result = Vec::with_capacity(nodes.len());
    while !ready.is_empty() {
        ready.sort_by(|a, b| {
            let left = metadata[a];
            let right = metadata[b];
            if compensating {
                right
                    .phase
                    .cmp(&left.phase)
                    .then(right.declaration_order.cmp(&left.declaration_order))
                    .then(b.cmp(a))
            } else {
                left.phase
                    .cmp(&right.phase)
                    .then(left.declaration_order.cmp(&right.declaration_order))
                    .then(a.cmp(b))
            }
        });
        let next = ready.remove(0);
        if let Some(children) = outgoing.get(&next) {
            for child in children {
                let count = incoming.get_mut(child).ok_or(DeploymentPlanError::Cycle)?;
                *count -= 1;
                if *count == 0 {
                    ready.push(child.clone());
                }
            }
        }
        result.push(next);
    }
    if result.len() == nodes.len() {
        Ok(result)
    } else {
        Err(DeploymentPlanError::Cycle)
    }
}
