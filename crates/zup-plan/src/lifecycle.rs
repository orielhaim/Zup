//! One desired-state transition for install, upgrade, modify, repair, or uninstall.

use std::collections::{BTreeMap, BTreeSet};

use crate::DesiredState;
use thiserror::Error;
use zup_core::ResourceKey;

use crate::{
    ExecutionPlan, ExecutionPlanError, ExtensionState, FileAssociationState, FileOperationKind,
    FilePrecondition, HostSnapshot, InstallLedger, IntegrationChangeKind, LauncherState,
    ObservedExtensionState, ObservedFileAssociationState, ObservedFileState, ObservedLauncherState,
    ObservedProtocolState, ObservedServiceState, OwnedResource, PathOperationKind, ProtocolState,
    RemovalKind, RemovalOperation, ServiceState, plan_execution,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum LifecycleAction {
    Install,
    Upgrade,
    Modify,
    Repair { force: bool },
    Uninstall,
}

/// Unprivileged choices that an execution authority independently plans.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeploymentIntent {
    pub action: LifecycleAction,
    pub enable: Vec<String>,
    pub disable: Vec<String>,
    pub install_directory: Option<std::path::PathBuf>,
}

#[derive(Debug, Error)]
pub enum LifecycleError {
    #[error("retired resource aliases desired state: {0:?}")]
    AliasedResource(ResourceKey),
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
    target: Option<&DesiredState<crate::TargetPath, crate::TargetPath>>,
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
        None => ExecutionPlan {
            binding: ledger.map(crate::DeploymentBinding::uninstall),
            selected_components: ledger
                .map(|ledger| ledger.selected_components.clone())
                .unwrap_or_default(),
            install_directory: ledger.and_then(|ledger| ledger.install_directory.clone()),
            ..ExecutionPlan::default()
        },
    };
    if let LifecycleAction::Repair { force } = action {
        repair_owned(
            &mut execution,
            desired.unwrap(),
            snapshot.unwrap(),
            ledger.unwrap(),
            force,
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
            if let Some(target) = desired {
                let alias = desired_keys.iter().any(|desired| {
                    desired.canonical_for(&target.target) == key.canonical_for(&target.target)
                }) || match owned {
                    OwnedResource::File { destination, .. } => target
                        .files
                        .iter()
                        .any(|file| destination.equivalent(&file.destination)),
                    OwnedResource::Launcher { launcher_path, .. } => target
                        .launchers
                        .iter()
                        .any(|launcher| launcher_path.equivalent(&launcher.launcher_path)),
                    OwnedResource::Service { name, .. }
                        if target.target.operating_system()
                            == zup_core::TargetOperatingSystem::Windows =>
                    {
                        target.services.iter().any(|service| {
                            service.name.as_str().to_lowercase() == name.to_lowercase()
                        })
                    }
                    _ => false,
                };
                if alias {
                    return Err(LifecycleError::AliasedResource(key.clone()));
                }
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
    }
    Ok(execution)
}

fn target_keys(
    target: &DesiredState<crate::TargetPath, crate::TargetPath>,
) -> BTreeSet<ResourceKey> {
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
    target: &DesiredState<crate::TargetPath, crate::TargetPath>,
    snapshot: &HostSnapshot,
    ledger: &InstallLedger,
    force: bool,
) -> Result<(), LifecycleError> {
    let observations: BTreeMap<_, _> = snapshot
        .files
        .iter()
        .map(|observed| (&observed.key, observed))
        .collect();
    for op in execution.files.iter_mut() {
        let observed = observations[&op.key];
        let owned = ledger.resources.get(&op.key);
        match (owned, op.kind, &observed.state) {
            (
                Some(OwnedResource::File {
                    sha256,
                    size,
                    destination,
                    executable,
                    ..
                }),
                FileOperationKind::Drift,
                ObservedFileState::Absent,
            ) if destination.equivalent(&op.destination)
                && *sha256 == op.expected_sha256
                && *size == op.expected_size
                && *executable == op.executable =>
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
                    executable,
                    ..
                }),
                FileOperationKind::Drift,
                ObservedFileState::File {
                    size: found_size,
                    sha256: found_hash,
                    ..
                },
            ) if force
                && destination.equivalent(&op.destination)
                && *sha256 == op.expected_sha256
                && *size == op.expected_size
                && *executable == op.executable =>
            {
                op.kind = FileOperationKind::RepairOwned;
                op.precondition = FilePrecondition::Exact {
                    size: *found_size,
                    sha256: *found_hash,
                };
                op.conflict = None;
            }
            (
                Some(OwnedResource::File {
                    sha256,
                    size,
                    executable,
                    ..
                }),
                FileOperationKind::Replace,
                _,
            ) if *sha256 == op.expected_sha256
                && *size == op.expected_size
                && *executable == op.executable =>
            {
                op.kind = FileOperationKind::RepairOwned;
            }
            (Some(_), FileOperationKind::Replace, _) | (None, FileOperationKind::Create, _) => {
                return Err(LifecycleError::RepairDesiredChanged {
                    key: op.key.clone(),
                });
            }
            _ => {}
        }
    }
    let observations: BTreeMap<_, _> = snapshot
        .launchers
        .iter()
        .map(|observed| (&observed.key, observed))
        .collect();
    for op in execution.launchers.iter_mut() {
        let observed = observations[&op.key];
        if force
            && op.kind == IntegrationChangeKind::Drift
            && matches!(observed.state, ObservedLauncherState::Launcher { .. })
            && matches!(ledger.resources.get(&op.key), Some(OwnedResource::Launcher { installed: LauncherState::Launcher { target, arguments, working_directory }, .. }) if target == &op.target && arguments == &op.arguments && working_directory == &op.working_directory)
        {
            op.kind = IntegrationChangeKind::RepairOwned;
            op.conflict = None;
        }
        if op.kind == IntegrationChangeKind::Drift
            && matches!(observed.state, ObservedLauncherState::Absent)
            && matches!(ledger.resources.get(&op.key), Some(OwnedResource::Launcher { installed: LauncherState::Launcher { target, arguments, working_directory }, .. }) if target == &op.target && arguments == &op.arguments && working_directory == &op.working_directory)
        {
            op.kind = IntegrationChangeKind::RestoreOwned;
            op.conflict = None;
        } else if op.kind == IntegrationChangeKind::UpdateOwned
            || (op.kind == IntegrationChangeKind::Create && !ledger.resources.contains_key(&op.key))
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
    let observations: BTreeMap<_, _> = snapshot
        .services
        .iter()
        .map(|observed| (&observed.key, observed))
        .collect();
    for op in execution.services.iter_mut() {
        let observed = observations[&op.key];
        if force
            && op.kind == IntegrationChangeKind::Drift
            && matches!(
                observed.state,
                ObservedServiceState::Service { .. } | ObservedServiceState::InvalidService
            )
            && matches!(ledger.resources.get(&op.key), Some(OwnedResource::Service { installed: ServiceState::Registration { display_name, command, start, readiness, execution }, .. }) if display_name == &op.display_name && command == &op.command && start == &op.start && readiness == &op.readiness && execution == &op.execution)
        {
            op.kind = IntegrationChangeKind::RepairOwned;
            op.conflict = None;
        }
        if op.kind == IntegrationChangeKind::Drift
            && matches!(observed.state, ObservedServiceState::Absent)
            && matches!(ledger.resources.get(&op.key), Some(OwnedResource::Service { installed: ServiceState::Registration { display_name, command, start, readiness, execution }, .. }) if display_name == &op.display_name && command == &op.command && start == &op.start && readiness == &op.readiness && execution == &op.execution)
        {
            op.kind = IntegrationChangeKind::RestoreOwned;
            op.conflict = None;
        } else if op.kind == IntegrationChangeKind::UpdateOwned
            || (op.kind == IntegrationChangeKind::Create && !ledger.resources.contains_key(&op.key))
        {
            return Err(LifecycleError::RepairDesiredChanged {
                key: op.key.clone(),
            });
        }
    }
    let observations: BTreeMap<_, _> = snapshot
        .protocols
        .iter()
        .map(|observed| (&observed.key, observed))
        .collect();
    for op in execution.protocols.iter_mut() {
        let observed = observations[&op.key];
        if force
            && op.kind == IntegrationChangeKind::Drift
            && matches!(
                observed.state,
                ObservedProtocolState::Registration {
                    url_protocol_marker: true,
                    ..
                }
            )
            && matches!(ledger.resources.get(&op.key), Some(OwnedResource::Protocol { installed: ProtocolState::Registration { command }, .. }) if command == &op.command)
        {
            op.kind = IntegrationChangeKind::RepairOwned;
            op.conflict = None;
        }
        if op.kind == IntegrationChangeKind::Drift
            && matches!(observed.state, ObservedProtocolState::Absent)
            && matches!(ledger.resources.get(&op.key), Some(OwnedResource::Protocol { installed: ProtocolState::Registration { command }, .. }) if command == &op.command)
        {
            op.kind = IntegrationChangeKind::RestoreOwned;
            op.conflict = None;
        } else if op.kind == IntegrationChangeKind::UpdateOwned
            || (op.kind == IntegrationChangeKind::Create && !ledger.resources.contains_key(&op.key))
        {
            return Err(LifecycleError::RepairDesiredChanged {
                key: op.key.clone(),
            });
        }
    }
    let observations: BTreeMap<_, _> = snapshot
        .file_associations
        .iter()
        .map(|observed| (&observed.key, observed))
        .collect();
    for op in execution.file_associations.iter_mut() {
        let observed = observations[&op.key];
        if force
            && op.association_kind == IntegrationChangeKind::Drift
            && matches!(
                observed.association_state,
                ObservedFileAssociationState::Registration { .. }
            )
            && matches!(ledger.resources.get(&op.key), Some(OwnedResource::FileAssociation { installed: FileAssociationState::Registration { description, command }, .. }) if description == &op.description && command == &op.command)
        {
            op.association_kind = IntegrationChangeKind::RepairOwned;
        }
        if op.association_kind == IntegrationChangeKind::Drift
            && matches!(
                observed.association_state,
                ObservedFileAssociationState::Absent
            )
            && matches!(ledger.resources.get(&op.key), Some(OwnedResource::FileAssociation { installed: FileAssociationState::Registration { description, command }, .. }) if description == &op.description && command == &op.command)
        {
            op.association_kind = IntegrationChangeKind::RestoreOwned;
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
        if force
            && op.extension_kind == IntegrationChangeKind::Drift
            && matches!(
                observed.extension_state,
                ObservedExtensionState::Mapped { .. }
            )
            && matches!(ledger.resources.get(&extension_key), Some(OwnedResource::Extension { installed: ExtensionState::Mapped { association_id }, .. }) if association_id == &op.id)
        {
            op.extension_kind = IntegrationChangeKind::RepairOwned;
        }
        if op.extension_kind == IntegrationChangeKind::Drift
            && matches!(observed.extension_state, ObservedExtensionState::Absent)
            && matches!(ledger.resources.get(&extension_key), Some(OwnedResource::Extension { installed: ExtensionState::Mapped { association_id }, .. }) if association_id == &op.id)
        {
            op.extension_kind = IntegrationChangeKind::RestoreOwned;
        }
        if matches!(
            op.association_kind,
            IntegrationChangeKind::Create | IntegrationChangeKind::UpdateOwned
        ) || matches!(
            op.extension_kind,
            IntegrationChangeKind::Create | IntegrationChangeKind::UpdateOwned
        ) {
            return Err(LifecycleError::RepairDesiredChanged {
                key: op.key.clone(),
            });
        }
        if (matches!(
            op.association_kind,
            IntegrationChangeKind::RestoreOwned | IntegrationChangeKind::RepairOwned
        ) || matches!(
            op.extension_kind,
            IntegrationChangeKind::RestoreOwned | IntegrationChangeKind::RepairOwned
        )) && matches!(
            op.association_kind,
            IntegrationChangeKind::NoOp
                | IntegrationChangeKind::RestoreOwned
                | IntegrationChangeKind::RepairOwned
        ) && matches!(
            op.extension_kind,
            IntegrationChangeKind::NoOp
                | IntegrationChangeKind::RestoreOwned
                | IntegrationChangeKind::RepairOwned
        ) {
            op.kind = if op.association_kind == IntegrationChangeKind::RepairOwned
                || op.extension_kind == IntegrationChangeKind::RepairOwned
            {
                IntegrationChangeKind::RepairOwned
            } else {
                IntegrationChangeKind::RestoreOwned
            };
            op.conflict = None;
        }
    }
    Ok(())
}
