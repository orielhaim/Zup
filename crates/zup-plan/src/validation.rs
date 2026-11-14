use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Display,
};

use thiserror::Error;
use zup_core::{ResourceKey, Target};

use crate::{DesiredState, capabilities};

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ContractError {
    #[error("resolved deployment metadata differs from its application requirements")]
    Requirements,
    #[error(transparent)]
    Generation(#[from] zup_core::GenerationError),
    #[error("deployment size overflow")]
    SizeOverflow,
    #[error("resource identity does not match its declaration: {0:?}")]
    Identity(ResourceKey),
    #[error("duplicate deployment resource: {0:?}")]
    Duplicate(ResourceKey),
    #[error("deployment paths overlap: {first} and {second}")]
    PathCollision { first: String, second: String },
    #[error("duplicate selected component")]
    DuplicateComponent,
    #[error("resource path targets a different platform than {0}")]
    Target(Target),
    #[error("unsupported deployment: {0}")]
    Capability(String),
    #[error(
        "service `{service}` requires `{path}`, which must be a unique managed file with the service's privilege"
    )]
    ServiceFile {
        service: zup_core::ServiceId,
        path: String,
    },
}

impl<P: Display, L, R> DesiredState<P, L, R> {
    pub fn validate(&self) -> Result<(), ContractError> {
        if self.generation_readers == zup_core::GenerationReaders::NativeExecutables
            && self.generation.is_none()
        {
            return Err(ContractError::Capability(
                "native generation readers require a versioned application payload".into(),
            ));
        }
        let mut data_ids = BTreeSet::new();
        for directory in &self.data {
            if !data_ids.insert(&directory.id) {
                return Err(ContractError::Capability(format!(
                    "persistent directory identity {} is repeated",
                    directory.id
                )));
            }
            for service in directory.location.services() {
                if self.scope != zup_core::SelectedScope::Machine
                    || !self.services.iter().any(|declared| &declared.id == service)
                {
                    return Err(ContractError::Capability(format!(
                        "persistent data {} requires an available machine service {service}",
                        directory.id
                    )));
                }
            }
        }
        if self.generation.as_ref().is_some_and(|generation| {
            generation.installation != self.installation
                || generation.descriptor.target() != self.target
                || !matches!(
                    generation.publication,
                    crate::GenerationPublication::Application { .. }
                )
        }) {
            return Err(ContractError::Capability(
                "generation installation or target mismatch".into(),
            ));
        }
        if self.maintenance.as_ref().is_some_and(|generation| {
            generation.installation != self.installation
                || generation.descriptor.target() != self.target
                || generation.publication != crate::GenerationPublication::Maintenance
        }) {
            return Err(ContractError::Capability(
                "maintenance generation installation, target or purpose mismatch".into(),
            ));
        }
        let mut keys = BTreeSet::new();
        let mut claim = |actual: &ResourceKey, expected: ResourceKey| {
            let expected = expected.canonical_for(&self.target);
            if actual.canonical_for(&self.target) != expected {
                return Err(ContractError::Identity(actual.clone()));
            }
            if !keys.insert(expected.clone()) {
                return Err(ContractError::Duplicate(expected));
            }
            Ok(())
        };
        let mut destinations = BTreeMap::new();
        for file in &self.files {
            let destination = file.destination().to_string();
            if let Some(first) =
                destinations.insert(self.target.path_identity(&destination), destination.clone())
            {
                return Err(ContractError::PathCollision {
                    first,
                    second: destination,
                });
            }
            let expected = match &file.key {
                ResourceKey::File {
                    id: zup_core::FileResourceId::Payload { id },
                } => ResourceKey::File {
                    id: zup_core::FileResourceId::Payload { id: id.clone() },
                },
                ResourceKey::File {
                    id: zup_core::FileResourceId::Plugin { .. },
                } => file.key.clone(),
                ResourceKey::File {
                    id:
                        zup_core::FileResourceId::Native {
                            id: zup_core::NativeFileId::ServiceSource { id },
                        },
                } if self.target.operating_system() == zup_core::TargetOperatingSystem::Linux
                    && self.scope == zup_core::SelectedScope::Machine
                    && file.privilege == zup_core::Privilege::System
                    && matches!(file.placement, zup_core::PayloadPlacement::Fixed { .. })
                    && self.services.iter().any(|service| &service.id == id) =>
                {
                    file.key.clone()
                }
                ResourceKey::File {
                    id: zup_core::FileResourceId::Native { id },
                } if !matches!(id, zup_core::NativeFileId::ServiceSource { .. })
                    && self.target.operating_system() == zup_core::TargetOperatingSystem::Linux
                    && matches!(file.placement, zup_core::PayloadPlacement::Fixed { .. }) =>
                {
                    file.key.clone()
                }
                _ => return Err(ContractError::Identity(file.key.clone())),
            };
            claim(&file.key, expected)?;
            if matches!(
                file.placement,
                zup_core::PayloadPlacement::Generation { .. }
            ) && (!matches!(
                file.key,
                ResourceKey::File {
                    id: zup_core::FileResourceId::Payload { .. }
                }
            ) || self.generation.is_none())
            {
                return Err(ContractError::Capability(
                    "generation payload has no deployment identity".into(),
                ));
            }
        }
        if let Some(generation) = &self.generation {
            let generation_count = self
                .files
                .iter()
                .filter(|file| {
                    matches!(
                        file.placement,
                        zup_core::PayloadPlacement::Generation { .. }
                    )
                })
                .count();
            if generation_count != generation.descriptor.files().len()
                || generation.sources.len() != generation_count
            {
                return Err(ContractError::Capability(
                    "generation bindings do not cover selected payloads".into(),
                ));
            }
            for member in generation.descriptor.files() {
                let source = generation.sources.get(&member.path).ok_or_else(|| {
                    ContractError::Capability(
                        "generation member has no logical source binding".into(),
                    )
                })?;
                let key = source.key.clone();
                let requirement = self
                    .files
                    .iter()
                    .find(|file| file.key == key)
                    .ok_or_else(|| ContractError::Identity(key.clone()))?;
                let expected = self
                    .target
                    .path_identity(&format!("{}/{}", generation.directory, member.path));
                if !matches!(
                    requirement.placement,
                    zup_core::PayloadPlacement::Generation { .. }
                ) || self
                    .target
                    .path_identity(&requirement.destination().to_string())
                    != expected
                    && self
                        .target
                        .path_identity(&requirement.destination().to_string())
                        != self
                            .target
                            .path_identity(&format!("${{payload}}/{}", member.path))
                    || requirement.source_relative != source.source_relative
                    || requirement.size != member.size
                    || requirement.sha256 != member.sha256
                    || requirement.executable != member.executable
                    || requirement.privilege != self.scope.authorization()
                {
                    return Err(ContractError::Identity(key));
                }
            }
        }
        let mut launcher_paths = BTreeSet::new();
        for launcher in &self.launchers {
            let name = if self.target.operating_system() == zup_core::TargetOperatingSystem::Windows
            {
                launcher.name.as_str().to_lowercase()
            } else {
                launcher.name.to_string()
            };
            if !launcher_paths.insert((launcher.location, name)) {
                return Err(ContractError::PathCollision {
                    first: launcher.name.to_string(),
                    second: launcher.name.to_string(),
                });
            }
            if !matches!(launcher.key, ResourceKey::Launcher { .. }) {
                return Err(ContractError::Identity(launcher.key.clone()));
            }
            claim(&launcher.key, launcher.key.clone())?;
        }
        let mut path_bindings = BTreeMap::new();
        for entry in &self.path_entries {
            let value = entry.value.to_string();
            if let Some(first) =
                path_bindings.insert(self.target.path_identity(&value), value.clone())
            {
                return Err(ContractError::PathCollision {
                    first,
                    second: value,
                });
            }
            claim(
                &entry.key,
                match &entry.key {
                    ResourceKey::PathEntry { .. } => entry.key.clone(),
                    _ => return Err(ContractError::Identity(entry.key.clone())),
                },
            )?;
        }
        let graph = self
            .services
            .iter()
            .map(|service| {
                (
                    service.key.canonical_for(&self.target),
                    zup_core::service_dependency_keys(&service.requires_services, self.target),
                )
            })
            .collect();
        zup_core::validate_service_graph(&graph).map_err(ContractError::Capability)?;
        let mut native_service_names = BTreeSet::new();
        for service in &self.services {
            let name = if self.target.is_windows() {
                service.name.as_str().to_lowercase()
            } else {
                service.name.to_string()
            };
            if !native_service_names.insert(name) {
                return Err(ContractError::Capability(format!(
                    "multiple services claim native name {}",
                    service.name
                )));
            }
            service
                .lifecycle
                .validate(service.start, &service.execution)
                .map_err(|reason| ContractError::Capability(reason.into()))?;
            service
                .execution
                .native_name(&self.app.id, &service.id, Some(&service.name))
                .map_err(|reason| ContractError::Capability(reason.into()))?;
            for argument in &service.command.arguments {
                if let zup_core::ServiceArgument::NativeName(id) =
                    zup_core::ServiceArgument::parse(argument)
                        .map_err(|error| ContractError::Capability(error.to_string()))?
                    && !self.services.iter().any(|service| service.id == id)
                {
                    return Err(ContractError::Capability(format!(
                        "service {} references unavailable service {id}",
                        service.id
                    )));
                }
                if let zup_core::ServiceArgument::WindowsJob(id) =
                    zup_core::ServiceArgument::parse(argument)
                        .map_err(|error| ContractError::Capability(error.to_string()))?
                    && (id != service.id
                        || !matches!(
                            service.execution,
                            zup_core::ServiceExecution::WindowsJob { .. }
                        ))
                {
                    return Err(ContractError::Capability(
                        "a native job argument must name its own declared Windows job scope".into(),
                    ));
                }
                if let zup_core::ServiceArgument::DataDirectory(id) =
                    zup_core::ServiceArgument::parse(argument)
                        .map_err(|error| ContractError::Capability(error.to_string()))?
                    && !self.data.iter().any(|data| {
                        data.id == id
                            && data.location.services().any(|writer| writer == &service.id)
                    })
                {
                    return Err(ContractError::Capability(format!(
                        "service {} cannot reference undeclared or inaccessible data {id}",
                        service.id
                    )));
                }
            }
            let dependencies =
                zup_core::service_dependency_keys(&service.requires_services, self.target);
            if service.dependency_names
                != service
                    .requires_services
                    .iter()
                    .map(|id| {
                        self.services
                            .iter()
                            .find(|dependency| {
                                ResourceKey::Service {
                                    id: dependency.id.clone(),
                                }
                                .canonical_for(&self.target)
                                    == ResourceKey::Service { id: id.clone() }
                                        .canonical_for(&self.target)
                            })
                            .map(|dependency| dependency.name.to_string())
                            .unwrap_or_default()
                    })
                    .collect::<Vec<_>>()
                || service.requires_services.len() > 256
                || dependencies.len() != service.requires_services.len()
                || dependencies.iter().any(|key| {
                    !self.services.iter().any(|dependency| {
                        dependency.key.canonical_for(&self.target) == *key
                            && dependency.privilege == service.privilege
                            && matches!(
                                dependency.execution,
                                zup_core::ServiceExecution::OpenrcUser { .. }
                            ) == matches!(
                                service.execution,
                                zup_core::ServiceExecution::OpenrcUser { .. }
                            )
                    })
                })
            {
                return Err(ContractError::Capability(format!(
                    "service {} requires unique managed services with the same privilege",
                    service.id
                )));
            }
            service
                .execution
                .validate_policies(service.failure, service.readiness, self.target)
                .map_err(|reason| ContractError::Capability(reason.into()))?;
            let mut dependencies = BTreeSet::new();
            if service.requires_files.len() > 256 {
                return Err(ContractError::Capability(
                    "a service may require at most 256 managed files".into(),
                ));
            }
            for path in &service.requires_files {
                let identity = self.target.path_identity(&path.to_string());
                if !dependencies.insert(identity.clone())
                    || !self.files.iter().any(|file| {
                        self.target.path_identity(&file.destination().to_string()) == identity
                            && file.privilege == service.privilege
                    })
                {
                    return Err(ContractError::ServiceFile {
                        service: service.id.clone(),
                        path: path.to_string(),
                    });
                }
            }
            claim(
                &service.key,
                ResourceKey::Service {
                    id: service.id.clone(),
                },
            )?;
        }
        for protocol in &self.protocols {
            claim(
                &protocol.key,
                ResourceKey::Protocol {
                    scheme: protocol.scheme.clone(),
                },
            )?;
        }
        for association in &self.file_associations {
            claim(
                &association.key,
                ResourceKey::FileAssociation {
                    id: association.id.clone(),
                },
            )?;
            let extension = ResourceKey::FileAssociationExtension {
                extension: association.extension.clone(),
            };
            claim(&extension, extension.clone())?;
        }
        let components: BTreeSet<_> = self.selected_components.iter().collect();
        if components.len() != self.selected_components.len() {
            return Err(ContractError::DuplicateComponent);
        }
        let errors = capabilities(&self.target)
            .ok_or_else(|| {
                ContractError::Capability(format!("unsupported target {}", self.target))
            })?
            .planned_resource_errors(self);
        if !errors.is_empty() {
            return Err(ContractError::Capability(errors.join("; ")));
        }
        self.summary()?;
        Ok(())
    }
}

impl crate::BoundDeployment {
    pub fn validate_resolved(&self) -> Result<(), ContractError> {
        self.validate()?;
        if let Some(generation) = &self.maintenance {
            let intent = generation.bind(self.scope.authorization(), BTreeSet::new())?;
            intent
                .validate(self.target)
                .map_err(|error| ContractError::Capability(error.to_string()))?;
            crate::validate_maintenance_content(&intent.materialization, self.preset.as_ref())?;
        }
        self.requirements.validate()?;
        let mut components = self.selected_components.clone();
        components.sort();
        if self.requirements.generation_readers != self.generation_readers
            || self.requirements.app != self.app
            || self.requirements.target != self.target
            || self.requirements.scope != self.scope
            || self.requirements.components != components
            || self.requirements.preset != self.preset
        {
            return Err(ContractError::Requirements);
        }
        let reserved = self
            .install_directory
            .join(".zup")
            .map_err(|error| ContractError::Capability(error.to_string()))?;
        if let Some(file) = self
            .fixed_files()
            .find(|file| file.destination().starts_with(&reserved))
        {
            return Err(ContractError::PathCollision {
                first: file.destination().to_string(),
                second: reserved.to_string(),
            });
        }
        let owned_paths: Vec<_> = self
            .files
            .iter()
            .map(|file| file.destination())
            .chain(
                self.launchers
                    .iter()
                    .map(|launcher| &launcher.launcher_path),
            )
            .collect();
        for (index, path) in owned_paths.iter().enumerate() {
            let conflict = if self.install_directory.starts_with(path) {
                Some(&self.install_directory)
            } else {
                owned_paths[..index]
                    .iter()
                    .copied()
                    .find(|previous| path.starts_with(previous) || previous.starts_with(path))
            };
            if let Some(previous) = conflict {
                return Err(ContractError::PathCollision {
                    first: previous.to_string(),
                    second: path.to_string(),
                });
            }
        }
        let mut data_paths: Vec<&zup_core::TargetPath> = Vec::new();
        for directory in &self.data {
            let Some(path) = directory.location.path() else {
                continue;
            };
            if path.target() != &self.target {
                return Err(ContractError::Target(self.target));
            }
            if path.starts_with(&reserved) || self.install_directory.starts_with(path) {
                return Err(ContractError::PathCollision {
                    first: path.to_string(),
                    second: self.install_directory.to_string(),
                });
            }
            if let Some(other) = data_paths
                .iter()
                .copied()
                .chain(owned_paths.iter().copied())
                .find(|other| other.starts_with(path) || path.starts_with(other))
            {
                return Err(ContractError::PathCollision {
                    first: other.to_string(),
                    second: path.to_string(),
                });
            }
            data_paths.push(path);
        }
        let paths = std::iter::once(&self.install_directory)
            .chain(self.files.iter().map(|file| file.destination()))
            .chain(self.launchers.iter().flat_map(|launcher| {
                [
                    Some(&launcher.launcher_path),
                    Some(&launcher.target),
                    launcher.working_directory.as_ref(),
                ]
                .into_iter()
                .flatten()
            }))
            .chain(self.path_entries.iter().map(|entry| &entry.value))
            .chain(
                self.services
                    .iter()
                    .map(|service| &service.command.executable),
            )
            .chain(
                self.protocols
                    .iter()
                    .map(|protocol| &protocol.command.executable),
            )
            .chain(
                self.file_associations
                    .iter()
                    .map(|association| &association.command.executable),
            );
        if paths.into_iter().any(|path| path.target() != &self.target) {
            return Err(ContractError::Target(self.target));
        }
        Ok(())
    }
}
