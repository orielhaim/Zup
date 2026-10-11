use serde::{Deserialize, Serialize};

use crate::{
    ComponentId, DiagnosticPresentation, InstallScope, InstallationHealth, PlanStatus,
    ProgressPresentation, UpdatePresentation,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProductIdentity {
    pub name: String,
    pub publisher: Option<String>,
    pub version: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComponentProminence {
    #[default]
    Auto,
    Primary,
    Secondary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SelectionRequirement {
    #[default]
    Defaulted,
    Explicit,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComponentOption {
    pub id: ComponentId,
    pub name: String,
    pub description: Option<String>,
    pub required: bool,
    pub selected: bool,
    pub installed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComponentGroupOption {
    pub id: String,
    pub label: Option<String>,
    pub description: Option<String>,
    pub prominence: ComponentProminence,
    pub selection: SelectionRequirement,
    pub components: Vec<ComponentId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallOptions {
    pub existing_version: Option<String>,
    pub scopes: Vec<InstallScope>,
    pub scope: InstallScope,
    pub components: Vec<ComponentOption>,
    #[serde(default)]
    pub groups: Vec<ComponentGroupOption>,
    pub install_directory: Option<String>,
    pub allow_directory_override: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MaintenanceState {
    pub installed_version: String,
    pub components: Vec<ComponentOption>,
    #[serde(default)]
    pub groups: Vec<ComponentGroupOption>,
    pub updates_enabled: bool,
    pub scope: InstallScope,
    pub install_directory: Option<String>,
    pub health: InstallationHealth,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "surface")]
pub enum Surface {
    Install(InstallOptions),
    Maintenance(MaintenanceState),
}

impl Surface {
    pub fn components(&self) -> &[ComponentOption] {
        match self {
            Self::Install(options) => &options.components,
            Self::Maintenance(state) => &state.components,
        }
    }

    pub fn groups(&self) -> &[ComponentGroupOption] {
        match self {
            Self::Install(options) => &options.groups,
            Self::Maintenance(state) => &state.groups,
        }
    }

    pub fn components_mut(&mut self) -> &mut [ComponentOption] {
        match self {
            Self::Install(options) => &mut options.components,
            Self::Maintenance(state) => &mut state.components,
        }
    }

    pub fn scope(&self) -> InstallScope {
        match self {
            Self::Install(options) => options.scope,
            Self::Maintenance(state) => state.scope,
        }
    }

    pub fn scopes(&self) -> &[InstallScope] {
        match self {
            Self::Install(options) => &options.scopes,
            Self::Maintenance(state) => std::slice::from_ref(&state.scope),
        }
    }

    pub fn set_scope(&mut self, scope: InstallScope) {
        match self {
            Self::Install(options) => options.scope = scope,
            Self::Maintenance(state) => state.scope = scope,
        }
    }

    pub fn install_directory(&self) -> Option<&str> {
        match self {
            Self::Install(options) => options.install_directory.as_deref(),
            Self::Maintenance(state) => state.install_directory.as_deref(),
        }
    }

    pub fn set_install_directory(&mut self, directory: Option<String>) {
        match self {
            Self::Install(options) => options.install_directory = directory,
            Self::Maintenance(state) => state.install_directory = directory,
        }
    }

    pub fn allows_directory_override(&self) -> bool {
        match self {
            Self::Install(options) => options.allow_directory_override,
            Self::Maintenance(_) => false,
        }
    }

    pub fn updates_enabled(&self) -> bool {
        match self {
            Self::Install(_) => false,
            Self::Maintenance(state) => state.updates_enabled,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationKind {
    Install,
    Upgrade,
    Modify,
    Repair,
    Uninstall,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchTarget {
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum InstallerState {
    Options,
    Maintenance,
    Running,
    WaitingForSafeCancellation,
    Blocked { blockers: Vec<String> },
    ConfirmUninstall,
    Succeeded,
    Failed,
    RecoveryRequired,
}

impl InstallerState {
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Running | Self::WaitingForSafeCancellation)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub product: ProductIdentity,
    pub surface: Surface,
    pub state: InstallerState,
    pub operation: Option<OperationKind>,
    pub progress: Option<ProgressPresentation>,
    pub plan: PlanStatus,
    pub diagnostic: Option<DiagnosticPresentation>,
    pub update: Option<UpdatePresentation>,
    pub repair_drift: Vec<String>,
    pub launch: Option<LaunchTarget>,
}
