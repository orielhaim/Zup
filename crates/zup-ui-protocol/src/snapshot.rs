//! The complete presentation state a preset renders.
//!
//! A snapshot is everything, not a delta. A preset that reconnects, a preset
//! that starts late, and a preset that missed an event all render correctly from
//! the snapshot they were handed, because there is no sequence to replay and no
//! order to reconstruct.

use serde::{Deserialize, Serialize};

use crate::{
    ComponentId, DiagnosticPresentation, InstallScope, InstallationHealth, PlanStatus,
    ProgressPresentation, UpdatePresentation,
};

/// What is being installed, as a person sees it named.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProductIdentity {
    pub name: String,
    pub publisher: Option<String>,
    pub version: String,
    pub description: Option<String>,
}

/// How clearly a component group should be presented.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComponentProminence {
    #[default]
    Auto,
    Primary,
    Secondary,
}

/// Whether a group's defaults are enough to install.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SelectionRequirement {
    #[default]
    Defaulted,
    /// At least one optional component in the group must be selected.
    Explicit,
}

/// One component a person may choose.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComponentOption {
    pub id: ComponentId,
    pub name: String,
    pub description: Option<String>,
    /// A required component cannot be turned off.
    pub required: bool,
    /// Whether the operation would include it, as the person has chosen.
    pub selected: bool,
    /// Whether the existing installation has it. Always false on a machine
    /// that has never had the application.
    pub installed: bool,
}

/// One coherent set of components.
///
/// Every member is also in the surface's component list, and every component
/// belongs to exactly one group. A package that declares no groups has one
/// implicit group holding all of its components.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComponentGroupOption {
    pub id: String,
    pub label: Option<String>,
    pub description: Option<String>,
    pub prominence: ComponentProminence,
    pub selection: SelectionRequirement,
    /// Member ids, in display order.
    pub components: Vec<ComponentId>,
}

/// The choices a fresh installation offers.
///
/// What this would cost is not here: it is in [`UiSnapshot::plan`], so there is
/// one number for it rather than one per surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallOptions {
    /// The installed version, when this package replaces one.
    pub existing_version: Option<String>,
    pub scopes: Vec<InstallScope>,
    pub scope: InstallScope,
    pub components: Vec<ComponentOption>,
    /// Component groups, in display order. Empty only when there are no components.
    #[serde(default)]
    pub groups: Vec<ComponentGroupOption>,
    /// A location chosen instead of the application's default, as a concrete
    /// path. `None` installs where the application says; the resolved location
    /// is in the plan.
    pub install_directory: Option<String>,
    /// False when the application author does not allow choosing a location.
    pub allow_directory_override: bool,
}

/// What a person can do to an installation that already exists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MaintenanceState {
    pub installed_version: String,
    pub components: Vec<ComponentOption>,
    #[serde(default)]
    pub groups: Vec<ComponentGroupOption>,
    pub updates_enabled: bool,
    pub scope: InstallScope,
    /// Where the installation is, as a concrete path.
    pub install_directory: Option<String>,
    pub health: InstallationHealth,
}

/// The situation this process was launched into.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "surface")]
pub enum UiSurface {
    /// Nothing is installed, or this package is newer than what is.
    Install(InstallOptions),
    /// An installation exists and can be changed.
    Maintenance(MaintenanceState),
}

impl UiSurface {
    /// The components a person may choose between.
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

    /// The scope this session applies to.
    pub fn scope(&self) -> InstallScope {
        match self {
            Self::Install(options) => options.scope,
            Self::Maintenance(state) => state.scope,
        }
    }

    /// The scopes this session may apply to. One, once an installation exists.
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

    /// The location this session installs into, when one is known without a plan.
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

    /// False when the application author does not allow choosing a location.
    pub fn allows_directory_override(&self) -> bool {
        match self {
            Self::Install(options) => options.allow_directory_override,
            Self::Maintenance(_) => false,
        }
    }

    /// Whether this session can check for updates.
    pub fn updates_enabled(&self) -> bool {
        match self {
            Self::Install(_) => false,
            Self::Maintenance(state) => state.updates_enabled,
        }
    }
}

/// Which lifecycle an operation is.
///
/// Carried on the snapshot because the same progress and the same success read
/// differently for each, and a preset that remembered which button it pressed
/// would be wrong after a reconnect, a retry, or an uninstall the host started.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationKind {
    /// A fresh installation.
    Install,
    /// An installation replacing an older version.
    Upgrade,
    /// A change to which components an installation has.
    Modify,
    /// Restoring managed resources.
    Repair,
    /// Removing the installation.
    Uninstall,
}

/// Something the host can start once an installation has committed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchTarget {
    /// The launcher's own name, as the application declared it.
    pub name: String,
}

/// Where the lifecycle has reached.
///
/// These are the states of an installation, not views. A preset decides what
/// each one looks like, and a preset may present two of them the same way.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum UiState {
    /// Waiting for a person to choose and start.
    Options,
    /// Waiting for a person to choose a maintenance operation.
    Maintenance,
    /// An operation is running.
    Running,
    /// Cancellation was requested and the engine has not reached a safe boundary.
    WaitingForSafeCancellation,
    /// The machine is not ready: running applications hold resources open.
    Blocked { blockers: Vec<String> },
    /// Asked to uninstall and waiting for the person to confirm.
    ConfirmUninstall,
    /// The last operation committed.
    Succeeded,
    /// The last operation failed and nothing is left half-done.
    Failed,
    /// A transaction did not finish safely and has to be reconciled first.
    RecoveryRequired,
}

impl UiState {
    /// Whether an operation is in flight, so the answer is not yet final.
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Running | Self::WaitingForSafeCancellation)
    }
}

/// Everything a preset needs to render the installer right now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiSnapshot {
    pub product: ProductIdentity,
    pub surface: UiSurface,
    pub state: UiState,
    /// The operation that is running, or that last finished.
    pub operation: Option<OperationKind>,
    /// Present while an operation is running.
    pub progress: Option<ProgressPresentation>,
    /// What the current choices would change.
    pub plan: PlanStatus,
    /// Present when the last operation failed or was blocked.
    pub diagnostic: Option<DiagnosticPresentation>,
    /// Present when the application is configured for updates.
    pub update: Option<UpdatePresentation>,
    /// Managed resources the last repair declined to overwrite.
    pub repair_drift: Vec<String>,
    /// What [`UiAction::Launch`](crate::UiAction::Launch) would start, when the
    /// host can start it now.
    pub launch: Option<LaunchTarget>,
}
