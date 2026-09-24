//! Windows observation for the pure lifecycle planner.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::path::Path;

use thiserror::Error;
use zup_core::{AppId, Frontend, ResourceKey, SelectedScope, hash_reader};
use zup_exec::{
    ExecutionPlan, InstallLedger, LifecycleAction, ObservedServiceState, ObservedShortcutState,
    OwnedResource, ServiceState, ShortcutState, plan_lifecycle,
};
use zup_platform::TargetPlan;

use crate::inspect::{InspectError, inspect_target};
use crate::ledger::{InstallLedgerStore, LedgerError};
use crate::shortcuts::{ShortcutReader, WindowsShortcutReader};

#[derive(Debug, Error)]
pub enum WindowsPlanError {
    #[error(transparent)]
    Inspect(#[from] InspectError),
    #[error(transparent)]
    Ledger(#[from] LedgerError),
    #[error(transparent)]
    Plan(#[from] zup_exec::LifecycleError),
    #[error("owned resource inspection failed: {0}")]
    OwnedInspection(String),
}

fn owned_registry_state<T>(
    result: Result<T, crate::integration::IntegrationError>,
) -> Result<Option<T>, WindowsPlanError> {
    match result {
        Ok(state) => Ok(Some(state)),
        Err(crate::integration::IntegrationError::Drift(_)) => Ok(None),
        Err(error) => Err(WindowsPlanError::OwnedInspection(error.to_string())),
    }
}

pub fn plan_target_lifecycle(
    action: LifecycleAction,
    app_id: &AppId,
    scope: SelectedScope,
    target: Option<&TargetPlan>,
    state_root: &Path,
) -> Result<ExecutionPlan, WindowsPlanError> {
    plan_target_lifecycle_with_frontend(action, app_id, scope, target, state_root, Frontend::Gui)
}

pub fn plan_target_lifecycle_with_frontend(
    action: LifecycleAction,
    app_id: &AppId,
    scope: SelectedScope,
    target: Option<&TargetPlan>,
    state_root: &Path,
    frontend: Frontend,
) -> Result<ExecutionPlan, WindowsPlanError> {
    let ledger = InstallLedgerStore::new(state_root).load(app_id, scope)?;
    let snapshot = target.map(inspect_target).transpose()?;
    let matches = ledger
        .as_ref()
        .map(inspect_owned_matches)
        .transpose()?
        .unwrap_or_default();
    let mut execution =
        plan_lifecycle(action, target, snapshot.as_ref(), ledger.as_ref(), &matches)?;
    if let Some(target) = target
        && let Some(maintenance) = target
            .files
            .iter()
            .find(|file| matches!(file.key, ResourceKey::Maintenance { .. }))
    {
        let app_id = target.app.id.to_string();
        let key = ResourceKey::UninstallEntry {
            app_id: app_id.clone(),
        };
        let key_path = format!("Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\{app_id}");
        let current = crate::integration::read_uninstall_entry(scope, &key_path)
            .map_err(|error| WindowsPlanError::OwnedInspection(error.to_string()))?;
        let previous = match (
            ledger
                .as_ref()
                .and_then(|ledger| ledger.resources.get(&key)),
            current,
        ) {
            (None, None) => None,
            (
                Some(OwnedResource::UninstallEntry {
                    scope: owned_scope,
                    state,
                }),
                Some(current),
            ) if *owned_scope == scope && *state == current => Some(state.clone()),
            _ => {
                return Err(WindowsPlanError::OwnedInspection(format!(
                    "Apps & Features registration {app_id} is occupied or changed"
                )));
            }
        };
        let scope_name = match scope {
            SelectedScope::User => "user",
            SelectedScope::Machine => "machine",
        };
        let state_root = state_root.to_string_lossy().into_owned();
        let maintenance_path = maintenance.destination.as_path();
        let command = |action: &str| {
            let mut arguments = vec![
                action.to_owned(),
                "--scope".into(),
                scope_name.into(),
                "--state-root".into(),
                state_root.clone(),
            ];
            match frontend {
                Frontend::Gui => arguments.push("--ui".into()),
                Frontend::Headless => {
                    arguments.push("--yes".into());
                    arguments.push("--output".into());
                    arguments.push("json".into());
                }
                Frontend::Console => {}
            }
            crate::cmdline::format_command_line(maintenance_path, &arguments)
        };
        let mut values = BTreeMap::new();
        let mut string = |name: &str, value: String| {
            values.insert(
                name.to_owned(),
                zup_exec::UninstallEntryValue::String(value),
            );
        };
        string("DisplayName", target.app.name.to_string());
        string("DisplayVersion", target.app.version.to_string());
        string(
            "Publisher",
            target
                .app
                .publisher
                .as_ref()
                .map_or_else(String::new, ToString::to_string),
        );
        string("InstallLocation", target.install_directory.to_string());
        let estimated_kb = target
            .summary
            .install_bytes
            .div_ceil(1024)
            .min(u32::MAX as u64) as u32;
        string("DisplayIcon", format!("{},0", maintenance_path.display()));
        string("UninstallString", command("uninstall"));
        string("ModifyPath", command("modify"));
        if frontend == Frontend::Headless {
            values.insert("NoModify".into(), zup_exec::UninstallEntryValue::Dword(1));
            values.insert("NoRepair".into(), zup_exec::UninstallEntryValue::Dword(1));
        }
        values.insert(
            "EstimatedSize".into(),
            zup_exec::UninstallEntryValue::Dword(estimated_kb),
        );
        execution
            .uninstall_entries
            .push(zup_exec::UninstallEntryOperation {
                key,
                scope,
                key_path,
                previous,
                installed: zup_exec::UninstallEntryState { values },
            });
    }
    Ok(execution)
}

fn inspect_owned_matches(
    ledger: &InstallLedger,
) -> Result<BTreeMap<ResourceKey, bool>, WindowsPlanError> {
    let mut matches = BTreeMap::new();
    for (key, owned) in &ledger.resources {
        let found = match owned {
            OwnedResource::File {
                destination,
                sha256,
                size,
                ..
            } => match fs::symlink_metadata(destination.as_path()) {
                Ok(meta) if meta.file_type().is_file() && !meta.file_type().is_symlink() => {
                    let file = File::open(destination.as_path())
                        .map_err(|error| WindowsPlanError::OwnedInspection(error.to_string()))?;
                    let identity = hash_reader(file)
                        .map_err(|error| WindowsPlanError::OwnedInspection(error.to_string()))?;
                    identity == (*size, *sha256)
                }
                Ok(_) => false,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                Err(error) => return Err(WindowsPlanError::OwnedInspection(error.to_string())),
            },
            OwnedResource::Shortcut {
                link_path,
                installed,
                ..
            } => {
                match WindowsShortcutReader
                    .read_shortcut(link_path)
                    .map_err(WindowsPlanError::OwnedInspection)?
                {
                    ObservedShortcutState::Absent => *installed == ShortcutState::Absent,
                    ObservedShortcutState::Shortcut {
                        target,
                        arguments,
                        working_directory,
                    } => {
                        *installed
                            == ShortcutState::Link {
                                target,
                                arguments,
                                working_directory,
                            }
                    }
                    _ => false,
                }
            }
            OwnedResource::PathEntry { value, value_type } => {
                let (ty, raw) = crate::integration::read_path(ledger.scope)
                    .map_err(|error| WindowsPlanError::OwnedInspection(error.to_string()))?;
                (ty == *value_type || (value_type == "missing" && ty == "expand_sz"))
                    && raw.split(';').any(|entry| entry == value.to_string())
            }
            OwnedResource::Service {
                name, installed, ..
            } => {
                match crate::scm::query_service(name).map_err(WindowsPlanError::OwnedInspection)? {
                    ObservedServiceState::Absent => *installed == ServiceState::Absent,
                    ObservedServiceState::Service {
                        display_name,
                        command,
                        start,
                        ..
                    } => {
                        *installed
                            == ServiceState::Registration {
                                display_name,
                                command,
                                start,
                            }
                    }
                }
            }
            OwnedResource::Protocol { installed, .. } => {
                let ResourceKey::Protocol { scheme } = key else {
                    return Err(WindowsPlanError::OwnedInspection(
                        "protocol key mismatch".into(),
                    ));
                };
                owned_registry_state(crate::integration::read_protocol(
                    ledger.scope,
                    scheme.as_str(),
                ))?
                .is_some_and(|state| state == *installed)
            }
            OwnedResource::ProgId { installed, .. } => {
                let ResourceKey::FileType { id } = key else {
                    return Err(WindowsPlanError::OwnedInspection(
                        "ProgID key mismatch".into(),
                    ));
                };
                owned_registry_state(crate::integration::read_progid(ledger.scope, id.as_str()))?
                    .is_some_and(|state| state == *installed)
            }
            OwnedResource::Extension { installed, .. } => {
                let ResourceKey::FileTypeExtension { extension } = key else {
                    return Err(WindowsPlanError::OwnedInspection(
                        "extension key mismatch".into(),
                    ));
                };
                owned_registry_state(crate::integration::read_extension(
                    ledger.scope,
                    extension.as_str(),
                ))?
                .is_some_and(|state| state == *installed)
            }
            OwnedResource::UninstallEntry { scope, state } => {
                let ResourceKey::UninstallEntry { app_id } = key else {
                    return Err(WindowsPlanError::OwnedInspection(
                        "uninstall entry key mismatch".into(),
                    ));
                };
                let path =
                    format!("Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\{app_id}");
                *scope == ledger.scope
                    && owned_registry_state(crate::integration::read_uninstall_entry(
                        *scope, &path,
                    ))? == Some(Some(state.clone()))
            }
        };
        matches.insert(key.clone(), found);
    }
    Ok(matches)
}
