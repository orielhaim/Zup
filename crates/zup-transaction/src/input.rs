use std::fmt;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use zup_core::{
    BackendResourceId, ComponentId, Privilege, RelativePath, ResourceKey, SelectedScope,
    Sha256Digest, TargetTriple, UiRuntime,
};
use zup_platform::TargetPath;

pub const MAX_BACKEND_PAYLOAD_BYTES: usize = 1024 * 1024;
pub const MAX_BACKEND_DEPENDENCIES: usize = 256;

/// What a [`TransactionInputError`] is about.
///
/// Errors name the thing that failed, so the name travels as the value itself
/// rather than as a string built at the call site. The key is boxed to keep the
/// error small, since errors are returned and stored rather than inspected in
/// bulk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransactionResource {
    /// The transaction's install directory, which is not a `ResourceKey`.
    InstallDirectory,
    /// One entry of the selected component list.
    Component(ComponentId),
    /// A keyed resource: a file, a removal, or a backend operation.
    Key(Box<ResourceKey>),
}

impl TransactionResource {
    /// The resource named by `key`.
    pub fn key(key: &ResourceKey) -> Self {
        Self::Key(Box::new(key.clone()))
    }
}

impl fmt::Display for TransactionResource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InstallDirectory => formatter.write_str("install_directory"),
            Self::Component(component) => write!(formatter, "{component}"),
            Self::Key(key) => write!(formatter, "{key:?}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileDelta {
    Create,
    Replace,
    RestoreOwned,
    RepairOwned,
    NoOp,
    Conflict,
    Drift,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileRemovalKind {
    RemoveOwned,
    Drift,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileWork {
    pub key: ResourceKey,
    pub source_relative: RelativePath,
    pub destination: TargetPath,
    pub precondition: FilePrecondition,
    pub expected_sha256: Sha256Digest,
    pub expected_size: u64,
    pub privilege: Privilege,
    pub delta: FileDelta,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileRemoval {
    pub key: ResourceKey,
    pub kind: FileRemovalKind,
    pub scope: SelectedScope,
    pub privilege: Privilege,
    pub destination: TargetPath,
    pub sha256: Sha256Digest,
    pub size: u64,
    pub created_directories: Vec<TargetPath>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendOperationIntent {
    Apply,
    Remove,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendOperation {
    pub key: ResourceKey,
    pub id: BackendResourceId,
    pub privilege: Privilege,
    pub intent: BackendOperationIntent,
    pub payload: Vec<u8>,
    pub dependencies: Vec<ResourceKey>,
}

impl BackendOperation {
    pub fn apply(
        key: ResourceKey,
        id: BackendResourceId,
        privilege: Privilege,
        payload: Vec<u8>,
    ) -> Self {
        Self {
            key,
            id,
            privilege,
            intent: BackendOperationIntent::Apply,
            payload,
            dependencies: Vec::new(),
        }
    }

    pub fn remove(
        key: ResourceKey,
        id: BackendResourceId,
        privilege: Privilege,
        payload: Vec<u8>,
    ) -> Self {
        Self {
            key,
            id,
            privilege,
            intent: BackendOperationIntent::Remove,
            payload,
            dependencies: Vec::new(),
        }
    }

    pub fn with_dependencies(mut self, dependencies: Vec<ResourceKey>) -> Self {
        self.dependencies = dependencies;
        self
    }

    pub fn validate(&self) -> Result<(), TransactionInputError> {
        if self.payload.len() > MAX_BACKEND_PAYLOAD_BYTES {
            return Err(TransactionInputError::PayloadTooLarge {
                resource: TransactionResource::key(&self.key),
                size: self.payload.len(),
                max: MAX_BACKEND_PAYLOAD_BYTES,
            });
        }
        if self.dependencies.len() > MAX_BACKEND_DEPENDENCIES {
            return Err(TransactionInputError::TooManyDependencies {
                resource: TransactionResource::key(&self.key),
                count: self.dependencies.len(),
                max: MAX_BACKEND_DEPENDENCIES,
            });
        }
        let mut dependencies = std::collections::BTreeSet::new();
        for dependency in &self.dependencies {
            if dependency == &self.key {
                return Err(TransactionInputError::SelfDependency {
                    resource: TransactionResource::key(&self.key),
                });
            }
            if !dependencies.insert(dependency) {
                return Err(TransactionInputError::DuplicateDependency {
                    resource: TransactionResource::key(&self.key),
                    dependency: TransactionResource::key(dependency),
                });
            }
        }
        if self.key
            != (ResourceKey::Backend {
                id: self.id.clone(),
            })
        {
            return Err(TransactionInputError::BackendIdentityMismatch {
                resource: TransactionResource::key(&self.key),
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransactionInput {
    pub target: TargetTriple,
    pub selected_components: Vec<ComponentId>,
    pub install_directory: Option<TargetPath>,
    pub uninstall: bool,
    pub retired_keys: Vec<ResourceKey>,
    pub files: Vec<FileWork>,
    pub removals: Vec<FileRemoval>,
    pub backend_operations: Vec<BackendOperation>,
    /// The UI runtime this transaction makes durable, or leaves absent.
    ///
    /// In the plan and the journal rather than beside them, because a recovery
    /// run works from the journal alone. Settings that lived only in the caller
    /// would be unrecoverable exactly when recovery is what is needed.
    pub ui: Option<UiRuntime>,
}

impl TransactionInput {
    pub fn new(target: TargetTriple) -> Self {
        Self {
            target,
            selected_components: Vec::new(),
            install_directory: None,
            uninstall: false,
            retired_keys: Vec::new(),
            files: Vec::new(),
            removals: Vec::new(),
            backend_operations: Vec::new(),
            ui: None,
        }
    }

    pub fn validate(&self) -> Result<(), TransactionInputError> {
        if self
            .install_directory
            .as_ref()
            .is_some_and(|directory| directory.target() != &self.target)
        {
            return Err(TransactionInputError::TargetMismatch {
                resource: TransactionResource::InstallDirectory,
            });
        }
        let mut components = std::collections::BTreeSet::new();
        for component in &self.selected_components {
            if !components.insert(component.clone()) {
                return Err(TransactionInputError::DuplicateComponent {
                    component: TransactionResource::Component(component.clone()),
                });
            }
        }
        let mut keys = std::collections::BTreeSet::new();
        for key in &self.retired_keys {
            if !keys.insert(key.clone()) {
                return Err(TransactionInputError::DuplicateKey {
                    resource: TransactionResource::key(key),
                });
            }
        }
        let mut file_keys = std::collections::BTreeSet::new();
        for file in &self.files {
            if file.destination.target() != &self.target {
                return Err(TransactionInputError::TargetMismatch {
                    resource: TransactionResource::key(&file.key),
                });
            }
            if !file_keys.insert(file.key.clone()) {
                return Err(TransactionInputError::DuplicateKey {
                    resource: TransactionResource::key(&file.key),
                });
            }
        }
        let mut removal_keys = std::collections::BTreeSet::new();
        for removal in &self.removals {
            if removal.destination.target() != &self.target
                || removal
                    .created_directories
                    .iter()
                    .any(|directory| directory.target() != &self.target)
            {
                return Err(TransactionInputError::TargetMismatch {
                    resource: TransactionResource::key(&removal.key),
                });
            }
            if !removal_keys.insert(removal.key.clone()) {
                return Err(TransactionInputError::DuplicateKey {
                    resource: TransactionResource::key(&removal.key),
                });
            }
            if !keys.contains(&removal.key) {
                return Err(TransactionInputError::UnretiredRemoval {
                    resource: TransactionResource::key(&removal.key),
                });
            }
        }
        let mut backend_keys = std::collections::BTreeSet::new();
        for operation in &self.backend_operations {
            operation.validate()?;
            if !backend_keys.insert(operation.key.clone()) {
                return Err(TransactionInputError::DuplicateKey {
                    resource: TransactionResource::key(&operation.key),
                });
            }
            if operation.intent == BackendOperationIntent::Remove && !keys.contains(&operation.key)
            {
                return Err(TransactionInputError::UnretiredBackendRemoval {
                    resource: TransactionResource::key(&operation.key),
                });
            }
        }
        for operation in &self.backend_operations {
            for dependency in &operation.dependencies {
                if !backend_keys.contains(dependency) && !file_keys.contains(dependency) {
                    return Err(TransactionInputError::UnknownDependency {
                        resource: TransactionResource::key(&operation.key),
                        dependency: TransactionResource::key(dependency),
                    });
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum TransactionInputError {
    #[error("backend payload for `{resource}` is {size} bytes; maximum is {max}")]
    PayloadTooLarge {
        resource: TransactionResource,
        size: usize,
        max: usize,
    },
    #[error("backend operation `{resource}` has {count} dependencies; maximum is {max}")]
    TooManyDependencies {
        resource: TransactionResource,
        count: usize,
        max: usize,
    },
    #[error("backend operation `{resource}` does not use its backend resource identity")]
    BackendIdentityMismatch { resource: TransactionResource },
    #[error("duplicate transaction resource key `{resource}`")]
    DuplicateKey { resource: TransactionResource },
    #[error("transaction resource `{resource}` targets a different target")]
    TargetMismatch { resource: TransactionResource },
    #[error("duplicate selected component `{component}`")]
    DuplicateComponent { component: TransactionResource },
    #[error("backend operation `{resource}` depends on itself")]
    SelfDependency { resource: TransactionResource },
    #[error("backend operation `{resource}` repeats dependency `{dependency}`")]
    DuplicateDependency {
        resource: TransactionResource,
        dependency: TransactionResource,
    },
    #[error("removal `{resource}` is not retired")]
    UnretiredRemoval { resource: TransactionResource },
    #[error("backend removal `{resource}` is not retired")]
    UnretiredBackendRemoval { resource: TransactionResource },
    #[error("backend operation `{resource}` depends on unknown resource `{dependency}`")]
    UnknownDependency {
        resource: TransactionResource,
        dependency: TransactionResource,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum FilePrecondition {
    Absent,
    Exact { size: u64, sha256: Sha256Digest },
}
