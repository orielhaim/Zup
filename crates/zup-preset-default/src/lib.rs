pub mod model;
pub mod present;
pub mod theme;
pub mod ui;
pub mod window;

#[cfg(test)]
mod tests;

use zup_sdk::preset::gpui_kit::AssetSource;
use zup_sdk::preset::prelude::*;
use zup_sdk::preset::{AssetRef, PresetContext};

#[derive(Debug, Clone, Default, serde::Deserialize, schemars::JsonSchema)]
pub struct Settings {
    pub logo: Option<AssetRef>,
    pub accent: Option<Accent>,
    #[serde(default)]
    pub appearance: Appearance,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(transparent)]
pub struct Accent(String);

impl Accent {
    pub fn new(hex: impl Into<String>) -> Self {
        Self(hex.into())
    }

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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Appearance {
    #[default]
    System,
    Light,
    Dark,
}

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

zup_sdk::preset::gpui_kit::assets::icon_assets!(
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
