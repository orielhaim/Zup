//! Committed installation ownership, separate from transaction journals.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use thiserror::Error;
use zup_core::{AppId, ResourceKey, SelectedScope};
use zup_exec::{
    FileTypeOperationKind, INSTALL_LEDGER_SCHEMA, InstallLedger, ManagedOperation, OwnedResource,
    PathOperationKind, ProtocolOperationKind, ServiceOperationKind, ServiceState,
    ShortcutOperationKind, ShortcutState,
};
use zup_transaction::{
    FilesystemTransactionStore, NodeKind, NodeState, OperationReceipt, TransactionId,
    TransactionPhase, TransactionPlan, TransactionRecord, TransactionStore,
};

use crate::durable::{DurableError, write_durable};

#[derive(Debug, Error)]
pub enum LedgerError {
    #[error("ledger I/O at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
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

    fn path(&self, app_id: &AppId, scope: SelectedScope) -> PathBuf {
        let mut hash = Sha256::new();
        hash.update(app_id.as_str().as_bytes());
        let digest = hash.finalize();
        let name: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
        let scope = match scope {
            SelectedScope::User => "user",
            SelectedScope::Machine => "machine",
        };
        self.root
            .join("installations")
            .join(format!("{name}-{scope}.json"))
    }

    pub fn load(
        &self,
        app_id: &AppId,
        scope: SelectedScope,
    ) -> Result<Option<InstallLedger>, LedgerError> {
        let path = self.path(app_id, scope);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(LedgerError::Io { path, source }),
        };
        let ledger: InstallLedger = serde_json::from_slice(&bytes)?;
        if ledger.schema != INSTALL_LEDGER_SCHEMA
            || ledger.app_id != *app_id
            || ledger.scope != scope
        {
            return Err(LedgerError::Invalid);
        }
        Ok(Some(ledger))
    }

    /// Complete ledger publication for committed journals before planning a
    /// new transaction. The caller holds the installation lock.
    pub fn repair_committed(
        &self,
        app_id: &AppId,
        scope: SelectedScope,
    ) -> Result<(), LedgerError> {
        let directory = self.root.join("transactions");
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(source) => {
                return Err(LedgerError::Io {
                    path: directory,
                    source,
                });
            }
        };
        let journals = FilesystemTransactionStore::new(&self.root);
        let mut committed = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| LedgerError::Io {
                path: directory.clone(),
                source,
            })?;
            let Some(id) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<uuid::Uuid>().ok())
            else {
                continue;
            };
            let record = journals
                .load(&TransactionId::from_uuid(id))
                .map_err(|_| LedgerError::Invalid)?;
            if record.app_id != *app_id || record.scope != scope {
                continue;
            }
            match record.phase {
                TransactionPhase::Committed => committed.push(record),
                TransactionPhase::RolledBack => {}
                _ => {
                    return Err(LedgerError::RecoveryRequired(
                        record.transaction_id.to_string(),
                    ));
                }
            }
        }
        committed.sort_by_key(|record| record.transaction_id.as_uuid());
        let mut latest = self
            .load(app_id, scope)?
            .and_then(|ledger| ledger.committed_transaction.parse::<uuid::Uuid>().ok());
        for record in committed {
            let id = record.transaction_id.as_uuid();
            if latest.is_none_or(|previous| id > previous) {
                self.publish_committed(&record, scope)?;
                latest = Some(id);
            } else {
                cleanup_committed_files(&record)?;
            }
        }
        Ok(())
    }

    /// Called under the installation lock before durable transaction intent.
    pub fn validate_plan(
        &self,
        app_id: &AppId,
        scope: SelectedScope,
        app_version: &semver::Version,
        plan: &TransactionPlan,
    ) -> Result<(), LedgerError> {
        let ledger = self.load(app_id, scope)?;
        let retired: std::collections::BTreeSet<_> = plan.retired_keys.iter().collect();
        if retired.len() != plan.retired_keys.len()
            || plan.retired_keys.iter().any(|key| {
                ledger
                    .as_ref()
                    .is_none_or(|ledger| !ledger.resources.contains_key(key))
            })
            || (plan.uninstall
                && ledger
                    .as_ref()
                    .is_none_or(|ledger| retired.len() != ledger.resources.len()))
        {
            return Err(LedgerError::Ownership("retired resources".into()));
        }
        for node in &plan.nodes {
            if let NodeKind::FileMutation { key, delta } = &node.kind {
                let owned = ledger.as_ref().and_then(|ledger| ledger.resources.get(key));
                let valid = match (delta, owned) {
                    (zup_exec::Delta::Create, None) => matches!(
                        node.meta.file_precondition,
                        Some(zup_exec::FilePrecondition::Absent)
                    ),
                    (zup_exec::Delta::Replace, Some(OwnedResource::File { sha256, size, .. })) => {
                        matches!(node.meta.file_precondition, Some(zup_exec::FilePrecondition::Exact { size: found_size, sha256: found_hash }) if found_size == *size && found_hash == *sha256)
                    }
                    (
                        zup_exec::Delta::RestoreOwned,
                        Some(OwnedResource::File { sha256, size, .. }),
                    ) => {
                        matches!(
                            node.meta.file_precondition,
                            Some(zup_exec::FilePrecondition::Absent)
                        ) && node.meta.expected_sha256 == Some(*sha256)
                            && node.meta.expected_size == Some(*size)
                    }
                    (
                        zup_exec::Delta::RepairOwned,
                        Some(OwnedResource::File { sha256, size, .. }),
                    ) => {
                        matches!(
                            node.meta.file_precondition,
                            Some(zup_exec::FilePrecondition::Exact { .. })
                        ) && node.meta.expected_sha256 == Some(*sha256)
                            && node.meta.expected_size == Some(*size)
                    }
                    _ => false,
                };
                if !valid
                    || !valid_file_key(key, app_id, app_version)
                    || node.meta.source_relative.is_none()
                {
                    return Err(LedgerError::Ownership(node.id.to_string()));
                }
                continue;
            }
            if let NodeKind::OwnedRemoval { key, resource } = &node.kind {
                let owned = ledger.as_ref().and_then(|ledger| ledger.resources.get(key));
                let valid_resource = matches!(
                    (resource, owned),
                    (
                        zup_transaction::ManagedResource::File,
                        Some(OwnedResource::File { .. })
                    ) | (
                        zup_transaction::ManagedResource::Shortcut,
                        Some(OwnedResource::Shortcut { .. })
                    ) | (
                        zup_transaction::ManagedResource::PathEntry,
                        Some(OwnedResource::PathEntry { .. })
                    ) | (
                        zup_transaction::ManagedResource::Service,
                        Some(OwnedResource::Service { .. })
                    ) | (
                        zup_transaction::ManagedResource::Protocol,
                        Some(OwnedResource::Protocol { .. })
                    ) | (
                        zup_transaction::ManagedResource::FileType,
                        Some(OwnedResource::ProgId { .. } | OwnedResource::Extension { .. })
                    ) | (
                        zup_transaction::ManagedResource::UninstallEntry,
                        Some(OwnedResource::UninstallEntry { .. })
                    )
                );
                if !valid_resource
                    || node.meta.removal.as_ref() != owned
                    || node.meta.removal_scope != Some(scope)
                    || !retired.contains(key)
                {
                    return Err(LedgerError::Ownership(node.id.to_string()));
                }
                continue;
            }
            let Some(managed) = &node.meta.managed else {
                if matches!(node.kind, NodeKind::ManagedIntegration { .. }) {
                    return Err(LedgerError::Ownership(node.id.to_string()));
                }
                continue;
            };
            let scope_and_key_match = match (managed, &node.kind) {
                (
                    ManagedOperation::Shortcut(op),
                    NodeKind::ManagedIntegration {
                        key,
                        resource: zup_transaction::ManagedResource::Shortcut,
                        ..
                    },
                ) => key == &op.key,
                (
                    ManagedOperation::Service(op),
                    NodeKind::ManagedIntegration {
                        key,
                        resource: zup_transaction::ManagedResource::Service,
                        ..
                    },
                ) => scope == SelectedScope::Machine && key == &op.key,
                (
                    ManagedOperation::Path(op),
                    NodeKind::ManagedIntegration {
                        key,
                        resource: zup_transaction::ManagedResource::PathEntry,
                        ..
                    },
                ) => op.scope == scope && key == &op.key,
                (
                    ManagedOperation::Protocol(op),
                    NodeKind::ManagedIntegration {
                        key,
                        resource: zup_transaction::ManagedResource::Protocol,
                        ..
                    },
                ) => op.scope == scope && key == &op.key,
                (
                    ManagedOperation::ProgId(op),
                    NodeKind::ManagedIntegration {
                        key,
                        resource: zup_transaction::ManagedResource::FileType,
                        ..
                    },
                ) => op.scope == scope && key == &op.key,
                (
                    ManagedOperation::Extension(op),
                    NodeKind::ManagedIntegration {
                        key: ResourceKey::FileTypeExtension { extension },
                        resource: zup_transaction::ManagedResource::FileType,
                        ..
                    },
                ) => op.scope == scope && extension.as_str() == op.extension,
                (
                    ManagedOperation::UninstallEntry(op),
                    NodeKind::ManagedIntegration {
                        key,
                        resource: zup_transaction::ManagedResource::UninstallEntry,
                        ..
                    },
                ) => {
                    let expected_key = ResourceKey::UninstallEntry {
                        app_id: app_id.to_string(),
                    };
                    let expected_path = format!(
                        "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\{}",
                        app_id
                    );
                    op.scope == scope
                        && key == &expected_key
                        && op.key == expected_key
                        && op.key_path == expected_path
                }
                _ => false,
            };
            if !scope_and_key_match {
                return Err(LedgerError::Ownership(node.id.to_string()));
            }
            let owned = ledger.as_ref().and_then(|ledger| match &node.kind {
                NodeKind::ManagedIntegration { key, .. } => ledger.resources.get(key),
                _ => None,
            });
            let valid = match managed {
                ManagedOperation::Shortcut(op) => match op.kind {
                    ShortcutOperationKind::Create => {
                        owned.is_none()
                            && matches!(op.previous, zup_exec::ObservedShortcutState::Absent)
                    }
                    ShortcutOperationKind::UpdateOwned => {
                        matches!(owned, Some(zup_exec::OwnedResource::Shortcut { link_path, installed, .. }) if link_path == &op.link_path && Some(installed) == shortcut_from_observed(&op.previous).as_ref())
                    }
                    ShortcutOperationKind::RestoreOwned => {
                        matches!(owned, Some(OwnedResource::Shortcut { link_path, installed: zup_exec::ShortcutState::Link { target, arguments, working_directory }, .. }) if link_path == &op.link_path && target == &op.target && arguments == &op.arguments && working_directory == &op.working_directory && matches!(op.previous, zup_exec::ObservedShortcutState::Absent))
                    }
                    _ => false,
                },
                ManagedOperation::Service(op) => match op.kind {
                    ServiceOperationKind::Create => {
                        owned.is_none()
                            && matches!(op.previous, zup_exec::ObservedServiceState::Absent)
                    }
                    ServiceOperationKind::UpdateOwned => {
                        matches!(owned, Some(zup_exec::OwnedResource::Service { name, installed, .. }) if name == &op.name && installed == &service_from_observed(&op.previous))
                    }
                    ServiceOperationKind::RestoreOwned => {
                        matches!(owned, Some(OwnedResource::Service { name, installed: zup_exec::ServiceState::Registration { display_name, command, start }, .. }) if name == &op.name && display_name == &op.display_name && command == &op.command && start == &op.start && matches!(op.previous, zup_exec::ObservedServiceState::Absent))
                    }
                    _ => false,
                },
                ManagedOperation::Path(op) => match op.kind {
                    PathOperationKind::Add => {
                        !op.previously_owned
                            && owned.is_none()
                            && matches!(op.previous, zup_exec::PathEntryState::Absent)
                    }
                    PathOperationKind::UpdateOwned => {
                        matches!(owned, Some(OwnedResource::PathEntry { value, .. }) if value == &op.value)
                    }
                    PathOperationKind::RestoreOwned => {
                        op.previously_owned
                            && matches!(op.previous, zup_exec::PathEntryState::Absent)
                            && matches!(owned, Some(OwnedResource::PathEntry { value, .. }) if value == &op.value)
                    }
                    _ => false,
                },
                ManagedOperation::Protocol(op) => match op.kind {
                    ProtocolOperationKind::Create => {
                        owned.is_none()
                            && matches!(op.previous, zup_exec::ObservedProtocolState::Absent)
                    }
                    ProtocolOperationKind::UpdateOwned => {
                        matches!(owned, Some(OwnedResource::Protocol { installed: zup_exec::ProtocolState::Registration { command }, .. }) if matches!(&op.previous, zup_exec::ObservedProtocolState::Registration { command: observed, url_protocol_marker: true } if observed == command))
                    }
                    ProtocolOperationKind::RestoreOwned => {
                        matches!(op.previous, zup_exec::ObservedProtocolState::Absent)
                            && matches!(owned, Some(OwnedResource::Protocol { installed: zup_exec::ProtocolState::Registration { command }, .. }) if command == &op.command)
                    }
                    _ => false,
                },
                ManagedOperation::ProgId(op) => match op.prog_id_kind {
                    FileTypeOperationKind::Create => {
                        owned.is_none()
                            && matches!(op.previous_id, zup_exec::ObservedProgIdState::Absent)
                    }
                    FileTypeOperationKind::UpdateOwned => {
                        matches!(owned, Some(OwnedResource::ProgId { installed: zup_exec::ProgIdState::Registration { description, command }, .. }) if matches!(&op.previous_id, zup_exec::ObservedProgIdState::Registration { description: observed_description, command: observed_command } if observed_description == description && observed_command == command))
                    }
                    FileTypeOperationKind::RestoreOwned => {
                        matches!(op.previous_id, zup_exec::ObservedProgIdState::Absent)
                            && matches!(owned, Some(OwnedResource::ProgId { installed: zup_exec::ProgIdState::Registration { description, command }, .. }) if description == &op.description && command == &op.command)
                    }
                    _ => false,
                },
                ManagedOperation::Extension(op) => match op.extension_kind {
                    FileTypeOperationKind::Create => {
                        owned.is_none()
                            && matches!(
                                op.previous_extension,
                                zup_exec::ObservedExtensionState::Absent
                            )
                    }
                    FileTypeOperationKind::UpdateOwned => {
                        matches!(owned, Some(OwnedResource::Extension { installed: zup_exec::ExtensionState::Mapped { prog_id }, .. }) if matches!(&op.previous_extension, zup_exec::ObservedExtensionState::Mapped { prog_id: observed } if observed.eq_ignore_ascii_case(prog_id)))
                    }
                    FileTypeOperationKind::RestoreOwned => {
                        matches!(
                            op.previous_extension,
                            zup_exec::ObservedExtensionState::Absent
                        ) && matches!(owned, Some(OwnedResource::Extension { installed: zup_exec::ExtensionState::Mapped { prog_id }, .. }) if prog_id == &op.id)
                    }
                    _ => false,
                },
                ManagedOperation::UninstallEntry(op) => match (&op.previous, owned) {
                    (None, None) => true,
                    (
                        Some(previous),
                        Some(OwnedResource::UninstallEntry {
                            scope: owned_scope,
                            state,
                        }),
                    ) => *owned_scope == scope && previous == state,
                    _ => false,
                },
            };
            if !valid {
                return Err(LedgerError::Ownership(node.id.to_string()));
            }
        }
        Ok(())
    }

    /// The caller holds the installation lock. The journal commits first; a
    /// crash before this publication leaves the prior ledger in place.
    pub fn publish_committed(
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
            !matches!(node.kind, NodeKind::Barrier)
                && !matches!(record.nodes.get(&node.id), Some(NodeState::Applied { .. }))
        }) {
            return Err(LedgerError::Uncommitted);
        }
        let previous = self.load(&record.app_id, scope)?;
        if previous
            .as_ref()
            .is_some_and(|l| l.committed_transaction == record.transaction_id.to_string())
        {
            cleanup_committed_files(record)?;
            return Ok(previous.expect("checked above"));
        }
        if record.plan.uninstall {
            if previous.as_ref().is_some_and(|ledger| {
                record.plan.retired_keys.len() != ledger.resources.len()
                    || record
                        .plan
                        .retired_keys
                        .iter()
                        .any(|key| !ledger.resources.contains_key(key))
            }) {
                return Err(LedgerError::Invalid);
            }
            let path = self.path(&record.app_id, scope);
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(source) => return Err(LedgerError::Io { path, source }),
            }
            cleanup_committed_files(record)?;
            cleanup_application_state(&self.root, record)?;
            return Ok(InstallLedger::new(record.app_id.clone(), scope));
        }
        let mut ledger =
            previous.unwrap_or_else(|| InstallLedger::new(record.app_id.clone(), scope));
        ledger.version = record.app_version.clone();
        ledger.selected_components = record.plan.selected_components.clone();
        for node in &record.plan.nodes {
            let key = match &node.kind {
                NodeKind::ManagedIntegration { key, .. } | NodeKind::FileMutation { key, .. } => {
                    key
                }
                _ => continue,
            };
            let Some(NodeState::Applied { receipt }) = record.nodes.get(&node.id) else {
                continue;
            };
            if matches!(node.kind, NodeKind::FileMutation { .. }) {
                let Some(source_relative) = node.meta.source_relative.clone() else {
                    return Err(LedgerError::Invalid);
                };
                let (destination, digest, size, created_directories) = match receipt.as_ref() {
                    OperationReceipt::CreateFile {
                        destination,
                        installed_sha256,
                        installed_size,
                        created_directories,
                    } => {
                        let mut directories: Vec<_> = created_directories
                            .iter()
                            .map(|path| {
                                zup_platform::TargetPath::new(PathBuf::from(path))
                                    .map_err(|_| LedgerError::Invalid)
                            })
                            .collect::<Result<Vec<_>, _>>()?;
                        if let Some(OwnedResource::File {
                            created_directories: old,
                            ..
                        }) = ledger.resources.get(key)
                        {
                            for directory in old {
                                if !directories.contains(directory) {
                                    directories.push(directory.clone());
                                }
                            }
                        }
                        (destination, installed_sha256, *installed_size, directories)
                    }
                    OperationReceipt::ReplaceFile {
                        destination,
                        new_sha256,
                        new_size,
                        ..
                    } => {
                        let directories = match ledger.resources.get(key) {
                            Some(OwnedResource::File {
                                created_directories,
                                ..
                            }) => created_directories.clone(),
                            _ => Vec::new(),
                        };
                        (destination, new_sha256, *new_size, directories)
                    }
                    _ => return Err(LedgerError::Invalid),
                };
                let expected_path = match key {
                    ResourceKey::File { destination }
                    | ResourceKey::Maintenance { destination, .. } => destination,
                    _ => return Err(LedgerError::Invalid),
                };
                let sha256 = digest.parse().map_err(|_| LedgerError::Invalid)?;
                if destination != expected_path
                    || node.meta.expected_sha256 != Some(sha256)
                    || node.meta.expected_size != Some(size)
                {
                    return Err(LedgerError::Invalid);
                }
                ledger.resources.insert(
                    key.clone(),
                    OwnedResource::File {
                        destination: zup_platform::TargetPath::new(PathBuf::from(destination))
                            .map_err(|_| LedgerError::Invalid)?,
                        source_relative,
                        sha256,
                        size,
                        created_directories,
                    },
                );
                continue;
            }
            match (node.meta.managed.as_ref(), receipt.as_ref()) {
                (
                    Some(ManagedOperation::Shortcut(op)),
                    OperationReceipt::Shortcut {
                        link_path,
                        previous,
                        installed,
                    },
                ) if link_path == &op.link_path
                    && Some(previous.as_ref()) == shortcut_from_observed(&op.previous).as_ref()
                    && installed.as_ref()
                        == &ShortcutState::Link {
                            target: op.target.clone(),
                            arguments: op.arguments.clone(),
                            working_directory: op.working_directory.clone(),
                        } => {}
                (
                    Some(ManagedOperation::Service(op)),
                    OperationReceipt::Service {
                        name,
                        previous,
                        installed,
                    },
                ) if name == &op.name
                    && previous.as_ref() == &service_from_observed(&op.previous)
                    && installed.as_ref()
                        == &ServiceState::Registration {
                            display_name: op.display_name.clone(),
                            command: op.command.clone(),
                            start: op.start,
                        } => {}
                (Some(ManagedOperation::Shortcut(_) | ManagedOperation::Service(_)), _) => {
                    return Err(LedgerError::Invalid);
                }
                _ => {}
            }
            let owned = match receipt.as_ref() {
                OperationReceipt::Shortcut {
                    link_path,
                    previous,
                    installed,
                } => {
                    let original = match ledger.resources.get(key) {
                        Some(OwnedResource::Shortcut { previous, .. }) => previous.clone(),
                        _ => *previous.clone(),
                    };
                    OwnedResource::Shortcut {
                        link_path: link_path.clone(),
                        previous: original,
                        installed: *installed.clone(),
                    }
                }
                OperationReceipt::Service {
                    name,
                    previous,
                    installed,
                } => {
                    let original = match ledger.resources.get(key) {
                        Some(OwnedResource::Service { previous, .. }) => previous.clone(),
                        _ => *previous.clone(),
                    };
                    OwnedResource::Service {
                        name: name.clone(),
                        previous: original,
                        installed: *installed.clone(),
                    }
                }
                OperationReceipt::PathEntry {
                    entry, value_type, ..
                } => {
                    let value = zup_platform::TargetPath::new(PathBuf::from(entry))
                        .map_err(|_| LedgerError::Invalid)?;
                    let original_type = match ledger.resources.get(key) {
                        Some(OwnedResource::PathEntry { value_type, .. }) => value_type.clone(),
                        _ => value_type.clone(),
                    };
                    OwnedResource::PathEntry {
                        value,
                        value_type: original_type,
                    }
                }
                OperationReceipt::Protocol {
                    previous,
                    installed,
                    ..
                } => {
                    let original = match ledger.resources.get(key) {
                        Some(OwnedResource::Protocol { previous, .. }) => previous.clone(),
                        _ => previous.clone(),
                    };
                    OwnedResource::Protocol {
                        previous: original,
                        installed: installed.clone(),
                    }
                }
                OperationReceipt::ProgId {
                    previous,
                    installed,
                    ..
                } => {
                    let original = match ledger.resources.get(key) {
                        Some(OwnedResource::ProgId { previous, .. }) => previous.clone(),
                        _ => previous.clone(),
                    };
                    OwnedResource::ProgId {
                        previous: original,
                        installed: installed.clone(),
                    }
                }
                OperationReceipt::Extension {
                    previous,
                    installed,
                    ..
                } => {
                    let original = match ledger.resources.get(key) {
                        Some(OwnedResource::Extension { previous, .. }) => previous.clone(),
                        _ => previous.clone(),
                    };
                    OwnedResource::Extension {
                        previous: original,
                        installed: installed.clone(),
                    }
                }
                OperationReceipt::UninstallEntry {
                    scope,
                    installed: Some(state),
                    ..
                } => OwnedResource::UninstallEntry {
                    scope: *scope,
                    state: state.clone(),
                },
                _ => continue,
            };
            ledger.resources.insert(key.clone(), owned);
        }
        for key in &record.plan.retired_keys {
            let directories = match ledger.resources.get(key) {
                Some(OwnedResource::File {
                    created_directories,
                    ..
                }) => created_directories.clone(),
                _ => Vec::new(),
            };
            for directory in directories {
                let inheritor = ledger.resources.iter().find_map(|(candidate, owned)| {
                    if candidate == key || record.plan.retired_keys.contains(candidate) {
                        return None;
                    }
                    match owned {
                        OwnedResource::File { destination, .. }
                            if destination.as_path().starts_with(directory.as_path()) =>
                        {
                            Some(candidate.clone())
                        }
                        _ => None,
                    }
                });
                if let Some(inheritor) = inheritor
                    && let Some(OwnedResource::File {
                        created_directories,
                        ..
                    }) = ledger.resources.get_mut(&inheritor)
                    && !created_directories.contains(&directory)
                {
                    created_directories.push(directory);
                }
            }
            ledger.resources.remove(key);
        }
        ledger.committed_transaction = record.transaction_id.to_string();
        let path = self.path(&record.app_id, scope);
        let bytes = serde_json::to_vec_pretty(&ledger)?;
        write_durable(Path::new(&path), &bytes)?;
        cleanup_committed_files(record)?;
        Ok(ledger)
    }
}

fn valid_file_key(key: &ResourceKey, app_id: &AppId, app_version: &semver::Version) -> bool {
    match key {
        ResourceKey::File { .. } => true,
        ResourceKey::Maintenance {
            app_id: resource_app,
            version,
            destination,
        } => {
            resource_app == app_id.as_str()
                && semver::Version::parse(version).is_ok_and(|parsed| &parsed == app_version)
                && zup_platform::TargetPath::new(PathBuf::from(destination)).is_ok()
        }
        _ => false,
    }
}

fn cleanup_application_state(
    root: &Path,
    uninstall: &TransactionRecord,
) -> Result<(), LedgerError> {
    let transactions = root.join("transactions");
    let entries = match std::fs::read_dir(&transactions) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(LedgerError::Io {
                path: transactions,
                source,
            });
        }
    };
    let store = FilesystemTransactionStore::new(root);
    for entry in entries {
        let entry = entry.map_err(|source| LedgerError::Io {
            path: transactions.clone(),
            source,
        })?;
        let Some(id) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<uuid::Uuid>().ok())
        else {
            continue;
        };
        let transaction_id = TransactionId::from_uuid(id);
        let record = store
            .load(&transaction_id)
            .map_err(|_| LedgerError::Invalid)?;
        if record.app_id != uninstall.app_id || record.scope != uninstall.scope {
            continue;
        }
        if record.transaction_id == uninstall.transaction_id {
            continue;
        }
        if !matches!(
            record.phase,
            TransactionPhase::Committed | TransactionPhase::RolledBack
        ) {
            return Err(LedgerError::RecoveryRequired(
                record.transaction_id.to_string(),
            ));
        }
        let directory = entry.path();
        if std::fs::symlink_metadata(&directory).is_ok_and(|metadata| metadata.file_type().is_dir())
        {
            std::fs::remove_dir_all(&directory).map_err(|source| LedgerError::Io {
                path: directory,
                source,
            })?;
        } else {
            return Err(LedgerError::Invalid);
        }
        let work = root.join("work").join(id.to_string());
        match std::fs::symlink_metadata(&work) {
            Ok(metadata) if metadata.file_type().is_dir() => std::fs::remove_dir_all(&work)
                .map_err(|source| LedgerError::Io { path: work, source })?,
            Ok(_) => return Err(LedgerError::Invalid),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => return Err(LedgerError::Io { path: work, source }),
        }
    }
    let transaction_dir = transactions.join(uninstall.transaction_id.to_string());
    match std::fs::symlink_metadata(&transaction_dir) {
        Ok(metadata) if metadata.file_type().is_dir() => {
            std::fs::remove_dir_all(&transaction_dir).map_err(|source| LedgerError::Io {
                path: transaction_dir,
                source,
            })?;
        }
        Ok(_) => return Err(LedgerError::Invalid),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(LedgerError::Io {
                path: transaction_dir,
                source,
            });
        }
    }
    let uninstall_work = root.join("work").join(uninstall.transaction_id.to_string());
    match std::fs::symlink_metadata(&uninstall_work) {
        Ok(metadata) if metadata.file_type().is_dir() => std::fs::remove_dir_all(&uninstall_work)
            .map_err(|source| LedgerError::Io {
            path: uninstall_work,
            source,
        })?,
        Ok(_) => return Err(LedgerError::Invalid),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(LedgerError::Io {
                path: uninstall_work,
                source,
            });
        }
    }
    for directory in [
        root.join("transactions"),
        root.join("work"),
        root.join("installations"),
    ] {
        match std::fs::remove_dir(&directory) {
            Ok(()) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty
                ) => {}
            Err(source) => {
                return Err(LedgerError::Io {
                    path: directory,
                    source,
                });
            }
        }
    }
    Ok(())
}

fn cleanup_committed_files(record: &TransactionRecord) -> Result<(), LedgerError> {
    let mut directories = std::collections::BTreeSet::new();
    for node in &record.plan.nodes {
        let Some(NodeState::Applied { receipt }) = record.nodes.get(&node.id) else {
            continue;
        };
        if let OperationReceipt::RemoveFile {
            backup_path,
            sha256,
            size,
            ..
        } = receipt.as_ref()
        {
            let backup = backup_path.as_path();
            match std::fs::symlink_metadata(backup) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(source) => {
                    return Err(LedgerError::Io {
                        path: backup.to_path_buf(),
                        source,
                    });
                }
                Ok(metadata) if metadata.file_type().is_file() => {
                    let file = std::fs::File::open(backup).map_err(|source| LedgerError::Io {
                        path: backup.to_path_buf(),
                        source,
                    })?;
                    let found = zup_core::hash_reader(file).map_err(|source| LedgerError::Io {
                        path: backup.to_path_buf(),
                        source,
                    })?;
                    if found != (*size, *sha256) {
                        return Err(LedgerError::Invalid);
                    }
                    std::fs::remove_file(backup).map_err(|source| LedgerError::Io {
                        path: backup.to_path_buf(),
                        source,
                    })?;
                }
                Ok(_) => return Err(LedgerError::Invalid),
            }
            if let Some(OwnedResource::File {
                created_directories,
                ..
            }) = &node.meta.removal
            {
                directories.extend(
                    created_directories
                        .iter()
                        .map(|directory| directory.as_path().to_path_buf()),
                );
            }
        }
    }
    let mut directories: Vec<_> = directories.into_iter().collect();
    directories.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    for directory in directories {
        match std::fs::remove_dir(&directory) {
            Ok(()) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty
                ) => {}
            Err(source) => {
                return Err(LedgerError::Io {
                    path: directory,
                    source,
                });
            }
        }
    }
    Ok(())
}

fn shortcut_from_observed(observed: &zup_exec::ObservedShortcutState) -> Option<ShortcutState> {
    match observed {
        zup_exec::ObservedShortcutState::Absent => Some(ShortcutState::Absent),
        zup_exec::ObservedShortcutState::Shortcut {
            target,
            arguments,
            working_directory,
        } => Some(ShortcutState::Link {
            target: target.clone(),
            arguments: arguments.clone(),
            working_directory: working_directory.clone(),
        }),
        _ => None,
    }
}

fn service_from_observed(observed: &zup_exec::ObservedServiceState) -> ServiceState {
    match observed {
        zup_exec::ObservedServiceState::Absent => ServiceState::Absent,
        zup_exec::ObservedServiceState::Service {
            display_name,
            command,
            start,
            ..
        } => ServiceState::Registration {
            display_name: display_name.clone(),
            command: command.clone(),
            start: *start,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use zup_core::{ResourceKey, SelectedScope};
    use zup_exec::{
        ExecutionPlan, ExecutionSummary, PathEntryState, PathOperation, PathOperationKind,
    };
    use zup_platform::TargetPath;
    use zup_transaction::{TransactionId, compile_transaction};

    fn make_record(app_id: &AppId, entry: &str) -> TransactionRecord {
        let value = TargetPath::new(PathBuf::from(entry)).unwrap();
        let execution = ExecutionPlan {
            selected_components: vec![],
            uninstall: false,
            removals: vec![],
            files: Vec::new(),
            shortcuts: Vec::new(),
            services: Vec::new(),
            protocols: Vec::new(),
            file_types: Vec::new(),
            uninstall_entries: Vec::new(),
            external_actions: Vec::new(),
            path_entries: vec![PathOperation {
                key: ResourceKey::PathEntry {
                    value: entry.into(),
                },
                kind: PathOperationKind::Add,
                value,
                scope: SelectedScope::User,
                previous: PathEntryState::Absent,
                previously_owned: false,
                conflict: None,
            }],
            summary: ExecutionSummary::default(),
        };
        let plan = compile_transaction(&execution).unwrap();
        TransactionRecord::new(
            TransactionId::new_v7(),
            app_id.clone(),
            SelectedScope::User,
            "1.0.0".parse().unwrap(),
            plan,
        )
    }

    #[test]
    fn ledger_publishes_only_committed_receipts() {
        let root = TempDir::new().unwrap();
        let store = InstallLedgerStore::new(root.path());
        let app_id = AppId::new("com.zup.ledger-test").unwrap();
        let mut record = make_record(&app_id, r"C:\ZupLedgerTest\bin");
        assert!(matches!(
            store.publish_committed(&record, SelectedScope::User),
            Err(LedgerError::Uncommitted)
        ));
        assert!(store.load(&app_id, SelectedScope::User).unwrap().is_none());
        record.phase = TransactionPhase::Committed;
        assert!(matches!(
            store.publish_committed(&record, SelectedScope::User),
            Err(LedgerError::Uncommitted)
        ));
        let node = record
            .plan
            .nodes
            .iter()
            .find(|node| matches!(node.kind, NodeKind::ManagedIntegration { .. }))
            .unwrap();
        record.nodes.insert(
            node.id.clone(),
            NodeState::Applied {
                receipt: Box::new(OperationReceipt::PathEntry {
                    scope: SelectedScope::User,
                    entry: r"C:\ZupLedgerTest\bin".into(),
                    value_type: "expand_sz".into(),
                }),
            },
        );
        let published = store
            .publish_committed(&record, SelectedScope::User)
            .unwrap();
        assert_eq!(published.resources.len(), 1);
        assert_eq!(
            store.load(&app_id, SelectedScope::User).unwrap(),
            Some(published.clone())
        );
        let mut failed = make_record(&app_id, r"C:\ZupLedgerTest\other");
        failed.phase = TransactionPhase::RolledBack;
        assert!(matches!(
            store.publish_committed(&failed, SelectedScope::User),
            Err(LedgerError::Uncommitted)
        ));
        assert_eq!(
            store.load(&app_id, SelectedScope::User).unwrap(),
            Some(published)
        );
    }

    #[test]
    fn plan_scope_and_claimed_ownership_are_checked_before_intent() {
        let root = TempDir::new().unwrap();
        let store = InstallLedgerStore::new(root.path());
        let app_id = AppId::new("com.zup.scope-test").unwrap();
        let record = make_record(&app_id, r"C:\ZupScopeTest\bin");
        let version = semver::Version::new(1, 0, 0);
        store
            .validate_plan(&app_id, SelectedScope::User, &version, &record.plan)
            .unwrap();
        assert!(matches!(
            store.validate_plan(&app_id, SelectedScope::Machine, &version, &record.plan),
            Err(LedgerError::Ownership(_))
        ));
        let mut forged = record.plan.clone();
        let node = forged
            .nodes
            .iter_mut()
            .find(|node| matches!(node.kind, NodeKind::ManagedIntegration { .. }))
            .unwrap();
        let Some(ManagedOperation::Path(op)) = node.meta.managed.as_mut() else {
            panic!("path node")
        };
        op.kind = PathOperationKind::UpdateOwned;
        assert!(matches!(
            store.validate_plan(&app_id, SelectedScope::User, &version, &forged),
            Err(LedgerError::Ownership(_))
        ));
    }

    #[test]
    fn committed_journal_repairs_missing_ledger() {
        let root = TempDir::new().unwrap();
        let app_id = AppId::new("com.zup.repair-test").unwrap();
        let mut record = make_record(&app_id, r"C:\ZupRepairTest\bin");
        let node = record
            .plan
            .nodes
            .iter()
            .find(|node| matches!(node.kind, NodeKind::ManagedIntegration { .. }))
            .unwrap();
        record.nodes.insert(
            node.id.clone(),
            NodeState::Applied {
                receipt: Box::new(OperationReceipt::PathEntry {
                    scope: SelectedScope::User,
                    entry: r"C:\ZupRepairTest\bin".into(),
                    value_type: "expand_sz".into(),
                }),
            },
        );
        record.phase = TransactionPhase::Committed;
        FilesystemTransactionStore::new(root.path())
            .create(&record)
            .unwrap();
        let ledger = InstallLedgerStore::new(root.path());
        assert!(ledger.load(&app_id, SelectedScope::User).unwrap().is_none());
        ledger
            .repair_committed(&app_id, SelectedScope::User)
            .unwrap();
        let repaired = ledger.load(&app_id, SelectedScope::User).unwrap().unwrap();
        assert_eq!(
            repaired.committed_transaction,
            record.transaction_id.to_string()
        );
        ledger
            .repair_committed(&app_id, SelectedScope::User)
            .unwrap();
        assert_eq!(
            ledger.load(&app_id, SelectedScope::User).unwrap(),
            Some(repaired)
        );
    }

    #[test]
    fn committed_maintenance_publication_recovers_authoritative_version() {
        let root = TempDir::new().unwrap();
        let app_id = AppId::new("com.zup.maintenance-recovery").unwrap();
        let destination = TargetPath::new(
            root.path()
                .join("maintenance/com.zup.maintenance-recovery/user/2.0.0/Setup.exe"),
        )
        .unwrap();
        let destination_string = destination.to_string();
        let (size, digest) = zup_core::hash_reader(&b"maintenance v2"[..]).unwrap();
        let execution = zup_exec::ExecutionPlan {
            files: vec![zup_exec::FileOperation {
                key: ResourceKey::Maintenance {
                    app_id: app_id.to_string(),
                    version: "2.0.0".into(),
                    destination: destination_string.clone(),
                },
                kind: zup_exec::FileOperationKind::Create,
                destination: destination.clone(),
                source_relative: zup_core::RelativePath::new("__zup_maintenance__.exe").unwrap(),
                precondition: zup_exec::FilePrecondition::Absent,
                expected_sha256: digest,
                expected_size: size,
                conflict: None,
            }],
            ..Default::default()
        };
        let plan = compile_transaction(&execution).unwrap();
        let mut record = TransactionRecord::new(
            TransactionId::new_v7(),
            app_id.clone(),
            SelectedScope::User,
            "2.0.0".parse().unwrap(),
            plan,
        );
        let mutation = record
            .plan
            .nodes
            .iter()
            .find(|node| matches!(node.kind, NodeKind::FileMutation { .. }))
            .unwrap();
        record.nodes.insert(
            mutation.id.clone(),
            NodeState::Applied {
                receipt: Box::new(OperationReceipt::CreateFile {
                    destination: destination_string,
                    installed_sha256: digest.to_hex(),
                    installed_size: size,
                    created_directories: vec![],
                }),
            },
        );
        let stage = record
            .plan
            .nodes
            .iter()
            .find(|node| matches!(node.kind, NodeKind::StageFile { .. }))
            .unwrap();
        record.nodes.insert(
            stage.id.clone(),
            NodeState::Applied {
                receipt: Box::new(OperationReceipt::StageFile {
                    staged_path: root
                        .path()
                        .join("work/staged-Setup.exe")
                        .display()
                        .to_string(),
                    sha256: digest.to_hex(),
                    size,
                }),
            },
        );
        record.phase = TransactionPhase::Committed;
        FilesystemTransactionStore::new(root.path())
            .create(&record)
            .unwrap();
        let ledgers = InstallLedgerStore::new(root.path());
        assert!(
            ledgers
                .load(&app_id, SelectedScope::User)
                .unwrap()
                .is_none()
        );
        ledgers
            .repair_committed(&app_id, SelectedScope::User)
            .unwrap();
        let repaired = ledgers.load(&app_id, SelectedScope::User).unwrap().unwrap();
        assert_eq!(repaired.version.to_string(), "2.0.0");
        assert!(repaired.resources.keys().any(
            |key| matches!(key, ResourceKey::Maintenance { version, .. } if version == "2.0.0")
        ));
    }

    #[test]
    fn shortcut_and_service_ownership_publishes_after_commit() {
        let root = TempDir::new().unwrap();
        let store = InstallLedgerStore::new(root.path());
        let app_id = AppId::new("com.zup.managed-test").unwrap();
        let target = TargetPath::new(PathBuf::from(r"C:\Zup\App.exe")).unwrap();
        let link_path = TargetPath::new(PathBuf::from(r"C:\Zup\App.lnk")).unwrap();
        let name = "zup-managed-test";
        let command = zup_platform::CommandSpec::new(target.clone(), vec!["--svc".into()]);
        let execution = ExecutionPlan {
            selected_components: vec![],
            uninstall: false,
            removals: vec![],
            files: vec![],
            path_entries: vec![],
            protocols: vec![],
            file_types: vec![],
            uninstall_entries: vec![],
            external_actions: vec![],
            shortcuts: vec![zup_exec::ShortcutOperation {
                key: ResourceKey::Shortcut {
                    location: zup_core::ShortcutLocation::Desktop,
                    name: "App".into(),
                },
                kind: ShortcutOperationKind::Create,
                link_path: link_path.clone(),
                target: target.clone(),
                arguments: vec![],
                working_directory: None,
                previous: zup_exec::ObservedShortcutState::Absent,
                conflict: None,
            }],
            services: vec![zup_exec::ServiceOperation {
                key: ResourceKey::Service {
                    id: zup_core::ServiceId::new(name).unwrap(),
                },
                kind: ServiceOperationKind::Create,
                id: name.into(),
                name: name.into(),
                display_name: "Zup Managed Test".into(),
                command: command.clone(),
                start: zup_core::ServiceStart::Disabled,
                previous: zup_exec::ObservedServiceState::Absent,
                conflict: None,
            }],
            summary: ExecutionSummary::default(),
        };
        let plan = compile_transaction(&execution).unwrap();
        let version = semver::Version::new(1, 0, 0);
        store
            .validate_plan(&app_id, SelectedScope::Machine, &version, &plan)
            .unwrap();
        assert!(matches!(
            store.validate_plan(&app_id, SelectedScope::User, &version, &plan),
            Err(LedgerError::Ownership(_))
        ));
        let mut record = TransactionRecord::new(
            TransactionId::new_v7(),
            app_id.clone(),
            SelectedScope::Machine,
            "1.0.0".parse().unwrap(),
            plan,
        );
        for node in &record.plan.nodes {
            let receipt = match &node.meta.managed {
                Some(ManagedOperation::Shortcut(_)) => OperationReceipt::Shortcut {
                    link_path: link_path.clone(),
                    previous: Box::new(ShortcutState::Absent),
                    installed: Box::new(ShortcutState::Link {
                        target: target.clone(),
                        arguments: vec![],
                        working_directory: None,
                    }),
                },
                Some(ManagedOperation::Service(_)) => OperationReceipt::Service {
                    name: name.into(),
                    previous: Box::new(ServiceState::Absent),
                    installed: Box::new(ServiceState::Registration {
                        display_name: "Zup Managed Test".into(),
                        command: command.clone(),
                        start: zup_core::ServiceStart::Disabled,
                    }),
                },
                _ => continue,
            };
            record.nodes.insert(
                node.id.clone(),
                NodeState::Applied {
                    receipt: Box::new(receipt),
                },
            );
        }
        assert!(matches!(
            store.publish_committed(&record, SelectedScope::Machine),
            Err(LedgerError::Uncommitted)
        ));
        assert!(
            store
                .load(&app_id, SelectedScope::Machine)
                .unwrap()
                .is_none()
        );
        record.phase = TransactionPhase::Committed;
        let ledger = store
            .publish_committed(&record, SelectedScope::Machine)
            .unwrap();
        assert_eq!(ledger.resources.len(), 2);
        assert_eq!(
            store.load(&app_id, SelectedScope::Machine).unwrap(),
            Some(ledger)
        );
    }
}
