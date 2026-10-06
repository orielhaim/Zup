//! Author a Zup plugin.
//!
//! A plugin is a WebAssembly component that answers one question: given the
//! application being installed and what a person selected, what should exist on
//! the machine afterwards. It does not install anything. It returns a
//! declaration, Zup decides whether that declaration is safe, and Zup performs
//! the install.
//!
//! The implementation layer beneath `zup_sdk::plugin`.
//!
//! A plugin has no way to run a command, write a registry key, elevate, or reach
//! the network: the world it implements imports nothing at all, which the host
//! verifies before it will load the component. Everything it declares is checked
//! against the application's own manifest before it is applied, so a plan that
//! contradicts what the application declares is refused rather than obeyed.
//!
//! A plugin learns only what the host tells it in [`Context`]. There is no
//! ambient state to read and nothing to look up: it is handed the answer to the
//! question it is being asked.
//!
//! # The build
//!
//! A plugin compiles to `wasm32-unknown-unknown` as a `cdylib`, and
//! `zup plugin build` turns that into a component. The WIT is not vendored into
//! your project, `wit-bindgen` is not a dependency you declare, and no
//! Component Model tool is something you have to install: this crate owns the
//! contract and Zup owns the rest.

#![deny(unsafe_code)]

// Generate the guest bindings for the `plugin` world, from the WIT the ABI crate
// owns.
//
// `export_macro_name` is renamed because the generator emits
// `use __export_plugin_impl as <that name>` at this crate's root, and a
// `macro_rules!` in the same namespace is shadowed by whichever comes later
// textually. Left at its default it would claim `export` and the macro an
// author calls - the one that writes the `Guest` impl and converts the plan -
// would be unreachable by that name.
//
// `pub_export_macro` is on because it is the only way the generator makes
// `__export_plugin_impl` reachable from another crate, and that helper is the
// only way to export a plugin whose bindings live here.
wit_bindgen::generate!({
    path: env!("ZUP_PLUGIN_SDK_WIT_DIR"),
    world: "plugin",
    export_macro_name: "bindings_export",
    pub_export_macro: true,
});

mod export;
mod plan;
mod plugin;

pub mod prelude;

pub use plan::{Error, FileAssociation, Launcher, Path, Plan, Protocol, Service};
pub use plugin::{Context, Plugin, Scope};

/// The conversion between a plugin's own types and the ABI's.
pub use plugin::__answer;

/// The bindings [`export!`] expands to.
///
/// Hidden because a plugin author names [`Plugin`] and the plan constructors,
/// not the generated shapes the macro writes on their behalf.
#[doc(hidden)]
pub mod planner {
    pub use crate::exports::zup::plugin::planner::{Context, Guest, InstallationPlan, PluginError};
}
