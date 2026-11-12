use crate::{
    InstallationPublisher, NodeState, OperationExecutor, TransactionError, TransactionOutcome,
    TransactionRecord, TransactionStore,
};
use serde::{Deserialize, Serialize};
use zup_core::{AppId, SelectedScope, Sha256Digest, TransactionId};
use zup_plan::{NodeKind, OperationId};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum RecoveryRequest {
    Retry {},
    Assess {},
    CompleteForward { decision: Box<RecoveryDecision> },
}

impl Default for RecoveryRequest {
    fn default() -> Self {
        Self::Retry {}
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryDecision {
    pub transaction: TransactionId,
    pub app: AppId,
    pub scope: SelectedScope,
    pub version: semver::Version,
    pub plan: Sha256Digest,
    pub revision: u64,
    pub evidence: Sha256Digest,
}
impl RecoveryDecision {
    pub(crate) fn matches(&self, record: &TransactionRecord) -> bool {
        self.transaction == record.transaction_id
            && self.app == record.app_id
            && self.scope == record.scope
            && self.version == record.app_version
            && self.plan == record.plan_hash
            && self.revision < record.revision
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryCause {
    ModifiedFilePreserved,
    IncompatibleDataExposure,
    NativeCompensationRefused,
    NativeEvidenceRequired,
    CompensationAlreadyStarted,
    UnsupportedForwardEffect,
    AuthenticatedEffectRequired,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryBlocker {
    pub operation: Option<OperationId>,
    pub cause: RecoveryCause,
    pub detail: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryAssessment {
    pub expected_effects: std::collections::BTreeMap<OperationId, NodeState>,
    pub transaction: TransactionId,
    pub causes: Vec<RecoveryBlocker>,
    pub blockers: Vec<RecoveryBlocker>,
    pub complete_forward: Option<RecoveryDecision>,
}

pub(crate) fn unsafe_compensation(record: &TransactionRecord) -> Vec<RecoveryBlocker> {
    let exposed = record.plan.nodes.iter().any(|node| {
        matches!(
            node.kind,
            NodeKind::FileMutation { .. }
                | NodeKind::NativeService { .. }
                | NodeKind::BackendOperation { .. }
                | NodeKind::Generation {
                    operation: zup_plan::GenerationOperation::Activate,
                    ..
                }
        ) && !matches!(record.nodes.get(&node.id), Some(NodeState::Pending))
    });
    record.plan.nodes.iter().filter_map(|node| {
        let (cause, detail) = match &node.kind {
            NodeKind::PersistentDirectory { operation } if exposed && operation.intent.previous.as_ref()
                .is_some_and(|previous| !previous.directory.compatibility.accepts(&operation.intent.directory.compatibility)) =>
                (RecoveryCause::IncompatibleDataExposure, format!("Data resource {} at {:?}: previous compatibility {:?}, intended compatibility {:?}. Application code may have exposed a newer format. Binary rollback requires an authenticated compatible data restore; the engine has no actual schema readback or backup-restore receipt.", operation.intent.directory.id, operation.intent.binding, operation.intent.previous.as_ref().map(|previous| &previous.directory.compatibility), operation.intent.directory.compatibility)),
            NodeKind::FileMutation { delta: zup_plan::FileOperationKind::RepairOwned, .. }
                if record.receipt(&node.id).is_some() =>
                {
                    let Some(crate::OperationReceipt::ReplaceFile { destination, backup_path, previous_object, object, .. }) = record.receipt(&node.id) else { return None; };
                    (RecoveryCause::ModifiedFilePreserved, format!("Repair of {destination} displaced external instance {previous_object:?}, preserved at {backup_path}. The authenticated replacement is {object:?}. Forward completion verifies both and owns only the replacement. Restoration or disposal of the external instance requires separate intervention."))
                },
            NodeKind::NativeService { operation } if record.receipt(&node.id).is_some() => {
                let zup_plan::NativeServiceOperation::Configure { service, .. } = operation.as_ref() else { return None; };
                (RecoveryCause::NativeCompensationRefused, service.validate_compensation().err()?.to_string())
            }
            _ => return None,
        };
        Some(RecoveryBlocker { operation: Some(node.id.clone()), cause, detail })
    }).collect()
}

pub fn assess_recovery<E: OperationExecutor>(
    record: &TransactionRecord,
    executor: &mut E,
) -> Result<RecoveryAssessment, TransactionError>
where
    E::Error: std::fmt::Display,
{
    record
        .validate()
        .map_err(|error| TransactionError::InvalidState(error.to_string()))?;
    let mut result = RecoveryAssessment {
        expected_effects: record.nodes.clone(),
        transaction: record.transaction_id,
        causes: unsafe_compensation(record),
        blockers: Vec::new(),
        complete_forward: None,
    };
    if result.causes.is_empty()
        || !matches!(
            record.phase,
            crate::TransactionPhase::RecoveryRequired {
                resume: crate::RecoveryPhase::Applying
            }
        )
        || record.resolution.is_some()
        || record.publication != crate::PublicationState::NotRequested
    {
        result.blockers.push(RecoveryBlocker { operation: None, cause: RecoveryCause::NativeEvidenceRequired,
            detail: "No new unsafe-compensation resolution is admissible in this phase. Retry can reconcile uncertain effects; an accepted forward intent resumes without another decision.".into() });
    }
    if result
        .causes
        .iter()
        .any(|cause| cause.cause == RecoveryCause::IncompatibleDataExposure)
    {
        result.blockers.push(RecoveryBlocker { operation: None, cause: RecoveryCause::IncompatibleDataExposure,
            detail: "The engine can verify directory identity and access, but has no application schema readback or authenticated data-backup restore receipt. Obtain an application-specific schema assessment and verified restore procedure. Neither forward compatibility nor safe binary rollback can be established here.".into() });
    }
    for node in &record.plan.nodes {
        let state = &record.nodes[&node.id];
        let mut blocked = |cause, detail| {
            result.blockers.push(RecoveryBlocker {
                operation: Some(node.id.clone()),
                cause,
                detail,
            })
        };
        if matches!(
            state,
            NodeState::RollingBack { .. } | NodeState::Compensated { .. } | NodeState::RolledBack
        ) {
            blocked(RecoveryCause::CompensationAlreadyStarted, "Compensation has started. Restore and verify native effects under a separate recovery procedure; forward replay is unavailable.".into());
            continue;
        }
        if matches!(
            node.kind,
            NodeKind::NativeService { .. }
                | NodeKind::ServiceControl { .. }
                | NodeKind::BackendOperation { .. }
                | NodeKind::Generation { .. }
                | NodeKind::NativeCache { .. }
        ) {
            blocked(RecoveryCause::UnsupportedForwardEffect, "Forward resolution of this effect requires a native lifecycle assessment not provided by this engine. Preserve the journal and retained roots; obtain platform-specific intervention.".into());
            continue;
        }
        match state {
            NodeState::Running | NodeState::Failed => blocked(RecoveryCause::NativeEvidenceRequired, "An effect lacks a settled receipt. Ordinary recovery must first reconcile it; unknown effects cannot be authorized away.".into()),
            NodeState::Applied { receipt } | NodeState::Verified { receipt } => {
                // Stage instances are consumed by fixed placement. The placement receipt proves both instances.
                if !matches!(node.kind, NodeKind::Barrier | NodeKind::ReserveInstallation | NodeKind::Retention { .. } | NodeKind::StageFile { .. })
                    && let Err(error) = executor.verify(node, receipt) {
                        blocked(RecoveryCause::NativeEvidenceRequired, format!("Expected protected receipt {receipt:?}; native readback refused: {error}. Restore the exact authorized object or provide the required access/source before reassessment."));
                    }
            }
            NodeState::Pending if !matches!(node.kind, NodeKind::Barrier | NodeKind::ReserveInstallation | NodeKind::PublishInstallation | NodeKind::Retention { .. } | NodeKind::Cleanup { .. }) =>
                blocked(RecoveryCause::AuthenticatedEffectRequired, "This effect has not completed with an authenticated native receipt. This resolution only finalizes already placed content; it cannot supply missing source, restore a backup, or authorize new data/native effects.".into()),
            _ => {}
        }
    }
    if result.blockers.is_empty() {
        result.complete_forward = Some(RecoveryDecision {
            transaction: record.transaction_id,
            app: record.app_id.clone(),
            scope: record.scope,
            version: record.app_version.clone(),
            plan: record.plan_hash,
            revision: record.revision,
            evidence: zup_core::hash_bytes(
                &serde_json::to_vec(record)
                    .map_err(|error| TransactionError::InvalidState(error.to_string()))?,
            ),
        });
    }
    Ok(result)
}

pub fn recover_requested<E: OperationExecutor, S: TransactionStore, P: InstallationPublisher>(
    record: TransactionRecord,
    store: &S,
    executor: &mut E,
    publisher: &P,
    request: &RecoveryRequest,
) -> Result<(TransactionRecord, TransactionOutcome), TransactionError>
where
    E::Error: std::fmt::Display,
{
    let mut fresh = store.load(&record.transaction_id)?;
    if fresh != record {
        return Err(TransactionError::InvalidState(
            "recovery journal changed; reassess under the installation lock".into(),
        ));
    }
    match request {
        RecoveryRequest::Retry {} => crate::recover(fresh, store, executor, publisher),
        RecoveryRequest::Assess {} => {
            let assessment = assess_recovery(&fresh, executor)?;
            Ok((fresh, TransactionOutcome::Assessed(Box::new(assessment))))
        }
        RecoveryRequest::CompleteForward { decision } => {
            if fresh.resolution.as_ref() != Some(decision.as_ref()) {
                let assessment = assess_recovery(&fresh, executor)?;
                if assessment.complete_forward.as_ref() != Some(decision.as_ref()) {
                    return Err(TransactionError::InvalidState(format!(
                        "stale or inadmissible forward decision; expected {:?}, supplied {:?}; blockers: {:?}",
                        assessment.complete_forward, decision, assessment.blockers
                    )));
                }
                let revision = fresh.revision;
                fresh.resolution = Some(*decision.clone());
                fresh.touch();
                store.compare_and_swap(revision, &fresh)?;
                crate::fault_injection::boundary(
                    crate::fault_injection::Boundary::ResolutionIntentPersisted,
                    &fresh.transaction_id.to_string(),
                );
            }
            crate::recover(fresh, store, executor, publisher)
        }
    }
}
