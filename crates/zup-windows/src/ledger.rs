use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use zup_transaction::valid_file_key;

use thiserror::Error;
use zup_core::{AppId, ResourceKey, SelectedScope, Sha256Digest};
use zup_plan::{InstallLedger, OwnedResource, TargetPath};

use zup_transaction::{
    BackendOperationIntent, FilesystemTransactionStore, NodeKind, OperationReceipt,
    TransactionPhase, TransactionPlan, TransactionRecord, maintenance_directory,
    maintenance_root as transaction_maintenance_root,
};

use crate::durable::DurableError;
use crate::lowering::{host_path, target_path_from_host};
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
    #[error("transaction plan does not match committed ownership: {0}")]
    Ownership(String),
    #[error("unfinished transaction {0} requires recovery")]
    RecoveryRequired(String),
    #[error("committed ledger reconciliation: {0}")]
    Reconciliation(String),
    #[error(transparent)]
    Durable(#[from] DurableError),
}

pub struct InstallLedgerStore {
    root: PathBuf,
}

impl InstallLedgerStore {
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
    ) -> Result<Option<InstallLedger>, LedgerError> {
        let path = self.path_for(app_id, scope);
        let bytes = match read_ledger(&path, scope) {
            Ok(bytes) => bytes,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(LedgerError::Io { path, source }),
        };
        let ledger: InstallLedger = serde_json::from_slice(&bytes)?;
        if !zup_transaction::validate_ledger_identity(&ledger, app_id, scope) {
            return Err(LedgerError::Invalid);
        }
        Ok(Some(ledger))
    }

    pub fn repair_committed(
        &self,
        app_id: &AppId,
        scope: SelectedScope,
    ) -> Result<(), LedgerError> {
        let journals = if scope == SelectedScope::Machine {
            crate::machine_authority::MachineRoot::acquire()
                .map_err(|error| LedgerError::Io {
                    path: self.root.clone(),
                    source: std::io::Error::other(error),
                })?
                .transaction_store()
        } else {
            FilesystemTransactionStore::private_at(&self.root)
        };
        let ledger = self.load(app_id, scope)?;
        let records = zup_transaction::settled_records(&journals, app_id, scope, ledger.as_ref())
            .map_err(|error| match error {
            zup_transaction::ReconciliationError::RecoveryRequired(id) => {
                LedgerError::RecoveryRequired(id.to_string())
            }
            other => LedgerError::Ownership(other.to_string()),
        })?;
        for record in records {
            match record.phase {
                TransactionPhase::Committed
                    if record.reconciliation == zup_transaction::ReconciliationState::Complete => {}
                TransactionPhase::Committed => {
                    self.publish_committed(&record, scope)?;
                }
                TransactionPhase::RolledBack => {
                    self.cleanup_work(&record)?;
                }
                _ => unreachable!("settled_records accepts only terminal journals"),
            }
        }
        Ok(())
    }

    pub(crate) fn cleanup_work(&self, record: &TransactionRecord) -> Result<(), LedgerError> {
        if !matches!(
            record.phase,
            TransactionPhase::Committed | TransactionPhase::RolledBack
        ) {
            return Err(LedgerError::Uncommitted);
        }
        if record.scope == SelectedScope::Machine {
            crate::machine_authority::MachineRoot::acquire()
                .and_then(|machine| machine.remove_work(&record.transaction_id))
                .map_err(|error| LedgerError::Io {
                    path: self
                        .root
                        .join("work")
                        .join(record.transaction_id.to_string()),
                    source: std::io::Error::other(error),
                })?;
        }
        Ok(())
    }

    pub fn validate_plan(
        &self,
        app_id: &AppId,
        scope: SelectedScope,
        app_version: &semver::Version,
        plan: &TransactionPlan,
    ) -> Result<(), LedgerError> {
        plan.validate()
            .map_err(|error| LedgerError::Ownership(error.to_string()))?;
        let ledger = self.load(app_id, scope)?;
        zup_transaction::validate_owned_service_inputs(plan, ledger.as_ref())
            .map_err(LedgerError::Ownership)?;
        crate::transaction_payload::validate_startup_inhibitions(plan, ledger.as_ref())
            .map_err(LedgerError::Ownership)?;
        let retired_ledger_keys = retired_ledger_keys(plan)?;
        let retired =
            zup_transaction::validate_ownership(plan, scope, ledger.as_ref(), &retired_ledger_keys)
                .map_err(|error| LedgerError::Ownership(error.to_string()))?;
        for node in &plan.nodes {
            match &node.kind {
                NodeKind::ServiceControl { operation } => {
                    crate::transaction_payload::validate_service_control(
                        plan,
                        operation,
                        ledger.as_ref(),
                    )
                    .map_err(LedgerError::Ownership)?
                }
                NodeKind::FileMutation { key, .. } => {
                    if !valid_file_key(key, app_id, app_version, &plan.target) {
                        return Err(LedgerError::Ownership(node.id.to_string()));
                    }
                }
                NodeKind::BackendOperation { key, intent } => {
                    let Some(operation) = &node.meta.backend else {
                        return Err(LedgerError::Ownership(node.id.to_string()));
                    };
                    if operation.key != *key
                        || operation.intent != *intent
                        || operation.payload.len() > zup_transaction::MAX_BACKEND_PAYLOAD_BYTES
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
                        || operation.payload.len() > zup_transaction::MAX_BACKEND_PAYLOAD_BYTES
                        || !retired.contains(key)
                        || !retired_ledger_keys.contains(&ledger_key)
                        || ledger
                            .as_ref()
                            .and_then(|ledger| ledger.resources.get(&ledger_key))
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
                NodeKind::FileRemoval { .. } | NodeKind::Barrier | NodeKind::StageFile { .. } => {}
            }
        }
        self.validate_ui(app_id, scope, app_version, plan, ledger.as_ref())?;
        Ok(())
    }

    fn validate_ui(
        &self,
        app_id: &AppId,
        scope: SelectedScope,
        app_version: &semver::Version,
        plan: &TransactionPlan,
        ledger: Option<&InstallLedger>,
    ) -> Result<(), LedgerError> {
        let root = crate::plain_path_text(&transaction_maintenance_root(&self.root, app_id, scope));
        let owned: BTreeMap<&str, &Sha256Digest> = ledger
            .into_iter()
            .flat_map(|ledger| ledger.resources.iter())
            .filter_map(|(key, resource)| match (key, resource) {
                (ResourceKey::File { destination }, OwnedResource::File { sha256, .. })
                    if zup_bundle::is_content_path(Path::new(&root), destination) =>
                {
                    Some((destination.as_str(), sha256))
                }
                _ => None,
            })
            .collect();
        let installed: BTreeMap<&str, Sha256Digest> = plan
            .nodes
            .iter()
            .filter_map(|node| match &node.kind {
                NodeKind::FileMutation {
                    key: ResourceKey::File { destination },
                    ..
                } => Some((
                    destination.as_str(),
                    node.meta
                        .expected_sha256
                        .expect("a file mutation states the digest it installs"),
                )),
                _ => None,
            })
            .collect();
        let retired: BTreeSet<&ResourceKey> = plan.retired_keys.iter().collect();

        let wanted: BTreeMap<String, Sha256Digest> = match &plan.preset {
            Some(ui) => {
                let directory = maintenance_directory(&self.root, app_id, scope, app_version);
                std::iter::once((
                    crate::plain_path_text(&zup_bundle::preset_path(
                        &directory,
                        &ui.executable,
                        plan.target.executable_suffix(),
                    )),
                    ui.executable,
                ))
                .chain(ui.preset.assets.iter().map(|asset| {
                    (
                        crate::plain_path_text(&zup_bundle::asset_path(
                            &directory,
                            asset.name.as_str(),
                            &asset.sha256,
                        )),
                        asset.sha256,
                    )
                }))
                .collect()
            }
            None => BTreeMap::new(),
        };
        for (path, digest) in &wanted {
            if installed.get(path.as_str()) != Some(digest)
                && !owned
                    .get(path.as_str())
                    .is_some_and(|found| **found == *digest)
            {
                return Err(LedgerError::Ownership(format!(
                    "the preset runtime needs {path}, and this plan neither installs it nor already \
                     owns it"
                )));
            }
        }
        if plan.uninstall {
            return Ok(());
        }
        for destination in owned.keys() {
            if wanted.contains_key(*destination) {
                continue;
            }
            let Some(ledger) = ledger else {
                continue;
            };
            let retired_here = ledger.resources.keys().any(|key| {
                matches!(key, ResourceKey::File { destination: d } if d == destination
                    && retired.contains(key))
            });
            if !retired_here {
                return Err(LedgerError::Ownership(format!(
                    "this plan stops presenting the window that owns {destination}, without \
                     retiring it"
                )));
            }
        }
        Ok(())
    }

    pub fn publish_committed(
        &self,
        record: &TransactionRecord,
        scope: SelectedScope,
    ) -> Result<InstallLedger, LedgerError> {
        if record.scope != scope {
            return Err(LedgerError::Invalid);
        }
        let store = if scope == SelectedScope::Machine {
            crate::machine_authority::MachineRoot::acquire()
                .map_err(|error| LedgerError::Io {
                    path: self.root.clone(),
                    source: std::io::Error::other(error),
                })?
                .transaction_store()
        } else {
            FilesystemTransactionStore::private_at(&self.root)
        };
        zup_transaction::reconcile_committed(&store, record, &self.root, |durable| {
            self.publish_ledger(durable, scope)
                .map_err(|error| error.to_string())
        })
        .map_err(LedgerError::Reconciliation)
    }

    fn publish_ledger(
        &self,
        record: &TransactionRecord,
        scope: SelectedScope,
    ) -> Result<InstallLedger, LedgerError> {
        if record.phase != TransactionPhase::Committed {
            return Err(LedgerError::Uncommitted);
        }
        if record.scope != scope {
            return Err(LedgerError::Invalid);
        }
        record.validate().map_err(|_| LedgerError::Invalid)?;
        if record.plan.nodes.iter().any(|node| {
            !matches!(node.kind, NodeKind::Barrier) && record.receipt(&node.id).is_none()
        }) {
            return Err(LedgerError::Uncommitted);
        }
        let previous = self.load(&record.app_id, scope)?;
        match zup_transaction::ledger_publication(record, previous.as_ref())
            .map_err(|error| LedgerError::Ownership(error.to_string()))?
        {
            zup_transaction::LedgerPublication::Settled => {
                return Ok(previous.unwrap_or_else(|| {
                    InstallLedger::new(
                        record.app_id.clone(),
                        record.target,
                        scope,
                        record.transaction_id,
                    )
                }));
            }
            zup_transaction::LedgerPublication::Current => {
                cleanup_committed_files(record, &self.root)?;
                self.cleanup_work(record)?;
                return Ok(previous.expect("published generation has a ledger"));
            }
            zup_transaction::LedgerPublication::Advance => {}
        }
        if record.plan.uninstall {
            let retired_ledger_keys = retired_ledger_keys(&record.plan)?;
            if let Some(ledger) = previous.as_ref()
                && (retired_ledger_keys.len() != ledger.resources.len()
                    || retired_ledger_keys
                        .iter()
                        .any(|key| !ledger.resources.contains_key(key)))
            {
                return Err(LedgerError::Invalid);
            }
            let path = self.path_for(&record.app_id, scope);
            if scope == SelectedScope::Machine {
                let name = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .ok_or(LedgerError::Invalid)?;
                crate::machine_authority::MachineRoot::acquire()
                    .map_err(|error| LedgerError::Io {
                        path: path.clone(),
                        source: std::io::Error::other(error),
                    })?
                    .remove_installation(name)
                    .map_err(|error| LedgerError::Io {
                        path: path.clone(),
                        source: std::io::Error::other(error),
                    })?;
            } else {
                match std::fs::remove_file(&path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(source) => return Err(LedgerError::Io { path, source }),
                }
            }
            cleanup_committed_files(record, &self.root)?;
            cleanup_removed_directories(record)?;
            cleanup_application_state(&self.root, record)?;
            return Ok(InstallLedger::new(
                record.app_id.clone(),
                record.target,
                scope,
                record.transaction_id,
            ));
        }
        let mut ledger = previous.unwrap_or_else(|| {
            InstallLedger::new(
                record.app_id.clone(),
                record.target,
                scope,
                record.transaction_id,
            )
        });
        if ledger.target != record.target {
            return Err(LedgerError::Ownership("target identity".into()));
        }
        zup_transaction::project_committed_files(record, &mut ledger)
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
            let receipt = receipt_from_bytes(payload).map_err(|_| LedgerError::Invalid)?;
            crate::integration::validate_service_receipt(node, &receipt)
                .map_err(|error| LedgerError::Ownership(error.to_string()))?;
            if matches!(node.kind, NodeKind::BackendRemoval { .. }) {
                continue;
            }
            if let Some(mut owned) = owned_from_backend_receipt(
                node,
                &key,
                ledger.resources.get(&key),
                payload,
                &record.target,
            )? {
                if let OwnedResource::Service {
                    installed,
                    requires_files,
                    requires_services,
                    ..
                } = &mut owned
                    && matches!(node.kind, NodeKind::BackendOperation { .. })
                {
                    let backend = node.meta.backend.as_ref().ok_or(LedgerError::Invalid)?;
                    let crate::transaction_payload::ApplyPayload::Service {
                        operation: service,
                        ..
                    } = serde_json::from_slice(&backend.payload)
                        .map_err(|_| LedgerError::Invalid)?
                    else {
                        return Err(LedgerError::Invalid);
                    };
                    **installed = service.desired_configuration();
                    *requires_files = service.requires_files;
                    *requires_services = service.requires_services;
                }
                ledger.resources.insert(key, owned);
            }
        }
        zup_transaction::retire_resources(&mut ledger, &retired_ledger_keys(&record.plan)?);
        ledger.committed_transaction = record.transaction_id;
        let path = self.path_for(&record.app_id, scope);
        let bytes = serde_json::to_vec_pretty(&ledger)?;
        if scope == SelectedScope::User {
            zup_transaction::publish_private(Path::new(&path), &bytes).map_err(|error| {
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
            crate::machine_authority::MachineRoot::acquire()
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
        cleanup_committed_files(record, &self.root)?;

        cleanup_removed_directories(record)?;
        self.cleanup_work(record)?;
        Ok(ledger)
    }
}

fn read_ledger(path: &Path, scope: SelectedScope) -> std::io::Result<Vec<u8>> {
    if scope == SelectedScope::User {
        return zup_transaction::read_private(path).map_err(|error| match error {
            private_fs::Error::Io(source) => source,
            other => std::io::Error::other(other),
        });
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "ledger name"))?;
    let root = crate::machine_state::default_state_root(SelectedScope::Machine)
        .map_err(std::io::Error::other)?;
    match crate::machine_fs::MachineDir::read_ledger(&root, name) {
        Ok(Some(bytes)) => Ok(bytes),
        Ok(None) => Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "machine ledger is absent",
        )),
        Err(crate::machine_state::MachineStateError::Io { source, .. }) => Err(source),
        Err(error) => Err(std::io::Error::other(error)),
    }
}

fn retired_ledger_keys(plan: &TransactionPlan) -> Result<BTreeSet<ResourceKey>, LedgerError> {
    zup_transaction::retired_ledger_keys(plan, |node| {
        ledger_key_for_backend_node(node).map_err(|error| LedgerError::Ownership(error.to_string()))
    })
}

fn owned_from_backend_receipt(
    node: &zup_transaction::TransactionNode,
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
        BackendReceipt::RemovePath { .. } => None,
        BackendReceipt::Service {
            name,
            privilege,
            previous,
            installed,
        } => Some(OwnedResource::Service {
            timeouts: {
                let backend = node.meta.backend.as_ref().ok_or(LedgerError::Invalid)?;
                let crate::transaction_payload::ApplyPayload::Service { operation, .. } =
                    serde_json::from_slice(&backend.payload).map_err(|_| LedgerError::Invalid)?
                else {
                    return Err(LedgerError::Invalid);
                };
                operation.lifecycle.timeouts_for(&operation.execution)
            },
            requires_files: Vec::new(),
            requires_services: Vec::new(),
            name,
            privilege,
            previous: old
                .and_then(|value| match value {
                    OwnedResource::Service { previous, .. } => Some(previous.clone()),
                    _ => None,
                })
                .unwrap_or(previous),
            installed: Box::new(installed),
        }),
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

fn cleanup_application_state(
    root: &Path,
    uninstall: &TransactionRecord,
) -> Result<(), LedgerError> {
    let transactions = root.join("transactions");
    let machine = if uninstall.scope == SelectedScope::Machine {
        Some(
            crate::machine_authority::MachineRoot::acquire().map_err(|error| LedgerError::Io {
                path: root.to_path_buf(),
                source: std::io::Error::other(error),
            })?,
        )
    } else {
        None
    };
    let _machine_work = machine
        .as_ref()
        .map(|machine| machine.work_dir())
        .transpose()
        .map_err(|error| LedgerError::Io {
            path: root.join("work"),
            source: std::io::Error::other(error),
        })?;
    let store = if let Some(machine) = &machine {
        machine.transaction_store()
    } else {
        FilesystemTransactionStore::private_at(root)
    };
    let records = zup_transaction::retired_records(&store, uninstall)
        .map_err(|error| LedgerError::Ownership(error.to_string()))?;
    for record in records {
        let work_directory = root.join("work").join(record.transaction_id.to_string());
        match std::fs::remove_dir_all(&work_directory) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(LedgerError::Io {
                    path: work_directory,
                    source,
                });
            }
        }
        if let Some(work) = &_machine_work {
            work.sync().map_err(|error| LedgerError::Io {
                path: root.join("work"),
                source: std::io::Error::other(error),
            })?;
        }
        if let Some(machine) = &machine {
            machine
                .remove_journal(&record.transaction_id)
                .map_err(|error| LedgerError::Io {
                    path: transactions.join(record.transaction_id.to_string()),
                    source: std::io::Error::other(error),
                })?;
        } else {
            let directory = transactions.join(record.transaction_id.to_string());
            if std::fs::symlink_metadata(&directory)
                .is_ok_and(|metadata| metadata.file_type().is_dir())
            {
                std::fs::remove_dir_all(&directory).map_err(|source| LedgerError::Io {
                    path: directory.clone(),
                    source,
                })?;
            } else {
                return Err(LedgerError::Invalid);
            }
        }
    }
    let work_directory = root.join("work").join(uninstall.transaction_id.to_string());
    if let Some(machine) = &machine {
        machine
            .remove_work(&uninstall.transaction_id)
            .map_err(|error| LedgerError::Io {
                path: work_directory,
                source: std::io::Error::other(error),
            })?;
    } else {
        match std::fs::remove_dir_all(&work_directory) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(LedgerError::Io {
                    path: work_directory,
                    source,
                });
            }
        }
    }
    if machine.is_none() {
        for directory in [
            root.join("transactions"),
            root.join("work"),
            root.join("installations"),
        ] {
            cleanup_empty_directory_tree(&directory)?;
        }
    }
    Ok(())
}

fn maintenance_root(path: &TargetPath, app_id: &AppId) -> Option<TargetPath> {
    let mut current = Some(path.clone());
    while let Some(candidate) = current {
        if candidate.file_name() == Some(app_id.as_str())
            && candidate
                .parent()
                .is_some_and(|parent| parent.file_name() == Some("maintenance"))
        {
            return Some(candidate);
        }
        current = candidate.parent();
    }
    None
}

fn cleanup_empty_directory_tree(root: &Path) -> Result<(), LedgerError> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(LedgerError::Io {
                path: root.to_path_buf(),
                source,
            });
        }
    };
    let mut directories = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| LedgerError::Io {
            path: root.to_path_buf(),
            source,
        })?;
        let file_type = entry.file_type().map_err(|source| LedgerError::Io {
            path: entry.path(),
            source,
        })?;
        if file_type.is_dir() {
            directories.push(entry.path());
        }
    }
    directories.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    for directory in directories {
        cleanup_empty_directory_tree(&directory)?;
    }
    match std::fs::remove_dir(root) {
        Ok(()) => {}
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty
            ) => {}
        Err(source) => {
            return Err(LedgerError::Io {
                path: root.to_path_buf(),
                source,
            });
        }
    }
    Ok(())
}

fn cleanup_removed_directories(record: &TransactionRecord) -> Result<(), LedgerError> {
    let mut directories = std::collections::BTreeSet::new();
    let mut maintenance_roots = std::collections::BTreeSet::new();
    for node in &record.plan.nodes {
        if !matches!(node.kind, NodeKind::FileRemoval { .. }) {
            continue;
        }
        let Some(removal) = &node.meta.removal else {
            continue;
        };
        if let Some(root) = maintenance_root(&removal.destination, &record.app_id) {
            maintenance_roots.insert(root);
        }
        for directory in &removal.created_directories {
            directories.insert(directory.clone());
            let Some(root) = maintenance_root(directory, &record.app_id) else {
                continue;
            };
            maintenance_roots.insert(root.clone());
            let mut current = directory.parent();
            while let Some(candidate) = current {
                directories.insert(candidate.clone());
                if candidate == root {
                    break;
                }
                if !candidate.starts_with(&root) {
                    break;
                }
                current = candidate.parent();
            }
        }
    }
    let mut directories: Vec<_> = directories.into_iter().collect();
    directories.sort_by_key(|path| std::cmp::Reverse(path.as_str().len()));
    for directory in directories {
        let path = host_path(&directory);
        match std::fs::remove_dir(&path) {
            Ok(()) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty
                ) => {}
            Err(source) => {
                return Err(LedgerError::Io { path, source });
            }
        }
    }
    for root in maintenance_roots {
        cleanup_empty_directory_tree(&host_path(&root))?;
    }
    Ok(())
}

fn cleanup_committed_files(
    record: &TransactionRecord,
    state_root: &Path,
) -> Result<(), LedgerError> {
    for node in &record.plan.nodes {
        let Some(receipt) = record.receipt(&node.id) else {
            continue;
        };
        let (backup, sha256, size) = match receipt {
            OperationReceipt::StageFile {
                staged_path,
                sha256,
                size,
            } => (
                PathBuf::from(staged_path),
                sha256.parse().map_err(|_| LedgerError::Invalid)?,
                *size,
            ),
            OperationReceipt::ReplaceFile {
                backup_path,
                previous_sha256,
                previous_size,
                ..
            } => (
                PathBuf::from(backup_path),
                previous_sha256.parse().map_err(|_| LedgerError::Invalid)?,
                *previous_size,
            ),
            OperationReceipt::RemoveFile {
                backup_path,
                sha256,
                size,
                ..
            } => (host_path(backup_path), *sha256, *size),
            _ => continue,
        };
        let owned = match (&node.kind, receipt) {
            (NodeKind::StageFile { key, .. }, OperationReceipt::StageFile { .. }) => {
                let parent = backup.parent().and_then(Path::parent);
                parent
                    == Some(
                        state_root
                            .join("work")
                            .join(record.transaction_id.to_string())
                            .join("staging")
                            .as_path(),
                    )
                    && backup.file_name().is_some_and(|name| {
                        name == format!("{}.payload", crate::file_executor::resource_token(key))
                            .as_str()
                    })
            }
            (NodeKind::FileMutation { key, .. }, OperationReceipt::ReplaceFile { .. }) => {
                backup
                    == state_root
                        .join("work")
                        .join(record.transaction_id.to_string())
                        .join("backup")
                        .join(format!("{}.bak", crate::file_executor::resource_token(key)))
            }
            (NodeKind::FileRemoval { .. }, OperationReceipt::RemoveFile { destination, .. }) => {
                let destination = host_path(destination);
                backup
                    == crate::file_executor::removal_backup_path(
                        &destination,
                        &record.transaction_id.to_string(),
                    )
                    && node
                        .meta
                        .removal
                        .as_ref()
                        .is_some_and(|removal| host_path(&removal.destination) == destination)
            }
            _ => false,
        };
        if !owned {
            return Err(LedgerError::Invalid);
        }
        crate::path_safety::ensure_no_links(&backup).map_err(|_| LedgerError::Invalid)?;
        match std::fs::symlink_metadata(&backup) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(LedgerError::Io {
                    path: backup.to_path_buf(),
                    source,
                });
            }
            Ok(metadata) if crate::path_safety::is_real_file(&backup, &metadata) => {
                let file = std::fs::File::open(&backup).map_err(|source| LedgerError::Io {
                    path: backup.to_path_buf(),
                    source,
                })?;
                if zup_core::hash_reader(file).map_err(|source| LedgerError::Io {
                    path: backup.to_path_buf(),
                    source,
                })? != (size, sha256)
                {
                    return Err(LedgerError::Invalid);
                }
                std::fs::remove_file(&backup).map_err(|source| LedgerError::Io {
                    path: backup.to_path_buf(),
                    source,
                })?;
            }
            Ok(_) => return Err(LedgerError::Invalid),
        }
    }
    Ok(())
}

impl From<zup_transaction::OwnershipError> for LedgerError {
    fn from(error: zup_transaction::OwnershipError) -> Self {
        Self::Ownership(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleanup_requires_transaction_owned_paths_even_when_bytes_match() {
        use zup_plan::{FileOperation, FileOperationKind, FilePrecondition};
        use zup_transaction::{NodeState, TransactionInput};
        let root = tempfile::tempdir().unwrap();
        let target = zup_core::Target::WINDOWS_X64;
        let destination = target_path_from_host(&root.path().join("app.bin"), &target).unwrap();
        let key = ResourceKey::File {
            destination: destination.to_string(),
        };
        let old = zup_core::hash_bytes(b"old");
        let new = zup_core::hash_bytes(b"new");
        let mut input = TransactionInput::new(target);
        input.files.push(FileOperation {
            key: key.clone(),
            source_relative: zup_core::RelativePath::new("app.bin").unwrap(),
            destination: destination.clone(),
            precondition: FilePrecondition::Exact {
                size: 3,
                sha256: old,
            },
            expected_sha256: new,
            expected_size: 3,
            privilege: zup_core::Privilege::User,
            kind: FileOperationKind::Replace,
            executable: false,

            conflict: None,
        });
        let mut record = TransactionRecord::new(
            zup_transaction::TransactionId::new_v7(),
            AppId::new("com.example.cleanup").unwrap(),
            SelectedScope::User,
            "1.0.0".parse().unwrap(),
            zup_transaction::compile_transaction(&input).unwrap(),
        );
        record.phase = TransactionPhase::Committed;
        let node = record
            .plan
            .nodes
            .iter()
            .find(|node| matches!(node.kind, NodeKind::FileMutation { .. }))
            .unwrap();
        let id = node.id.clone();
        let foreign = root.path().join("foreign.bin");
        std::fs::write(&foreign, b"old").unwrap();
        let receipt = |backup: &Path| OperationReceipt::ReplaceFile {
            destination: destination.to_string(),
            previous_sha256: old.to_hex(),
            previous_size: 3,
            previous_mode: 0,
            backup_path: backup.display().to_string(),
            new_sha256: new.to_hex(),
            new_size: 3,
            executable: false,
        };
        record.nodes.insert(
            id.clone(),
            NodeState::Verified {
                receipt: Box::new(receipt(&foreign)),
            },
        );
        assert!(cleanup_committed_files(&record, root.path()).is_err());
        assert_eq!(std::fs::read(&foreign).unwrap(), b"old");
        let owned = root
            .path()
            .join("work")
            .join(record.transaction_id.to_string())
            .join("backup")
            .join(format!(
                "{}.bak",
                crate::file_executor::resource_token(&key)
            ));
        std::fs::create_dir_all(owned.parent().unwrap()).unwrap();
        std::fs::write(&owned, b"old").unwrap();
        record.nodes.insert(
            id,
            NodeState::Verified {
                receipt: Box::new(receipt(&owned)),
            },
        );
        cleanup_committed_files(&record, root.path()).unwrap();
        cleanup_committed_files(&record, root.path()).unwrap();
        assert!(!owned.exists());
        assert!(foreign.exists());
    }
}
