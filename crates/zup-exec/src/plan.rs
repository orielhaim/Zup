//! Pure delta planner: TargetPlan + MachineSnapshot → ExecutionPlan.

use std::collections::BTreeMap;

use miette::Diagnostic;
use thiserror::Error;
use tracing::{info, info_span};
use zup_core::ResourceKey;
use zup_platform::TargetPlan;

use crate::observe::{
    MachineSnapshot, ObservedFileState, ObservedFileType, ObservedPathEntry, ObservedProgIdState,
    ObservedProtocolState, ObservedServiceState, ObservedShortcutState, PathEntryState,
};
use crate::operation::{
    Conflict, ExecutionPlan, ExecutionSummary, ExternalActionOperation, FileOperation,
    FileOperationKind, FilePrecondition, FileTypeOperation, FileTypeOperationKind, PathOperation,
    PathOperationKind, ProtocolOperation, ProtocolOperationKind, ServiceOperation,
    ServiceOperationKind, ShortcutOperation, ShortcutOperationKind,
};
use crate::{
    ExtensionState, InstallLedger, OwnedResource, ProgIdState, ProtocolState, ServiceState,
    ShortcutState,
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
    snapshot: &MachineSnapshot,
    ledger: Option<&InstallLedger>,
) -> Result<ExecutionPlan, ExecutionPlanError> {
    let _span = info_span!("plan_execution").entered();

    validate_snapshot(target, snapshot)?;
    if ledger.is_some_and(|ledger| {
        ledger.schema != crate::INSTALL_LEDGER_SCHEMA
            || ledger.app_id != target.app.id
            || ledger.scope != target.scope
    }) {
        return Err(ExecutionPlanError::LedgerMismatch);
    }

    let mut summary = ExecutionSummary {
        requires_elevation: target.summary.requires_elevation,
        ..Default::default()
    };

    let files = plan_files(target, snapshot, ledger, &mut summary)?;
    let shortcuts = plan_shortcuts(target, snapshot, ledger, &mut summary);
    let path_entries = plan_paths(target, snapshot, ledger, &mut summary);
    let services = plan_services(target, snapshot, ledger, &mut summary);
    let protocols = plan_protocols(target, snapshot, ledger, &mut summary);
    let file_types = plan_file_types(target, snapshot, ledger, &mut summary);
    let external_actions = plan_actions(target, &mut summary);

    info!(
        create = summary.files_create
            + summary.shortcuts_create
            + summary.path_entries_add
            + summary.services_create
            + summary.protocols_create
            + summary.file_types_create,
        conflict = summary.files_conflict
            + summary.shortcuts_conflict
            + summary.path_entries_conflict
            + summary.services_conflict
            + summary.protocols_conflict
            + summary.file_types_conflict,
        opaque = summary.opaque_actions,
        "execution plan complete"
    );

    Ok(ExecutionPlan {
        selected_components: target.selected_components.clone(),
        uninstall: false,
        removals: Vec::new(),
        files,
        shortcuts,
        path_entries,
        services,
        protocols,
        file_types,
        uninstall_entries: Vec::new(),
        external_actions,
        summary,
    })
}

fn key_debug(key: &ResourceKey) -> String {
    format!("{key:?}")
}

fn validate_snapshot(
    target: &TargetPlan,
    snapshot: &MachineSnapshot,
) -> Result<(), ExecutionPlanError> {
    check_unique(&snapshot.files.iter().map(|o| &o.key).collect::<Vec<_>>())?;
    check_unique(
        &snapshot
            .shortcuts
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
            .file_types
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

    // Shortcuts
    let mut seen = BTreeMap::new();
    for observed in &snapshot.shortcuts {
        if seen.insert(&observed.key, observed).is_some() {
            return Err(ExecutionPlanError::DuplicateSnapshotObservation {
                key: key_debug(&observed.key),
            });
        }
    }
    for desired in &target.shortcuts {
        let Some(observed) = seen.get(&desired.key) else {
            return Err(ExecutionPlanError::MissingSnapshotObservation {
                key: key_debug(&desired.key),
            });
        };
        if observed.link_path != desired.link_path {
            return Err(ExecutionPlanError::SnapshotResourceMismatch {
                key: key_debug(&desired.key),
                reason: format!(
                    "link path mismatch: `{}` vs `{}`",
                    observed.link_path, desired.link_path
                ),
            });
        }
    }
    for key in seen.keys() {
        if !target.shortcuts.iter().any(|s| &s.key == *key) {
            return Err(ExecutionPlanError::SnapshotResourceMismatch {
                key: key_debug(key),
                reason: "unexpected shortcut observation".to_owned(),
            });
        }
    }

    // PATH, services, protocols, file types: key presence only.
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
        &target.file_types.iter().map(|x| &x.key).collect::<Vec<_>>(),
        &snapshot
            .file_types
            .iter()
            .map(|x| &x.key)
            .collect::<Vec<_>>(),
        "file type",
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
    snapshot: &MachineSnapshot,
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
            conflict,
        });
    }
    Ok(out)
}

fn plan_shortcuts(
    target: &TargetPlan,
    snapshot: &MachineSnapshot,
    ledger: Option<&InstallLedger>,
    summary: &mut ExecutionSummary,
) -> Vec<ShortcutOperation> {
    let mut out = Vec::with_capacity(target.shortcuts.len());
    for (desired, observed) in target.shortcuts.iter().zip(snapshot.shortcuts.iter()) {
        let owned = ledger.and_then(|l| l.resources.get(&desired.key));
        let installed = match owned {
            Some(OwnedResource::Shortcut {
                link_path,
                installed,
                ..
            }) if link_path == &desired.link_path => Some(installed),
            _ => None,
        };
        let current = match &observed.state {
            ObservedShortcutState::Absent => Some(ShortcutState::Absent),
            ObservedShortcutState::Shortcut {
                target,
                arguments,
                working_directory,
            } => Some(ShortcutState::Link {
                target: target.clone(),
                arguments: arguments.clone(),
                working_directory: working_directory.clone(),
            }),
            _ => None,
        };
        let (kind, conflict) = if owned.is_some() && installed.is_none()
            || installed.is_some_and(|installed| current.as_ref() != Some(installed))
        {
            summary.shortcuts_conflict += 1;
            (
                ShortcutOperationKind::Drift,
                Some(Conflict::ShortcutAlreadyOwnedByDifferentTarget {
                    link_path: observed.link_path.to_string(),
                    reason: "owned shortcut changed after installation".to_owned(),
                }),
            )
        } else {
            match &observed.state {
                ObservedShortcutState::Absent => {
                    summary.shortcuts_create += 1;
                    (ShortcutOperationKind::Create, None)
                }
                ObservedShortcutState::Shortcut {
                    target,
                    arguments,
                    working_directory,
                } => {
                    let matches = *target == desired.target
                        && *arguments == desired.arguments
                        && *working_directory == desired.working_directory;
                    if matches {
                        summary.shortcuts_unchanged += 1;
                        (ShortcutOperationKind::NoOp, None)
                    } else {
                        if installed.is_some() {
                            summary.shortcuts_create += 1;
                            (ShortcutOperationKind::UpdateOwned, None)
                        } else {
                            summary.shortcuts_conflict += 1;
                            (
                                ShortcutOperationKind::Conflict,
                                Some(Conflict::ShortcutAlreadyOwnedByDifferentTarget {
                                    link_path: observed.link_path.to_string(),
                                    reason: "existing shortcut content differs from desired"
                                        .to_owned(),
                                }),
                            )
                        }
                    }
                }
                ObservedShortcutState::InvalidShortcut => {
                    summary.shortcuts_conflict += 1;
                    (
                        ShortcutOperationKind::Conflict,
                        Some(Conflict::ShortcutAlreadyOwnedByDifferentTarget {
                            link_path: observed.link_path.to_string(),
                            reason: "existing file is not a valid shell link".to_owned(),
                        }),
                    )
                }
                ObservedShortcutState::NonFile => {
                    summary.shortcuts_conflict += 1;
                    (
                        ShortcutOperationKind::Conflict,
                        Some(Conflict::TargetNonFile {
                            path: observed.link_path.to_string(),
                        }),
                    )
                }
            }
        };

        out.push(ShortcutOperation {
            key: desired.key.clone(),
            kind,
            link_path: desired.link_path.clone(),
            target: desired.target.clone(),
            arguments: desired.arguments.clone(),
            working_directory: desired.working_directory.clone(),
            previous: observed.state.clone(),
            conflict,
        });
    }
    out
}

fn plan_paths(
    target: &TargetPlan,
    snapshot: &MachineSnapshot,
    ledger: Option<&InstallLedger>,
    summary: &mut ExecutionSummary,
) -> Vec<PathOperation> {
    let mut out = Vec::with_capacity(target.path_entries.len());
    for (desired, observed) in target.path_entries.iter().zip(snapshot.path_entries.iter()) {
        let owned = matches!(ledger.and_then(|l| l.resources.get(&desired.key)), Some(OwnedResource::PathEntry { value, .. }) if value == &desired.value);
        let (kind, conflict) = match &observed.state {
            PathEntryState::Absent => {
                if owned {
                    summary.path_entries_conflict += 1;
                    (
                        PathOperationKind::Drift,
                        Some(Conflict::PathEntryConflict {
                            value: desired.value.to_string(),
                            reason: "previously owned PATH entry was removed".into(),
                        }),
                    )
                } else {
                    summary.path_entries_add += 1;
                    (PathOperationKind::Add, None)
                }
            }
            PathEntryState::Present { raw_entry } => {
                if owned && raw_entry != &desired.value.to_string() {
                    summary.path_entries_conflict += 1;
                    (
                        PathOperationKind::Drift,
                        Some(Conflict::PathEntryConflict {
                            value: desired.value.to_string(),
                            reason: "owned PATH entry changed form".into(),
                        }),
                    )
                } else {
                    summary.path_entries_present += 1;
                    (PathOperationKind::Present, None)
                }
            }
        };
        let _ = &ObservedPathEntry {
            key: observed.key.clone(),
            desired: observed.desired.clone(),
            scope: observed.scope,
            state: observed.state.clone(),
        };
        out.push(PathOperation {
            key: desired.key.clone(),
            kind,
            value: desired.value.clone(),
            scope: desired.scope,
            previous: observed.state.clone(),
            previously_owned: owned,
            conflict,
        });
    }
    out
}

fn plan_services(
    target: &TargetPlan,
    snapshot: &MachineSnapshot,
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
            previous: observed.state.clone(),
            conflict,
        });
    }
    out
}

fn plan_protocols(
    target: &TargetPlan,
    snapshot: &MachineSnapshot,
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
            previous: observed.state.clone(),
            conflict,
        });
    }
    out
}

fn plan_file_types(
    target: &TargetPlan,
    snapshot: &MachineSnapshot,
    ledger: Option<&InstallLedger>,
    summary: &mut ExecutionSummary,
) -> Vec<FileTypeOperation> {
    let mut out = Vec::with_capacity(target.file_types.len());
    for (desired, observed) in target.file_types.iter().zip(snapshot.file_types.iter()) {
        let ext_key = ResourceKey::FileTypeExtension {
            extension: desired.extension.clone(),
        };
        let owned_id = ledger.and_then(|l| l.resources.get(&desired.key));
        let owned_ext = ledger.and_then(|l| l.resources.get(&ext_key));
        let id_owned_matches = matches!(owned_id, Some(OwnedResource::ProgId { installed, .. }) if progid_observed_matches(installed, &observed.id_state));
        let ext_owned_matches = matches!(owned_ext, Some(OwnedResource::Extension { installed, .. }) if extension_observed_matches(installed, &observed.extension_state));
        let desired_desc = desired.description.clone();
        let id_ok = match &observed.id_state {
            ObservedProgIdState::Absent => false,
            ObservedProgIdState::Registration {
                description,
                command,
            } => *description == desired_desc && commands_match(command, &desired.command),
            ObservedProgIdState::Malformed { .. } => false,
        };
        let ext_ok = match &observed.extension_state {
            crate::observe::ObservedExtensionState::Absent => false,
            crate::observe::ObservedExtensionState::Mapped { prog_id } => {
                prog_id.eq_ignore_ascii_case(desired.id.as_str())
            }
            crate::observe::ObservedExtensionState::Malformed { .. } => false,
        };

        let prog_id_kind = if owned_id.is_some() && !id_owned_matches {
            FileTypeOperationKind::Drift
        } else if id_ok {
            FileTypeOperationKind::NoOp
        } else if matches!(observed.id_state, ObservedProgIdState::Absent) && owned_id.is_none() {
            FileTypeOperationKind::Create
        } else if id_owned_matches {
            FileTypeOperationKind::UpdateOwned
        } else if owned_id.is_some() {
            FileTypeOperationKind::Drift
        } else {
            FileTypeOperationKind::Conflict
        };
        let extension_kind = if owned_ext.is_some() && !ext_owned_matches {
            FileTypeOperationKind::Drift
        } else if ext_ok {
            FileTypeOperationKind::NoOp
        } else if matches!(
            observed.extension_state,
            crate::observe::ObservedExtensionState::Absent
        ) && owned_ext.is_none()
        {
            FileTypeOperationKind::Create
        } else if ext_owned_matches {
            FileTypeOperationKind::UpdateOwned
        } else if owned_ext.is_some() {
            FileTypeOperationKind::Drift
        } else {
            FileTypeOperationKind::Conflict
        };

        let (kind, conflict) = if [prog_id_kind, extension_kind]
            .contains(&FileTypeOperationKind::Drift)
        {
            summary.file_types_conflict += 1;
            (
                FileTypeOperationKind::Drift,
                Some(Conflict::FileTypeProgIdConflict {
                    id: desired.id.to_string(),
                    reason: "zup-owned association drifted".into(),
                }),
            )
        } else if [prog_id_kind, extension_kind].contains(&FileTypeOperationKind::Conflict) {
            summary.file_types_conflict += 1;
            (
                FileTypeOperationKind::Conflict,
                Some(Conflict::FileTypeProgIdConflict {
                    id: desired.id.to_string(),
                    reason: "foreign association differs".into(),
                }),
            )
        } else if prog_id_kind == FileTypeOperationKind::NoOp
            && extension_kind == FileTypeOperationKind::NoOp
        {
            summary.file_types_unchanged += 1;
            (FileTypeOperationKind::NoOp, None)
        } else {
            summary.file_types_create += 1;
            (
                if [prog_id_kind, extension_kind].contains(&FileTypeOperationKind::UpdateOwned) {
                    FileTypeOperationKind::UpdateOwned
                } else {
                    FileTypeOperationKind::Create
                },
                None,
            )
        };

        let _ = ObservedFileType {
            key: observed.key.clone(),
            extension: observed.extension.clone(),
            id: observed.id.clone(),
            scope: observed.scope,
            id_state: observed.id_state.clone(),
            extension_state: observed.extension_state.clone(),
        };

        out.push(FileTypeOperation {
            key: desired.key.clone(),
            kind,
            prog_id_kind,
            extension_kind,
            extension: desired.extension.to_string(),
            id: desired.id.to_string(),
            description: desired.description.clone(),
            command: desired.command.clone(),
            scope: desired.scope,
            previous_id: observed.id_state.clone(),
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

fn progid_observed_matches(expected: &ProgIdState, observed: &ObservedProgIdState) -> bool {
    match (expected, observed) {
        (ProgIdState::Absent, ObservedProgIdState::Absent) => true,
        (
            ProgIdState::Registration {
                description: a,
                command: ac,
            },
            ObservedProgIdState::Registration {
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
            ExtensionState::Mapped { prog_id: a },
            crate::observe::ObservedExtensionState::Mapped { prog_id: b },
        ) => a.eq_ignore_ascii_case(b),
        _ => false,
    }
}

fn plan_actions(
    target: &TargetPlan,
    summary: &mut ExecutionSummary,
) -> Vec<ExternalActionOperation> {
    let mut out = Vec::with_capacity(target.actions.len());
    for action in &target.actions {
        summary.opaque_actions += 1;
        out.push(ExternalActionOperation {
            key: action.key.clone(),
            id: action.id.clone(),
            kind: action.kind,
            apply: action.apply.clone(),
            rollback: action.rollback.clone(),
            uninstall: action.uninstall.clone(),
            privilege: action.privilege,
            opaque: action.opaque,
        });
    }
    out
}

/// Semantic command equality (path case/separator-insensitive; args exact).
fn commands_match(a: &zup_platform::CommandSpec, b: &zup_platform::CommandSpec) -> bool {
    paths_equal(&a.executable, &b.executable) && a.arguments == b.arguments
}

fn paths_equal(a: &zup_platform::TargetPath, b: &zup_platform::TargetPath) -> bool {
    let an = a.to_string().replace('\\', "/").to_lowercase();
    let bn = b.to_string().replace('\\', "/").to_lowercase();
    an.trim_end_matches('/') == bn.trim_end_matches('/')
}

/// Normalize a PATH segment for comparison (quotes, slashes, case, trailing sep).
pub fn normalize_path_entry(raw: &str) -> String {
    let trimmed = raw.trim();
    let unquoted = trimmed
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(trimmed)
        .trim();
    let normalized = unquoted.replace('\\', "/").to_lowercase();
    normalized.trim_end_matches('/').to_owned()
}

/// True when `desired` appears in the PATH string under Windows PATH rules.
pub fn path_contains_entry(path_value: &str, desired: &zup_platform::TargetPath) -> bool {
    let want = normalize_path_entry(&desired.to_string());
    path_value
        .split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .any(|entry| {
            // Do not treat `%VAR%` forms as equal to concrete paths.
            if entry.contains('%') {
                return false;
            }
            normalize_path_entry(entry) == want
        })
}
