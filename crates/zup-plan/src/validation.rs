use std::{collections::BTreeSet, fmt::Display};

use thiserror::Error;
use zup_core::{ResourceKey, Target};

use crate::{DesiredState, TargetPath, capabilities};

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ContractError {
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
}

impl<P: Display, L> DesiredState<P, L> {
    pub fn validate(&self) -> Result<(), ContractError> {
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
        for file in &self.files {
            let expected = match &file.key {
                ResourceKey::Maintenance {
                    app_id, version, ..
                } if app_id == self.app.id.as_str() && version == &self.app.version.to_string() => {
                    ResourceKey::Maintenance {
                        app_id: app_id.clone(),
                        version: version.clone(),
                        destination: file.destination.to_string(),
                    }
                }
                _ => ResourceKey::File {
                    destination: file.destination.to_string(),
                },
            };
            claim(&file.key, expected)?;
        }
        for launcher in &self.launchers {
            claim(
                &launcher.key,
                ResourceKey::Launcher {
                    location: launcher.location,
                    name: launcher.name.to_string(),
                },
            )?;
        }
        for entry in &self.path_entries {
            claim(
                &entry.key,
                ResourceKey::PathEntry {
                    value: entry.value.to_string(),
                },
            )?;
        }
        for service in &self.services {
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

impl DesiredState<TargetPath, TargetPath> {
    pub fn validate_resolved(&self) -> Result<(), ContractError> {
        self.validate()?;
        let owned_paths: Vec<_> = self
            .files
            .iter()
            .map(|file| &file.destination)
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
        let paths = std::iter::once(&self.install_directory)
            .chain(self.files.iter().map(|file| &file.destination))
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
