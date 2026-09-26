use std::fmt;

use zup_core::{LauncherLocation, PluginBinding, SelectedScope, ServiceStart, TargetTriple};
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
    pub fn load(
        package: zup_bundle::Package,
        expected_target: &TargetTriple,
    ) -> Result<Self, LoadError> {
        Ok(Self {
            loaded: load_bundle(package, expected_target)?,
        })
    }

    pub fn target(&self) -> &TargetTriple {
        &self.loaded.target
    }

    pub fn plugin_count(&self) -> usize {
        self.loaded.plugins.len()
    }
}

impl PluginExecutor for WasmtimePluginExecutor {
    fn target(&self) -> &TargetTriple {
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
                self.loaded.target.clone(),
                context.target.clone(),
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
            id: service.id,
            name: service.name,
            display_name: service.display_name,
            binary: service.binary,
            arguments: service.arguments,
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

#[cfg(test)]
mod tests {
    use super::*;
    use zup_core::{AppId, NonEmptyString};
    use zup_plugin_contract::{
        FileAssociation, GeneratedFile, LauncherLocation as ContractLauncherLocation, PathEntry,
        Protocol, ResourceItem, Service, ServiceStart as ContractServiceStart,
    };

    #[test]
    fn converts_all_contract_resource_families() {
        let resources = [
            ResourceItem::GeneratedFile(GeneratedFile {
                destination: "file".to_owned(),
                contents: vec![1, 2, 3],
            }),
            ResourceItem::Launcher(zup_plugin_contract::Launcher {
                location: ContractLauncherLocation::Desktop,
                name: "name".to_owned(),
                target: "target".to_owned(),
                arguments: vec!["arg".to_owned()],
                working_directory: None,
            }),
            ResourceItem::PathEntry(PathEntry {
                value: "path".to_owned(),
            }),
            ResourceItem::Service(Service {
                id: "service".to_owned(),
                name: "service".to_owned(),
                display_name: None,
                binary: "binary".to_owned(),
                arguments: Vec::new(),
                start: ContractServiceStart::Manual,
            }),
            ResourceItem::Protocol(Protocol {
                scheme: "scheme".to_owned(),
                executable: "executable".to_owned(),
                args: Vec::new(),
            }),
            ResourceItem::FileAssociation(FileAssociation {
                extension: ".ext".to_owned(),
                id: "type".to_owned(),
                description: None,
                executable: "executable".to_owned(),
            }),
        ];
        let converted = resources
            .into_iter()
            .map(convert_resource)
            .collect::<Vec<_>>();
        assert_eq!(converted.len(), 6);
        assert!(matches!(converted[0], PluginResource::GeneratedFile { .. }));
        assert!(matches!(converted[1], PluginResource::Launcher { .. }));
        assert!(matches!(
            &converted[1],
            PluginResource::Launcher {
                location: LauncherLocation::Desktop,
                ..
            }
        ));
        assert!(matches!(converted[2], PluginResource::PathEntry { .. }));
        assert!(matches!(converted[3], PluginResource::Service { .. }));
        assert!(matches!(
            &converted[3],
            PluginResource::Service {
                start: ServiceStart::Manual,
                ..
            }
        ));
        assert!(matches!(converted[4], PluginResource::Protocol { .. }));
        assert!(matches!(
            converted[5],
            PluginResource::FileAssociation { .. }
        ));
    }

    #[test]
    fn passes_plugin_identity_and_target_exactly() {
        let binding = PluginBinding {
            id: zup_core::PluginId::new("helper").unwrap(),
            component: None,
            when: None,
        };
        let app = zup_core::App {
            id: AppId::new("com.example.app").unwrap(),
            name: NonEmptyString::new("App").unwrap(),
            version: "1.2.3".parse().unwrap(),
            publisher: None,
            main: None,
            description: None,
        };
        let planning = PluginPlanningContext {
            app,
            install_directory: zup_core::Template::parse("${location.user_data}/App").unwrap(),
            scope: SelectedScope::Machine,
            selected_components: vec![zup_core::ComponentId::new("core").unwrap()],
            target: TargetTriple::parse("aarch64-pc-windows-msvc").unwrap(),
        };
        let context = contract_context(&binding, &planning);
        assert_eq!(context.plugin_id, "helper");
        assert_eq!(context.app_id, "com.example.app");
        assert_eq!(context.app_name, "App");
        assert_eq!(context.app_version, "1.2.3");
        assert_eq!(context.install_directory, "${location.user_data}/App");
        assert!(matches!(context.install_scope, InstallScope::Machine));
        assert_eq!(context.target, "aarch64-pc-windows-msvc");
        assert_eq!(context.selected_components, ["core"]);
    }

    #[test]
    fn maps_typed_contract_failures() {
        assert_eq!(
            map_invocation_error(InvocationError::Timeout),
            PluginFailure::Timeout
        );
        assert_eq!(
            map_invocation_error(InvocationError::FuelExhausted),
            PluginFailure::FuelExhausted
        );
        assert_eq!(
            map_invocation_error(InvocationError::MemoryLimit),
            PluginFailure::MemoryLimit
        );
        assert_eq!(
            map_invocation_error(InvocationError::OutputLimit {
                actual: 9,
                limit: 8,
            }),
            PluginFailure::output_limit("returned 9 bytes; limit is 8")
        );
    }
}
