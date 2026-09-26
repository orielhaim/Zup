//! Desired-state installation planner for zup.
//!
//! ```text
//! BuildPlan + PlanRequest → InstallPlan
//! ```
//!
//! Answers: *what should this installation contain for this scope/component
//! selection?* Execution delta planning lives in `zup-exec`.

#![forbid(unsafe_code)]

mod error;
mod plan;
mod plan_types;
mod plugins;
mod request;
mod resolve;
mod resources;
mod select;

pub use error::{PlanError, SelectedScope};
pub use plan::{plan, plan_with_plugins, plan_without_plugins};
pub use plan_types::{InstallPlan, PlanSummary};
pub use plugins::{
    CancellationQuery, GeneratedFile, MAX_PLUGIN_ARGUMENT_BYTES, MAX_PLUGIN_ARGUMENTS,
    MAX_PLUGIN_ERROR_BYTES, MAX_PLUGIN_GENERATED_BYTES, MAX_PLUGIN_GENERATED_FILE_BYTES,
    MAX_PLUGIN_RESOURCES, MAX_PLUGIN_STRING_BYTES, MAX_PLUGIN_TEXT_BYTES, NeverCancelled,
    PlannedInstallation, PluginExecutor, PluginFailure, PluginPlanningContext, PluginResource,
    PluginResourceProposal, sanitize_error,
};
pub use request::{ComponentOverrides, PlanRequest};
pub use resolve::{resolve_install_directory, resolve_template};
pub use resources::{
    PlannedFile, PlannedFileAssociation, PlannedLauncher, PlannedPathEntry, PlannedPrerequisite,
    PlannedProtocol, PlannedService,
};
pub use select::select_components;
pub use zup_build::{BuildPlan, TargetBuildPlan};
pub use zup_core::{PluginBinding, PluginId, ResourceKey};
