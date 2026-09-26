//! One desired-state transition for install, upgrade, modify, repair, or uninstall.

use std::collections::{BTreeMap, BTreeSet};

use thiserror::Error;
use zup_core::{Privilege, ResourceKey};
use zup_platform::TargetPlan;

use crate::{
    ExecutionPlan, ExecutionPlanError, ExtensionState, FileAssociationOperationKind,
    FileAssociationState, FileOperationKind, FilePrecondition, HostSnapshot, InstallLedger,
    LauncherOperationKind, LauncherState, ObservedExtensionState, ObservedFileAssociationState,
    ObservedFileState, ObservedLauncherState, ObservedProtocolState, ObservedServiceState,
    OwnedResource, PathOperationKind, ProtocolOperationKind, ProtocolState, RemovalKind,
    RemovalOperation, ServiceOperationKind, ServiceState, plan_execution,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleAction {
    Install,
    Upgrade,
    Modify,
    Repair { force_files: bool },
    Uninstall,
}

#[derive(Debug, Error)]
pub enum LifecycleError {
    #[error("a committed installation is required")]
    NotInstalled,
    #[error("installation already exists")]
    AlreadyInstalled,
    #[error("this transition requires a desired target plan")]
    MissingTarget,
    #[error("downgrade from {installed} to {requested} is refused")]
    Downgrade {
        installed: semver::Version,
        requested: semver::Version,
    },
    #[error("{action} requires version {installed}, found {requested}")]
    VersionMismatch {
        action: &'static str,
        installed: semver::Version,
        requested: semver::Version,
    },
    #[error("repair target differs from committed owned state: {key:?}")]
    RepairDesiredChanged { key: ResourceKey },
    #[error("missing ownership observation for {0:?}")]
    MissingOwnedObservation(ResourceKey),
    #[error(transparent)]
    Execution(#[from] ExecutionPlanError),
}

/// `owned_matches` is a read-only observation of each previously owned resource.
/// Executors repeat every ownership check immediately before mutation.
pub fn plan_lifecycle(
    action: LifecycleAction,
    target: Option<&TargetPlan>,
    snapshot: Option<&HostSnapshot>,
    ledger: Option<&InstallLedger>,
    owned_matches: &BTreeMap<ResourceKey, bool>,
) -> Result<ExecutionPlan, LifecycleError> {
    let desired = match action {
        LifecycleAction::Uninstall => None,
        _ => Some(target.ok_or(LifecycleError::MissingTarget)?),
    };
    match action {
        LifecycleAction::Install if ledger.is_some() => {
            return Err(LifecycleError::AlreadyInstalled);
        }
        LifecycleAction::Install => {}
        _ if ledger.is_none() => return Err(LifecycleError::NotInstalled),
        _ => {}
    }
    if let (Some(target), Some(ledger)) = (desired, ledger) {
        if target.app.id != ledger.app_id || target.scope != ledger.scope {
            return Err(LifecycleError::Execution(
                ExecutionPlanError::LedgerMismatch,
            ));
        }
        match action {
            LifecycleAction::Upgrade if target.app.version < ledger.version => {
                return Err(LifecycleError::Downgrade {
                    installed: ledger.version.clone(),
                    requested: target.app.version.clone(),
                });
            }
            LifecycleAction::Upgrade if target.app.version == ledger.version => {
                return Err(LifecycleError::VersionMismatch {
                    action: "upgrade",
                    installed: ledger.version.clone(),
                    requested: target.app.version.clone(),
                });
            }
            LifecycleAction::Modify | LifecycleAction::Repair { .. }
                if target.app.version != ledger.version =>
            {
                return Err(LifecycleError::VersionMismatch {
                    action: if matches!(action, LifecycleAction::Modify) {
                        "modify"
                    } else {
                        "repair"
                    },
                    installed: ledger.version.clone(),
                    requested: target.app.version.clone(),
                });
            }
            _ => {}
        }
    }
    let mut execution = match desired {
        Some(target) => plan_execution(
            target,
            snapshot.ok_or(LifecycleError::MissingTarget)?,
            ledger,
        )?,
        None => ExecutionPlan::default(),
    };
    if let LifecycleAction::Repair { force_files } = action {
        repair_owned(
            &mut execution,
            desired.unwrap(),
            snapshot.unwrap(),
            ledger.unwrap(),
            force_files,
        )?;
    }
    let desired_keys = desired.map_or_else(BTreeSet::new, target_keys);
    if let Some(ledger) = ledger
        && !matches!(action, LifecycleAction::Repair { .. })
    {
        for (key, owned) in &ledger.resources {
            if desired_keys.contains(key) || matches!(owned, OwnedResource::Backend { .. }) {
                continue;
            }
            let matches = *owned_matches
                .get(key)
                .ok_or_else(|| LifecycleError::MissingOwnedObservation(key.clone()))?;
            execution.removals.push(RemovalOperation {
                key: key.clone(),
                kind: if matches {
                    RemovalKind::RemoveOwned
                } else {
                    RemovalKind::Drift
                },
                scope: ledger.scope,
                // The authority recorded when the resource was installed, not
                // an inference from the scope the ledger happens to carry.
                privilege: owned.privilege(),
                owned: owned.clone(),
            });
        }
    }
    if action == LifecycleAction::Uninstall {
        execution.uninstall = true;
        execution.summary.requires_authorization = execution
            .removals
            .iter()
            .any(|removal| removal.privilege == Privilege::System)
            || ledger.is_some_and(|ledger| {
                ledger
                    .resources
                    .values()
                    .any(|owned| owned.privilege() == Privilege::System)
            });
    }
    Ok(execution)
}

fn target_keys(target: &TargetPlan) -> BTreeSet<ResourceKey> {
    let mut keys = BTreeSet::new();
    keys.extend(target.files.iter().map(|item| item.key.clone()));
    keys.extend(target.launchers.iter().map(|item| item.key.clone()));
    keys.extend(target.path_entries.iter().map(|item| item.key.clone()));
    keys.extend(target.services.iter().map(|item| item.key.clone()));
    keys.extend(target.protocols.iter().map(|item| item.key.clone()));
    for item in &target.file_associations {
        keys.insert(item.key.clone());
        keys.insert(ResourceKey::FileAssociationExtension {
            extension: item.extension.clone(),
        });
    }
    keys
}

fn repair_owned(
    execution: &mut ExecutionPlan,
    target: &TargetPlan,
    snapshot: &HostSnapshot,
    ledger: &InstallLedger,
    force_files: bool,
) -> Result<(), LifecycleError> {
    for (op, observed) in execution.files.iter_mut().zip(&snapshot.files) {
        let owned = ledger.resources.get(&op.key);
        match (owned, op.kind, &observed.state) {
            (
                Some(OwnedResource::File {
                    sha256,
                    size,
                    destination,
                    ..
                }),
                FileOperationKind::Drift,
                ObservedFileState::Absent,
            ) if destination == &op.destination
                && *sha256 == op.expected_sha256
                && *size == op.expected_size =>
            {
                op.kind = FileOperationKind::RestoreOwned;
                op.precondition = FilePrecondition::Absent;
                op.conflict = None;
            }
            (
                Some(OwnedResource::File {
                    sha256,
                    size,
                    destination,
                    ..
                }),
                FileOperationKind::Drift,
                ObservedFileState::File {
                    size: found_size,
                    sha256: found_hash,
                },
            ) if force_files
                && destination == &op.destination
                && *sha256 == op.expected_sha256
                && *size == op.expected_size =>
            {
                op.kind = FileOperationKind::RepairOwned;
                op.precondition = FilePrecondition::Exact {
                    size: *found_size,
                    sha256: *found_hash,
                };
                op.conflict = None;
            }
            (Some(_), FileOperationKind::Replace, _) | (None, FileOperationKind::Create, _) => {
                return Err(LifecycleError::RepairDesiredChanged {
                    key: op.key.clone(),
                });
            }
            _ => {}
        }
    }
    for (op, observed) in execution.launchers.iter_mut().zip(&snapshot.launchers) {
        if op.kind == LauncherOperationKind::Drift
            && matches!(observed.state, ObservedLauncherState::Absent)
            && matches!(ledger.resources.get(&op.key), Some(OwnedResource::Launcher { installed: LauncherState::Launcher { target, arguments, working_directory }, .. }) if target == &op.target && arguments == &op.arguments && working_directory == &op.working_directory)
        {
            op.kind = LauncherOperationKind::RestoreOwned;
            op.conflict = None;
        } else if op.kind == LauncherOperationKind::UpdateOwned
            || (op.kind == LauncherOperationKind::Create && !ledger.resources.contains_key(&op.key))
        {
            return Err(LifecycleError::RepairDesiredChanged {
                key: op.key.clone(),
            });
        }
    }
    for op in &mut execution.path_entries {
        if op.kind == PathOperationKind::Drift && op.previously_owned {
            op.kind = PathOperationKind::RestoreOwned;
            op.conflict = None;
        } else if op.kind == PathOperationKind::Add && !op.previously_owned {
            return Err(LifecycleError::RepairDesiredChanged {
                key: op.key.clone(),
            });
        }
    }
    for (op, observed) in execution.services.iter_mut().zip(&snapshot.services) {
        if op.kind == ServiceOperationKind::Drift
            && matches!(observed.state, ObservedServiceState::Absent)
            && matches!(ledger.resources.get(&op.key), Some(OwnedResource::Service { installed: ServiceState::Registration { display_name, command, start }, .. }) if display_name == &op.display_name && command == &op.command && start == &op.start)
        {
            op.kind = ServiceOperationKind::RestoreOwned;
            op.conflict = None;
        } else if op.kind == ServiceOperationKind::UpdateOwned
            || (op.kind == ServiceOperationKind::Create && !ledger.resources.contains_key(&op.key))
        {
            return Err(LifecycleError::RepairDesiredChanged {
                key: op.key.clone(),
            });
        }
    }
    for (op, observed) in execution.protocols.iter_mut().zip(&snapshot.protocols) {
        if op.kind == ProtocolOperationKind::Drift
            && matches!(observed.state, ObservedProtocolState::Absent)
            && matches!(ledger.resources.get(&op.key), Some(OwnedResource::Protocol { installed: ProtocolState::Registration { command }, .. }) if command == &op.command)
        {
            op.kind = ProtocolOperationKind::RestoreOwned;
            op.conflict = None;
        } else if op.kind == ProtocolOperationKind::UpdateOwned
            || (op.kind == ProtocolOperationKind::Create && !ledger.resources.contains_key(&op.key))
        {
            return Err(LifecycleError::RepairDesiredChanged {
                key: op.key.clone(),
            });
        }
    }
    for (op, observed) in execution
        .file_associations
        .iter_mut()
        .zip(&snapshot.file_associations)
    {
        if op.association_kind == FileAssociationOperationKind::Drift
            && matches!(
                observed.association_state,
                ObservedFileAssociationState::Absent
            )
            && matches!(ledger.resources.get(&op.key), Some(OwnedResource::FileAssociation { installed: FileAssociationState::Registration { description, command }, .. }) if description == &op.description && command == &op.command)
        {
            op.association_kind = FileAssociationOperationKind::RestoreOwned;
        }
        let extension_key = ResourceKey::FileAssociationExtension {
            extension: target
                .file_associations
                .iter()
                .find(|item| item.key == op.key)
                .expect("validated target")
                .extension
                .clone(),
        };
        if op.extension_kind == FileAssociationOperationKind::Drift
            && matches!(observed.extension_state, ObservedExtensionState::Absent)
            && matches!(ledger.resources.get(&extension_key), Some(OwnedResource::Extension { installed: ExtensionState::Mapped { association_id }, .. }) if association_id == &op.id)
        {
            op.extension_kind = FileAssociationOperationKind::RestoreOwned;
        }
        if matches!(
            op.association_kind,
            FileAssociationOperationKind::Create | FileAssociationOperationKind::UpdateOwned
        ) || matches!(
            op.extension_kind,
            FileAssociationOperationKind::Create | FileAssociationOperationKind::UpdateOwned
        ) {
            return Err(LifecycleError::RepairDesiredChanged {
                key: op.key.clone(),
            });
        }
        if (matches!(
            op.association_kind,
            FileAssociationOperationKind::RestoreOwned
        ) || matches!(
            op.extension_kind,
            FileAssociationOperationKind::RestoreOwned
        )) && matches!(
            op.association_kind,
            FileAssociationOperationKind::NoOp | FileAssociationOperationKind::RestoreOwned
        ) && matches!(
            op.extension_kind,
            FileAssociationOperationKind::NoOp | FileAssociationOperationKind::RestoreOwned
        ) {
            op.kind = FileAssociationOperationKind::RestoreOwned;
            op.conflict = None;
        }
    }
    Ok(())
}
