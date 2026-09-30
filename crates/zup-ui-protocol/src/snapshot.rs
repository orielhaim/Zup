//! The complete presentation state a preset renders.
//!
//! A snapshot is everything, not a delta. A preset that reconnects, a preset
//! that starts late, and a preset that missed an event all render correctly from
//! the snapshot they were handed, because there is no sequence to replay and no
//! order to reconstruct.

use serde::{Deserialize, Serialize};

use crate::{
    ComponentId, DiagnosticPresentation, InstallScope, InstallationHealth, PlanPreview,
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

/// One component a person may choose.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComponentOption {
    pub id: ComponentId,
    pub name: String,
    pub description: Option<String>,
    /// A required component cannot be turned off.
    pub required: bool,
    pub selected: bool,
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
    pub install_directory: Option<String>,
    /// False when the application author does not allow choosing a location.
    pub allow_directory_override: bool,
}

/// What a person can do to an installation that already exists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MaintenanceState {
    pub installed_version: String,
    pub components: Vec<ComponentOption>,
    pub updates_enabled: bool,
    pub scope: InstallScope,
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

    /// The location this session installs into, as a person reads it.
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
    /// Present while an operation is running.
    pub progress: Option<ProgressPresentation>,
    /// The most recent answer to "what will change".
    pub plan: Option<PlanPreview>,
    /// Present when the last operation failed or was blocked.
    pub diagnostic: Option<DiagnosticPresentation>,
    /// Present when the application is configured for updates.
    pub update: Option<UpdatePresentation>,
    /// Managed resources the last repair declined to overwrite.
    pub repair_drift: Vec<String>,
}
