//! Transaction plan model and pure compilation from `ExecutionPlan`.
//!
//! The serialized model is ours. `petgraph` is used only internally for
//! validation, topological ordering, and reverse rollback ordering.

use std::collections::BTreeMap;

use miette::Diagnostic;
use petgraph::algo::is_cyclic_directed;
use petgraph::graph::DiGraph;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use zup_core::{Privilege, RelativePath, ResourceKey, Sha256Digest};
use zup_exec::{
    Delta, ExecutionPlan, FileTypeOperationKind, PathOperationKind, ProtocolOperationKind,
    ServiceOperationKind, ShortcutOperationKind,
};

use crate::id::{
    OperationId, action_kind_token, file_kind_token, file_type_kind_token, path_kind_token,
    protocol_kind_token, service_kind_token, shortcut_kind_token,
};
use crate::rollback::{RollbackCapability, RollbackGuarantee};

/// Errors produced while compiling an execution plan into a transaction plan.
#[derive(Debug, Error, Diagnostic)]
pub enum TransactionPlanError {
    #[error("execution plan contains unresolved conflict: {reason}")]
    #[diagnostic(code(zup_transaction::plan_conflict))]
    UnresolvedConflict { reason: String },

    #[error("unsupported operation kind for `{key}`")]
    #[diagnostic(code(zup_transaction::unsupported_operation))]
    UnsupportedOperation { key: String },

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

    #[error("forbidden operation ordering involving `{id}`")]
    #[diagnostic(code(zup_transaction::forbidden_ordering))]
    ForbiddenOrdering { id: String },
}

/// Phase bucket used for deterministic ordering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Begin,
    Stage,
    Preflight,
    CommitIntent,
    FileMutation,
    ManagedIntegration,
    OpaqueAction,
    Verify,
    Commit,
}

/// Kind of transaction node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum NodeKind {
    /// Control barrier (begin, preflight, commit-intent, verify, commit).
    Barrier,
    /// Stage a payload file before commit intent.
    StageFile { key: ResourceKey },
    /// Mutate a file (create/replace).
    FileMutation { key: ResourceKey, delta: Delta },
    /// Managed system integration (shortcut, path, service, protocol, file type).
    ManagedIntegration {
        key: ResourceKey,
        delta: Delta,
        resource: ManagedResource,
    },
    OwnedRemoval {
        key: ResourceKey,
        resource: ManagedResource,
    },
    /// Opaque external action. Side effects unknown to the planner.
    OpaqueAction { key: ResourceKey },
}

/// Which managed resource family a node belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagedResource {
    File,
    Shortcut,
    PathEntry,
    Service,
    Protocol,
    FileType,
}

/// One executable (or barrier) transaction node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransactionNode {
    pub id: OperationId,
    pub phase: Phase,
    pub kind: NodeKind,
    pub rollback: RollbackCapability,
    /// Declaration order within the source `ExecutionPlan`.
    pub declaration_order: u32,
    /// Extra metadata needed later by executors (e.g. expected hash).
    pub meta: NodeMeta,
}

/// Executor-facing payload summary (no secrets).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct NodeMeta {
    pub source_relative: Option<RelativePath>,
    pub file_precondition: Option<zup_exec::FilePrecondition>,
    pub expected_sha256: Option<Sha256Digest>,
    pub expected_size: Option<u64>,
    pub privilege: Option<Privilege>,
    pub has_rollback_command: bool,
    pub managed: Option<zup_exec::ManagedOperation>,
    pub removal: Option<zup_exec::OwnedResource>,
    pub removal_scope: Option<zup_core::SelectedScope>,
}

/// Directed dependency edge (`from` must complete before `to` starts).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dependency {
    pub from: OperationId,
    pub to: OperationId,
}

/// Audit-only counts of operations that need no mutation node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct TransactionAudit {
    pub files_unchanged: usize,
    pub shortcuts_unchanged: usize,
    pub path_entries_present: usize,
    pub services_unchanged: usize,
    pub protocols_unchanged: usize,
    pub file_types_unchanged: usize,
    pub noop_total: usize,
}

/// Pure, deterministic transaction plan (no runtime identity).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransactionPlan {
    pub selected_components: Vec<zup_core::ComponentId>,
    pub uninstall: bool,
    pub retired_keys: Vec<ResourceKey>,
    pub nodes: Vec<TransactionNode>,
    pub dependencies: Vec<Dependency>,
    pub rollback_guarantee: RollbackGuarantee,
    pub audit: TransactionAudit,
    /// Deterministic execution order of mutating nodes (OperationIds).
    pub execution_order: Vec<OperationId>,
    /// Deterministic reverse order used for rollback.
    pub rollback_order: Vec<OperationId>,
}

impl TransactionPlan {
    /// Canonical deterministic fingerprint of the logical plan.
    pub fn fingerprint(&self) -> Sha256Digest {
        let json = serde_json::to_vec(self).expect("plan serializes");
        let mut hasher = Sha256::new();
        hasher.update(&json);
        Sha256Digest::from_hasher(hasher)
    }

    /// Debug DOT rendering of the logical graph (no Graphviz dependency).
    pub fn to_dot(&self) -> String {
        let mut out = String::from("digraph transaction {\n");
        for node in &self.nodes {
            out.push_str(&format!(
                "  \"{}\" [label=\"{}\\n{:?}\"];\n",
                node.id, node.id, node.phase
            ));
        }
        for dep in &self.dependencies {
            out.push_str(&format!("  \"{}\" -> \"{}\";\n", dep.from, dep.to));
        }
        out.push_str("}\n");
        out
    }
}

/// Compile an `ExecutionPlan` into a `TransactionPlan`. Pure.
pub fn compile_transaction(
    execution: &ExecutionPlan,
) -> Result<TransactionPlan, TransactionPlanError> {
    reject_conflicts(execution)?;

    let mut nodes = Vec::new();
    let mut audit = TransactionAudit::default();
    let mut order = 0u32;

    let push = |node: TransactionNode, order: &mut u32, nodes: &mut Vec<TransactionNode>| {
        *order += 1;
        let mut node = node;
        node.declaration_order = *order;
        nodes.push(node);
    };

    // Barriers
    push(
        barrier(OperationId::new(OperationId::BEGIN), Phase::Begin),
        &mut order,
        &mut nodes,
    );

    // Stage mutating files
    let mut stage_ids = Vec::new();
    let mut file_mutation_ids = Vec::new();
    for file in &execution.files {
        match file.kind {
            zup_exec::FileOperationKind::NoOp => {
                audit.files_unchanged += 1;
                audit.noop_total += 1;
            }
            zup_exec::FileOperationKind::Conflict | zup_exec::FileOperationKind::Drift => {
                return Err(TransactionPlanError::UnresolvedConflict {
                    reason: format!("file conflict on {:?}", file.key),
                });
            }
            kind @ (zup_exec::FileOperationKind::Create
            | zup_exec::FileOperationKind::Replace
            | zup_exec::FileOperationKind::RestoreOwned
            | zup_exec::FileOperationKind::RepairOwned) => {
                let stage_id = OperationId::stage_file(&file.key);
                push(
                    TransactionNode {
                        id: stage_id.clone(),
                        phase: Phase::Stage,
                        kind: NodeKind::StageFile {
                            key: file.key.clone(),
                        },
                        rollback: RollbackCapability::Automatic,
                        declaration_order: 0,
                        meta: NodeMeta {
                            source_relative: Some(file.source_relative.clone()),
                            file_precondition: Some(file.precondition),
                            expected_sha256: Some(file.expected_sha256),
                            expected_size: Some(file.expected_size),
                            privilege: Some(zup_core::Privilege::User),
                            has_rollback_command: false,
                            managed: None,
                            removal: None,
                            removal_scope: None,
                        },
                    },
                    &mut order,
                    &mut nodes,
                );
                stage_ids.push(stage_id);

                let mut_id = OperationId::resource(file_kind_token(kind), &file.key);
                push(
                    TransactionNode {
                        id: mut_id.clone(),
                        phase: Phase::FileMutation,
                        kind: NodeKind::FileMutation {
                            key: file.key.clone(),
                            delta: match kind {
                                zup_exec::FileOperationKind::Create => Delta::Create,
                                zup_exec::FileOperationKind::RestoreOwned => Delta::RestoreOwned,
                                zup_exec::FileOperationKind::RepairOwned => Delta::RepairOwned,
                                _ => Delta::Replace,
                            },
                        },
                        rollback: RollbackCapability::Automatic,
                        declaration_order: 0,
                        meta: NodeMeta {
                            source_relative: Some(file.source_relative.clone()),
                            file_precondition: Some(file.precondition),
                            expected_sha256: Some(file.expected_sha256),
                            expected_size: Some(file.expected_size),
                            privilege: Some(zup_core::Privilege::User),
                            has_rollback_command: false,
                            managed: None,
                            removal: None,
                            removal_scope: None,
                        },
                    },
                    &mut order,
                    &mut nodes,
                );
                file_mutation_ids.push(mut_id);
            }
        }
    }

    push(
        barrier(OperationId::new(OperationId::PREFLIGHT), Phase::Preflight),
        &mut order,
        &mut nodes,
    );
    push(
        barrier(
            OperationId::new(OperationId::COMMIT_INTENT),
            Phase::CommitIntent,
        ),
        &mut order,
        &mut nodes,
    );

    // Managed integrations
    let mut managed_ids = Vec::new();
    for shortcut in &execution.shortcuts {
        push_managed_shortcut(
            shortcut,
            &mut order,
            &mut nodes,
            &mut audit,
            &mut managed_ids,
        )?;
    }
    for path in &execution.path_entries {
        push_managed_path(path, &mut order, &mut nodes, &mut audit, &mut managed_ids)?;
    }
    for service in &execution.services {
        push_managed_service(
            service,
            &mut order,
            &mut nodes,
            &mut audit,
            &mut managed_ids,
        )?;
    }
    for protocol in &execution.protocols {
        push_managed_protocol(
            protocol,
            &mut order,
            &mut nodes,
            &mut audit,
            &mut managed_ids,
        )?;
    }
    for file_type in &execution.file_types {
        push_managed_file_type(
            file_type,
            &mut order,
            &mut nodes,
            &mut audit,
            &mut managed_ids,
        )?;
    }

    let mut managed_removal_ids = Vec::new();
    let mut file_removal_ids = Vec::new();
    for removal in &execution.removals {
        if removal.kind == zup_exec::RemovalKind::Drift {
            continue;
        }
        let resource = match &removal.owned {
            zup_exec::OwnedResource::File { .. } => ManagedResource::File,
            zup_exec::OwnedResource::Shortcut { .. } => ManagedResource::Shortcut,
            zup_exec::OwnedResource::PathEntry { .. } => ManagedResource::PathEntry,
            zup_exec::OwnedResource::Service { .. } => ManagedResource::Service,
            zup_exec::OwnedResource::Protocol { .. } => ManagedResource::Protocol,
            zup_exec::OwnedResource::ProgId { .. } | zup_exec::OwnedResource::Extension { .. } => {
                ManagedResource::FileType
            }
        };
        let id = OperationId::resource("remove_owned", &removal.key);
        push(
            TransactionNode {
                id: id.clone(),
                phase: Phase::ManagedIntegration,
                kind: NodeKind::OwnedRemoval {
                    key: removal.key.clone(),
                    resource,
                },
                rollback: RollbackCapability::Automatic,
                declaration_order: 0,
                meta: NodeMeta {
                    removal: Some(removal.owned.clone()),
                    removal_scope: Some(removal.scope),
                    ..Default::default()
                },
            },
            &mut order,
            &mut nodes,
        );
        if resource == ManagedResource::File {
            file_removal_ids.push(id)
        } else {
            managed_removal_ids.push(id)
        }
    }

    // Opaque actions — late, after all reversible work.
    let mut opaque_ids = Vec::new();
    for action in &execution.external_actions {
        let id = OperationId::resource(action_kind_token(Delta::RunOpaque), &action.key);
        let rollback = if action.rollback.is_some() {
            RollbackCapability::Compensating
        } else {
            RollbackCapability::None
        };
        push(
            TransactionNode {
                id: id.clone(),
                phase: Phase::OpaqueAction,
                kind: NodeKind::OpaqueAction {
                    key: action.key.clone(),
                },
                rollback,
                declaration_order: 0,
                meta: NodeMeta {
                    source_relative: None,
                    file_precondition: None,
                    expected_sha256: None,
                    expected_size: None,
                    privilege: Some(action.privilege),
                    has_rollback_command: action.rollback.is_some(),
                    managed: None,
                    removal: None,
                    removal_scope: None,
                },
            },
            &mut order,
            &mut nodes,
        );
        opaque_ids.push(id);
        let _ = action;
    }

    push(
        barrier(OperationId::new(OperationId::VERIFY), Phase::Verify),
        &mut order,
        &mut nodes,
    );
    push(
        barrier(OperationId::new(OperationId::COMMIT), Phase::Commit),
        &mut order,
        &mut nodes,
    );

    // Edges
    let mut deps = Vec::new();
    let begin = OperationId::new(OperationId::BEGIN);
    let preflight = OperationId::new(OperationId::PREFLIGHT);
    let commit_intent = OperationId::new(OperationId::COMMIT_INTENT);
    let verify = OperationId::new(OperationId::VERIFY);
    let commit = OperationId::new(OperationId::COMMIT);

    for stage in &stage_ids {
        deps.push(Dependency {
            from: begin.clone(),
            to: stage.clone(),
        });
        deps.push(Dependency {
            from: stage.clone(),
            to: preflight.clone(),
        });
    }
    if stage_ids.is_empty() {
        deps.push(Dependency {
            from: begin.clone(),
            to: preflight.clone(),
        });
    }

    deps.push(Dependency {
        from: preflight.clone(),
        to: commit_intent.clone(),
    });

    for file in &file_mutation_ids {
        deps.push(Dependency {
            from: commit_intent.clone(),
            to: file.clone(),
        });
    }

    for managed in &managed_ids {
        // Managed integrations depend on commit intent and on all file mutations
        // (do not register an executable before its files exist).
        deps.push(Dependency {
            from: commit_intent.clone(),
            to: managed.clone(),
        });
        for file in &file_mutation_ids {
            deps.push(Dependency {
                from: file.clone(),
                to: managed.clone(),
            });
        }
    }
    for managed in &managed_removal_ids {
        deps.push(Dependency {
            from: commit_intent.clone(),
            to: managed.clone(),
        });
        for file in &file_mutation_ids {
            deps.push(Dependency {
                from: file.clone(),
                to: managed.clone(),
            });
        }
    }
    for file in &file_removal_ids {
        for predecessor in file_mutation_ids
            .iter()
            .chain(managed_ids.iter())
            .chain(managed_removal_ids.iter())
            .chain(std::iter::once(&commit_intent))
        {
            deps.push(Dependency {
                from: predecessor.clone(),
                to: file.clone(),
            });
        }
    }
    for extension in nodes.iter().filter(|node| {
        matches!(
            node.meta.removal,
            Some(zup_exec::OwnedResource::Extension { .. })
        )
    }) {
        let Some(zup_exec::OwnedResource::Extension {
            installed: zup_exec::ExtensionState::Mapped { prog_id },
            ..
        }) = &extension.meta.removal
        else {
            continue;
        };
        if let Some(prog_id_node) = nodes.iter().find(|node| matches!((&node.kind, &node.meta.removal),
            (NodeKind::OwnedRemoval { key: ResourceKey::FileType { id }, .. }, Some(zup_exec::OwnedResource::ProgId { .. })) if id.as_str() == prog_id))
        {
            deps.push(Dependency { from: extension.id.clone(), to: prog_id_node.id.clone() });
        }
    }

    for file_type in &execution.file_types {
        let prog_id = nodes.iter().find(|node| matches!(&node.meta.managed, Some(zup_exec::ManagedOperation::ProgId(op)) if op.key == file_type.key));
        let extension = nodes.iter().find(|node| matches!(&node.meta.managed, Some(zup_exec::ManagedOperation::Extension(op)) if op.key == file_type.key));
        if let (Some(prog_id), Some(extension)) = (prog_id, extension) {
            deps.push(Dependency {
                from: prog_id.id.clone(),
                to: extension.id.clone(),
            });
        }
    }

    // Opaque actions after managed operations and file mutations.
    for opaque in &opaque_ids {
        for predecessor in file_mutation_ids
            .iter()
            .chain(managed_ids.iter())
            .chain(managed_removal_ids.iter())
            .chain(file_removal_ids.iter())
            .chain(std::iter::once(&commit_intent))
        {
            deps.push(Dependency {
                from: predecessor.clone(),
                to: opaque.clone(),
            });
        }
    }

    // Verify after all mutations; commit after verify.
    for tail in file_mutation_ids
        .iter()
        .chain(managed_ids.iter())
        .chain(managed_removal_ids.iter())
        .chain(file_removal_ids.iter())
        .chain(opaque_ids.iter())
    {
        deps.push(Dependency {
            from: tail.clone(),
            to: verify.clone(),
        });
    }
    if file_mutation_ids.is_empty()
        && managed_ids.is_empty()
        && managed_removal_ids.is_empty()
        && file_removal_ids.is_empty()
        && opaque_ids.is_empty()
    {
        deps.push(Dependency {
            from: commit_intent.clone(),
            to: verify.clone(),
        });
    }
    deps.push(Dependency {
        from: verify.clone(),
        to: commit.clone(),
    });

    validate_graph(&nodes, &deps)?;

    let execution_order = topological_ids(&nodes, &deps, false)?;
    let rollback_order = topological_ids(&nodes, &deps, true)?;

    // Irreversible operations must run after all reversible work that can precede
    // them — encoded above via opaque → depends on file + managed.
    for node in &nodes {
        if node.rollback == RollbackCapability::None
            && node.phase != Phase::OpaqueAction
            && node.phase != Phase::Begin
            && node.phase != Phase::Preflight
            && node.phase != Phase::CommitIntent
            && node.phase != Phase::Verify
            && node.phase != Phase::Commit
        {
            return Err(TransactionPlanError::ForbiddenOrdering {
                id: node.id.to_string(),
            });
        }
    }

    let rollback_guarantee =
        RollbackGuarantee::from_capabilities(nodes.iter().map(|n| &n.rollback));

    Ok(TransactionPlan {
        selected_components: execution.selected_components.clone(),
        uninstall: execution.uninstall,
        retired_keys: execution.removals.iter().map(|op| op.key.clone()).collect(),
        nodes,
        dependencies: deps,
        rollback_guarantee,
        audit,
        execution_order,
        rollback_order,
    })
}

fn barrier(id: OperationId, phase: Phase) -> TransactionNode {
    TransactionNode {
        id,
        phase,
        kind: NodeKind::Barrier,
        rollback: RollbackCapability::Automatic,
        declaration_order: 0,
        meta: NodeMeta::default(),
    }
}

fn reject_conflicts(execution: &ExecutionPlan) -> Result<(), TransactionPlanError> {
    fn check(has_conflict: bool, what: &str) -> Result<(), TransactionPlanError> {
        if has_conflict {
            Err(TransactionPlanError::UnresolvedConflict {
                reason: what.to_owned(),
            })
        } else {
            Ok(())
        }
    }

    for f in &execution.files {
        check(f.conflict.is_some(), "file")?;
    }
    for s in &execution.shortcuts {
        check(s.conflict.is_some(), "shortcut")?;
    }
    for p in &execution.path_entries {
        check(p.conflict.is_some(), "path entry")?;
    }
    for s in &execution.services {
        check(s.conflict.is_some(), "service")?;
    }
    for p in &execution.protocols {
        check(p.conflict.is_some(), "protocol")?;
    }
    for f in &execution.file_types {
        check(f.conflict.is_some(), "file type")?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn push_managed_shortcut(
    op: &zup_exec::ShortcutOperation,
    order: &mut u32,
    nodes: &mut Vec<TransactionNode>,
    audit: &mut TransactionAudit,
    managed_ids: &mut Vec<OperationId>,
) -> Result<(), TransactionPlanError> {
    match op.kind {
        ShortcutOperationKind::NoOp => {
            audit.shortcuts_unchanged += 1;
            audit.noop_total += 1;
        }
        ShortcutOperationKind::Conflict | ShortcutOperationKind::Drift => {
            return Err(TransactionPlanError::UnresolvedConflict {
                reason: "shortcut".into(),
            });
        }
        kind @ (ShortcutOperationKind::Create
        | ShortcutOperationKind::UpdateOwned
        | ShortcutOperationKind::RestoreOwned) => {
            let id = OperationId::resource(shortcut_kind_token(kind), &op.key);
            nodes.push(TransactionNode {
                id: id.clone(),
                phase: Phase::ManagedIntegration,
                kind: NodeKind::ManagedIntegration {
                    key: op.key.clone(),
                    delta: match kind {
                        ShortcutOperationKind::Create => Delta::Create,
                        ShortcutOperationKind::RestoreOwned => Delta::RestoreOwned,
                        _ => Delta::Replace,
                    },
                    resource: ManagedResource::Shortcut,
                },
                rollback: RollbackCapability::Automatic,
                declaration_order: *order,
                meta: NodeMeta {
                    managed: Some(zup_exec::ManagedOperation::Shortcut(op.clone())),
                    ..Default::default()
                },
            });
            *order += 1;
            managed_ids.push(id);
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn push_managed_path(
    op: &zup_exec::PathOperation,
    order: &mut u32,
    nodes: &mut Vec<TransactionNode>,
    audit: &mut TransactionAudit,
    managed_ids: &mut Vec<OperationId>,
) -> Result<(), TransactionPlanError> {
    match op.kind {
        PathOperationKind::Present => {
            audit.path_entries_present += 1;
            audit.noop_total += 1;
        }
        PathOperationKind::Conflict | PathOperationKind::Drift => {
            return Err(TransactionPlanError::UnresolvedConflict {
                reason: "path".into(),
            });
        }
        kind @ (PathOperationKind::Add
        | PathOperationKind::UpdateOwned
        | PathOperationKind::RestoreOwned) => {
            let id = OperationId::resource(path_kind_token(kind), &op.key);
            nodes.push(TransactionNode {
                id: id.clone(),
                phase: Phase::ManagedIntegration,
                kind: NodeKind::ManagedIntegration {
                    key: op.key.clone(),
                    delta: match kind {
                        PathOperationKind::Add => Delta::Create,
                        PathOperationKind::RestoreOwned => Delta::RestoreOwned,
                        _ => Delta::Replace,
                    },
                    resource: ManagedResource::PathEntry,
                },
                rollback: RollbackCapability::Automatic,
                declaration_order: *order,
                meta: NodeMeta {
                    managed: Some(zup_exec::ManagedOperation::Path(op.clone())),
                    ..Default::default()
                },
            });
            *order += 1;
            managed_ids.push(id);
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn push_managed_service(
    op: &zup_exec::ServiceOperation,
    order: &mut u32,
    nodes: &mut Vec<TransactionNode>,
    audit: &mut TransactionAudit,
    managed_ids: &mut Vec<OperationId>,
) -> Result<(), TransactionPlanError> {
    match op.kind {
        ServiceOperationKind::NoOp => {
            audit.services_unchanged += 1;
            audit.noop_total += 1;
        }
        ServiceOperationKind::Conflict | ServiceOperationKind::Drift => {
            return Err(TransactionPlanError::UnresolvedConflict {
                reason: "service".into(),
            });
        }
        kind @ (ServiceOperationKind::Create
        | ServiceOperationKind::UpdateOwned
        | ServiceOperationKind::RestoreOwned) => {
            let id = OperationId::resource(service_kind_token(kind), &op.key);
            nodes.push(TransactionNode {
                id: id.clone(),
                phase: Phase::ManagedIntegration,
                kind: NodeKind::ManagedIntegration {
                    key: op.key.clone(),
                    delta: match kind {
                        ServiceOperationKind::Create => Delta::Create,
                        ServiceOperationKind::RestoreOwned => Delta::RestoreOwned,
                        _ => Delta::Replace,
                    },
                    resource: ManagedResource::Service,
                },
                rollback: RollbackCapability::Automatic,
                declaration_order: *order,
                meta: NodeMeta {
                    managed: Some(zup_exec::ManagedOperation::Service(op.clone())),
                    ..Default::default()
                },
            });
            *order += 1;
            managed_ids.push(id);
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn push_managed_protocol(
    op: &zup_exec::ProtocolOperation,
    order: &mut u32,
    nodes: &mut Vec<TransactionNode>,
    audit: &mut TransactionAudit,
    managed_ids: &mut Vec<OperationId>,
) -> Result<(), TransactionPlanError> {
    match op.kind {
        ProtocolOperationKind::NoOp => {
            audit.protocols_unchanged += 1;
            audit.noop_total += 1;
        }
        ProtocolOperationKind::Conflict | ProtocolOperationKind::Drift => {
            return Err(TransactionPlanError::UnresolvedConflict {
                reason: "protocol".into(),
            });
        }
        kind @ (ProtocolOperationKind::Create
        | ProtocolOperationKind::UpdateOwned
        | ProtocolOperationKind::RestoreOwned) => {
            let id = OperationId::resource(protocol_kind_token(kind), &op.key);
            nodes.push(TransactionNode {
                id: id.clone(),
                phase: Phase::ManagedIntegration,
                kind: NodeKind::ManagedIntegration {
                    key: op.key.clone(),
                    delta: match kind {
                        ProtocolOperationKind::Create => Delta::Create,
                        ProtocolOperationKind::RestoreOwned => Delta::RestoreOwned,
                        _ => Delta::Replace,
                    },
                    resource: ManagedResource::Protocol,
                },
                rollback: RollbackCapability::Automatic,
                declaration_order: *order,
                meta: NodeMeta {
                    managed: Some(zup_exec::ManagedOperation::Protocol(op.clone())),
                    ..Default::default()
                },
            });
            *order += 1;
            managed_ids.push(id);
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn push_managed_file_type(
    op: &zup_exec::FileTypeOperation,
    order: &mut u32,
    nodes: &mut Vec<TransactionNode>,
    audit: &mut TransactionAudit,
    managed_ids: &mut Vec<OperationId>,
) -> Result<(), TransactionPlanError> {
    for kind in [op.prog_id_kind, op.extension_kind] {
        if matches!(
            kind,
            FileTypeOperationKind::Conflict | FileTypeOperationKind::Drift
        ) {
            return Err(TransactionPlanError::UnresolvedConflict {
                reason: "file type".into(),
            });
        }
    }
    if op.prog_id_kind == FileTypeOperationKind::NoOp
        && op.extension_kind == FileTypeOperationKind::NoOp
    {
        audit.file_types_unchanged += 1;
        audit.noop_total += 1;
        return Ok(());
    }
    if matches!(
        op.prog_id_kind,
        FileTypeOperationKind::Create
            | FileTypeOperationKind::UpdateOwned
            | FileTypeOperationKind::RestoreOwned
    ) {
        let kind = op.prog_id_kind;
        let prog_id = OperationId::resource(file_type_kind_token(kind), &op.key);
        nodes.push(TransactionNode {
            id: prog_id.clone(),
            phase: Phase::ManagedIntegration,
            kind: NodeKind::ManagedIntegration {
                key: op.key.clone(),
                delta: match kind {
                    FileTypeOperationKind::Create => Delta::Create,
                    FileTypeOperationKind::RestoreOwned => Delta::RestoreOwned,
                    _ => Delta::Replace,
                },
                resource: ManagedResource::FileType,
            },
            rollback: RollbackCapability::Automatic,
            declaration_order: *order,
            meta: NodeMeta {
                managed: Some(zup_exec::ManagedOperation::ProgId(op.clone())),
                ..Default::default()
            },
        });
        *order += 1;
        managed_ids.push(prog_id);
    }
    if matches!(
        op.extension_kind,
        FileTypeOperationKind::Create
            | FileTypeOperationKind::UpdateOwned
            | FileTypeOperationKind::RestoreOwned
    ) {
        let kind = op.extension_kind;
        let extension = zup_core::FileExtension::new(&op.extension).map_err(|_| {
            TransactionPlanError::UnsupportedOperation {
                key: op.extension.clone(),
            }
        })?;
        let key = ResourceKey::FileTypeExtension { extension };
        let extension_id = OperationId::resource(file_type_kind_token(kind), &key);
        nodes.push(TransactionNode {
            id: extension_id.clone(),
            phase: Phase::ManagedIntegration,
            kind: NodeKind::ManagedIntegration {
                key,
                delta: match kind {
                    FileTypeOperationKind::Create => Delta::Create,
                    FileTypeOperationKind::RestoreOwned => Delta::RestoreOwned,
                    _ => Delta::Replace,
                },
                resource: ManagedResource::FileType,
            },
            rollback: RollbackCapability::Automatic,
            declaration_order: *order,
            meta: NodeMeta {
                managed: Some(zup_exec::ManagedOperation::Extension(op.clone())),
                ..Default::default()
            },
        });
        *order += 1;
        managed_ids.push(extension_id);
    }
    Ok(())
}

fn validate_graph(
    nodes: &[TransactionNode],
    deps: &[Dependency],
) -> Result<(), TransactionPlanError> {
    let mut ids = BTreeMap::new();
    for node in nodes {
        if ids.insert(node.id.clone(), node).is_some() {
            return Err(TransactionPlanError::DuplicateOperationId {
                id: node.id.to_string(),
            });
        }
    }
    for dep in deps {
        if dep.from == dep.to {
            return Err(TransactionPlanError::SelfDependency {
                id: dep.from.to_string(),
            });
        }
        if !ids.contains_key(&dep.from) {
            return Err(TransactionPlanError::UnknownDependency {
                id: dep.from.to_string(),
            });
        }
        if !ids.contains_key(&dep.to) {
            return Err(TransactionPlanError::UnknownDependency {
                id: dep.to.to_string(),
            });
        }
    }

    let mut graph: DiGraph<(), ()> = DiGraph::new();
    let mut index = BTreeMap::new();
    for node in nodes {
        index.insert(node.id.clone(), graph.add_node(()));
    }
    for dep in deps {
        graph.add_edge(index[&dep.from], index[&dep.to], ());
    }
    if is_cyclic_directed(&graph) {
        return Err(TransactionPlanError::Cycle);
    }
    Ok(())
}

fn topological_ids(
    nodes: &[TransactionNode],
    deps: &[Dependency],
    reverse: bool,
) -> Result<Vec<OperationId>, TransactionPlanError> {
    let meta: BTreeMap<OperationId, &TransactionNode> =
        nodes.iter().map(|n| (n.id.clone(), n)).collect();
    let mut incoming: BTreeMap<OperationId, usize> =
        nodes.iter().map(|node| (node.id.clone(), 0)).collect();
    let mut outgoing: BTreeMap<OperationId, Vec<OperationId>> = BTreeMap::new();
    for dep in deps {
        let (from, to) = if reverse {
            (&dep.to, &dep.from)
        } else {
            (&dep.from, &dep.to)
        };
        *incoming.get_mut(to).ok_or(TransactionPlanError::Cycle)? += 1;
        outgoing.entry(from.clone()).or_default().push(to.clone());
    }
    let mut ready: Vec<OperationId> = incoming
        .iter()
        .filter(|(_, count)| **count == 0)
        .map(|(id, _)| id.clone())
        .collect();
    let mut ids = Vec::with_capacity(nodes.len());
    while !ready.is_empty() {
        ready.sort_by(|a, b| {
            let (a_node, b_node) = (meta[a], meta[b]);
            if reverse {
                b_node
                    .phase
                    .cmp(&a_node.phase)
                    .then(b_node.declaration_order.cmp(&a_node.declaration_order))
                    .then(b.cmp(a))
            } else {
                a_node
                    .phase
                    .cmp(&b_node.phase)
                    .then(a_node.declaration_order.cmp(&b_node.declaration_order))
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
        ids.push(next);
    }
    if ids.len() == nodes.len() {
        Ok(ids)
    } else {
        Err(TransactionPlanError::Cycle)
    }
}
