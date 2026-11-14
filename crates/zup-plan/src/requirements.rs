use serde::{Deserialize, Serialize};
use zup_core::{
    App, ComponentId, DeploymentContractId, InstalledPreset, PayloadPlacement, Privilege,
    RelativePath, ResourceKey, SelectedScope, Sha256Digest, Target, Template,
};

use crate::{
    ContractError, DesiredFileAssociation, DesiredLauncher, DesiredPathEntry, DesiredProtocol,
    DesiredService, DesiredState, PlannedPrerequisite,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequiredFile {
    pub key: ResourceKey,
    pub placement: PayloadPlacement<RelativePath, Template>,
    pub size: u64,
    pub sha256: Sha256Digest,
    pub executable: bool,
    pub privilege: Privilege,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeploymentRequirements {
    pub app: App,
    pub generation_readers: zup_core::GenerationReaders,
    pub target: Target,
    pub scope: SelectedScope,
    pub directory: Template,
    pub data: Vec<zup_core::PersistentDirectory>,
    pub components: Vec<ComponentId>,
    pub prerequisites: Vec<PlannedPrerequisite>,
    pub files: Vec<RequiredFile>,
    pub launchers: Vec<DesiredLauncher>,
    pub path_entries: Vec<DesiredPathEntry>,
    pub services: Vec<DesiredService>,
    pub protocols: Vec<DesiredProtocol>,
    pub file_associations: Vec<DesiredFileAssociation>,
    pub preset: Option<InstalledPreset>,
}

impl DeploymentRequirements {
    pub fn bind_locations(
        mut self,
        resolver: &impl crate::InstallLocationResolver,
    ) -> Result<Self, crate::TemplateResolveError> {
        let bind = |template: &mut Template| -> Result<(), crate::TemplateResolveError> {
            if template.contains_variable(zup_core::Variable::Payload) {
                if let Some(variable) = template.parts().iter().find_map(|part| match part {
                    zup_core::TemplatePart::Variable(variable)
                        if *variable != zup_core::Variable::Payload =>
                    {
                        Some(*variable)
                    }
                    _ => None,
                }) {
                    return Err(crate::TemplateResolveError::UnresolvedVariable { variable });
                }
                return Ok(());
            }
            let path = crate::resolve_template_path(template, &self.target, resolver, self.scope)?;
            *template = Template::parse(path.as_str()).map_err(|_| {
                crate::TemplateResolveError::UnresolvedLiteral {
                    text: path.to_string(),
                }
            })?;
            Ok(())
        };
        bind(&mut self.directory)?;
        for directory in &mut self.data {
            if let zup_core::PersistentLocation::Path { path, .. } = &mut directory.location {
                bind(path)?;
            }
        }
        for file in &mut self.files {
            if let PayloadPlacement::Fixed { path, .. } = &mut file.placement {
                bind(path)?;
            }
        }
        for launcher in &mut self.launchers {
            bind(&mut launcher.target)?;
            if let Some(directory) = &mut launcher.working_directory {
                bind(directory)?;
            }
        }
        for entry in &mut self.path_entries {
            bind(&mut entry.value)?;
        }
        for service in &mut self.services {
            bind(&mut service.command.executable)?;
            for path in &mut service.requires_files {
                bind(path)?;
            }
        }
        for protocol in &mut self.protocols {
            bind(&mut protocol.command.executable)?;
        }
        for association in &mut self.file_associations {
            bind(&mut association.command.executable)?;
        }
        self.canonicalize();
        Ok(self)
    }

    pub fn capture(plan: &DesiredState) -> Result<Self, ContractError> {
        plan.validate()?;
        let files = plan
            .files
            .iter()
            .map(|file| {
                let placement = match &file.placement {
                    PayloadPlacement::Fixed { path, replacement } => PayloadPlacement::Fixed {
                        path: path.clone(),
                        replacement: *replacement,
                    },
                    PayloadPlacement::Generation { .. } => {
                        let member = plan
                            .generation
                            .as_ref()
                            .and_then(|generation| {
                                generation.sources.iter().find_map(|(path, source)| {
                                    (file.key == source.key).then_some(path)
                                })
                            })
                            .ok_or_else(|| ContractError::Identity(file.key.clone()))?;
                        PayloadPlacement::Generation {
                            path: member.clone(),
                        }
                    }
                };
                Ok(RequiredFile {
                    key: file.key.clone(),
                    placement,
                    size: file.size,
                    sha256: file.sha256,
                    executable: file.executable,
                    privilege: file.privilege,
                })
            })
            .collect::<Result<Vec<_>, ContractError>>()?;
        let mut requirements = Self {
            generation_readers: plan.generation_readers,
            app: plan.app.clone(),
            target: plan.target,
            scope: plan.scope,
            directory: plan.install_directory.clone(),
            data: plan.data.clone(),
            components: plan.selected_components.clone(),
            prerequisites: plan.prerequisites.clone(),
            files,
            launchers: plan.launchers.clone(),
            path_entries: plan.path_entries.clone(),
            services: plan.services.clone(),
            protocols: plan.protocols.clone(),
            file_associations: plan.file_associations.clone(),
            preset: plan.preset.clone(),
        };
        requirements.canonicalize();
        Ok(requirements)
    }

    fn canonicalize(&mut self) {
        self.data.sort_by(|a, b| a.id.cmp(&b.id));
        for key in self
            .files
            .iter_mut()
            .map(|resource| &mut resource.key)
            .chain(self.launchers.iter_mut().map(|resource| &mut resource.key))
            .chain(
                self.path_entries
                    .iter_mut()
                    .map(|resource| &mut resource.key),
            )
            .chain(self.services.iter_mut().map(|resource| &mut resource.key))
            .chain(self.protocols.iter_mut().map(|resource| &mut resource.key))
            .chain(
                self.file_associations
                    .iter_mut()
                    .map(|resource| &mut resource.key),
            )
        {
            *key = key.canonical_for(&self.target);
        }
        self.components.sort();
        self.prerequisites.sort_by(|a, b| a.id.cmp(&b.id));
        self.files.sort_by(|a, b| a.key.cmp(&b.key));
        self.launchers.sort_by(|a, b| a.key.cmp(&b.key));
        self.path_entries.sort_by(|a, b| a.key.cmp(&b.key));
        self.services.sort_by(|a, b| a.key.cmp(&b.key));
        self.protocols.sort_by(|a, b| a.key.cmp(&b.key));
        self.file_associations.sort_by(|a, b| a.key.cmp(&b.key));
    }

    pub fn validate(&self) -> Result<(), ContractError> {
        if self.generation_readers == zup_core::GenerationReaders::NativeExecutables
            && !self
                .files
                .iter()
                .any(|file| matches!(file.placement, PayloadPlacement::Generation { .. }))
        {
            return Err(ContractError::Capability(
                "native generation readers require versioned application files".into(),
            ));
        }
        if self.data.iter().any(|directory| {
            directory.location.path().is_some_and(|path| {
                path.contains_variable(zup_core::Variable::Payload)
                    || path.contains_variable(zup_core::Variable::AppVersion)
            })
        }) {
            return Err(ContractError::Capability(
                "persistent data paths must remain stable across deployments".into(),
            ));
        }

        for data in &self.data {
            for id in data.location.services() {
                if self.scope != SelectedScope::Machine
                    || !self.services.iter().any(|service| &service.id == id)
                {
                    return Err(ContractError::Capability(
                        "service data requires an included machine service".into(),
                    ));
                }
            }
        }
        let mut native_service_names = std::collections::BTreeSet::new();
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
                        "service {} references data without declared access",
                        service.id
                    )));
                }
            }
            let expected = self
                .data
                .iter()
                .filter_map(|data| match &data.location {
                    zup_core::PersistentLocation::Service { service: id }
                        if id == &service.id
                            && matches!(
                                service.execution,
                                zup_core::ServiceExecution::SystemdUser { .. }
                                    | zup_core::ServiceExecution::SystemdDynamicUser
                            ) =>
                    {
                        Some(crate::service_state_name(&self.app.id, id, &data.id))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            let mut expected = expected;
            expected.sort();
            if service.state_directories != expected {
                return Err(ContractError::Capability(
                    "native state namespaces differ from the service data declarations".into(),
                ));
            }
        }
        let mut canonical = self.clone();
        canonical.canonicalize();
        if canonical != *self
            || self.data.windows(2).any(|pair| pair[0].id == pair[1].id)
            || self.components.windows(2).any(|pair| pair[0] == pair[1])
            || self
                .prerequisites
                .windows(2)
                .any(|pair| pair[0].id == pair[1].id)
        {
            return Err(ContractError::Capability(
                "deployment requirements are not in canonical order or repeat a selection".into(),
            ));
        }
        let mut keys = std::collections::BTreeSet::new();
        for key in self
            .files
            .iter()
            .map(|resource| &resource.key)
            .chain(self.launchers.iter().map(|resource| &resource.key))
            .chain(self.path_entries.iter().map(|resource| &resource.key))
            .chain(self.services.iter().map(|resource| &resource.key))
            .chain(self.protocols.iter().map(|resource| &resource.key))
            .chain(self.file_associations.iter().map(|resource| &resource.key))
        {
            if key.canonical_for(&self.target) != *key {
                return Err(ContractError::Identity(key.clone()));
            }
            if !keys.insert(key) {
                return Err(ContractError::Duplicate(key.clone()));
            }
        }
        let files = self
            .files
            .iter()
            .filter_map(|file| match &file.placement {
                PayloadPlacement::Generation { path } => Some(zup_core::GenerationFile {
                    path: path.clone(),
                    size: file.size,
                    sha256: file.sha256,
                    executable: file.executable,
                }),
                PayloadPlacement::Fixed { .. } => None,
            })
            .collect();
        zup_core::GenerationDescriptor::new(self.target, files)?;
        Ok(())
    }

    pub fn identity(&self) -> DeploymentContractId {
        let mut canonical = self.clone();
        canonical.canonicalize();
        let preimage = serde_json::to_vec(&("zup.deployment.requirements", 1u32, &canonical))
            .expect("deployment requirements serialize");
        DeploymentContractId::from_digest(zup_core::hash_bytes(&preimage))
    }
}

impl<R> DesiredState<zup_core::TargetPath, zup_core::TargetPath, R> {
    pub fn bind_fixed(self) -> Result<crate::BoundDeployment, ContractError> {
        if self.generation.is_some() || self.maintenance.is_some() {
            return Err(ContractError::Capability(
                "generation requirements must be bound from symbolic declarations".into(),
            ));
        }
        let template = |path: &zup_core::TargetPath| {
            Template::parse(path.as_str())
                .map_err(|error| ContractError::Capability(error.to_string()))
        };
        let command = |command: &zup_core::CommandSpec| -> Result<_, ContractError> {
            Ok(zup_core::CommandSpec::new(
                template(&command.executable)?,
                command.arguments.clone(),
            ))
        };
        let symbolic = DesiredState {
            generation_readers: self.generation_readers,
            requirements: (),
            maintenance: None,
            installation: self.installation,
            generation: None,
            app: self.app.clone(),
            target: self.target,
            scope: self.scope,
            install_directory: template(&self.install_directory)?,
            data: self
                .data
                .iter()
                .map(|directory| directory.map_path(template))
                .collect::<Result<_, _>>()?,
            selected_components: self.selected_components.clone(),
            prerequisites: self.prerequisites.clone(),
            files: self
                .files
                .iter()
                .map(|file| {
                    Ok(crate::DesiredFile {
                        key: file.key.clone(),
                        source_relative: file.source_relative.clone(),
                        placement: file.placement.resolve(|path| template(path))?,
                        size: file.size,
                        sha256: file.sha256,
                        privilege: file.privilege,
                        executable: file.executable,
                    })
                })
                .collect::<Result<_, ContractError>>()?,
            launchers: self
                .launchers
                .iter()
                .map(|launcher| {
                    Ok(DesiredLauncher {
                        key: launcher.key.clone(),
                        location: launcher.location,
                        launcher_path: (),
                        name: launcher.name.clone(),
                        target: template(&launcher.target)?,
                        arguments: launcher.arguments.clone(),
                        working_directory: launcher
                            .working_directory
                            .as_ref()
                            .map(template)
                            .transpose()?,
                        privilege: launcher.privilege,
                    })
                })
                .collect::<Result<_, ContractError>>()?,
            path_entries: self
                .path_entries
                .iter()
                .map(|entry| {
                    Ok(DesiredPathEntry {
                        key: entry.key.clone(),
                        value: template(&entry.value)?,
                        scope: entry.scope,
                        privilege: entry.privilege,
                    })
                })
                .collect::<Result<_, ContractError>>()?,
            services: self
                .services
                .iter()
                .map(|service| {
                    Ok(DesiredService {
                        state_directories: service.state_directories.clone(),
                        key: service.key.clone(),
                        id: service.id.clone(),
                        name: service.name.clone(),
                        display_name: service.display_name.clone(),
                        command: command(&service.command)?,
                        start: service.start,
                        lifecycle: service.lifecycle,
                        readiness: service.readiness,
                        requires_files: service
                            .requires_files
                            .iter()
                            .map(template)
                            .collect::<Result<_, _>>()?,
                        requires_services: service.requires_services.clone(),
                        dependency_names: service.dependency_names.clone(),
                        privilege: service.privilege,
                        execution: service.execution.clone(),
                        failure: service.failure,
                    })
                })
                .collect::<Result<_, ContractError>>()?,
            protocols: self
                .protocols
                .iter()
                .map(|protocol| {
                    Ok(DesiredProtocol {
                        key: protocol.key.clone(),
                        scheme: protocol.scheme.clone(),
                        command: command(&protocol.command)?,
                        scope: protocol.scope,
                        privilege: protocol.privilege,
                    })
                })
                .collect::<Result<_, ContractError>>()?,
            file_associations: self
                .file_associations
                .iter()
                .map(|association| {
                    Ok(DesiredFileAssociation {
                        key: association.key.clone(),
                        extension: association.extension.clone(),
                        id: association.id.clone(),
                        description: association.description.clone(),
                        command: command(&association.command)?,
                        scope: association.scope,
                        privilege: association.privilege,
                    })
                })
                .collect::<Result<_, ContractError>>()?,
            preset: self.preset.clone(),
        };
        let requirements = DeploymentRequirements::capture(&symbolic)?;
        Ok(DesiredState {
            generation_readers: self.generation_readers,
            requirements,
            maintenance: self.maintenance,
            installation: self.installation,
            generation: self.generation,
            app: self.app,
            target: self.target,
            scope: self.scope,
            install_directory: self.install_directory,
            selected_components: self.selected_components,
            prerequisites: self.prerequisites,
            files: self.files,
            data: self.data,
            launchers: self.launchers,
            path_entries: self.path_entries,
            services: self.services,
            protocols: self.protocols,
            file_associations: self.file_associations,
            preset: self.preset,
        })
    }
}
