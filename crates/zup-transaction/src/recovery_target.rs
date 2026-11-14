use zup_core::{AppId, RecoveryKind, RecoverySummary, SelectedScope, TransactionId};

use crate::{
    FilesystemTransactionStore, PublicationState, ReconciliationState, StoreError,
    TransactionPhase, TransactionRecord, TransactionStore,
};

/// A record loaded and identity checked through the journal volume.
#[derive(Debug, Clone)]
pub struct RecoveryTarget {
    record: TransactionRecord,
    kind: RecoveryKind,
}

impl RecoveryTarget {
    pub fn record(&self) -> &TransactionRecord {
        &self.record
    }

    pub fn summary(&self) -> RecoverySummary {
        let record = &self.record;
        RecoverySummary {
            transaction: record.transaction_id,
            installation: record.plan.installation,
            app: record.app_id.clone(),
            version: record.app_version.clone(),
            scope: record.scope,
            target: record.target,
            directory: record.plan.install_directory.clone(),
            kind: self.kind,
            detail: record.recovery_detail(),
            cleanup: record
                .plan
                .nodes
                .iter()
                .filter(|node| !record.nodes[&node.id].collection_complete())
                .filter_map(|node| match &node.kind {
                    zup_plan::NodeKind::Cleanup { operation } => Some(match &operation.candidate {
                        zup_plan::CleanupCandidate::Generation(generation) => {
                            zup_core::RecoveryCleanup::Generation {
                                directory: generation.materialization.directory().clone(),
                            }
                        }
                        zup_plan::CleanupCandidate::Fixed(file) => {
                            zup_core::RecoveryCleanup::FixedFile {
                                path: file.path.clone(),
                            }
                        }
                        zup_plan::CleanupCandidate::History(history) => {
                            zup_core::RecoveryCleanup::Metadata {
                                transaction: history.source,
                            }
                        }
                    }),
                    _ => None,
                })
                .collect(),
        }
    }
}

impl FilesystemTransactionStore {
    pub fn active_launchers(
        &self,
        installation: &zup_plan::InstallationRecord,
    ) -> Result<Option<zup_core::ActiveLaunchers>, String> {
        if !installation.is_installed() {
            return Ok(None);
        }
        let record = self
            .load(&installation.commit.transaction)
            .map_err(|error| error.to_string())?;
        crate::verify_installation_evidence(installation, &record)?;
        if record.phase != TransactionPhase::Committed
            || self
                .pending_recovery(&installation.app_id, installation.scope)
                .map_err(|error| error.to_string())?
                .is_some_and(|pending| pending.record.transaction_id != record.transaction_id)
        {
            return Ok(None);
        }
        let launchers = installation
            .current
            .requirements
            .launchers
            .iter()
            .filter_map(|launcher| {
                let Some(zup_plan::OwnedResource::Launcher {
                    installed:
                        zup_plan::LauncherState::Launcher {
                            target,
                            arguments,
                            working_directory,
                        },
                    ..
                }) = installation.current.resources.get(&launcher.key)
                else {
                    return None;
                };
                Some(zup_core::ActiveLauncher {
                    name: launcher.name.to_string(),
                    target: target.clone(),
                    arguments: arguments.clone(),
                    working_directory: working_directory.clone(),
                })
            })
            .collect();
        Ok(Some(zup_core::ActiveLaunchers {
            transaction: record.transaction_id,
            installation: installation.id,
            app: record.app_id,
            scope: record.scope,
            target: record.target,
            launchers,
        }))
    }

    pub fn recovery_target(
        &self,
        app: &AppId,
        scope: SelectedScope,
        id: TransactionId,
    ) -> Result<Option<RecoveryTarget>, StoreError> {
        let record = self.load(&id)?;
        if record.app_id != *app || record.scope != scope {
            return Err(StoreError::Rejected {
                id: id.to_string(),
                reason: "recovery identity mismatch".into(),
            });
        }
        let kind = match record.phase {
            TransactionPhase::Committed if !record.cleanup_complete() => {
                RecoveryKind::CommittedCleanup
            }
            TransactionPhase::Committed
                if record.reconciliation != ReconciliationState::Complete =>
            {
                RecoveryKind::CommittedReconciliation
            }
            TransactionPhase::Committed | TransactionPhase::RolledBack => return Ok(None),
            _ if record.rollback_failure.is_some()
                && record.resolution.is_none()
                && !crate::resolution::unsafe_compensation(&record).is_empty() =>
            {
                RecoveryKind::OperatorRequired
            }
            _ if record.publication != PublicationState::NotRequested => {
                RecoveryKind::PublicationUncertain
            }
            _ => RecoveryKind::Interrupted,
        };
        Ok(Some(RecoveryTarget { record, kind }))
    }

    pub fn pending_recovery(
        &self,
        app: &AppId,
        scope: SelectedScope,
    ) -> Result<Option<RecoveryTarget>, StoreError> {
        let mut pending = None;
        for id in self.list_ids()? {
            if let Some(target) = self.recovery_target(app, scope, id)? {
                if pending.is_some() {
                    return Err(StoreError::Rejected {
                        id: id.to_string(),
                        reason: "multiple unresolved transactions require operator inspection"
                            .into(),
                    });
                }
                pending = Some(target);
            }
        }
        Ok(pending)
    }
}
