use std::collections::BTreeMap;
use std::fs::{self, File};
use std::path::Path;

use thiserror::Error;
use zup_core::{AppId, Frontend, Privilege, ResourceKey, SelectedScope, hash_reader};
use zup_exec::{
    InstallLedger, LauncherState, LifecycleAction, ObservedLauncherState, ObservedServiceState,
    OwnedResource, ServiceState, plan_lifecycle,
};
use zup_platform::TargetPlan;
use zup_transaction::{TransactionPlan, compile_transaction};

use crate::inspect::{InspectError, inspect_target};
use crate::ledger::{InstallLedgerStore, LedgerError};
use crate::lowering::host_path;
use crate::shortcuts::{ShortcutReader, WindowsShortcutReader};
use crate::transaction_payload::{
    AppsFeaturesState, AppsFeaturesValue, AppsPlanningInput, apps_backend_id, apps_key,
    compile_execution_plan, decode_apps_owned_payload,
};

#[derive(Debug, Error)]
pub enum WindowsPlanError {
    #[error(transparent)]
    Inspect(#[from] InspectError),
    #[error(transparent)]
    Ledger(#[from] LedgerError),
    #[error(transparent)]
    Plan(#[from] zup_exec::LifecycleError),
    #[error(transparent)]
    Payload(#[from] crate::transaction_payload::TransactionPayloadError),
    #[error("owned resource inspection failed: {0}")]
    OwnedInspection(String),
    #[error("the target is not a Windows target")]
    UnsupportedTarget,
    #[error("target identity does not match the requested application or scope")]
    TargetMismatch,
}

pub fn plan_target_lifecycle(
    action: LifecycleAction,
    app_id: &AppId,
    scope: SelectedScope,
    target: Option<&TargetPlan>,
    state_root: &Path,
) -> Result<TransactionPlan, WindowsPlanError> {
    plan_target_lifecycle_with_frontend(action, app_id, scope, target, state_root, Frontend::Gui)
}

pub fn plan_target_lifecycle_with_frontend(
    action: LifecycleAction,
    app_id: &AppId,
    scope: SelectedScope,
    target: Option<&TargetPlan>,
    state_root: &Path,
    frontend: Frontend,
) -> Result<TransactionPlan, WindowsPlanError> {
    if let Some(target) = target {
        if target.target.operating_system() != zup_core::TargetOperatingSystem::Windows {
            return Err(WindowsPlanError::UnsupportedTarget);
        }
        if &target.app.id != app_id || target.scope != scope {
            return Err(WindowsPlanError::TargetMismatch);
        }
    }
    let ledger = InstallLedgerStore::new(state_root).load(app_id, scope)?;
    let snapshot = target.map(inspect_target).transpose()?;
    let matches = ledger
        .as_ref()
        .map(inspect_owned_matches)
        .transpose()?
        .unwrap_or_default();
    let execution = plan_lifecycle(action, target, snapshot.as_ref(), ledger.as_ref(), &matches)?;

    let key_path = format!(
        "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\{}",
        app_id
    );
    let current = crate::integration::inspect_uninstall_registration(scope, app_id.as_str())
        .map_err(|error| WindowsPlanError::OwnedInspection(error.to_string()))?;
    let owned = ledger
        .as_ref()
        .and_then(|ledger| ledger.resources.get(&apps_key(app_id)))
        .and_then(|resource| match resource {
            OwnedResource::Backend {
                id,
                payload,
                privilege,
                ..
            } if *id == apps_backend_id(app_id) => {
                Some((decode_apps_owned_payload(payload).ok()?, *privilege))
            }
            _ => None,
        });
    let (owned, owned_privilege) = owned.map_or((None, Privilege::User), |(state, privilege)| {
        (Some(state), privilege)
    });
    let desired = target.and_then(|target| {
        target
            .files
            .iter()
            .find(|file| matches!(file.key, ResourceKey::Maintenance { .. }))
            .map(|_| apps_state(target, scope, state_root, frontend))
    });
    // The Apps & Features registration needs the same authority as the
    // maintenance executable it points at, so reuse that operation privilege.
    let maintenance_privilege = target
        .and_then(|target| {
            target
                .files
                .iter()
                .find(|file| matches!(file.key, ResourceKey::Maintenance { .. }))
                .map(|file| file.privilege)
        })
        .unwrap_or(owned_privilege);
    let apps = AppsPlanningInput {
        app_id: app_id.clone(),
        privilege: maintenance_privilege,
        current,
        owned,
        desired,
        key_path,
        uninstall: execution.uninstall,
    };
    let input = compile_execution_plan(&execution, target, scope, app_id, ledger.as_ref(), apps)?;
    compile_transaction(&input)
        .map_err(|error| WindowsPlanError::OwnedInspection(error.to_string()))
}

fn apps_state(
    target: &TargetPlan,
    scope: SelectedScope,
    state_root: &Path,
    frontend: Frontend,
) -> AppsFeaturesState {
    let maintenance = target
        .files
        .iter()
        .find(|file| matches!(file.key, ResourceKey::Maintenance { .. }))
        .expect("maintenance file selected by caller");
    let maintenance_path = host_path(&maintenance.destination);
    let scope_name = match scope {
        SelectedScope::User => "user",
        SelectedScope::Machine => "machine",
    };
    let state_root = state_root.to_string_lossy().into_owned();
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
        crate::cmdline::format_command_line(&maintenance_path, &arguments)
    };
    let estimated_kb = target
        .summary
        .install_bytes
        .div_ceil(1024)
        .min(u32::MAX as u64) as u32;
    let mut values = BTreeMap::new();
    values.insert(
        "DisplayName".into(),
        AppsFeaturesValue::String(target.app.name.to_string()),
    );
    values.insert(
        "DisplayVersion".into(),
        AppsFeaturesValue::String(target.app.version.to_string()),
    );
    values.insert(
        "Publisher".into(),
        AppsFeaturesValue::String(
            target
                .app
                .publisher
                .as_ref()
                .map_or_else(String::new, ToString::to_string),
        ),
    );
    values.insert(
        "InstallLocation".into(),
        AppsFeaturesValue::String(target.install_directory.to_string()),
    );
    values.insert(
        "DisplayIcon".into(),
        AppsFeaturesValue::String(format!("{},0", maintenance_path.display())),
    );
    values.insert(
        "UninstallString".into(),
        AppsFeaturesValue::String(command("uninstall")),
    );
    values.insert(
        "ModifyPath".into(),
        AppsFeaturesValue::String(command("modify")),
    );
    if frontend == Frontend::Headless {
        values.insert("NoModify".into(), AppsFeaturesValue::Dword(1));
        values.insert("NoRepair".into(), AppsFeaturesValue::Dword(1));
    }
    values.insert(
        "EstimatedSize".into(),
        AppsFeaturesValue::Dword(estimated_kb),
    );
    AppsFeaturesState { values }
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
            } => match fs::symlink_metadata(host_path(destination)) {
                Ok(meta) if meta.file_type().is_file() && !meta.file_type().is_symlink() => {
                    let file = File::open(host_path(destination))
                        .map_err(|error| WindowsPlanError::OwnedInspection(error.to_string()))?;
                    hash_reader(file)
                        .map_err(|error| WindowsPlanError::OwnedInspection(error.to_string()))?
                        == (*size, *sha256)
                }
                Ok(_) => false,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                Err(error) => return Err(WindowsPlanError::OwnedInspection(error.to_string())),
            },
            OwnedResource::Launcher {
                launcher_path,
                installed,
                ..
            } => match WindowsShortcutReader
                .read_shortcut(launcher_path)
                .map_err(WindowsPlanError::OwnedInspection)?
            {
                ObservedLauncherState::Absent => *installed == LauncherState::Absent,
                ObservedLauncherState::Launcher {
                    target,
                    arguments,
                    working_directory,
                } => {
                    *installed
                        == LauncherState::Launcher {
                            target,
                            arguments,
                            working_directory,
                        }
                }
                _ => false,
            },
            OwnedResource::PathEntry {
                value, value_type, ..
            } => {
                // Ownership is checked against the search path that owns the
                // entry, using Windows segment identity rather than an exact
                // string compare.
                let entry = value.clone();
                let (kind, raw) = crate::integration::read_path(ledger.scope)
                    .map_err(|error| WindowsPlanError::OwnedInspection(error.to_string()))?;
                (kind == *value_type || crate::search_path::lost_expansion(value_type, &kind))
                    && crate::search_path::contains(&ledger.target, &raw, &entry)
            }
            OwnedResource::Service {
                name, installed, ..
            } => {
                match crate::scm::query_service(name, &ledger.target)
                    .map_err(WindowsPlanError::OwnedInspection)?
                {
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
                    &ledger.target,
                ))?
                .is_some_and(|state| state == *installed)
            }
            OwnedResource::FileAssociation { installed, .. } => {
                let ResourceKey::FileAssociation { id } = key else {
                    return Err(WindowsPlanError::OwnedInspection(
                        "file association key mismatch".into(),
                    ));
                };
                owned_registry_state(crate::integration::read_progid(
                    ledger.scope,
                    id.as_str(),
                    &ledger.target,
                ))?
                .is_some_and(|state| state == *installed)
            }
            OwnedResource::Extension { installed, .. } => {
                let ResourceKey::FileAssociationExtension { extension } = key else {
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
            OwnedResource::Backend { id, payload, .. } => {
                let ResourceKey::Backend { id: key_id } = key else {
                    return Err(WindowsPlanError::OwnedInspection(
                        "backend key mismatch".into(),
                    ));
                };
                if key_id != id {
                    return Err(WindowsPlanError::OwnedInspection(
                        "backend identity mismatch".into(),
                    ));
                }
                let state = decode_apps_owned_payload(payload)
                    .map_err(|error| WindowsPlanError::OwnedInspection(error.to_string()))?;
                let app_id = id
                    .as_str()
                    .strip_prefix("windows:apps-features:")
                    .ok_or_else(|| {
                        WindowsPlanError::OwnedInspection(
                            "backend identity is not Apps & Features".into(),
                        )
                    })?;
                crate::integration::inspect_uninstall_registration(ledger.scope, app_id)
                    .ok()
                    .flatten()
                    .is_some_and(|current| current == state)
            }
        };
        matches.insert(key.clone(), found);
    }
    Ok(matches)
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
