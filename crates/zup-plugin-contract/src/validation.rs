use std::fmt;
use std::sync::{Arc, Mutex};

use thiserror::Error;
use wasmtime::Precompiled;
use wasmtime::Trap;
use wasmtime::component::types::ComponentItem;
use wasmtime::component::{Component, Linker};

use crate::bindings::PluginPre;
use crate::config::{
    MAX_AOT_BYTES, MAX_MEMORY_COUNT, MAX_MEMORY_PAGES, MAX_PLAN_OUTPUT_BYTES, MAX_PLAN_RESOURCES,
    MAX_TABLE_COUNT, MAX_TABLE_ELEMENTS, PluginEngine,
};
use crate::runtime::{
    InvocationError, SandboxLimits, run_with_watchdog, sandbox_store, sanitize_error,
};
use crate::{Context, InstallationPlan, PluginError, ResourceItem};
use zup_plugin_abi::{PLAN_FUNCTION_NAME, PLANNER_EXPORT_NAME};

#[derive(Clone)]
pub struct ValidatedComponent {
    pre: PluginPre<SandboxLimits>,
    fingerprint: crate::EngineFingerprint,
}

impl fmt::Debug for ValidatedComponent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ValidatedComponent")
            .field("fingerprint", &self.fingerprint)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ContractError {
    #[error("expected a WebAssembly component, got a core WebAssembly module")]
    CoreModule,
    #[error("component compilation failed: {message}")]
    Compilation { message: String },
    #[error("component imports are forbidden; found {name:?}")]
    UnexpectedImport { name: String },
    #[error("component must export {expected:?}")]
    MissingExport { expected: &'static str },
    #[error("component has unexpected export {name:?}")]
    UnexpectedExport { name: String },
    #[error("component export {name:?} must be a component instance")]
    UnexpectedExportKind { name: &'static str },
    #[error("planner export must contain a function named {expected:?}")]
    MissingPlanExport { expected: &'static str },
    #[error("planner export has unexpected function {name:?}")]
    UnexpectedPlanExport { name: String },
    #[error("component resource requirements cannot be determined")]
    UnpredictableResources,
    #[error("component requires {actual} {resource}, limit is {limit}")]
    ResourceLimit {
        resource: &'static str,
        actual: u64,
        limit: u64,
    },
    #[error("component does not implement the zup plugin contract: {message}")]
    Signature { message: String },
    #[error("trusted AOT component could not be loaded: {message}")]
    TrustedAot { message: String },
    #[error("AOT component is {size} bytes; the limit is {limit} bytes")]
    AotTooLarge { size: usize, limit: usize },
}

impl ValidatedComponent {
    pub fn fingerprint(&self) -> crate::EngineFingerprint {
        self.fingerprint
    }

    pub fn plan(
        &self,
        context: &Context,
    ) -> Result<Result<InstallationPlan, PluginError>, InvocationError> {
        self.plan_with_cancellation(context, &|| false)
    }

    pub fn plan_with_cancellation(
        &self,
        context: &Context,
        cancellation: &(dyn Fn() -> bool + Send + Sync),
    ) -> Result<Result<InstallationPlan, PluginError>, InvocationError> {
        let mut store = sandbox_store(self.pre.engine()).map_err(InvocationError::setup)?;
        let pre = &self.pre;
        let call_error: Arc<Mutex<Option<bool>>> = Arc::new(Mutex::new(None));
        let captured_error = Arc::clone(&call_error);
        let result = run_with_watchdog(self.pre.engine(), cancellation, || {
            let plugin = pre.instantiate(&mut store)?;
            store.set_hostcall_fuel(crate::config::MAX_PLAN_OUTPUT_BYTES);
            plugin
                .zup_plugin_planner()
                .call_plan(&mut store, context)
                .inspect_err(|error| {
                    if let Ok(mut slot) = captured_error.lock() {
                        *slot = Some(error.root_cause().is::<Trap>());
                    }
                })
        });
        let result = match result {
            Ok(result) => result,
            Err(error) => {
                if store.data().memory_growth_rejected() || store.data().table_growth_rejected() {
                    return Err(InvocationError::MemoryLimit);
                }
                if matches!(error, InvocationError::Trap { .. })
                    && call_error
                        .lock()
                        .ok()
                        .and_then(|slot| *slot)
                        .is_some_and(|is_trap| !is_trap)
                {
                    return Err(InvocationError::OutputLimit {
                        actual: MAX_PLAN_OUTPUT_BYTES as u64,
                        limit: MAX_PLAN_OUTPUT_BYTES as u64,
                    });
                }
                return Err(error);
            }
        };

        match result {
            Ok(plan) => {
                validate_plan_output(&plan)?;
                Ok(Ok(plan))
            }
            Err(error) => {
                validate_error_output(&error)?;
                Ok(Err(error))
            }
        }
    }
}

impl PluginEngine {
    #[cfg(feature = "compiler")]
    pub fn precompile_component(&self, component_wasm: &[u8]) -> Result<Vec<u8>, ContractError> {
        if is_core_module(component_wasm) {
            return Err(ContractError::CoreModule);
        }
        if self.target != crate::HOST_TARGET {
            PluginEngine::host()
                .map_err(|error| ContractError::Compilation {
                    message: sanitize_error(error.to_string()),
                })?
                .precompile_component(component_wasm)?;
        }
        let component_aot = self
            .engine
            .precompile_component(component_wasm)
            .map_err(|error| ContractError::Compilation {
                message: sanitize_error(error.to_string()),
            })?;
        if self.target == crate::HOST_TARGET {
            self.validate_trusted_aot_inner(&component_aot)?;
        } else {
            self.verify_precompiled(&component_aot)?;
        }
        Ok(component_aot)
    }

    pub fn verify_precompiled(&self, component_aot: &[u8]) -> Result<(), ContractError> {
        if component_aot.len() > MAX_AOT_BYTES {
            return Err(ContractError::AotTooLarge {
                size: component_aot.len(),
                limit: MAX_AOT_BYTES,
            });
        }
        if wasmtime::Engine::detect_precompiled(component_aot) != Some(Precompiled::Component) {
            return Err(ContractError::TrustedAot {
                message: "precompiled output is not a Wasmtime component".to_owned(),
            });
        }
        Ok(())
    }

    #[cfg(feature = "compiler")]
    pub fn compile_component(
        &self,
        component_wasm: &[u8],
    ) -> Result<ValidatedComponent, ContractError> {
        let component_aot = self.precompile_component(component_wasm)?;
        if self.target == crate::HOST_TARGET {
            self.validate_trusted_aot_inner(&component_aot)
        } else {
            PluginEngine::host()
                .map_err(|error| ContractError::Compilation {
                    message: sanitize_error(error.to_string()),
                })?
                .compile_component(component_wasm)
        }
    }

    /// # Safety
    /// `component_aot` is bytes the verified package authenticated.
    pub unsafe fn validate_trusted_aot(
        &self,
        component_aot: &[u8],
    ) -> Result<ValidatedComponent, ContractError> {
        self.validate_trusted_aot_inner(component_aot)
    }

    fn validate_trusted_aot_inner(
        &self,
        component_aot: &[u8],
    ) -> Result<ValidatedComponent, ContractError> {
        self.verify_precompiled(component_aot)?;
        let component =
            unsafe { Component::deserialize(&self.engine, component_aot) }.map_err(|error| {
                ContractError::TrustedAot {
                    message: sanitize_error(error.to_string()),
                }
            })?;
        self.validate(component)
    }

    fn validate(&self, component: Component) -> Result<ValidatedComponent, ContractError> {
        {
            let component_type = component.component_type();
            if let Some((name, _)) = component_type.imports(&self.engine).next() {
                return Err(ContractError::UnexpectedImport {
                    name: sanitize_error(name.to_owned()),
                });
            }

            let mut exports = component_type.exports(&self.engine);
            let Some((name, planner)) = exports.next() else {
                return Err(ContractError::MissingExport {
                    expected: PLANNER_EXPORT_NAME,
                });
            };
            if name != PLANNER_EXPORT_NAME {
                return Err(ContractError::UnexpectedExport {
                    name: sanitize_error(name.to_owned()),
                });
            }
            if let Some((name, _)) = exports.next() {
                return Err(ContractError::UnexpectedExport {
                    name: sanitize_error(name.to_owned()),
                });
            }

            let ComponentItem::ComponentInstance(planner) = planner.ty else {
                return Err(ContractError::UnexpectedExportKind {
                    name: PLANNER_EXPORT_NAME,
                });
            };
            let mut found_plan = false;
            for (name, export) in planner.exports(&self.engine) {
                match export.ty {
                    ComponentItem::Type(_) => {}
                    ComponentItem::ComponentFunc(_)
                        if !found_plan && name == PLAN_FUNCTION_NAME =>
                    {
                        found_plan = true;
                    }
                    _ => {
                        return Err(ContractError::UnexpectedPlanExport {
                            name: sanitize_error(name.to_owned()),
                        });
                    }
                }
            }
            if !found_plan {
                return Err(ContractError::MissingPlanExport {
                    expected: PLAN_FUNCTION_NAME,
                });
            }
        }

        let resources = component
            .resources_required()
            .ok_or(ContractError::UnpredictableResources)?;
        check_resource_limit(
            "memories",
            u64::from(resources.num_memories),
            MAX_MEMORY_COUNT as u64,
        )?;
        if let Some(pages) = resources.max_initial_memory_size {
            check_resource_limit("memory pages", pages, MAX_MEMORY_PAGES)?;
        }
        check_resource_limit(
            "tables",
            u64::from(resources.num_tables),
            MAX_TABLE_COUNT as u64,
        )?;
        if let Some(elements) = resources.max_initial_table_size {
            check_resource_limit("table elements", elements, MAX_TABLE_ELEMENTS)?;
        }

        let linker = Linker::<SandboxLimits>::new(&self.engine);
        let instance_pre =
            linker
                .instantiate_pre(&component)
                .map_err(|error| ContractError::Signature {
                    message: sanitize_error(error.to_string()),
                })?;
        let pre = PluginPre::new(instance_pre).map_err(|error| ContractError::Signature {
            message: sanitize_error(error.to_string()),
        })?;
        let mut store = sandbox_store(&self.engine).map_err(|error| ContractError::Signature {
            message: sanitize_error(error.to_string()),
        })?;
        run_with_watchdog(&self.engine, &|| false, || {
            pre.instantiate(&mut store).map(|_| ())
        })
        .map_err(|error| ContractError::Signature {
            message: sanitize_error(error.to_string()),
        })?;

        Ok(ValidatedComponent {
            pre,
            fingerprint: self.fingerprint(),
        })
    }
}

#[cfg(feature = "compiler")]
fn is_core_module(bytes: &[u8]) -> bool {
    bytes.starts_with(b"\0asm\x01\0\0\0")
}

fn check_resource_limit(
    resource: &'static str,
    actual: u64,
    limit: u64,
) -> Result<(), ContractError> {
    if actual > limit {
        return Err(ContractError::ResourceLimit {
            resource,
            actual,
            limit,
        });
    }
    Ok(())
}

fn validate_plan_output(plan: &InstallationPlan) -> Result<(), InvocationError> {
    if plan.resources.len() > MAX_PLAN_RESOURCES {
        return Err(InvocationError::ResourceLimit {
            resource: "plan resources",
            actual: plan.resources.len() as u64,
            limit: MAX_PLAN_RESOURCES as u64,
        });
    }
    let output_size = plan_output_size(plan);
    if output_size > MAX_PLAN_OUTPUT_BYTES {
        return Err(InvocationError::OutputLimit {
            actual: u64::try_from(output_size).unwrap_or(u64::MAX),
            limit: MAX_PLAN_OUTPUT_BYTES as u64,
        });
    }
    Ok(())
}

fn validate_error_output(error: &PluginError) -> Result<(), InvocationError> {
    let size = string_size(&error.code).saturating_add(string_size(&error.message));
    if size > MAX_PLAN_OUTPUT_BYTES {
        return Err(InvocationError::OutputLimit {
            actual: u64::try_from(size).unwrap_or(u64::MAX),
            limit: MAX_PLAN_OUTPUT_BYTES as u64,
        });
    }
    Ok(())
}

fn plan_output_size(plan: &InstallationPlan) -> usize {
    let mut size = list_size(plan.resources.len());
    for resource in &plan.resources {
        size = size.saturating_add(1);
        size = match resource {
            ResourceItem::GeneratedFile(file) => size
                .saturating_add(string_size(&file.destination))
                .saturating_add(list_size(file.contents.len()))
                .saturating_add(file.contents.len()),
            ResourceItem::Launcher(launcher) => size
                .saturating_add(1)
                .saturating_add(string_size(&launcher.name))
                .saturating_add(string_size(&launcher.target))
                .saturating_add(string_list_size(&launcher.arguments))
                .saturating_add(option_string_size(&launcher.working_directory)),
            ResourceItem::PathEntry(entry) => size.saturating_add(string_size(&entry.value)),
            ResourceItem::Service(service) => size
                .saturating_add(string_size(&service.id))
                .saturating_add(string_size(&service.name))
                .saturating_add(option_string_size(&service.display_name))
                .saturating_add(string_size(&service.binary))
                .saturating_add(string_list_size(&service.arguments))
                .saturating_add(1),
            ResourceItem::Protocol(protocol) => size
                .saturating_add(string_size(&protocol.scheme))
                .saturating_add(string_size(&protocol.executable))
                .saturating_add(string_list_size(&protocol.args)),
            ResourceItem::FileAssociation(file_association) => size
                .saturating_add(string_size(&file_association.extension))
                .saturating_add(string_size(&file_association.id))
                .saturating_add(option_string_size(&file_association.description))
                .saturating_add(string_size(&file_association.executable)),
        };
    }
    size
}

fn string_size(value: &str) -> usize {
    value.len().saturating_add(4)
}

fn list_size(length: usize) -> usize {
    length.saturating_add(4)
}

fn string_list_size(values: &[String]) -> usize {
    values.iter().fold(list_size(values.len()), |size, value| {
        size.saturating_add(string_size(value))
    })
}

fn option_string_size(value: &Option<String>) -> usize {
    value
        .as_ref()
        .map_or(1, |value| 1usize.saturating_add(string_size(value)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    use crate::{GeneratedFile, PluginError, ResourceItem};

    #[rstest]
    #[case::a_generated_file_larger_than_the_limit(
        Over::Plan(InstallationPlan {
            resources: vec![ResourceItem::GeneratedFile(GeneratedFile {
                destination: "file".to_owned(),
                contents: vec![0; MAX_PLAN_OUTPUT_BYTES + 1],
            })],
        }),
        true
    )]
    #[case::a_plugin_error_code_larger_than_the_limit(
        Over::Error(PluginError {
            code: "x".repeat(MAX_PLAN_OUTPUT_BYTES),
            message: "y".to_owned(),
        }),
        false
    )]
    fn output_past_the_limit_is_refused(#[case] over: Over, #[case] is_a_plan: bool) {
        let result = match over {
            Over::Plan(plan) => validate_plan_output(&plan),
            Over::Error(error) => validate_error_output(&error),
        };
        assert!(
            matches!(result, Err(InvocationError::OutputLimit { .. })),
            "a {} is refused",
            if is_a_plan { "plan" } else { "error" }
        );
    }

    enum Over {
        Plan(InstallationPlan),
        Error(PluginError),
    }
}
