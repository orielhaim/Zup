use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub const MAX_ASSET_PATH_BYTES: usize = 1024;

pub const MAX_SETTINGS_BYTES: usize = 256 * 1024;

pub const MAX_ASSETS: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Configuration {
    pub settings: serde_json::Value,
    pub assets: BTreeMap<String, String>,
}

impl Configuration {
    pub fn empty() -> Self {
        Self {
            settings: serde_json::Value::Object(serde_json::Map::new()),
            assets: BTreeMap::new(),
        }
    }

    pub fn validate(&self) -> Result<(), ConfigurationError> {
        if self.assets.len() > MAX_ASSETS {
            return Err(ConfigurationError::TooManyAssets {
                count: self.assets.len(),
                limit: MAX_ASSETS,
            });
        }
        for (name, path) in &self.assets {
            if name.is_empty() {
                return Err(ConfigurationError::EmptyAssetName);
            }
            if path.len() > MAX_ASSET_PATH_BYTES {
                return Err(ConfigurationError::AssetPathTooLong {
                    name: name.clone(),
                    limit: MAX_ASSET_PATH_BYTES,
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigurationError {
    #[error("configuration names {count} assets; the limit is {limit}")]
    TooManyAssets { count: usize, limit: usize },
    #[error("an asset name is empty")]
    EmptyAssetName,
    #[error("the path for asset `{name}` exceeds {limit} bytes")]
    AssetPathTooLong { name: String, limit: usize },
}
