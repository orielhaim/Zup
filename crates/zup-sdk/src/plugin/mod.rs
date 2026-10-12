wit_bindgen::generate!({
    path: env!("ZUP_PLUGIN_SDK_WIT_DIR"),
    world: "plugin",
    export_macro_name: "bindings_export",
    pub_export_macro: true,
});

mod api;
mod export;
mod plan;

pub mod prelude;

pub use api::{Context, Plugin, Scope};
pub use plan::{Error, FileAssociation, Launcher, Path, Plan, Protocol, Service};

pub use api::__answer;

pub use crate::export;

#[doc(hidden)]
pub mod planner {
    pub use super::exports::zup::plugin::planner::{Context, Guest, InstallationPlan, PluginError};
}
