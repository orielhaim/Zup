//! Portable desired installation plan.

use serde::{Deserialize, Serialize};
use zup_core::{App, ComponentId, Template};

use crate::error::SelectedScope;
use crate::resources::{
    PlannedFile, PlannedFileType, PlannedPathEntry, PlannedPrerequisite, PlannedProtocol,
    PlannedService, PlannedShortcut,
};

/// Derived summary of a planned installation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanSummary {
    pub file_count: usize,
    pub install_bytes: u64,
    pub selected_component_count: usize,
    pub resource_count: usize,
    pub requires_elevation: bool,
    pub prerequisite_count: usize,
    pub download_bytes: u64,
}

/// Desired-state installation for one scope and component selection.
///
/// Portable and serializable. Contains no build-machine paths, no unselected
/// resources, no unresolved `${install}`/`${app.*}` variables, and no OS objects.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallPlan {
    pub app: App,
    pub scope: SelectedScope,
    pub install_directory: Template,
    pub selected_components: Vec<ComponentId>,

    pub prerequisites: Vec<PlannedPrerequisite>,
    pub files: Vec<PlannedFile>,
    pub shortcuts: Vec<PlannedShortcut>,
    pub path_entries: Vec<PlannedPathEntry>,
    pub services: Vec<PlannedService>,
    pub protocols: Vec<PlannedProtocol>,
    pub file_types: Vec<PlannedFileType>,

    pub summary: PlanSummary,
}
