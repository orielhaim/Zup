use std::collections::BTreeMap;
use zup_core::TargetPath;
use zup_plan::{
    BackendOperation, BackendOperationIntent, DeploymentEffects, FileRemoval,
    MAX_BACKEND_PAYLOAD_BYTES,
};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use zup_core::{AppId, BackendResourceId, Privilege, ResourceKey, SelectedScope};
use zup_plan::{
    ExecutionPlan, FileAssociationOperation, InstallationRecord, IntegrationChangeKind,
    LauncherOperation, OwnedResource, PathOperation, PathOperationKind, ProtocolOperation,
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
    RebindPath {
        scope: SelectedScope,
        previous: zup_plan::PathBinding,
        installed: zup_plan::PathBinding,
        privilege: Privilege,
    },
    RemovePath {
        scope: SelectedScope,
        entry: String,
        value_type: String,
        privilege: Privilege,
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
    node: &zup_plan::DeploymentNode,
    ledger: Option<&InstallationRecord>,
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
    let owned = ledger.and_then(|ledger| ledger.current.resources.get(&ledger_key));
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
                    if matches!(payload_owned.as_ref(), OwnedResource::Service(_)) {
                        return Err("services require typed native service operations".into());
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
            let payload_privilege = match &payload {
                ApplyPayload::Launcher(op) => op.privilege,
                ApplyPayload::Path(op) => op.privilege,
                ApplyPayload::Protocol(op) => op.privilege,
                ApplyPayload::FileAssociation(op) | ApplyPayload::Extension(op) => op.privilege,
                ApplyPayload::AppsFeatures { operation } => operation.privilege,
            };
            if payload_privilege != operation.privilege {
                return Err("backend payload privilege mismatch".into());
            }
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
                    use zup_plan::{PathPrecondition, PathPresence};
                    let authorized = match (&op.precondition, owned, op.kind) {
                        (
                            PathPrecondition::Unclaimed {
                                presence: PathPresence::Missing,
                            },
                            None,
                            PathOperationKind::Add,
                        ) => true,
                        (
                            PathPrecondition::Owned {
                                binding,
                                previous,
                                desired,
                            },
                            Some(OwnedResource::PathEntry {
                                value, value_type, ..
                            }),
                            kind,
                        ) if binding.value == *value && binding.value_type == *value_type => {
                            match kind {
                                PathOperationKind::RebindOwned => {
                                    *previous == PathPresence::Single
                                        && *desired == PathPresence::Missing
                                        && !value.equivalent(&op.value)
                                }
                                PathOperationKind::RestoreOwned => {
                                    *previous == PathPresence::Missing
                                        && *desired == PathPresence::Missing
                                        && value.equivalent(&op.value)
                                }
                                _ => false,
                            }
                        }
                        _ => false,
                    };
                    if op.scope != scope || !authorized {
                        return Err("search path ownership or observed binding mismatch".into());
                    }
                    Ok(())
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

pub(crate) fn validate_native_service(
    operation: &zup_plan::NativeServiceOperation,
    ledger: Option<&InstallationRecord>,
    scope: SelectedScope,
) -> Result<(), String> {
    if scope != SelectedScope::Machine {
        return Err("SCM requires machine scope".into());
    }
    let zup_plan::NativeServiceOperation::Configure {
        service: op,
        activation,
        ..
    } = operation
    else {
        return Ok(());
    };
    let startup_inhibited = *activation == zup_plan::ServiceActivation::Inhibited;
    let owned = ledger.and_then(|record| record.current.resources.get(operation.key()));
    if op.lifecycle.timeouts.is_none() {
        return Err("native service intent requires resolved timeouts".into());
    }
    op.lifecycle
        .validate(op.start, &op.execution)
        .map_err(str::to_owned)?;
    if op.previous.configuration().as_ref().is_some_and(|state| {
        matches!(
            state,
            zup_plan::ServiceState::Registration {
                failure: zup_core::ServiceFailurePolicy::Windows { .. },
                ..
            }
        )
    }) && !matches!(op.failure, zup_core::ServiceFailurePolicy::Windows { .. })
    {
        crate::service_control::require_safe_stopped_startup_restore(
            &op.name,
            &op.previous
                .configuration()
                .ok_or("service registration required")?,
        )?;
    }
    if startup_inhibited
        && !op
            .previous
            .configuration()
            .as_ref()
            .is_some_and(zup_plan::requires_startup_inhibition)
        && !zup_plan::requires_startup_inhibition(&op.desired_configuration())
    {
        return Err("service startup inhibition has no recovery policy".into());
    }
    let installed = match owned {
        Some(OwnedResource::Service(zup_plan::OwnedService {
            registration,
            name,
            installed,
            ..
        })) if *registration == op.registration && name.eq_ignore_ascii_case(&op.name) => {
            Some(installed.as_ref())
        }
        None => None,
        _ => return Err("service ownership address mismatch".into()),
    };
    let desired = op.desired_configuration();
    let previous = crate::integration::observed_service(op.previous.clone())
        .map_err(|error| error.to_string())?;
    if op.kind == IntegrationChangeKind::NoOp {
        return if installed == Some(&desired) && previous == desired {
            Ok(())
        } else {
            Err("unchanged service contract differs from protected ownership".into())
        };
    }
    validate_transition(
        op.kind,
        &previous,
        installed,
        &desired,
        &zup_plan::ServiceState::Absent,
    )
}

pub(crate) fn validate_service_control(
    plan: &zup_plan::DeploymentPlan,
    operation: &zup_plan::ServiceControlOperation,
    ledger: Option<&InstallationRecord>,
) -> Result<(), String> {
    if !operation
        .registration
        .belongs_to(plan.installation, plan.target)
    {
        return Err("service ownership does not match the authenticated installation".into());
    }
    let mut protected_files =
        match ledger.and_then(|ledger| ledger.current.resources.get(&operation.key)) {
            Some(OwnedResource::Service(zup_plan::OwnedService { requires_files, .. })) => {
                requires_files.clone()
            }
            _ => Vec::new(),
        };
    for node in &plan.nodes {
        if let zup_plan::NodeKind::NativeService { operation: native } = &node.kind
            && let zup_plan::NativeServiceOperation::Configure { service, .. } = native.as_ref()
            && service.key == operation.key
        {
            protected_files.extend(service.requires_files.clone());
        }
    }
    zup_plan::validate_runtime_preservation(plan, operation, &protected_files)?;
    let owned = ledger.and_then(|ledger| ledger.current.resources.get(&operation.key));
    if let Some(OwnedResource::Service(zup_plan::OwnedService {
        name,
        installed,
        timeouts,
        ..
    })) = owned
    {
        if operation.before_configuration != operation.configuration
            && operation.stage == zup_plan::ServiceControlStage::Restore
            && operation.after == zup_plan::ServiceTerminalState::Stopped
        {
            crate::service_control::require_safe_stopped_startup_restore(
                &operation.native_name,
                &operation.owned_configuration,
            )?;
        }
        if name != &operation.native_name || installed.as_ref() != &operation.owned_configuration {
            return Err("service control ownership address mismatch".into());
        }
        if *timeouts == operation.timeouts
            && (operation.configuration == **installed
                || operation.before_configuration == **installed
                    && operation.stage == zup_plan::ServiceControlStage::Quiesce)
        {
            return Ok(());
        }
    }
    for node in &plan.nodes {
        let zup_plan::NodeKind::NativeService { operation: native } = &node.kind else {
            continue;
        };
        let zup_plan::NativeServiceOperation::Configure { service, .. } = native.as_ref() else {
            continue;
        };
        let installed = service.desired_configuration();
        let repairing_live_configuration = operation.intent
            == zup_plan::ServiceControlIntent::Preserve
            && operation.stage == zup_plan::ServiceControlStage::Quiesce
            && service.kind == IntegrationChangeKind::RepairOwned
            && service.previous.configuration().as_ref() == Some(&operation.configuration)
            && installed == operation.owned_configuration;
        if service.key == operation.key
            && operation.timeouts
                == if operation.stage == zup_plan::ServiceControlStage::Quiesce {
                    match owned {
                        Some(OwnedResource::Service(zup_plan::OwnedService {
                            timeouts, ..
                        })) => *timeouts,
                        _ => service.lifecycle.timeouts_for(&service.execution),
                    }
                } else {
                    service.lifecycle.timeouts_for(&service.execution)
                }
            && service.name == operation.native_name
            && (installed == operation.configuration || repairing_live_configuration)
            && (owned.is_some()
                || service.kind == IntegrationChangeKind::Create
                    && service.previous == zup_plan::ObservedServiceState::Absent)
        {
            return Ok(());
        }
    }
    Err("service control is not bound to an owned registration transition".into())
}

pub(crate) fn validate_startup_inhibitions(
    plan: &zup_plan::DeploymentPlan,
    ledger: Option<&zup_plan::InstallationRecord>,
) -> Result<(), String> {
    for node in &plan.nodes {
        let zup_plan::NodeKind::NativeService { operation: native } = &node.kind else {
            continue;
        };
        let mut protected = Vec::new();
        let (key, previous, desired, guarded) = match native.as_ref() {
            zup_plan::NativeServiceOperation::Configure {
                service,
                activation,
                ..
            } => {
                protected.extend(service.requires_files.clone());
                protected.push(service.command.executable.clone());
                (
                    service.key.clone(),
                    service
                        .previous
                        .configuration()
                        .ok_or("service registration required")?,
                    service.desired_configuration(),
                    *activation == zup_plan::ServiceActivation::Inhibited,
                )
            }
            zup_plan::NativeServiceOperation::Remove {
                key,
                service,
                activation,
            } => {
                protected.extend(service.requires_files.clone());
                (
                    key.clone(),
                    service.installed.as_ref().clone(),
                    zup_plan::ServiceState::Absent,
                    *activation == zup_plan::ServiceActivation::Inhibited,
                )
            }
        };
        if let Some(OwnedResource::Service(zup_plan::OwnedService { requires_files, .. })) =
            ledger.and_then(|ledger| ledger.current.resources.get(&key))
        {
            protected.extend(requires_files.clone());
        }
        if let zup_plan::ServiceState::Registration { command, .. } = &previous {
            protected.push(command.executable.clone());
        }
        zup_plan::validate_startup_inhibition(
            plan, &key, &previous, &desired, &protected, guarded,
        )?;
    }
    Ok(())
}

pub(crate) fn ledger_key_for_backend_node(
    node: &zup_plan::DeploymentNode,
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
    let key = key.canonical_for(&zup_core::Target::WINDOWS_X64);
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
    pub(crate) transition: zup_plan::DeploymentTransition,
}

pub(crate) fn compile_execution_plan(
    execution: &ExecutionPlan,
    target: Option<&zup_plan::BoundDeployment>,
    scope: SelectedScope,
    app_id: &AppId,
    ledger: Option<&InstallationRecord>,
    apps: AppsPlanningInput,
) -> Result<DeploymentEffects, TransactionPayloadError> {
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
    if apps.app_id != *app_id {
        return Err(TransactionPayloadError::Identity(
            "Apps & Features app identity mismatch".into(),
        ));
    }
    let mut input = DeploymentEffects::new(binding.installation, binding.requirements.clone());
    input.observed_transaction = binding.observed_transaction;
    input.observed = binding.observed_activation;
    input.selected_components = execution.selected_components.clone();
    input.install_directory = target
        .map(|target| target.install_directory.clone())
        .or_else(|| execution.install_directory.clone());
    input.transition = execution.transition;

    input.preset = if execution.transition.is_remove() {
        None
    } else {
        target.and_then(|target| target.preset.clone())
    };

    for file in &execution.files {
        input.files.push(file.clone());
    }
    for removal in &execution.removals {
        if let OwnedResource::Service(zup_plan::OwnedService {
            registration,
            name,
            installed,
            privilege,
            timeouts,
            requires_files,
            ..
        }) = &removal.owned
        {
            use zup_transaction::NativeServiceManager;
            let mut control = zup_plan::ServiceControlOperation {
                requires_files: requires_files.clone(),
                registration: *registration,
                intent: zup_plan::ServiceControlIntent::Transition,
                key: removal.key.clone(),
                native_name: name.clone(),
                privilege: *privilege,
                stage: zup_plan::ServiceControlStage::Quiesce,
                before_configuration: installed.as_ref().clone(),
                configuration: installed.as_ref().clone(),
                owned_configuration: installed.as_ref().clone(),
                before: zup_plan::ServiceTerminalState::Running,
                after: zup_plan::ServiceTerminalState::Stopped,
                manager_generation: crate::service_control::manager_generation()
                    .map_err(TransactionPayloadError::Identity)?,
                before_execution_generation: "probe".into(),
                process_scope: zup_plan::ServiceScopeObservation::WindowsJob {
                    boot_sequence: crate::service_scope::boot_sequence()
                        .map_err(TransactionPayloadError::Identity)?,
                    state: zup_plan::WindowsJobState::Absent,
                },
                timeouts: *timeouts,
                dependencies: Vec::new(),
            };
            let current = crate::service_control::ScmLifecycle::default()
                .observe(&control)
                .map_err(TransactionPayloadError::Identity)?;
            if current.configuration != **installed {
                return Err(TransactionPayloadError::Identity(
                    "service changed before uninstall".into(),
                ));
            }
            match current.runtime {
                zup_plan::ServiceRuntimeState::Running | zup_plan::ServiceRuntimeState::Stopped => {
                    control.before = if current.runtime == zup_plan::ServiceRuntimeState::Running {
                        zup_plan::ServiceTerminalState::Running
                    } else {
                        zup_plan::ServiceTerminalState::Stopped
                    };
                    control.before_execution_generation = current.execution_generation;
                    control.process_scope = current.process_scope;
                    control.configuration =
                        zup_plan::disabled_startup(installed).expect("registration");
                    input.service_controls.push(control);
                }
                _ => {
                    return Err(TransactionPayloadError::Identity(
                        "service runtime is ambiguous during uninstall".into(),
                    ));
                }
            }
        }
        match &removal.owned {
            OwnedResource::File {
                instance,
                destination,
                sha256,
                size,
                created_directories,
                ..
            } => {
                input.removals.push(FileRemoval {
                    instance: *instance,
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
            OwnedResource::Service(service) => {
                input
                    .native_services
                    .push(zup_plan::NativeServiceOperation::Remove {
                        key: removal.key.clone(),
                        service: Box::new(service.clone()),
                        activation: if zup_plan::requires_startup_inhibition(&service.installed) {
                            zup_plan::ServiceActivation::Inhibited
                        } else {
                            zup_plan::ServiceActivation::Declared
                        },
                    });
                input.retired_keys.push(removal.key.clone());
            }
            OwnedResource::Launcher { .. }
            | OwnedResource::PathEntry { .. }
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
    let affected_services = execution.affected_services(ledger);
    for service in &execution.services {
        let owned = ledger.and_then(|ledger| ledger.current.resources.get(&service.key));
        let payload_changed = affected_services.contains(&service.key);
        let affected = payload_changed
            || service.previous.configuration().as_ref() != Some(&service.desired_configuration());
        let preserving = !payload_changed
            && service.preserves_runtime()
            && service.lifecycle.change == zup_core::ChangedServiceState::Preserve;
        let startup_inhibited = affected
            && !preserving
            && (service
                .previous
                .configuration()
                .as_ref()
                .is_some_and(zup_plan::requires_startup_inhibition)
                || zup_plan::requires_startup_inhibition(&service.desired_configuration()));
        if affected
            && (startup_inhibited
                || owned.is_some()
                || service.previous == zup_plan::ObservedServiceState::Absent
                    && service.lifecycle.install == zup_core::InitialServiceState::Running)
        {
            use zup_transaction::NativeServiceManager;
            let generation = crate::service_control::manager_generation()
                .map_err(TransactionPayloadError::Identity)?;
            let (execution_generation, process_scope) = if service.previous
                == zup_plan::ObservedServiceState::Absent
            {
                (
                    "stopped".into(),
                    crate::service_scope::observation(zup_plan::WindowsJobState::Absent)
                        .map_err(TransactionPayloadError::Identity)?,
                )
            } else {
                let configuration = service.previous.configuration().ok_or_else(|| {
                    TransactionPayloadError::NotExecutable("service registration".into())
                })?;
                let probe = zup_plan::ServiceControlOperation {
                    requires_files: service.requires_files.clone(),
                    registration: service.registration,
                    intent: zup_plan::ServiceControlIntent::Transition,
                    key: service.key.clone(),
                    native_name: service.name.clone(),
                    privilege: service.privilege,
                    stage: zup_plan::ServiceControlStage::Quiesce,
                    before_configuration: configuration.clone(),
                    configuration: configuration.clone(),
                    owned_configuration: configuration,
                    before: zup_plan::ServiceTerminalState::Running,
                    after: zup_plan::ServiceTerminalState::Stopped,
                    manager_generation: generation.clone(),
                    before_execution_generation: "probe".into(),
                    process_scope: zup_plan::ServiceScopeObservation::WindowsJob {
                        boot_sequence: crate::service_scope::boot_sequence()
                            .map_err(TransactionPayloadError::Identity)?,
                        state: zup_plan::WindowsJobState::Absent,
                    },
                    timeouts: match ledger
                        .and_then(|ledger| ledger.current.resources.get(&service.key))
                    {
                        Some(OwnedResource::Service(zup_plan::OwnedService {
                            timeouts, ..
                        })) => *timeouts,
                        _ => service.lifecycle.timeouts_for(&service.execution),
                    },
                    dependencies: Vec::new(),
                };
                let observation = crate::service_control::ScmLifecycle::default()
                    .observe(&probe)
                    .map_err(TransactionPayloadError::Identity)?;
                (observation.execution_generation, observation.process_scope)
            };
            input.service_controls.extend(
                zup_plan::plan_service_controls(
                    service,
                    owned,
                    payload_changed,
                    service.name.clone(),
                    generation,
                    execution_generation,
                    process_scope,
                )
                .map_err(TransactionPayloadError::Identity)?,
            );
            let previous = service.previous.configuration().ok_or_else(|| {
                TransactionPayloadError::Identity("service registration required".into())
            })?;
            let retiring_recovery = matches!(
                previous,
                zup_plan::ServiceState::Registration {
                    failure: zup_core::ServiceFailurePolicy::Windows { .. },
                    ..
                }
            ) && !matches!(
                service.failure,
                zup_core::ServiceFailurePolicy::Windows { .. }
            );
            let restores_running =
                matches!(service.previous, zup_plan::ObservedServiceState::Absent)
                    && service.lifecycle.install == zup_core::InitialServiceState::Running
                    || matches!(
                        service.previous,
                        zup_plan::ObservedServiceState::Service {
                            runtime_state: Some(zup_plan::ServiceRuntimeState::Running),
                            ..
                        }
                    ) && service.lifecycle.change == zup_core::ChangedServiceState::Preserve;
            if retiring_recovery
                || startup_inhibited
                    && !restores_running
                    && previous != zup_plan::ServiceState::Absent
                    && service.start != zup_core::ServiceStart::Disabled
            {
                crate::service_control::require_safe_stopped_startup_restore(
                    &service.name,
                    &previous,
                )
                .map_err(TransactionPayloadError::Identity)?;
            }
        }
        if matches!(
            service.kind,
            IntegrationChangeKind::Conflict | IntegrationChangeKind::Drift
        ) {
            return Err(TransactionPayloadError::NotExecutable("service".into()));
        }
        let mut native_service = service.clone();
        native_service.lifecycle = native_service.lifecycle.resolved(&native_service.execution);
        input
            .native_services
            .push(zup_plan::NativeServiceOperation::Configure {
                service: Box::new(native_service),
                previous: match owned {
                    Some(OwnedResource::Service(service)) => Some(Box::new(service.clone())),
                    None => None,
                    _ => {
                        return Err(TransactionPayloadError::Identity(
                            "service ownership type mismatch".into(),
                        ));
                    }
                },
                activation: if startup_inhibited {
                    zup_plan::ServiceActivation::Inhibited
                } else {
                    zup_plan::ServiceActivation::Declared
                },
            });
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
            let extension_key = ResourceKey::FileAssociationExtension { extension }
                .canonical_for(&zup_core::Target::WINDOWS_X64);
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
    } else if (apps.transition.is_remove() || apps.desired.is_none()) && apps.owned.is_some() {
        input.retired_keys.push(ResourceKey::Backend {
            id: apps_backend_id(&apps.app_id),
        });
    }
    zup_plan::bind_service_dependencies(&mut input.service_controls, execution, ledger)
        .map_err(TransactionPayloadError::Identity)?;
    input
        .bind_generations(target)
        .map_err(|error| TransactionPayloadError::Identity(error.to_string()))?;
    Ok(input)
}

fn compile_apps_operation(
    apps: &AppsPlanningInput,
    scope: SelectedScope,
) -> Result<Option<BackendOperation>, TransactionPayloadError> {
    let remove = apps.transition.is_remove() || apps.desired.is_none();
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
            let mut input = DeploymentEffects::new(
                "00000000-0000-7000-8000-000000000001".parse().unwrap(),
                crate::requirements_fixture::requirements(
                    zup_core::Target::parse("windows-x64").unwrap(),
                    zup_core::SelectedScope::User,
                    "example.application",
                    "1.0.0",
                ),
            );
            input.install_directory =
                Some(zup_core::TargetPath::new(input.target, r"C:\Programs\Test").unwrap());
            input.backend_operations.push(BackendOperation {
                key: apps_key(&app),
                id: apps_backend_id(&app),
                privilege: Privilege::User,
                intent: BackendOperationIntent::Apply,
                payload: encode(&ApplyPayload::AppsFeatures { operation }).unwrap(),
                dependencies: vec![],
            });
            let plan = zup_plan::compile_deployment(&input).unwrap();
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
