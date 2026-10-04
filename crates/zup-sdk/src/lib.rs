//! Author a Zup installer preset or an installation plugin.
//!
//! Zup has two kinds of extension, and this crate is the whole of what a Rust
//! author needs for either of them:
//!
//! ```toml
//! [dependencies]
//! zup-sdk = { version = "0.1.0", features = ["preset"] }
//! ```
//!
//! ```toml
//! [dependencies]
//! zup-sdk = { version = "0.1.0", features = ["plugin"] }
//! ```
//!
//! # A preset
//!
//! The window an installer presents. A preset is an ordinary GPUI application
//! that draws what the host publishes and asks for what a person clicked;
//! everything Zup-specific it is given is the session, its settings, its assets,
//! and the actions it may request.
//!
//! ```no_run
//! use zup_sdk::preset::prelude::*;
//!
//! #[zup_sdk::preset::settings]
//! struct Settings {
//!     hero: Option<String>,
//! }
//!
//! struct MyPreset;
//!
//! impl Preset for MyPreset {
//!     const NAME: &'static str = env!("CARGO_PKG_NAME");
//!     const VERSION: &'static str = env!("CARGO_PKG_VERSION");
//!     type Settings = Settings;
//!
//!     fn launch(context: PresetContext<Self::Settings>, cx: &mut App) {
//!         // Ordinary GPUI, with the installer's state already published.
//!     }
//! }
//!
//! fn main() {
//!     zup_sdk::preset::run::<MyPreset>();
//! }
//! ```
//!
//! # A plugin
//!
//! A declarative extension to what an application installs. A plugin is
//! compiled to WebAssembly, answers one question, and returns resources for Zup
//! to install; it never installs anything itself.
//!
//! ```no_run
//! use zup_sdk::plugin::prelude::*;
//!
//! struct MyPlugin;
//!
//! impl Plugin for MyPlugin {
//!     fn plan(context: Context) -> Result<Plan, Error> {
//!         Ok(Plan::new().generated_file(GeneratedFile::text(
//!             "${install}/notes.txt",
//!             format!("installed by {}", context.plugin_id),
//!         )))
//!     }
//! }
//!
//! zup_sdk::plugin::export!(MyPlugin);
//! ```
//!
//! # What this crate is not
//!
//! It is not a framework, and neither half is. A preset draws with GPUI and a
//! plugin returns a declaration; neither is given a layout abstraction, a
//! widget vocabulary, or a scripting model, because each of those would be
//! something to learn before you could do the thing you came to do.
//!
//! The rules that protect a user's machine are not here either. A preset cannot
//! reach a plan, a transaction, or an elevation decision, and a plugin cannot
//! run a command or write a registry key. Those boundaries live in the host,
//! which is the only side that enforces them.

/// Derive and re-export the crates a preset's settings are built from.
///
/// Hidden because it is not an authoring surface: `#[zup_sdk::preset::settings]`
/// applies these, and a preset should not be naming them itself. They are
/// forwarded from the preset SDK rather than declared here, so there is exactly
/// one version of each in the graph and a preset's settings schema is always the
/// one that SDK generates.
#[cfg(feature = "preset")]
#[doc(hidden)]
pub mod __private {
    pub use zup_preset_sdk::__private::{schemars, serde, serde_json};
}

/// A plugin: a declarative extension to what an application installs.
///
/// Compiles to `wasm32-unknown-unknown`. `zup plugin build` turns that into the
/// component Zup loads.
#[cfg(feature = "plugin")]
pub mod plugin {
    // Listed rather than globbed. The bindings generator inside
    // `zup-plugin-sdk` publishes macros of its own alongside the authoring
    // ones, and a glob would put a second `export` in this namespace - one that
    // takes a bare type name and looks for bindings beside itself. Naming what
    // is public is what makes `export!` here mean the macro an author expects.
    pub use zup_plugin_sdk::{
        Context, Error, FileAssociation, Launcher, Path, Plan, Plugin, Protocol, Scope, Service,
        export,
    };
    pub mod prelude {
        pub use zup_plugin_sdk::prelude::*;
    }
}

/// The window an installer presents.
///
/// Listed rather than globbed, so that publishing something new in the crate
/// beneath is a decision to publish it here rather than an accident of what
/// that crate happens to export.
#[cfg(feature = "preset")]
pub mod preset {
    pub use zup_preset_sdk::{
        ActionSender, ApplicationAssets, AssetRef, NoSettings, Preset, PresetContext, PresetError,
        PresetSettings, Session, SessionState, run, settings,
    };
    pub use zup_preset_sdk::gpui_kit;
    pub mod prelude {
        pub use zup_preset_sdk::prelude::*;
    }
    pub mod presentation {
        pub use zup_preset_sdk::presentation::*;
    }
    /// GPUI's headless test harness, for a preset that tests its own windows.
    #[cfg(feature = "test-support")]
    pub use zup_preset_sdk::test_support;
}
