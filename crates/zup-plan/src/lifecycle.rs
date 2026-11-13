//! One desired-state transition for install, upgrade, modify, repair, or uninstall.

use std::collections::{BTreeMap, BTreeSet};

use thiserror::Error;
use zup_core::ResourceKey;

use crate::{
    ExecutionPlan, ExecutionPlanError, ExtensionState, FileAssociationState, FileOperationKind,
    FilePrecondition, HostSnapshot, InstallationRecord, IntegrationChangeKind, LauncherState,
    ObservedExtensionState, ObservedFileAssociationState, ObservedFileState, ObservedLauncherState,
    ObservedProtocolState, ObservedServiceState, OwnedResource, PathOperationKind, ProtocolState,
    RemovalKind, RemovalOperation, plan_execution,
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeploymentTransition {
    #[default]
    Deploy,
    Maintain,
    Remove,
}

impl DeploymentTransition {
    pub const fn is_deploy(self) -> bool {
        matches!(self, Self::Deploy)
    }
    pub const fn is_maintain(self) -> bool {
        matches!(self, Self::Maintain)
    }
    pub const fn is_remove(self) -> bool {
        matches!(self, Self::Remove)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum LifecycleAction {
    Cleanup {
        keep: u16,
    },
    Install,
    Upgrade,
    Downgrade,
    Rollback {
        revision: zup_core::ActivationRevision,
    },
    Modify,
    Repair {
        force: bool,
    },
    Uninstall,
}

impl LifecycleAction {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Cleanup { .. } => "cleanup",
            Self::Install => "install",
            Self::Upgrade => "upgrade",
            Self::Downgrade => "downgrade",
            Self::Rollback { .. } => "rollback",
            Self::Modify => "modify",
            Self::Repair { .. } => "repair",
            Self::Uninstall => "uninstall",
        }
    }
}

/// Unprivileged choices that an execution authority independently plans.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeploymentIntent {
    pub deployment: crate::DeploymentSelection,
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
    #[error(
        "requested downgrade version {requested} is not older than installed version {installed}"
    )]
    NotDowngrade {
        installed: semver::Version,
        requested: semver::Version,
    },
    #[error(
        "rollback must select a retained deployment with its exact requirements and materialization"
    )]
    InvalidRollback,
    #[error("{action} requires version {installed}, found {requested}")]
    VersionMismatch {
        action: &'static str,
        installed: semver::Version,
        requested: semver::Version,
    },
    #[error(
        "repair requirements differ from the committed deployment; use Upgrade or Modify for application changes"
    )]
    RepairRequirementsChanged,
    #[error("repair source does not reproduce the installed component selection or location")]
    RepairSelectionChanged,
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
    target: Option<&crate::BoundDeployment>,
    snapshot: Option<&HostSnapshot>,
    ledger: Option<&InstallationRecord>,
    owned_matches: &BTreeMap<ResourceKey, bool>,
) -> Result<ExecutionPlan, LifecycleError> {
    if let LifecycleAction::Cleanup { .. } = action {
        let record = ledger.ok_or(LifecycleError::NotInstalled)?;
        record
            .validate()
            .map_err(|_| LifecycleError::Execution(ExecutionPlanError::LedgerMismatch))?;
        return Ok(ExecutionPlan {
            binding: Some(crate::DeploymentBinding::recorded(record)),
            selected_components: record.current.selected_components.clone(),
            install_directory: Some(record.directory.clone()),
            transition: DeploymentTransition::Maintain,
            ..ExecutionPlan::default()
        });
    }
    let desired = match action {
        LifecycleAction::Uninstall => None,
        _ => Some(target.ok_or(LifecycleError::MissingTarget)?),
    };
    let installed = ledger.is_some_and(InstallationRecord::is_installed);
    match action {
        LifecycleAction::Install if installed => {
            return Err(LifecycleError::AlreadyInstalled);
        }
        LifecycleAction::Install => {}
        _ if !installed => return Err(LifecycleError::NotInstalled),
        _ => {}
    }
    if let (Some(target), Some(ledger)) = (desired, ledger) {
        if target.app.id != ledger.app_id || target.scope != ledger.scope {
            return Err(LifecycleError::Execution(
                ExecutionPlanError::LedgerMismatch,
            ));
        }
        match action {
            LifecycleAction::Downgrade if target.app.version >= ledger.current.version => {
                return Err(LifecycleError::NotDowngrade {
                    installed: ledger.current.version.clone(),
                    requested: target.app.version.clone(),
                });
            }
            LifecycleAction::Rollback { revision } => {
                let retained = ledger
                    .retained
                    .iter()
                    .find(|snapshot| snapshot.revision == revision)
                    .ok_or(LifecycleError::InvalidRollback)?;
                if target.requirements != retained.requirements
                    || target.generation.as_ref().map(|generation| generation.id)
                        != retained
                            .materialization
                            .as_ref()
                            .map(|generation| generation.id())
                    || target.maintenance.as_ref().map(|generation| generation.id)
                        != retained
                            .maintenance
                            .as_ref()
                            .map(|generation| generation.id())
                {
                    return Err(LifecycleError::InvalidRollback);
                }
            }
            LifecycleAction::Upgrade if target.app.version < ledger.current.version => {
                return Err(LifecycleError::Downgrade {
                    installed: ledger.current.version.clone(),
                    requested: target.app.version.clone(),
                });
            }
            LifecycleAction::Modify | LifecycleAction::Repair { .. }
                if target.app.version != ledger.current.version =>
            {
                return Err(LifecycleError::VersionMismatch {
                    action: if matches!(action, LifecycleAction::Modify) {
                        "modify"
                    } else {
                        "repair"
                    },
                    installed: ledger.current.version.clone(),
                    requested: target.app.version.clone(),
                });
            }
            _ => {}
        }
    }
    if matches!(action, LifecycleAction::Repair { .. }) {
        let target = desired.unwrap();
        let record = ledger.unwrap();
        if target.selected_components.iter().collect::<BTreeSet<_>>()
            != record
                .current
                .selected_components
                .iter()
                .collect::<BTreeSet<_>>()
            || target.install_directory != record.directory
        {
            return Err(LifecycleError::RepairSelectionChanged);
        }
    }
    let mut execution = match desired {
        Some(target) => plan_execution(
            target,
            snapshot.ok_or(LifecycleError::MissingTarget)?,
            ledger,
        )?,
        None => ExecutionPlan {
            binding: ledger.map(crate::DeploymentBinding::recorded),
            selected_components: ledger
                .map(|ledger| ledger.current.selected_components.clone())
                .unwrap_or_default(),
            install_directory: ledger.map(|ledger| ledger.directory.clone()),
            ..ExecutionPlan::default()
        },
    };
    if let LifecycleAction::Repair { force } = action {
        if desired.unwrap().requirements != ledger.unwrap().current.requirements {
            return Err(LifecycleError::RepairRequirementsChanged);
        }
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
        for (key, owned) in &ledger.current.resources {
            if desired_keys.contains(key) || matches!(owned, OwnedResource::Backend { .. }) {
                continue;
            }
            if let Some(target) = desired {
                let alias = desired_keys.iter().any(|desired| {
                    desired.canonical_for(&target.target) == key.canonical_for(&target.target)
                }) || match owned {
                    OwnedResource::File { destination, .. } => target
                        .fixed_files()
                        .any(|file| destination.equivalent(file.destination())),
                    OwnedResource::Launcher { launcher_path, .. } => target
                        .launchers
                        .iter()
                        .any(|launcher| launcher_path.equivalent(&launcher.launcher_path)),
                    OwnedResource::Service(crate::OwnedService { name, .. })
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
        execution.transition = crate::DeploymentTransition::Remove;
    }
    Ok(execution)
}

fn target_keys(target: &crate::BoundDeployment) -> BTreeSet<ResourceKey> {
    let mut keys = BTreeSet::new();
    keys.extend(target.fixed_files().map(|item| item.key.clone()));
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
    target: &crate::BoundDeployment,
    snapshot: &HostSnapshot,
    ledger: &InstallationRecord,
    force: bool,
) -> Result<(), LifecycleError> {
    let rebinding = crate::rebinding::GenerationRebinding::new(target, ledger);
    let may_rebind = |key: &ResourceKey| rebinding.as_ref().is_some_and(|proof| proof.permits(key));
    let observations: BTreeMap<_, _> = snapshot
        .files
        .iter()
        .map(|observed| (&observed.key, observed))
        .collect();
    for op in execution.files.iter_mut() {
        let observed = observations[&op.key];
        let owned = ledger.current.resources.get(&op.key);
        if matches!(owned, Some(OwnedResource::File { replacement, .. }) if *replacement != op.replacement)
        {
            return Err(LifecycleError::RepairDesiredChanged {
                key: op.key.clone(),
            });
        }
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
                    instance,
                    ..
                }),
                FileOperationKind::Drift,
                ObservedFileState::File {
                    object,
                    size: found_size,
                    sha256: found_hash,
                    ..
                },
            ) if force
                && instance.object == *object
                && destination.equivalent(&op.destination)
                && *sha256 == op.expected_sha256
                && *size == op.expected_size
                && *executable == op.executable =>
            {
                op.kind = FileOperationKind::RepairOwned;
                op.precondition = FilePrecondition::Exact {
                    object: *object,
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
            && matches!(ledger.current.resources.get(&op.key), Some(OwnedResource::Launcher { installed: LauncherState::Launcher { target, arguments, working_directory }, .. }) if target == &op.target && arguments == &op.arguments && working_directory == &op.working_directory)
        {
            op.kind = IntegrationChangeKind::RepairOwned;
            op.conflict = None;
        }
        if op.kind == IntegrationChangeKind::Drift
            && matches!(observed.state, ObservedLauncherState::Absent)
            && matches!(ledger.current.resources.get(&op.key), Some(OwnedResource::Launcher { installed: LauncherState::Launcher { target, arguments, working_directory }, .. }) if target == &op.target && arguments == &op.arguments && working_directory == &op.working_directory)
        {
            op.kind = IntegrationChangeKind::RestoreOwned;
            op.conflict = None;
        } else if op.kind == IntegrationChangeKind::UpdateOwned && !may_rebind(&op.key)
            || (op.kind == IntegrationChangeKind::Create
                && !ledger.current.resources.contains_key(&op.key))
        {
            return Err(LifecycleError::RepairDesiredChanged {
                key: op.key.clone(),
            });
        }
    }
    for op in &mut execution.path_entries {
        if op.kind == PathOperationKind::Drift
            && matches!(&op.precondition, crate::PathPrecondition::Owned { binding, previous: crate::PathPresence::Missing, desired: crate::PathPresence::Missing } if binding.value.equivalent(&op.value))
        {
            op.kind = PathOperationKind::RestoreOwned;
            op.conflict = None;
        } else if op.kind == PathOperationKind::Add
            || op.kind == PathOperationKind::RebindOwned && !may_rebind(&op.key)
        {
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
            && matches!(ledger.current.resources.get(&op.key), Some(OwnedResource::Service(crate::OwnedService { installed, .. })) if installed.as_ref() == &op.desired_configuration())
        {
            op.kind = IntegrationChangeKind::RepairOwned;
            op.conflict = None;
        }
        if op.kind == IntegrationChangeKind::Drift
            && matches!(observed.state, ObservedServiceState::Absent)
            && matches!(ledger.current.resources.get(&op.key), Some(OwnedResource::Service(crate::OwnedService { installed, .. })) if installed.as_ref() == &op.desired_configuration())
        {
            op.kind = IntegrationChangeKind::RestoreOwned;
            op.conflict = None;
        } else if op.kind == IntegrationChangeKind::UpdateOwned && !may_rebind(&op.key)
            || (op.kind == IntegrationChangeKind::Create
                && !ledger.current.resources.contains_key(&op.key))
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
            && matches!(ledger.current.resources.get(&op.key), Some(OwnedResource::Protocol { installed: ProtocolState::Registration { command }, .. }) if command == &op.command)
        {
            op.kind = IntegrationChangeKind::RepairOwned;
            op.conflict = None;
        }
        if op.kind == IntegrationChangeKind::Drift
            && matches!(observed.state, ObservedProtocolState::Absent)
            && matches!(ledger.current.resources.get(&op.key), Some(OwnedResource::Protocol { installed: ProtocolState::Registration { command }, .. }) if command == &op.command)
        {
            op.kind = IntegrationChangeKind::RestoreOwned;
            op.conflict = None;
        } else if op.kind == IntegrationChangeKind::UpdateOwned && !may_rebind(&op.key)
            || (op.kind == IntegrationChangeKind::Create
                && !ledger.current.resources.contains_key(&op.key))
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
            && matches!(ledger.current.resources.get(&op.key), Some(OwnedResource::FileAssociation { installed: FileAssociationState::Registration { description, command }, .. }) if description == &op.description && command == &op.command)
        {
            op.association_kind = IntegrationChangeKind::RepairOwned;
        }
        if op.association_kind == IntegrationChangeKind::Drift
            && matches!(
                observed.association_state,
                ObservedFileAssociationState::Absent
            )
            && matches!(ledger.current.resources.get(&op.key), Some(OwnedResource::FileAssociation { installed: FileAssociationState::Registration { description, command }, .. }) if description == &op.description && command == &op.command)
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
            && matches!(ledger.current.resources.get(&extension_key), Some(OwnedResource::Extension { installed: ExtensionState::Mapped { association_id }, .. }) if association_id == &op.id)
        {
            op.extension_kind = IntegrationChangeKind::RepairOwned;
        }
        if op.extension_kind == IntegrationChangeKind::Drift
            && matches!(observed.extension_state, ObservedExtensionState::Absent)
            && matches!(ledger.current.resources.get(&extension_key), Some(OwnedResource::Extension { installed: ExtensionState::Mapped { association_id }, .. }) if association_id == &op.id)
        {
            op.extension_kind = IntegrationChangeKind::RestoreOwned;
        }
        if op.association_kind == IntegrationChangeKind::Create
            || op.association_kind == IntegrationChangeKind::UpdateOwned && !may_rebind(&op.key)
            || matches!(
                op.extension_kind,
                IntegrationChangeKind::Create | IntegrationChangeKind::UpdateOwned
            )
        {
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
