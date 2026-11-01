use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use zup_core::{AppId, BackendResourceId, Privilege, ResourceKey, SelectedScope};
use zup_plan::{
    DesiredState, ExecutionPlan, FileAssociationOperation, InstallLedger, IntegrationChangeKind,
    LauncherOperation, OwnedResource, PathOperation, PathOperationKind, ProtocolOperation,
    ServiceOperation, TargetPath,
};

use zup_transaction::{
    BackendOperation, BackendOperationIntent, FileRemoval, MAX_BACKEND_PAYLOAD_BYTES,
    TransactionInput,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppsFeaturesState {
    pub values: BTreeMap<String, AppsFeaturesValue>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value")]
pub enum AppsFeaturesValue {
    String(String),
    Dword(u32),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppsFeaturesOperation {
    pub scope: SelectedScope,
    pub privilege: Privilege,
    pub key_path: String,
    pub previous: Option<AppsFeaturesState>,
    pub installed: AppsFeaturesState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value")]
pub(crate) enum ApplyPayload {
    Launcher(LauncherOperation),
    Path(PathOperation),
    Service(ServiceOperation),
    Protocol(ProtocolOperation),
    FileAssociation(FileAssociationOperation),
    Extension(FileAssociationOperation),
    AppsFeatures { operation: AppsFeaturesOperation },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value")]
pub(crate) enum RemovePayload {
    Owned {
        scope: SelectedScope,
        key: ResourceKey,

        owned: Box<OwnedResource>,
    },
    AppsFeatures {
        scope: SelectedScope,
        key_path: String,
        state: AppsFeaturesState,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub(crate) enum BackendReceipt {
    Launcher {
        launcher_path: TargetPath,
        privilege: Privilege,
        previous: zup_plan::LauncherState,
        installed: zup_plan::LauncherState,
    },
    Path {
        scope: SelectedScope,
        entry: String,
        value_type: String,
        privilege: Privilege,
    },
    RemovePath {
        scope: SelectedScope,
        entry: String,
        value_type: String,
        privilege: Privilege,
    },
    Service {
        name: String,
        privilege: Privilege,
        previous: zup_plan::ServiceState,
        installed: zup_plan::ServiceState,
    },
    Protocol {
        scope: SelectedScope,
        scheme: String,
        privilege: Privilege,
        previous: zup_plan::ProtocolState,
        installed: zup_plan::ProtocolState,
    },
    FileAssociation {
        scope: SelectedScope,
        id: String,
        privilege: Privilege,
        previous: zup_plan::FileAssociationState,
        installed: zup_plan::FileAssociationState,
    },
    Extension {
        scope: SelectedScope,
        extension: String,
        privilege: Privilege,
        previous: zup_plan::ExtensionState,
        installed: zup_plan::ExtensionState,
    },
    AppsFeatures {
        scope: SelectedScope,
        key_path: String,
        privilege: Privilege,
        previous: Option<AppsFeaturesState>,
        installed: Option<AppsFeaturesState>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NativeReconcileResult {
    NotApplied,

    AppliedWithReceipt(Box<BackendReceipt>),
    Ambiguous,
}

#[derive(Debug, Error)]
pub enum TransactionPayloadError {
    #[error("transaction payload serialization failed: {0}")]
    Serialization(String),
    #[error("transaction payload exceeds {max} bytes")]
    PayloadTooLarge { max: usize },
    #[error("transaction operation is not executable: {0}")]
    NotExecutable(String),
    #[error("transaction operation has an unsupported Windows shape: {0}")]
    Unsupported(String),
    #[error("backend operation identity mismatch for `{0}`")]
    Identity(String),
    #[error("Apps & Features registration is occupied or changed")]
    AppsRegistrationConflict,
}

pub(crate) fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, TransactionPayloadError> {
    let bytes = serde_json::to_vec(value)
        .map_err(|error| TransactionPayloadError::Serialization(error.to_string()))?;
    if bytes.len() > MAX_BACKEND_PAYLOAD_BYTES {
        return Err(TransactionPayloadError::PayloadTooLarge {
            max: MAX_BACKEND_PAYLOAD_BYTES,
        });
    }
    Ok(bytes)
}

pub(crate) fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, TransactionPayloadError> {
    if bytes.len() > MAX_BACKEND_PAYLOAD_BYTES {
        return Err(TransactionPayloadError::PayloadTooLarge {
            max: MAX_BACKEND_PAYLOAD_BYTES,
        });
    }
    serde_json::from_slice(bytes)
        .map_err(|error| TransactionPayloadError::Serialization(error.to_string()))
}

use serde::de::DeserializeOwned;

pub(crate) fn backend_id_for_key(key: &ResourceKey) -> BackendResourceId {
    let encoded = serde_json::to_string(key).expect("resource key serializes");
    BackendResourceId::new(format!("windows:{encoded}")).expect("backend id is non-empty")
}

pub(crate) fn apps_backend_id(app_id: &AppId) -> BackendResourceId {
    BackendResourceId::new(format!("windows:apps-features:{}", app_id.as_str()))
        .expect("apps backend id is non-empty")
}

pub(crate) fn apps_key(app_id: &AppId) -> ResourceKey {
    ResourceKey::Backend {
        id: apps_backend_id(app_id),
    }
}

pub(crate) fn apps_owned_payload(state: &AppsFeaturesState) -> Vec<u8> {
    encode(state).expect("Apps & Features state is bounded")
}

pub(crate) fn decode_apps_owned_payload(
    bytes: &[u8],
) -> Result<AppsFeaturesState, TransactionPayloadError> {
    decode(bytes)
}

fn backend_key(key: &ResourceKey) -> ResourceKey {
    ResourceKey::Backend {
        id: backend_id_for_key(key),
    }
}

fn backend_operation<T: Serialize>(
    source_key: &ResourceKey,
    privilege: Privilege,
    payload: &T,
    dependencies: Vec<ResourceKey>,
) -> Result<BackendOperation, TransactionPayloadError> {
    let key = backend_key(source_key);
    let id = match &key {
        ResourceKey::Backend { id } => id.clone(),
        _ => return Err(TransactionPayloadError::Identity(format!("{key:?}"))),
    };
    Ok(BackendOperation {
        key,
        id,
        privilege,
        intent: BackendOperationIntent::Apply,
        payload: encode(payload)?,
        dependencies: dependencies
            .into_iter()
            .map(|dependency| backend_key(&dependency))
            .collect(),
    })
}

fn backend_removal<T: Serialize>(
    key: &ResourceKey,
    privilege: Privilege,
    payload: &T,
    dependencies: Vec<ResourceKey>,
) -> Result<BackendOperation, TransactionPayloadError> {
    let backend_resource_key = backend_key(key);
    let id = match &backend_resource_key {
        ResourceKey::Backend { id } => id.clone(),
        _ => {
            return Err(TransactionPayloadError::Identity(format!(
                "{backend_resource_key:?}"
            )));
        }
    };
    Ok(BackendOperation {
        key: backend_resource_key,
        id,
        privilege,
        intent: BackendOperationIntent::Remove,
        payload: encode(payload)?,
        dependencies: dependencies
            .into_iter()
            .map(|dependency| backend_key(&dependency))
            .collect(),
    })
}

pub(crate) fn validate_backend_node(
    node: &zup_transaction::TransactionNode,
    ledger: Option<&InstallLedger>,
    scope: SelectedScope,
    app_id: &AppId,
) -> Result<(), String> {
    let operation = node
        .meta
        .backend
        .as_ref()
        .ok_or_else(|| "backend node has no operation".to_owned())?;
    if !matches!(operation.key, ResourceKey::Backend { .. })
        || operation.key
            != (ResourceKey::Backend {
                id: operation.id.clone(),
            })
    {
        return Err("backend operation identity is invalid".to_owned());
    }
    let ledger_key = ledger_key_for_backend_node(node).map_err(|error| error.to_string())?;
    let owned = ledger.and_then(|ledger| ledger.resources.get(&ledger_key));
    if node.meta.privilege != Some(operation.privilege)
        || owned.is_some_and(|owned| owned.privilege() != operation.privilege)
    {
        return Err("backend privilege mismatch".into());
    }

    match operation.intent {
        BackendOperationIntent::Remove => {
            let payload: RemovePayload =
                decode(&operation.payload).map_err(|error| error.to_string())?;
            match payload {
                RemovePayload::Owned {
                    scope: payload_scope,
                    key,
                    owned: payload_owned,
                } => {
                    if payload_scope != scope || owned != Some(payload_owned.as_ref()) {
                        return Err("backend removal ownership mismatch".to_owned());
                    }
                    let expected = backend_key(&key);
                    if expected != operation.key {
                        return Err("backend removal key mismatch".to_owned());
                    }
                }
                RemovePayload::AppsFeatures {
                    scope: payload_scope,
                    key_path,
                    state,
                } => {
                    let Some(OwnedResource::Backend { payload, .. }) = owned else {
                        return Err("Apps & Features ownership is missing".to_owned());
                    };
                    if payload_scope != scope
                        || decode_apps_owned_payload(payload).ok().as_ref() != Some(&state)
                        || key_path
                            != format!(
                                "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\{}",
                                app_id
                            )
                        || operation.key != apps_key(app_id)
                    {
                        return Err("Apps & Features ownership mismatch".to_owned());
                    }
                }
            }
            Ok(())
        }
        BackendOperationIntent::Apply => {
            let payload: ApplyPayload =
                decode(&operation.payload).map_err(|error| error.to_string())?;
            match payload {
                ApplyPayload::Launcher(op) => {
                    let installed = match owned {
                        Some(OwnedResource::Launcher {
                            launcher_path,
                            installed,
                            ..
                        }) if launcher_path == &op.launcher_path => Some(installed),
                        None => None,
                        _ => return Err("launcher ownership address mismatch".into()),
                    };
                    let previous = crate::integration::observed_launcher(op.previous)
                        .map_err(|error| error.to_string())?;
                    validate_transition(
                        op.kind,
                        &previous,
                        installed,
                        &zup_plan::LauncherState::Launcher {
                            target: op.target,
                            arguments: op.arguments,
                            working_directory: op.working_directory,
                        },
                        &zup_plan::LauncherState::Absent,
                    )
                }
                ApplyPayload::Path(op) => {
                    let matches = matches!(owned, Some(OwnedResource::PathEntry { value, .. }) if value == &op.value);
                    if op.scope != scope
                        || op.previously_owned != matches
                        || match op.kind {
                            PathOperationKind::Add => owned.is_some() || op.present,
                            PathOperationKind::UpdateOwned => !matches || !op.present,
                            PathOperationKind::RestoreOwned => !matches || op.present,
                            _ => true,
                        }
                    {
                        return Err("search path ownership mismatch".into());
                    }
                    Ok(())
                }
                ApplyPayload::Service(op) => {
                    let installed = match owned {
                        Some(OwnedResource::Service {
                            name, installed, ..
                        }) if name.eq_ignore_ascii_case(&op.name) => Some(installed),
                        None => None,
                        _ => return Err("service ownership address mismatch".into()),
                    };
                    let previous = crate::integration::observed_service(op.previous)
                        .map_err(|error| error.to_string())?;
                    validate_transition(
                        op.kind,
                        &previous,
                        installed,
                        &zup_plan::ServiceState::Registration {
                            display_name: op.display_name,
                            command: op.command,
                            start: op.start,
                        },
                        &zup_plan::ServiceState::Absent,
                    )
                }
                ApplyPayload::Protocol(op) => {
                    if op.scope != scope {
                        return Err("protocol scope mismatch".into());
                    }
                    let installed = match owned {
                        Some(OwnedResource::Protocol { installed, .. }) => Some(installed),
                        None => None,
                        _ => return Err("protocol ownership mismatch".into()),
                    };
                    let previous = crate::integration::protocol_from_observed(&op.previous)
                        .map_err(|error| error.to_string())?;
                    validate_transition(
                        op.kind,
                        &previous,
                        installed,
                        &zup_plan::ProtocolState::Registration {
                            command: op.command,
                        },
                        &zup_plan::ProtocolState::Absent,
                    )
                }
                ApplyPayload::FileAssociation(op) => {
                    if op.scope != scope {
                        return Err("association scope mismatch".into());
                    }
                    let installed = match owned {
                        Some(OwnedResource::FileAssociation { installed, .. }) => Some(installed),
                        None => None,
                        _ => return Err("association ownership mismatch".into()),
                    };
                    let previous = crate::integration::file_association_from_observed(
                        &op.previous_association,
                    )
                    .map_err(|error| error.to_string())?;
                    validate_transition(
                        op.association_kind,
                        &previous,
                        installed,
                        &zup_plan::FileAssociationState::Registration {
                            description: op.description,
                            command: op.command,
                        },
                        &zup_plan::FileAssociationState::Absent,
                    )
                }
                ApplyPayload::Extension(op) => {
                    if op.scope != scope {
                        return Err("extension scope mismatch".into());
                    }
                    let installed = match owned {
                        Some(OwnedResource::Extension { installed, .. }) => Some(installed),
                        None => None,
                        _ => return Err("extension ownership mismatch".into()),
                    };
                    let previous =
                        crate::integration::extension_from_observed(&op.previous_extension)
                            .map_err(|error| error.to_string())?;
                    validate_transition(
                        op.extension_kind,
                        &previous,
                        installed,
                        &zup_plan::ExtensionState::Mapped {
                            association_id: op.id,
                        },
                        &zup_plan::ExtensionState::Absent,
                    )
                }
                ApplyPayload::AppsFeatures {
                    operation: apps_operation,
                } => {
                    if apps_operation.scope != scope
                        || apps_operation.privilege != operation.privilege
                        || operation.key != apps_key(app_id)
                        || apps_operation.key_path
                            != format!(
                                "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\{}",
                                app_id
                            )
                    {
                        return Err("Apps & Features address mismatch".into());
                    }
                    let Some(OwnedResource::Backend { id, payload, .. }) = owned else {
                        if apps_operation.previous.is_some() {
                            return Err("Apps & Features ownership is missing".to_owned());
                        }
                        return Ok(());
                    };
                    if id != &operation.id {
                        return Err("Apps & Features ownership identity mismatch".to_owned());
                    }
                    if decode_apps_owned_payload(payload).ok().as_ref()
                        != apps_operation.previous.as_ref()
                    {
                        return Err("Apps & Features ownership state mismatch".to_owned());
                    }
                    Ok(())
                }
            }
        }
    }
}

pub(crate) fn ledger_key_for_backend_node(
    node: &zup_transaction::TransactionNode,
) -> Result<ResourceKey, TransactionPayloadError> {
    let operation = node.meta.backend.as_ref().ok_or_else(|| {
        TransactionPayloadError::NotExecutable("backend node has no operation".into())
    })?;
    let key = match operation.intent {
        BackendOperationIntent::Apply => match decode::<ApplyPayload>(&operation.payload)
            .map_err(|error| TransactionPayloadError::Identity(error.to_string()))?
        {
            ApplyPayload::Launcher(operation) => operation.key,
            ApplyPayload::Path(operation) => operation.key,
            ApplyPayload::Service(operation) => operation.key,
            ApplyPayload::Protocol(operation) => operation.key,
            ApplyPayload::FileAssociation(operation) => operation.key,
            ApplyPayload::Extension(operation) => ResourceKey::FileAssociationExtension {
                extension: zup_core::FileExtension::new(&operation.extension)
                    .map_err(|error| TransactionPayloadError::Identity(error.to_string()))?,
            },
            ApplyPayload::AppsFeatures { .. } => operation.key.clone(),
        },
        BackendOperationIntent::Remove => match decode::<RemovePayload>(&operation.payload)
            .map_err(|error| TransactionPayloadError::Identity(error.to_string()))?
        {
            RemovePayload::Owned { key, .. } => key,
            RemovePayload::AppsFeatures { .. } => operation.key.clone(),
        },
    };
    let expected_key = if matches!(key, ResourceKey::Backend { .. }) {
        key.clone()
    } else {
        backend_key(&key)
    };
    if expected_key != operation.key {
        return Err(TransactionPayloadError::Identity(
            "backend operation key does not match its payload".into(),
        ));
    }
    Ok(key)
}

fn validate_transition<T: PartialEq>(
    kind: IntegrationChangeKind,
    observed: &T,
    installed: Option<&T>,
    desired: &T,
    absent: &T,
) -> Result<(), String> {
    let valid = match kind {
        IntegrationChangeKind::Create => installed.is_none() && observed == absent,
        IntegrationChangeKind::UpdateOwned => installed == Some(observed),
        IntegrationChangeKind::RestoreOwned => observed == absent && installed == Some(desired),
        IntegrationChangeKind::RepairOwned => observed != absent && installed == Some(desired),
        _ => false,
    };
    valid
        .then_some(())
        .ok_or_else(|| "backend ownership precondition mismatch".into())
}

pub(crate) struct AppsPlanningInput {
    pub(crate) app_id: AppId,

    pub(crate) privilege: Privilege,
    pub(crate) current: Option<AppsFeaturesState>,
    pub(crate) owned: Option<AppsFeaturesState>,
    pub(crate) desired: Option<AppsFeaturesState>,
    pub(crate) key_path: String,
    pub(crate) uninstall: bool,
}

pub(crate) fn compile_execution_plan(
    execution: &ExecutionPlan,
    target: Option<&DesiredState<zup_plan::TargetPath, zup_plan::TargetPath>>,
    scope: SelectedScope,
    app_id: &AppId,
    ledger: Option<&InstallLedger>,
    apps: AppsPlanningInput,
) -> Result<TransactionInput, TransactionPayloadError> {
    let binding = execution
        .validate_desired(target)
        .map_err(|error| TransactionPayloadError::Identity(error.to_string()))?;
    execution
        .validate_generation(ledger)
        .map_err(|error| TransactionPayloadError::Identity(error.to_string()))?;
    if binding.app_id != *app_id || binding.scope != scope {
        return Err(TransactionPayloadError::Identity(
            "deployment identity mismatch".into(),
        ));
    }
    let target_triple = binding.target;
    if apps.app_id != *app_id {
        return Err(TransactionPayloadError::Identity(
            "Apps & Features app identity mismatch".into(),
        ));
    }
    let mut input = TransactionInput::new(target_triple);
    input.observed_transaction = binding.observed_transaction;
    input.selected_components = execution.selected_components.clone();
    input.install_directory = target
        .map(|target| target.install_directory.clone())
        .or_else(|| execution.install_directory.clone());
    input.uninstall = execution.uninstall;

    input.preset = if execution.uninstall {
        None
    } else {
        target.and_then(|target| target.preset.clone())
    };

    for file in &execution.files {
        input.files.push(file.clone());
    }
    for removal in &execution.removals {
        match &removal.owned {
            OwnedResource::File {
                destination,
                sha256,
                size,
                created_directories,
                ..
            } => {
                input.removals.push(FileRemoval {
                    key: removal.key.clone(),
                    kind: removal.kind,
                    scope: removal.scope,
                    privilege: removal.privilege,
                    destination: destination.clone(),
                    sha256: *sha256,
                    size: *size,
                    created_directories: created_directories.clone(),
                });
                input.retired_keys.push(removal.key.clone());
            }
            OwnedResource::Launcher { .. }
            | OwnedResource::PathEntry { .. }
            | OwnedResource::Service { .. }
            | OwnedResource::Protocol { .. }
            | OwnedResource::FileAssociation { .. }
            | OwnedResource::Extension { .. } => {
                let payload = RemovePayload::Owned {
                    scope: removal.scope,
                    key: removal.key.clone(),
                    owned: Box::new(removal.owned.clone()),
                };
                let operation =
                    backend_removal(&removal.key, removal.privilege, &payload, Vec::new())?;
                input.retired_keys.push(operation.key.clone());
                input.backend_operations.push(operation);
            }
            OwnedResource::Backend { .. } => {}
        }
    }

    for launcher in &execution.launchers {
        if launcher.kind == IntegrationChangeKind::NoOp {
            continue;
        }
        if matches!(
            launcher.kind,
            IntegrationChangeKind::Conflict | IntegrationChangeKind::Drift
        ) {
            return Err(TransactionPayloadError::NotExecutable("launcher".into()));
        }
        input.backend_operations.push(backend_operation(
            &launcher.key,
            launcher.privilege,
            &ApplyPayload::Launcher(launcher.clone()),
            Vec::new(),
        )?);
    }
    for path in &execution.path_entries {
        if path.kind == PathOperationKind::Present {
            continue;
        }
        if matches!(
            path.kind,
            PathOperationKind::Conflict | PathOperationKind::Drift
        ) {
            return Err(TransactionPayloadError::NotExecutable("path".into()));
        }
        input.backend_operations.push(backend_operation(
            &path.key,
            path.privilege,
            &ApplyPayload::Path(path.clone()),
            Vec::new(),
        )?);
    }
    for service in &execution.services {
        if service.kind == IntegrationChangeKind::NoOp {
            continue;
        }
        if matches!(
            service.kind,
            IntegrationChangeKind::Conflict | IntegrationChangeKind::Drift
        ) {
            return Err(TransactionPayloadError::NotExecutable("service".into()));
        }
        input.backend_operations.push(backend_operation(
            &service.key,
            service.privilege,
            &ApplyPayload::Service(service.clone()),
            Vec::new(),
        )?);
    }
    for protocol in &execution.protocols {
        if protocol.kind == IntegrationChangeKind::NoOp {
            continue;
        }
        if matches!(
            protocol.kind,
            IntegrationChangeKind::Conflict | IntegrationChangeKind::Drift
        ) {
            return Err(TransactionPayloadError::NotExecutable("protocol".into()));
        }
        input.backend_operations.push(backend_operation(
            &protocol.key,
            protocol.privilege,
            &ApplyPayload::Protocol(protocol.clone()),
            Vec::new(),
        )?);
    }
    for association in &execution.file_associations {
        if association.association_kind == IntegrationChangeKind::NoOp
            && association.extension_kind == IntegrationChangeKind::NoOp
        {
            continue;
        }
        if matches!(
            association.association_kind,
            IntegrationChangeKind::Conflict | IntegrationChangeKind::Drift
        ) || matches!(
            association.extension_kind,
            IntegrationChangeKind::Conflict | IntegrationChangeKind::Drift
        ) {
            return Err(TransactionPayloadError::NotExecutable(
                "file association".into(),
            ));
        }
        if association.association_kind != IntegrationChangeKind::NoOp {
            input.backend_operations.push(backend_operation(
                &association.key,
                association.privilege,
                &ApplyPayload::FileAssociation(association.clone()),
                Vec::new(),
            )?);
        }
        if association.extension_kind != IntegrationChangeKind::NoOp {
            let extension = zup_core::FileExtension::new(&association.extension)
                .map_err(|error| TransactionPayloadError::Unsupported(error.to_string()))?;
            let extension_key = ResourceKey::FileAssociationExtension { extension };
            let dependencies = if association.association_kind == IntegrationChangeKind::NoOp {
                Vec::new()
            } else {
                vec![association.key.clone()]
            };
            input.backend_operations.push(backend_operation(
                &extension_key,
                association.privilege,
                &ApplyPayload::Extension(association.clone()),
                dependencies,
            )?);
        }
    }

    if let Some(operation) = compile_apps_operation(&apps, scope)? {
        if operation.intent == BackendOperationIntent::Remove {
            input.retired_keys.push(operation.key.clone());
        }
        input.backend_operations.push(operation);
    } else if (apps.uninstall || apps.desired.is_none()) && apps.owned.is_some() {
        input.retired_keys.push(ResourceKey::Backend {
            id: apps_backend_id(&apps.app_id),
        });
    }
    Ok(input)
}

fn compile_apps_operation(
    apps: &AppsPlanningInput,
    scope: SelectedScope,
) -> Result<Option<BackendOperation>, TransactionPayloadError> {
    let remove = apps.uninstall || apps.desired.is_none();
    if remove {
        let Some(state) = apps.owned.clone() else {
            return Ok(None);
        };
        if apps
            .current
            .as_ref()
            .is_some_and(|current| current != &state)
        {
            return Ok(None);
        }
        if apps.current.is_none() {
            return Ok(None);
        }
        let id = apps_backend_id(&apps.app_id);
        let key = ResourceKey::Backend { id: id.clone() };
        return Ok(Some(BackendOperation {
            key,
            id,
            privilege: apps.privilege,
            intent: BackendOperationIntent::Remove,
            payload: encode(&RemovePayload::AppsFeatures {
                scope,
                key_path: apps.key_path.clone(),
                state,
            })?,
            dependencies: Vec::new(),
        }));
    }
    let desired = apps
        .desired
        .clone()
        .expect("non-removal Apps & Features planning has a desired state");
    if apps.current == Some(desired.clone()) {
        return Ok(None);
    }
    if apps.current.is_some() && apps.owned.is_none() {
        return Err(TransactionPayloadError::AppsRegistrationConflict);
    }
    if apps.owned.as_ref().is_some_and(|owned| {
        apps.current
            .as_ref()
            .is_some_and(|current| current != owned)
    }) {
        return Err(TransactionPayloadError::AppsRegistrationConflict);
    }
    let id = apps_backend_id(&apps.app_id);
    let key = ResourceKey::Backend { id: id.clone() };
    let operation = AppsFeaturesOperation {
        scope,
        privilege: apps.privilege,
        key_path: apps.key_path.clone(),
        previous: apps.current.clone(),
        installed: desired,
    };
    Ok(Some(BackendOperation {
        key,
        id,
        privilege: apps.privilege,
        intent: BackendOperationIntent::Apply,
        payload: encode(&ApplyPayload::AppsFeatures { operation })?,
        dependencies: Vec::new(),
    }))
}

pub(crate) fn receipt_bytes(receipt: &BackendReceipt) -> Result<Vec<u8>, TransactionPayloadError> {
    encode(receipt)
}

pub(crate) fn receipt_from_bytes(bytes: &[u8]) -> Result<BackendReceipt, TransactionPayloadError> {
    decode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registration_authority_cannot_be_retargeted_to_another_registry_key() {
        let app = AppId::new("com.zup.authority").unwrap();
        let scope = SelectedScope::User;
        let operation = AppsFeaturesOperation {
            scope,
            privilege: Privilege::User,
            key_path: format!("Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\{app}"),
            previous: None,
            installed: AppsFeaturesState {
                values: BTreeMap::new(),
            },
        };
        for (path, permitted) in [
            (operation.key_path.clone(), true),
            ("Software\\Foreign".into(), false),
        ] {
            let mut operation = operation.clone();
            operation.key_path = path;
            let mut input = TransactionInput::new(zup_core::Target::parse("windows-x64").unwrap());
            input.backend_operations.push(BackendOperation {
                key: apps_key(&app),
                id: apps_backend_id(&app),
                privilege: Privilege::User,
                intent: BackendOperationIntent::Apply,
                payload: encode(&ApplyPayload::AppsFeatures { operation }).unwrap(),
                dependencies: vec![],
            });
            let plan = zup_transaction::compile_transaction(&input).unwrap();
            let node = plan
                .nodes
                .iter()
                .find(|node| node.meta.backend.is_some())
                .unwrap();
            assert_eq!(
                validate_backend_node(node, None, scope, &app).is_ok(),
                permitted
            );
        }
    }

    #[test]
    fn owned_updates_and_repairs_require_the_correct_prior_authority() {
        assert!(
            validate_transition(IntegrationChangeKind::UpdateOwned, &1, Some(&1), &2, &0).is_ok()
        );
        assert!(
            validate_transition(IntegrationChangeKind::UpdateOwned, &2, Some(&1), &2, &0).is_err()
        );
        assert!(
            validate_transition(IntegrationChangeKind::RepairOwned, &2, Some(&1), &1, &0).is_ok()
        );
        assert!(validate_transition(IntegrationChangeKind::RepairOwned, &2, None, &1, &0).is_err());
        assert!(
            validate_transition(IntegrationChangeKind::RepairOwned, &2, Some(&1), &3, &0).is_err()
        );
    }
}
