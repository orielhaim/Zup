mod executor;
mod loader;

pub use executor::WasmtimePluginExecutor;
pub use loader::LoadError;

pub struct NativePluginExecutor<R> {
    pub executor: WasmtimePluginExecutor,
    pub locations: R,
}

impl<R: zup_plan::InstallLocationResolver> zup_plan::PluginExecutor for NativePluginExecutor<R> {
    fn target(&self) -> &zup_core::Target {
        self.executor.target()
    }

    fn plan(
        &mut self,
        binding: &zup_core::PluginBinding,
        context: &zup_plan::PluginPlanningContext,
        cancellation: &dyn zup_plan::CancellationQuery,
    ) -> Result<zup_plan::PluginResourceProposal, zup_plan::PluginFailure> {
        let root = zup_plan::resolve_template_path(
            &context.install_directory,
            &context.target,
            &self.locations,
            context.scope,
        )
        .map_err(|error| zup_plan::PluginFailure::internal(error.to_string()))?;
        let mut context = context.clone();
        context.install_directory = zup_core::Template::parse(root.as_str())
            .map_err(|error| zup_plan::PluginFailure::internal(error.to_string()))?;
        self.executor.plan(binding, &context, cancellation)
    }
}
