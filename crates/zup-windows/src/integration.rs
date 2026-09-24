//! Ownership-aware PATH, protocol, ProgID, and extension mutations.

use std::collections::BTreeMap;
use windows_link::link;
use windows_registry::{CURRENT_USER, Key, LOCAL_MACHINE, Type};
use zup_core::{ResourceKey, SelectedScope};
use zup_exec::{
    ExtensionState, ManagedOperation, OwnedResource, ProgIdState, ProtocolState, ServiceState,
    ShortcutState,
};
use zup_transaction::{OperationReceipt, ReconcileResult, TransactionNode};

use crate::cmdline::{command_spec_from_command_line, format_command_line, path_entry_matches};

#[derive(Debug, thiserror::Error)]
pub enum IntegrationError {
    #[error("registry operation failed: {0}")]
    Registry(String),
    #[error("managed resource drift: {0}")]
    Drift(String),
    #[error("unsupported managed operation")]
    Unsupported,
}

pub fn apply_managed(node: &TransactionNode) -> Result<OperationReceipt, IntegrationError> {
    match node
        .meta
        .managed
        .as_ref()
        .ok_or(IntegrationError::Unsupported)?
    {
        ManagedOperation::Shortcut(op) => {
            crate::shortcuts::apply(op).map_err(IntegrationError::Drift)
        }
        ManagedOperation::Path(op) => apply_path(op),
        ManagedOperation::Service(op) => {
            crate::services::apply(op).map_err(IntegrationError::Drift)
        }
        ManagedOperation::Protocol(op) => apply_protocol(op),
        ManagedOperation::ProgId(op) => apply_progid(op),
        ManagedOperation::Extension(op) => apply_extension(op),
        ManagedOperation::UninstallEntry(op) => {
            if read_uninstall_entry(op.scope, &op.key_path)? != op.previous {
                return Err(IntegrationError::Drift(format!(
                    "uninstall entry {} changed",
                    op.key_path
                )));
            }
            write_uninstall_entry(op.scope, &op.key_path, Some(&op.installed))?;
            Ok(OperationReceipt::UninstallEntry {
                scope: op.scope,
                key_path: op.key_path.clone(),
                previous: op.previous.clone(),
                installed: Some(op.installed.clone()),
            })
        }
    }
}

fn removal_receipt(node: &TransactionNode) -> Result<OperationReceipt, IntegrationError> {
    let scope = node
        .meta
        .removal_scope
        .ok_or(IntegrationError::Unsupported)?;
    let owned = node
        .meta
        .removal
        .as_ref()
        .ok_or(IntegrationError::Unsupported)?;
    let zup_transaction::NodeKind::OwnedRemoval { key, .. } = &node.kind else {
        return Err(IntegrationError::Unsupported);
    };
    match (key, owned) {
        (
            ResourceKey::Shortcut { .. },
            OwnedResource::Shortcut {
                link_path,
                previous,
                installed,
            },
        ) => Ok(OperationReceipt::Shortcut {
            link_path: link_path.clone(),
            previous: Box::new(previous.clone()),
            installed: Box::new(installed.clone()),
        }),
        (ResourceKey::PathEntry { .. }, OwnedResource::PathEntry { value, value_type }) => {
            Ok(OperationReceipt::PathEntry {
                scope,
                entry: value.to_string(),
                value_type: value_type.clone(),
            })
        }
        (
            ResourceKey::Service { .. },
            OwnedResource::Service {
                name,
                previous,
                installed,
            },
        ) => Ok(OperationReceipt::Service {
            name: name.clone(),
            previous: Box::new(previous.clone()),
            installed: Box::new(installed.clone()),
        }),
        (
            ResourceKey::Protocol { scheme },
            OwnedResource::Protocol {
                previous,
                installed,
            },
        ) => Ok(OperationReceipt::Protocol {
            scope,
            scheme: scheme.to_string(),
            previous: previous.clone(),
            installed: installed.clone(),
        }),
        (
            ResourceKey::FileType { id },
            OwnedResource::ProgId {
                previous,
                installed,
            },
        ) => Ok(OperationReceipt::ProgId {
            scope,
            id: id.to_string(),
            previous: previous.clone(),
            installed: installed.clone(),
        }),
        (
            ResourceKey::FileTypeExtension { extension },
            OwnedResource::Extension {
                previous,
                installed,
            },
        ) => Ok(OperationReceipt::Extension {
            scope,
            extension: extension.to_string(),
            previous: previous.clone(),
            installed: installed.clone(),
        }),
        (
            ResourceKey::UninstallEntry { app_id },
            OwnedResource::UninstallEntry {
                scope: owned_scope,
                state,
            },
        ) if *owned_scope == scope => Ok(OperationReceipt::UninstallEntry {
            scope,
            key_path: format!("Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\{app_id}"),
            previous: None,
            installed: Some(state.clone()),
        }),
        _ => Err(IntegrationError::Unsupported),
    }
}

fn inverted_removal_receipt(
    receipt: OperationReceipt,
) -> Result<OperationReceipt, IntegrationError> {
    Ok(match receipt {
        OperationReceipt::Shortcut {
            link_path,
            previous,
            installed,
        } => OperationReceipt::Shortcut {
            link_path,
            previous: installed,
            installed: previous,
        },
        OperationReceipt::PathEntry {
            scope,
            entry,
            value_type,
        } => OperationReceipt::RemovePathEntry {
            scope,
            entry,
            value_type,
        },
        OperationReceipt::Service {
            name,
            previous,
            installed,
        } => OperationReceipt::Service {
            name,
            previous: installed,
            installed: previous,
        },
        OperationReceipt::Protocol {
            scope,
            scheme,
            previous,
            installed,
        } => OperationReceipt::Protocol {
            scope,
            scheme,
            previous: installed,
            installed: previous,
        },
        OperationReceipt::ProgId {
            scope,
            id,
            previous,
            installed,
        } => OperationReceipt::ProgId {
            scope,
            id,
            previous: installed,
            installed: previous,
        },
        OperationReceipt::Extension {
            scope,
            extension,
            previous,
            installed,
        } => OperationReceipt::Extension {
            scope,
            extension,
            previous: installed,
            installed: previous,
        },
        OperationReceipt::UninstallEntry {
            scope,
            key_path,
            previous,
            installed,
        } => OperationReceipt::UninstallEntry {
            scope,
            key_path,
            previous: installed,
            installed: previous,
        },
        _ => return Err(IntegrationError::Unsupported),
    })
}

pub fn apply_owned_removal(node: &TransactionNode) -> Result<OperationReceipt, IntegrationError> {
    let original = removal_receipt(node)?;
    rollback_managed(&original)?;
    inverted_removal_receipt(original)
}

pub fn reconcile_owned_removal(
    node: &TransactionNode,
) -> Result<ReconcileResult, IntegrationError> {
    let original = removal_receipt(node)?;
    let status = match &original {
        OperationReceipt::Shortcut {
            link_path,
            previous,
            installed,
        } => {
            use crate::shortcuts::ShortcutReader;
            match crate::shortcuts::WindowsShortcutReader.read_shortcut(link_path) {
                Ok(zup_exec::ObservedShortcutState::Absent) => {
                    compare_removal(&ShortcutState::Absent, previous, installed)
                }
                Ok(zup_exec::ObservedShortcutState::Shortcut {
                    target,
                    arguments,
                    working_directory,
                }) => compare_removal(
                    &ShortcutState::Link {
                        target,
                        arguments,
                        working_directory,
                    },
                    previous,
                    installed,
                ),
                _ => ReconcileResult::Ambiguous,
            }
        }
        OperationReceipt::PathEntry {
            scope,
            entry,
            value_type,
        } => match read_path(*scope) {
            Ok((ty, raw))
                if ty == *value_type
                    || (value_type == "missing"
                        && matches!(ty.as_str(), "missing" | "expand_sz")) =>
            {
                let count = raw.split(';').filter(|part| *part == entry).count();
                match count {
                    0 => ReconcileResult::Applied,
                    1 => ReconcileResult::NotApplied,
                    _ => ReconcileResult::Ambiguous,
                }
            }
            _ => ReconcileResult::Ambiguous,
        },
        OperationReceipt::Service {
            name,
            previous,
            installed,
        } => match crate::scm::query_service(name) {
            Ok(zup_exec::ObservedServiceState::Absent) => {
                compare_removal(&ServiceState::Absent, previous, installed)
            }
            Ok(zup_exec::ObservedServiceState::Service {
                display_name,
                command,
                start,
                ..
            }) => compare_removal(
                &ServiceState::Registration {
                    display_name,
                    command,
                    start,
                },
                previous,
                installed,
            ),
            Err(_) => ReconcileResult::Ambiguous,
        },
        OperationReceipt::Protocol {
            scope,
            scheme,
            previous,
            installed,
        } => match read_protocol(*scope, scheme) {
            Ok(current) => compare_removal(&current, previous, installed),
            Err(_) => ReconcileResult::Ambiguous,
        },
        OperationReceipt::ProgId {
            scope,
            id,
            previous,
            installed,
        } => match read_progid(*scope, id) {
            Ok(current) => compare_removal(&current, previous, installed),
            Err(_) => ReconcileResult::Ambiguous,
        },
        OperationReceipt::Extension {
            scope,
            extension,
            previous,
            installed,
        } => match read_extension(*scope, extension) {
            Ok(current) => compare_removal(&current, previous, installed),
            Err(_) => ReconcileResult::Ambiguous,
        },
        OperationReceipt::UninstallEntry {
            scope,
            key_path,
            previous,
            installed,
        } => match read_uninstall_entry(*scope, key_path) {
            Ok(current) => compare_removal(&current, previous, installed),
            Err(_) => ReconcileResult::Ambiguous,
        },
        _ => return Err(IntegrationError::Unsupported),
    };
    if status == ReconcileResult::Applied {
        Ok(ReconcileResult::AppliedWithReceipt(
            inverted_removal_receipt(original)?,
        ))
    } else {
        Ok(status)
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

pub fn rollback_managed(receipt: &OperationReceipt) -> Result<(), IntegrationError> {
    match receipt {
        OperationReceipt::Shortcut {
            link_path,
            previous,
            installed,
        } => crate::shortcuts::rollback(link_path, previous, installed)
            .map_err(IntegrationError::Drift),
        OperationReceipt::Service {
            name,
            previous,
            installed,
        } => crate::services::rollback(name, previous, installed).map_err(IntegrationError::Drift),
        OperationReceipt::PathEntry {
            scope,
            entry,
            value_type,
        } => rollback_path(*scope, entry, value_type),
        OperationReceipt::RemovePathEntry {
            scope,
            entry,
            value_type,
        } => restore_removed_path(*scope, entry, value_type),
        OperationReceipt::Protocol {
            scope,
            scheme,
            previous,
            installed,
        } => {
            if read_protocol(*scope, scheme)? != *installed {
                return Err(IntegrationError::Drift(format!("protocol {scheme}")));
            }
            write_protocol(*scope, scheme, previous)
        }
        OperationReceipt::ProgId {
            scope,
            id,
            previous,
            installed,
        } => {
            if read_progid(*scope, id)? != *installed {
                return Err(IntegrationError::Drift(format!("ProgID {id}")));
            }
            write_progid(*scope, id, previous)
        }
        OperationReceipt::Extension {
            scope,
            extension,
            previous,
            installed,
        } => {
            if read_extension(*scope, extension)? != *installed {
                return Err(IntegrationError::Drift(format!("extension {extension}")));
            }
            write_extension(*scope, extension, previous)
        }
        OperationReceipt::UninstallEntry {
            scope,
            key_path,
            previous,
            installed,
        } => {
            if read_uninstall_entry(*scope, key_path)? != *installed {
                return Err(IntegrationError::Drift(format!(
                    "uninstall entry {key_path}"
                )));
            }
            write_uninstall_entry(*scope, key_path, previous.as_ref())
        }
        _ => Err(IntegrationError::Unsupported),
    }
}

pub fn reconcile_managed(node: &TransactionNode) -> Result<ReconcileResult, IntegrationError> {
    let result = match node
        .meta
        .managed
        .as_ref()
        .ok_or(IntegrationError::Unsupported)?
    {
        ManagedOperation::Shortcut(op) => {
            return crate::shortcuts::reconcile(op).map_err(IntegrationError::Drift);
        }
        ManagedOperation::Path(op) => {
            let (value_type, raw) = match read_path(op.scope) {
                Ok(value) => value,
                Err(_) => return Ok(ReconcileResult::Ambiguous),
            };
            let entry = op.value.to_string();
            if raw.split(';').any(|s| s == entry) {
                ReconcileResult::AppliedWithReceipt(OperationReceipt::PathEntry {
                    scope: op.scope,
                    entry,
                    value_type,
                })
            } else if matches!(op.previous, zup_exec::PathEntryState::Absent) {
                ReconcileResult::NotApplied
            } else {
                ReconcileResult::Ambiguous
            }
        }
        ManagedOperation::Protocol(op) => {
            let current = match read_protocol(op.scope, op.scheme.as_str()) {
                Ok(value) => value,
                Err(_) => return Ok(ReconcileResult::Ambiguous),
            };
            let previous = protocol_from_observed(&op.previous)?;
            let installed = ProtocolState::Registration {
                command: op.command.clone(),
            };
            if current == installed {
                ReconcileResult::AppliedWithReceipt(OperationReceipt::Protocol {
                    scope: op.scope,
                    scheme: op.scheme.to_string(),
                    previous,
                    installed,
                })
            } else {
                ReconcileResult::NotApplied
            }
        }
        ManagedOperation::Service(op) => {
            return crate::services::reconcile(op).map_err(IntegrationError::Drift);
        }
        ManagedOperation::ProgId(op) => {
            let current = match read_progid(op.scope, &op.id) {
                Ok(value) => value,
                Err(_) => return Ok(ReconcileResult::Ambiguous),
            };
            let previous = progid_from_observed(&op.previous_id)?;
            let installed = ProgIdState::Registration {
                description: op.description.clone(),
                command: op.command.clone(),
            };
            if current == installed {
                ReconcileResult::AppliedWithReceipt(OperationReceipt::ProgId {
                    scope: op.scope,
                    id: op.id.clone(),
                    previous,
                    installed,
                })
            } else {
                ReconcileResult::NotApplied
            }
        }
        ManagedOperation::Extension(op) => {
            let current = match read_extension(op.scope, &op.extension) {
                Ok(value) => value,
                Err(_) => return Ok(ReconcileResult::Ambiguous),
            };
            let previous = extension_from_observed(&op.previous_extension)?;
            let installed = ExtensionState::Mapped {
                prog_id: op.id.clone(),
            };
            if current == installed {
                ReconcileResult::AppliedWithReceipt(OperationReceipt::Extension {
                    scope: op.scope,
                    extension: op.extension.clone(),
                    previous,
                    installed,
                })
            } else {
                ReconcileResult::NotApplied
            }
        }
        ManagedOperation::UninstallEntry(op) => {
            let current = match read_uninstall_entry(op.scope, &op.key_path) {
                Ok(value) => value,
                Err(_) => return Ok(ReconcileResult::Ambiguous),
            };
            if current.as_ref() == Some(&op.installed) {
                ReconcileResult::AppliedWithReceipt(OperationReceipt::UninstallEntry {
                    scope: op.scope,
                    key_path: op.key_path.clone(),
                    previous: op.previous.clone(),
                    installed: Some(op.installed.clone()),
                })
            } else if current == op.previous {
                ReconcileResult::NotApplied
            } else {
                ReconcileResult::Ambiguous
            }
        }
    };
    Ok(result)
}

fn apply_path(op: &zup_exec::PathOperation) -> Result<OperationReceipt, IntegrationError> {
    let key = environment_key(op.scope, true)?
        .ok_or_else(|| IntegrationError::Registry("environment key".into()))?;
    let (value_type, raw) = read_path_from_key(&key)?;
    if raw
        .split(';')
        .filter(|s| !s.trim().is_empty())
        .any(|s| path_entry_matches(s.trim(), &op.value))
    {
        return Err(IntegrationError::Drift(
            "PATH entry appeared after planning".into(),
        ));
    }
    let entry = op.value.to_string();
    let next = if raw.is_empty() {
        entry.clone()
    } else if raw.ends_with(';') {
        format!("{raw}{entry}")
    } else {
        format!("{raw};{entry}")
    };
    set_path(&key, &value_type, &next)?;
    Ok(OperationReceipt::PathEntry {
        scope: op.scope,
        entry,
        value_type,
    })
}

fn rollback_path(
    scope: SelectedScope,
    entry: &str,
    value_type: &str,
) -> Result<(), IntegrationError> {
    let Some(key) = environment_key(scope, true)? else {
        return Err(IntegrationError::Drift("PATH key missing".into()));
    };
    let (current_type, raw) = read_path_from_key(&key)?;
    if current_type != value_type && !(value_type == "missing" && current_type == "expand_sz") {
        return Err(IntegrationError::Drift("PATH value type changed".into()));
    }
    if raw.split(';').filter(|part| *part == entry).count() != 1 {
        return Err(IntegrationError::Drift(
            "installed PATH entry is missing or duplicated".into(),
        ));
    }
    let mut removed = false;
    let kept: Vec<&str> = raw
        .split(';')
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
            "installed PATH entry changed or disappeared".into(),
        ));
    }
    if value_type == "missing" && kept.iter().all(|part| part.is_empty()) {
        key.remove_value("Path")
            .or_else(ignore_missing)
            .map_err(regerr)?;
    } else {
        set_path(
            &key,
            if value_type == "missing" {
                "expand_sz"
            } else {
                value_type
            },
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
    let key = environment_key(scope, true)?
        .ok_or_else(|| IntegrationError::Drift("PATH key missing".into()))?;
    let (current_type, raw) = read_path_from_key(&key)?;
    if current_type != value_type
        && !(value_type == "missing" && matches!(current_type.as_str(), "missing" | "expand_sz"))
    {
        return Err(IntegrationError::Drift(
            "PATH value type changed after removal".into(),
        ));
    }
    if raw.split(';').any(|part| part == entry) {
        return Err(IntegrationError::Drift(
            "removed PATH entry appeared again".into(),
        ));
    }
    let next = if raw.is_empty() {
        entry.to_owned()
    } else if raw.ends_with(';') {
        format!("{raw}{entry}")
    } else {
        format!("{raw};{entry}")
    };
    set_path(
        &key,
        if current_type == "missing" {
            "expand_sz"
        } else {
            &current_type
        },
        &next,
    )?;
    broadcast_environment_change();
    Ok(())
}

fn apply_protocol(op: &zup_exec::ProtocolOperation) -> Result<OperationReceipt, IntegrationError> {
    let previous = protocol_from_observed(&op.previous)?;
    if read_protocol(op.scope, op.scheme.as_str())? != previous {
        return Err(IntegrationError::Drift(format!(
            "protocol {} changed",
            op.scheme
        )));
    }
    let installed = ProtocolState::Registration {
        command: op.command.clone(),
    };
    write_protocol(op.scope, op.scheme.as_str(), &installed)?;
    Ok(OperationReceipt::Protocol {
        scope: op.scope,
        scheme: op.scheme.to_string(),
        previous,
        installed,
    })
}

fn apply_progid(op: &zup_exec::FileTypeOperation) -> Result<OperationReceipt, IntegrationError> {
    let previous = progid_from_observed(&op.previous_id)?;
    if read_progid(op.scope, &op.id)? != previous {
        return Err(IntegrationError::Drift(format!("ProgID {} changed", op.id)));
    }
    let installed = ProgIdState::Registration {
        description: op.description.clone(),
        command: op.command.clone(),
    };
    write_progid(op.scope, &op.id, &installed)?;
    Ok(OperationReceipt::ProgId {
        scope: op.scope,
        id: op.id.clone(),
        previous,
        installed,
    })
}

fn apply_extension(op: &zup_exec::FileTypeOperation) -> Result<OperationReceipt, IntegrationError> {
    let previous = extension_from_observed(&op.previous_extension)?;
    if read_extension(op.scope, &op.extension)? != previous {
        return Err(IntegrationError::Drift(format!(
            "extension {} changed",
            op.extension
        )));
    }
    let installed = ExtensionState::Mapped {
        prog_id: op.id.clone(),
    };
    write_extension(op.scope, &op.extension, &installed)?;
    Ok(OperationReceipt::Extension {
        scope: op.scope,
        extension: op.extension.clone(),
        previous,
        installed,
    })
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

pub(crate) fn read_uninstall_entry(
    scope: SelectedScope,
    key_path: &str,
) -> Result<Option<zup_exec::UninstallEntryState>, IntegrationError> {
    let root = match scope {
        SelectedScope::User => &CURRENT_USER,
        SelectedScope::Machine => &LOCAL_MACHINE,
    };
    let Some(key) = open_optional(root, key_path, false)? else {
        return Ok(None);
    };
    if key.keys().map_err(regerr)?.next().is_some() {
        return Err(IntegrationError::Drift(format!(
            "uninstall key {key_path} has child keys"
        )));
    }
    let mut values = BTreeMap::new();
    for (name, raw) in key.values().map_err(regerr)? {
        let value = match raw.ty() {
            Type::String => {
                zup_exec::UninstallEntryValue::String(String::try_from(raw).map_err(regerr)?)
            }
            Type::U32 => zup_exec::UninstallEntryValue::Dword(u32::try_from(raw).map_err(regerr)?),
            _ => {
                return Err(IntegrationError::Drift(format!(
                    "uninstall value {name} has unsupported registry type"
                )));
            }
        };
        values.insert(name, value);
    }
    Ok(Some(zup_exec::UninstallEntryState { values }))
}

pub fn inspect_uninstall_registration(
    scope: SelectedScope,
    app_id: &str,
) -> Result<Option<zup_exec::UninstallEntryState>, IntegrationError> {
    let key_path = format!("Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\{app_id}");
    read_uninstall_entry(scope, &key_path)
}

fn write_uninstall_entry(
    scope: SelectedScope,
    key_path: &str,
    state: Option<&zup_exec::UninstallEntryState>,
) -> Result<(), IntegrationError> {
    let root = match scope {
        SelectedScope::User => &CURRENT_USER,
        SelectedScope::Machine => &LOCAL_MACHINE,
    };
    let Some(state) = state else {
        if let Some(key) = open_optional(root, key_path, true)? {
            if key.keys().map_err(regerr)?.next().is_some() {
                return Err(IntegrationError::Drift(format!(
                    "uninstall key {key_path} gained child keys"
                )));
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
            zup_exec::UninstallEntryValue::String(value) => key.set_string(name, value),
            zup_exec::UninstallEntryValue::Dword(value) => key.set_u32(name, *value),
        }
        .map_err(regerr)?;
    }
    Ok(())
}

fn environment_key(scope: SelectedScope, create: bool) -> Result<Option<Key>, IntegrationError> {
    let (root, path) = match scope {
        SelectedScope::User => (&CURRENT_USER, "Environment"),
        SelectedScope::Machine => (
            &LOCAL_MACHINE,
            "SYSTEM\\CurrentControlSet\\Control\\Session Manager\\Environment",
        ),
    };
    match if create {
        root.create(path)
    } else {
        root.open(path)
    } {
        Ok(k) => Ok(Some(k)),
        Err(e) if !create && is_not_found(&e) => Ok(None),
        Err(e) => Err(regerr(e)),
    }
}
pub(crate) fn read_path(scope: SelectedScope) -> Result<(String, String), IntegrationError> {
    match environment_key(scope, false)? {
        Some(key) => read_path_from_key(&key),
        None => Ok(("missing".into(), String::new())),
    }
}
fn read_path_from_key(key: &Key) -> Result<(String, String), IntegrationError> {
    match key.get_value("Path") {
        Ok(value) => match value.ty() {
            Type::String => Ok(("sz".into(), String::try_from(value).map_err(regerr)?)),
            Type::ExpandString => {
                Ok(("expand_sz".into(), String::try_from(value).map_err(regerr)?))
            }
            _ => Ok(("invalid".into(), String::new())),
        },
        Err(e) if is_not_found(&e) => Ok(("missing".into(), String::new())),
        Err(e) => Err(regerr(e)),
    }
}
fn set_path(key: &Key, ty: &str, value: &str) -> Result<(), IntegrationError> {
    match ty {
        "sz" => key.set_string("Path", value),
        "expand_sz" | "missing" => key.set_expand_string("Path", value),
        _ => {
            return Err(IntegrationError::Drift(
                "PATH has unsupported registry type".into(),
            ));
        }
    }
    .map_err(regerr)
}

pub(crate) fn read_protocol(
    scope: SelectedScope,
    scheme: &str,
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
            Err(IntegrationError::Drift(format!(
                "protocol {scheme} has unowned registry content"
            )))
        };
    }
    if !marker {
        return Err(IntegrationError::Drift("protocol marker missing".into()));
    }
    let raw = raw.ok_or_else(|| IntegrationError::Drift("protocol command missing".into()))?;
    let command = command_spec_from_command_line(&raw).map_err(IntegrationError::Drift)?;
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
                .and_then(|k| k.set_string("", format_spec(command)))
                .map_err(regerr)
        }
    }
}
pub(crate) fn read_progid(scope: SelectedScope, id: &str) -> Result<ProgIdState, IntegrationError> {
    let root = classes(scope, false)?;
    let Some(key) = open_optional(&root, id, false)? else {
        return Ok(ProgIdState::Absent);
    };
    let description =
        read_optional_string(&key, "FriendlyTypeName")?.or(read_optional_string(&key, "")?);
    let raw = match open_optional(&key, "shell\\open\\command", false)? {
        Some(command) => read_optional_string(&command, "")?,
        None => None,
    };
    if description.is_none() && raw.is_none() {
        return if registration_tree_empty(&key)? {
            Ok(ProgIdState::Absent)
        } else {
            Err(IntegrationError::Drift(format!(
                "ProgID {id} has unowned registry content"
            )))
        };
    }
    let raw = raw.ok_or_else(|| IntegrationError::Drift("ProgID command missing".into()))?;
    let command = command_spec_from_command_line(&raw).map_err(IntegrationError::Drift)?;
    Ok(ProgIdState::Registration {
        description,
        command,
    })
}
fn write_progid(
    scope: SelectedScope,
    id: &str,
    state: &ProgIdState,
) -> Result<(), IntegrationError> {
    let root = classes(scope, true)?;
    match state {
        ProgIdState::Absent => {
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
        ProgIdState::Registration {
            description,
            command,
        } => {
            let key = root.create(id).map_err(regerr)?;
            if let Some(d) = description {
                key.set_string("FriendlyTypeName", d).map_err(regerr)?;
            } else {
                key.remove_value("FriendlyTypeName")
                    .or_else(ignore_missing)
                    .map_err(regerr)?;
            }
            key.create("shell\\open\\command")
                .and_then(|k| k.set_string("", format_spec(command)))
                .map_err(regerr)
        }
    }
}
pub(crate) fn read_extension(
    scope: SelectedScope,
    ext: &str,
) -> Result<ExtensionState, IntegrationError> {
    let root = classes(scope, false)?;
    let Some(key) = open_optional(&root, ext, false)? else {
        return Ok(ExtensionState::Absent);
    };
    match key.get_value("") {
        Ok(value) if matches!(value.ty(), Type::String | Type::ExpandString) => {
            Ok(ExtensionState::Mapped {
                prog_id: String::try_from(value).map_err(regerr)?,
            })
        }
        Ok(_) => Err(IntegrationError::Drift(format!(
            "extension {ext} has an unsupported value type"
        ))),
        Err(e) if is_not_found(&e) => {
            if registration_tree_empty(&key)? {
                Ok(ExtensionState::Absent)
            } else {
                Err(IntegrationError::Drift(format!(
                    "extension {ext} has unowned registry content"
                )))
            }
        }
        Err(e) => Err(regerr(e)),
    }
}
fn write_extension(
    scope: SelectedScope,
    ext: &str,
    state: &ExtensionState,
) -> Result<(), IntegrationError> {
    let root = classes(scope, true)?;
    match state {
        ExtensionState::Absent => {
            if let Some(key) = open_optional(&root, ext, true)? {
                key.remove_value("")
                    .or_else(ignore_missing)
                    .map_err(regerr)?;
                if registration_tree_empty(&key)? {
                    root.remove_tree(ext)
                        .or_else(ignore_missing)
                        .map_err(regerr)?;
                }
            }
            Ok(())
        }
        ExtensionState::Mapped { prog_id } => root
            .create(ext)
            .and_then(|k| k.set_string("", prog_id))
            .map_err(regerr),
    }
}

fn protocol_from_observed(
    value: &zup_exec::ObservedProtocolState,
) -> Result<ProtocolState, IntegrationError> {
    match value {
        zup_exec::ObservedProtocolState::Absent => Ok(ProtocolState::Absent),
        zup_exec::ObservedProtocolState::Registration {
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
fn progid_from_observed(
    value: &zup_exec::ObservedProgIdState,
) -> Result<ProgIdState, IntegrationError> {
    match value {
        zup_exec::ObservedProgIdState::Absent => Ok(ProgIdState::Absent),
        zup_exec::ObservedProgIdState::Registration {
            description,
            command,
        } => Ok(ProgIdState::Registration {
            description: description.clone(),
            command: command.clone(),
        }),
        _ => Err(IntegrationError::Drift(
            "invalid ProgID precondition".into(),
        )),
    }
}
fn extension_from_observed(
    value: &zup_exec::ObservedExtensionState,
) -> Result<ExtensionState, IntegrationError> {
    match value {
        zup_exec::ObservedExtensionState::Absent => Ok(ExtensionState::Absent),
        zup_exec::ObservedExtensionState::Mapped { prog_id } => Ok(ExtensionState::Mapped {
            prog_id: prog_id.clone(),
        }),
        _ => Err(IntegrationError::Drift(
            "invalid extension precondition".into(),
        )),
    }
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
        Ok(_) => Err(IntegrationError::Drift(format!(
            "registry value {name} has an unsupported type"
        ))),
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

fn format_spec(command: &zup_platform::CommandSpec) -> String {
    format_command_line(command.executable.as_path(), &command.arguments)
}

type Hwnd = *mut core::ffi::c_void;
link!("user32.dll" "system" fn SendMessageTimeoutW(hwnd: Hwnd, msg: u32, wparam: usize, lparam: isize, flags: u32, timeout: u32, result: *mut usize) -> isize);
fn broadcast_environment_change() {
    let name: Vec<u16> = "Environment".encode_utf16().chain([0]).collect();
    // SAFETY: HWND_BROADCAST is a documented sentinel and name remains alive for the call.
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
        && record.nodes.values().any(|state| matches!(state, zup_transaction::NodeState::Applied { receipt } if matches!(receipt.as_ref(), OperationReceipt::PathEntry { .. }))) {
        broadcast_environment_change();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shortcuts::ShortcutReader;
    use tempfile::TempDir;
    use zup_core::{AppId, FileTypeId, ProtocolScheme, ResourceKey};
    use zup_exec::{
        FileTypeOperation, FileTypeOperationKind, PathOperation, PathOperationKind,
        ProtocolOperation, ProtocolOperationKind,
    };
    use zup_platform::{CommandSpec, TargetPath};
    use zup_transaction::{
        FilesystemTransactionStore, NodeKind, NodeState, OperationExecutor, TransactionCoordinator,
        TransactionOutcome, TransactionPhase, TransactionStore, compile_transaction, recover,
    };

    fn command(name: &str) -> CommandSpec {
        CommandSpec::new(
            TargetPath::new(std::path::PathBuf::from(format!(
                r"C:\zup-tests\{name}.exe"
            )))
            .unwrap(),
            vec!["%1".into()],
        )
    }

    struct RegistryCleanup {
        classes: Vec<String>,
        user_choice_extension: Option<String>,
    }

    impl RegistryCleanup {
        fn classes(keys: Vec<String>) -> Self {
            Self {
                classes: keys,
                user_choice_extension: None,
            }
        }
    }

    impl Drop for RegistryCleanup {
        fn drop(&mut self) {
            if let Ok(root) = classes(SelectedScope::User, true) {
                for key in &self.classes {
                    let _ = root.remove_tree(key).or_else(ignore_missing);
                }
            }
            if let Some(extension) = &self.user_choice_extension
                && let Ok(root) = CURRENT_USER
                    .options()
                    .read()
                    .write()
                    .open(r"Software\Microsoft\Windows\CurrentVersion\Explorer\FileExts")
            {
                let _ = root.remove_tree(extension).or_else(ignore_missing);
            }
        }
    }

    #[test]
    fn protocol_create_upgrade_drift_and_rollback() {
        let scheme = format!("zup-test-{}", uuid::Uuid::now_v7().simple());
        let _cleanup = RegistryCleanup::classes(vec![scheme.clone()]);
        let key = ResourceKey::Protocol {
            scheme: ProtocolScheme::new(&scheme).unwrap(),
        };
        let first = ProtocolOperation {
            key: key.clone(),
            kind: ProtocolOperationKind::Create,
            scheme: ProtocolScheme::new(&scheme).unwrap(),
            command: command("first"),
            previous: zup_exec::ObservedProtocolState::Absent,
            scope: SelectedScope::User,
            conflict: None,
        };
        let first_receipt = apply_protocol(&first).unwrap();
        assert_eq!(
            read_protocol(SelectedScope::User, &scheme).unwrap(),
            ProtocolState::Registration {
                command: command("first")
            }
        );
        let second = ProtocolOperation {
            command: command("second"),
            kind: ProtocolOperationKind::UpdateOwned,
            previous: zup_exec::ObservedProtocolState::Registration {
                command: command("first"),
                url_protocol_marker: true,
            },
            ..first
        };
        let second_receipt = apply_protocol(&second).unwrap();
        assert_eq!(
            read_protocol(SelectedScope::User, &scheme).unwrap(),
            ProtocolState::Registration {
                command: command("second")
            }
        );
        write_protocol(
            SelectedScope::User,
            &scheme,
            &ProtocolState::Registration {
                command: command("foreign"),
            },
        )
        .unwrap();
        assert!(matches!(
            rollback_managed(&second_receipt),
            Err(IntegrationError::Drift(_))
        ));
        assert_eq!(
            read_protocol(SelectedScope::User, &scheme).unwrap(),
            ProtocolState::Registration {
                command: command("foreign")
            }
        );
        write_protocol(
            SelectedScope::User,
            &scheme,
            &ProtocolState::Registration {
                command: command("second"),
            },
        )
        .unwrap();
        rollback_managed(&second_receipt).unwrap();
        rollback_managed(&first_receipt).unwrap();
    }

    #[test]
    fn association_parts_leave_user_choice_untouched() {
        let suffix = uuid::Uuid::now_v7().simple().to_string();
        let id = format!("Zup.Test.{suffix}");
        let extension = format!(".zup{suffix}");
        let _cleanup = RegistryCleanup {
            classes: vec![id.clone(), extension.clone()],
            user_choice_extension: Some(extension.clone()),
        };
        let choice_root = CURRENT_USER
            .create(format!(
                r"Software\Microsoft\Windows\CurrentVersion\Explorer\FileExts\{extension}"
            ))
            .unwrap();
        choice_root
            .create("UserChoice")
            .unwrap()
            .set_string("ProgId", "Foreign.Document")
            .unwrap();
        let op = FileTypeOperation {
            key: ResourceKey::FileType {
                id: FileTypeId::new(&id).unwrap(),
            },
            kind: FileTypeOperationKind::Create,
            prog_id_kind: FileTypeOperationKind::Create,
            extension_kind: FileTypeOperationKind::Create,
            extension: extension.clone(),
            id: id.clone(),
            description: Some("Zup test".into()),
            command: command("document"),
            scope: SelectedScope::User,
            previous_id: zup_exec::ObservedProgIdState::Absent,
            previous_extension: zup_exec::ObservedExtensionState::Absent,
            conflict: None,
        };
        let id_receipt = apply_progid(&op).unwrap();
        let extension_receipt = apply_extension(&op).unwrap();
        assert_eq!(
            read_extension(SelectedScope::User, &extension).unwrap(),
            ExtensionState::Mapped {
                prog_id: id.clone()
            }
        );
        assert_eq!(
            choice_root
                .open("UserChoice")
                .unwrap()
                .get_string("ProgId")
                .unwrap(),
            "Foreign.Document"
        );
        rollback_managed(&extension_receipt).unwrap();
        rollback_managed(&id_receipt).unwrap();
        assert_eq!(
            choice_root
                .open("UserChoice")
                .unwrap()
                .get_string("ProgId")
                .unwrap(),
            "Foreign.Document"
        );
        CURRENT_USER
            .open(r"Software\Microsoft\Windows\CurrentVersion\Explorer\FileExts")
            .unwrap()
            .remove_tree(&extension)
            .unwrap();
    }

    #[test]
    fn path_rollback_preserves_unrelated_entries() {
        let entry = format!(r"C:\zup-test-{}\bin", uuid::Uuid::now_v7().simple());
        let value = TargetPath::new(std::path::PathBuf::from(&entry)).unwrap();
        let before = read_path(SelectedScope::User).unwrap();
        let op = PathOperation {
            key: ResourceKey::PathEntry {
                value: entry.clone(),
            },
            kind: PathOperationKind::Add,
            value,
            scope: SelectedScope::User,
            previous: zup_exec::PathEntryState::Absent,
            previously_owned: false,
            conflict: None,
        };
        let receipt = apply_path(&op).unwrap();
        let (_, current) = read_path(SelectedScope::User).unwrap();
        assert!(current.contains(&entry));
        assert!(current.contains(&before.1));
        rollback_managed(&receipt).unwrap();
        let after = read_path(SelectedScope::User).unwrap();
        assert!(!after.1.split(';').any(|part| part == entry));
        assert_eq!(after.0, before.0);
    }

    struct RegistryExecutor {
        applied: usize,
        fail_after: Option<usize>,
    }
    impl OperationExecutor for RegistryExecutor {
        type Error = String;
        fn apply(&mut self, node: &TransactionNode) -> Result<OperationReceipt, Self::Error> {
            if self.fail_after == Some(self.applied) {
                return Err("injected failure".into());
            }
            self.applied += 1;
            apply_managed(node).map_err(|e| e.to_string())
        }
        fn rollback(
            &mut self,
            _node: &TransactionNode,
            receipt: &OperationReceipt,
        ) -> Result<(), Self::Error> {
            rollback_managed(receipt).map_err(|e| e.to_string())
        }
        fn reconcile(
            &mut self,
            node: &TransactionNode,
            _receipt: Option<&OperationReceipt>,
        ) -> Result<ReconcileResult, Self::Error> {
            reconcile_managed(node).map_err(|e| e.to_string())
        }
    }

    fn protocol_execution(schemes: &[String]) -> zup_exec::ExecutionPlan {
        zup_exec::ExecutionPlan {
            selected_components: vec![],
            install_directory: None,
            uninstall: false,
            removals: vec![],
            files: vec![],
            shortcuts: vec![],
            path_entries: vec![],
            services: vec![],
            file_types: vec![],
            uninstall_entries: vec![],
            protocols: schemes
                .iter()
                .map(|scheme| ProtocolOperation {
                    key: ResourceKey::Protocol {
                        scheme: ProtocolScheme::new(scheme).unwrap(),
                    },
                    kind: ProtocolOperationKind::Create,
                    scheme: ProtocolScheme::new(scheme).unwrap(),
                    command: command("coordinator"),
                    previous: zup_exec::ObservedProtocolState::Absent,
                    scope: SelectedScope::User,
                    conflict: None,
                })
                .collect(),
            summary: zup_exec::ExecutionSummary::default(),
        }
    }

    #[test]
    fn coordinator_rolls_back_first_registration_after_second_fails() {
        let prefix = format!("zup-test-{}", uuid::Uuid::now_v7().simple());
        let schemes = vec![format!("{prefix}-a"), format!("{prefix}-b")];
        let _cleanup = RegistryCleanup::classes(schemes.clone());
        let directory = TempDir::new().unwrap();
        let store = FilesystemTransactionStore::new(directory.path());
        let coordinator = TransactionCoordinator::new(store);
        let plan = compile_transaction(&protocol_execution(&schemes)).unwrap();
        let record = coordinator
            .begin(
                AppId::new("com.zup.registry-test").unwrap(),
                SelectedScope::User,
                "1.0.0".parse().unwrap(),
                plan,
            )
            .unwrap();
        let mut executor = RegistryExecutor {
            applied: 0,
            fail_after: Some(1),
        };
        let (_, outcome) = coordinator.execute(record, &mut executor).unwrap();
        assert_eq!(outcome, TransactionOutcome::RolledBack);
        for scheme in &schemes {
            assert_eq!(
                read_protocol(SelectedScope::User, scheme).unwrap(),
                ProtocolState::Absent
            );
        }
        assert!(
            crate::ledger::InstallLedgerStore::new(directory.path())
                .load(
                    &AppId::new("com.zup.registry-test").unwrap(),
                    SelectedScope::User
                )
                .unwrap()
                .is_none()
        );
        let root = classes(SelectedScope::User, true).unwrap();
        for scheme in schemes {
            root.remove_tree(scheme).or_else(ignore_missing).unwrap();
        }
    }

    #[test]
    fn coordinator_recovers_write_before_receipt() {
        let scheme = format!("zup-test-{}", uuid::Uuid::now_v7().simple());
        let _cleanup = RegistryCleanup::classes(vec![scheme.clone()]);
        let directory = TempDir::new().unwrap();
        let store = FilesystemTransactionStore::new(directory.path());
        let plan = compile_transaction(&protocol_execution(std::slice::from_ref(&scheme))).unwrap();
        let mut record = zup_transaction::TransactionRecord::new(
            zup_transaction::TransactionId::new_v7(),
            AppId::new("com.zup.recovery-test").unwrap(),
            SelectedScope::User,
            "1.0.0".parse().unwrap(),
            plan,
        );
        store.create(&record).unwrap();
        let node = record
            .plan
            .nodes
            .iter()
            .find(|node| matches!(node.kind, NodeKind::ManagedIntegration { .. }))
            .unwrap()
            .clone();
        record.phase = TransactionPhase::Applying;
        record.nodes.insert(node.id.clone(), NodeState::Running);
        let revision = record.revision;
        record.touch();
        store.compare_and_swap(revision, &record).unwrap();
        apply_managed(&node).unwrap();
        let mut executor = RegistryExecutor {
            applied: 0,
            fail_after: None,
        };
        let (record, outcome) = recover(record, &store, &mut executor).unwrap();
        assert_eq!(outcome, TransactionOutcome::Committed);
        assert!(
            matches!(record.nodes.get(&node.id), Some(NodeState::Applied { receipt }) if matches!(receipt.as_ref(), OperationReceipt::Protocol { .. }))
        );
        let ledger = crate::ledger::InstallLedgerStore::new(directory.path())
            .publish_committed(&record, SelectedScope::User)
            .unwrap();
        assert_eq!(ledger.resources.len(), 1);
        classes(SelectedScope::User, true)
            .unwrap()
            .remove_tree(scheme)
            .unwrap();
    }

    #[test]
    fn coordinator_recovers_shortcut_write_before_receipt() {
        let directory = TempDir::new().unwrap();
        let target = TargetPath::new(directory.path().join("App.exe")).unwrap();
        std::fs::write(target.as_path(), b"app").unwrap();
        let link_path = TargetPath::new(directory.path().join("App.lnk")).unwrap();
        let execution = zup_exec::ExecutionPlan {
            selected_components: vec![],
            install_directory: None,
            uninstall: false,
            removals: vec![],
            files: vec![],
            shortcuts: vec![zup_exec::ShortcutOperation {
                key: ResourceKey::Shortcut {
                    location: zup_core::ShortcutLocation::Desktop,
                    name: "App".into(),
                },
                kind: zup_exec::ShortcutOperationKind::Create,
                link_path: link_path.clone(),
                target: target.clone(),
                arguments: vec!["a b".into()],
                working_directory: None,
                previous: zup_exec::ObservedShortcutState::Absent,
                conflict: None,
            }],
            path_entries: vec![],
            services: vec![],
            protocols: vec![],
            file_types: vec![],
            uninstall_entries: vec![],
            summary: zup_exec::ExecutionSummary::default(),
        };
        let store = FilesystemTransactionStore::new(directory.path());
        let plan = compile_transaction(&execution).unwrap();
        let app_id = AppId::new("com.zup.shortcut-recovery").unwrap();
        let mut record = zup_transaction::TransactionRecord::new(
            zup_transaction::TransactionId::new_v7(),
            app_id.clone(),
            SelectedScope::User,
            "1.0.0".parse().unwrap(),
            plan,
        );
        store.create(&record).unwrap();
        let node = record
            .plan
            .nodes
            .iter()
            .find(|node| matches!(node.kind, NodeKind::ManagedIntegration { .. }))
            .unwrap()
            .clone();
        record.phase = TransactionPhase::Applying;
        record.nodes.insert(node.id.clone(), NodeState::Running);
        let revision = record.revision;
        record.touch();
        store.compare_and_swap(revision, &record).unwrap();
        apply_managed(&node).unwrap();
        let mut executor = RegistryExecutor {
            applied: 0,
            fail_after: None,
        };
        let (record, outcome) = recover(record, &store, &mut executor).unwrap();
        assert_eq!(outcome, TransactionOutcome::Committed);
        assert!(
            matches!(record.nodes.get(&node.id), Some(NodeState::Applied { receipt }) if matches!(receipt.as_ref(), OperationReceipt::Shortcut { .. }))
        );
        let ledger = crate::ledger::InstallLedgerStore::new(directory.path());
        assert!(ledger.load(&app_id, SelectedScope::User).unwrap().is_none());
        let published = ledger
            .publish_committed(&record, SelectedScope::User)
            .unwrap();
        assert_eq!(published.resources.len(), 1);
        assert!(matches!(
            crate::shortcuts::WindowsShortcutReader
                .read_shortcut(&link_path)
                .unwrap(),
            zup_exec::ObservedShortcutState::Shortcut { .. }
        ));
    }

    #[test]
    fn coordinator_recovers_service_write_before_receipt_when_elevated() {
        if !crate::transport::is_process_elevated().unwrap() {
            eprintln!("SCM recovery mutation requires an elevated test process");
            return;
        }
        struct Cleanup(String);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                use windows_service::service::ServiceAccess;
                use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
                if let Ok(manager) =
                    ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
                    && let Ok(service) = manager.open_service(&self.0, ServiceAccess::DELETE)
                {
                    let _ = service.delete();
                }
            }
        }
        let directory = TempDir::new().unwrap();
        let name = format!("zup-test-{}", uuid::Uuid::now_v7().simple());
        let _cleanup = Cleanup(name.clone());
        let binary = TargetPath::new(std::env::current_exe().unwrap()).unwrap();
        let operation = zup_exec::ServiceOperation {
            key: ResourceKey::Service {
                id: zup_core::ServiceId::new(&name).unwrap(),
            },
            kind: zup_exec::ServiceOperationKind::Create,
            id: name.clone(),
            name: name.clone(),
            display_name: "Zup Recovery Test".into(),
            command: zup_platform::CommandSpec::new(binary, vec!["--service".into()]),
            start: zup_core::ServiceStart::Disabled,
            previous: zup_exec::ObservedServiceState::Absent,
            conflict: None,
        };
        let execution = zup_exec::ExecutionPlan {
            selected_components: vec![],
            install_directory: None,
            uninstall: false,
            removals: vec![],
            files: vec![],
            shortcuts: vec![],
            path_entries: vec![],
            services: vec![operation],
            protocols: vec![],
            file_types: vec![],
            uninstall_entries: vec![],
            summary: zup_exec::ExecutionSummary::default(),
        };
        let store = FilesystemTransactionStore::new(directory.path());
        let plan = compile_transaction(&execution).unwrap();
        let app_id = AppId::new("com.zup.service-recovery").unwrap();
        let mut record = zup_transaction::TransactionRecord::new(
            zup_transaction::TransactionId::new_v7(),
            app_id.clone(),
            SelectedScope::Machine,
            "1.0.0".parse().unwrap(),
            plan,
        );
        store.create(&record).unwrap();
        let node = record
            .plan
            .nodes
            .iter()
            .find(|node| matches!(node.kind, NodeKind::ManagedIntegration { .. }))
            .unwrap()
            .clone();
        record.phase = TransactionPhase::Applying;
        record.nodes.insert(node.id.clone(), NodeState::Running);
        let revision = record.revision;
        record.touch();
        store.compare_and_swap(revision, &record).unwrap();
        apply_managed(&node).unwrap();
        let mut executor = RegistryExecutor {
            applied: 0,
            fail_after: None,
        };
        let (record, outcome) = recover(record, &store, &mut executor).unwrap();
        assert_eq!(outcome, TransactionOutcome::Committed);
        assert!(
            matches!(record.nodes.get(&node.id), Some(NodeState::Applied { receipt }) if matches!(receipt.as_ref(), OperationReceipt::Service { .. }))
        );
        let ledger = crate::ledger::InstallLedgerStore::new(directory.path());
        assert!(
            ledger
                .load(&app_id, SelectedScope::Machine)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            ledger
                .publish_committed(&record, SelectedScope::Machine)
                .unwrap()
                .resources
                .len(),
            1
        );
    }

    #[test]
    fn owned_protocol_removal_restores_previous_and_refuses_later_drift() {
        let scheme = format!("zup-test-{}", uuid::Uuid::now_v7().simple());
        let _cleanup = RegistryCleanup::classes(vec![scheme.clone()]);
        let previous = ProtocolState::Registration {
            command: command("foreign"),
        };
        let installed = ProtocolState::Registration {
            command: command("owned"),
        };
        write_protocol(SelectedScope::User, &scheme, &installed).unwrap();
        let key = ResourceKey::Protocol {
            scheme: ProtocolScheme::new(&scheme).unwrap(),
        };
        let plan = compile_transaction(&zup_exec::ExecutionPlan {
            removals: vec![zup_exec::RemovalOperation {
                key,
                kind: zup_exec::RemovalKind::RemoveOwned,
                scope: SelectedScope::User,
                owned: zup_exec::OwnedResource::Protocol {
                    previous: previous.clone(),
                    installed: installed.clone(),
                },
            }],
            ..Default::default()
        })
        .unwrap();
        let node = plan
            .nodes
            .iter()
            .find(|node| matches!(node.kind, NodeKind::OwnedRemoval { .. }))
            .unwrap();
        assert_eq!(
            reconcile_owned_removal(node).unwrap(),
            ReconcileResult::NotApplied
        );
        let receipt = apply_owned_removal(node).unwrap();
        assert_eq!(
            read_protocol(SelectedScope::User, &scheme).unwrap(),
            previous
        );
        assert!(matches!(
            reconcile_owned_removal(node).unwrap(),
            ReconcileResult::AppliedWithReceipt(_)
        ));
        rollback_managed(&receipt).unwrap();
        assert_eq!(
            read_protocol(SelectedScope::User, &scheme).unwrap(),
            installed
        );
        write_protocol(
            SelectedScope::User,
            &scheme,
            &ProtocolState::Registration {
                command: command("user-edit"),
            },
        )
        .unwrap();
        assert!(apply_owned_removal(node).is_err());
        assert_eq!(
            reconcile_owned_removal(node).unwrap(),
            ReconcileResult::Ambiguous
        );
        assert_eq!(
            read_protocol(SelectedScope::User, &scheme).unwrap(),
            ProtocolState::Registration {
                command: command("user-edit")
            }
        );
    }

    #[test]
    fn apps_and_features_registration_reconciles_and_refuses_drifted_rollback() {
        let app_id = format!("com.zup.arp-{}", uuid::Uuid::now_v7().simple());
        let key_path = format!("Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\{app_id}");
        let _cleanup = RegistryCleanup::classes(vec![]);
        let mut values = BTreeMap::new();
        values.insert(
            "DisplayName".into(),
            zup_exec::UninstallEntryValue::String("Acme".into()),
        );
        values.insert(
            "EstimatedSize".into(),
            zup_exec::UninstallEntryValue::Dword(42),
        );
        let installed = zup_exec::UninstallEntryState { values };
        let op = zup_exec::UninstallEntryOperation {
            key: ResourceKey::UninstallEntry {
                app_id: app_id.clone(),
            },
            scope: SelectedScope::User,
            key_path: key_path.clone(),
            previous: None,
            installed: installed.clone(),
        };
        let plan = compile_transaction(&zup_exec::ExecutionPlan {
            uninstall_entries: vec![op],
            ..Default::default()
        })
        .unwrap();
        let node = plan
            .nodes
            .iter()
            .find(|node| {
                matches!(
                    node.kind,
                    NodeKind::ManagedIntegration {
                        resource: zup_transaction::ManagedResource::UninstallEntry,
                        ..
                    }
                )
            })
            .unwrap();
        let receipt = apply_managed(node).unwrap();
        assert_eq!(
            read_uninstall_entry(SelectedScope::User, &key_path).unwrap(),
            Some(installed.clone())
        );
        assert!(matches!(
            reconcile_managed(node).unwrap(),
            ReconcileResult::AppliedWithReceipt(OperationReceipt::UninstallEntry { .. })
        ));
        rollback_managed(&receipt).unwrap();
        assert_eq!(
            read_uninstall_entry(SelectedScope::User, &key_path).unwrap(),
            None
        );

        let receipt = apply_managed(node).unwrap();
        let changed = zup_exec::UninstallEntryState {
            values: BTreeMap::from([(
                "DisplayName".into(),
                zup_exec::UninstallEntryValue::String("Changed externally".into()),
            )]),
        };
        write_uninstall_entry(SelectedScope::User, &key_path, Some(&changed)).unwrap();
        assert!(matches!(
            rollback_managed(&receipt),
            Err(IntegrationError::Drift(_))
        ));
        assert_eq!(
            read_uninstall_entry(SelectedScope::User, &key_path).unwrap(),
            Some(changed)
        );
        CURRENT_USER.remove_tree(&key_path).unwrap();
    }
}
