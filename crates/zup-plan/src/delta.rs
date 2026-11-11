//! Pure delta planner: crate::BoundDeployment + HostSnapshot → ExecutionPlan.

use std::collections::BTreeMap;

use miette::Diagnostic;
use thiserror::Error;
use tracing::{info, info_span};
use zup_core::ResourceKey;

use crate::observe::{
    HostSnapshot, ObservedFileAssociationState, ObservedFileState, ObservedLauncherState,
    ObservedProtocolState, ObservedServiceState,
};
use crate::operation::{
    Conflict, ExecutionPlan, FileAssociationOperation, FileOperation, FileOperationKind,
    FilePrecondition, IntegrationChangeKind, LauncherOperation, PathOperation, PathOperationKind,
    ProtocolOperation, ServiceOperation,
};
use crate::{
    ExtensionState, FileAssociationState, InstallationRecord, LauncherState, OwnedResource,
    ProtocolState,
};

/// Errors produced while computing an execution plan (consistency / overflow).
#[derive(Debug, Error, Diagnostic)]
pub enum ExecutionPlanError {
    #[error(transparent)]
    #[diagnostic(code(zup_plan::invalid_contract))]
    Contract(#[from] crate::ContractError),
    #[error("missing snapshot observation for `{key}`")]
    #[diagnostic(code(zup_plan::missing_snapshot_observation))]
    MissingSnapshotObservation { key: String },

    #[error("duplicate snapshot observation for `{key}`")]
    #[diagnostic(code(zup_plan::duplicate_snapshot_observation))]
    DuplicateSnapshotObservation { key: String },

    #[error("snapshot resource mismatch for `{key}`: {reason}")]
    #[diagnostic(code(zup_plan::snapshot_resource_mismatch))]
    SnapshotResourceMismatch { key: String, reason: String },

    #[error("installation ledger identity or schema mismatch")]
    #[diagnostic(code(zup_plan::ledger_mismatch))]
    LedgerMismatch,
}

/// Compare desired and observed state. Pure - zero I/O.
pub fn plan_execution(
    target: &crate::BoundDeployment,
    snapshot: &HostSnapshot,
    ledger: Option<&InstallationRecord>,
) -> Result<ExecutionPlan, ExecutionPlanError> {
    let _span = info_span!("plan_execution").entered();

    target.validate_resolved()?;
    validate_snapshot(target, snapshot)?;
    if ledger.is_some_and(|ledger| {
        ledger.validate().is_err()
            || ledger.id != target.installation
            || ledger.app_id != target.app.id
            || ledger.scope != target.scope
            || ledger.target != target.target
    }) {
        return Err(ExecutionPlanError::LedgerMismatch);
    }

    let files = plan_files(target, snapshot, ledger);
    let launchers = plan_launchers(target, snapshot, ledger);
    let path_entries = plan_paths(target, snapshot, ledger);
    let services = plan_services(target, snapshot, ledger);
    let protocols = plan_protocols(target, snapshot, ledger);
    let file_associations = plan_file_associations(target, snapshot, ledger);

    info!(
        files = files.len(),
        launchers = launchers.len(),
        services = services.len(),
        "execution plan complete"
    );

    Ok(ExecutionPlan {
        binding: Some(crate::DeploymentBinding::desired(target, ledger)),
        selected_components: target.selected_components.clone(),
        install_directory: Some(target.install_directory.clone()),
        transition: crate::DeploymentTransition::Deploy,
        removals: Vec::new(),
        files,
        launchers,
        path_entries,
        services,
        protocols,
        file_associations,
    })
}

fn key_debug(key: &ResourceKey) -> String {
    format!("{key:?}")
}

fn validate_snapshot(
    target: &crate::BoundDeployment,
    snapshot: &HostSnapshot,
) -> Result<(), ExecutionPlanError> {
    // Files: key + destination identity.
    let mut seen = BTreeMap::new();
    for observed in &snapshot.files {
        if let ObservedFileState::File { object, .. } = observed.state
            && object.operating_system() != target.target.operating_system()
        {
            return Err(ExecutionPlanError::SnapshotResourceMismatch {
                key: key_debug(&observed.key),
                reason: "native file identity belongs to a different operating system".into(),
            });
        }
        if seen.insert(&observed.key, observed).is_some() {
            return Err(ExecutionPlanError::DuplicateSnapshotObservation {
                key: key_debug(&observed.key),
            });
        }
    }
    for desired in target.fixed_files() {
        let Some(observed) = seen.get(&desired.key) else {
            return Err(ExecutionPlanError::MissingSnapshotObservation {
                key: key_debug(&desired.key),
            });
        };
        if &observed.path != desired.destination() {
            return Err(ExecutionPlanError::SnapshotResourceMismatch {
                key: key_debug(&desired.key),
                reason: format!(
                    "destination mismatch: `{}` vs `{}`",
                    observed.path,
                    desired.destination()
                ),
            });
        }
    }
    for key in seen.keys() {
        if !target.fixed_files().any(|f| &f.key == *key) {
            return Err(ExecutionPlanError::SnapshotResourceMismatch {
                key: key_debug(key),
                reason: "unexpected file observation".to_owned(),
            });
        }
    }

    // Launchers
    let mut seen = BTreeMap::new();
    for observed in &snapshot.launchers {
        if seen.insert(&observed.key, observed).is_some() {
            return Err(ExecutionPlanError::DuplicateSnapshotObservation {
                key: key_debug(&observed.key),
            });
        }
    }
    for desired in &target.launchers {
        let Some(observed) = seen.get(&desired.key) else {
            return Err(ExecutionPlanError::MissingSnapshotObservation {
                key: key_debug(&desired.key),
            });
        };
        if observed.launcher_path != desired.launcher_path {
            return Err(ExecutionPlanError::SnapshotResourceMismatch {
                key: key_debug(&desired.key),
                reason: format!(
                    "link path mismatch: `{}` vs `{}`",
                    observed.launcher_path, desired.launcher_path
                ),
            });
        }
    }
    for key in seen.keys() {
        if !target.launchers.iter().any(|s| &s.key == *key) {
            return Err(ExecutionPlanError::SnapshotResourceMismatch {
                key: key_debug(key),
                reason: "unexpected launcher observation".to_owned(),
            });
        }
    }

    check_pairs(
        &target
            .path_entries
            .iter()
            .map(|x| &x.key)
            .collect::<Vec<_>>(),
        &snapshot
            .path_entries
            .iter()
            .map(|x| &x.key)
            .collect::<Vec<_>>(),
        "path entry",
    )?;
    check_pairs(
        &target.services.iter().map(|x| &x.key).collect::<Vec<_>>(),
        &snapshot.services.iter().map(|x| &x.key).collect::<Vec<_>>(),
        "service",
    )?;
    check_pairs(
        &target.protocols.iter().map(|x| &x.key).collect::<Vec<_>>(),
        &snapshot
            .protocols
            .iter()
            .map(|x| &x.key)
            .collect::<Vec<_>>(),
        "protocol",
    )?;
    check_pairs(
        &target
            .file_associations
            .iter()
            .map(|x| &x.key)
            .collect::<Vec<_>>(),
        &snapshot
            .file_associations
            .iter()
            .map(|x| &x.key)
            .collect::<Vec<_>>(),
        "file association",
    )?;

    let mismatch = |key: &ResourceKey, reason: &str| ExecutionPlanError::SnapshotResourceMismatch {
        key: key_debug(key),
        reason: reason.into(),
    };
    for resource in &target.path_entries {
        let observed = snapshot
            .path_entries
            .iter()
            .find(|observed| observed.key == resource.key)
            .expect("presence validated");
        if observed.desired != resource.value || observed.scope != resource.scope {
            return Err(mismatch(&resource.key, "path identity or scope mismatch"));
        }
    }
    for resource in &target.services {
        let observed = snapshot
            .services
            .iter()
            .find(|observed| observed.key == resource.key)
            .expect("presence validated");
        if observed.id != resource.id {
            return Err(mismatch(&resource.key, "service identity mismatch"));
        }
    }
    for resource in &target.protocols {
        let observed = snapshot
            .protocols
            .iter()
            .find(|observed| observed.key == resource.key)
            .expect("presence validated");
        if observed.scheme != resource.scheme || observed.scope != resource.scope {
            return Err(mismatch(
                &resource.key,
                "protocol identity or scope mismatch",
            ));
        }
    }
    for resource in &target.file_associations {
        let observed = snapshot
            .file_associations
            .iter()
            .find(|observed| observed.key == resource.key)
            .expect("presence validated");
        if observed.id != resource.id.as_str()
            || observed.extension != resource.extension.as_str()
            || observed.scope != resource.scope
        {
            return Err(mismatch(
                &resource.key,
                "association identity, extension or scope mismatch",
            ));
        }
    }

    Ok(())
}

fn check_pairs(
    desired: &[&ResourceKey],
    observed: &[&ResourceKey],
    what: &str,
) -> Result<(), ExecutionPlanError> {
    let mut seen = BTreeMap::new();
    for key in observed {
        if seen.insert(*key, ()).is_some() {
            return Err(ExecutionPlanError::DuplicateSnapshotObservation {
                key: key_debug(key),
            });
        }
    }
    for key in desired {
        if !seen.contains_key(*key) {
            return Err(ExecutionPlanError::MissingSnapshotObservation {
                key: key_debug(key),
            });
        }
    }
    for key in observed {
        if !desired.contains(key) {
            return Err(ExecutionPlanError::SnapshotResourceMismatch {
                key: key_debug(key),
                reason: format!("unexpected {what} observation"),
            });
        }
    }
    Ok(())
}

fn plan_files(
    target: &crate::BoundDeployment,
    snapshot: &HostSnapshot,
    ledger: Option<&InstallationRecord>,
) -> Vec<FileOperation> {
    let mut out = Vec::with_capacity(target.files.len());
    let observations: BTreeMap<_, _> = snapshot
        .files
        .iter()
        .map(|observed| (&observed.key, observed))
        .collect();
    for desired in target.fixed_files() {
        let observed = observations[&desired.key];
        let owned = ledger.and_then(|ledger| ledger.current.resources.get(&desired.key));
        let installed = match owned {
            Some(OwnedResource::File {
                instance,
                destination,
                sha256,
                size,
                ..
            }) if destination.equivalent(desired.destination()) => {
                Some((instance.object, *size, *sha256))
            }
            _ => None,
        };
        let declaration_matches = !matches!(owned, Some(OwnedResource::File { executable, .. }) if *executable != desired.executable);

        let (kind, precondition, conflict) = if owned.is_some() && installed.is_none()
            || installed.is_some_and(|identity| !matches!(&observed.state, ObservedFileState::File { object, size, sha256, .. } if (*object, *size, *sha256) == identity))
        {
            (FileOperationKind::Drift, FilePrecondition::Absent, Some(Conflict::File { path: observed.path.to_string() }))
        } else { match &observed.state {
            ObservedFileState::Absent => {
                (FileOperationKind::Create, FilePrecondition::Absent, None)
            }
            ObservedFileState::File { object, size, sha256, executable } => {
                let precondition = FilePrecondition::Exact {
                    object: *object,
                    size: *size,
                    sha256: *sha256,
                };
                if installed.is_some() && *size == desired.size && *sha256 == desired.sha256 && executable.is_none_or(|current| current == desired.executable) && declaration_matches {
                    (FileOperationKind::NoOp, precondition, None)
                } else if installed.is_some() {
                    (FileOperationKind::Replace, precondition, None)
                } else {
                    (FileOperationKind::Conflict, precondition, Some(Conflict::File { path: observed.path.to_string() }))
                }
            }
            ObservedFileState::NonFile => {
                (
                    FileOperationKind::Conflict,
                    FilePrecondition::Absent,
                    Some(Conflict::TargetNonFile {
                        path: observed.path.to_string(),
                    }),
                )
            }
        }};

        out.push(FileOperation {
            key: desired.key.clone(),
            replacement: desired
                .placement
                .fixed_replacement()
                .expect("fixed file selection"),
            kind,
            destination: desired.destination().clone(),
            source_relative: desired.source_relative.clone(),
            precondition,
            expected_sha256: desired.sha256,
            expected_size: desired.size,
            privilege: desired.privilege,
            executable: desired.executable,
            conflict,
        });
    }
    out
}

fn plan_launchers(
    target: &crate::BoundDeployment,
    snapshot: &HostSnapshot,
    ledger: Option<&InstallationRecord>,
) -> Vec<LauncherOperation> {
    let icon = target
        .maintenance
        .as_ref()
        .and_then(|generation| generation.maintenance_runtime());
    let mut out = Vec::with_capacity(target.launchers.len());
    let observations: BTreeMap<_, _> = snapshot
        .launchers
        .iter()
        .map(|observed| (&observed.key, observed))
        .collect();
    for desired in target.launchers.iter() {
        let observed = observations[&desired.key];
        let owned = ledger.and_then(|l| l.current.resources.get(&desired.key));
        let installed = match owned {
            Some(OwnedResource::Launcher {
                launcher_path,
                installed,
                ..
            }) if launcher_path == &desired.launcher_path => Some(installed),
            _ => None,
        };
        let current = match &observed.state {
            ObservedLauncherState::Absent => Some(LauncherState::Absent),
            ObservedLauncherState::Launcher {
                target,
                arguments,
                working_directory,
            } => Some(LauncherState::Launcher {
                target: target.clone(),
                arguments: arguments.clone(),
                working_directory: working_directory.clone(),
            }),
            _ => None,
        };
        let (kind, conflict) = if owned.is_some() && installed.is_none()
            || installed.is_some_and(|installed| current.as_ref() != Some(installed))
        {
            (
                IntegrationChangeKind::Drift,
                Some(Conflict::LauncherAlreadyOwnedByDifferentTarget {
                    launcher_path: observed.launcher_path.to_string(),
                    reason: "owned launcher changed after installation".to_owned(),
                }),
            )
        } else {
            match &observed.state {
                ObservedLauncherState::Absent => (IntegrationChangeKind::Create, None),
                ObservedLauncherState::Launcher {
                    target,
                    arguments,
                    working_directory,
                } => {
                    let matches = *target == desired.target
                        && *arguments == desired.arguments
                        && *working_directory == desired.working_directory;
                    if matches {
                        (IntegrationChangeKind::NoOp, None)
                    } else {
                        if installed.is_some() {
                            (IntegrationChangeKind::UpdateOwned, None)
                        } else {
                            (
                                IntegrationChangeKind::Conflict,
                                Some(Conflict::LauncherAlreadyOwnedByDifferentTarget {
                                    launcher_path: observed.launcher_path.to_string(),
                                    reason: "existing launcher content differs from desired"
                                        .to_owned(),
                                }),
                            )
                        }
                    }
                }
                ObservedLauncherState::InvalidLauncher => (
                    IntegrationChangeKind::Conflict,
                    Some(Conflict::LauncherAlreadyOwnedByDifferentTarget {
                        launcher_path: observed.launcher_path.to_string(),
                        reason: "existing file is not a valid shell link".to_owned(),
                    }),
                ),
                ObservedLauncherState::NonFile => (
                    IntegrationChangeKind::Conflict,
                    Some(Conflict::TargetNonFile {
                        path: observed.launcher_path.to_string(),
                    }),
                ),
            }
        };

        out.push(LauncherOperation {
            key: desired.key.clone(),
            kind,
            launcher_path: desired.launcher_path.clone(),
            target: desired.target.clone(),
            arguments: desired.arguments.clone(),
            working_directory: desired.working_directory.clone(),
            privilege: desired.privilege,
            previous: observed.state.clone(),
            conflict,
            icon: icon.clone(),
        });
    }
    out
}

fn plan_paths(
    target: &crate::BoundDeployment,
    snapshot: &HostSnapshot,
    ledger: Option<&InstallationRecord>,
) -> Vec<PathOperation> {
    let mut out = Vec::with_capacity(target.path_entries.len());
    let observations: BTreeMap<_, _> = snapshot
        .path_entries
        .iter()
        .map(|observed| (&observed.key, observed))
        .collect();
    for desired in target.path_entries.iter() {
        let observed = observations[&desired.key];
        let presence = |path: &crate::TargetPath| match observed
            .search_path
            .entries()
            .iter()
            .filter(|entry| entry.equivalent(path))
            .count()
        {
            0 => crate::PathPresence::Missing,
            1 => crate::PathPresence::Single,
            _ => crate::PathPresence::Multiple,
        };
        let current = presence(&desired.value);
        let precondition =
            match ledger.and_then(|ledger| ledger.current.resources.get(&desired.key)) {
                Some(OwnedResource::PathEntry {
                    value, value_type, ..
                }) => crate::PathPrecondition::Owned {
                    binding: crate::PathBinding {
                        value: value.clone(),
                        value_type: value_type.clone(),
                    },
                    previous: presence(value),
                    desired: current,
                },
                _ => crate::PathPrecondition::Unclaimed { presence: current },
            };
        let kind = match &precondition {
            crate::PathPrecondition::Unclaimed {
                presence: crate::PathPresence::Missing,
            } => PathOperationKind::Add,
            crate::PathPrecondition::Unclaimed { .. } => PathOperationKind::Present,
            crate::PathPrecondition::Owned {
                binding,
                previous: crate::PathPresence::Single,
                desired: crate::PathPresence::Single,
            } if binding.value.equivalent(&desired.value) => PathOperationKind::Present,
            crate::PathPrecondition::Owned {
                binding,
                previous: crate::PathPresence::Single,
                desired: crate::PathPresence::Missing,
            } if !binding.value.equivalent(&desired.value) => PathOperationKind::RebindOwned,
            crate::PathPrecondition::Owned { .. } => PathOperationKind::Drift,
        };
        let conflict = (kind == PathOperationKind::Drift).then(|| Conflict::PathEntryConflict {
            value: desired.value.to_string(), reason: "owned search-path binding is missing, duplicated, or conflicts with its requested replacement".into(),
        });
        out.push(PathOperation {
            key: desired.key.clone(),
            kind,
            value: desired.value.clone(),
            scope: desired.scope,
            privilege: desired.privilege,
            precondition,
            conflict,
        });
    }
    out
}

fn plan_services(
    target: &crate::BoundDeployment,
    snapshot: &HostSnapshot,
    ledger: Option<&InstallationRecord>,
) -> Vec<ServiceOperation> {
    let mut out = Vec::with_capacity(target.services.len());
    let observations: BTreeMap<_, _> = snapshot
        .services
        .iter()
        .map(|observed| (&observed.key, observed))
        .collect();
    for desired in target.services.iter() {
        let registration = zup_core::NativeServiceRegistration::for_service(
            &desired.execution,
            target.installation,
        );
        let observed = observations[&desired.key];
        let owned = ledger.and_then(|l| l.current.resources.get(&desired.key));
        let registration_matches = match &desired.key {
            ResourceKey::Service { id } => observed.registration.matches(registration, id),
            _ => false,
        };
        let installed = match owned {
            Some(OwnedResource::Service(crate::OwnedService {
                name, installed, ..
            })) if name == desired.name.as_str() => Some(installed.as_ref()),
            _ => None,
        };
        let current = observed.state.configuration();
        let (kind, conflict) = if owned.is_some() && installed.is_none()
            || owned.is_some_and(|owned| !matches!(owned, OwnedResource::Service(crate::OwnedService { registration: previous, .. }) if *previous == registration))
            || owned.is_some() && observed.state != ObservedServiceState::Absent && !registration_matches
            || installed.is_some_and(|installed| Some(installed) != current.as_ref())
        {
            (
                IntegrationChangeKind::Drift,
                Some(Conflict::ServiceAlreadyExistsWithDifferentConfiguration {
                    service: desired.name.to_string(),
                    reason: "owned service changed after installation".to_owned(),
                }),
            )
        } else {
            match &observed.state {
                ObservedServiceState::Absent => (IntegrationChangeKind::Create, None),
                ObservedServiceState::InvalidService => (
                    IntegrationChangeKind::Conflict,
                    Some(Conflict::ServiceAlreadyExistsWithDifferentConfiguration {
                        service: desired.name.to_string(),
                        reason: "existing service is malformed".into(),
                    }),
                ),
                ObservedServiceState::Service {
                    native_timeouts,
                    failure,
                    dependencies,
                    execution,
                    display_name,
                    command,
                    start,
                    readiness,
                    ..
                } => {
                    let desired_display = desired
                        .display_name
                        .as_ref()
                        .map(|n| n.to_string())
                        .unwrap_or_else(|| desired.name.to_string());
                    let matches = *failure == desired.failure && *dependencies == desired.dependency_names && *display_name == desired_display
                        && *native_timeouts == desired.lifecycle.timeouts_for(&desired.execution).native()
                        && *execution == desired.execution
                        && commands_match(command, &desired.command)
                        && *start == desired.start
                        && *readiness == desired.readiness
                        && owned.is_none_or(|owned| matches!(owned, OwnedResource::Service(crate::OwnedService { requires_files, requires_services, timeouts, .. }) if requires_files == &desired.requires_files && requires_services == &desired.requires_services && *timeouts == desired.lifecycle.timeouts_for(&desired.execution)));
                    if matches && installed.is_some() {
                        (IntegrationChangeKind::NoOp, None)
                    } else {
                        if installed.is_some() {
                            (IntegrationChangeKind::UpdateOwned, None)
                        } else {
                            (
                                IntegrationChangeKind::Conflict,
                                Some(Conflict::ServiceAlreadyExistsWithDifferentConfiguration {
                                    service: desired.name.to_string(),
                                    reason: "existing service is not owned by this installation"
                                        .to_owned(),
                                }),
                            )
                        }
                    }
                }
            }
        };

        out.push(ServiceOperation {
            state_directories: desired.state_directories.clone(),
            registration,
            failure: desired.failure,
            execution: desired.execution.clone(),
            lifecycle: desired.lifecycle.resolved(&desired.execution),
            readiness: desired.readiness,
            requires_services: desired.requires_services.clone(),
            dependency_names: desired.dependency_names.clone(),
            requires_files: desired.requires_files.clone(),
            key: desired.key.clone(),
            kind,
            id: desired.id.to_string(),
            name: desired.name.to_string(),
            display_name: desired
                .display_name
                .as_ref()
                .map(|n| n.to_string())
                .unwrap_or_else(|| desired.name.to_string()),
            command: desired.command.clone(),
            start: desired.start,
            privilege: desired.privilege,
            previous: observed.state.clone(),
            conflict,
        });
    }
    out
}

fn plan_protocols(
    target: &crate::BoundDeployment,
    snapshot: &HostSnapshot,
    ledger: Option<&InstallationRecord>,
) -> Vec<ProtocolOperation> {
    let mut out = Vec::with_capacity(target.protocols.len());
    let observations: BTreeMap<_, _> = snapshot
        .protocols
        .iter()
        .map(|observed| (&observed.key, observed))
        .collect();
    for desired in target.protocols.iter() {
        let observed = observations[&desired.key];
        let owned = ledger.and_then(|l| l.current.resources.get(&desired.key));
        let observed_owned = matches!(owned, Some(OwnedResource::Protocol { installed, .. }) if protocol_observed_matches(installed, &observed.state));
        let has_ownership = matches!(owned, Some(OwnedResource::Protocol { .. }));
        let (kind, conflict) = match &observed.state {
            ObservedProtocolState::Absent => {
                if has_ownership {
                    (
                        IntegrationChangeKind::Drift,
                        Some(Conflict::ProtocolAlreadyRegistered {
                            scheme: desired.scheme.to_string(),
                            reason: "zup-owned protocol disappeared".into(),
                        }),
                    )
                } else {
                    (IntegrationChangeKind::Create, None)
                }
            }
            ObservedProtocolState::Registration {
                command,
                url_protocol_marker,
            } => {
                let matches = *url_protocol_marker && commands_match(command, &desired.command);
                if matches && (!has_ownership || observed_owned) {
                    (IntegrationChangeKind::NoOp, None)
                } else if observed_owned {
                    (IntegrationChangeKind::UpdateOwned, None)
                } else if has_ownership {
                    (
                        IntegrationChangeKind::Drift,
                        Some(Conflict::ProtocolAlreadyRegistered {
                            scheme: desired.scheme.to_string(),
                            reason: "zup-owned protocol drifted after installation".into(),
                        }),
                    )
                } else {
                    (
                        IntegrationChangeKind::Conflict,
                        Some(Conflict::ProtocolAlreadyRegistered {
                            scheme: desired.scheme.to_string(),
                            reason: "existing protocol handler differs or is incomplete".to_owned(),
                        }),
                    )
                }
            }
            ObservedProtocolState::Malformed { reason } => (
                if has_ownership {
                    IntegrationChangeKind::Drift
                } else {
                    IntegrationChangeKind::Conflict
                },
                Some(Conflict::ProtocolAlreadyRegistered {
                    scheme: desired.scheme.to_string(),
                    reason: reason.clone(),
                }),
            ),
        };

        out.push(ProtocolOperation {
            key: desired.key.clone(),
            kind,
            scheme: desired.scheme.clone(),
            command: desired.command.clone(),
            scope: desired.scope,
            privilege: desired.privilege,
            previous: observed.state.clone(),
            conflict,
        });
    }
    out
}

fn plan_file_associations(
    target: &crate::BoundDeployment,
    snapshot: &HostSnapshot,
    ledger: Option<&InstallationRecord>,
) -> Vec<FileAssociationOperation> {
    let mut out = Vec::with_capacity(target.file_associations.len());
    let observations: BTreeMap<_, _> = snapshot
        .file_associations
        .iter()
        .map(|observed| (&observed.key, observed))
        .collect();
    for desired in target.file_associations.iter() {
        let observed = observations[&desired.key];
        let ext_key = ResourceKey::FileAssociationExtension {
            extension: desired.extension.clone(),
        };
        let owned_id = ledger.and_then(|l| l.current.resources.get(&desired.key));
        let owned_ext = ledger.and_then(|l| l.current.resources.get(&ext_key));
        let id_owned_matches = matches!(owned_id, Some(OwnedResource::FileAssociation { installed, .. }) if file_association_observed_matches(installed, &observed.association_state));
        let ext_owned_matches = matches!(owned_ext, Some(OwnedResource::Extension { installed, .. }) if extension_observed_matches(installed, &observed.extension_state));
        let desired_desc = desired.description.clone();
        let id_ok = match &observed.association_state {
            ObservedFileAssociationState::Absent => false,
            ObservedFileAssociationState::Registration {
                description,
                command,
            } => *description == desired_desc && commands_match(command, &desired.command),
            ObservedFileAssociationState::Malformed { .. } => false,
        };
        let ext_ok = match &observed.extension_state {
            crate::observe::ObservedExtensionState::Absent => false,
            crate::observe::ObservedExtensionState::Mapped { association_id } => {
                association_id.eq_ignore_ascii_case(desired.id.as_str())
            }
            crate::observe::ObservedExtensionState::Malformed { .. } => false,
        };

        let association_kind = if owned_id.is_some() && !id_owned_matches {
            IntegrationChangeKind::Drift
        } else if id_ok {
            IntegrationChangeKind::NoOp
        } else if matches!(
            observed.association_state,
            ObservedFileAssociationState::Absent
        ) && owned_id.is_none()
        {
            IntegrationChangeKind::Create
        } else if id_owned_matches {
            IntegrationChangeKind::UpdateOwned
        } else if owned_id.is_some() {
            IntegrationChangeKind::Drift
        } else {
            IntegrationChangeKind::Conflict
        };
        let extension_kind = if owned_ext.is_some() && !ext_owned_matches {
            IntegrationChangeKind::Drift
        } else if ext_ok {
            IntegrationChangeKind::NoOp
        } else if matches!(
            observed.extension_state,
            crate::observe::ObservedExtensionState::Absent
        ) && owned_ext.is_none()
        {
            IntegrationChangeKind::Create
        } else if ext_owned_matches {
            IntegrationChangeKind::UpdateOwned
        } else if owned_ext.is_some() {
            IntegrationChangeKind::Drift
        } else {
            IntegrationChangeKind::Conflict
        };

        let (kind, conflict) = if [association_kind, extension_kind]
            .contains(&IntegrationChangeKind::Drift)
        {
            (
                IntegrationChangeKind::Drift,
                Some(Conflict::FileAssociationConflict {
                    id: desired.id.to_string(),
                    reason: "zup-owned association drifted".into(),
                }),
            )
        } else if [association_kind, extension_kind].contains(&IntegrationChangeKind::Conflict) {
            (
                IntegrationChangeKind::Conflict,
                Some(Conflict::FileAssociationConflict {
                    id: desired.id.to_string(),
                    reason: "foreign association differs".into(),
                }),
            )
        } else if association_kind == IntegrationChangeKind::NoOp
            && extension_kind == IntegrationChangeKind::NoOp
        {
            (IntegrationChangeKind::NoOp, None)
        } else {
            (
                if [association_kind, extension_kind].contains(&IntegrationChangeKind::UpdateOwned)
                {
                    IntegrationChangeKind::UpdateOwned
                } else {
                    IntegrationChangeKind::Create
                },
                None,
            )
        };

        out.push(FileAssociationOperation {
            key: desired.key.clone(),
            kind,
            association_kind,
            extension_kind,
            extension: desired.extension.to_string(),
            id: desired.id.to_string(),
            description: desired.description.clone(),
            command: desired.command.clone(),
            scope: desired.scope,
            privilege: desired.privilege,
            previous_association: observed.association_state.clone(),
            previous_extension: observed.extension_state.clone(),
            conflict,
        });
    }
    out
}

fn protocol_observed_matches(expected: &ProtocolState, observed: &ObservedProtocolState) -> bool {
    match (expected, observed) {
        (ProtocolState::Absent, ObservedProtocolState::Absent) => true,
        (
            ProtocolState::Registration { command: a },
            ObservedProtocolState::Registration {
                command: b,
                url_protocol_marker: true,
            },
        ) => commands_match(a, b),
        _ => false,
    }
}

fn file_association_observed_matches(
    expected: &FileAssociationState,
    observed: &ObservedFileAssociationState,
) -> bool {
    match (expected, observed) {
        (FileAssociationState::Absent, ObservedFileAssociationState::Absent) => true,
        (
            FileAssociationState::Registration {
                description: a,
                command: ac,
            },
            ObservedFileAssociationState::Registration {
                description: b,
                command: bc,
            },
        ) => a == b && commands_match(ac, bc),
        _ => false,
    }
}

fn extension_observed_matches(
    expected: &ExtensionState,
    observed: &crate::observe::ObservedExtensionState,
) -> bool {
    match (expected, observed) {
        (ExtensionState::Absent, crate::observe::ObservedExtensionState::Absent) => true,
        (
            ExtensionState::Mapped { association_id: a },
            crate::observe::ObservedExtensionState::Mapped { association_id: b },
        ) => a.eq_ignore_ascii_case(b),
        _ => false,
    }
}

/// Semantic command equality (target-aware path semantics; args exact).
fn commands_match(a: &zup_core::CommandSpec, b: &zup_core::CommandSpec) -> bool {
    a.executable.equivalent(&b.executable) && a.arguments == b.arguments
}
