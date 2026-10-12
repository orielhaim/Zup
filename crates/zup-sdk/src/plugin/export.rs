#[macro_export]
macro_rules! export {
    ($plugin:ident) => {
        impl $crate::plugin::planner::Guest for $plugin {
            fn plan(
                context: $crate::plugin::planner::Context,
            ) -> ::core::result::Result<
                $crate::plugin::planner::InstallationPlan,
                $crate::plugin::planner::PluginError,
            > {
                $crate::plugin::__answer::<$plugin>()(context)
            }
        }

        $crate::plugin::bindings_export!($plugin with_types_in $crate::plugin);
    };
}
