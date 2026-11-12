use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
#[cfg(test)]
use zup_plan::DeploymentEffects;
use zup_plan::{DeploymentPlan, NodeKind};
use zup_transaction::valid_file_key;

use zup_core::{AppId, ResourceKey, SelectedScope};
use zup_plan::InstallationRecord;
use zup_transaction::{TransactionPhase, TransactionRecord};

use crate::error::{ExecError, PathError};
use crate::fs::OwnedDirectory;

pub struct LinuxInstallationStore {
    root: PathBuf,
}

impl LinuxInstallationStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn path_for(&self, app_id: &AppId, scope: SelectedScope) -> PathBuf {
        zup_transaction::ledger_path(&self.root, app_id, scope)
    }

    pub fn load(
        &self,
        app_id: &AppId,
        scope: SelectedScope,
    ) -> Result<Option<InstallationRecord>, ExecError> {
        Ok(self
            .load_evidence(app_id, scope)?
            .map(|(installation, _)| installation))
    }

    /// Read-only planning metadata; execution requires protected journal evidence.
    pub fn observe(
        &self,
        app_id: &AppId,
        scope: SelectedScope,
    ) -> Result<Option<InstallationRecord>, ExecError> {
        let path = self.path_for(app_id, scope);
        let bytes = match read_ledger(&path, scope) {
            Ok(bytes) => bytes,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(PathError::io(&(path), source).into()),
        };
        let ledger = InstallationRecord::decode(&bytes).map_err(|_| ExecError::LedgerInvalid)?;
        if !zup_transaction::validate_installation_record(&ledger, app_id, scope) {
            return Err(ExecError::LedgerInvalid);
        }
        Ok(Some(ledger))
    }

    fn load_evidence(
        &self,
        app_id: &AppId,
        scope: SelectedScope,
    ) -> Result<Option<(InstallationRecord, TransactionRecord)>, ExecError> {
        let Some(ledger) = self.observe(app_id, scope)? else {
            return Ok(None);
        };
        use zup_transaction::TransactionStore;
        let journal = zup_transaction::FilesystemTransactionStore::private_at(&self.root)
            .load(&ledger.commit.transaction)
            .map_err(|error| ExecError::Ownership(error.to_string()))?;
        zup_transaction::verify_installation_evidence(&ledger, &journal)
            .map_err(ExecError::Ownership)?;
        Ok(Some((ledger, journal)))
    }

    fn load_authoritative(
        &self,
        app_id: &AppId,
        scope: SelectedScope,
        publishing: Option<zup_core::TransactionId>,
    ) -> Result<Option<InstallationRecord>, ExecError> {
        let Some((installation, journal)) = self.load_evidence(app_id, scope)? else {
            return Ok(None);
        };
        if journal.phase != TransactionPhase::Committed
            && publishing != Some(journal.transaction_id)
        {
            return Err(ExecError::RecoveryRequired {
                transaction: Some(journal.transaction_id),
                detail: journal.recovery_detail(),
            });
        }
        Ok(Some(installation))
    }

    pub fn validate_plan(
        &self,
        app_id: &AppId,
        scope: SelectedScope,
        plan: &DeploymentPlan,
    ) -> Result<(), ExecError> {
        plan.validate()
            .map_err(|error| ExecError::Ownership(error.to_string()))?;
        let ledger = self.load_authoritative(app_id, scope, None)?;
        let claims =
            crate::persistent::discover(&self.root, scope, app_id, plan.target, ledger.as_ref())?;
        zup_plan::validate_owned_service_inputs(plan, ledger.as_ref())
            .map_err(ExecError::Ownership)?;
        let retired_ledger = retired_owned_keys(plan)?;
        zup_transaction::validate_ownership(
            plan,
            scope,
            ledger.as_ref(),
            &retired_ledger,
            claims.as_ref(),
        )
        .map_err(|error| ExecError::Ownership(error.to_string()))?;
        for node in &plan.nodes {
            match &node.kind {
                NodeKind::PersistentDirectory { .. } => {}
                NodeKind::NativeService { operation } => {
                    operation
                        .source_node(&plan.nodes)
                        .map_err(|error| ExecError::Ownership(error.to_string()))?;
                }
                NodeKind::ServiceControl { operation } => {
                    crate::service_control::validate_binding(plan, operation, ledger.as_ref())
                        .map_err(|_| ExecError::Ownership(node.id.to_string()))?
                }
                NodeKind::FileMutation { key, .. } => {
                    if !valid_file_key(key, &plan.target) {
                        return Err(ExecError::Ownership(node.id.to_string()));
                    }
                }
                NodeKind::BackendOperation { .. } | NodeKind::BackendRemoval { .. } => {
                    return Err(ExecError::Ownership(node.id.to_string()));
                }
                NodeKind::Cleanup { .. }
                | NodeKind::NativeCache { .. }
                | NodeKind::FileRemoval { .. }
                | NodeKind::Barrier
                | NodeKind::StageFile { .. }
                | NodeKind::RetainFile { .. }
                | NodeKind::Generation { .. }
                | NodeKind::Retention { .. }
                | NodeKind::ReserveInstallation
                | NodeKind::PublishInstallation => {}
            }
        }
        if let Some(preset) = &plan.preset {
            validate_preset_plan(preset)?;
        }
        Ok(())
    }

    pub fn persistent_claims(
        &self,
        app_id: &AppId,
        scope: SelectedScope,
        target: zup_core::Target,
    ) -> Result<Option<zup_plan::PersistentClaimSelection>, ExecError> {
        let installation = self.load_authoritative(app_id, scope, None)?;
        crate::persistent::discover(&self.root, scope, app_id, target, installation.as_ref())
    }

    pub fn reconcile_installation(
        &self,
        record: &TransactionRecord,
        scope: SelectedScope,
    ) -> Result<InstallationRecord, ExecError> {
        let store = zup_transaction::FilesystemTransactionStore::private_at(&self.root);
        zup_transaction::reconcile_committed(&store, record, &self.root, |record| {
            let ledger = self
                .load_authoritative(&record.app_id, scope, None)
                .map_err(|error| error.to_string())?
                .ok_or_else(|| "committed installation record is missing".to_string())?;
            if ledger.commit.transaction != record.transaction_id {
                if ledger.id == record.plan.installation
                    && ledger.target == record.target
                    && (record.reconciliation == zup_transaction::ReconciliationState::Complete
                        || ledger.retained.iter().any(|snapshot| {
                            snapshot.transaction == record.transaction_id
                                && snapshot.plan_hash == record.plan_hash
                                && snapshot.requirements == record.plan.requirements
                        }))
                {
                    return Ok(ledger);
                }
                return Err(
                    "committed transaction is not rooted in the authoritative installation".into(),
                );
            }
            zup_transaction::verify_installation_evidence(&ledger, record)?;
            if !record.plan.transition.is_remove() && scope == SelectedScope::Machine {
                crate::machine::normalize_published_modes(&self.root, &ledger)
                    .map_err(|error| error.to_string())?;
            }
            if scope == SelectedScope::Machine {
                crate::machine::normalize_state_modes(
                    &self.root,
                    rustix::process::geteuid().as_raw(),
                )
                .map_err(|error| error.to_string())?;
            }
            Ok(ledger)
        })
        .map_err(ExecError::Executor)
    }

    fn prepare_installation(
        &self,
        record: &TransactionRecord,
        scope: SelectedScope,
    ) -> Result<InstallationRecord, ExecError> {
        if record.phase != TransactionPhase::Applying {
            return Err(ExecError::LedgerInvalid);
        }
        if record.scope != scope {
            return Err(ExecError::LedgerInvalid);
        }
        record.validate().map_err(|_| ExecError::LedgerInvalid)?;
        if record.plan.nodes.iter().any(|node| {
            !node.kind.is_post_commit()
                && !matches!(
                    node.kind,
                    NodeKind::Barrier
                        | NodeKind::ReserveInstallation
                        | NodeKind::PublishInstallation
                )
                && record.receipt(&node.id).is_none()
        }) {
            return Err(ExecError::LedgerUncommitted);
        }
        let previous =
            self.load_authoritative(&record.app_id, scope, Some(record.transaction_id))?;
        if let Some(previous) = &previous
            && previous.commit.transaction != record.transaction_id
        {
            let store = zup_transaction::FilesystemTransactionStore::private_at(&self.root);
            for node in &record.plan.nodes {
                if let NodeKind::Retention { selection } = &node.kind {
                    zup_transaction::validate_retention(&store, previous, selection, record)
                        .map_err(ExecError::Ownership)?;
                }
            }
        }
        if record.plan.transition.is_remove() {
            let retired_owned_keys = retired_owned_keys(&record.plan)?;
            if let Some(ledger) = previous.as_ref()
                && ledger.commit.transaction != record.transaction_id
                && (retired_owned_keys.len() != ledger.current.resources.len()
                    || retired_owned_keys
                        .iter()
                        .any(|key| !ledger.current.resources.contains_key(key)))
            {
                return Err(ExecError::LedgerInvalid);
            }
        }
        let mut ledger = zup_transaction::prepare_installation_record(record, previous.as_ref())
            .map_err(|_| ExecError::LedgerInvalid)?;
        zup_transaction::retire_resources(&mut ledger, &retired_owned_keys(&record.plan)?);
        Ok(ledger)
    }

    fn write_installation(
        &self,
        record: &TransactionRecord,
        scope: SelectedScope,
    ) -> Result<InstallationRecord, ExecError> {
        let ledger = self.prepare_installation(record, scope)?;
        let path = self.path_for(&record.app_id, scope);
        let bytes = ledger.encode().map_err(|_| ExecError::LedgerInvalid)?;
        let expected = match record.publication {
            zup_transaction::PublicationState::Requested { record_hash }
            | zup_transaction::PublicationState::Published { record_hash } => record_hash,
            zup_transaction::PublicationState::NotRequested => {
                return Err(ExecError::LedgerInvalid);
            }
        };
        if zup_core::hash_bytes(&bytes) != expected {
            return Err(ExecError::LedgerInvalid);
        }
        write_ledger(&path, &bytes, scope)?;
        if self.load(&record.app_id, scope)?.as_ref() != Some(&ledger) {
            return Err(ExecError::LedgerInvalid);
        }
        Ok(ledger)
    }

    pub fn repair_committed(&self, app_id: &AppId, scope: SelectedScope) -> Result<(), ExecError> {
        let journals = zup_transaction::FilesystemTransactionStore::private_at(&self.root);
        let ledger = self.load(app_id, scope)?;
        let records = zup_transaction::settled_records(&journals, app_id, scope, ledger.as_ref())
            .map_err(|error| match error {
            zup_transaction::ReconciliationError::RecoveryRequired(id) => {
                ExecError::RecoveryRequired {
                    transaction: Some(id),
                    detail: format!(
                        "Transaction {id} must be recovered before ownership can be reconciled"
                    ),
                }
            }
            other => ExecError::Ownership(other.to_string()),
        })?;
        for record in records {
            match record.phase {
                TransactionPhase::Committed
                    if record.reconciliation == zup_transaction::ReconciliationState::Complete => {}
                TransactionPhase::Committed => {
                    self.reconcile_installation(&record, scope)?;
                }
                TransactionPhase::RolledBack => {}
                _ => unreachable!("settled_records accepts only terminal journals"),
            }
        }
        Ok(())
    }
}

impl zup_transaction::InstallationPublisher for LinuxInstallationStore {
    type Error = ExecError;

    fn read_current(
        &self,
        transaction: &TransactionRecord,
    ) -> Result<Option<InstallationRecord>, Self::Error> {
        self.load_authoritative(
            &transaction.app_id,
            transaction.scope,
            Some(transaction.transaction_id),
        )
    }

    fn prepare(&self, transaction: &TransactionRecord) -> Result<InstallationRecord, Self::Error> {
        self.prepare_installation(transaction, transaction.scope)
    }

    fn publish(
        &self,
        transaction: &TransactionRecord,
        observed: &zup_transaction::PublicationObservation,
    ) -> Result<InstallationRecord, Self::Error> {
        observed
            .check_current(transaction, self.read_current(transaction)?)
            .map_err(ExecError::Ownership)?;
        self.write_installation(transaction, transaction.scope)
    }
}

fn retired_owned_keys(plan: &DeploymentPlan) -> Result<BTreeSet<ResourceKey>, ExecError> {
    zup_transaction::retired_owned_keys(plan, |node| Err(ExecError::Ownership(node.id.to_string())))
}

fn validate_preset_plan(preset: &zup_core::InstalledPreset) -> Result<(), ExecError> {
    if preset.preset.protocol != zup_preset_protocol::PRESET_PROTOCOL_VERSION {
        return Err(ExecError::Ownership(format!(
            "preset protocol {} is not this build's {}",
            preset.preset.protocol,
            zup_preset_protocol::PRESET_PROTOCOL_VERSION
        )));
    }
    for name in &preset.preset.required_capabilities {
        zup_preset_protocol::Capability::parse(name).ok_or_else(|| {
            ExecError::Ownership(format!("preset requires unknown capability `{name}`"))
        })?;
    }
    Ok(())
}

fn read_ledger(path: &Path, scope: SelectedScope) -> std::io::Result<Vec<u8>> {
    if scope == SelectedScope::User {
        return zup_storage::read_private(path).map_err(|error| match error {
            private_fs::Error::Io(source) => source,
            other => std::io::Error::other(other),
        });
    }
    std::fs::read(path)
}

fn write_ledger(path: &Path, bytes: &[u8], scope: SelectedScope) -> Result<(), ExecError> {
    if scope == SelectedScope::User {
        return zup_storage::publish_private(path, bytes).map_err(|error| {
            PathError::io(
                path,
                match error {
                    private_fs::Error::Io(source) => source,
                    other => std::io::Error::other(other),
                },
            )
            .into()
        });
    }
    let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    else {
        return Err(PathError::io(
            path,
            std::io::Error::other("a ledger path has a parent directory"),
        )
        .into());
    };
    let directory = OwnedDirectory::create(parent, crate::fs::STATE_DIRECTORY_MODE)
        .map_err(|error| PathError::io(path, std::io::Error::other(error.to_string())))?;
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .ok_or_else(|| {
            PathError::io(path, std::io::Error::other("a ledger path has a file name"))
        })?;
    directory
        .write_durable(&name, bytes, crate::fs::STATE_FILE_MODE)
        .map_err(|error| PathError::io(path, std::io::Error::other(error.to_string())))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use zup_transaction::TransactionStore;

    #[test]
    fn uninstall_reconciliation_preserves_a_later_committed_install() {
        let root = tempfile::tempdir().unwrap();
        let journals =
            zup_transaction::FilesystemTransactionStore::private_at(root.path().join("state"));
        let app = AppId::new("com.example.reconciliation").unwrap();
        let installation = zup_core::InstallationId::new_v7();
        let ledgers = LinuxInstallationStore::new(root.path().join("state"));
        let committed = |uninstall, version| {
            let previous = ledgers.load(&app, SelectedScope::User).unwrap();
            let mut input = DeploymentEffects::new(
                installation,
                crate::requirements_fixture::requirements(
                    zup_core::Target::LINUX_X64,
                    zup_core::SelectedScope::User,
                    app.as_str(),
                    &semver::Version::new(version, 0, 0).to_string(),
                ),
            );
            input.install_directory = Some(
                zup_core::TargetPath::new(
                    zup_core::Target::LINUX_X64,
                    "/opt/reconciliation-fixture",
                )
                .unwrap(),
            );
            input.observed = previous.as_ref().map(|record| record.current.selection());
            input.observed_transaction = previous.as_ref().map(|record| record.commit.transaction);
            input.transition = if uninstall {
                zup_plan::DeploymentTransition::Remove
            } else {
                zup_plan::DeploymentTransition::Deploy
            };
            let open = zup_transaction::TransactionCoordinator::new(journals.clone())
                .begin(
                    app.clone(),
                    SelectedScope::User,
                    semver::Version::new(version, 0, 0),
                    zup_plan::compile_deployment(&input).unwrap(),
                )
                .unwrap();
            let mut executor = crate::LinuxFileExecutor::new();
            executor.register_plan(&open.record().plan).unwrap();
            executor.set_transaction_id(open.transaction_id);
            let settled = open.execute(&mut executor, &ledgers).unwrap();
            assert_eq!(
                settled.outcome,
                zup_transaction::TransactionOutcome::Committed,
                "{:?}; {:?}",
                settled.record.failure,
                settled.record.recovery_failure
            );
            settled.record
        };
        let _initial = committed(false, 0);
        let uninstall = committed(true, 1);
        ledgers
            .reconcile_installation(&uninstall, SelectedScope::User)
            .unwrap();
        let install = committed(false, 2);
        ledgers.repair_committed(&app, SelectedScope::User).unwrap();
        let ledger = ledgers.load(&app, SelectedScope::User).unwrap().unwrap();
        assert_eq!(ledger.commit.transaction, install.transaction_id);
        assert_eq!(
            journals
                .load(&install.transaction_id)
                .unwrap()
                .reconciliation,
            zup_transaction::ReconciliationState::Complete
        );
        let settled_uninstall = journals.load(&uninstall.transaction_id).unwrap();
        ledgers
            .reconcile_installation(&uninstall, SelectedScope::User)
            .unwrap();
        assert_eq!(
            journals.load(&uninstall.transaction_id).unwrap(),
            settled_uninstall
        );
        assert_eq!(
            ledgers.load(&app, SelectedScope::User).unwrap().unwrap(),
            ledger
        );
    }
}
