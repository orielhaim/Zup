//! Pure delta planner: TargetPlan + HostSnapshot → ExecutionPlan.

use std::collections::BTreeMap;

use miette::Diagnostic;
use thiserror::Error;
use tracing::{info, info_span};
use zup_core::ResourceKey;
use zup_platform::TargetPlan;

use crate::observe::{
    HostSnapshot, ObservedFileAssociationState, ObservedFileState, ObservedLauncherState,
    ObservedProtocolState, ObservedServiceState,
};
use crate::operation::{
    Conflict, ExecutionPlan, ExecutionSummary, FileAssociationOperation,
    FileAssociationOperationKind, FileOperation, FileOperationKind, FilePrecondition,
    LauncherOperation, LauncherOperationKind, PathOperation, PathOperationKind, ProtocolOperation,
    ProtocolOperationKind, ServiceOperation, ServiceOperationKind,
};
use crate::{
    ExtensionState, FileAssociationState, InstallLedger, LauncherState, OwnedResource,
    ProtocolState, ServiceState,
};

/// Errors produced while computing an execution plan (consistency / overflow).
#[derive(Debug, Error, Diagnostic)]
pub enum ExecutionPlanError {
    #[error("missing snapshot observation for `{key}`")]
    #[diagnostic(code(zup_exec::missing_snapshot_observation))]
    MissingSnapshotObservation { key: String },

    #[error("duplicate snapshot observation for `{key}`")]
    #[diagnostic(code(zup_exec::duplicate_snapshot_observation))]
    DuplicateSnapshotObservation { key: String },

    #[error("snapshot resource mismatch for `{key}`: {reason}")]
    #[diagnostic(code(zup_exec::snapshot_resource_mismatch))]
    SnapshotResourceMismatch { key: String, reason: String },

    #[error("execution summary size overflow")]
    #[diagnostic(code(zup_exec::size_overflow))]
    SizeOverflow,

    #[error("installation ledger identity or schema mismatch")]
    #[diagnostic(code(zup_exec::ledger_mismatch))]
    LedgerMismatch,
}

/// Compare desired and observed state. Pure — zero I/O.
pub fn plan_execution(
    target: &TargetPlan,
    snapshot: &HostSnapshot,
    ledger: Option<&InstallLedger>,
) -> Result<ExecutionPlan, ExecutionPlanError> {
    let _span = info_span!("plan_execution").entered();

    validate_snapshot(target, snapshot)?;
    if ledger.is_some_and(|ledger| {
        ledger.schema != crate::INSTALL_LEDGER_SCHEMA
            || ledger.app_id != target.app.id
            || ledger.scope != target.scope
            || ledger.target != target.target
    }) {
        return Err(ExecutionPlanError::LedgerMismatch);
    }

    let mut summary = ExecutionSummary {
        requires_authorization: target.summary.requires_authorization,
        ..Default::default()
    };

    let files = plan_files(target, snapshot, ledger, &mut summary)?;
    let launchers = plan_launchers(target, snapshot, ledger, &mut summary);
    let path_entries = plan_paths(target, snapshot, ledger, &mut summary);
    let services = plan_services(target, snapshot, ledger, &mut summary);
    let protocols = plan_protocols(target, snapshot, ledger, &mut summary);
    let file_associations = plan_file_associations(target, snapshot, ledger, &mut summary);

    info!(
        create = summary.files_create
            + summary.launchers_create
            + summary.path_entries_add
            + summary.services_create
            + summary.protocols_create
            + summary.file_associations_create,
        conflict = summary.files_conflict
            + summary.launchers_conflict
            + summary.path_entries_conflict
            + summary.services_conflict
            + summary.protocols_conflict
            + summary.file_associations_conflict,
        "execution plan complete"
    );

    Ok(ExecutionPlan {
        selected_components: target.selected_components.clone(),
        install_directory: Some(target.install_directory.clone()),
        uninstall: false,
        removals: Vec::new(),
        files,
        launchers,
        path_entries,
        services,
        protocols,
        file_associations,
        summary,
    })
}

fn key_debug(key: &ResourceKey) -> String {
    format!("{key:?}")
}

fn validate_snapshot(
    target: &TargetPlan,
    snapshot: &HostSnapshot,
) -> Result<(), ExecutionPlanError> {
    check_unique(&snapshot.files.iter().map(|o| &o.key).collect::<Vec<_>>())?;
    check_unique(
        &snapshot
            .launchers
            .iter()
            .map(|o| &o.key)
            .collect::<Vec<_>>(),
    )?;
    check_unique(
        &snapshot
            .path_entries
            .iter()
            .map(|o| &o.key)
            .collect::<Vec<_>>(),
    )?;
    check_unique(&snapshot.services.iter().map(|o| &o.key).collect::<Vec<_>>())?;
    check_unique(
        &snapshot
            .protocols
            .iter()
            .map(|o| &o.key)
            .collect::<Vec<_>>(),
    )?;
    check_unique(
        &snapshot
            .file_associations
            .iter()
            .map(|o| &o.key)
            .collect::<Vec<_>>(),
    )?;

    // Files: key + destination identity.
    let mut seen = BTreeMap::new();
    for observed in &snapshot.files {
        if seen.insert(&observed.key, observed).is_some() {
            return Err(ExecutionPlanError::DuplicateSnapshotObservation {
                key: key_debug(&observed.key),
            });
        }
    }
    for desired in &target.files {
        let Some(observed) = seen.get(&desired.key) else {
            return Err(ExecutionPlanError::MissingSnapshotObservation {
                key: key_debug(&desired.key),
            });
        };
        if observed.path != desired.destination {
            return Err(ExecutionPlanError::SnapshotResourceMismatch {
                key: key_debug(&desired.key),
                reason: format!(
                    "destination mismatch: `{}` vs `{}`",
                    observed.path, desired.destination
                ),
            });
        }
    }
    for key in seen.keys() {
        if !target.files.iter().any(|f| &f.key == *key) {
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

    // Search path, services, protocols, associations: key presence only.
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

    Ok(())
}

fn check_unique(keys: &[&ResourceKey]) -> Result<(), ExecutionPlanError> {
    let mut seen = BTreeMap::new();
    for key in keys {
        if seen.insert(*key, ()).is_some() {
            return Err(ExecutionPlanError::DuplicateSnapshotObservation {
                key: key_debug(key),
            });
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
    target: &TargetPlan,
    snapshot: &HostSnapshot,
    ledger: Option<&InstallLedger>,
    summary: &mut ExecutionSummary,
) -> Result<Vec<FileOperation>, ExecutionPlanError> {
    let mut out = Vec::with_capacity(target.files.len());
    for (desired, observed) in target.files.iter().zip(snapshot.files.iter()) {
        let owned = ledger.and_then(|ledger| ledger.resources.get(&desired.key));
        let installed = match owned {
            Some(OwnedResource::File {
                destination,
                sha256,
                size,
                ..
            }) if destination == &desired.destination => Some((*size, *sha256)),
            _ => None,
        };
        summary.total_desired_bytes = summary
            .total_desired_bytes
            .checked_add(desired.size)
            .ok_or(ExecutionPlanError::SizeOverflow)?;

        let (kind, precondition, conflict) = if owned.is_some() && installed.is_none()
            || installed.is_some_and(|identity| !matches!(&observed.state, ObservedFileState::File { size, sha256 } if (*size, *sha256) == identity))
        {
            summary.files_conflict += 1;
            (FileOperationKind::Drift, FilePrecondition::Absent, Some(Conflict::File { path: observed.path.to_string() }))
        } else { match &observed.state {
            ObservedFileState::Absent => {
                summary.files_create += 1;
                summary.write_bytes = summary
                    .write_bytes
                    .checked_add(desired.size)
                    .ok_or(ExecutionPlanError::SizeOverflow)?;
                (FileOperationKind::Create, FilePrecondition::Absent, None)
            }
            ObservedFileState::File { size, sha256 } => {
                let precondition = FilePrecondition::Exact {
                    size: *size,
                    sha256: *sha256,
                };
                if *sha256 == desired.sha256 {
                    summary.files_unchanged += 1;
                    (FileOperationKind::NoOp, precondition, None)
                } else if installed.is_some() {
                    summary.files_replace += 1;
                    summary.write_bytes = summary
                        .write_bytes
                        .checked_add(desired.size)
                        .ok_or(ExecutionPlanError::SizeOverflow)?;
                    (FileOperationKind::Replace, precondition, None)
                } else {
                    summary.files_conflict += 1;
                    (FileOperationKind::Conflict, precondition, Some(Conflict::File { path: observed.path.to_string() }))
                }
            }
            ObservedFileState::NonFile => {
                summary.files_conflict += 1;
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
            kind,
            destination: desired.destination.clone(),
            source_relative: desired.source_relative.clone(),
            precondition,
            expected_sha256: desired.sha256,
            expected_size: desired.size,
            privilege: desired.privilege,
            conflict,
        });
    }
    Ok(out)
}

fn plan_launchers(
    target: &TargetPlan,
    snapshot: &HostSnapshot,
    ledger: Option<&InstallLedger>,
    summary: &mut ExecutionSummary,
) -> Vec<LauncherOperation> {
    let mut out = Vec::with_capacity(target.launchers.len());
    for (desired, observed) in target.launchers.iter().zip(snapshot.launchers.iter()) {
        let owned = ledger.and_then(|l| l.resources.get(&desired.key));
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
            summary.launchers_conflict += 1;
            (
                LauncherOperationKind::Drift,
                Some(Conflict::LauncherAlreadyOwnedByDifferentTarget {
                    launcher_path: observed.launcher_path.to_string(),
                    reason: "owned launcher changed after installation".to_owned(),
                }),
            )
        } else {
            match &observed.state {
                ObservedLauncherState::Absent => {
                    summary.launchers_create += 1;
                    (LauncherOperationKind::Create, None)
                }
                ObservedLauncherState::Launcher {
                    target,
                    arguments,
                    working_directory,
                } => {
                    let matches = *target == desired.target
                        && *arguments == desired.arguments
                        && *working_directory == desired.working_directory;
                    if matches {
                        summary.launchers_unchanged += 1;
                        (LauncherOperationKind::NoOp, None)
                    } else {
                        if installed.is_some() {
                            summary.launchers_create += 1;
                            (LauncherOperationKind::UpdateOwned, None)
                        } else {
                            summary.launchers_conflict += 1;
                            (
                                LauncherOperationKind::Conflict,
                                Some(Conflict::LauncherAlreadyOwnedByDifferentTarget {
                                    launcher_path: observed.launcher_path.to_string(),
                                    reason: "existing launcher content differs from desired"
                                        .to_owned(),
                                }),
                            )
                        }
                    }
                }
                ObservedLauncherState::InvalidLauncher => {
                    summary.launchers_conflict += 1;
                    (
                        LauncherOperationKind::Conflict,
                        Some(Conflict::LauncherAlreadyOwnedByDifferentTarget {
                            launcher_path: observed.launcher_path.to_string(),
                            reason: "existing file is not a valid shell link".to_owned(),
                        }),
                    )
                }
                ObservedLauncherState::NonFile => {
                    summary.launchers_conflict += 1;
                    (
                        LauncherOperationKind::Conflict,
                        Some(Conflict::TargetNonFile {
                            path: observed.launcher_path.to_string(),
                        }),
                    )
                }
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
        });
    }
    out
}

fn plan_paths(
    target: &TargetPlan,
    snapshot: &HostSnapshot,
    ledger: Option<&InstallLedger>,
    summary: &mut ExecutionSummary,
) -> Vec<PathOperation> {
    let mut out = Vec::with_capacity(target.path_entries.len());
    for (desired, observed) in target.path_entries.iter().zip(snapshot.path_entries.iter()) {
        let owned = matches!(ledger.and_then(|l| l.resources.get(&desired.key)), Some(OwnedResource::PathEntry { value, .. }) if value.equivalent(&desired.value));
        // Membership of an already target-normalized entry. Splitting, case
        // folding, and host formatting happened in the inspecting adapter.
        let present = observed.search_path.contains(&desired.value);
        let (kind, conflict) = match present {
            true => {
                summary.path_entries_present += 1;
                (PathOperationKind::Present, None)
            }
            false if owned => {
                summary.path_entries_conflict += 1;
                (
                    PathOperationKind::Drift,
                    Some(Conflict::PathEntryConflict {
                        value: desired.value.to_string(),
                        reason: "previously owned search-path entry is missing".into(),
                    }),
                )
            }
            false => {
                summary.path_entries_add += 1;
                (PathOperationKind::Add, None)
            }
        };
        out.push(PathOperation {
            key: desired.key.clone(),
            kind,
            value: desired.value.clone(),
            scope: desired.scope,
            privilege: desired.privilege,
            present,
            previously_owned: owned,
            conflict,
        });
    }
    out
}

fn plan_services(
    target: &TargetPlan,
    snapshot: &HostSnapshot,
    ledger: Option<&InstallLedger>,
    summary: &mut ExecutionSummary,
) -> Vec<ServiceOperation> {
    let mut out = Vec::with_capacity(target.services.len());
    for (desired, observed) in target.services.iter().zip(snapshot.services.iter()) {
        let owned = ledger.and_then(|l| l.resources.get(&desired.key));
        let installed = match owned {
            Some(OwnedResource::Service {
                name, installed, ..
            }) if name == desired.name.as_str() => Some(installed),
            _ => None,
        };
        let current = match &observed.state {
            ObservedServiceState::Absent => ServiceState::Absent,
            ObservedServiceState::Service {
                display_name,
                command,
                start,
                ..
            } => ServiceState::Registration {
                display_name: display_name.clone(),
                command: command.clone(),
                start: *start,
            },
        };
        let (kind, conflict) = if owned.is_some() && installed.is_none()
            || installed.is_some_and(|installed| installed != &current)
        {
            summary.services_conflict += 1;
            (
                ServiceOperationKind::Drift,
                Some(Conflict::ServiceAlreadyExistsWithDifferentConfiguration {
                    service: desired.name.to_string(),
                    reason: "owned service changed after installation".to_owned(),
                }),
            )
        } else {
            match &observed.state {
                ObservedServiceState::Absent => {
                    summary.services_create += 1;
                    (ServiceOperationKind::Create, None)
                }
                ObservedServiceState::Service {
                    display_name,
                    command,
                    start,
                    ..
                } => {
                    let desired_display = desired
                        .display_name
                        .as_ref()
                        .map(|n| n.to_string())
                        .unwrap_or_else(|| desired.name.to_string());
                    let matches = *display_name == desired_display
                        && commands_match(command, &desired.command)
                        && *start == desired.start;
                    if matches {
                        summary.services_unchanged += 1;
                        (ServiceOperationKind::NoOp, None)
                    } else {
                        if installed.is_some() {
                            summary.services_create += 1;
                            (ServiceOperationKind::UpdateOwned, None)
                        } else {
                            summary.services_conflict += 1;
                            (
                                ServiceOperationKind::Conflict,
                                Some(Conflict::ServiceAlreadyExistsWithDifferentConfiguration {
                                    service: desired.name.to_string(),
                                    reason: "existing service configuration differs".to_owned(),
                                }),
                            )
                        }
                    }
                }
            }
        };

        out.push(ServiceOperation {
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
    target: &TargetPlan,
    snapshot: &HostSnapshot,
    ledger: Option<&InstallLedger>,
    summary: &mut ExecutionSummary,
) -> Vec<ProtocolOperation> {
    let mut out = Vec::with_capacity(target.protocols.len());
    for (desired, observed) in target.protocols.iter().zip(snapshot.protocols.iter()) {
        let owned = ledger.and_then(|l| l.resources.get(&desired.key));
        let observed_owned = matches!(owned, Some(OwnedResource::Protocol { installed, .. }) if protocol_observed_matches(installed, &observed.state));
        let has_ownership = matches!(owned, Some(OwnedResource::Protocol { .. }));
        let (kind, conflict) = match &observed.state {
            ObservedProtocolState::Absent => {
                if has_ownership {
                    summary.protocols_conflict += 1;
                    (
                        ProtocolOperationKind::Drift,
                        Some(Conflict::ProtocolAlreadyRegistered {
                            scheme: desired.scheme.to_string(),
                            reason: "zup-owned protocol disappeared".into(),
                        }),
                    )
                } else {
                    summary.protocols_create += 1;
                    (ProtocolOperationKind::Create, None)
                }
            }
            ObservedProtocolState::Registration {
                command,
                url_protocol_marker,
            } => {
                let matches = *url_protocol_marker && commands_match(command, &desired.command);
                if matches && (!has_ownership || observed_owned) {
                    summary.protocols_unchanged += 1;
                    (ProtocolOperationKind::NoOp, None)
                } else if observed_owned {
                    summary.protocols_create += 1;
                    (ProtocolOperationKind::UpdateOwned, None)
                } else if has_ownership {
                    summary.protocols_conflict += 1;
                    (
                        ProtocolOperationKind::Drift,
                        Some(Conflict::ProtocolAlreadyRegistered {
                            scheme: desired.scheme.to_string(),
                            reason: "zup-owned protocol drifted after installation".into(),
                        }),
                    )
                } else {
                    summary.protocols_conflict += 1;
                    (
                        ProtocolOperationKind::Conflict,
                        Some(Conflict::ProtocolAlreadyRegistered {
                            scheme: desired.scheme.to_string(),
                            reason: "existing protocol handler differs or is incomplete".to_owned(),
                        }),
                    )
                }
            }
            ObservedProtocolState::Malformed { reason } => {
                summary.protocols_conflict += 1;
                (
                    if has_ownership {
                        ProtocolOperationKind::Drift
                    } else {
                        ProtocolOperationKind::Conflict
                    },
                    Some(Conflict::ProtocolAlreadyRegistered {
                        scheme: desired.scheme.to_string(),
                        reason: reason.clone(),
                    }),
                )
            }
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
    target: &TargetPlan,
    snapshot: &HostSnapshot,
    ledger: Option<&InstallLedger>,
    summary: &mut ExecutionSummary,
) -> Vec<FileAssociationOperation> {
    let mut out = Vec::with_capacity(target.file_associations.len());
    for (desired, observed) in target
        .file_associations
        .iter()
        .zip(snapshot.file_associations.iter())
    {
        let ext_key = ResourceKey::FileAssociationExtension {
            extension: desired.extension.clone(),
        };
        let owned_id = ledger.and_then(|l| l.resources.get(&desired.key));
        let owned_ext = ledger.and_then(|l| l.resources.get(&ext_key));
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
            FileAssociationOperationKind::Drift
        } else if id_ok {
            FileAssociationOperationKind::NoOp
        } else if matches!(
            observed.association_state,
            ObservedFileAssociationState::Absent
        ) && owned_id.is_none()
        {
            FileAssociationOperationKind::Create
        } else if id_owned_matches {
            FileAssociationOperationKind::UpdateOwned
        } else if owned_id.is_some() {
            FileAssociationOperationKind::Drift
        } else {
            FileAssociationOperationKind::Conflict
        };
        let extension_kind = if owned_ext.is_some() && !ext_owned_matches {
            FileAssociationOperationKind::Drift
        } else if ext_ok {
            FileAssociationOperationKind::NoOp
        } else if matches!(
            observed.extension_state,
            crate::observe::ObservedExtensionState::Absent
        ) && owned_ext.is_none()
        {
            FileAssociationOperationKind::Create
        } else if ext_owned_matches {
            FileAssociationOperationKind::UpdateOwned
        } else if owned_ext.is_some() {
            FileAssociationOperationKind::Drift
        } else {
            FileAssociationOperationKind::Conflict
        };

        let (kind, conflict) =
            if [association_kind, extension_kind].contains(&FileAssociationOperationKind::Drift) {
                summary.file_associations_conflict += 1;
                (
                    FileAssociationOperationKind::Drift,
                    Some(Conflict::FileAssociationConflict {
                        id: desired.id.to_string(),
                        reason: "zup-owned association drifted".into(),
                    }),
                )
            } else if [association_kind, extension_kind]
                .contains(&FileAssociationOperationKind::Conflict)
            {
                summary.file_associations_conflict += 1;
                (
                    FileAssociationOperationKind::Conflict,
                    Some(Conflict::FileAssociationConflict {
                        id: desired.id.to_string(),
                        reason: "foreign association differs".into(),
                    }),
                )
            } else if association_kind == FileAssociationOperationKind::NoOp
                && extension_kind == FileAssociationOperationKind::NoOp
            {
                summary.file_associations_unchanged += 1;
                (FileAssociationOperationKind::NoOp, None)
            } else {
                summary.file_associations_create += 1;
                (
                    if [association_kind, extension_kind]
                        .contains(&FileAssociationOperationKind::UpdateOwned)
                    {
                        FileAssociationOperationKind::UpdateOwned
                    } else {
                        FileAssociationOperationKind::Create
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
fn commands_match(a: &zup_platform::CommandSpec, b: &zup_platform::CommandSpec) -> bool {
    a.executable.equivalent(&b.executable) && a.arguments == b.arguments
}
