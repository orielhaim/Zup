use std::collections::BTreeMap;
use std::path::PathBuf;
use zup_storage::PublishError;

use crate::OperationReceipt;
use jiff::Timestamp;
use miette::Diagnostic;
use semver::Version;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use zup_core::{AppId, SelectedScope, Sha256Digest, Target};
use zup_plan::NodeKind;

use zup_core::TransactionId;
use zup_plan::DeploymentPlan;
use zup_plan::OperationId;

// Older engines cannot resume an authorized forward resolution.
pub const JOURNAL_SCHEMA: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryPhase {
    Prepared,
    Applying,
    RollingBack,
}

impl From<RecoveryPhase> for TransactionPhase {
    fn from(phase: RecoveryPhase) -> Self {
        match phase {
            RecoveryPhase::Prepared => Self::Prepared,
            RecoveryPhase::Applying => Self::Applying,
            RecoveryPhase::RollingBack => Self::RollingBack,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransactionPhase {
    Prepared,
    Applying,
    RollingBack,
    RolledBack,
    Committed,
    RecoveryRequired { resume: RecoveryPhase },
}

impl TransactionPhase {
    pub fn transition(self, next: Self) -> Result<Self, PhaseError> {
        use TransactionPhase::*;
        let ok = matches!(
            (self, next),
            (Prepared, Applying)
                | (Prepared, RollingBack)
                | (Applying, Committed)
                | (Applying, RollingBack)
                | (
                    Applying,
                    RecoveryRequired {
                        resume: RecoveryPhase::Applying
                    }
                )
                | (RollingBack, RolledBack)
                | (
                    RollingBack,
                    RecoveryRequired {
                        resume: RecoveryPhase::RollingBack
                    }
                )
                | (
                    Prepared,
                    RecoveryRequired {
                        resume: RecoveryPhase::Prepared
                    }
                )
        ) || matches!(self, RecoveryRequired { resume } if Self::from(resume) == next);
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

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
#[error("invalid transaction phase transition {from:?} → {to:?}")]
pub struct PhaseError {
    pub from: TransactionPhase,
    pub to: TransactionPhase,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeState {
    Pending,
    Running,
    Applied {
        receipt: Box<crate::executor::OperationReceipt>,
    },
    Verified {
        receipt: Box<crate::executor::OperationReceipt>,
    },
    RollingBack {
        receipt: Box<crate::executor::OperationReceipt>,
    },
    RolledBack,
    Compensated {
        receipt: Box<crate::executor::OperationReceipt>,
    },
    Failed,
}

impl NodeState {
    pub fn is_rolled_back(&self) -> bool {
        matches!(self, Self::RolledBack | Self::Compensated { .. })
    }

    pub fn transition(self, next: Self) -> Result<Self, NodeStateError> {
        use NodeState::*;
        let ok = matches!(
            (&self, &next),
            (Pending, Running)
                | (Running, Applied { .. })
                | (Running, Failed)
                | (Running, Pending)
                | (Applied { .. }, Verified { .. })
                | (Applied { .. }, RollingBack { .. })
                | (Applied { .. }, Compensated { .. })
                | (Verified { .. }, RollingBack { .. })
                | (RollingBack { .. }, Compensated { .. })
                | (RollingBack { .. }, Failed)
                | (Failed, RollingBack { .. })
                | (Failed, RolledBack)
                | (Pending, RollingBack { .. })
                | (Pending, RolledBack)
        ) || matches!(
            (&self, &next),
            (RollingBack { receipt: before }, RollingBack { receipt: after })
                if matches!((before.as_ref(), after.as_ref()),
                    (crate::OperationReceipt::ServiceControl(before), crate::OperationReceipt::ServiceControl(after))
                        if before.operation == after.operation
                            && matches!(after.authority, crate::ServiceControlAuthority::ReobservedQuiescent { .. }))
        );
        let preserves_evidence = match (&self, &next) {
            (
                Applied { receipt: before } | RollingBack { receipt: before },
                Compensated { receipt: after },
            ) => before == after,
            _ => true,
        };
        if ok && preserves_evidence {
            Ok(next)
        } else {
            Err(NodeStateError {
                from: self,
                to: next,
            })
        }
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[error("invalid node state transition")]
pub struct NodeStateError {
    pub from: NodeState,
    pub to: NodeState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransactionRecord {
    pub resolution: Option<crate::RecoveryDecision>,
    pub reservation: Option<crate::InstallationReservation>,
    pub publication: crate::PublicationState,
    pub failure: Option<TransactionFailure>,
    pub rollback_failure: Option<TransactionFailure>,
    pub recovery_failure: Option<TransactionFailure>,
    pub reconciliation: crate::ReconciliationState,
    pub release: Option<zup_core::ReleaseIdentity>,
    pub schema: u32,
    pub transaction_id: TransactionId,
    pub app_id: AppId,
    pub scope: SelectedScope,
    pub target: Target,
    pub app_version: Version,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    pub plan_hash: Sha256Digest,
    pub plan: DeploymentPlan,
    pub phase: TransactionPhase,
    pub revision: u64,
    pub nodes: BTreeMap<OperationId, NodeState>,
}

impl TransactionRecord {
    pub fn recovery_detail(&self) -> String {
        let detail = self
            .recovery_failure
            .as_ref()
            .or(self.rollback_failure.as_ref())
            .or(self.failure.as_ref())
            .map(|failure| failure.message.as_str());
        if self.recovery_failure.is_none()
            && self.rollback_failure.is_some()
            && self.resolution.is_none()
            && !crate::resolution::unsafe_compensation(self).is_empty()
        {
            return format!(
                "{} Assessment is available through recover --transaction {} --assess. Review complete_forward and blockers, then pass the exact complete_forward JSON object with --decision FILE. No data restore or forced native rollback is supported.",
                detail.unwrap_or("Unsafe compensation refused."),
                self.transaction_id
            );
        }
        if self.phase == TransactionPhase::Committed {
            format!(
                "Deployment committed; {}",
                detail.unwrap_or("post-commit work requires recovery")
            )
        } else {
            detail
                .unwrap_or(
                    "Transaction effects require recovery before another operation can proceed",
                )
                .to_owned()
        }
    }

    pub fn new(
        transaction_id: TransactionId,
        app_id: AppId,
        scope: SelectedScope,
        app_version: Version,
        plan: DeploymentPlan,
    ) -> Self {
        let now = Timestamp::now();
        let target = plan.target;
        let plan_hash = plan.fingerprint();
        let mut nodes = BTreeMap::new();
        for node in &plan.nodes {
            nodes.insert(node.id.clone(), NodeState::Pending);
        }
        Self {
            resolution: None,
            reservation: None,
            publication: crate::PublicationState::NotRequested,
            failure: None,
            rollback_failure: None,
            recovery_failure: None,
            reconciliation: crate::ReconciliationState::Pending { failure: None },
            release: None,
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

    pub fn cleanup_complete(&self) -> bool {
        self.plan
            .nodes
            .iter()
            .filter(|node| node.kind.is_post_commit())
            .all(|node| matches!(self.nodes.get(&node.id), Some(NodeState::Verified { .. })))
    }

    pub fn validate(&self) -> Result<(), CorruptReason> {
        if self.resolution.as_ref().is_some_and(|decision| {
            !decision.matches(self)
                || !matches!(
                    self.phase,
                    TransactionPhase::Applying
                        | TransactionPhase::Committed
                        | TransactionPhase::RecoveryRequired {
                            resume: RecoveryPhase::Applying
                        }
                )
                || self.nodes.values().any(|state| {
                    matches!(
                        state,
                        NodeState::RollingBack { .. }
                            | NodeState::Compensated { .. }
                            | NodeState::RolledBack
                    )
                })
        }) {
            return Err(CorruptReason::InvalidPlan);
        }
        if self.plan.requirements.app.id != self.app_id
            || self.plan.requirements.app.version != self.app_version
            || self.plan.requirements.scope != self.scope
        {
            return Err(CorruptReason::InvalidPlan);
        }
        let reserved = self
            .nodes
            .get(&zup_plan::OperationId::new(zup_plan::OperationId::RESERVE));
        if self
            .reservation
            .as_ref()
            .is_some_and(|reservation| !reservation.matches(self))
            || match (&self.reservation, reserved) {
                (
                    None,
                    Some(
                        NodeState::Pending
                        | NodeState::Running
                        | NodeState::Failed
                        | NodeState::RolledBack,
                    ),
                ) => false,
                (Some(_), Some(NodeState::Applied { receipt })) => {
                    **receipt != crate::OperationReceipt::Control
                }
                (Some(_), Some(NodeState::RolledBack | NodeState::Compensated { .. })) => false,
                _ => true,
            }
        {
            return Err(CorruptReason::InvalidPhase);
        }
        if self.reservation.is_none()
            && self.plan.nodes.iter().any(|node| {
                !matches!(node.kind, zup_plan::NodeKind::ReserveInstallation)
                    && node.id.as_str() != zup_plan::OperationId::BEGIN
                    && !matches!(
                        self.nodes.get(&node.id),
                        Some(NodeState::Pending | NodeState::RolledBack | NodeState::Failed)
                    )
            })
        {
            return Err(CorruptReason::InvalidPhase);
        }
        let publication_node = self
            .nodes
            .get(&zup_plan::OperationId::new(zup_plan::OperationId::COMMIT));
        let publication_valid = match self.publication {
            crate::PublicationState::NotRequested => matches!(
                publication_node,
                Some(NodeState::Pending | NodeState::RolledBack)
            ),
            crate::PublicationState::Requested { .. } => {
                matches!(publication_node, Some(NodeState::Running))
            }
            crate::PublicationState::Published { .. } => matches!(publication_node,
                Some(NodeState::Applied { receipt }) if **receipt == crate::OperationReceipt::Control),
        };
        if !publication_valid {
            return Err(CorruptReason::InvalidPhase);
        }
        if !matches!(self.publication, crate::PublicationState::NotRequested)
            && self
                .plan
                .nodes
                .iter()
                .filter(|node| {
                    !node.kind.is_post_commit()
                        && !matches!(node.kind, zup_plan::NodeKind::PublishInstallation)
                })
                .any(|node| match self.nodes.get(&node.id) {
                    Some(NodeState::Verified { .. }) => !node.kind.requires_verification(),
                    Some(NodeState::Applied { .. }) => node.kind.requires_verification(),
                    _ => true,
                })
        {
            return Err(CorruptReason::InvalidPhase);
        }
        if (self.phase == TransactionPhase::Committed
            && !matches!(self.publication, crate::PublicationState::Published { .. }))
            || (!matches!(self.publication, crate::PublicationState::NotRequested)
                && !matches!(
                    self.phase,
                    TransactionPhase::Applying
                        | TransactionPhase::Committed
                        | TransactionPhase::RecoveryRequired {
                            resume: RecoveryPhase::Applying
                        }
                ))
        {
            return Err(CorruptReason::InvalidPhase);
        }
        for failure in [
            &self.failure,
            &self.rollback_failure,
            &self.recovery_failure,
        ]
        .into_iter()
        .flatten()
        {
            if failure.message.is_empty()
                || failure.message.len() > 16384
                || failure
                    .operation
                    .as_ref()
                    .is_some_and(|id| !self.nodes.contains_key(id))
            {
                return Err(CorruptReason::InvalidPlan);
            }
        }
        if self.reconciliation == crate::ReconciliationState::Complete
            && (self.phase != TransactionPhase::Committed || !self.cleanup_complete())
        {
            return Err(CorruptReason::InvalidPhase);
        }
        if self.schema != JOURNAL_SCHEMA {
            return Err(CorruptReason::UnsupportedSchema { found: self.schema });
        }
        if self.plan_hash != self.plan.fingerprint() {
            return Err(CorruptReason::PlanHashMismatch);
        }
        if let Some(release) = &self.release
            && (release.app_id != self.app_id
                || release.target != self.target
                || release.version != self.app_version.to_string())
        {
            return Err(CorruptReason::InvalidPlan);
        }
        if self.target != self.plan.target {
            return Err(CorruptReason::TargetMismatch);
        }
        self.plan
            .validate()
            .map_err(|_| CorruptReason::InvalidPlan)?;
        for node in &self.plan.nodes {
            if node
                .meta
                .privilege
                .is_some_and(|privilege| privilege != self.scope.authorization())
                || node
                    .meta
                    .removal
                    .as_ref()
                    .is_some_and(|removal| removal.scope != self.scope)
            {
                return Err(CorruptReason::InvalidPlan);
            }
            let Some(state) = self.nodes.get(&node.id) else {
                return Err(CorruptReason::NodeStateMismatch);
            };
            if matches!(state, NodeState::Verified { receipt }
            if matches!(receipt.as_ref(), OperationReceipt::NativeServiceCompensation { .. } | OperationReceipt::NativeCache {
                completion: crate::NativeCacheCompletion::CompensationOnly, ..
            })) {
                return Err(CorruptReason::InvalidReceipt);
            }
            if let NodeKind::PersistentDirectory { operation } = &node.kind
                && let Some(OperationReceipt::PersistentDirectory(receipt)) = self.receipt(&node.id)
                && operation.intent.previous.is_none()
                && receipt.owned.origin != self.transaction_id
            {
                return Err(CorruptReason::InvalidReceipt);
            }
            if let NodeState::Applied { receipt }
            | NodeState::Verified { receipt }
            | NodeState::RollingBack { receipt }
            | NodeState::Compensated { receipt } = state
                && (receipt.validate().is_err()
                    || !receipt_matches_node(node, receipt, self.target))
            {
                return Err(CorruptReason::InvalidReceipt);
            }
            if let NodeKind::FileMutation { key, .. } = &node.kind
                && let Some(receipt) = self.receipt(&node.id)
                && let OperationReceipt::CreateFile { object, .. }
                | OperationReceipt::ReplaceFile { object, .. } = receipt
                && !matches!(self.receipt(&zup_plan::OperationId::stage_file(key)),
                    Some(OperationReceipt::StageFile { object: staged, .. }) if staged == object)
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

    pub fn touch(&mut self) {
        self.revision = self.revision.saturating_add(1);
        self.updated_at = Timestamp::now();
    }

    pub fn receipt(&self, id: &OperationId) -> Option<&crate::executor::OperationReceipt> {
        match self.nodes.get(id) {
            Some(NodeState::Applied { receipt })
            | Some(NodeState::Verified { receipt })
            | Some(NodeState::RollingBack { receipt })
            | Some(NodeState::Compensated { receipt }) => Some(receipt),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransactionFailure {
    pub operation: Option<OperationId>,
    pub message: String,
}

fn phase_accepts(
    phase: TransactionPhase,
    node: &zup_plan::DeploymentNode,
    state: &NodeState,
) -> bool {
    use zup_plan::NodeKind;
    if node.kind.is_post_commit() {
        return match phase {
            TransactionPhase::Committed => matches!(
                state,
                NodeState::Pending
                    | NodeState::Running
                    | NodeState::Applied { .. }
                    | NodeState::Verified { .. }
            ),
            _ => matches!(state, NodeState::Pending),
        };
    }
    match phase {
        TransactionPhase::Prepared => match &node.kind {
            NodeKind::RetainFile { .. } => matches!(
                state,
                NodeState::Pending
                    | NodeState::Running
                    | NodeState::Applied { .. }
                    | NodeState::Verified { .. }
                    | NodeState::Failed
            ),
            NodeKind::ReserveInstallation | NodeKind::Barrier | NodeKind::StageFile { .. } => {
                matches!(
                    state,
                    NodeState::Pending
                        | NodeState::Running
                        | NodeState::Applied { .. }
                        | NodeState::Failed
                )
            }
            _ => matches!(state, NodeState::Pending),
        },
        TransactionPhase::Applying | TransactionPhase::RollingBack => true,
        TransactionPhase::Committed => {
            if node.kind.requires_verification() {
                matches!(state, NodeState::Verified { .. })
            } else {
                matches!(state, NodeState::Applied { .. })
            }
        }
        TransactionPhase::RolledBack => matches!(
            state,
            NodeState::Pending
                | NodeState::RolledBack
                | NodeState::Compensated { .. }
                | NodeState::Failed
        ),
        TransactionPhase::RecoveryRequired { .. } => true,
    }
}

fn receipt_matches_node(
    node: &zup_plan::DeploymentNode,
    receipt: &crate::executor::OperationReceipt,
    target: zup_core::Target,
) -> bool {
    use crate::executor::OperationReceipt;
    use zup_plan::NodeKind;
    let object = match receipt {
        OperationReceipt::StageFile { object, .. }
        | OperationReceipt::RetainedFile { object, .. }
        | OperationReceipt::CreateFile { object, .. }
        | OperationReceipt::ReplaceFile { object, .. }
        | OperationReceipt::RemoveFile { object, .. } => Some(object),
        _ => None,
    };
    if object.is_some_and(|object| object.operating_system() != target.operating_system()) {
        return false;
    }
    let expected_content = |size: u64, digest: &zup_core::Sha256Digest| {
        node.meta.expected_size == Some(size) && Some(*digest) == node.meta.expected_sha256
    };
    let destination_matches = |_key: &zup_core::ResourceKey, destination: &str| {
        node.meta.destination.as_ref().is_some_and(|expected| {
            zup_core::TargetPath::new(target, destination)
                .is_ok_and(|observed| expected.equivalent(&observed))
        })
    };
    if let OperationReceipt::CreateFile {
        destination,
        created_directories,
        ..
    } = receipt
    {
        let Ok(destination) = zup_core::TargetPath::new(target, destination) else {
            return false;
        };
        if created_directories.iter().any(|directory| {
            !zup_core::TargetPath::new(target, directory).is_ok_and(|directory| {
                directory.parent().is_some()
                    && directory != destination
                    && destination.starts_with(&directory)
            })
        }) {
            return false;
        }
    }
    let content_matches = match (&node.kind, receipt) {
        (
            NodeKind::RetainFile { .. },
            OperationReceipt::RetainedFile {
                object,
                destination,
                size,
                sha256,
            },
        ) => {
            destination.target() == &target
                && node.meta.destination.as_ref() == Some(destination)
                && expected_content(*size, sha256)
                && matches!(node.meta.file_precondition, Some(zup_plan::FilePrecondition::Exact { object: expected, size: expected_size, sha256: expected_hash })
                    if expected == *object && expected_size == *size && expected_hash == *sha256)
        }
        (NodeKind::StageFile { .. }, OperationReceipt::StageFile { size, sha256, .. }) => {
            expected_content(*size, sha256)
        }
        (
            NodeKind::FileMutation { key, .. },
            OperationReceipt::CreateFile {
                destination,
                installed_sha256,
                installed_size,
                executable,
                ..
            },
        ) => {
            destination_matches(key, destination)
                && expected_content(*installed_size, installed_sha256)
                && node.meta.executable.unwrap_or(false) == *executable
        }
        (
            NodeKind::FileMutation { key, .. },
            OperationReceipt::ReplaceFile {
                previous_object,
                destination,
                new_sha256,
                new_size,
                executable,
                previous_sha256,
                previous_size,
                ..
            },
        ) => {
            destination_matches(key, destination)
                && expected_content(*new_size, new_sha256)
                && node.meta.executable.unwrap_or(false) == *executable
                && matches!(node.meta.file_precondition, Some(zup_plan::FilePrecondition::Exact { object, size, sha256 })
                    if object == *previous_object && size == *previous_size && *previous_sha256 == sha256)
        }
        (
            NodeKind::FileRemoval { .. },
            OperationReceipt::RemoveFile {
                object,
                destination,
                sha256,
                size,
                ..
            },
        ) => node.meta.removal.as_ref().is_some_and(|removal| {
            removal.destination == *destination
                && removal.instance.object == *object
                && removal.sha256 == *sha256
                && removal.size == *size
        }),
        _ => true,
    };
    if !content_matches {
        return false;
    }
    match (&node.kind, receipt) {
        (NodeKind::Cleanup { operation }, OperationReceipt::Cleanup { intent, .. }) => {
            *intent == operation.fingerprint()
        }
        (
            NodeKind::PersistentDirectory { operation },
            OperationReceipt::PersistentDirectory(receipt),
        ) => {
            let owned = &receipt.owned;
            receipt.stage == operation.stage
                && owned.installation == operation.intent.installation
                && owned.directory == operation.intent.directory
                && owned.ownership.binding() == operation.intent.binding
                && operation.intent.previous.as_ref().is_none_or(|previous| {
                    previous.ownership == owned.ownership && previous.origin == owned.origin
                })
        }
        (
            NodeKind::NativeCache { operation },
            OperationReceipt::NativeCache { intent, completion },
        ) => {
            *intent == operation.fingerprint()
                && match completion {
                    crate::NativeCacheCompletion::CompensationOnly => true,
                    crate::NativeCacheCompletion::Completed { tool, .. } => {
                        tool.target() == operation.directory.target()
                            && tool.as_str()
                                == format!("/usr/bin/{}", operation.kind.executable_name())
                    }
                }
        }
        (NodeKind::Generation { operation, intent }, OperationReceipt::Generation(receipt)) => {
            receipt.matches(*operation, intent)
        }
        (NodeKind::ServiceControl { operation }, OperationReceipt::ServiceControl(receipt)) => {
            operation.as_ref() == &receipt.operation
        }
        (NodeKind::NativeService { operation }, OperationReceipt::NativeService(receipt)) => {
            receipt.matches(operation)
        }
        (
            NodeKind::NativeService { operation },
            OperationReceipt::NativeServiceCompensation { intent },
        ) => operation.source_key().is_some() && *intent == operation.fingerprint(),
        (
            NodeKind::Retention { .. }
            | NodeKind::ReserveInstallation
            | NodeKind::Barrier
            | NodeKind::PublishInstallation,
            OperationReceipt::Control,
        ) => true,
        (NodeKind::StageFile { .. }, OperationReceipt::StageFile { .. }) => true,
        (NodeKind::RetainFile { .. }, OperationReceipt::RetainedFile { .. }) => true,
        (
            NodeKind::FileMutation {
                delta:
                    zup_plan::FileOperationKind::Create | zup_plan::FileOperationKind::RestoreOwned,
                ..
            },
            OperationReceipt::CreateFile { .. },
        ) => true,
        (
            NodeKind::FileMutation {
                delta:
                    zup_plan::FileOperationKind::Replace | zup_plan::FileOperationKind::RepairOwned,
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

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum CorruptReason {
    #[error("missing journal")]
    Missing,
    #[error("invalid JSON: {0}")]
    InvalidJson(String),
    #[error("unsupported journal schema {found}")]
    UnsupportedSchema { found: u32 },
    #[error("journal transaction identity does not match its directory")]
    IdentityMismatch,
    #[error("plan fingerprint mismatch")]
    PlanHashMismatch,
    #[error("plan target does not match journal target")]
    TargetMismatch,
    #[error("invalid transaction plan")]
    InvalidPlan,
    #[error("invalid operation receipt")]
    InvalidReceipt,
    #[error("invalid node state")]
    NodeStateMismatch,
    #[error("invalid transaction phase")]
    InvalidPhase,
    #[error("revision mismatch (expected {expected}, found {found})")]
    RevisionMismatch { expected: u64, found: u64 },
}

#[derive(Debug, Error, Diagnostic)]
pub enum StoreError {
    #[error(
        "installation {installation} is reserved for a different application, scope, target, or directory"
    )]
    #[diagnostic(code(zup_transaction::installation_conflict))]
    InstallationConflict {
        installation: zup_core::InstallationId,
    },
    #[error("invalid transaction directory name `{name}`")]
    #[diagnostic(code(zup_transaction::invalid_directory))]
    InvalidDirectory { name: String },

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

    #[error("journal publication failed at `{path}`: {source}")]
    #[diagnostic(code(zup_transaction::publication))]
    Publication {
        path: PathBuf,
        #[source]
        source: PublishError,
    },
}
