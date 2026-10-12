use std::borrow::Cow;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use gpui_kit::{AssetSource, Result as GpuiResult, SharedString};

#[derive(Debug, thiserror::Error)]
#[error("could not read application asset `{name}`: {source}")]
struct MissingAsset {
    name: String,
    #[source]
    source: std::io::Error,
}

#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct AssetRef(String);

impl AssetRef {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

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

#[derive(Debug, Clone, Default)]
pub struct ApplicationAssets {
    files: Arc<RwLock<BTreeMap<String, PathBuf>>>,
}

impl ApplicationAssets {
    pub(crate) fn from_configuration(configuration: &zup_preset_protocol::Configuration) -> Self {
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

    pub fn path(&self, asset: &AssetRef) -> Option<PathBuf> {
        self.files
            .read()
            .expect("asset table")
            .get(asset.as_str())
            .cloned()
    }

    pub fn names(&self) -> Vec<String> {
        self.files
            .read()
            .expect("asset table")
            .keys()
            .cloned()
            .collect()
    }

    pub(crate) fn replace(&self, assets: BTreeMap<String, String>) {
        *self.files.write().expect("asset table") = assets
            .into_iter()
            .map(|(name, path)| (name, PathBuf::from(path)))
            .collect();
    }
}

pub(crate) struct PresetAssets {
    application: ApplicationAssets,
    own: Box<dyn AssetSource>,
    components: gpui_kit::assets::Assets,
}

impl PresetAssets {
    pub(crate) fn new(application: ApplicationAssets, own: impl AssetSource) -> Self {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assets_are_looked_up_by_name() {
        let assets = ApplicationAssets::from_configuration(&zup_preset_protocol::Configuration {
            settings: serde_json::json!({}),
            assets: [(
                "branding/logo.svg".to_owned(),
                "C:/Temp/zup/ui/logo".to_owned(),
            )]
            .into_iter()
            .collect(),
        });
        assert_eq!(
            assets.path(&AssetRef::new("branding/logo.svg")),
            Some(PathBuf::from("C:/Temp/zup/ui/logo"))
        );
        assert!(
            assets
                .path(&AssetRef::new("branding/missing.svg"))
                .is_none()
        );
        assert_eq!(assets.names(), ["branding/logo.svg"]);
    }

    #[test]
    fn a_re_resolved_configuration_replaces_the_asset_table() {
        let assets = ApplicationAssets::default();
        assets.replace(
            [("logo".to_owned(), "C:/Temp/zup/ui/logo".to_owned())]
                .into_iter()
                .collect(),
        );
        assert!(assets.path(&AssetRef::new("logo")).is_some());
        assets.replace(Default::default());
        assert!(assets.path(&AssetRef::new("logo")).is_none());
    }
}
