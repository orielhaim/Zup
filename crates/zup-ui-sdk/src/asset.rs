//! Application-provided assets, and how a preset gets at them.
//!
//! There are two kinds of asset and they are not the same thing:
//!
//! - **Preset-owned** assets belong to the preset's own code: icons, fonts,
//!   illustrations. A preset returns an asset source for them from
//!   [`Preset::assets`](crate::Preset::assets); they are part of the compiled
//!   preset.
//! - **Application-provided** assets are data an application configured when it
//!   chose this preset. A logo, a hero image, a font. They are not compiled
//!   into the preset, which is the whole reason one compiled preset serves
//!   every application that selects it.
//!
//! An application names one with an [`AssetRef`] in `[ui.settings]`; the build
//! resolves the path, verifies it, hashes it, and stores it; the host
//! materializes it and tells this preset where the file is; the SDK serves it
//! through GPUI's own [`AssetSource`], so `img()` and the SVG renderer work
//! exactly as they do for a preset's own assets.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use gpui_kit::{AssetSource, Result as GpuiResult, SharedString};

/// An application asset the host named that is not readable where it said it
/// was.
///
/// Its own type rather than a formatted string because a preset author reading
/// a GPUI log gets the asset name and the OS's reason, which are the two things
/// they can act on.
#[derive(Debug, thiserror::Error)]
#[error("could not read application asset `{name}`: {source}")]
struct MissingAsset {
    name: String,
    #[source]
    source: std::io::Error,
}

/// An application-provided asset, named in `[ui.settings]`.
///
/// A plain string on the wire and in `zup.toml`, because that is what an
/// application author writes:
///
/// ```toml
/// [ui.settings]
/// logo = "branding/logo.svg"
/// ```
///
/// and a type here rather than a bare `String` so a preset's settings struct
/// says what the value means. The build reads the schema this type generates to
/// find which settings are assets, so a preset author declares the type once
/// and gets asset resolution, validation, and hashing without a line of
/// scaffolding.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct AssetRef(String);

impl AssetRef {
    /// Name an asset.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The name an application configured.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for AssetRef {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl schemars::JsonSchema for AssetRef {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "AssetRef".into()
    }

    fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "title": "Asset",
            "description": "A file this application provides, named relative to the project. \
                            Resolved, verified, and materialized by the zup build; the preset \
                            reads the verified file at runtime.",
            "x-zup-asset": true,
        })
    }
}

/// The files the host materialized for this session, by asset name.
///
/// The preset never learns a project path. It gets the files, verified, at
/// paths the host chose.
#[derive(Debug, Clone, Default)]
pub struct ApplicationAssets {
    files: Arc<RwLock<BTreeMap<String, PathBuf>>>,
}

impl ApplicationAssets {
    /// The files the host materialized.
    pub fn from_configuration(configuration: &zup_ui_protocol::UiConfiguration) -> Self {
        Self {
            files: Arc::new(RwLock::new(
                configuration
                    .assets
                    .iter()
                    .map(|(name, path)| (name.clone(), PathBuf::from(path)))
                    .collect(),
            )),
        }
    }

    /// Where one asset lives, if the application configured it.
    pub fn path(&self, asset: &AssetRef) -> Option<PathBuf> {
        self.files
            .read()
            .expect("asset table")
            .get(asset.as_str())
            .cloned()
    }

    /// Every asset this application provided, by name.
    pub fn names(&self) -> Vec<String> {
        self.files
            .read()
            .expect("asset table")
            .keys()
            .cloned()
            .collect()
    }

    /// Replace the table, for a session whose assets were re-resolved.
    pub fn replace(&self, assets: BTreeMap<String, String>) {
        *self.files.write().expect("asset table") = assets
            .into_iter()
            .map(|(name, path)| (name, PathBuf::from(path)))
            .collect();
    }
}

/// The asset source a preset's window reads: the application's files, then the
/// preset's own, then the component library's icons.
///
/// The application's files are read through the same interface GPUI already
/// has, so `img("logo.svg")` and the SVG renderer are unchanged - there is no
/// second image-loading stack, and a preset does not learn which kind of asset
/// a name refers to.
pub struct PresetAssets {
    /// Assets the application configured.
    application: ApplicationAssets,
    /// Assets compiled into the preset.
    own: Box<dyn AssetSource>,
    /// The icons `gpui-kit`'s components draw themselves with.
    components: gpui_kit::assets::Assets,
}

impl PresetAssets {
    pub fn new(application: ApplicationAssets, own: impl AssetSource) -> Self {
        Self {
            application,
            own: Box::new(own),
            components: gpui_kit::assets::Assets::new(""),
        }
    }
}

impl AssetSource for PresetAssets {
    fn load(&self, path: &str) -> GpuiResult<Option<Cow<'static, [u8]>>> {
        if let Some(file) = self.application.path(&AssetRef::new(path)) {
            return match std::fs::read(&file) {
                Ok(bytes) => Ok(Some(Cow::Owned(bytes))),
                Err(error) => Err(MissingAsset {
                    name: path.to_owned(),
                    source: error,
                }
                .into()),
            };
        }
        if let Ok(Some(bytes)) = self.own.load(path) {
            return Ok(Some(bytes));
        }
        self.components.load(path)
    }

    fn list(&self, path: &str) -> GpuiResult<Vec<SharedString>> {
        let mut listed: Vec<SharedString> = self
            .application
            .names()
            .into_iter()
            .filter(|name| name.starts_with(path))
            .map(SharedString::from)
            .collect();
        listed.extend(self.own.list(path).unwrap_or_default());
        listed.extend(self.components.list(path)?);
        listed.sort();
        listed.dedup();
        Ok(listed)
    }
}
