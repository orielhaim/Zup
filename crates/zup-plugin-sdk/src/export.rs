//! Export a plugin as the component Zup loads.
//!
//! This module exists only to give the export a name an author can call. The
//! work is done by `wit_bindgen`, which emits a `macro_rules!` at the crate
//! root; a `macro_rules!` is only callable by name from the module that defines
//! it, so the one line that calls it lives here rather than in the same file as
//! the SDK's other exports.

/// Export the plugin named by the argument as a Zup component.
///
/// Writes the `Guest` implementation the generated bindings expect, converts the
/// plan the plugin returns into what the ABI carries, and emits the export
/// symbols that make the crate a component rather than a library.
///
/// ```no_run
/// # use zup_plugin_sdk::prelude::*;
/// # struct Configure;
/// # impl Plugin for Configure {
/// #     fn plan(_: Context) -> Result<Plan, Error> { Ok(Plan::new()) }
/// # }
/// zup_plugin_sdk::export!(Configure);
/// ```
///
/// Named `export` rather than `plugin_export` because the code generator takes
/// the name the obvious candidate would want; see the crate root.
#[macro_export]
macro_rules! export {
    // Bound as an identifier, not a type: the generator's export takes an
    // identifier, and a plugin is always named by one. A `$ty:ty` here would
    // forward to a rule that cannot match it.
    ($plugin:ident) => {
        impl $crate::planner::Guest for $plugin {
            fn plan(
                context: $crate::planner::Context,
            ) -> ::core::result::Result<
                $crate::planner::InstallationPlan,
                $crate::planner::PluginError,
            > {
                $crate::__answer::<$plugin>()(context)
            }
        }

        $crate::bindings_export!($plugin with_types_in $crate);
    };
}