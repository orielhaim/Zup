use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

pub const FILE_NAME: &str = "zup.preset.dev.toml";

#[derive(Debug, thiserror::Error)]
pub enum DevelopmentError {
    #[error("`{0}` could not be read: {1}")]
    Unreadable(PathBuf, String),
    #[error("`{0}` is not a table: {1}")]
    Malformed(PathBuf, String),
    #[error("the asset `{name}` names no file")]
    AssetWithoutFile { name: String },
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Development {
    #[serde(default)]
    pub settings: serde_json::Value,
    #[serde(default)]
    pub assets: BTreeMap<String, String>,
}

impl Development {
    pub fn read(root: &Path) -> Result<Self, DevelopmentError> {
        let path = root.join(FILE_NAME);
        if !path.is_file() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(&path)
            .map_err(|error| DevelopmentError::Unreadable(path.clone(), error.to_string()))?;
        toml::from_str(&text).map_err(|error| DevelopmentError::Malformed(path, error.to_string()))
    }

    pub fn asset_files(&self, root: &Path) -> Result<Vec<(String, PathBuf)>, DevelopmentError> {
        self.assets
            .iter()
            .map(|(name, file)| {
                if file.is_empty() {
                    return Err(DevelopmentError::AssetWithoutFile { name: name.clone() });
                }
                Ok((name.clone(), root.join(file)))
            })
            .collect()
    }

    pub fn watched_files(&self, root: &Path) -> Vec<PathBuf> {
        let mut files: Vec<PathBuf> = self
            .assets
            .values()
            .filter(|file| !file.is_empty())
            .map(|file| root.join(file))
            .collect();
        files.sort();
        files.dedup();
        files
    }
}
