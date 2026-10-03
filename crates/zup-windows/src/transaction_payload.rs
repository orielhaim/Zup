use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use zup_core::{AppId, BackendResourceId, Privilege, ResourceKey, SelectedScope};
use zup_exec::{
    ExecutionPlan, FileAssociationOperation, FileAssociationOperationKind, FileOperationKind,
    InstallLedger, LauncherOperation, LauncherOperationKind, OwnedResource, PathOperation,
    PathOperationKind, ProtocolOperation, ProtocolOperationKind, ServiceOperation,
    ServiceOperationKind,
};
use zup_platform::{TargetPath, TargetPlan};
use zup_transaction::{
    BackendOperation, BackendOperationIntent, FileDelta, FilePrecondition, FileRemoval,
    FileRemovalKind, FileWork, MAX_BACKEND_PAYLOAD_BYTES, TransactionInput,
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

/// One Apps & Features registration write.
///
/// `scope` selects the host store; `privilege` states the authority the write
/// needs. Neither is derived from the other.
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
        /// Boxed behind an owned handle: `OwnedResource` is an order of
        /// magnitude wider than the Apps & Features arm, and a removal payload
        /// is decoded once, inspected, and dropped.
        owned: Box<OwnedResource>,
    },
    AppsFeatures {
        scope: SelectedScope,
        key_path: String,
        state: AppsFeaturesState,
    },
}

/// Durable proof of one applied host operation.
///
/// `privilege` is recorded here so ownership can be written back to the ledger
/// with the exact authority the operation needed, instead of re-deriving one
/// from the scope the application happens to live in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub(crate) enum BackendReceipt {
    Launcher {
        launcher_path: TargetPath,
        privilege: Privilege,
        previous: zup_exec::LauncherState,
        installed: zup_exec::LauncherState,
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
        previous: zup_exec::ServiceState,
        installed: zup_exec::ServiceState,
    },
    Protocol {
        scope: SelectedScope,
        scheme: String,
        privilege: Privilege,
        previous: zup_exec::ProtocolState,
        installed: zup_exec::ProtocolState,
    },
    FileAssociation {
        scope: SelectedScope,
        id: String,
        privilege: Privilege,
        previous: zup_exec::FileAssociationState,
        installed: zup_exec::FileAssociationState,
    },
    Extension {
        scope: SelectedScope,
        extension: String,
        privilege: Privilege,
        previous: zup_exec::ExtensionState,
        installed: zup_exec::ExtensionState,
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
    /// Boxed behind an owned handle: a receipt carries whole before/after
    /// states, so inlining it would make every `NotApplied` and `Ambiguous`
    /// result as wide as a completed reconciliation.
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

fn file_delta(kind: FileOperationKind) -> Result<FileDelta, TransactionPayloadError> {
    Ok(match kind {
        FileOperationKind::Create => FileDelta::Create,
        FileOperationKind::Replace => FileDelta::Replace,
        FileOperationKind::RestoreOwned => FileDelta::RestoreOwned,
        FileOperationKind::RepairOwned => FileDelta::RepairOwned,
        FileOperationKind::NoOp => FileDelta::NoOp,
        FileOperationKind::Conflict => FileDelta::Conflict,
        FileOperationKind::Drift => FileDelta::Drift,
    })
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
                        || key_path.is_empty()
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
                    validate_semantic_owned(owned, &op.key, LedgerFamily::Launcher)
                }
                ApplyPayload::Path(op) => {
                    validate_semantic_owned(owned, &op.key, LedgerFamily::Path)
                }
                ApplyPayload::Service(op) => {
                    validate_semantic_owned(owned, &op.key, LedgerFamily::Service)
                }
                ApplyPayload::Protocol(op) => {
                    validate_semantic_owned(owned, &op.key, LedgerFamily::Protocol)
                }
                ApplyPayload::FileAssociation(op) => {
                    validate_semantic_owned(owned, &op.key, LedgerFamily::Association)
                }
                ApplyPayload::Extension(op) => {
                    let key = ResourceKey::FileAssociationExtension {
                        extension: zup_core::FileExtension::new(&op.extension)
                            .map_err(|error| error.to_string())?,
                    };
                    validate_semantic_owned(owned, &key, LedgerFamily::Extension)
                }
                ApplyPayload::AppsFeatures {
                    operation: apps_operation,
                } => {
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
            ApplyPayload::Extension(operation) => operation.key,
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

#[derive(Clone, Copy)]
enum LedgerFamily {
    Launcher,
    Path,
    Service,
    Protocol,
    Association,
    Extension,
}

fn validate_semantic_owned(
    owned: Option<&OwnedResource>,
    _key: &ResourceKey,
    family: LedgerFamily,
) -> Result<(), String> {
    match (family, owned) {
        (LedgerFamily::Launcher, None)
        | (LedgerFamily::Path, None)
        | (LedgerFamily::Service, None)
        | (LedgerFamily::Protocol, None)
        | (LedgerFamily::Association, None)
        | (LedgerFamily::Extension, None) => Ok(()),
        (LedgerFamily::Launcher, Some(OwnedResource::Launcher { .. }))
        | (LedgerFamily::Path, Some(OwnedResource::PathEntry { .. }))
        | (LedgerFamily::Service, Some(OwnedResource::Service { .. }))
        | (LedgerFamily::Protocol, Some(OwnedResource::Protocol { .. }))
        | (LedgerFamily::Association, Some(OwnedResource::FileAssociation { .. }))
        | (LedgerFamily::Extension, Some(OwnedResource::Extension { .. })) => Ok(()),
        _ => Err("backend ownership family mismatch".to_owned()),
    }
}

pub(crate) struct AppsPlanningInput {
    pub(crate) app_id: AppId,
    /// Authority the registration is written with. The host store lives in
    /// `scope`; the authority is stated separately.
    pub(crate) privilege: Privilege,
    pub(crate) current: Option<AppsFeaturesState>,
    pub(crate) owned: Option<AppsFeaturesState>,
    pub(crate) desired: Option<AppsFeaturesState>,
    pub(crate) key_path: String,
    pub(crate) uninstall: bool,
}

pub(crate) fn compile_execution_plan(
    execution: &ExecutionPlan,
    target: Option<&TargetPlan>,
    scope: SelectedScope,
    app_id: &AppId,
    ledger: Option<&InstallLedger>,
    apps: AppsPlanningInput,
) -> Result<TransactionInput, TransactionPayloadError> {
    let target_triple = target
        .map(|target| target.target.clone())
        .or_else(|| ledger.map(|ledger| ledger.target.clone()))
        .ok_or_else(|| {
            TransactionPayloadError::Identity("target is missing from plan and ledger".into())
        })?;
    if apps.app_id != *app_id {
        return Err(TransactionPayloadError::Identity(
            "Apps & Features app identity mismatch".into(),
        ));
    }
    let mut input = TransactionInput::new(target_triple);
    input.selected_components = execution.selected_components.clone();
    input.install_directory = target
        .map(|target| target.install_directory.clone())
        .or_else(|| execution.install_directory.clone());
    input.uninstall = execution.uninstall;
    // The preset runtime is a statement about this transaction, not about the
    // machine: an uninstall and a target with no window both leave none, and a
    // repair carries the one it is preserving forward.
    input.preset = if execution.uninstall {
        None
    } else {
        target.and_then(|target| target.preset.clone())
    };

    for file in &execution.files {
        input.files.push(FileWork {
            key: file.key.clone(),
            source_relative: file.source_relative.clone(),
            destination: file.destination.clone(),
            precondition: match file.precondition {
                zup_exec::FilePrecondition::Absent => FilePrecondition::Absent,
                zup_exec::FilePrecondition::Exact { size, sha256 } => {
                    FilePrecondition::Exact { size, sha256 }
                }
            },
            expected_sha256: file.expected_sha256,
            expected_size: file.expected_size,
            privilege: file.privilege,
            delta: file_delta(file.kind)?,
        });
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
                    kind: match removal.kind {
                        zup_exec::RemovalKind::RemoveOwned => FileRemovalKind::RemoveOwned,
                        zup_exec::RemovalKind::Drift => FileRemovalKind::Drift,
                    },
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
        if launcher.kind == LauncherOperationKind::NoOp {
            continue;
        }
        if matches!(
            launcher.kind,
            LauncherOperationKind::Conflict | LauncherOperationKind::Drift
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
        if service.kind == ServiceOperationKind::NoOp {
            continue;
        }
        if matches!(
            service.kind,
            ServiceOperationKind::Conflict | ServiceOperationKind::Drift
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
        if protocol.kind == ProtocolOperationKind::NoOp {
            continue;
        }
        if matches!(
            protocol.kind,
            ProtocolOperationKind::Conflict | ProtocolOperationKind::Drift
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
        if association.association_kind == FileAssociationOperationKind::NoOp
            && association.extension_kind == FileAssociationOperationKind::NoOp
        {
            continue;
        }
        if matches!(
            association.association_kind,
            FileAssociationOperationKind::Conflict | FileAssociationOperationKind::Drift
        ) || matches!(
            association.extension_kind,
            FileAssociationOperationKind::Conflict | FileAssociationOperationKind::Drift
        ) {
            return Err(TransactionPayloadError::NotExecutable(
                "file association".into(),
            ));
        }
        if association.association_kind != FileAssociationOperationKind::NoOp {
            input.backend_operations.push(backend_operation(
                &association.key,
                association.privilege,
                &ApplyPayload::FileAssociation(association.clone()),
                Vec::new(),
            )?);
        }
        if association.extension_kind != FileAssociationOperationKind::NoOp {
            let extension = zup_core::FileExtension::new(&association.extension)
                .map_err(|error| TransactionPayloadError::Unsupported(error.to_string()))?;
            let extension_key = ResourceKey::FileAssociationExtension { extension };
            let dependencies = if association.association_kind == FileAssociationOperationKind::NoOp
            {
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
