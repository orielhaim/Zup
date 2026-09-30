//! The default zup installer interface.
//!
//! This preset is written against `zup-ui-sdk` and nothing else, exactly as an
//! application author's preset would be. That is the point of it: if the first
//! party interface needed a private crate, an installer author, or a
//! privileged reach into the machine, the public boundary would be a fiction.
//! Every fact this window shows arrived in a `UiSnapshot`, and everything it
//! asks for is a `UiAction` the host may refuse.
//!
//! Nothing here is a second source of truth. The view holds no copy of the
//! selection, the scope, or the progress: it draws the snapshot it was given and
//! asks for a change, and the next snapshot is the answer. The one thing it owns
//! is the text in the install-location box, because a text box is a widget
//! rather than a fact about the machine.

mod view;

#[cfg(test)]
mod tests;

use zup_ui_sdk::PresetContext;
use zup_ui_sdk::prelude::*;

/// What an application can change about how this preset looks.
///
/// Every field is optional, because a preset has to work for an application that
/// configures nothing at all - the host sends an empty document and the window
/// has to open.
#[derive(Debug, Clone, Default, serde::Deserialize, schemars::JsonSchema)]
pub struct Settings {
    /// A line above the product name. Purely presentational, so the host has no
    /// opinion about it.
    pub hero: Option<String>,
}

/// The default preset.
pub struct DefaultPreset;

impl Preset for DefaultPreset {
    const NAME: &'static str = env!("CARGO_PKG_NAME");
    const VERSION: &'static str = env!("CARGO_PKG_VERSION");

    type Settings = Settings;

    fn launch(context: PresetContext<Self::Settings>, cx: &mut App) {
        view::open(context, cx);
    }
}
