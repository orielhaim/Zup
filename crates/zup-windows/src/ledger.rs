use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
#[cfg(test)]
use zup_core::TargetPath;
#[cfg(test)]
use zup_plan::DeploymentEffects;
use zup_plan::{BackendOperationIntent, DeploymentPlan, NodeKind};
use zup_transaction::TransactionStore;
use zup_transaction::valid_file_key;

use thiserror::Error;
use zup_core::{AppId, ResourceKey, SelectedScope};
use zup_plan::{InstallationRecord, OwnedResource};

use zup_transaction::{
    FilesystemTransactionStore, OperationReceipt, TransactionPhase, TransactionRecord,
};

use crate::durable::DurableError;
use crate::lowering::target_path_from_host;
use crate::transaction_payload::{
    BackendReceipt, apps_owned_payload, ledger_key_for_backend_node, receipt_from_bytes,
};

#[derive(Debug, Error)]
pub enum LedgerError {
    #[error("ledger I/O at {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("ledger JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("ledger identity or schema mismatch")]
    Invalid,
    #[error("ledger publish requires a committed transaction")]
    Uncommitted,
    #[error("resource ownership validation failed: {0}")]
    Ownership(String),
    #[error("unfinished transaction {0} requires recovery")]
    RecoveryRequired(zup_core::TransactionId),
    #[error("committed ledger reconciliation: {0}")]
    Reconciliation(String),
    #[error(transparent)]
    Durable(#[from] DurableError),
}

pub struct InstallationRecordStore {
    root: PathBuf,
}

impl InstallationRecordStore {
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
    ) -> Result<Option<InstallationRecord>, LedgerError> {
        Ok(self
            .load_evidence(app_id, scope)?
            .map(|(installation, _)| installation))
    }

    /// Read-only planning metadata; execution requires protected journal evidence.
    pub fn observe(
        &self,
        app_id: &AppId,
        scope: SelectedScope,
    ) -> Result<Option<InstallationRecord>, LedgerError> {
        let path = self.path_for(app_id, scope);
        let bytes = match read_ledger(&path, app_id, scope) {
            Ok(bytes) => bytes,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(LedgerError::Io { path, source }),
        };
        let ledger = InstallationRecord::decode(&bytes).map_err(|_| LedgerError::Invalid)?;
        if !zup_transaction::validate_installation_record(&ledger, app_id, scope) {
            return Err(LedgerError::Invalid);
        }
        Ok(Some(ledger))
    }

    fn load_evidence(
        &self,
        app_id: &AppId,
        scope: SelectedScope,
    ) -> Result<Option<(InstallationRecord, TransactionRecord)>, LedgerError> {
        let Some(ledger) = self.observe(app_id, scope)? else {
            return Ok(None);
        };
        use zup_transaction::TransactionStore;
        let store = if scope == SelectedScope::Machine {
            let machine = crate::machine_authority::MachineRoot::from_existing(&self.root, app_id)
                .map_err(|error| LedgerError::Ownership(error.to_string()))?;
            if machine.path() != self.root {
                return Err(LedgerError::Invalid);
            }
            machine.transaction_store()
        } else {
            FilesystemTransactionStore::private_at(&self.root)
        };
        let journal = store
            .load(&ledger.commit.transaction)
            .map_err(|error| LedgerError::Ownership(error.to_string()))?;
        zup_transaction::verify_installation_evidence(&ledger, &journal)
            .map_err(LedgerError::Ownership)?;
        Ok(Some((ledger, journal)))
    }

    fn load_authoritative(
        &self,
        app_id: &AppId,
        scope: SelectedScope,
        publishing: Option<zup_core::TransactionId>,
    ) -> Result<Option<InstallationRecord>, LedgerError> {
        let Some((installation, journal)) = self.load_evidence(app_id, scope)? else {
            return Ok(None);
        };
        if journal.phase != TransactionPhase::Committed
            && publishing != Some(journal.transaction_id)
        {
            return Err(LedgerError::RecoveryRequired(journal.transaction_id));
        }
        Ok(Some(installation))
    }

    pub fn repair_committed(
        &self,
        app_id: &AppId,
        scope: SelectedScope,
    ) -> Result<(), LedgerError> {
        let journals = if scope == SelectedScope::Machine {
            match crate::machine_authority::MachineRoot::from_existing(&self.root, app_id) {
                Ok(machine) => machine.transaction_store(),
                Err(crate::filesystem::FilesystemError::Io { source, .. })
                    if source.kind() == std::io::ErrorKind::NotFound =>
                {
                    return Ok(());
                }
                Err(error) => {
                    return Err(LedgerError::Io {
                        path: self.root.clone(),
                        source: std::io::Error::other(error),
                    });
                }
            }
        } else {
            FilesystemTransactionStore::private_at(&self.root)
        };
        let ledger = self.load(app_id, scope)?;
        let records = zup_transaction::settled_records(&journals, app_id, scope, ledger.as_ref())
            .map_err(|error| match error {
            zup_transaction::ReconciliationError::RecoveryRequired(id) => {
                LedgerError::RecoveryRequired(id)
            }
            other => LedgerError::Ownership(other.to_string()),
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
            let durable = journals
                .load(&record.transaction_id)
                .map_err(|error| LedgerError::Ownership(error.to_string()))?;
            crate::recovery::collect(&self.root, &durable).map_err(LedgerError::Ownership)?;
        }
        Ok(())
    }

    pub fn validate_plan(
        &self,
        app_id: &AppId,
        scope: SelectedScope,
        plan: &DeploymentPlan,
    ) -> Result<(), LedgerError> {
        plan.validate()
            .map_err(|error| LedgerError::Ownership(error.to_string()))?;
        let ledger = self.load_authoritative(app_id, scope, None)?;
        let claims =
            crate::persistent::discover(&self.root, scope, app_id, plan.target, ledger.as_ref())
                .map_err(LedgerError::Ownership)?;
        zup_plan::validate_owned_service_inputs(plan, ledger.as_ref())
            .map_err(LedgerError::Ownership)?;
        crate::transaction_payload::validate_startup_inhibitions(plan, ledger.as_ref())
            .map_err(LedgerError::Ownership)?;
        let retired_owned_keys = retired_owned_keys(plan)?;
        let retired = zup_transaction::validate_ownership(
            plan,
            scope,
            ledger.as_ref(),
            &retired_owned_keys,
            claims.as_ref(),
        )
        .map_err(|error| LedgerError::Ownership(error.to_string()))?;
        for node in &plan.nodes {
            match &node.kind {
                NodeKind::Retention { .. } | NodeKind::PersistentDirectory { .. } => {}
                NodeKind::NativeService { operation } => {
                    crate::transaction_payload::validate_native_service(
                        operation,
                        ledger.as_ref(),
                        scope,
                    )
                    .map_err(LedgerError::Ownership)?;
                }
                NodeKind::NativeCache { .. } => {
                    return Err(LedgerError::Ownership(node.id.to_string()));
                }
                NodeKind::ServiceControl { operation } => {
                    crate::transaction_payload::validate_service_control(
                        plan,
                        operation,
                        ledger.as_ref(),
                    )
                    .map_err(LedgerError::Ownership)?
                }
                NodeKind::FileMutation { key, .. } | NodeKind::RetainFile { key } => {
                    if !valid_file_key(key, &plan.target) {
                        return Err(LedgerError::Ownership(node.id.to_string()));
                    }
                }
                NodeKind::BackendOperation { key, intent } => {
                    let Some(operation) = &node.meta.backend else {
                        return Err(LedgerError::Ownership(node.id.to_string()));
                    };
                    if operation.key != *key
                        || operation.intent != *intent
                        || operation.payload.len() > zup_plan::MAX_BACKEND_PAYLOAD_BYTES
                    {
                        return Err(LedgerError::Ownership(node.id.to_string()));
                    }
                    if let Err(reason) = crate::transaction_payload::validate_backend_node(
                        node,
                        ledger.as_ref(),
                        scope,
                        app_id,
                    ) {
                        return Err(LedgerError::Ownership(reason));
                    }
                }
                NodeKind::BackendRemoval { key } => {
                    let Some(operation) = &node.meta.backend else {
                        return Err(LedgerError::Ownership(node.id.to_string()));
                    };
                    let ledger_key = ledger_key_for_backend_node(node)
                        .map_err(|error| LedgerError::Ownership(error.to_string()))?;
                    if operation.key != *key
                        || operation.intent != BackendOperationIntent::Remove
                        || operation.payload.len() > zup_plan::MAX_BACKEND_PAYLOAD_BYTES
                        || !retired.contains(key)
                        || !retired_owned_keys.contains(&ledger_key)
                        || ledger
                            .as_ref()
                            .and_then(|ledger| ledger.current.resources.get(&ledger_key))
                            .is_none()
                    {
                        return Err(LedgerError::Ownership(node.id.to_string()));
                    }
                    if let Err(reason) = crate::transaction_payload::validate_backend_node(
                        node,
                        ledger.as_ref(),
                        scope,
                        app_id,
                    ) {
                        return Err(LedgerError::Ownership(reason));
                    }
                }
                NodeKind::Cleanup { .. }
                | NodeKind::Generation { .. }
                | NodeKind::ReserveInstallation
                | NodeKind::PublishInstallation
                | NodeKind::FileRemoval { .. }
                | NodeKind::Barrier
                | NodeKind::StageFile { .. } => {}
            }
        }
        Ok(())
    }

    pub fn persistent_claims(
        &self,
        app_id: &AppId,
        scope: SelectedScope,
        target: zup_core::Target,
    ) -> Result<Option<zup_plan::PersistentClaimSelection>, LedgerError> {
        let installation = self.load_authoritative(app_id, scope, None)?;
        crate::persistent::discover(&self.root, scope, app_id, target, installation.as_ref())
            .map_err(LedgerError::Ownership)
    }

    pub fn reconcile_installation(
        &self,
        record: &TransactionRecord,
        scope: SelectedScope,
    ) -> Result<InstallationRecord, LedgerError> {
        if record.scope != scope {
            return Err(LedgerError::Invalid);
        }
        let store = if scope == SelectedScope::Machine {
            crate::machine_authority::MachineRoot::from_existing(&self.root, &record.app_id)
                .map_err(|error| LedgerError::Io {
                    path: self.root.clone(),
                    source: std::io::Error::other(error),
                })?
                .transaction_store()
        } else {
            FilesystemTransactionStore::private_at(&self.root)
        };
        zup_transaction::reconcile_committed(&store, record, &self.root, |durable| {
            let installation = self
                .load_authoritative(&durable.app_id, scope, None)
                .map_err(|error| error.to_string())?
                .ok_or_else(|| "committed installation record is missing".to_string())?;
            if installation.commit.transaction != durable.transaction_id {
                if installation.id == durable.plan.installation
                    && installation.target == durable.target
                    && (durable.reconciliation == zup_transaction::ReconciliationState::Complete
                        || installation.retained.iter().any(|snapshot| {
                            snapshot.transaction == durable.transaction_id
                                && snapshot.plan_hash == durable.plan_hash
                                && snapshot.requirements == durable.plan.requirements
                        }))
                {
                    return Ok(installation);
                }
                return Err(
                    "committed transaction is not rooted in the authoritative installation".into(),
                );
            }
            zup_transaction::verify_installation_evidence(&installation, durable)?;
            Ok(installation)
        })
        .map_err(LedgerError::Reconciliation)
    }

    fn prepare_installation(
        &self,
        record: &TransactionRecord,
        scope: SelectedScope,
    ) -> Result<InstallationRecord, LedgerError> {
        if record.phase != TransactionPhase::Applying {
            return Err(LedgerError::Invalid);
        }
        if record.scope != scope {
            return Err(LedgerError::Invalid);
        }
        record.validate().map_err(|_| LedgerError::Invalid)?;
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
            return Err(LedgerError::Uncommitted);
        }
        let previous =
            self.load_authoritative(&record.app_id, scope, Some(record.transaction_id))?;
        if let Some(previous) = &previous
            && previous.commit.transaction != record.transaction_id
        {
            let store = if scope == SelectedScope::Machine {
                let machine = crate::machine_authority::MachineRoot::from_existing(
                    &self.root,
                    &record.app_id,
                )
                .map_err(|error| LedgerError::Ownership(error.to_string()))?;
                if machine.path() != self.root {
                    return Err(LedgerError::Invalid);
                }
                machine.transaction_store()
            } else {
                FilesystemTransactionStore::private_at(&self.root)
            };
            for node in &record.plan.nodes {
                if let NodeKind::Retention { selection } = &node.kind {
                    zup_transaction::validate_retention(&store, previous, selection, record)
                        .map_err(LedgerError::Ownership)?;
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
                return Err(LedgerError::Invalid);
            }
        }
        let mut ledger = zup_transaction::prepare_installation_record(record, previous.as_ref())
            .map_err(|_| LedgerError::Invalid)?;
        for node in &record.plan.nodes {
            if !matches!(
                &node.kind,
                NodeKind::BackendOperation { .. } | NodeKind::BackendRemoval { .. }
            ) {
                continue;
            }
            let key = ledger_key_for_backend_node(node)
                .map_err(|error| LedgerError::Ownership(error.to_string()))?;
            let Some(OperationReceipt::Backend { payload, .. }) = record.receipt(&node.id) else {
                return Err(LedgerError::Invalid);
            };
            if matches!(node.kind, NodeKind::BackendRemoval { .. }) {
                continue;
            }
            if let Some(owned) = owned_from_backend_receipt(
                node,
                &key,
                ledger.current.resources.get(&key),
                payload,
                &record.target,
            )? {
                ledger.current.resources.insert(key, owned);
            }
        }
        zup_transaction::retire_resources(&mut ledger, &retired_owned_keys(&record.plan)?);
        Ok(ledger)
    }

    fn write_installation(
        &self,
        record: &TransactionRecord,
        scope: SelectedScope,
    ) -> Result<InstallationRecord, LedgerError> {
        let ledger = self.prepare_installation(record, scope)?;
        let path = self.path_for(&record.app_id, scope);
        let bytes = ledger.encode().map_err(|_| LedgerError::Invalid)?;
        let expected = match record.publication {
            zup_transaction::PublicationState::Requested { record_hash }
            | zup_transaction::PublicationState::Published { record_hash } => record_hash,
            zup_transaction::PublicationState::NotRequested => return Err(LedgerError::Invalid),
        };
        if zup_core::hash_bytes(&bytes) != expected {
            return Err(LedgerError::Invalid);
        }
        if scope == SelectedScope::User {
            zup_storage::publish_private(Path::new(&path), &bytes).map_err(|error| {
                LedgerError::Io {
                    path: PathBuf::from(&path),
                    source: match error {
                        private_fs::Error::Io(source) => source,
                        other => std::io::Error::other(other),
                    },
                }
            })?;
        } else {
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or(LedgerError::Invalid)?;
            crate::machine_authority::MachineRoot::from_existing(&self.root, &record.app_id)
                .map_err(|error| LedgerError::Io {
                    path: path.clone(),
                    source: std::io::Error::other(error),
                })?
                .publish_installation(name, &bytes)
                .map_err(|error| LedgerError::Io {
                    path,
                    source: std::io::Error::other(error),
                })?;
        }
        if self.load(&record.app_id, scope)?.as_ref() != Some(&ledger) {
            return Err(LedgerError::Invalid);
        }
        Ok(ledger)
    }
}

impl zup_transaction::InstallationPublisher for InstallationRecordStore {
    type Error = LedgerError;

    fn prepare(&self, transaction: &TransactionRecord) -> Result<InstallationRecord, Self::Error> {
        self.prepare_installation(transaction, transaction.scope)
    }

    fn publish(&self, transaction: &TransactionRecord) -> Result<InstallationRecord, Self::Error> {
        self.write_installation(transaction, transaction.scope)
    }
}

fn read_ledger(path: &Path, app_id: &AppId, scope: SelectedScope) -> std::io::Result<Vec<u8>> {
    if scope == SelectedScope::User {
        return zup_storage::read_private(path).map_err(|error| match error {
            private_fs::Error::Io(source) => source,
            other => std::io::Error::other(other),
        });
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "ledger name"))?;
    let root = crate::machine_state::default_state_root(app_id, SelectedScope::Machine)
        .map_err(std::io::Error::other)?;
    match crate::filesystem::MachineDir::read_ledger(&root, name) {
        Ok(Some(bytes)) => Ok(bytes),
        Ok(None) => Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "machine ledger is absent",
        )),
        Err(crate::filesystem::FilesystemError::Io { source, .. }) => Err(source),
        Err(error) => Err(std::io::Error::other(error)),
    }
}

fn retired_owned_keys(plan: &DeploymentPlan) -> Result<BTreeSet<ResourceKey>, LedgerError> {
    zup_transaction::retired_owned_keys(plan, |node| {
        ledger_key_for_backend_node(node).map_err(|error| LedgerError::Ownership(error.to_string()))
    })
}

fn owned_from_backend_receipt(
    _node: &zup_plan::DeploymentNode,
    key: &ResourceKey,
    old: Option<&OwnedResource>,
    payload: &[u8],
    target: &zup_core::Target,
) -> Result<Option<OwnedResource>, LedgerError> {
    let receipt = receipt_from_bytes(payload).map_err(|_| LedgerError::Invalid)?;
    Ok(match receipt {
        BackendReceipt::Launcher {
            launcher_path,
            privilege,
            previous,
            installed,
        } => Some(OwnedResource::Launcher {
            launcher_path,
            privilege,
            previous: old
                .and_then(|value| match value {
                    OwnedResource::Launcher { previous, .. } => Some(previous.clone()),
                    _ => None,
                })
                .unwrap_or(previous),
            installed,
        }),
        BackendReceipt::Path {
            scope: _,
            entry,
            value_type,
            privilege,
        } => Some(OwnedResource::PathEntry {
            value: target_path_from_host(Path::new(&entry), target)
                .map_err(|_| LedgerError::Invalid)?,
            value_type,
            privilege,
        }),
        BackendReceipt::RebindPath {
            installed,
            privilege,
            ..
        } => Some(OwnedResource::PathEntry {
            value: installed.value,
            value_type: installed.value_type,
            privilege,
        }),
        BackendReceipt::RemovePath { .. } => None,
        BackendReceipt::Protocol {
            privilege,
            previous,
            installed,
            ..
        } => Some(OwnedResource::Protocol {
            privilege,
            previous: old
                .and_then(|value| match value {
                    OwnedResource::Protocol { previous, .. } => Some(previous.clone()),
                    _ => None,
                })
                .unwrap_or(previous),
            installed,
        }),
        BackendReceipt::FileAssociation {
            privilege,
            previous,
            installed,
            ..
        } => Some(OwnedResource::FileAssociation {
            privilege,
            previous: old
                .and_then(|value| match value {
                    OwnedResource::FileAssociation { previous, .. } => Some(previous.clone()),
                    _ => None,
                })
                .unwrap_or(previous),
            installed,
        }),
        BackendReceipt::Extension {
            privilege,
            previous,
            installed,
            ..
        } => Some(OwnedResource::Extension {
            privilege,
            previous: old
                .and_then(|value| match value {
                    OwnedResource::Extension { previous, .. } => Some(previous.clone()),
                    _ => None,
                })
                .unwrap_or(previous),
            installed,
        }),
        BackendReceipt::AppsFeatures {
            privilege,
            installed: Some(state),
            ..
        } => Some(OwnedResource::Backend {
            id: match key {
                ResourceKey::Backend { id } => id.clone(),
                _ => return Err(LedgerError::Invalid),
            },
            privilege,
            payload: apps_owned_payload(&state),
        }),
        BackendReceipt::AppsFeatures {
            installed: None, ..
        } => None,
    })
}

impl From<zup_transaction::OwnershipError> for LedgerError {
    fn from(error: zup_transaction::OwnershipError) -> Self {
        Self::Ownership(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observation_probe_app() -> AppId {
        assert_eq!(std::env::var("ZUP_TEST_DISPOSABLE_VM").as_deref(), Ok("1"));
        let app = std::env::var("ZUP_TEST_MACHINE_OBSERVATION_APPID").unwrap();
        assert!(app.starts_with("com.zup.observation-probe-"));
        AppId::new(app).unwrap()
    }

    #[test]
    #[ignore = "creates protected metadata in a disposable Windows VM"]
    fn prepare_machine_observation_probe() {
        struct EmptyDeployment;
        impl zup_transaction::OperationExecutor for EmptyDeployment {
            type Error = &'static str;
            fn prepare(&mut self, _: &zup_plan::DeploymentNode) -> Result<(), Self::Error> {
                Ok(())
            }
            fn apply(
                &mut self,
                node: &zup_plan::DeploymentNode,
                _: &TransactionRecord,
            ) -> Result<OperationReceipt, Self::Error> {
                if node.kind.is_barrier() {
                    Ok(OperationReceipt::Control)
                } else {
                    Err("unexpected fixture effect")
                }
            }
            fn verify(
                &mut self,
                _: &zup_plan::DeploymentNode,
                _: &OperationReceipt,
            ) -> Result<(), Self::Error> {
                Ok(())
            }
            fn rollback(
                &mut self,
                _: &zup_plan::DeploymentNode,
                _: &OperationReceipt,
            ) -> Result<(), Self::Error> {
                Err("unexpected fixture compensation")
            }
            fn reconcile(
                &mut self,
                _: &zup_plan::DeploymentNode,
                _: Option<&OperationReceipt>,
                _: &TransactionRecord,
            ) -> Result<zup_transaction::ReconcileResult, Self::Error> {
                Err("unexpected fixture reconciliation")
            }
        }
        let app = observation_probe_app();
        let machine = crate::machine_authority::MachineRoot::acquire(&app).unwrap();
        let target = zup_core::Target::WINDOWS_X64;
        let mut effects = DeploymentEffects::new(
            zup_core::InstallationId::new_v7(),
            crate::requirements_fixture::requirements(
                target,
                zup_core::SelectedScope::Machine,
                app.as_str(),
                "1.0.0",
            ),
        );
        effects.install_directory =
            Some(TargetPath::new(target, format!(r"C:\Program Files\{}", app.as_str())).unwrap());
        let open = zup_transaction::TransactionCoordinator::new(machine.transaction_store())
            .begin(
                app.clone(),
                SelectedScope::Machine,
                "1.0.0".parse().unwrap(),
                zup_plan::compile_deployment(&effects).unwrap(),
            )
            .unwrap();
        let store = InstallationRecordStore::new(machine.path());
        assert_eq!(
            open.execute(&mut EmptyDeployment, &store).unwrap().outcome,
            zup_transaction::TransactionOutcome::Committed
        );
        assert!(store.load(&app, SelectedScope::Machine).unwrap().is_some());
    }

    #[test]
    #[ignore = "requires a prepared machine record and a standard-user token in a disposable VM"]
    fn standard_user_observes_machine_metadata_without_private_journal_access() {
        let app = observation_probe_app();
        let root = crate::machine_state::default_state_root(&app, SelectedScope::Machine).unwrap();
        let store = InstallationRecordStore::new(&root);
        let observed = store
            .observe(&app, SelectedScope::Machine)
            .unwrap()
            .unwrap();
        assert_eq!(
            observed.current.version,
            "1.0.0".parse::<semver::Version>().unwrap()
        );
        assert!(
            std::fs::read(
                root.join("transactions")
                    .join(observed.commit.transaction.to_string())
                    .join("transaction.json")
            )
            .is_err()
        );
        assert!(store.load(&app, SelectedScope::Machine).is_err());
        assert!(
            std::fs::OpenOptions::new()
                .write(true)
                .open(store.path_for(&app, SelectedScope::Machine))
                .is_err()
        );
        assert_eq!(
            store.observe(&app, SelectedScope::Machine).unwrap(),
            Some(observed)
        );
    }
}
