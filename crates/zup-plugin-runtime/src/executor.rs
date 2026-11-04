use std::fmt;

use zup_core::{LauncherLocation, PluginBinding, SelectedScope, ServiceStart, Target};
use zup_plan::{
    CancellationQuery, PluginExecutor, PluginFailure, PluginPlanningContext, PluginResource,
    PluginResourceProposal,
};
use zup_plugin_contract::{
    Context, InstallScope, InvocationError, LauncherLocation as ContractLauncherLocation,
    ResourceItem, ServiceStart as ContractServiceStart,
};

use crate::loader::{LoadError, LoadedBundle, load_bundle};

pub struct WasmtimePluginExecutor {
    loaded: LoadedBundle,
}

impl fmt::Debug for WasmtimePluginExecutor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WasmtimePluginExecutor")
            .field("target", &self.loaded.target)
            .field("plugin_count", &self.loaded.plugins.len())
            .finish()
    }
}

impl WasmtimePluginExecutor {
    pub fn load(package: zup_bundle::Package, expected_target: &Target) -> Result<Self, LoadError> {
        Ok(Self {
            loaded: load_bundle(package, expected_target)?,
        })
    }

    pub fn from_compiled(
        installer: &zup_core::Installer,
        artifacts: &[zup_bundle::CompiledPluginArtifact],
    ) -> Result<Self, LoadError> {
        Ok(Self {
            loaded: crate::loader::load_compiled(installer, artifacts)?,
        })
    }

    pub fn load_acquired(
        plan: &zup_bundle::PortableBuildPlan,
        source: &zup_bundle::AcquiredPayloadSource,
        expected_target: &Target,
    ) -> Result<Self, LoadError> {
        Ok(Self {
            loaded: crate::loader::load_acquired(plan, source, expected_target)?,
        })
    }

    pub fn target(&self) -> &Target {
        &self.loaded.target
    }

    pub fn plugin_count(&self) -> usize {
        self.loaded.plugins.len()
    }
}

impl PluginExecutor for WasmtimePluginExecutor {
    fn target(&self) -> &Target {
        &self.loaded.target
    }

    fn plan(
        &mut self,
        binding: &PluginBinding,
        context: &PluginPlanningContext,
        cancellation: &dyn CancellationQuery,
    ) -> Result<PluginResourceProposal, PluginFailure> {
        if context.target != self.loaded.target {
            return Err(PluginFailure::target_mismatch(
                self.loaded.target,
                context.target,
            ));
        }
        if cancellation.is_cancelled() {
            return Err(PluginFailure::Cancelled);
        }
        let component = self
            .loaded
            .component(&binding.id)
            .map_err(|error| PluginFailure::internal(error.to_string()))?;
        let context = contract_context(binding, context);
        let cancellation_query = || cancellation.is_cancelled();
        let result = component.plan_with_cancellation(&context, &cancellation_query);
        let plan = match result {
            Ok(Ok(plan)) => plan,
            Ok(Err(error)) => {
                return Err(PluginFailure::rejected(error.code, error.message));
            }
            Err(error) => return Err(map_invocation_error(error)),
        };
        Ok(PluginResourceProposal::new(
            plan.resources.into_iter().map(convert_resource).collect(),
        ))
    }
}

fn contract_context(binding: &PluginBinding, context: &PluginPlanningContext) -> Context {
    Context {
        plugin_id: binding.id.to_string(),
        app_id: context.app.id.to_string(),
        app_name: context.app.name.to_string(),
        app_version: context.app.version.to_string(),
        install_directory: context.install_directory.to_string(),
        install_scope: match context.scope {
            SelectedScope::User => InstallScope::User,
            SelectedScope::Machine => InstallScope::Machine,
        },
        target: context.target.to_string(),
        selected_components: context
            .selected_components
            .iter()
            .map(|component| component.to_string())
            .collect(),
    }
}

fn convert_resource(resource: ResourceItem) -> PluginResource {
    match resource {
        ResourceItem::GeneratedFile(file) => PluginResource::GeneratedFile {
            destination: file.destination,
            contents: file.contents,
        },
        ResourceItem::Launcher(launcher) => PluginResource::Launcher {
            location: match launcher.location {
                ContractLauncherLocation::Menu => LauncherLocation::Menu,
                ContractLauncherLocation::Desktop => LauncherLocation::Desktop,
            },
            name: launcher.name,
            target: launcher.target,
            arguments: launcher.arguments,
            working_directory: launcher.working_directory,
        },
        ResourceItem::PathEntry(entry) => PluginResource::PathEntry { value: entry.value },
        ResourceItem::Service(service) => PluginResource::Service {
            readiness: match service.readiness {
                zup_plugin_contract::ServiceReadiness::Manager => {
                    zup_core::ServiceReadiness::Manager
                }
                zup_plugin_contract::ServiceReadiness::Notify => zup_core::ServiceReadiness::Notify,
            },
            id: service.id,
            name: service.name,
            display_name: service.display_name,
            binary: service.binary,
            arguments: service.arguments,
            lifecycle: zup_core::ServiceLifecycle {
                install: match service.lifecycle.install {
                    zup_plugin_contract::InitialServiceState::Stopped => {
                        zup_core::InitialServiceState::Stopped
                    }
                    zup_plugin_contract::InitialServiceState::Running => {
                        zup_core::InitialServiceState::Running
                    }
                },
                change: match service.lifecycle.change {
                    zup_plugin_contract::ChangedServiceState::Preserve => {
                        zup_core::ChangedServiceState::Preserve
                    }
                    zup_plugin_contract::ChangedServiceState::Stopped => {
                        zup_core::ChangedServiceState::Stopped
                    }
                },
                start_timeout_seconds: service.lifecycle.start_timeout_seconds,
                stop_timeout_seconds: service.lifecycle.stop_timeout_seconds,
            },
            start: match service.start {
                ContractServiceStart::Automatic => ServiceStart::Automatic,
                ContractServiceStart::Manual => ServiceStart::Manual,
                ContractServiceStart::Disabled => ServiceStart::Disabled,
            },
        },
        ResourceItem::Protocol(protocol) => PluginResource::Protocol {
            scheme: protocol.scheme,
            executable: protocol.executable,
            args: protocol.args,
        },
        ResourceItem::FileAssociation(file_association) => PluginResource::FileAssociation {
            extension: file_association.extension,
            id: file_association.id,
            description: file_association.description,
            executable: file_association.executable,
        },
    }
}

fn map_invocation_error(error: InvocationError) -> PluginFailure {
    match error {
        InvocationError::Cancelled => PluginFailure::Cancelled,
        InvocationError::Timeout => PluginFailure::timeout(),
        InvocationError::FuelExhausted => PluginFailure::fuel_exhausted(),
        InvocationError::MemoryLimit => PluginFailure::memory_limit(),
        InvocationError::Trap { message } => PluginFailure::trap(message),
        InvocationError::OutputLimit { actual, limit } => {
            PluginFailure::output_limit(format!("returned {actual} bytes; limit is {limit}"))
        }
        InvocationError::ResourceLimit {
            resource,
            actual,
            limit,
        } => PluginFailure::output_limit(format!("returned {actual} {resource}; limit is {limit}")),
        InvocationError::InvalidOutput { message } => PluginFailure::invalid_output(message),
        InvocationError::Setup { message } | InvocationError::Internal { message } => {
            PluginFailure::internal(message)
        }
    }
}
