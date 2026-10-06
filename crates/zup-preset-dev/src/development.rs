//! The application a preset is being developed against.
//!
//! A preset is not an application. It is the part of one that draws, and it
//! cannot be developed against nothing: the settings it takes and the files it
//! reads are chosen by whoever installs it, and a preset whose author has never
//! seen a real one of either is a preset that meets its first application on a
//! user's machine.
//!
//! So `zup preset dev` presents an application, described in the preset's own
//! project. It is a source file rather than something under the development
//! state directory, because it is the thing being authored: it belongs in the
//! same commit as the code it exercises, and a `.zup` directory is by
//! construction disposable.
//!
//! The document is a plain table of settings and a table of files. It is not a
//! preset format, carries no preset identity, and is validated against the schema
//! the preset generates rather than against anything Zup defines - which is what
//! makes a change to it a data change, and a data change costs no compilation.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// The name of the file in a preset project that describes the application `zup
/// ui dev` presents.
///
/// Public because it is a thing a preset author puts in a commit and a thing the
/// watcher has to recognise, and both of those are outside this file.
pub const FILE_NAME: &str = "zup.preset.dev.toml";

/// Why a development document could not be used.
#[derive(Debug, thiserror::Error)]
pub enum DevelopmentError {
    #[error("`{0}` could not be read: {1}")]
    Unreadable(PathBuf, String),
    #[error("`{0}` is not a table: {1}")]
    Malformed(PathBuf, String),
    #[error("the asset `{name}` names no file")]
    AssetWithoutFile { name: String },
}

/// The application `zup preset dev` presents.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Development {
    /// What the application configured, as the preset's own settings type reads
    /// it. Values are data; a change here is a change to a document, not to a
    /// program.
    #[serde(default)]
    pub settings: serde_json::Value,
    /// The files the application provides, by the name its settings refer to.
    #[serde(default)]
    pub assets: BTreeMap<String, String>,
}

impl Development {
    /// Read the document in `root`, or the empty one when there is none.
    ///
    /// An absent document is not a failure. A preset with no settings and no
    /// assets is a legitimate thing to develop, and requiring a file before the
    /// first build would mean a preset could not be run before it was configured.
    pub fn read(root: &Path) -> Result<Self, DevelopmentError> {
        let path = root.join(FILE_NAME);
        if !path.is_file() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(&path)
            .map_err(|error| DevelopmentError::Unreadable(path.clone(), error.to_string()))?;
        toml::from_str(&text).map_err(|error| DevelopmentError::Malformed(path, error.to_string()))
    }

    /// Where each named asset's file is on this machine.
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

    /// The files this document depends on, so a watcher can tell an asset change
    /// from a code change.
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
