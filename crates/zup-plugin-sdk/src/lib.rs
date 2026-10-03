//! Author a Zup plugin.
//!
//! A plugin is a WebAssembly component that answers one question: given the
//! application being installed and what a person selected, what should exist on
//! the machine afterwards. It does not install anything. It returns a
//! declaration, Zup decides whether that declaration is safe, and Zup performs
//! the install.
//!
//! ```no_run
//! use zup_plugin_sdk::prelude::*;
//!
//! struct Configure;
//!
//! impl Plugin for Configure {
//!     fn plan(context: Context) -> Result<Plan, Error> {
//!         Ok(Plan::new().generated_file(GeneratedFile::text(
//!             "${install}/configure.txt",
//!             format!("installing {} for {}", context.app_name, context.plugin_id),
//!         ))
//!     }
//! }
//!
//! zup_plugin_sdk::export!(Configure);
//! ```
//!
//! # What a plugin can do
//!
//! Return resources. A plugin has no way to run a command, write a registry
//! key, elevate, or reach the network: the world it implements imports nothing
//! at all, which the host verifies before it will load the component. Everything
//! a plugin declares is checked against the application's own manifest before it
//! is applied, so a plan that contradicts what the application declares is
//! refused rather than obeyed.
//!
//! # What a plugin needs to know
//!
//! Only what the host tells it in [`Context`]. There is no ambient state to read
//! and nothing to look up: a plugin is handed the answer to the question it is
//! being asked.
//!
//! # The build
//!
//! A plugin compiles to `wasm32-unknown-unknown` as a `cdylib`, and
//! `zup plugin build` turns that into a component. The WIT is not vendored into
//! your project and `wit-bindgen` is not a dependency you declare; this crate
//! owns both.

#![deny(unsafe_code)]

// Generate the guest bindings for the `plugin` world, from the WIT the ABI
// crate owns.
//
// `pub_export_macro` is deliberately off. On, the generator publishes a macro
// called `export` that takes a bare type name and looks for the bindings beside
// itself - correct only when a plugin and its bindings are one crate, which is
// not the shape an author has, and it collides with the macro below. With it
// off, the generator still emits `__export_plugin_impl`, which takes the module
// to look in and is what `plugin_export!` calls.
// Generate the guest bindings for the `plugin` world, from the WIT the ABI
// crate owns.
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
///
/// Named here so `plugin_export!` has one place to point at; not part of the
/// authoring surface.
pub use plugin::__answer;

/// The types the WIT defines, under the names the WIT gives them.
///
/// A plugin author reaches these through [`prelude`], which presents them as
/// ordinary Rust. They are public because `export!` expands to code that names
/// them, and because the components of a plan are these values.
pub mod planner {
    pub use crate::exports::zup::plugin::planner::{
        Context, FileAssociation, GeneratedFile, Guest, InstallScope, InstallationPlan, Launcher,
        LauncherLocation, PathEntry, PluginError, Protocol, ResourceItem, Service, ServiceStart,
    };
}