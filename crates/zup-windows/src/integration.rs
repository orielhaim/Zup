use std::collections::BTreeMap;
use zup_plan::DeploymentNode;
#[cfg(test)]
use zup_plan::{BackendOperation, BackendOperationIntent, OperationId, Phase};

use windows_link::link;
use windows_registry::{CURRENT_USER, Key, LOCAL_MACHINE, Type};
use zup_core::{ResourceKey, SelectedScope, Target};
use zup_plan::{
    ExtensionState, FileAssociationState, LauncherState, OwnedResource, ProtocolState, ServiceState,
};
use zup_transaction::{OperationReceipt, ReconcileResult};

use crate::cmdline::{command_spec_from_command_line, format_command_line};
use crate::lowering::host_path;
use crate::transaction_payload::{
    ApplyPayload, AppsFeaturesOperation, AppsFeaturesState, AppsFeaturesValue, BackendReceipt,
    NativeReconcileResult, RemovePayload, decode, receipt_bytes, receipt_from_bytes,
};

#[derive(Debug, thiserror::Error)]
pub enum IntegrationError {
    #[error("registry operation failed: {0}")]
    Registry(String),
    #[error("managed resource drift: {0}")]
    Drift(String),
    #[error("unsupported managed operation")]
    Unsupported,
}

pub(crate) fn apply_managed(node: &DeploymentNode) -> Result<OperationReceipt, IntegrationError> {
    let operation = node
        .meta
        .backend
        .as_ref()
        .ok_or(IntegrationError::Unsupported)?;
    let key = operation.key.clone();
    match decode::<ApplyPayload>(&operation.payload)
        .map_err(|error| IntegrationError::Drift(error.to_string()))?
    {
        ApplyPayload::Launcher(op) => {
            native_apply(key, crate::shortcuts::apply(&op).map_err(drift)?)
        }
        ApplyPayload::Path(op) => apply_path(&key, &op),
        ApplyPayload::Protocol(op) => apply_protocol(&key, &op),
        ApplyPayload::FileAssociation(op) => apply_progid(&key, &op),
        ApplyPayload::Extension(op) => apply_extension(&key, &op),
        ApplyPayload::AppsFeatures { operation } => apply_apps(&key, &operation),
    }
}

fn native_apply(
    key: ResourceKey,
    receipt: BackendReceipt,
) -> Result<OperationReceipt, IntegrationError> {
    let payload =
        receipt_bytes(&receipt).map_err(|error| IntegrationError::Drift(error.to_string()))?;
    Ok(OperationReceipt::Backend { key, payload })
}

fn drift(error: impl std::fmt::Display) -> IntegrationError {
    IntegrationError::Drift(error.to_string())
}

fn native_reconcile(
    key: ResourceKey,
    result: Result<NativeReconcileResult, impl std::fmt::Display>,
) -> Result<ReconcileResult, IntegrationError> {
    match result.map_err(drift)? {
        NativeReconcileResult::AppliedWithReceipt(receipt) => Ok(
            ReconcileResult::AppliedWithReceipt(native_apply(key, *receipt)?),
        ),
        NativeReconcileResult::NotApplied => Ok(ReconcileResult::NotApplied),
        NativeReconcileResult::Ambiguous => Ok(ReconcileResult::Ambiguous),
    }
}

fn native_owned_receipt(
    key: ResourceKey,
    scope: SelectedScope,
    owned: &OwnedResource,
) -> Result<BackendReceipt, IntegrationError> {
    let privilege = owned.privilege();
    Ok(match owned {
        OwnedResource::Launcher {
            launcher_path,
            previous,
            installed,
            ..
        } => BackendReceipt::Launcher {
            launcher_path: launcher_path.clone(),
            privilege,
            previous: previous.clone(),
            installed: installed.clone(),
        },
        OwnedResource::PathEntry {
            value, value_type, ..
        } => BackendReceipt::Path {
            scope,
            entry: value.to_string(),
            value_type: value_type.clone(),
            privilege,
        },
        OwnedResource::Protocol {
            previous,
            installed,
            ..
        } => BackendReceipt::Protocol {
            scope,
            scheme: match &key {
                ResourceKey::Protocol { scheme } => scheme.to_string(),
                _ => String::new(),
            },
            privilege,
            previous: previous.clone(),
            installed: installed.clone(),
        },
        OwnedResource::FileAssociation {
            previous,
            installed,
            ..
        } => BackendReceipt::FileAssociation {
            scope,
            id: match &key {
                ResourceKey::FileAssociation { id } => id.to_string(),
                _ => String::new(),
            },
            privilege,
            previous: previous.clone(),
            installed: installed.clone(),
        },
        OwnedResource::Extension {
            previous,
            installed,
            ..
        } => BackendReceipt::Extension {
            scope,
            extension: match &key {
                ResourceKey::FileAssociationExtension { extension } => extension.to_string(),
                _ => String::new(),
            },
            privilege,
            previous: previous.clone(),
            installed: installed.clone(),
        },
        OwnedResource::Backend { .. } => return Err(IntegrationError::Unsupported),
        OwnedResource::Service(_) | OwnedResource::File { .. } => {
            return Err(IntegrationError::Unsupported);
        }
    })
}

fn invert_receipt(receipt: BackendReceipt) -> Result<BackendReceipt, IntegrationError> {
    Ok(match receipt {
        BackendReceipt::Launcher {
            launcher_path,
            privilege,
            previous,
            installed,
        } => BackendReceipt::Launcher {
            launcher_path,
            privilege,
            previous: installed,
            installed: previous,
        },
        BackendReceipt::Path {
            scope,
            entry,
            value_type,
            privilege,
        } => BackendReceipt::RemovePath {
            scope,
            entry,
            value_type,
            privilege,
        },
        BackendReceipt::RebindPath {
            scope,
            previous,
            installed,
            privilege,
        } => BackendReceipt::RebindPath {
            scope,
            previous: installed,
            installed: previous,
            privilege,
        },
        BackendReceipt::RemovePath {
            scope,
            entry,
            value_type,
            privilege,
        } => BackendReceipt::Path {
            scope,
            entry,
            value_type,
            privilege,
        },
        BackendReceipt::Protocol {
            scope,
            scheme,
            privilege,
            previous,
            installed,
        } => BackendReceipt::Protocol {
            scope,
            scheme,
            privilege,
            previous: installed,
            installed: previous,
        },
        BackendReceipt::FileAssociation {
            scope,
            id,
            privilege,
            previous,
            installed,
        } => BackendReceipt::FileAssociation {
            scope,
            id,
            privilege,
            previous: installed,
            installed: previous,
        },
        BackendReceipt::Extension {
            scope,
            extension,
            privilege,
            previous,
            installed,
        } => BackendReceipt::Extension {
            scope,
            extension,
            privilege,
            previous: installed,
            installed: previous,
        },
        BackendReceipt::AppsFeatures {
            scope,
            key_path,
            privilege,
            previous,
            installed,
        } => BackendReceipt::AppsFeatures {
            scope,
            key_path,
            privilege,
            previous: installed,
            installed: previous,
        },
    })
}

pub(crate) fn apply_owned_removal(
    node: &DeploymentNode,
) -> Result<OperationReceipt, IntegrationError> {
    let operation = node
        .meta
        .backend
        .as_ref()
        .ok_or(IntegrationError::Unsupported)?;
    let key = operation.key.clone();
    let payload: RemovePayload =
        decode(&operation.payload).map_err(|error| IntegrationError::Drift(error.to_string()))?;
    match payload {
        RemovePayload::Owned {
            scope,
            key: semantic_key,
            owned,
        } => {
            let original = native_owned_receipt(semantic_key, scope, &owned)?;
            rollback_native(&original)?;
            native_apply(key, invert_receipt(original)?)
        }
        RemovePayload::AppsFeatures {
            scope,
            key_path,
            state,
        } => {
            match read_apps_features(scope, &key_path)? {
                Some(current) if current == state => {
                    write_apps_features(scope, &key_path, None)?;
                }
                None => {}
                Some(_) => {
                    return Err(IntegrationError::Drift(
                        "Apps & Features registration changed".into(),
                    ));
                }
            }
            native_apply(
                key,
                BackendReceipt::AppsFeatures {
                    scope,
                    key_path,
                    privilege: operation.privilege,
                    previous: Some(state),
                    installed: None,
                },
            )
        }
    }
}

pub fn reconcile_owned_removal(node: &DeploymentNode) -> Result<ReconcileResult, IntegrationError> {
    let operation = node
        .meta
        .backend
        .as_ref()
        .ok_or(IntegrationError::Unsupported)?;
    let key = operation.key.clone();
    let payload: RemovePayload =
        decode(&operation.payload).map_err(|error| IntegrationError::Drift(error.to_string()))?;
    match payload {
        RemovePayload::Owned {
            scope,
            key: semantic_key,
            owned,
        } => {
            let original = native_owned_receipt(semantic_key, scope, &owned)?;
            let status = reconcile_native(&original)?;
            if status == ReconcileResult::Applied {
                Ok(ReconcileResult::AppliedWithReceipt(native_apply(
                    key,
                    invert_receipt(original)?,
                )?))
            } else {
                Ok(status)
            }
        }
        RemovePayload::AppsFeatures {
            scope,
            key_path,
            state,
        } => match read_apps_features(scope, &key_path)? {
            Some(current) if current == state => Ok(ReconcileResult::NotApplied),
            None => Ok(ReconcileResult::AppliedWithReceipt(native_apply(
                key,
                BackendReceipt::AppsFeatures {
                    scope,
                    key_path,
                    privilege: operation.privilege,
                    previous: Some(state),
                    installed: None,
                },
            )?)),
            Some(_) => Err(IntegrationError::Drift(
                "Apps & Features registration changed".into(),
            )),
        },
    }
}

pub(crate) fn rollback_managed(
    _node: &DeploymentNode,
    receipt: &OperationReceipt,
) -> Result<(), IntegrationError> {
    let OperationReceipt::Backend { payload, .. } = receipt else {
        return Err(IntegrationError::Unsupported);
    };
    let receipt =
        receipt_from_bytes(payload).map_err(|error| IntegrationError::Drift(error.to_string()))?;
    rollback_native(&receipt)
}

pub(crate) fn verify_managed(
    _node: &DeploymentNode,
    receipt: &OperationReceipt,
) -> Result<(), IntegrationError> {
    let OperationReceipt::Backend { payload, .. } = receipt else {
        return Err(IntegrationError::Unsupported);
    };
    let receipt =
        receipt_from_bytes(payload).map_err(|error| IntegrationError::Drift(error.to_string()))?;
    verify_native(&receipt)
}

fn verify_native(receipt: &BackendReceipt) -> Result<(), IntegrationError> {
    match receipt {
        BackendReceipt::Launcher {
            launcher_path,
            installed,
            ..
        } => {
            let observed = crate::shortcuts::read_shortcut(launcher_path).map_err(drift)?;
            expect(
                "launcher",
                &launcher_path.to_string(),
                &observed_launcher(observed)?,
                installed,
            )
        }
        BackendReceipt::Path {
            scope,
            entry,
            value_type,
            ..
        }
        | BackendReceipt::RemovePath {
            scope,
            entry,
            value_type,
            ..
        } => {
            let (current_type, raw) = read_path(*scope)?;
            if current_type != *value_type
                && !crate::search_path::lost_expansion(value_type, &current_type)
            {
                return Err(IntegrationError::Drift(
                    "search-path value type changed".into(),
                ));
            }
            let installed = search_path_entry_count(&raw, entry) == 1;
            let expected = matches!(receipt, BackendReceipt::Path { .. });
            if installed != expected {
                return Err(IntegrationError::Drift(format!(
                    "search path does not name `{entry}` as installed"
                )));
            }
            Ok(())
        }
        BackendReceipt::RebindPath {
            scope,
            previous,
            installed,
            ..
        } => {
            let (kind, raw) = read_path(*scope)?;
            if !path_type_matches(&previous.value_type, &kind)
                || path_binding_count(&raw, &previous.value) != 0
                || path_binding_count(&raw, &installed.value) != 1
            {
                return Err(IntegrationError::Drift(
                    "search-path replacement differs from its installed binding".into(),
                ));
            }
            Ok(())
        }
        BackendReceipt::Protocol {
            scope,
            scheme,
            previous,
            installed,
            ..
        } => {
            let target = protocol_target(installed)
                .or_else(|| protocol_target(previous))
                .ok_or_else(|| IntegrationError::Drift("protocol target is missing".into()))?;
            expect(
                "protocol",
                scheme,
                &read_protocol(*scope, scheme, target)?,
                installed,
            )
        }
        BackendReceipt::FileAssociation {
            scope,
            id,
            previous,
            installed,
            ..
        } => {
            let target = file_association_target(installed)
                .or_else(|| file_association_target(previous))
                .ok_or_else(|| {
                    IntegrationError::Drift("file association target is missing".into())
                })?;
            expect(
                "file association",
                id,
                &read_progid(*scope, id, target)?,
                installed,
            )
        }
        BackendReceipt::Extension {
            scope,
            extension,
            installed,
            ..
        } => expect(
            "extension",
            extension,
            &read_extension(*scope, extension)?,
            installed,
        ),
        BackendReceipt::AppsFeatures {
            scope,
            key_path,
            installed,
            ..
        } => expect(
            "Apps & Features registration",
            key_path,
            &read_apps_features(*scope, key_path)?,
            installed,
        ),
    }
}

fn expect<T: PartialEq + std::fmt::Debug>(
    what: &str,
    which: &str,
    found: &T,
    installed: &T,
) -> Result<(), IntegrationError> {
    if found == installed {
        Ok(())
    } else {
        Err(IntegrationError::Drift(format!(
            "{what} `{which}` is {found:?}, not the installed state {installed:?}"
        )))
    }
}

pub(crate) fn observed_launcher(
    observed: zup_plan::ObservedLauncherState,
) -> Result<LauncherState, IntegrationError> {
    Ok(match observed {
        zup_plan::ObservedLauncherState::Absent => LauncherState::Absent,
        zup_plan::ObservedLauncherState::Launcher {
            target,
            arguments,
            working_directory,
        } => LauncherState::Launcher {
            target,
            arguments,
            working_directory,
        },
        zup_plan::ObservedLauncherState::InvalidLauncher
        | zup_plan::ObservedLauncherState::NonFile => {
            return Err(IntegrationError::Drift("launcher is unreadable".into()));
        }
    })
}

pub(crate) fn observed_service(
    observed: zup_plan::ObservedServiceState,
) -> Result<ServiceState, IntegrationError> {
    observed
        .configuration()
        .ok_or_else(|| IntegrationError::Drift("service is malformed".into()))
}

fn rollback_native(receipt: &BackendReceipt) -> Result<(), IntegrationError> {
    match receipt {
        BackendReceipt::Launcher {
            launcher_path,
            previous,
            installed,
            ..
        } => crate::shortcuts::rollback(launcher_path, previous, installed).map_err(drift),
        BackendReceipt::Path {
            scope,
            entry,
            value_type,
            ..
        } => rollback_path(*scope, entry, value_type),
        BackendReceipt::RemovePath {
            scope,
            entry,
            value_type,
            ..
        } => restore_removed_path(*scope, entry, value_type),
        BackendReceipt::RebindPath {
            scope,
            previous,
            installed,
            ..
        } => rebind_path(*scope, installed, previous),
        BackendReceipt::Protocol {
            scope,
            scheme,
            previous,
            installed,
            ..
        } => {
            let target = protocol_target(installed)
                .or_else(|| protocol_target(previous))
                .ok_or(IntegrationError::Drift("protocol target is missing".into()))?;
            if read_protocol(*scope, scheme, target)? != *installed {
                return Err(IntegrationError::Drift(format!("protocol {scheme}")));
            }
            write_protocol(*scope, scheme, previous)
        }
        BackendReceipt::FileAssociation {
            scope,
            id,
            previous,
            installed,
            ..
        } => {
            let target = file_association_target(installed)
                .or_else(|| file_association_target(previous))
                .ok_or(IntegrationError::Drift(
                    "file association target is missing".into(),
                ))?;
            if read_progid(*scope, id, target)? != *installed {
                return Err(IntegrationError::Drift(format!("file association {id}")));
            }
            write_progid(*scope, id, previous)
        }
        BackendReceipt::Extension {
            scope,
            extension,
            previous,
            installed,
            ..
        } => {
            if read_extension(*scope, extension)? != *installed {
                return Err(IntegrationError::Drift(format!("extension {extension}")));
            }
            write_extension(*scope, extension, previous)
        }
        BackendReceipt::AppsFeatures {
            scope,
            key_path,
            previous,
            installed,
            ..
        } => {
            if read_apps_features(*scope, key_path)? != installed.clone() {
                return Err(IntegrationError::Drift(
                    "Apps & Features registration changed".into(),
                ));
            }
            write_apps_features(*scope, key_path, previous.as_ref())
        }
    }
}

fn reconcile_native(receipt: &BackendReceipt) -> Result<ReconcileResult, IntegrationError> {
    match receipt {
        BackendReceipt::Launcher {
            launcher_path,
            previous,
            installed,
            ..
        } => match crate::shortcuts::read_shortcut(launcher_path) {
            Ok(zup_plan::ObservedLauncherState::Absent) => {
                Ok(compare_removal(&LauncherState::Absent, previous, installed))
            }
            Ok(zup_plan::ObservedLauncherState::Launcher {
                target,
                arguments,
                working_directory,
            }) => Ok(compare_removal(
                &LauncherState::Launcher {
                    target,
                    arguments,
                    working_directory,
                },
                previous,
                installed,
            )),
            _ => Ok(ReconcileResult::Ambiguous),
        },
        BackendReceipt::Path {
            scope,
            entry,
            value_type,
            ..
        }
        | BackendReceipt::RemovePath {
            scope,
            entry,
            value_type,
            ..
        } => match read_path(*scope) {
            Ok((ty, raw))
                if ty == *value_type || crate::search_path::lost_expansion(value_type, &ty) =>
            {
                match search_path_entry_count(&raw, entry) {
                    0 => Ok(ReconcileResult::Applied),
                    1 => Ok(ReconcileResult::NotApplied),
                    _ => Ok(ReconcileResult::Ambiguous),
                }
            }
            _ => Ok(ReconcileResult::Ambiguous),
        },
        BackendReceipt::RebindPath {
            scope,
            previous,
            installed,
            ..
        } => {
            let (kind, raw) = read_path(*scope)?;
            Ok(if !path_type_matches(&previous.value_type, &kind) {
                ReconcileResult::Ambiguous
            } else {
                match (
                    path_binding_count(&raw, &previous.value),
                    path_binding_count(&raw, &installed.value),
                ) {
                    (1, 0) => ReconcileResult::Applied,
                    (0, 1) => ReconcileResult::NotApplied,
                    _ => ReconcileResult::Ambiguous,
                }
            })
        }
        BackendReceipt::Protocol {
            scope,
            scheme,
            previous,
            installed,
            ..
        } => {
            let Some(target) = protocol_target(installed).or_else(|| protocol_target(previous))
            else {
                return Ok(ReconcileResult::Ambiguous);
            };
            match read_protocol(*scope, scheme, target) {
                Ok(current) => Ok(compare_removal(&current, previous, installed)),
                Err(_) => Ok(ReconcileResult::Ambiguous),
            }
        }
        BackendReceipt::FileAssociation {
            scope,
            id,
            previous,
            installed,
            ..
        } => {
            let Some(target) =
                file_association_target(installed).or_else(|| file_association_target(previous))
            else {
                return Ok(ReconcileResult::Ambiguous);
            };
            match read_progid(*scope, id, target) {
                Ok(current) => Ok(compare_removal(&current, previous, installed)),
                Err(_) => Ok(ReconcileResult::Ambiguous),
            }
        }
        BackendReceipt::Extension {
            scope,
            extension,
            previous,
            installed,
            ..
        } => match read_extension(*scope, extension) {
            Ok(current) => Ok(compare_removal(&current, previous, installed)),
            Err(_) => Ok(ReconcileResult::Ambiguous),
        },
        BackendReceipt::AppsFeatures {
            scope,
            key_path,
            previous,
            installed,
            ..
        } => match read_apps_features(*scope, key_path) {
            Ok(current) if current == *previous => Ok(ReconcileResult::Applied),
            Ok(current) if current == *installed => Ok(ReconcileResult::NotApplied),
            _ => Ok(ReconcileResult::Ambiguous),
        },
    }
}

fn protocol_target(state: &ProtocolState) -> Option<&Target> {
    match state {
        ProtocolState::Registration { command, .. } => Some(command.executable.target()),
        ProtocolState::Absent => None,
    }
}

fn file_association_target(state: &FileAssociationState) -> Option<&Target> {
    match state {
        FileAssociationState::Registration { command, .. } => Some(command.executable.target()),
        FileAssociationState::Absent => None,
    }
}

fn compare_removal<T: PartialEq>(current: &T, previous: &T, installed: &T) -> ReconcileResult {
    if current == previous {
        ReconcileResult::Applied
    } else if current == installed {
        ReconcileResult::NotApplied
    } else {
        ReconcileResult::Ambiguous
    }
}

pub fn reconcile_managed(node: &DeploymentNode) -> Result<ReconcileResult, IntegrationError> {
    let Some(operation) = node.meta.backend.as_ref() else {
        return Err(IntegrationError::Unsupported);
    };
    let key = operation.key.clone();
    match decode::<ApplyPayload>(&operation.payload)
        .map_err(|error| IntegrationError::Drift(error.to_string()))?
    {
        ApplyPayload::Launcher(op) => native_reconcile(key, crate::shortcuts::reconcile(&op)),
        ApplyPayload::Path(op) => {
            let (value_type, raw) = match read_path(op.scope) {
                Ok(value) => value,
                Err(_) => return Ok(ReconcileResult::Ambiguous),
            };
            reconcile_path_binding(key, &op, &value_type, &raw)
        }
        ApplyPayload::Protocol(op) => {
            let current =
                match read_protocol(op.scope, op.scheme.as_str(), op.command.executable.target()) {
                    Ok(value) => value,
                    Err(_) => return Ok(ReconcileResult::Ambiguous),
                };
            let previous = protocol_from_observed(&op.previous)?;
            let installed = ProtocolState::Registration {
                command: op.command.clone(),
            };
            if current == installed {
                native_reconcile(
                    key,
                    Ok::<_, IntegrationError>(NativeReconcileResult::AppliedWithReceipt(Box::new(
                        BackendReceipt::Protocol {
                            scope: op.scope,
                            scheme: op.scheme.to_string(),
                            privilege: op.privilege,
                            previous,
                            installed,
                        },
                    ))),
                )
            } else {
                Ok(ReconcileResult::NotApplied)
            }
        }
        ApplyPayload::FileAssociation(op) => {
            let current = match read_progid(op.scope, &op.id, op.command.executable.target()) {
                Ok(value) => value,
                Err(_) => return Ok(ReconcileResult::Ambiguous),
            };
            let previous = file_association_from_observed(&op.previous_association)?;
            let installed = FileAssociationState::Registration {
                description: op.description.clone(),
                command: op.command.clone(),
            };
            if current == installed {
                native_reconcile(
                    key,
                    Ok::<_, IntegrationError>(NativeReconcileResult::AppliedWithReceipt(Box::new(
                        BackendReceipt::FileAssociation {
                            scope: op.scope,
                            id: op.id.clone(),
                            privilege: op.privilege,
                            previous,
                            installed,
                        },
                    ))),
                )
            } else {
                Ok(ReconcileResult::NotApplied)
            }
        }
        ApplyPayload::Extension(op) => {
            let current = match read_extension(op.scope, &op.extension) {
                Ok(value) => value,
                Err(_) => return Ok(ReconcileResult::Ambiguous),
            };
            let previous = extension_from_observed(&op.previous_extension)?;
            let installed = ExtensionState::Mapped {
                association_id: op.id.clone(),
            };
            if current == installed {
                native_reconcile(
                    key,
                    Ok::<_, IntegrationError>(NativeReconcileResult::AppliedWithReceipt(Box::new(
                        BackendReceipt::Extension {
                            scope: op.scope,
                            extension: op.extension.clone(),
                            privilege: op.privilege,
                            previous,
                            installed,
                        },
                    ))),
                )
            } else {
                Ok(ReconcileResult::NotApplied)
            }
        }
        ApplyPayload::AppsFeatures { operation } => {
            let current = match read_apps_features(operation.scope, &operation.key_path) {
                Ok(value) => value,
                Err(_) => return Ok(ReconcileResult::Ambiguous),
            };
            if current == Some(operation.installed.clone()) {
                native_reconcile(
                    key,
                    Ok::<_, IntegrationError>(NativeReconcileResult::AppliedWithReceipt(Box::new(
                        BackendReceipt::AppsFeatures {
                            scope: operation.scope,
                            key_path: operation.key_path,
                            privilege: operation.privilege,
                            previous: operation.previous,
                            installed: Some(operation.installed),
                        },
                    ))),
                )
            } else if current == operation.previous {
                Ok(ReconcileResult::NotApplied)
            } else {
                Ok(ReconcileResult::Ambiguous)
            }
        }
    }
}

fn reconcile_path_binding(
    key: ResourceKey,
    op: &zup_plan::PathOperation,
    value_type: &str,
    raw: &str,
) -> Result<ReconcileResult, IntegrationError> {
    let count = path_binding_count(raw, &op.value);
    match &op.precondition {
        zup_plan::PathPrecondition::Unclaimed { .. } => Ok(if count == 0 {
            ReconcileResult::NotApplied
        } else {
            ReconcileResult::Ambiguous
        }),
        zup_plan::PathPrecondition::Owned { binding, .. } => {
            if !path_type_matches(&binding.value_type, value_type) {
                return Ok(ReconcileResult::Ambiguous);
            }
            if op.kind == zup_plan::PathOperationKind::RebindOwned {
                return Ok(
                    if path_binding_count(raw, &binding.value) == 1 && count == 0 {
                        ReconcileResult::NotApplied
                    } else {
                        ReconcileResult::Ambiguous
                    },
                );
            }
            match count {
                0 => Ok(ReconcileResult::NotApplied),
                1 => native_apply(
                    key,
                    BackendReceipt::Path {
                        scope: op.scope,
                        entry: op.value.to_string(),
                        value_type: binding.value_type.clone(),
                        privilege: op.privilege,
                    },
                )
                .map(ReconcileResult::AppliedWithReceipt),
                _ => Ok(ReconcileResult::Ambiguous),
            }
        }
    }
}

fn environment_lock(
    scope: SelectedScope,
) -> Result<zup_transaction::ResourceLock, IntegrationError> {
    let key = "native-environment-path";
    let lease = match scope {
        SelectedScope::Machine => {
            crate::machine_authority::MachineRoot::shared_lock(key).map_err(drift)?
        }
        SelectedScope::User => zup_transaction::ResourceLock::try_acquire(
            &crate::host_dirs::user_data()
                .map_err(drift)?
                .join("zup-native-resources"),
            key,
        )
        .map_err(drift)?,
    };
    lease.ok_or_else(|| {
        IntegrationError::Drift(
            "another Zup transaction is changing this scope's PATH; retry after it completes"
                .into(),
        )
    })
}

fn path_type_matches(previous: &str, current: &str) -> bool {
    current == previous || crate::search_path::lost_expansion(previous, current)
}

fn path_binding_count(raw: &str, value: &zup_core::TargetPath) -> usize {
    crate::search_path::split(raw)
        .into_iter()
        .filter_map(|part| crate::search_path::to_target_path(value.target(), part))
        .filter(|part| part.equivalent(value))
        .count()
}

fn rebind_value(
    raw: &str,
    previous: &zup_core::TargetPath,
    installed: &zup_core::TargetPath,
) -> Result<String, IntegrationError> {
    if previous.equivalent(installed)
        || path_binding_count(raw, previous) != 1
        || path_binding_count(raw, installed) != 0
    {
        return Err(IntegrationError::Drift(
            "owned PATH binding is missing, duplicated, or its replacement is occupied".into(),
        ));
    }
    Ok(raw
        .split(';')
        .map(|part| {
            if crate::search_path::to_target_path(previous.target(), part)
                .is_some_and(|part| part.equivalent(previous))
            {
                installed.to_string()
            } else {
                part.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join(";"))
}

fn rebind_path(
    scope: SelectedScope,
    previous: &zup_plan::PathBinding,
    installed: &zup_plan::PathBinding,
) -> Result<(), IntegrationError> {
    let _lease = environment_lock(scope)?;
    let key = environment_key(scope, EnvironmentAccess::ModifyExisting)?
        .ok_or_else(|| IntegrationError::Drift("search-path key is missing".into()))?;
    rebind_path_key(&key, previous, installed)
}

fn rebind_path_key(
    key: &Key,
    previous: &zup_plan::PathBinding,
    installed: &zup_plan::PathBinding,
) -> Result<(), IntegrationError> {
    let (kind, raw) = read_path_from_key(key)?;
    if !path_type_matches(&previous.value_type, &kind) {
        return Err(IntegrationError::Drift(
            "search-path value type changed".into(),
        ));
    }
    let next = rebind_value(&raw, &previous.value, &installed.value)?;
    set_path(key, &kind, &next)?;
    if read_path_from_key(key)? != (kind, next) {
        return Err(IntegrationError::Drift(
            "search-path write outcome changed during readback; recovery is required".into(),
        ));
    }
    Ok(())
}

fn apply_path(
    key: &ResourceKey,
    op: &zup_plan::PathOperation,
) -> Result<OperationReceipt, IntegrationError> {
    if op.kind == zup_plan::PathOperationKind::RebindOwned {
        let zup_plan::PathPrecondition::Owned {
            binding,
            previous: zup_plan::PathPresence::Single,
            desired: zup_plan::PathPresence::Missing,
        } = &op.precondition
        else {
            return Err(IntegrationError::Drift(
                "PATH rebinding lacks an exact owned before-image".into(),
            ));
        };
        let installed = zup_plan::PathBinding {
            value: op.value.clone(),
            value_type: binding.value_type.clone(),
        };
        rebind_path(op.scope, binding, &installed)?;
        return native_apply(
            key.clone(),
            BackendReceipt::RebindPath {
                scope: op.scope,
                previous: binding.clone(),
                installed,
                privilege: op.privilege,
            },
        );
    }
    let _lease = environment_lock(op.scope)?;
    let registry_key = environment_key(op.scope, EnvironmentAccess::Insert)?
        .ok_or_else(|| IntegrationError::Registry("environment key".into()))?;
    let (current_type, raw) = read_path_from_key(&registry_key)?;
    if path_binding_count(&raw, &op.value) != 0 {
        return Err(IntegrationError::Drift(
            "search-path entry appeared after planning".into(),
        ));
    }
    let value_type = match (&op.kind, &op.precondition) {
        (
            zup_plan::PathOperationKind::Add,
            zup_plan::PathPrecondition::Unclaimed {
                presence: zup_plan::PathPresence::Missing,
            },
        ) => current_type.clone(),
        (
            zup_plan::PathOperationKind::RestoreOwned,
            zup_plan::PathPrecondition::Owned {
                binding,
                previous: zup_plan::PathPresence::Missing,
                desired: zup_plan::PathPresence::Missing,
            },
        ) if binding.value.equivalent(&op.value)
            && (current_type == crate::search_path::VALUE_TYPE_MISSING
                || path_type_matches(&binding.value_type, &current_type)) =>
        {
            binding.value_type.clone()
        }
        _ => {
            return Err(IntegrationError::Drift(
                "search-path insertion does not match its authorized before-image".into(),
            ));
        }
    };
    let entry = op.value.to_string();
    let next = append_search_path_entry(&raw, &entry);
    let kind = crate::search_path::write_value_type(&value_type);
    set_path(&registry_key, kind, &next)?;
    if read_path_from_key(&registry_key)? != (kind.to_owned(), next) {
        return Err(IntegrationError::Drift(
            "search-path write outcome changed during readback; recovery is required".into(),
        ));
    }
    native_apply(
        key.clone(),
        BackendReceipt::Path {
            scope: op.scope,
            entry,
            value_type,
            privilege: op.privilege,
        },
    )
}

fn append_search_path_entry(raw: &str, entry: &str) -> String {
    if raw.is_empty() {
        entry.to_owned()
    } else if raw.ends_with(';') {
        format!("{raw}{entry}")
    } else {
        format!("{raw};{entry}")
    }
}

fn search_path_entry_count(raw: &str, entry: &str) -> usize {
    crate::search_path::split(raw)
        .into_iter()
        .filter(|segment| *segment == entry)
        .count()
}

fn rollback_path(
    scope: SelectedScope,
    entry: &str,
    value_type: &str,
) -> Result<(), IntegrationError> {
    let _lease = environment_lock(scope)?;
    let Some(key) = environment_key(scope, EnvironmentAccess::ModifyExisting)? else {
        return Err(IntegrationError::Drift("search-path key missing".into()));
    };
    let (current_type, raw) = read_path_from_key(&key)?;
    if current_type != value_type && !crate::search_path::lost_expansion(value_type, &current_type)
    {
        return Err(IntegrationError::Drift(
            "search-path value type changed".into(),
        ));
    }
    if search_path_entry_count(&raw, entry) != 1 {
        return Err(IntegrationError::Drift(
            "installed search-path entry is missing or duplicated".into(),
        ));
    }
    let mut removed = false;
    let kept: Vec<&str> = crate::search_path::split(&raw)
        .into_iter()
        .filter(|part| {
            if !removed && *part == entry {
                removed = true;
                false
            } else {
                true
            }
        })
        .collect();
    if !removed {
        return Err(IntegrationError::Drift(
            "installed search-path entry changed or disappeared".into(),
        ));
    }
    if value_type == crate::search_path::VALUE_TYPE_MISSING && kept.is_empty() {
        key.remove_value(crate::search_path::PATH_VALUE_NAME)
            .or_else(ignore_missing)
            .map_err(regerr)?;
    } else {
        set_path(
            &key,
            crate::search_path::write_value_type(value_type),
            &kept.join(";"),
        )?;
    }
    broadcast_environment_change();
    Ok(())
}

fn restore_removed_path(
    scope: SelectedScope,
    entry: &str,
    value_type: &str,
) -> Result<(), IntegrationError> {
    let _lease = environment_lock(scope)?;
    let key = environment_key(scope, EnvironmentAccess::Insert)?
        .ok_or_else(|| IntegrationError::Drift("search-path key missing".into()))?;
    let (current_type, raw) = read_path_from_key(&key)?;
    if current_type != value_type && !crate::search_path::lost_expansion(value_type, &current_type)
    {
        return Err(IntegrationError::Drift(
            "search-path value type changed after removal".into(),
        ));
    }
    if search_path_entry_count(&raw, entry) > 0 {
        return Err(IntegrationError::Drift(
            "removed search-path entry appeared again".into(),
        ));
    }
    let next = append_search_path_entry(&raw, entry);
    set_path(
        &key,
        crate::search_path::write_value_type(&current_type),
        &next,
    )?;
    broadcast_environment_change();
    Ok(())
}

fn apply_protocol(
    key: &ResourceKey,
    op: &zup_plan::ProtocolOperation,
) -> Result<OperationReceipt, IntegrationError> {
    let previous = protocol_from_observed(&op.previous)?;
    if read_protocol(op.scope, op.scheme.as_str(), op.command.executable.target())? != previous {
        return Err(IntegrationError::Drift(format!("protocol {}", op.scheme)));
    }
    let installed = ProtocolState::Registration {
        command: op.command.clone(),
    };
    write_protocol(op.scope, op.scheme.as_str(), &installed)?;
    native_apply(
        key.clone(),
        BackendReceipt::Protocol {
            scope: op.scope,
            scheme: op.scheme.to_string(),
            privilege: op.privilege,
            previous,
            installed,
        },
    )
}

fn apply_progid(
    key: &ResourceKey,
    op: &zup_plan::FileAssociationOperation,
) -> Result<OperationReceipt, IntegrationError> {
    let previous = file_association_from_observed(&op.previous_association)?;
    if read_progid(op.scope, &op.id, op.command.executable.target())? != previous {
        return Err(IntegrationError::Drift(format!(
            "file association {}",
            op.id
        )));
    }
    let installed = FileAssociationState::Registration {
        description: op.description.clone(),
        command: op.command.clone(),
    };
    write_progid(op.scope, &op.id, &installed)?;
    native_apply(
        key.clone(),
        BackendReceipt::FileAssociation {
            scope: op.scope,
            id: op.id.clone(),
            privilege: op.privilege,
            previous,
            installed,
        },
    )
}

fn apply_extension(
    key: &ResourceKey,
    op: &zup_plan::FileAssociationOperation,
) -> Result<OperationReceipt, IntegrationError> {
    let previous = extension_from_observed(&op.previous_extension)?;
    if read_extension(op.scope, &op.extension)? != previous {
        return Err(IntegrationError::Drift(format!(
            "extension {}",
            op.extension
        )));
    }
    let installed = ExtensionState::Mapped {
        association_id: op.id.clone(),
    };
    write_extension(op.scope, &op.extension, &installed)?;
    native_apply(
        key.clone(),
        BackendReceipt::Extension {
            scope: op.scope,
            extension: op.extension.clone(),
            privilege: op.privilege,
            previous,
            installed,
        },
    )
}

fn apply_apps(
    key: &ResourceKey,
    operation: &AppsFeaturesOperation,
) -> Result<OperationReceipt, IntegrationError> {
    if read_apps_features(operation.scope, &operation.key_path)? != operation.previous {
        return Err(IntegrationError::Drift(
            "Apps & Features registration changed".into(),
        ));
    }
    write_apps_features(
        operation.scope,
        &operation.key_path,
        Some(&operation.installed),
    )?;
    native_apply(
        key.clone(),
        BackendReceipt::AppsFeatures {
            scope: operation.scope,
            key_path: operation.key_path.clone(),
            privilege: operation.privilege,
            previous: operation.previous.clone(),
            installed: Some(operation.installed.clone()),
        },
    )
}

fn classes(scope: SelectedScope, create: bool) -> Result<Key, IntegrationError> {
    let root = match scope {
        SelectedScope::User => &CURRENT_USER,
        SelectedScope::Machine => &LOCAL_MACHINE,
    };
    let path = "Software\\Classes";
    (if create {
        root.create(path)
    } else {
        root.open(path)
    })
    .map_err(regerr)
}

pub(crate) fn read_apps_features(
    scope: SelectedScope,
    key_path: &str,
) -> Result<Option<AppsFeaturesState>, IntegrationError> {
    let root = match scope {
        SelectedScope::User => &CURRENT_USER,
        SelectedScope::Machine => &LOCAL_MACHINE,
    };
    let Some(key) = open_optional(root, key_path, false)? else {
        return Ok(None);
    };
    if key.keys().map_err(regerr)?.next().is_some() {
        return Err(IntegrationError::Drift(
            "registration key has child keys".into(),
        ));
    }
    let mut values = BTreeMap::new();
    for (name, raw) in key.values().map_err(regerr)? {
        let value = match raw.ty() {
            Type::String => AppsFeaturesValue::String(String::try_from(raw).map_err(regerr)?),
            Type::U32 => AppsFeaturesValue::Dword(u32::try_from(raw).map_err(regerr)?),
            _ => {
                return Err(IntegrationError::Drift(
                    "unsupported registration value type".into(),
                ));
            }
        };
        values.insert(name, value);
    }
    Ok(Some(AppsFeaturesState { values }))
}

pub fn inspect_uninstall_registration(
    scope: SelectedScope,
    app_id: &str,
) -> Result<Option<AppsFeaturesState>, IntegrationError> {
    let key_path = format!("Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\{app_id}");
    read_apps_features(scope, &key_path)
}

fn write_apps_features(
    scope: SelectedScope,
    key_path: &str,
    state: Option<&AppsFeaturesState>,
) -> Result<(), IntegrationError> {
    let root = match scope {
        SelectedScope::User => &CURRENT_USER,
        SelectedScope::Machine => &LOCAL_MACHINE,
    };
    let Some(state) = state else {
        if let Some(key) = open_optional(root, key_path, true)? {
            if key.keys().map_err(regerr)?.next().is_some() {
                return Err(IntegrationError::Drift(
                    "registration key gained child keys".into(),
                ));
            }
            root.remove_tree(key_path)
                .or_else(ignore_missing)
                .map_err(regerr)?;
        }
        return Ok(());
    };
    let key = root.create(key_path).map_err(regerr)?;
    let names = key
        .values()
        .map_err(regerr)?
        .map(|(name, _)| name)
        .collect::<Vec<_>>();
    for name in names {
        key.remove_value(&name)
            .or_else(ignore_missing)
            .map_err(regerr)?;
    }
    for (name, value) in &state.values {
        match value {
            AppsFeaturesValue::String(value) => key.set_string(name, value),
            AppsFeaturesValue::Dword(value) => key.set_u32(name, *value),
        }
        .map_err(regerr)?;
    }
    Ok(())
}

enum EnvironmentAccess {
    Read,
    Insert,
    ModifyExisting,
}

fn environment_key(
    scope: SelectedScope,
    access: EnvironmentAccess,
) -> Result<Option<Key>, IntegrationError> {
    let (root, path) = match scope {
        SelectedScope::User => (&CURRENT_USER, "Environment"),
        SelectedScope::Machine => (
            &LOCAL_MACHINE,
            "SYSTEM\\CurrentControlSet\\Control\\Session Manager\\Environment",
        ),
    };
    let result = match access {
        EnvironmentAccess::Read => root.open(path),
        EnvironmentAccess::Insert => root.create(path),
        EnvironmentAccess::ModifyExisting => root.options().read().write().open(path),
    };
    match result {
        Ok(key) => Ok(Some(key)),
        Err(error) if !matches!(access, EnvironmentAccess::Insert) && is_not_found(&error) => {
            Ok(None)
        }
        Err(error) => Err(regerr(error)),
    }
}

pub(crate) fn read_path(scope: SelectedScope) -> Result<(String, String), IntegrationError> {
    match environment_key(scope, EnvironmentAccess::Read)? {
        Some(key) => read_path_from_key(&key),
        None => Ok((crate::search_path::VALUE_TYPE_MISSING.into(), String::new())),
    }
}

fn read_path_from_key(key: &Key) -> Result<(String, String), IntegrationError> {
    match key.get_value(crate::search_path::PATH_VALUE_NAME) {
        Ok(value) => {
            let kind = match value.ty() {
                Type::String => crate::search_path::VALUE_TYPE_PLAIN,
                Type::ExpandString => crate::search_path::VALUE_TYPE_EXPAND,
                _ => {
                    return Err(IntegrationError::Drift(
                        "search path has an unsupported host type".into(),
                    ));
                }
            };
            Ok((kind.into(), String::try_from(value).map_err(regerr)?))
        }
        Err(error) if is_not_found(&error) => {
            Ok((crate::search_path::VALUE_TYPE_MISSING.into(), String::new()))
        }
        Err(error) => Err(regerr(error)),
    }
}

fn set_path(key: &Key, value_type: &str, value: &str) -> Result<(), IntegrationError> {
    let name = crate::search_path::PATH_VALUE_NAME;
    match value_type {
        crate::search_path::VALUE_TYPE_PLAIN => key.set_string(name, value),
        crate::search_path::VALUE_TYPE_EXPAND => key.set_expand_string(name, value),
        _ => {
            return Err(IntegrationError::Drift(
                "search path has an unsupported host type".into(),
            ));
        }
    }
    .map_err(regerr)
}

pub(crate) fn read_protocol(
    scope: SelectedScope,
    scheme: &str,
    target: &Target,
) -> Result<ProtocolState, IntegrationError> {
    let root = classes(scope, false)?;
    let Some(key) = open_optional(&root, scheme, false)? else {
        return Ok(ProtocolState::Absent);
    };
    let marker = match key.get_value("URL Protocol") {
        Ok(_) => true,
        Err(error) if is_not_found(&error) => false,
        Err(error) => return Err(regerr(error)),
    };
    let raw = match open_optional(&key, "shell\\open\\command", false)? {
        Some(command) => read_optional_string(&command, "")?,
        None => None,
    };
    if !marker && raw.is_none() {
        return if registration_tree_empty(&key)? {
            Ok(ProtocolState::Absent)
        } else {
            Err(IntegrationError::Drift(
                "protocol has unowned content".into(),
            ))
        };
    }
    if !marker {
        return Err(IntegrationError::Drift("protocol marker missing".into()));
    }
    let raw = raw.ok_or_else(|| IntegrationError::Drift("protocol command missing".into()))?;
    let command = command_spec_from_command_line(&raw, target).map_err(IntegrationError::Drift)?;
    Ok(ProtocolState::Registration { command })
}

fn write_protocol(
    scope: SelectedScope,
    scheme: &str,
    state: &ProtocolState,
) -> Result<(), IntegrationError> {
    let root = classes(scope, true)?;
    match state {
        ProtocolState::Absent => {
            if let Some(key) = open_optional(&root, scheme, true)? {
                key.remove_value("URL Protocol")
                    .or_else(ignore_missing)
                    .map_err(regerr)?;
                if let Some(command) = open_optional(&key, "shell\\open\\command", true)? {
                    command
                        .remove_value("")
                        .or_else(ignore_missing)
                        .map_err(regerr)?;
                }
                if registration_tree_empty(&key)? {
                    root.remove_tree(scheme)
                        .or_else(ignore_missing)
                        .map_err(regerr)?;
                }
            }
            Ok(())
        }
        ProtocolState::Registration { command } => {
            let key = root.create(scheme).map_err(regerr)?;
            key.set_string("URL Protocol", "").map_err(regerr)?;
            key.create("shell\\open\\command")
                .and_then(|command_key| command_key.set_string("", format_spec(command)))
                .map_err(regerr)
        }
    }
}

pub(crate) fn read_progid(
    scope: SelectedScope,
    id: &str,
    target: &Target,
) -> Result<FileAssociationState, IntegrationError> {
    let root = classes(scope, false)?;
    let Some(key) = open_optional(&root, id, false)? else {
        return Ok(FileAssociationState::Absent);
    };
    let description =
        read_optional_string(&key, "FriendlyTypeName")?.or(read_optional_string(&key, "")?);
    let raw = match open_optional(&key, "shell\\open\\command", false)? {
        Some(command) => read_optional_string(&command, "")?,
        None => None,
    };
    if description.is_none() && raw.is_none() {
        return if registration_tree_empty(&key)? {
            Ok(FileAssociationState::Absent)
        } else {
            Err(IntegrationError::Drift(
                "file association has unowned content".into(),
            ))
        };
    }
    let raw =
        raw.ok_or_else(|| IntegrationError::Drift("file association command missing".into()))?;
    let command = command_spec_from_command_line(&raw, target).map_err(IntegrationError::Drift)?;
    Ok(FileAssociationState::Registration {
        description,
        command,
    })
}

fn write_progid(
    scope: SelectedScope,
    id: &str,
    state: &FileAssociationState,
) -> Result<(), IntegrationError> {
    let root = classes(scope, true)?;
    match state {
        FileAssociationState::Absent => {
            if let Some(key) = open_optional(&root, id, true)? {
                key.remove_value("FriendlyTypeName")
                    .or_else(ignore_missing)
                    .map_err(regerr)?;
                if let Some(command) = open_optional(&key, "shell\\open\\command", true)? {
                    command
                        .remove_value("")
                        .or_else(ignore_missing)
                        .map_err(regerr)?;
                }
                if registration_tree_empty(&key)? {
                    root.remove_tree(id)
                        .or_else(ignore_missing)
                        .map_err(regerr)?;
                }
            }
            Ok(())
        }
        FileAssociationState::Registration {
            description,
            command,
        } => {
            let key = root.create(id).map_err(regerr)?;
            if let Some(description) = description {
                key.set_string("FriendlyTypeName", description)
                    .map_err(regerr)?;
            } else {
                key.remove_value("FriendlyTypeName")
                    .or_else(ignore_missing)
                    .map_err(regerr)?;
            }
            key.create("shell\\open\\command")
                .and_then(|command_key| command_key.set_string("", format_spec(command)))
                .map_err(regerr)
        }
    }
}

pub(crate) fn read_extension(
    scope: SelectedScope,
    extension: &str,
) -> Result<ExtensionState, IntegrationError> {
    let root = classes(scope, false)?;
    let Some(key) = open_optional(&root, extension, false)? else {
        return Ok(ExtensionState::Absent);
    };
    match key.get_value("") {
        Ok(value) if matches!(value.ty(), Type::String | Type::ExpandString) => {
            Ok(ExtensionState::Mapped {
                association_id: String::try_from(value).map_err(regerr)?,
            })
        }
        Ok(_) => Err(IntegrationError::Drift(
            "extension value has an unsupported type".into(),
        )),
        Err(error) if is_not_found(&error) => {
            if registration_tree_empty(&key)? {
                Ok(ExtensionState::Absent)
            } else {
                Err(IntegrationError::Drift(
                    "extension has unowned content".into(),
                ))
            }
        }
        Err(error) => Err(regerr(error)),
    }
}

fn write_extension(
    scope: SelectedScope,
    extension: &str,
    state: &ExtensionState,
) -> Result<(), IntegrationError> {
    let root = classes(scope, true)?;
    match state {
        ExtensionState::Absent => {
            if let Some(key) = open_optional(&root, extension, true)? {
                key.remove_value("")
                    .or_else(ignore_missing)
                    .map_err(regerr)?;
                if registration_tree_empty(&key)? {
                    root.remove_tree(extension)
                        .or_else(ignore_missing)
                        .map_err(regerr)?;
                }
            }
            Ok(())
        }
        ExtensionState::Mapped { association_id } => root
            .create(extension)
            .and_then(|key| key.set_string("", association_id))
            .map_err(regerr),
    }
}

pub(crate) fn protocol_from_observed(
    value: &zup_plan::ObservedProtocolState,
) -> Result<ProtocolState, IntegrationError> {
    match value {
        zup_plan::ObservedProtocolState::Absent => Ok(ProtocolState::Absent),
        zup_plan::ObservedProtocolState::Registration {
            command,
            url_protocol_marker: true,
        } => Ok(ProtocolState::Registration {
            command: command.clone(),
        }),
        _ => Err(IntegrationError::Drift(
            "invalid protocol precondition".into(),
        )),
    }
}

pub(crate) fn file_association_from_observed(
    value: &zup_plan::ObservedFileAssociationState,
) -> Result<FileAssociationState, IntegrationError> {
    match value {
        zup_plan::ObservedFileAssociationState::Absent => Ok(FileAssociationState::Absent),
        zup_plan::ObservedFileAssociationState::Registration {
            description,
            command,
        } => Ok(FileAssociationState::Registration {
            description: description.clone(),
            command: command.clone(),
        }),
        _ => Err(IntegrationError::Drift(
            "invalid file association precondition".into(),
        )),
    }
}

pub(crate) fn extension_from_observed(
    value: &zup_plan::ObservedExtensionState,
) -> Result<ExtensionState, IntegrationError> {
    match value {
        zup_plan::ObservedExtensionState::Absent => Ok(ExtensionState::Absent),
        zup_plan::ObservedExtensionState::Mapped { association_id } => Ok(ExtensionState::Mapped {
            association_id: association_id.clone(),
        }),
        _ => Err(IntegrationError::Drift(
            "invalid extension precondition".into(),
        )),
    }
}

fn format_spec(command: &zup_core::CommandSpec) -> String {
    format_command_line(&host_path(&command.executable), &command.arguments)
}

fn regerr(error: windows_result::Error) -> IntegrationError {
    IntegrationError::Registry(error.message().to_owned())
}

fn is_not_found(error: &windows_result::Error) -> bool {
    error.code().0 as u32 == 0x8007_0002
}

fn open_optional(parent: &Key, path: &str, write: bool) -> Result<Option<Key>, IntegrationError> {
    let result = if write {
        parent.options().read().write().open(path)
    } else {
        parent.open(path)
    };
    match result {
        Ok(key) => Ok(Some(key)),
        Err(error) if is_not_found(&error) => Ok(None),
        Err(error) => Err(regerr(error)),
    }
}

fn read_optional_string(key: &Key, name: &str) -> Result<Option<String>, IntegrationError> {
    match key.get_value(name) {
        Ok(value) if matches!(value.ty(), Type::String | Type::ExpandString) => {
            Ok(Some(String::try_from(value).map_err(regerr)?))
        }
        Ok(_) => Err(IntegrationError::Drift(
            "registry value has an unsupported type".into(),
        )),
        Err(error) if is_not_found(&error) => Ok(None),
        Err(error) => Err(regerr(error)),
    }
}

fn registration_tree_empty(key: &Key) -> Result<bool, IntegrationError> {
    if key.values().map_err(regerr)?.next().is_some() {
        return Ok(false);
    }
    for child in key.keys().map_err(regerr)? {
        let child = key.open(&child).map_err(regerr)?;
        if !registration_tree_empty(&child)? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn ignore_missing(error: windows_result::Error) -> Result<(), windows_result::Error> {
    if is_not_found(&error) {
        Ok(())
    } else {
        Err(error)
    }
}

type Hwnd = *mut core::ffi::c_void;
link!("user32.dll" "system" fn SendMessageTimeoutW(hwnd: Hwnd, msg: u32, wparam: usize, lparam: isize, flags: u32, timeout: u32, result: *mut usize) -> isize);

fn broadcast_environment_change() {
    let name: Vec<u16> = "Environment".encode_utf16().chain([0]).collect();
    unsafe {
        SendMessageTimeoutW(
            0xffffusize as Hwnd,
            0x001A,
            0,
            name.as_ptr() as isize,
            0x0002,
            5000,
            std::ptr::null_mut(),
        );
    }
}

pub fn notify_committed_path_change(record: &zup_transaction::TransactionRecord) {
    if record.phase == zup_transaction::TransactionPhase::Committed
        && record.plan.nodes.iter().any(|node| {
            record
                .receipt(&node.id)
                .is_some_and(|receipt| matches!(receipt, OperationReceipt::Backend { payload, .. }
                    if receipt_from_bytes(payload).is_ok_and(|receipt| matches!(receipt, BackendReceipt::Path { .. } | BackendReceipt::RemovePath { .. } | BackendReceipt::RebindPath { .. }))))
        })
    {
        broadcast_environment_change();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transaction_payload::{ApplyPayload, RemovePayload, encode};
    use zup_core::{AppId, Privilege};

    #[rstest::rstest]
    #[case(
        r#"; C:\Windows ;"c:/apps/a";;%TOOLS%;D:\Other;"#,
        r"; C:\Windows ;C:\Apps\B;;%TOOLS%;D:\Other;"
    )]
    #[case(r"C:\Apps\A", r"C:\Apps\B")]
    fn path_rebinding_preserves_unrelated_segments(#[case] raw: &str, #[case] expected: &str) {
        let target = zup_core::Target::parse("windows-x64").unwrap();
        let previous = zup_core::TargetPath::new(target, r"C:\Apps\A").unwrap();
        let installed = zup_core::TargetPath::new(target, r"C:\Apps\B").unwrap();
        assert_eq!(rebind_value(raw, &previous, &installed).unwrap(), expected);
    }

    #[rstest::rstest]
    #[case(r"C:\Windows")]
    #[case(r"C:\Apps\A;c:/apps/a/")]
    #[case(r"C:\Apps\A;C:\Apps\B")]
    fn path_rebinding_refuses_uncertain_membership(#[case] raw: &str) {
        let target = zup_core::Target::parse("windows-x64").unwrap();
        let previous = zup_core::TargetPath::new(target, r"C:\Apps\A").unwrap();
        let installed = zup_core::TargetPath::new(target, r"C:\Apps\B").unwrap();
        assert!(rebind_value(raw, &previous, &installed).is_err());
    }

    #[test]
    fn native_path_rebinding_round_trip_preserves_other_writers_entries() {
        let location = format!(r"Software\ZupVerify\Path-{}", uuid::Uuid::now_v7().simple());
        let key = CURRENT_USER.create(&location).unwrap();
        let target = Target::parse("windows-x64").unwrap();
        let binding = |path: &str| zup_plan::PathBinding {
            value: zup_core::TargetPath::new(target, path).unwrap(),
            value_type: crate::search_path::VALUE_TYPE_EXPAND.into(),
        };
        let previous = binding(r"C:\Apps\A");
        let installed = binding(r"C:\Apps\B");
        let original = r"%SystemRoot%;C:\Apps\A;; D:\Tools ;";
        key.set_expand_string("Path", original).unwrap();
        rebind_path_key(&key, &previous, &installed).unwrap();
        assert_eq!(
            read_path_from_key(&key).unwrap(),
            (
                crate::search_path::VALUE_TYPE_EXPAND.into(),
                r"%SystemRoot%;C:\Apps\B;; D:\Tools ;".into()
            )
        );
        key.set_expand_string("Path", r"%SystemRoot%;C:\Apps\B;; D:\Tools ;E:\Added")
            .unwrap();
        rebind_path_key(&key, &installed, &previous).unwrap();
        assert_eq!(
            read_path_from_key(&key).unwrap().1,
            format!("{original}E:\\Added")
        );
        cleanup(&location);
    }

    #[test]
    fn missing_path_receipt_cannot_adopt_an_unclaimed_entry() {
        let target = Target::parse("windows-x64").unwrap();
        let key = ResourceKey::PathEntry {
            id: zup_core::DeclaredResourceId::Application {
                id: zup_core::ResourceId::new("cli").unwrap(),
            },
        };
        let operation = zup_plan::PathOperation {
            key: key.clone(),
            kind: zup_plan::PathOperationKind::Add,
            value: zup_core::TargetPath::new(target, r"C:\Apps\B").unwrap(),
            scope: SelectedScope::User,
            privilege: Privilege::User,
            precondition: zup_plan::PathPrecondition::Unclaimed {
                presence: zup_plan::PathPresence::Missing,
            },
            conflict: None,
        };
        assert!(matches!(
            reconcile_path_binding(
                key.clone(),
                &operation,
                crate::search_path::VALUE_TYPE_EXPAND,
                r"C:\Apps\B"
            )
            .unwrap(),
            ReconcileResult::Ambiguous
        ));
        assert!(matches!(
            reconcile_path_binding(
                key,
                &operation,
                crate::search_path::VALUE_TYPE_EXPAND,
                r"C:\Windows"
            )
            .unwrap(),
            ReconcileResult::NotApplied
        ));
    }

    fn apps_operation(app_id: &AppId) -> AppsFeaturesOperation {
        AppsFeaturesOperation {
            scope: SelectedScope::User,
            privilege: Privilege::User,
            key_path: format!(
                r"Software\Microsoft\Windows\CurrentVersion\Uninstall\zup-verify-{}",
                app_id.as_str()
            ),
            previous: None,
            installed: AppsFeaturesState {
                values: BTreeMap::from([(
                    "DisplayName".to_owned(),
                    AppsFeaturesValue::String("Zup Verify Fixture".to_owned()),
                )]),
            },
        }
    }

    fn backend_apply_node(app_id: &AppId, operation: AppsFeaturesOperation) -> DeploymentNode {
        let key = crate::transaction_payload::apps_key(app_id);
        let id = match &key {
            ResourceKey::Backend { id } => id.clone(),
            other => panic!("not a backend key: {other:?}"),
        };
        DeploymentNode {
            id: OperationId::resource("backend_apply", &key),
            phase: Phase::Backend,
            kind: zup_plan::NodeKind::BackendOperation {
                key: key.clone(),
                intent: BackendOperationIntent::Apply,
            },
            declaration_order: 1,
            meta: zup_plan::NodeMeta {
                privilege: Some(Privilege::User),
                backend: Some(BackendOperation {
                    key,
                    id,
                    privilege: Privilege::User,
                    intent: BackendOperationIntent::Apply,
                    payload: encode(&ApplyPayload::AppsFeatures { operation })
                        .expect("bounded payload"),
                    dependencies: Vec::new(),
                }),
                ..Default::default()
            },
        }
    }

    fn backend_remove_node(app_id: &AppId, operation: &AppsFeaturesOperation) -> DeploymentNode {
        let key = crate::transaction_payload::apps_key(app_id);
        let id = match &key {
            ResourceKey::Backend { id } => id.clone(),
            other => panic!("not a backend key: {other:?}"),
        };
        DeploymentNode {
            id: OperationId::resource("backend_remove", &key),
            phase: Phase::Backend,
            kind: zup_plan::NodeKind::BackendRemoval { key: key.clone() },
            declaration_order: 2,
            meta: zup_plan::NodeMeta {
                privilege: Some(Privilege::User),
                backend: Some(BackendOperation {
                    key,
                    id,
                    privilege: Privilege::User,
                    intent: BackendOperationIntent::Remove,
                    payload: encode(&RemovePayload::AppsFeatures {
                        scope: operation.scope,
                        key_path: operation.key_path.clone(),
                        state: operation.installed.clone(),
                    })
                    .expect("bounded payload"),
                    dependencies: Vec::new(),
                }),
                ..Default::default()
            },
        }
    }

    fn fixture() -> (AppId, AppsFeaturesOperation) {
        let app_id = AppId::new(format!("com.zup.verify-{}", uuid::Uuid::now_v7().simple()))
            .expect("app id");
        let operation = apps_operation(&app_id);
        (app_id, operation)
    }

    fn cleanup(key_path: &str) {
        let _ = windows_registry::CURRENT_USER.remove_tree(key_path);
    }

    #[test]
    fn backend_apply_verification_follows_the_installed_state() {
        let (app_id, operation) = fixture();
        let node = backend_apply_node(&app_id, operation.clone());
        let receipt = apply_managed(&node).expect("apply");
        verify_managed(&node, &receipt).expect("the live registration is the installed state");

        windows_registry::CURRENT_USER
            .create(&operation.key_path)
            .expect("open for write")
            .set_string("DisplayName", "someone else")
            .expect("drift");
        assert!(
            verify_managed(&node, &receipt).is_err(),
            "a drifted registration must not verify"
        );
        cleanup(&operation.key_path);
    }

    #[test]
    fn backend_removal_verification_follows_the_removed_state() {
        let (app_id, operation) = fixture();
        let apply = backend_apply_node(&app_id, operation.clone());
        let applied = apply_managed(&apply).expect("apply");

        let remove = backend_remove_node(&app_id, &operation);
        let removed = apply_owned_removal(&remove).expect("remove");
        verify_managed(&remove, &removed).expect("the removed state is the installed state");
        assert!(
            read_apps_features(operation.scope, operation.key_path.as_str())
                .expect("read")
                .is_none(),
            "the registration is gone"
        );
        assert!(
            verify_managed(&apply, &applied).is_err(),
            "the apply receipt no longer describes the host"
        );
        cleanup(&operation.key_path);
    }
}
