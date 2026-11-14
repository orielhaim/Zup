use crate::FileOperation;
use crate::RemovalKind;
use std::fmt;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use zup_core::TargetPath;
use zup_core::{
    BackendResourceId, ComponentId, InstalledPreset, Privilege, ResourceKey, SelectedScope,
    Sha256Digest, Target,
};

pub const MAX_BACKEND_PAYLOAD_BYTES: usize = 1024 * 1024;
pub const MAX_BACKEND_DEPENDENCIES: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeploymentResource {
    InstallDirectory,
    Component(ComponentId),
    Key(Box<ResourceKey>),
}

impl DeploymentResource {
    pub fn key(key: &ResourceKey) -> Self {
        Self::Key(Box::new(key.clone()))
    }
}

impl fmt::Display for DeploymentResource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InstallDirectory => formatter.write_str("install_directory"),
            Self::Component(component) => write!(formatter, "{component}"),
            Self::Key(key) => write!(formatter, "{key:?}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileRemoval {
    pub instance: zup_core::FixedFileInstance,
    pub key: ResourceKey,
    pub kind: RemovalKind,
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

    pub fn validate(&self) -> Result<(), DeploymentEffectsError> {
        if self.payload.len() > MAX_BACKEND_PAYLOAD_BYTES {
            return Err(DeploymentEffectsError::PayloadTooLarge {
                resource: DeploymentResource::key(&self.key),
                size: self.payload.len(),
                max: MAX_BACKEND_PAYLOAD_BYTES,
            });
        }
        if self.dependencies.len() > MAX_BACKEND_DEPENDENCIES {
            return Err(DeploymentEffectsError::TooManyDependencies {
                resource: DeploymentResource::key(&self.key),
                count: self.dependencies.len(),
                max: MAX_BACKEND_DEPENDENCIES,
            });
        }
        let mut dependencies = std::collections::BTreeSet::new();
        for dependency in &self.dependencies {
            if dependency == &self.key {
                return Err(DeploymentEffectsError::SelfDependency {
                    resource: DeploymentResource::key(&self.key),
                });
            }
            if !dependencies.insert(dependency) {
                return Err(DeploymentEffectsError::DuplicateDependency {
                    resource: DeploymentResource::key(&self.key),
                    dependency: DeploymentResource::key(dependency),
                });
            }
        }
        if self.key
            != (ResourceKey::Backend {
                id: self.id.clone(),
            })
        {
            return Err(DeploymentEffectsError::BackendIdentityMismatch {
                resource: DeploymentResource::key(&self.key),
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeploymentEffects {
    pub cleanup: Vec<crate::CleanupOperation>,
    pub retention: Option<crate::RetentionChange>,
    pub data: Vec<crate::PersistentDirectoryOperation>,
    pub requirements: crate::DeploymentRequirements,
    pub installation: zup_core::InstallationId,
    pub observed: Option<crate::ActivationSelection>,
    pub generations: Vec<crate::GenerationIntent>,
    pub target: Target,
    pub selected_components: Vec<ComponentId>,
    pub install_directory: Option<TargetPath>,
    pub transition: crate::DeploymentTransition,
    pub retired_keys: Vec<ResourceKey>,
    pub files: Vec<FileOperation>,
    pub removals: Vec<FileRemoval>,
    pub backend_operations: Vec<BackendOperation>,
    pub native_services: Vec<crate::NativeServiceOperation>,
    pub native_caches: Vec<crate::NativeCacheOperation>,
    pub service_controls: Vec<crate::ServiceControlOperation>,
    pub preset: Option<InstalledPreset>,
    /// Committed transaction this plan was prepared against.
    /// `None` means no installation existed.
    pub observed_transaction: Option<zup_core::TransactionId>,
}

impl DeploymentEffects {
    pub fn bind_persistent(
        &mut self,
        desired: Option<&crate::BoundDeployment>,
        previous: Option<&crate::InstallationRecord>,
        claims: Option<&crate::PersistentClaimSelection>,
        bindings: &std::collections::BTreeMap<zup_core::ResourceId, crate::PersistentBinding>,
    ) -> Result<(), crate::ContractError> {
        if let Some(claims) = claims
            && (claims.installation != self.installation
                || self
                    .install_directory
                    .as_ref()
                    .is_none_or(|directory| !directory.equivalent(&claims.directory)))
        {
            return Err(crate::ContractError::Capability(
                "persistent data claims belong to a different installation".into(),
            ));
        }
        self.data.clear();
        let Some(desired) = desired else {
            return Ok(());
        };
        if bindings.len() != desired.data.len()
            || desired
                .data
                .iter()
                .any(|directory| !bindings.contains_key(&directory.id))
        {
            return Err(crate::ContractError::Capability(
                "persistent data needs exactly one native binding per declaration".into(),
            ));
        }
        for directory in &desired.data {
            let previous = previous
                .and_then(|record| record.data.get(&directory.id))
                .or_else(|| claims.and_then(|claims| claims.owners.get(&directory.id)))
                .cloned();
            let intent = crate::PersistentDirectoryIntent {
                installation: self.installation,
                directory: directory.clone(),
                binding: bindings[&directory.id].clone(),
                previous,
                privilege: desired.scope.authorization(),
            };
            intent
                .validate(desired.target, desired.scope)
                .map_err(crate::ContractError::Capability)?;
            for stage in [
                crate::PersistentDirectoryStage::Prepare,
                crate::PersistentDirectoryStage::Access,
            ] {
                self.data.push(crate::PersistentDirectoryOperation {
                    intent: intent.clone(),
                    stage,
                });
            }
        }
        Ok(())
    }
    pub fn bind_generations(
        &mut self,
        desired: Option<&crate::BoundDeployment>,
    ) -> Result<(), crate::ContractError> {
        self.generations = match desired.and_then(|desired| {
            desired
                .generation
                .as_ref()
                .map(|generation| (desired, generation))
        }) {
            Some((desired, generation)) => {
                desired.validate_resolved()?;
                let exposures = self
                    .files
                    .iter()
                    .filter(|file| {
                        !matches!(
                            file.kind,
                            crate::FileOperationKind::NoOp
                                | crate::FileOperationKind::Conflict
                                | crate::FileOperationKind::Drift
                        )
                    })
                    .map(|file| file.key.clone())
                    .chain(
                        self.backend_operations
                            .iter()
                            .filter(|operation| operation.intent == BackendOperationIntent::Apply)
                            .map(|operation| operation.key.clone()),
                    )
                    .chain(
                        self.native_services
                            .iter()
                            .filter(|operation| {
                                matches!(operation, crate::NativeServiceOperation::Configure { .. })
                            })
                            .map(|operation| operation.key().clone()),
                    )
                    .chain(
                        self.service_controls
                            .iter()
                            .filter(|operation| {
                                operation.stage == crate::ServiceControlStage::Restore
                            })
                            .map(|operation| operation.key.clone()),
                    )
                    .collect();
                vec![generation.bind(desired.scope.authorization(), exposures)?]
            }
            None => Vec::new(),
        };
        if let Some(desired) = desired {
            desired.validate_resolved()?;
            if let Some(maintenance) = &desired.maintenance {
                let exposures = self
                    .backend_operations
                    .iter()
                    .filter(|operation| operation.intent == BackendOperationIntent::Apply)
                    .map(|operation| operation.key.clone())
                    .collect();
                self.generations
                    .push(maintenance.bind(desired.scope.authorization(), exposures)?);
            }
        }
        Ok(())
    }

    pub fn new(
        installation: zup_core::InstallationId,
        requirements: crate::DeploymentRequirements,
    ) -> Self {
        let target = requirements.target;
        let selected_components = requirements.components.clone();
        let preset = requirements.preset.clone();
        Self {
            retention: None,
            cleanup: Vec::new(),
            data: Vec::new(),
            requirements,
            installation,
            observed: None,
            generations: Vec::new(),
            target,
            selected_components,
            install_directory: None,
            transition: crate::DeploymentTransition::Deploy,
            retired_keys: Vec::new(),
            files: Vec::new(),
            removals: Vec::new(),
            backend_operations: Vec::new(),
            native_services: Vec::new(),
            native_caches: Vec::new(),
            service_controls: Vec::new(),
            preset,
            observed_transaction: None,
        }
    }

    pub fn validate(&self) -> Result<(), DeploymentEffectsError> {
        for directory in &self.requirements.data {
            if self.transition.is_deploy()
                && !self
                    .data
                    .iter()
                    .any(|operation| operation.intent.directory.id == directory.id)
            {
                return Err(DeploymentEffectsError::Persistent {
                    reason: format!(
                        "persistent directory {} is missing its owned operation",
                        directory.id
                    ),
                });
            }
        }
        for operation in &self.data {
            operation
                .validate(self.target, self.requirements.scope)
                .map_err(|reason| DeploymentEffectsError::Persistent { reason })?;
        }
        let mut services = std::collections::BTreeMap::new();
        for operation in &self.native_services {
            operation
                .validate(self.installation, self.target)
                .map_err(|error| DeploymentEffectsError::NativeService {
                    reason: error.to_string(),
                })?;
            if services.insert(operation.key(), operation).is_some() {
                return Err(DeploymentEffectsError::DuplicateKey {
                    resource: DeploymentResource::key(operation.key()),
                });
            }
            if matches!(operation, crate::NativeServiceOperation::Remove { .. })
                && !self.retired_keys.contains(operation.key())
            {
                return Err(DeploymentEffectsError::UnretiredRemoval {
                    resource: DeploymentResource::key(operation.key()),
                });
            }
        }
        for operation in &self.native_services {
            for id in operation.contract().requires_services {
                let key = ResourceKey::Service { id }.canonical_for(&self.target);
                let dependency = services.get(&key).ok_or_else(|| {
                    DeploymentEffectsError::UnknownDependency {
                        resource: DeploymentResource::key(operation.key()),
                        dependency: DeploymentResource::key(&key),
                    }
                })?;
                if matches!(operation, crate::NativeServiceOperation::Configure { .. })
                    && matches!(dependency, crate::NativeServiceOperation::Remove { .. })
                {
                    return Err(DeploymentEffectsError::NativeService {
                        reason: "a configured service depends on a removed service".into(),
                    });
                }
            }
        }
        for operation in &self.native_caches {
            operation.validate(self.target).map_err(|error| {
                DeploymentEffectsError::NativeCache {
                    reason: error.to_string(),
                }
            })?;
        }
        let mut purposes = std::collections::HashSet::new();
        let mut materializations = std::collections::BTreeSet::new();
        for generation in &self.generations {
            if !purposes.insert(std::mem::discriminant(&generation.publication))
                || !materializations.insert(generation.materialization.id())
            {
                return Err(DeploymentEffectsError::Generation {
                    reason: "deployment repeats a generation purpose or materialization".into(),
                });
            }
            if generation.materialization.installation() != self.installation
                || matches!(generation.publication, crate::GenerationPublication::Application { previous, .. } if previous != self.observed)
            {
                return Err(DeploymentEffectsError::Generation {
                    reason: "installation or activation binding mismatch".into(),
                });
            }
            generation.validate(self.target).map_err(|error| {
                DeploymentEffectsError::Generation {
                    reason: error.to_string(),
                }
            })?;
        }
        let mut controls = std::collections::BTreeSet::new();
        for control in &self.service_controls {
            control
                .validate()
                .map_err(|reason| DeploymentEffectsError::ServiceControl { reason })?;
            if !controls.insert((&control.key, control.stage)) {
                return Err(DeploymentEffectsError::DuplicateKey {
                    resource: DeploymentResource::key(&control.key),
                });
            }
        }
        if self
            .install_directory
            .as_ref()
            .is_some_and(|directory| directory.target() != &self.target)
        {
            return Err(DeploymentEffectsError::TargetMismatch {
                resource: DeploymentResource::InstallDirectory,
            });
        }
        let mut components = std::collections::BTreeSet::new();
        for component in &self.selected_components {
            if !components.insert(component.clone()) {
                return Err(DeploymentEffectsError::DuplicateComponent {
                    component: DeploymentResource::Component(component.clone()),
                });
            }
        }
        let mut keys = std::collections::BTreeSet::new();
        for key in &self.retired_keys {
            if !keys.insert(key.clone()) {
                return Err(DeploymentEffectsError::DuplicateKey {
                    resource: DeploymentResource::key(key),
                });
            }
        }
        let mut file_keys = std::collections::BTreeSet::new();
        for file in &self.files {
            if file.destination.target() != &self.target {
                return Err(DeploymentEffectsError::TargetMismatch {
                    resource: DeploymentResource::key(&file.key),
                });
            }
            if !file_keys.insert(file.key.clone()) {
                return Err(DeploymentEffectsError::DuplicateKey {
                    resource: DeploymentResource::key(&file.key),
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
                return Err(DeploymentEffectsError::TargetMismatch {
                    resource: DeploymentResource::key(&removal.key),
                });
            }
            if !removal_keys.insert(removal.key.clone()) {
                return Err(DeploymentEffectsError::DuplicateKey {
                    resource: DeploymentResource::key(&removal.key),
                });
            }
            if !keys.contains(&removal.key) {
                return Err(DeploymentEffectsError::UnretiredRemoval {
                    resource: DeploymentResource::key(&removal.key),
                });
            }
        }
        let mut backend_keys = std::collections::BTreeSet::new();
        for operation in &self.backend_operations {
            operation.validate()?;
            if !backend_keys.insert(operation.key.clone()) {
                return Err(DeploymentEffectsError::DuplicateKey {
                    resource: DeploymentResource::key(&operation.key),
                });
            }
            if operation.intent == BackendOperationIntent::Remove && !keys.contains(&operation.key)
            {
                return Err(DeploymentEffectsError::UnretiredBackendRemoval {
                    resource: DeploymentResource::key(&operation.key),
                });
            }
        }
        let removal_keys: std::collections::BTreeSet<_> =
            self.removals.iter().map(|removal| &removal.key).collect();
        for operation in &self.backend_operations {
            for dependency in &operation.dependencies {
                if !backend_keys.contains(dependency)
                    && !file_keys.contains(dependency)
                    && !removal_keys.contains(dependency)
                {
                    return Err(DeploymentEffectsError::UnknownDependency {
                        resource: DeploymentResource::key(&operation.key),
                        dependency: DeploymentResource::key(dependency),
                    });
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum DeploymentEffectsError {
    #[error("persistent data: {reason}")]
    Persistent { reason: String },
    #[error("invalid native service operation: {reason}")]
    NativeService { reason: String },
    #[error("invalid native cache operation: {reason}")]
    NativeCache { reason: String },
    #[error("invalid generation intent: {reason}")]
    Generation { reason: String },
    #[error("invalid service lifecycle intent: {reason}")]
    ServiceControl { reason: String },
    #[error("backend payload for `{resource}` is {size} bytes; maximum is {max}")]
    PayloadTooLarge {
        resource: DeploymentResource,
        size: usize,
        max: usize,
    },
    #[error("backend operation `{resource}` has {count} dependencies; maximum is {max}")]
    TooManyDependencies {
        resource: DeploymentResource,
        count: usize,
        max: usize,
    },
    #[error("backend operation `{resource}` does not use its backend resource identity")]
    BackendIdentityMismatch { resource: DeploymentResource },
    #[error("duplicate transaction resource key `{resource}`")]
    DuplicateKey { resource: DeploymentResource },
    #[error("transaction resource `{resource}` targets a different target")]
    TargetMismatch { resource: DeploymentResource },
    #[error("duplicate selected component `{component}`")]
    DuplicateComponent { component: DeploymentResource },
    #[error("backend operation `{resource}` depends on itself")]
    SelfDependency { resource: DeploymentResource },
    #[error("backend operation `{resource}` repeats dependency `{dependency}`")]
    DuplicateDependency {
        resource: DeploymentResource,
        dependency: DeploymentResource,
    },
    #[error("removal `{resource}` is not retired")]
    UnretiredRemoval { resource: DeploymentResource },
    #[error("backend removal `{resource}` is not retired")]
    UnretiredBackendRemoval { resource: DeploymentResource },
    #[error("backend operation `{resource}` depends on unknown resource `{dependency}`")]
    UnknownDependency {
        resource: DeploymentResource,
        dependency: DeploymentResource,
    },
}
