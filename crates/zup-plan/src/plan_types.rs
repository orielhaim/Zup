use serde::{Deserialize, Serialize};
use zup_core::{App, ComponentId, TargetTriple, Template};

use crate::error::SelectedScope;
use crate::resources::{
    PlannedFile, PlannedFileAssociation, PlannedLauncher, PlannedPathEntry, PlannedPrerequisite,
    PlannedProtocol, PlannedService,
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
pub struct InstallPlan {
    pub app: App,
    pub target: TargetTriple,
    pub scope: SelectedScope,
    pub install_directory: Template,
    pub selected_components: Vec<ComponentId>,

    pub prerequisites: Vec<PlannedPrerequisite>,
    pub files: Vec<PlannedFile>,
    pub launchers: Vec<PlannedLauncher>,
    pub path_entries: Vec<PlannedPathEntry>,
    pub services: Vec<PlannedService>,
    pub protocols: Vec<PlannedProtocol>,
    pub file_associations: Vec<PlannedFileAssociation>,

    pub summary: PlanSummary,
}
