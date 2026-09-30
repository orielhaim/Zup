use std::collections::{BTreeMap, BTreeSet};

use miette::Diagnostic;
use petgraph::algo::is_cyclic_directed;
use petgraph::graph::DiGraph;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use zup_core::{
    ComponentId, Privilege, RelativePath, ResourceKey, Sha256Digest, TargetTriple, UiRuntime,
};
use zup_platform::TargetPath;

use crate::id::OperationId;
use crate::input::{
    BackendOperation, BackendOperationIntent, FileDelta, FilePrecondition, FileRemoval,
    FileRemovalKind, FileWork, TransactionInput, TransactionInputError,
};

pub const TRANSACTION_PLAN_SCHEMA: u32 = 2;

#[derive(Debug, Error, Diagnostic)]
pub enum TransactionPlanError {
    #[error("transaction input is invalid: {0}")]
    #[diagnostic(code(zup_transaction::invalid_input))]
    InvalidInput(#[from] TransactionInputError),

    #[error("unresolved transaction conflict: {reason}")]
    #[diagnostic(code(zup_transaction::plan_conflict))]
    UnresolvedConflict { reason: String },

    #[error("duplicate operation id `{id}`")]
    #[diagnostic(code(zup_transaction::duplicate_operation_id))]
    DuplicateOperationId { id: String },

    #[error("dependency references unknown operation `{id}`")]
    #[diagnostic(code(zup_transaction::unknown_dependency))]
    UnknownDependency { id: String },

    #[error("operation `{id}` depends on itself")]
    #[diagnostic(code(zup_transaction::self_dependency))]
    SelfDependency { id: String },

    #[error("transaction graph contains a cycle")]
    #[diagnostic(code(zup_transaction::cycle))]
    Cycle,

    #[error("invalid transaction node `{id}`: {reason}")]
    #[diagnostic(code(zup_transaction::invalid_node))]
    InvalidNode { id: String, reason: String },

    #[error("invalid transaction execution or rollback order")]
    #[diagnostic(code(zup_transaction::invalid_order))]
    InvalidOrder,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Begin,
    Stage,
    Preflight,
    CommitIntent,
    FileMutation,
    Backend,
    Verify,
    Commit,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum NodeKind {
    Barrier,
    StageFile {
        key: ResourceKey,
    },
    FileMutation {
        key: ResourceKey,
        delta: FileDelta,
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
    /// True for a control node that orders the plan without mutating.
    pub fn is_barrier(&self) -> bool {
        matches!(self, Self::Barrier)
    }

    /// True when the node changes installed state that must be observed
    /// against its receipt before commit.
    ///
    /// Staged payloads are excluded: the file mutation that publishes them
    /// verifies the same bytes at their destination, and the staged copy is
    /// gone by then.
    pub fn requires_verification(&self) -> bool {
        matches!(
            self,
            Self::FileMutation { .. }
                | Self::FileRemoval { .. }
                | Self::BackendOperation { .. }
                | Self::BackendRemoval { .. }
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransactionNode {
    pub id: OperationId,
    pub phase: Phase,
    pub kind: NodeKind,
    pub declaration_order: u32,
    pub meta: NodeMeta,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct NodeMeta {
    pub source_relative: Option<RelativePath>,
    pub file_precondition: Option<FilePrecondition>,
    pub expected_sha256: Option<Sha256Digest>,
    pub expected_size: Option<u64>,
    pub privilege: Option<Privilege>,
    pub backend: Option<BackendOperation>,
    pub removal: Option<FileRemoval>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dependency {
    pub from: OperationId,
    pub to: OperationId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct TransactionAudit {
    pub files_unchanged: usize,
    pub backend_unchanged: usize,
    pub drifted_removals: usize,
    pub noop_total: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransactionPlan {
    pub schema: u32,
    pub target: TargetTriple,
    pub selected_components: Vec<ComponentId>,
    pub install_directory: Option<TargetPath>,
    pub uninstall: bool,
    pub retired_keys: Vec<ResourceKey>,
    pub nodes: Vec<TransactionNode>,
    pub dependencies: Vec<Dependency>,
    pub audit: TransactionAudit,
    pub execution_order: Vec<OperationId>,
    pub rollback_order: Vec<OperationId>,
    /// The UI runtime this plan makes durable, or leaves absent.
    ///
    /// Journalled with the rest of the plan so that a recovery run, which sees
    /// nothing but this record, can still state which window the installation
    /// presents.
    pub ui: Option<UiRuntime>,
}

impl TransactionPlan {
    pub fn fingerprint(&self) -> Sha256Digest {
        let json = serde_json::to_vec(self).expect("transaction plan serializes");
        let mut hasher = Sha256::new();
        hasher.update(&json);
        Sha256Digest::from_hasher(hasher)
    }

    /// True when at least one node needs system authority.
    ///
    /// Authorization is read from each node, never from a scope, so a
    /// per-user transaction that owns a host-wide service still reports true.
    pub fn requires_authorization(&self) -> bool {
        self.nodes
            .iter()
            .any(|node| node.meta.privilege == Some(Privilege::System))
    }

    pub fn total_work(&self) -> u64 {
        self.nodes
            .iter()
            .map(|node| {
                if matches!(
                    node.kind,
                    NodeKind::StageFile { .. } | NodeKind::FileMutation { .. }
                ) {
                    node.meta.expected_size.unwrap_or(1).max(1)
                } else {
                    1
                }
            })
            .sum::<u64>()
            .max(1)
    }

    pub fn validate(&self) -> Result<(), TransactionPlanError> {
        if self.schema != TRANSACTION_PLAN_SCHEMA {
            return Err(TransactionPlanError::InvalidNode {
                id: "schema".into(),
                reason: format!("unsupported transaction plan schema {}", self.schema),
            });
        }
        if self
            .install_directory
            .as_ref()
            .is_some_and(|directory| directory.target() != &self.target)
        {
            return Err(TransactionPlanError::InvalidNode {
                id: "install_directory".into(),
                reason: "target does not match the transaction".into(),
            });
        }
        let mut ids = BTreeSet::new();
        let mut retired = BTreeSet::new();
        for key in &self.retired_keys {
            if !retired.insert(key) {
                return Err(TransactionPlanError::InvalidNode {
                    id: format!("{key:?}"),
                    reason: "retired key is duplicated".into(),
                });
            }
        }
        for (index, node) in self.nodes.iter().enumerate() {
            if !ids.insert(node.id.clone()) {
                return Err(TransactionPlanError::DuplicateOperationId {
                    id: node.id.to_string(),
                });
            }
            if node.declaration_order != (index as u32).saturating_add(1) {
                return Err(TransactionPlanError::InvalidNode {
                    id: node.id.to_string(),
                    reason: "declaration order is not contiguous".into(),
                });
            }
            let invalid = |reason: &str| TransactionPlanError::InvalidNode {
                id: node.id.to_string(),
                reason: reason.into(),
            };
            match &node.kind {
                NodeKind::Barrier => {}
                NodeKind::StageFile { .. } | NodeKind::FileMutation { .. } => {
                    if node.meta.source_relative.is_none()
                        || node.meta.file_precondition.is_none()
                        || node.meta.expected_sha256.is_none()
                        || node.meta.expected_size.is_none()
                        || node.meta.privilege.is_none()
                    {
                        return Err(invalid("file metadata is incomplete"));
                    }
                }
                NodeKind::FileRemoval { key } => {
                    let Some(removal) = &node.meta.removal else {
                        return Err(invalid("file removal metadata is missing"));
                    };
                    if removal.key != *key
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
                        || operation.intent != BackendOperationIntent::Remove
                        || !retired.contains(&key)
                        || node.meta.removal.is_some()
                    {
                        return Err(invalid("backend removal metadata is inconsistent"));
                    }
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
                return Err(TransactionPlanError::SelfDependency {
                    id: edge.from.to_string(),
                });
            }
            if !ids.contains(&edge.from) || !ids.contains(&edge.to) {
                return Err(TransactionPlanError::UnknownDependency {
                    id: edge.from.to_string(),
                });
            }
            graph.add_edge(index[&edge.from], index[&edge.to], ());
        }
        if is_cyclic_directed(&graph) {
            return Err(TransactionPlanError::Cycle);
        }
        Self::validate_order(&self.execution_order, &ids, &self.dependencies, false)?;
        Self::validate_order(&self.rollback_order, &ids, &self.dependencies, true)?;
        Ok(())
    }

    fn validate_order(
        order: &[OperationId],
        ids: &BTreeSet<OperationId>,
        dependencies: &[Dependency],
        reverse: bool,
    ) -> Result<(), TransactionPlanError> {
        if order.len() != ids.len() {
            return Err(TransactionPlanError::InvalidOrder);
        }
        let mut seen = BTreeSet::new();
        let mut positions = BTreeMap::new();
        for (position, id) in order.iter().enumerate() {
            if !ids.contains(id) || !seen.insert(id.clone()) {
                return Err(TransactionPlanError::InvalidOrder);
            }
            positions.insert(id.clone(), position);
        }
        for dependency in dependencies {
            let from = positions[&dependency.from];
            let to = positions[&dependency.to];
            if (!reverse && from >= to) || (reverse && from <= to) {
                return Err(TransactionPlanError::InvalidOrder);
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

pub fn compile_transaction(
    input: &TransactionInput,
) -> Result<TransactionPlan, TransactionPlanError> {
    input.validate()?;
    reject_conflicts(input)?;

    let mut nodes = Vec::new();
    let mut audit = TransactionAudit::default();
    let mut order = 0u32;
    let push = |node: TransactionNode, order: &mut u32, nodes: &mut Vec<TransactionNode>| {
        *order = order.saturating_add(1);
        let mut node = node;
        node.declaration_order = *order;
        nodes.push(node);
    };

    push(barrier(OperationId::BEGIN), &mut order, &mut nodes);
    let mut stage_ids = Vec::new();
    let mut file_ids = Vec::new();
    let mut file_ids_by_key = BTreeMap::new();
    for file in &input.files {
        match file.delta {
            FileDelta::NoOp => {
                audit.files_unchanged += 1;
                audit.noop_total += 1;
            }
            FileDelta::Conflict | FileDelta::Drift => {
                return Err(TransactionPlanError::UnresolvedConflict {
                    reason: format!("file {:?}", file.key),
                });
            }
            FileDelta::Create
            | FileDelta::Replace
            | FileDelta::RestoreOwned
            | FileDelta::RepairOwned => {
                let stage_id = OperationId::stage_file(&file.key);
                push(
                    TransactionNode {
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
                let mutation_id = OperationId::resource(file_delta_token(file.delta), &file.key);
                push(
                    TransactionNode {
                        id: mutation_id.clone(),
                        phase: Phase::FileMutation,
                        kind: NodeKind::FileMutation {
                            key: file.key.clone(),
                            delta: file.delta,
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

    push(barrier(OperationId::PREFLIGHT), &mut order, &mut nodes);
    push(barrier(OperationId::COMMIT_INTENT), &mut order, &mut nodes);

    let mut backend_ids = BTreeMap::new();
    let mut backend_node_ids = Vec::new();
    for operation in &input.backend_operations {
        let id = OperationId::resource(
            match operation.intent {
                BackendOperationIntent::Apply => "backend_apply",
                BackendOperationIntent::Remove => "backend_remove",
            },
            &operation.key,
        );
        backend_ids.insert(operation.key.clone(), id.clone());
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
            TransactionNode {
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

    let mut file_removal_ids = Vec::new();
    for removal in &input.removals {
        if removal.kind == FileRemovalKind::Drift {
            audit.drifted_removals += 1;
            continue;
        }
        let id = OperationId::resource("remove_file", &removal.key);
        push(
            TransactionNode {
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
        file_removal_ids.push(id);
    }

    push(barrier(OperationId::VERIFY), &mut order, &mut nodes);
    push(barrier(OperationId::COMMIT), &mut order, &mut nodes);

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
                .or_else(|| file_ids_by_key.get(dependency));
            let from = from.ok_or_else(|| TransactionPlanError::UnknownDependency {
                id: format!("{:?}", dependency),
            })?;
            edges.push(Dependency {
                from: from.clone(),
                to: to.clone(),
            });
        }
    }
    for file in &file_removal_ids {
        for predecessor in file_ids
            .iter()
            .chain(backend_node_ids.iter())
            .chain(std::iter::once(&commit_intent))
        {
            edges.push(Dependency {
                from: predecessor.clone(),
                to: file.clone(),
            });
        }
    }
    let tails = file_ids
        .iter()
        .chain(backend_node_ids.iter())
        .chain(file_removal_ids.iter());
    for tail in tails {
        edges.push(Dependency {
            from: tail.clone(),
            to: verify.clone(),
        });
    }
    if file_ids.is_empty() && backend_node_ids.is_empty() && file_removal_ids.is_empty() {
        edges.push(Dependency {
            from: commit_intent,
            to: verify.clone(),
        });
    }
    edges.push(Dependency {
        from: verify,
        to: commit,
    });

    validate_graph(&nodes, &edges)?;
    let execution_order = topological_ids(&nodes, &edges, false)?;
    let rollback_order = topological_ids(&nodes, &edges, true)?;
    let plan = TransactionPlan {
        schema: TRANSACTION_PLAN_SCHEMA,
        target: input.target.clone(),
        selected_components: input.selected_components.clone(),
        install_directory: input.install_directory.clone(),
        uninstall: input.uninstall,
        retired_keys: input.retired_keys.clone(),
        nodes,
        dependencies: edges,
        audit,
        execution_order,
        rollback_order,
        ui: input.ui.clone(),
    };
    plan.validate()?;
    Ok(plan)
}

fn barrier(id: &'static str) -> TransactionNode {
    TransactionNode {
        id: OperationId::new(id),
        phase: match id {
            OperationId::BEGIN => Phase::Begin,
            OperationId::PREFLIGHT => Phase::Preflight,
            OperationId::COMMIT_INTENT => Phase::CommitIntent,
            OperationId::VERIFY => Phase::Verify,
            _ => Phase::Commit,
        },
        kind: NodeKind::Barrier,
        declaration_order: 0,
        meta: NodeMeta::default(),
    }
}

fn file_meta(file: &FileWork) -> NodeMeta {
    NodeMeta {
        source_relative: Some(file.source_relative.clone()),
        file_precondition: Some(file.precondition),
        expected_sha256: Some(file.expected_sha256),
        expected_size: Some(file.expected_size),
        privilege: Some(file.privilege),
        ..Default::default()
    }
}

fn reject_conflicts(input: &TransactionInput) -> Result<(), TransactionPlanError> {
    for file in &input.files {
        if matches!(file.delta, FileDelta::Conflict | FileDelta::Drift) {
            return Err(TransactionPlanError::UnresolvedConflict {
                reason: format!("file {:?}", file.key),
            });
        }
    }
    Ok(())
}

fn file_delta_token(delta: FileDelta) -> &'static str {
    match delta {
        FileDelta::Create => "file_create",
        FileDelta::Replace => "file_replace",
        FileDelta::RestoreOwned => "file_restore_owned",
        FileDelta::RepairOwned => "file_repair_owned",
        FileDelta::NoOp => "file_noop",
        FileDelta::Conflict => "file_conflict",
        FileDelta::Drift => "file_drift",
    }
}

fn validate_graph(
    nodes: &[TransactionNode],
    dependencies: &[Dependency],
) -> Result<(), TransactionPlanError> {
    let mut ids = BTreeMap::new();
    for node in nodes {
        if ids.insert(node.id.clone(), node).is_some() {
            return Err(TransactionPlanError::DuplicateOperationId {
                id: node.id.to_string(),
            });
        }
    }
    for dependency in dependencies {
        if dependency.from == dependency.to {
            return Err(TransactionPlanError::SelfDependency {
                id: dependency.from.to_string(),
            });
        }
        if !ids.contains_key(&dependency.from) || !ids.contains_key(&dependency.to) {
            return Err(TransactionPlanError::UnknownDependency {
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
        return Err(TransactionPlanError::Cycle);
    }
    Ok(())
}

fn topological_ids(
    nodes: &[TransactionNode],
    dependencies: &[Dependency],
    reverse: bool,
) -> Result<Vec<OperationId>, TransactionPlanError> {
    let metadata: BTreeMap<OperationId, &TransactionNode> =
        nodes.iter().map(|node| (node.id.clone(), node)).collect();
    let mut incoming: BTreeMap<OperationId, usize> =
        nodes.iter().map(|node| (node.id.clone(), 0)).collect();
    let mut outgoing: BTreeMap<OperationId, Vec<OperationId>> = BTreeMap::new();
    for dependency in dependencies {
        let (from, to) = if reverse {
            (&dependency.to, &dependency.from)
        } else {
            (&dependency.from, &dependency.to)
        };
        *incoming.get_mut(to).ok_or(TransactionPlanError::Cycle)? += 1;
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
            if reverse {
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
                let count = incoming.get_mut(child).ok_or(TransactionPlanError::Cycle)?;
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
        Err(TransactionPlanError::Cycle)
    }
}
