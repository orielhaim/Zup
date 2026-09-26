//! Read-only inspection of target-host state.
//!
//! **Invariant: inspection must never mutate the machine.** No create, write,
//! registry set, service configure, or execute calls are permitted here.

use std::fs::{self, File};

use thiserror::Error;
use tracing::{info, info_span};
use zup_core::SelectedScope;
use zup_core::hash_reader;
use zup_exec::{
    HostSnapshot, ObservedExtensionState, ObservedFile, ObservedFileAssociation,
    ObservedFileAssociationState, ObservedFileState, ObservedLauncher, ObservedPathEntry,
    ObservedProtocol, ObservedProtocolState, ObservedService, SearchPath,
};
use zup_platform::TargetPlan;

use crate::cmdline;
use crate::lowering::host_path;
use crate::registry::{RegistryError, RegistryReader, RegistryValue, WindowsRegistryReader};
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

    #[error("launcher inspection failed for `{path}`: {reason}")]
    LauncherInspectionFailed { path: String, reason: String },

    #[error("service inspection failed for `{name}`: {reason}")]
    ServiceQueryFailed { name: String, reason: String },
}

/// Inspect every active target resource and produce an immutable snapshot.
pub fn inspect_target(target: &TargetPlan) -> Result<HostSnapshot, InspectError> {
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
) -> Result<HostSnapshot, InspectError>
where
    R: RegistryReader,
    S: ServiceReader,
    K: ShortcutReader,
{
    let _span = info_span!("inspect_target").entered();
    info!("inspection started (read-only)");

    let files = inspect_files(target)?;
    let shortcut_obs = inspect_launchers(target, shortcuts)?;
    let path_entries = inspect_path_entries(target, registry)?;
    let service_obs = inspect_services(target, services)?;
    let protocol_obs = inspect_protocols(target, registry)?;
    let file_associations = inspect_file_associations(target, registry)?;

    info!(
        files = files.len(),
        shortcuts = shortcut_obs.len(),
        path_entries = path_entries.len(),
        services = service_obs.len(),
        protocols = protocol_obs.len(),
        file_associations = file_associations.len(),
        "inspection complete"
    );

    Ok(HostSnapshot {
        files,
        launchers: shortcut_obs,
        path_entries,
        services: service_obs,
        protocols: protocol_obs,
        file_associations,
    })
}

/// File-only inspection (also used as a narrower advanced API).
pub fn inspect_files(target: &TargetPlan) -> Result<Vec<ObservedFile>, InspectError> {
    let mut files = Vec::with_capacity(target.files.len());
    let mut bytes_inspected = 0u64;

    for file in &target.files {
        let path = host_path(&file.destination);
        let state = observe_file(&path, &mut bytes_inspected)?;
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

    let file_association = meta.file_type();
    if file_association.is_symlink() || !file_association.is_file() {
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

fn inspect_launchers<K: ShortcutReader>(
    target: &TargetPlan,
    reader: &K,
) -> Result<Vec<ObservedLauncher>, InspectError> {
    let mut out = Vec::with_capacity(target.launchers.len());
    for shortcut in &target.launchers {
        let state = reader
            .read_shortcut(&shortcut.launcher_path)
            .map_err(|reason| InspectError::LauncherInspectionFailed {
                path: shortcut.launcher_path.to_string(),
                reason,
            })?;
        out.push(ObservedLauncher {
            key: shortcut.key.clone(),
            launcher_path: shortcut.launcher_path.clone(),
            state,
        });
    }
    Ok(out)
}

fn inspect_path_entries<R: RegistryReader>(
    target: &TargetPlan,
    registry: &R,
) -> Result<Vec<ObservedPathEntry>, InspectError> {
    // One read per owning search path, then the portable, target-normalized
    // view is reused for every entry that path owns.
    let mut cache: Vec<(SelectedScope, SearchPath)> = Vec::new();
    let mut out = Vec::with_capacity(target.path_entries.len());

    for entry in &target.path_entries {
        if !cache.iter().any(|(scope, _)| *scope == entry.scope) {
            let search_path = match crate::search_path::read(registry, entry.scope)
                .map_err(InspectError::Registry)?
            {
                Some((_value_type, value)) => crate::search_path::collect(&target.target, &value),
                None => SearchPath::default(),
            };
            cache.push((entry.scope, search_path));
        }
        let search_path = cache
            .iter()
            .find(|(scope, _)| *scope == entry.scope)
            .map(|(_, search_path)| search_path.clone())
            .expect("search path was cached above");

        out.push(ObservedPathEntry {
            key: entry.key.clone(),
            desired: entry.value.clone(),
            scope: entry.scope,
            search_path,
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
            .read_service(service.name.as_str(), &target.target)
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
                    Some(raw) => {
                        match cmdline::command_spec_from_command_line(&raw, &target.target) {
                            Ok(command) => ObservedProtocolState::Registration {
                                command,
                                url_protocol_marker: marker,
                            },
                            Err(reason) => ObservedProtocolState::Malformed { reason },
                        }
                    }
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

fn inspect_file_associations<R: RegistryReader>(
    target: &TargetPlan,
    registry: &R,
) -> Result<Vec<ObservedFileAssociation>, InspectError> {
    let mut out = Vec::with_capacity(target.file_associations.len());
    for file_association in &target.file_associations {
        let id = file_association.id.as_str();
        let association_state = match registry.open_classes_key(file_association.scope, id)? {
            None => ObservedFileAssociationState::Absent,
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
                        ObservedFileAssociationState::Absent
                    }
                    None => ObservedFileAssociationState::Malformed {
                        reason: "missing shell\\open\\command".to_owned(),
                    },
                    Some(raw) => {
                        match cmdline::command_spec_from_command_line(&raw, &target.target) {
                            Ok(command) => ObservedFileAssociationState::Registration {
                                description,
                                command,
                            },
                            Err(reason) => ObservedFileAssociationState::Malformed { reason },
                        }
                    }
                }
            }
        };

        let ext = file_association.extension.as_str();
        let extension_state = match registry.open_classes_key(file_association.scope, ext)? {
            None => ObservedExtensionState::Absent,
            Some(key) => match R::read_value(&key, "") {
                RegistryValue::Sz(s) | RegistryValue::ExpandSz(s) if !s.is_empty() => {
                    ObservedExtensionState::Mapped { association_id: s }
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

        out.push(ObservedFileAssociation {
            key: file_association.key.clone(),
            extension: ext.to_owned(),
            id: id.to_owned(),
            scope: file_association.scope,
            association_state,
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
