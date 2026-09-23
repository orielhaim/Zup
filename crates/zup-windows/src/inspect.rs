//! Read-only inspection of target-machine state.
//!
//! **Invariant: inspection must never mutate the machine.** No create, write,
//! registry set, service configure, or execute calls are permitted here.

use std::fs::{self, File};

use thiserror::Error;
use tracing::{info, info_span};
use zup_core::hash_reader;
use zup_exec::{
    MachineSnapshot, ObservedExtensionState, ObservedFile, ObservedFileState, ObservedFileType,
    ObservedPathEntry, ObservedProgIdState, ObservedProtocol, ObservedProtocolState,
    ObservedService, ObservedShortcut, PathEntryState,
};
use zup_platform::TargetPlan;

use crate::cmdline;
use crate::registry::{
    RegistryError, RegistryReader, RegistryValue, WindowsRegistryReader, read_path_value,
};
use crate::services::{ServiceReader, WindowsServiceReader};
use crate::shortcuts::{ShortcutReader, WindowsShortcutReader};

/// Errors produced while inspecting target resources.
#[derive(Debug, Error)]
pub enum InspectError {
    #[error("failed to read metadata for `{path}`")]
    TargetMetadataFailed {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to read `{path}`")]
    TargetReadFailed {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("target file `{path}` changed while being inspected")]
    TargetChangedDuringInspection { path: String },

    #[error("registry inspection failed")]
    Registry(#[from] RegistryError),

    #[error("shortcut inspection failed for `{path}`: {reason}")]
    ShortcutFailed { path: String, reason: String },

    #[error("service inspection failed for `{name}`: {reason}")]
    ServiceQueryFailed { name: String, reason: String },
}

/// Inspect every active target resource and produce an immutable snapshot.
pub fn inspect_target(target: &TargetPlan) -> Result<MachineSnapshot, InspectError> {
    let registry = WindowsRegistryReader;
    let services = WindowsServiceReader;
    let shortcuts = WindowsShortcutReader;
    inspect_target_with(target, &registry, &services, &shortcuts)
}

/// Inspect using injected backends (tests and advanced callers).
pub fn inspect_target_with<R, S, K>(
    target: &TargetPlan,
    registry: &R,
    services: &S,
    shortcuts: &K,
) -> Result<MachineSnapshot, InspectError>
where
    R: RegistryReader,
    S: ServiceReader,
    K: ShortcutReader,
{
    let _span = info_span!("inspect_target").entered();
    info!("inspection started (read-only)");

    let files = inspect_files(target)?;
    let shortcut_obs = inspect_shortcuts(target, shortcuts)?;
    let path_entries = inspect_path_entries(target, registry)?;
    let service_obs = inspect_services(target, services)?;
    let protocol_obs = inspect_protocols(target, registry)?;
    let file_types = inspect_file_types(target, registry)?;

    info!(
        files = files.len(),
        shortcuts = shortcut_obs.len(),
        path_entries = path_entries.len(),
        services = service_obs.len(),
        protocols = protocol_obs.len(),
        file_types = file_types.len(),
        "inspection complete"
    );

    Ok(MachineSnapshot {
        files,
        shortcuts: shortcut_obs,
        path_entries,
        services: service_obs,
        protocols: protocol_obs,
        file_types,
    })
}

/// File-only inspection (also used as a narrower advanced API).
pub fn inspect_files(target: &TargetPlan) -> Result<Vec<ObservedFile>, InspectError> {
    let mut files = Vec::with_capacity(target.files.len());
    let mut bytes_inspected = 0u64;

    for file in &target.files {
        let path = file.destination.as_path();
        let state = observe_file(path, &mut bytes_inspected)?;
        files.push(ObservedFile {
            key: file.key.clone(),
            path: file.destination.clone(),
            state,
        });
    }
    Ok(files)
}

fn observe_file(
    path: &std::path::Path,
    bytes_inspected: &mut u64,
) -> Result<ObservedFileState, InspectError> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ObservedFileState::Absent);
        }
        Err(source) => {
            return Err(InspectError::TargetMetadataFailed {
                path: path.display().to_string(),
                source,
            });
        }
    };

    let file_type = meta.file_type();
    if file_type.is_symlink() || !file_type.is_file() {
        return Ok(ObservedFileState::NonFile);
    }

    let before = fs::metadata(path).map_err(|source| InspectError::TargetMetadataFailed {
        path: path.display().to_string(),
        source,
    })?;
    let file = File::open(path).map_err(|source| InspectError::TargetReadFailed {
        path: path.display().to_string(),
        source,
    })?;
    let (size, sha256) = hash_reader(file).map_err(|source| InspectError::TargetReadFailed {
        path: path.display().to_string(),
        source,
    })?;
    let after = fs::metadata(path).map_err(|source| InspectError::TargetMetadataFailed {
        path: path.display().to_string(),
        source,
    })?;

    if before.len() != after.len() || before.len() != size {
        return Err(InspectError::TargetChangedDuringInspection {
            path: path.display().to_string(),
        });
    }
    if let (Ok(a), Ok(b)) = (before.modified(), after.modified())
        && a != b
    {
        return Err(InspectError::TargetChangedDuringInspection {
            path: path.display().to_string(),
        });
    }

    *bytes_inspected = bytes_inspected.saturating_add(size);
    Ok(ObservedFileState::File { size, sha256 })
}

fn inspect_shortcuts<K: ShortcutReader>(
    target: &TargetPlan,
    reader: &K,
) -> Result<Vec<ObservedShortcut>, InspectError> {
    let mut out = Vec::with_capacity(target.shortcuts.len());
    for shortcut in &target.shortcuts {
        let state = reader
            .read_shortcut(&shortcut.link_path)
            .map_err(|reason| InspectError::ShortcutFailed {
                path: shortcut.link_path.to_string(),
                reason,
            })?;
        out.push(ObservedShortcut {
            key: shortcut.key.clone(),
            link_path: shortcut.link_path.clone(),
            state,
        });
    }
    Ok(out)
}

fn inspect_path_entries<R: RegistryReader>(
    target: &TargetPlan,
    registry: &R,
) -> Result<Vec<ObservedPathEntry>, InspectError> {
    let mut cache: Vec<(zup_core::SelectedScope, Option<String>)> = Vec::new();
    let mut out = Vec::with_capacity(target.path_entries.len());

    for entry in &target.path_entries {
        if !cache.iter().any(|(s, _)| *s == entry.scope) {
            let value = read_path_value(registry, entry.scope)?;
            cache.push((entry.scope, value));
        }
        let path_value = cache
            .iter()
            .find(|(s, _)| *s == entry.scope)
            .and_then(|(_, v)| v.clone());

        let state = match path_value {
            None => PathEntryState::Absent,
            Some(raw) => {
                // Find the matching raw segment, if any.
                let mut found = None;
                for seg in raw.split(';') {
                    let seg = seg.trim();
                    if seg.is_empty() {
                        continue;
                    }
                    if cmdline::path_entry_matches(seg, &entry.value) {
                        found = Some(seg.to_owned());
                        break;
                    }
                }
                match found {
                    Some(raw_entry) => PathEntryState::Present { raw_entry },
                    None => PathEntryState::Absent,
                }
            }
        };

        out.push(ObservedPathEntry {
            key: entry.key.clone(),
            desired: entry.value.clone(),
            scope: entry.scope,
            state,
        });
    }
    Ok(out)
}

fn inspect_services<S: ServiceReader>(
    target: &TargetPlan,
    reader: &S,
) -> Result<Vec<ObservedService>, InspectError> {
    let mut out = Vec::with_capacity(target.services.len());
    for service in &target.services {
        let state = reader
            .read_service(service.name.as_str())
            .map_err(|reason| InspectError::ServiceQueryFailed {
                name: service.name.to_string(),
                reason,
            })?;
        out.push(ObservedService {
            key: service.key.clone(),
            id: service.id.clone(),
            state,
        });
    }
    Ok(out)
}

fn inspect_protocols<R: RegistryReader>(
    target: &TargetPlan,
    registry: &R,
) -> Result<Vec<ObservedProtocol>, InspectError> {
    let mut out = Vec::with_capacity(target.protocols.len());
    for protocol in &target.protocols {
        let scheme = protocol.scheme.as_str();
        let state = match registry.open_classes_key(protocol.scope, scheme)? {
            None => ObservedProtocolState::Absent,
            Some(key) => {
                let marker = key.get_value("URL Protocol").map(|_| true).unwrap_or(false);
                match open_command(registry, &key) {
                    None if !marker && registry_key_empty(&key)? => ObservedProtocolState::Absent,
                    None => ObservedProtocolState::Malformed {
                        reason: "missing shell\\open\\command".to_owned(),
                    },
                    Some(raw) => match cmdline::command_spec_from_command_line(&raw) {
                        Ok(command) => ObservedProtocolState::Registration {
                            command,
                            url_protocol_marker: marker,
                        },
                        Err(reason) => ObservedProtocolState::Malformed { reason },
                    },
                }
            }
        };
        out.push(ObservedProtocol {
            key: protocol.key.clone(),
            scheme: protocol.scheme.clone(),
            scope: protocol.scope,
            state,
        });
    }
    Ok(out)
}

fn inspect_file_types<R: RegistryReader>(
    target: &TargetPlan,
    registry: &R,
) -> Result<Vec<ObservedFileType>, InspectError> {
    let mut out = Vec::with_capacity(target.file_types.len());
    for file_type in &target.file_types {
        let id = file_type.id.as_str();
        let id_state = match registry.open_classes_key(file_type.scope, id)? {
            None => ObservedProgIdState::Absent,
            Some(key) => {
                let description = match R::read_value(&key, "FriendlyTypeName") {
                    RegistryValue::Sz(s) | RegistryValue::ExpandSz(s) if !s.is_empty() => Some(s),
                    _ => match R::read_value(&key, "") {
                        RegistryValue::Sz(s) | RegistryValue::ExpandSz(s) if !s.is_empty() => {
                            Some(s)
                        }
                        _ => None,
                    },
                };
                match open_command(registry, &key) {
                    None if description.is_none() && registry_key_empty(&key)? => {
                        ObservedProgIdState::Absent
                    }
                    None => ObservedProgIdState::Malformed {
                        reason: "missing shell\\open\\command".to_owned(),
                    },
                    Some(raw) => match cmdline::command_spec_from_command_line(&raw) {
                        Ok(command) => ObservedProgIdState::Registration {
                            description,
                            command,
                        },
                        Err(reason) => ObservedProgIdState::Malformed { reason },
                    },
                }
            }
        };

        let ext = file_type.extension.as_str();
        let extension_state = match registry.open_classes_key(file_type.scope, ext)? {
            None => ObservedExtensionState::Absent,
            Some(key) => match R::read_value(&key, "") {
                RegistryValue::Sz(s) | RegistryValue::ExpandSz(s) if !s.is_empty() => {
                    ObservedExtensionState::Mapped { prog_id: s }
                }
                RegistryValue::Missing if registry_key_empty(&key)? => {
                    ObservedExtensionState::Absent
                }
                RegistryValue::Missing => ObservedExtensionState::Malformed {
                    reason: "extension registration has no default ProgID".to_owned(),
                },
                RegistryValue::Other { .. } | RegistryValue::Sz(_) | RegistryValue::ExpandSz(_) => {
                    ObservedExtensionState::Malformed {
                        reason: "extension default value is not a ProgID string".to_owned(),
                    }
                }
            },
        };

        out.push(ObservedFileType {
            key: file_type.key.clone(),
            extension: ext.to_owned(),
            id: id.to_owned(),
            scope: file_type.scope,
            id_state,
            extension_state,
        });
    }
    Ok(out)
}

fn registry_key_empty(key: &windows_registry::Key) -> Result<bool, InspectError> {
    let values = key
        .values()
        .map_err(|error| RegistryError::Failed(error.message().to_owned()))?;
    let children = key
        .keys()
        .map_err(|error| RegistryError::Failed(error.message().to_owned()))?;
    Ok(values.take(1).next().is_none() && children.take(1).next().is_none())
}

fn open_command<R: RegistryReader>(registry: &R, key: &windows_registry::Key) -> Option<String> {
    let _ = registry;
    let shell = key.open("shell").ok()?;
    let open = shell.open("open").ok()?;
    let cmd = open.open("command").ok()?;
    match R::read_value(&cmd, "") {
        RegistryValue::Sz(s) | RegistryValue::ExpandSz(s) => Some(s),
        RegistryValue::Missing | RegistryValue::Other { .. } => None,
    }
}
