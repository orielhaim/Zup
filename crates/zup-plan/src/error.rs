use miette::Diagnostic;
use thiserror::Error;
use zup_core::{ComponentId, PluginId, TargetTriple, Variable};

use crate::plugins::PluginFailure;

pub use zup_core::SelectedScope;

#[derive(Debug, Error, Diagnostic)]
pub enum PlanError {
    #[error("build plan has no target `{target}`")]
    #[diagnostic(code(zup_plan::unknown_build_target))]
    UnknownBuildTarget { target: zup_core::TargetTriple },

    #[error("scope `{requested}` is not allowed (manifest scope is `{allowed}`)")]
    #[diagnostic(code(zup_plan::scope_not_allowed))]
    ScopeNotAllowed {
        requested: SelectedScope,
        allowed: zup_core::InstallScope,
    },

    #[error("install directory for scope `{scope}` is not configured")]
    #[diagnostic(code(zup_plan::scope_required))]
    ScopeRequired { scope: SelectedScope },

    #[error("install directory override is not allowed by the manifest")]
    #[diagnostic(
        code(zup_plan::install_directory_override_not_allowed),
        help("set [install] allow_directory_override = true to expose the path control")
    )]
    InstallDirectoryOverrideNotAllowed,

    #[error("active plugin `{plugin_id}` requires the plugin planning seam")]
    #[diagnostic(code(zup_plan::plugin_planning_required))]
    PluginPlanningRequired { plugin_id: PluginId },

    #[error("plugin executor target `{found}` does not match plan target `{expected}`")]
    #[diagnostic(code(zup_plan::plugin_target_mismatch))]
    PluginTargetMismatch {
        expected: TargetTriple,
        found: TargetTriple,
    },

    #[error("plugin `{plugin_id}` failed: {failure}")]
    #[diagnostic(code(zup_plan::plugin_execution_failed))]
    PluginExecutionFailed {
        plugin_id: PluginId,
        #[source]
        failure: PluginFailure,
    },

    #[error("plugin `{plugin_id}` planning was cancelled")]
    #[diagnostic(code(zup_plan::plugin_cancelled))]
    PluginCancelled { plugin_id: PluginId },

    #[error("plugin `{plugin_id}` returned invalid resource `{resource}`: {reason}")]
    #[diagnostic(code(zup_plan::plugin_resource_rejected))]
    PluginResourceRejected {
        plugin_id: PluginId,
        resource: String,
        reason: String,
    },

    #[error("plugin resource collision for `{resource}` at `{identity}`")]
    #[diagnostic(code(zup_plan::plugin_resource_collision))]
    PluginResourceCollision {
        plugin_id: Option<PluginId>,
        resource: String,
        identity: String,
        existing_plugin_id: Option<PluginId>,
        existing_resource: String,
    },

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

    #[error("unknown component `{id}` in plan request")]
    #[diagnostic(code(zup_plan::unknown_component_override))]
    UnknownComponentOverride { id: ComponentId },

    #[error("component `{id}` is both enabled and disabled")]
    #[diagnostic(code(zup_plan::component_both_enabled_and_disabled))]
    ComponentBothEnabledAndDisabled { id: ComponentId },

    #[error("required component `{id}` cannot be disabled")]
    #[diagnostic(code(zup_plan::required_component_disabled))]
    RequiredComponentDisabled { id: ComponentId },

    #[error("component `{id}` is required by the selection but was explicitly disabled")]
    #[diagnostic(
        code(zup_plan::dependency_explicitly_disabled),
        help("enable `{id}` or disable the components that require it")
    )]
    DependencyExplicitlyDisabled { id: ComponentId },

    #[error("install directory references `${{install}}`")]
    #[diagnostic(code(zup_plan::recursive_install_directory))]
    RecursiveInstallDirectory,

    #[error("template resolution failed for variable `{variable}`")]
    #[diagnostic(code(zup_plan::template_resolution))]
    TemplateResolutionError { variable: Variable },

    #[error("active launcher collision at `{location}` / `{name}`")]
    #[diagnostic(code(zup_plan::active_launcher_collision))]
    ActiveLauncherCollision { location: String, name: String },

    #[error("active search-path entry collision at `{value}`")]
    #[diagnostic(code(zup_plan::active_path_collision))]
    ActivePathCollision { value: String },

    #[error("active service collision for `{id}`")]
    #[diagnostic(code(zup_plan::active_service_collision))]
    ActiveServiceCollision { id: String },

    #[error("active protocol collision for scheme `{scheme}`")]
    #[diagnostic(code(zup_plan::active_protocol_collision))]
    ActiveProtocolCollision { scheme: String },

    #[error("active file association collision for id `{id}`")]
    #[diagnostic(code(zup_plan::active_file_association_collision))]
    ActiveFileAssociationCollision { id: String },

    #[error("active file extension collision for `{extension}`")]
    #[diagnostic(code(zup_plan::active_extension_collision))]
    ActiveExtensionCollision { extension: String },

    #[error("active file destination collision at `{destination}`")]
    #[diagnostic(code(zup_plan::active_file_collision))]
    ActiveFileCollision { destination: String },

    #[error("plan size overflow")]
    #[diagnostic(code(zup_plan::size_overflow))]
    SizeOverflow,
}
