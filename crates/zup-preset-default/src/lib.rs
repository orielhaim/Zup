//! The default zup installer interface.
//!
//! This preset is written against `zup-preset-sdk` and nothing else, exactly as an
//! application author's preset would be. Every fact this window shows arrived
//! in a `Snapshot`, and everything it asks for is a `Action` the host may
//! refuse.
//!
//! The window holds no copy of the selection, the scope, or the progress. The
//! state it keeps is presentation only: which disclosures are open, whether the
//! plan sheet is showing, and which overlay belongs to which state.
//!
//! Installing is a decision followed by an operation, so there are no pages to
//! step through. [`model`] reads a snapshot as the one screen it belongs to,
//! [`ui`] holds the installer-domain components those screens are built from,
//! and [`window`] arranges them.

pub mod model;
pub mod present;
pub mod theme;
pub mod ui;
pub mod window;

#[cfg(test)]
mod tests;

use zup_preset_sdk::gpui_kit::AssetSource;
use zup_preset_sdk::prelude::*;
use zup_preset_sdk::{AssetRef, PresetContext};

/// What an application can change about how this preset looks.
///
/// Semantic on purpose: an application chooses its logo and its colour, and
/// the preset decides what spacing, sizes and corners those deserve. An
/// application that needs more than this wants a preset of its own.
#[derive(Debug, Clone, Default, serde::Deserialize, schemars::JsonSchema)]
pub struct Settings {
    /// The application's logo, shown beside its name and in the title bar.
    /// SVG or PNG, square, at least 128 pixels for raster images.
    pub logo: Option<AssetRef>,
    /// The colour of the main button, selections and progress.
    pub accent: Option<Accent>,
    /// Light or dark, or whatever the person's system uses (the default).
    #[serde(default)]
    pub appearance: Appearance,
}

/// A brand colour, as `#rrggbb`.
///
/// Adjusted where needed so text on it and beside it stays readable in both
/// palettes: a brand colour is a wish, and contrast is not negotiable.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(transparent)]
pub struct Accent(String);

impl Accent {
    pub fn new(hex: impl Into<String>) -> Self {
        Self(hex.into())
    }

    /// The colour, when it is one.
    pub fn rgb(&self) -> Option<[u8; 3]> {
        let hex = self.0.strip_prefix('#')?;
        if hex.len() != 6 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        let channel = |at: usize| u8::from_str_radix(&hex[at..at + 2], 16).ok();
        Some([channel(0)?, channel(2)?, channel(4)?])
    }
}

impl schemars::JsonSchema for Accent {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Accent".into()
    }

    fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "title": "Accent colour",
            "description": "A colour as #rrggbb, used for the main button, selections and progress.",
            "pattern": "^#[0-9a-fA-F]{6}$",
        })
    }
}

/// Which palette the window draws with.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Appearance {
    /// Follow the system, and change with it.
    #[default]
    System,
    Light,
    Dark,
}

/// The default preset.
pub struct DefaultPreset;

impl Preset for DefaultPreset {
    const NAME: &'static str = env!("CARGO_PKG_NAME");
    const VERSION: &'static str = env!("CARGO_PKG_VERSION");

    type Settings = Settings;

    fn assets() -> impl AssetSource {
        Icons
    }

    fn launch(context: PresetContext<Self::Settings>, cx: &mut App) {
        window::open(context, cx);
    }
}

zup_preset_sdk::gpui_kit::assets::icon_assets!(
    pub Icons,
    [
        AppWindow,
        ArrowRight,
        Ban,
        BadgeCheck,
        Check,
        ChevronDown,
        ChevronRight,
        CircleAlert,
        CircleArrowUp,
        CircleCheck,
        CircleCheckBig,
        CircleX,
        ClipboardCopy,
        Download,
        ExternalLink,
        FileText,
        Files,
        Folder,
        FolderOpen,
        Globe,
        HardDrive,
        Info,
        Layers,
        Link,
        ListChecks,
        LoaderCircle,
        OctagonAlert,
        Package,
        PackageCheck,
        Play,
        Plug,
        Puzzle,
        RefreshCw,
        RotateCcw,
        ScrollText,
        Server,
        ShieldAlert,
        ShieldCheck,
        SquareTerminal,
        TriangleAlert,
        Trash,
        User,
        Users,
        Wrench,
        X,
    ]
);
