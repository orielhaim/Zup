use serde::{Deserialize, Serialize};
use zup_core::{App, ComponentId, PrerequisitePackage, Privilege, Target, Template};

use crate::error::SelectedScope;
use crate::resources::{
    DesiredFile, DesiredFileAssociation, DesiredLauncher, DesiredPathEntry, DesiredProtocol,
    DesiredService, PlannedPrerequisite,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanSummary {
    pub file_count: usize,
    pub install_bytes: u64,
    pub selected_component_count: usize,
    pub resource_count: usize,
    pub requires_authorization: bool,
    pub prerequisite_count: usize,
    pub download_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DesiredState<P = Template, L = ()> {
    pub app: App,
    pub target: Target,
    pub scope: SelectedScope,
    pub install_directory: P,
    pub selected_components: Vec<ComponentId>,

    pub prerequisites: Vec<PlannedPrerequisite>,
    pub files: Vec<DesiredFile<P>>,
    pub launchers: Vec<DesiredLauncher<P, L>>,
    pub path_entries: Vec<DesiredPathEntry<P>>,
    pub services: Vec<DesiredService<P>>,
    pub protocols: Vec<DesiredProtocol<P>>,
    pub file_associations: Vec<DesiredFileAssociation<P>>,

    pub preset: Option<zup_core::InstalledPreset>,
}

impl<P, L> DesiredState<P, L> {
    pub fn summary(&self) -> Result<PlanSummary, crate::ContractError> {
        let mut install_bytes = 0u64;
        for file in &self.files {
            install_bytes = install_bytes
                .checked_add(file.size)
                .ok_or(crate::ContractError::SizeOverflow)?;
        }
        let download_bytes = self
            .prerequisites
            .iter()
            .try_fold(0u64, |total, prerequisite| match &prerequisite.package {
                PrerequisitePackage::Remote {
                    size: Some(size), ..
                } => total
                    .checked_add(*size)
                    .ok_or(crate::ContractError::SizeOverflow),
                PrerequisitePackage::Remote { .. }
                | PrerequisitePackage::Embedded { .. }
                | PrerequisitePackage::DetectOnly
                | PrerequisitePackage::Repository => Ok(total),
            })?;
        let resource_count = self.launchers.len()
            + self.path_entries.len()
            + self.services.len()
            + self.protocols.len()
            + self.file_associations.len();
        let requires_authorization = self
            .prerequisites
            .iter()
            .any(|resource| resource.installer.privilege == Privilege::System)
            || self
                .files
                .iter()
                .any(|resource| resource.privilege == Privilege::System)
            || self
                .launchers
                .iter()
                .any(|resource| resource.privilege == Privilege::System)
            || self
                .path_entries
                .iter()
                .any(|resource| resource.privilege == Privilege::System)
            || self
                .services
                .iter()
                .any(|resource| resource.privilege == Privilege::System)
            || self
                .protocols
                .iter()
                .any(|resource| resource.privilege == Privilege::System)
            || self
                .file_associations
                .iter()
                .any(|resource| resource.privilege == Privilege::System);
        Ok(PlanSummary {
            file_count: self.files.len(),
            install_bytes,
            selected_component_count: self.selected_components.len(),
            resource_count,
            requires_authorization,
            prerequisite_count: self.prerequisites.len(),
            download_bytes,
        })
    }
}

impl<P, L> DesiredState<P, L> {
    pub fn canonicalize_keys(&mut self) {
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
    }
}
