//! What the application configured, as the host resolved it.
//!
//! A preset's settings are the application's data, not the installer's: the
//! host validates them against the schema the package carries, then hands the
//! preset the same values to deserialize into its own types. Sending the JSON
//! rather than a typed value is deliberate - the protocol cannot know a
//! preset's `Settings` struct, and inventing a generic value type here would
//! be a second, untyped protocol layered on the first.
//!
//! Assets are the other half. An `AssetRef` in a preset's settings is a name;
//! the host has already resolved it, verified its digest, and written it to
//! disk. The preset receives the name-to-path mapping and reads the file
//! through GPUI's own asset source, so no image or font bytes travel over UI
//! IPC.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The longest an asset path may be once materialized.
///
/// A path the host produced, not one a person typed, so the bound is a
/// statement about what this implementation writes rather than a filter.
pub const MAX_ASSET_PATH_BYTES: usize = 1024;

/// The largest a settings document may be.
pub const MAX_SETTINGS_BYTES: usize = 256 * 1024;

/// The most asset references one application may configure.
pub const MAX_ASSETS: usize = 256;

/// The application's preset configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Configuration {
    /// `[ui.settings]` as JSON, already schema-validated.
    pub settings: serde_json::Value,
    /// Each asset name to the file the host materialized it to.
    pub assets: BTreeMap<String, String>,
}

impl Configuration {
    /// A configuration with no settings and no assets.
    pub fn empty() -> Self {
        Self {
            settings: serde_json::Value::Object(serde_json::Map::new()),
            assets: BTreeMap::new(),
        }
    }

    /// Whether a configuration is one this protocol version will carry.
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

/// A configuration this protocol version will not carry.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigurationError {
    #[error("configuration names {count} assets; the limit is {limit}")]
    TooManyAssets { count: usize, limit: usize },
    #[error("an asset name is empty")]
    EmptyAssetName,
    #[error("the path for asset `{name}` exceeds {limit} bytes")]
    AssetPathTooLong { name: String, limit: usize },
}
