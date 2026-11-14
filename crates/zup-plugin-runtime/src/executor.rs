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
        plan: &zup_core::PackagePlan,
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
            plan.resources
                .into_iter()
                .map(convert_resource)
                .collect::<Result<_, _>>()?,
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

fn convert_resource(resource: ResourceItem) -> Result<PluginResource, PluginFailure> {
    Ok(match resource {
        ResourceItem::Data(directory) => PluginResource::Data {
            directory: convert_data(directory)?,
        },
        ResourceItem::GeneratedFile(file) => PluginResource::GeneratedFile {
            id: file.id,
            replacement: match file.replacement {
                zup_plugin_contract::FixedReplacement::Atomic => zup_core::FixedReplacement::Atomic,
                zup_plugin_contract::FixedReplacement::Quiescent => {
                    zup_core::FixedReplacement::Quiescent
                }
            },
            destination: file.destination,
            contents: file.contents,
        },
        ResourceItem::Launcher(launcher) => PluginResource::Launcher {
            id: launcher.id,
            location: match launcher.location {
                ContractLauncherLocation::Menu => LauncherLocation::Menu,
                ContractLauncherLocation::Desktop => LauncherLocation::Desktop,
            },
            name: launcher.name,
            target: launcher.target,
            arguments: launcher.arguments,
            working_directory: launcher.working_directory,
        },
        ResourceItem::PathEntry(entry) => PluginResource::PathEntry {
            id: entry.id,
            value: entry.value,
        },
        ResourceItem::Service(service) => PluginResource::Service {
            failure: match service.failure {
                zup_plugin_contract::ServiceFailurePolicy::None => {
                    zup_core::ServiceFailurePolicy::None
                }
                zup_plugin_contract::ServiceFailurePolicy::Systemd(policy) => {
                    zup_core::ServiceFailurePolicy::Systemd {
                        condition: match policy.condition {
                            zup_plugin_contract::SystemdRestartCondition::OnFailure => {
                                zup_core::SystemdRestartCondition::OnFailure
                            }
                            zup_plugin_contract::SystemdRestartCondition::OnAbnormal => {
                                zup_core::SystemdRestartCondition::OnAbnormal
                            }
                        },
                        delay_seconds: policy.delay_seconds,
                        start_limit_burst: policy.start_limit_burst,
                        start_limit_interval_seconds: policy.start_limit_interval_seconds,
                    }
                }
                zup_plugin_contract::ServiceFailurePolicy::Windows(policy) => {
                    zup_core::ServiceFailurePolicy::Windows {
                        delay_seconds: policy.delay_seconds,
                        restart_attempts: policy.restart_attempts,
                        reset_seconds: policy.reset_seconds,
                    }
                }
                zup_plugin_contract::ServiceFailurePolicy::Openrc(policy) => {
                    zup_core::ServiceFailurePolicy::Openrc {
                        delay_seconds: policy.delay_seconds,
                        max_respawns: policy.max_respawns,
                        respawn_period_seconds: policy.respawn_period_seconds,
                    }
                }
            },
            execution: match service.execution {
                zup_plugin_contract::ServiceExecution::WindowsJob(account) => {
                    zup_core::ServiceExecution::WindowsJob {
                        account: match account {
                            zup_plugin_contract::WindowsServiceAccount::LocalService => {
                                zup_core::WindowsServiceAccount::LocalService
                            }
                            zup_plugin_contract::WindowsServiceAccount::NetworkService => {
                                zup_core::WindowsServiceAccount::NetworkService
                            }
                            zup_plugin_contract::WindowsServiceAccount::LocalSystem => {
                                zup_core::WindowsServiceAccount::LocalSystem
                            }
                        },
                    }
                }
                zup_plugin_contract::ServiceExecution::SystemdDynamicUser => {
                    zup_core::ServiceExecution::SystemdDynamicUser
                }
                zup_plugin_contract::ServiceExecution::OpenrcUser(user) => {
                    zup_core::ServiceExecution::OpenrcUser {
                        user: user.user,
                        group: user.group,
                    }
                }
                zup_plugin_contract::ServiceExecution::SystemdUser(user) => {
                    zup_core::ServiceExecution::SystemdUser {
                        user: user.user,
                        group: user.group,
                    }
                }
            },
            requires_services: service.requires_services.clone(),
            requires_files: service.requires_files,
            readiness: match service.readiness {
                zup_plugin_contract::ServiceReadiness::Manager => {
                    zup_core::ServiceReadiness::Manager
                }
                zup_plugin_contract::ServiceReadiness::Notify => zup_core::ServiceReadiness::Notify,
            },
            id: service.id,
            native_name: service.native_name,
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
                timeouts: service.lifecycle.timeouts.map(|timeouts| match timeouts {
                    zup_plugin_contract::ServiceTimeouts::Systemd(value) => {
                        zup_core::ServiceTimeouts::Systemd {
                            start_timeout_seconds: value.start_timeout_seconds,
                            abort_timeout_seconds: value.abort_timeout_seconds,
                            stop_grace_seconds: value.stop_grace_seconds,
                            settle_seconds: value.settle_seconds,
                        }
                    }
                    zup_plugin_contract::ServiceTimeouts::Openrc(value) => {
                        zup_core::ServiceTimeouts::Openrc {
                            start_timeout_seconds: value.start_timeout_seconds,
                            stop_grace_seconds: value.stop_grace_seconds,
                            kill_wait_seconds: value.kill_wait_seconds,
                            settle_seconds: value.settle_seconds,
                        }
                    }
                    zup_plugin_contract::ServiceTimeouts::Windows(value) => {
                        zup_core::ServiceTimeouts::Windows {
                            start_observation_seconds: value.start_observation_seconds,
                            stop_observation_seconds: value.stop_observation_seconds,
                        }
                    }
                }),
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
    })
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

fn convert_data(
    directory: zup_plugin_contract::DataDirectory,
) -> Result<zup_core::PersistentDirectory, PluginFailure> {
    use zup_plugin_contract::{DataAccess, DataCompatibility, DataLocation};
    let reject = |error: &dyn std::fmt::Display| PluginFailure::invalid_output(error.to_string());
    let location = match directory.location {
        DataLocation::Path(path) => zup_core::PersistentLocation::Path {
            path: zup_core::Template::parse(&path.path).map_err(|error| reject(&error))?,
            access: match path.access {
                DataAccess::Owner => zup_core::PersistentAccess::Owner,
                DataAccess::Services(services) => zup_core::PersistentAccess::Services {
                    services: services
                        .into_iter()
                        .map(zup_core::ServiceId::new)
                        .collect::<Result<Vec<_>, _>>()
                        .map_err(|error| reject(&error))?
                        .try_into()
                        .map_err(|error: zup_core::InvalidServiceDataWriters| reject(&error))?,
                },
            },
        },
        DataLocation::Service(service) => zup_core::PersistentLocation::Service {
            service: zup_core::ServiceId::new(service).map_err(|error| reject(&error))?,
        },
    };
    let compatibility = match directory.compatibility {
        DataCompatibility::Stable => zup_core::DataCompatibility::Stable,
        DataCompatibility::Schema(schema) => zup_core::DataCompatibility::Schema {
            writes: std::num::NonZeroU32::new(schema.writes).ok_or_else(|| {
                PluginFailure::invalid_output("data schema versions must be positive")
            })?,
            reads: schema
                .reads
                .into_iter()
                .map(|value| {
                    std::num::NonZeroU32::new(value).ok_or_else(|| {
                        PluginFailure::invalid_output("data schema versions must be positive")
                    })
                })
                .collect::<Result<_, _>>()?,
        },
    };
    Ok(zup_core::PersistentDirectory {
        id: zup_core::ResourceId::new(directory.id).map_err(|error| reject(&error))?,
        location,
        compatibility,
    })
}
