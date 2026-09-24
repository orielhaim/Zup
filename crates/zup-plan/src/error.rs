//! Desired-state planning errors.

use miette::Diagnostic;
use thiserror::Error;
use zup_core::{ComponentId, PluginId, Variable};

use crate::plugins::PluginFailure;

pub use zup_core::SelectedScope;

/// Errors produced while computing a desired [`crate::InstallPlan`].
#[derive(Debug, Error, Diagnostic)]
pub enum PlanError {
    /// Requested scope is not allowed by the manifest install scope.
    #[error("scope `{requested}` is not allowed (manifest scope is `{allowed}`)")]
    #[diagnostic(code(zup_plan::scope_not_allowed))]
    ScopeNotAllowed {
        requested: SelectedScope,
        allowed: zup_core::InstallScope,
    },

    /// No install directory is configured for the concrete scope.
    #[error("install directory for scope `{scope}` is not configured")]
    #[diagnostic(code(zup_plan::scope_required))]
    ScopeRequired { scope: SelectedScope },

    #[error("install directory override is not allowed by the manifest")]
    #[diagnostic(
        code(zup_plan::install_directory_override_not_allowed),
        help("set [install] allow_directory_override = true to expose the path control")
    )]
    InstallDirectoryOverrideNotAllowed,

    /// The ordinary planner cannot execute an active plugin.
    #[error("active plugin `{plugin_id}` requires the plugin planning seam")]
    #[diagnostic(code(zup_plan::plugin_planning_required))]
    PluginPlanningRequired { plugin_id: PluginId },

    /// An executor failed while planning a plugin.
    #[error("plugin `{plugin_id}` failed: {failure}")]
    #[diagnostic(code(zup_plan::plugin_execution_failed))]
    PluginExecutionFailed {
        plugin_id: PluginId,
        #[source]
        failure: PluginFailure,
    },

    /// Planning was cancelled before a plugin was invoked.
    #[error("plugin `{plugin_id}` planning was cancelled")]
    #[diagnostic(code(zup_plan::plugin_cancelled))]
    PluginCancelled { plugin_id: PluginId },

    /// A plugin returned a resource that cannot be represented by the core model.
    #[error("plugin `{plugin_id}` returned invalid resource `{resource}`: {reason}")]
    #[diagnostic(code(zup_plan::plugin_resource_rejected))]
    PluginResourceRejected {
        plugin_id: PluginId,
        resource: String,
        reason: String,
    },

    /// A plugin resource collided with another active or proposed resource.
    #[error("plugin resource collision for `{resource}` at `{identity}`")]
    #[diagnostic(code(zup_plan::plugin_resource_collision))]
    PluginResourceCollision {
        plugin_id: Option<PluginId>,
        resource: String,
        identity: String,
        existing_plugin_id: Option<PluginId>,
        existing_resource: String,
    },

    /// A plugin proposal exceeded a bounded resource limit.
    #[error(
        "plugin `{plugin_id}` resource limit exceeded for `{resource}`: {actual} exceeds {limit}"
    )]
    #[diagnostic(code(zup_plan::plugin_resource_limit))]
    PluginResourceLimit {
        plugin_id: PluginId,
        resource: String,
        actual: u64,
        limit: u64,
    },

    /// An enable/disable override names an unknown component.
    #[error("unknown component `{id}` in plan request")]
    #[diagnostic(code(zup_plan::unknown_component_override))]
    UnknownComponentOverride { id: ComponentId },

    /// The same component is both explicitly enabled and disabled.
    #[error("component `{id}` is both enabled and disabled")]
    #[diagnostic(code(zup_plan::component_both_enabled_and_disabled))]
    ComponentBothEnabledAndDisabled { id: ComponentId },

    /// Explicit disable of a required component.
    #[error("required component `{id}` cannot be disabled")]
    #[diagnostic(code(zup_plan::required_component_disabled))]
    RequiredComponentDisabled { id: ComponentId },

    /// A selected component depends on an explicitly disabled component.
    #[error("component `{id}` is required by the selection but was explicitly disabled")]
    #[diagnostic(
        code(zup_plan::dependency_explicitly_disabled),
        help("enable `{id}` or disable the components that require it")
    )]
    DependencyExplicitlyDisabled { id: ComponentId },

    /// Install directory template still references `${install}` after validation.
    #[error("install directory references `${{install}}`")]
    #[diagnostic(code(zup_plan::recursive_install_directory))]
    RecursiveInstallDirectory,

    /// Template substitution left the template in an invalid state.
    #[error("template resolution failed for variable `{variable}`")]
    #[diagnostic(code(zup_plan::template_resolution))]
    TemplateResolutionError { variable: Variable },

    /// Two active shortcuts share location and name.
    #[error("active shortcut collision at `{location}` / `{name}`")]
    #[diagnostic(code(zup_plan::active_shortcut_collision))]
    ActiveShortcutCollision { location: String, name: String },

    /// Two active PATH entries resolve to the same value.
    #[error("active PATH entry collision at `{value}`")]
    #[diagnostic(code(zup_plan::active_path_collision))]
    ActivePathCollision { value: String },

    /// Two active services share an id.
    #[error("active service collision for `{id}`")]
    #[diagnostic(code(zup_plan::active_service_collision))]
    ActiveServiceCollision { id: String },

    /// Two active protocols share a scheme.
    #[error("active protocol collision for scheme `{scheme}`")]
    #[diagnostic(code(zup_plan::active_protocol_collision))]
    ActiveProtocolCollision { scheme: String },

    /// Two active file types share an id.
    #[error("active file type collision for id `{id}`")]
    #[diagnostic(code(zup_plan::active_file_type_collision))]
    ActiveFileTypeCollision { id: String },

    /// Two active file types own the same extension.
    #[error("active file extension collision for `{extension}`")]
    #[diagnostic(code(zup_plan::active_extension_collision))]
    ActiveExtensionCollision { extension: String },

    /// Two active files share a destination identity.
    #[error("active file destination collision at `{destination}`")]
    #[diagnostic(code(zup_plan::active_file_collision))]
    ActiveFileCollision { destination: String },

    /// Checked size arithmetic overflowed.
    #[error("plan size overflow")]
    #[diagnostic(code(zup_plan::size_overflow))]
    SizeOverflow,
}
