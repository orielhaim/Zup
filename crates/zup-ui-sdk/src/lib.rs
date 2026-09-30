//! Author a native GPUI installer preset for zup.
//!
//! A preset is a normal Rust program that happens to talk to a zup installer.
//! That is the whole idea, and it is why this crate is small: it hands a preset
//! a session, its settings, and its assets, and then gets out of the way.
//!
//! ```no_run
//! use zup_ui_sdk::prelude::*;
//! use gpui::ParentElement;
//!
//! #[derive(Default, serde::Deserialize, schemars::JsonSchema)]
//! struct Settings {
//!     hero: Option<String>,
//!     accent: Option<String>,
//! }
//!
//! struct Installer {
//!     state: Entity<SessionState>,
//!     _changes: Subscription,
//! }
//!
//! impl gpui::Render for Installer {
//!     fn render(
//!         &mut self,
//!         _: &mut gpui::Window,
//!         _: &mut gpui::Context<Self>,
//!     ) -> impl gpui::IntoElement {
//!         gpui::div().child("Installer")
//!     }
//! }
//!
//! struct Aurora;
//!
//! impl Preset for Aurora {
//!     const NAME: &'static str = env!("CARGO_PKG_NAME");
//!     const VERSION: &'static str = env!("CARGO_PKG_VERSION");
//!     type Settings = Settings;
//!
//!     fn launch(context: PresetContext<Self::Settings>, cx: &mut App) {
//!         // Ordinary GPUI, with the installer's state already published.
//!         let session = context.session().clone();
//!         let state = session.state();
//!         let changes = cx.observe(&state, |_, _| {});
//!         gpui::open_window(gpui::WindowOptions::default(), cx, move |_, cx| {
//!             cx.new(|_| Installer { state, _changes: changes })
//!         })
//!         .expect("open the installer window");
//!     }
//! }
//!
//! fn main() {
//!     zup_ui_sdk::run::<Aurora>();
//! }
//! ```
//!
//! # What this crate is not
//!
//! It is not a UI framework. There is no layout abstraction, no screen
//! abstraction, no widget vocabulary, and no preset markup language, because a
//! preset author has [`gpui_kit::component`], GPUI itself, and ordinary Rust,
//! and any of those could express a layout the other could not. Everything a
//! preset is given is Zup-facing: the state the installer is in, the
//! configuration the application chose, and the files it provided.
//!
//! # What a preset can reach
//!
//! Two things: [`UiSession::send`], which asks the host to do something, and
//! whatever the host publishes. A preset has no access to a plan, a
//! transaction, an elevation decision, or a package, and asking for one is not
//! an omission - it is the boundary that makes a third-party preset safe to
//! ship. The host validates every action against the state it owns, so a
//! preset that asks for something this installation cannot do is refused rather
//! than obeyed.
//!
//! # GPUI versions
//!
//! A preset project depends on this crate alone. [`gpui_kit`] is re-exported,
//! so the GPUI stack a preset builds against is the one this SDK was built
//! against and there is nothing to keep in step.
#![deny(unsafe_code)]

mod asset;
mod preset;
mod session;
mod settings;
mod transport;

pub mod prelude;

pub use asset::{ApplicationAssets, AssetRef, PresetAssets};
pub use preset::{
    Describe, NoSettings, Preset, PresetContext, PresetError, Settings, describe, run, serve,
};
pub use session::{ActionSender, SessionState, UiSession};
pub use settings::PresetSettings;
pub use transport::{Bootstrap, Channel, Identity, Opened, Requester, TransportError};

/// The GPUI stack a preset is written against.
///
/// Re-exported rather than depended on directly: a preset project lists
/// `zup-ui-sdk` and nothing else, so upgrading the GPUI stack is one version
/// bump rather than a sweep through four crates.
pub use gpui_kit;

/// The versioned contract, for a preset that names protocol types directly.
///
/// A preset normally reaches these through [`prelude`]; this is here for the
/// rare case where a preset's own API takes a protocol type as a parameter.
pub use zup_ui_protocol;
