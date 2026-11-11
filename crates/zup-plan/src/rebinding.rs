use zup_core::{
    CommandSpec, MaterializationSpec, ResourceKey, TargetPath, Template, TemplatePart, Variable,
};

use crate::{
    BoundDeployment, InstallationRecord, LauncherState, OwnedResource, PlannedGeneration,
    ServiceState,
};

pub(crate) struct GenerationRebinding<'a> {
    previous: &'a MaterializationSpec,
    next: &'a PlannedGeneration<TargetPath>,
    desired: &'a BoundDeployment,
    installed: &'a InstallationRecord,
}

#[derive(Clone, Copy)]
enum ReferenceKind {
    File,
    Directory,
}

impl<'a> GenerationRebinding<'a> {
    pub fn new(desired: &'a BoundDeployment, installed: &'a InstallationRecord) -> Option<Self> {
        let previous = installed.current.materialization.as_ref()?;
        let next = desired.generation.as_ref()?;
        let expected = desired
            .install_directory
            .join(format!("{}/{}", zup_core::DEPLOYMENTS_DIRECTORY, next.id))
            .ok()?;
        (installed.validate().is_ok()
            && desired.validate_resolved().is_ok()
            && next.directory == expected
            && desired.requirements == installed.current.requirements
            && previous.descriptor() == &next.descriptor
            && previous.installation() == next.installation
            && next.publication.previous() == Some(installed.current.selection()))
        .then_some(Self {
            previous,
            next,
            desired,
            installed,
        })
    }

    fn path(
        &self,
        template: &Template,
        previous: &TargetPath,
        next: &TargetPath,
        kind: ReferenceKind,
    ) -> bool {
        if !template.contains_variable(Variable::Payload) {
            return previous.equivalent(next);
        }
        let suffix = match template.parts() {
            [TemplatePart::Variable(Variable::Payload)] => "",
            [
                TemplatePart::Variable(Variable::Payload),
                TemplatePart::Literal(suffix),
            ] => suffix.as_str(),
            _ => return false,
        };
        let bind = |root: &TargetPath| {
            if suffix.is_empty() {
                Ok(root.clone())
            } else {
                root.join_suffix(suffix)
            }
        };
        let (Ok(old), Ok(new)) = (bind(self.previous.directory()), bind(&self.next.directory))
        else {
            return false;
        };
        if !old.equivalent(previous) || !new.equivalent(next) {
            return false;
        }
        match kind {
            ReferenceKind::File => self.next.descriptor.files().iter().any(|file| {
                self.next
                    .directory
                    .join(file.path.as_str())
                    .is_ok_and(|path| path.equivalent(&new))
            }),
            ReferenceKind::Directory => {
                new.equivalent(&self.next.directory)
                    || self.next.descriptor.files().iter().any(|file| {
                        self.next
                            .directory
                            .join(file.path.as_str())
                            .ok()
                            .and_then(|path| path.parent())
                            .is_some_and(|parent| parent.starts_with(&new))
                    })
            }
        }
    }

    fn directory(
        &self,
        template: Option<&Template>,
        old: Option<&TargetPath>,
        new: Option<&TargetPath>,
    ) -> bool {
        match (template, old, new) {
            (None, None, None) => true,
            (Some(template), Some(old), Some(new)) => {
                self.path(template, old, new, ReferenceKind::Directory)
            }
            _ => false,
        }
    }

    fn command(
        &self,
        requirement: &CommandSpec<Template>,
        old: &CommandSpec,
        new: &CommandSpec,
    ) -> bool {
        let arguments = requirement
            .arguments
            .iter()
            .map(
                |argument| match zup_core::ServiceArgument::parse(argument).ok()? {
                    zup_core::ServiceArgument::Literal(value) => Some(value.to_owned()),
                    zup_core::ServiceArgument::WindowsJob(id) => {
                        Some(zup_core::service_job_name(self.installed.id, &id))
                    }
                    zup_core::ServiceArgument::NativeName(id) => self
                        .desired
                        .services
                        .iter()
                        .find(|service| service.id == id)
                        .map(|service| service.name.to_string()),
                    zup_core::ServiceArgument::DataDirectory(id) => self
                        .installed
                        .data
                        .get(&id)?
                        .ownership
                        .binding()
                        .path(self.installed.target)
                        .ok()
                        .map(|path| path.to_string()),
                },
            )
            .collect::<Option<Vec<_>>>();
        arguments.as_ref() == Some(&old.arguments)
            && arguments.as_ref() == Some(&new.arguments)
            && self.path(
                &requirement.executable,
                &old.executable,
                &new.executable,
                ReferenceKind::File,
            )
    }

    pub fn permits(&self, key: &ResourceKey) -> bool {
        let requirements = &self.desired.requirements;
        let Some(owned) = self.installed.current.resources.get(key) else {
            return false;
        };
        match owned {
            OwnedResource::Launcher {
                launcher_path,
                installed:
                    LauncherState::Launcher {
                        target,
                        arguments,
                        working_directory,
                    },
                ..
            } => {
                let (Some(required), Some(desired)) = (
                    requirements.launchers.iter().find(|item| &item.key == key),
                    self.desired.launchers.iter().find(|item| &item.key == key),
                ) else {
                    return false;
                };
                launcher_path == &desired.launcher_path
                    && arguments == &required.arguments
                    && arguments == &desired.arguments
                    && self.path(
                        &required.target,
                        target,
                        &desired.target,
                        ReferenceKind::File,
                    )
                    && self.directory(
                        required.working_directory.as_ref(),
                        working_directory.as_ref(),
                        desired.working_directory.as_ref(),
                    )
            }
            OwnedResource::PathEntry { value, .. } => {
                let (Some(required), Some(desired)) = (
                    requirements
                        .path_entries
                        .iter()
                        .find(|item| &item.key == key),
                    self.desired
                        .path_entries
                        .iter()
                        .find(|item| &item.key == key),
                ) else {
                    return false;
                };
                self.path(
                    &required.value,
                    value,
                    &desired.value,
                    ReferenceKind::Directory,
                )
            }
            OwnedResource::Protocol {
                installed: crate::ProtocolState::Registration { command },
                ..
            } => {
                let (Some(required), Some(desired)) = (
                    requirements.protocols.iter().find(|item| &item.key == key),
                    self.desired.protocols.iter().find(|item| &item.key == key),
                ) else {
                    return false;
                };
                self.command(&required.command, command, &desired.command)
            }
            OwnedResource::FileAssociation {
                installed:
                    crate::FileAssociationState::Registration {
                        description,
                        command,
                    },
                ..
            } => {
                let (Some(required), Some(desired)) = (
                    requirements
                        .file_associations
                        .iter()
                        .find(|item| &item.key == key),
                    self.desired
                        .file_associations
                        .iter()
                        .find(|item| &item.key == key),
                ) else {
                    return false;
                };
                description == &required.description
                    && description == &desired.description
                    && self.command(&required.command, command, &desired.command)
            }
            OwnedResource::Service(owned) => {
                let (Some(required), Some(desired)) = (
                    requirements.services.iter().find(|item| &item.key == key),
                    self.desired.services.iter().find(|item| &item.key == key),
                ) else {
                    return false;
                };
                let ServiceState::Registration { command, .. } = owned.installed.as_ref() else {
                    return false;
                };
                if owned.name != desired.name.as_str()
                    || !self.command(&required.command, command, &desired.command)
                    || owned.requires_files.len() != required.requires_files.len()
                    || desired.requires_files.len() != required.requires_files.len()
                    || !required
                        .requires_files
                        .iter()
                        .zip(owned.requires_files.iter().zip(&desired.requires_files))
                        .all(|(requirement, (old, new))| {
                            self.path(requirement, old, new, ReferenceKind::File)
                        })
                {
                    return false;
                }
                let mut rebound = owned.installed.as_ref().clone();
                let ServiceState::Registration { command, .. } = &mut rebound else {
                    return false;
                };
                **command = desired.command.clone();
                rebound
                    == ServiceState::Registration {
                        state_directories: desired.state_directories.clone(),
                        native_timeouts: desired
                            .lifecycle
                            .timeouts_for(&desired.execution)
                            .native(),
                        dependencies: desired.dependency_names.clone(),
                        failure: desired.failure,
                        execution: desired.execution.clone(),
                        display_name: desired
                            .display_name
                            .as_ref()
                            .unwrap_or(&desired.name)
                            .to_string(),
                        command: Box::new(desired.command.clone()),
                        start: desired.start,
                        readiness: desired.readiness,
                    }
            }
            _ => false,
        }
    }
}
