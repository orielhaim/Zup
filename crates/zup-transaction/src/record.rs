//! Durable transaction state.

use std::collections::BTreeMap;
use std::path::PathBuf;

use jiff::Timestamp;
use miette::Diagnostic;
use semver::Version;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use zup_core::{AppId, SelectedScope, Sha256Digest, TargetTriple};

use crate::id::{OperationId, TransactionId};
use crate::plan::TransactionPlan;

/// Persistent journal schema version.
pub const JOURNAL_SCHEMA: u32 = 1;

/// Transaction-level phase state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransactionPhase {
    Prepared,
    Applying,
    RollingBack,
    RolledBack,
    Committed,
    RecoveryRequired,
}

impl TransactionPhase {
    /// Validated phase transition.
    pub fn transition(self, next: Self) -> Result<Self, PhaseError> {
        use TransactionPhase::*;
        let ok = matches!(
            (self, next),
            (Prepared, Applying)
                | (Prepared, RollingBack)
                | (Applying, Committed)
                | (Applying, RollingBack)
                | (Applying, RecoveryRequired)
                | (RollingBack, RolledBack)
                | (RollingBack, RecoveryRequired)
                | (Prepared, RecoveryRequired)
        );
        if ok {
            Ok(next)
        } else {
            Err(PhaseError {
                from: self,
                to: next,
            })
        }
    }
}

/// Invalid phase transition.
#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
#[error("invalid transaction phase transition {from:?} → {to:?}")]
pub struct PhaseError {
    pub from: TransactionPhase,
    pub to: TransactionPhase,
}

/// Per-node durable state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeState {
    Pending,
    Running,
    Applied {
        receipt: Box<crate::executor::OperationReceipt>,
    },
    /// Applied and observed to match its receipt.
    Verified {
        receipt: Box<crate::executor::OperationReceipt>,
    },
    RollingBack,
    RolledBack,
    Failed,
}

impl NodeState {
    /// Validated node state transition.
    pub fn transition(self, next: Self) -> Result<Self, NodeStateError> {
        use NodeState::*;
        let ok = matches!(
            (&self, &next),
            (Pending, Running)
                | (Running, Applied { .. })
                | (Running, Failed)
                | (Running, Pending)
                | (Applied { .. }, Verified { .. })
                | (Applied { .. }, RollingBack)
                | (Applied { .. }, RolledBack)
                | (Verified { .. }, RollingBack)
                | (RollingBack, RolledBack)
                | (RollingBack, Failed)
                | (Failed, RollingBack)
                | (Failed, RolledBack)
                | (Pending, RollingBack)
                | (Pending, RolledBack)
        );
        if ok {
            Ok(next)
        } else {
            Err(NodeStateError {
                from: self,
                to: next,
            })
        }
    }
}

/// Invalid node state transition.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[error("invalid node state transition")]
pub struct NodeStateError {
    pub from: NodeState,
    pub to: NodeState,
}

/// Durable state of one execution attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransactionRecord {
    pub schema: u32,
    pub transaction_id: TransactionId,
    pub app_id: AppId,
    pub scope: SelectedScope,
    pub target: TargetTriple,
    pub app_version: Version,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    pub plan_hash: Sha256Digest,
    pub plan: TransactionPlan,
    pub phase: TransactionPhase,
    pub revision: u64,
    pub nodes: BTreeMap<OperationId, NodeState>,
}

impl TransactionRecord {
    /// Create a prepared record from a compiled plan.
    pub fn new(
        transaction_id: TransactionId,
        app_id: AppId,
        scope: SelectedScope,
        app_version: Version,
        plan: TransactionPlan,
    ) -> Self {
        let now = Timestamp::now();
        let target = plan.target.clone();
        let plan_hash = plan.fingerprint();
        let mut nodes = BTreeMap::new();
        for node in &plan.nodes {
            nodes.insert(node.id.clone(), NodeState::Pending);
        }
        Self {
            schema: JOURNAL_SCHEMA,
            transaction_id,
            app_id,
            scope,
            target,
            app_version,
            created_at: now,
            updated_at: now,
            plan_hash,
            plan,
            phase: TransactionPhase::Prepared,
            revision: 0,
            nodes,
        }
    }

    /// Validate internal consistency (schema, fingerprint, node map).
    pub fn validate(&self) -> Result<(), CorruptReason> {
        if self.schema != JOURNAL_SCHEMA {
            return Err(CorruptReason::UnsupportedSchema { found: self.schema });
        }
        if self.plan_hash != self.plan.fingerprint() {
            return Err(CorruptReason::PlanHashMismatch);
        }
        if self.target != self.plan.target {
            return Err(CorruptReason::TargetMismatch);
        }
        self.plan
            .validate()
            .map_err(|_| CorruptReason::InvalidPlan)?;
        for node in &self.plan.nodes {
            let Some(state) = self.nodes.get(&node.id) else {
                return Err(CorruptReason::NodeStateMismatch);
            };
            if let NodeState::Applied { receipt } | NodeState::Verified { receipt } = state
                && (receipt.validate().is_err() || !receipt_matches_node(node, receipt))
            {
                return Err(CorruptReason::InvalidReceipt);
            }
            if !phase_accepts(self.phase, node, state) {
                return Err(CorruptReason::InvalidPhase);
            }
        }
        if self.nodes.len() != self.plan.nodes.len() {
            return Err(CorruptReason::NodeStateMismatch);
        }
        for node in &self.plan.nodes {
            if !self.nodes.contains_key(&node.id) {
                return Err(CorruptReason::NodeStateMismatch);
            }
        }
        Ok(())
    }

    /// Bump revision and timestamps after a durable mutation.
    pub fn touch(&mut self) {
        self.revision = self.revision.saturating_add(1);
        self.updated_at = Timestamp::now();
    }

    /// The receipt a node landed, whether or not it has been verified.
    pub fn receipt(&self, id: &OperationId) -> Option<&crate::executor::OperationReceipt> {
        match self.nodes.get(id) {
            Some(NodeState::Applied { receipt }) | Some(NodeState::Verified { receipt }) => {
                Some(receipt)
            }
            _ => None,
        }
    }
}

/// A node's durable state must be reachable in the phase the record is in.
fn phase_accepts(
    phase: TransactionPhase,
    node: &crate::plan::TransactionNode,
    state: &NodeState,
) -> bool {
    use crate::plan::NodeKind;
    match phase {
        // Before commit intent only preparation and staging may have landed.
        TransactionPhase::Prepared => match &node.kind {
            NodeKind::Barrier | NodeKind::StageFile { .. } => matches!(
                state,
                NodeState::Pending
                    | NodeState::Running
                    | NodeState::Applied { .. }
                    | NodeState::Failed
            ),
            _ => matches!(state, NodeState::Pending),
        },
        TransactionPhase::Applying | TransactionPhase::RollingBack => true,
        // Commit is only reachable once every barrier ran and every
        // installed-state change was observed against its receipt.
        TransactionPhase::Committed => {
            if node.kind.requires_verification() {
                matches!(state, NodeState::Verified { .. })
            } else {
                matches!(state, NodeState::Applied { .. })
            }
        }
        TransactionPhase::RolledBack => matches!(
            state,
            NodeState::Pending | NodeState::RolledBack | NodeState::Failed
        ),
        TransactionPhase::RecoveryRequired => true,
    }
}

fn receipt_matches_node(
    node: &crate::plan::TransactionNode,
    receipt: &crate::executor::OperationReceipt,
) -> bool {
    use crate::executor::OperationReceipt;
    use crate::plan::NodeKind;
    match (&node.kind, receipt) {
        (NodeKind::Barrier, OperationReceipt::Control) => true,
        (NodeKind::StageFile { .. }, OperationReceipt::StageFile { .. }) => true,
        (
            NodeKind::FileMutation {
                delta: crate::input::FileDelta::Create | crate::input::FileDelta::RestoreOwned,
                ..
            },
            OperationReceipt::CreateFile { .. },
        ) => true,
        (
            NodeKind::FileMutation {
                delta: crate::input::FileDelta::Replace | crate::input::FileDelta::RepairOwned,
                ..
            },
            OperationReceipt::ReplaceFile { .. },
        ) => true,
        (NodeKind::FileRemoval { .. }, OperationReceipt::RemoveFile { .. }) => true,
        (
            NodeKind::BackendOperation { key, .. },
            OperationReceipt::Backend {
                key: receipt_key, ..
            },
        )
        | (
            NodeKind::BackendRemoval { key },
            OperationReceipt::Backend {
                key: receipt_key, ..
            },
        ) => key == receipt_key,
        _ => false,
    }
}

/// Typed journal corruption reasons.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum CorruptReason {
    #[error("missing journal")]
    Missing,
    #[error("invalid JSON: {0}")]
    InvalidJson(String),
    #[error("unsupported journal schema {found}")]
    UnsupportedSchema { found: u32 },
    #[error("plan fingerprint mismatch")]
    PlanHashMismatch,
    #[error("plan target does not match journal target")]
    TargetMismatch,
    #[error("invalid transaction plan")]
    InvalidPlan,
    #[error("invalid operation receipt")]
    InvalidReceipt,
    #[error("duplicate operation id")]
    DuplicateOperationId,
    #[error("invalid dependency")]
    InvalidDependency,
    #[error("invalid node state")]
    NodeStateMismatch,
    #[error("invalid transaction phase")]
    InvalidPhase,
    #[error("revision mismatch (expected {expected}, found {found})")]
    RevisionMismatch { expected: u64, found: u64 },
}

/// Store-level errors (including optimistic concurrency conflicts).
#[derive(Debug, Error, Diagnostic)]
pub enum StoreError {
    #[error("journal corrupt: {0}")]
    #[diagnostic(code(zup_transaction::corrupt_journal))]
    Corrupt(#[from] CorruptReason),

    #[error("transaction {id} already exists")]
    #[diagnostic(code(zup_transaction::already_exists))]
    AlreadyExists { id: String },

    #[error("compare-and-swap conflict at revision {expected}")]
    #[diagnostic(code(zup_transaction::revision_conflict))]
    RevisionConflict { expected: u64 },

    #[error("transaction {id} rejected the change: {reason}")]
    #[diagnostic(code(zup_transaction::rejected))]
    Rejected { id: String, reason: String },

    #[error("transaction {id} did not settle within {attempts} update attempts")]
    #[diagnostic(code(zup_transaction::update_exhausted))]
    UpdateExhausted { id: String, attempts: u32 },

    #[error("store I/O failed at `{path}`: {source}")]
    #[diagnostic(code(zup_transaction::store_io))]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("journal serialization failed: {0}")]
    #[diagnostic(code(zup_transaction::serialize))]
    Serialize(#[source] serde_json::Error),

    #[error("journal persistence failed: {0}")]
    #[diagnostic(code(zup_transaction::persistence))]
    Persistence(String),
}
