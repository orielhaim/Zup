//! Typed `zup.toml` data model.

use std::path::PathBuf;

use semver::Version;
use serde::{Deserialize, Serialize};

/// Currently supported manifest schema version.
pub const SCHEMA_VERSION: u32 = 1;

/// A parsed and validated `zup.toml` manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub schema: u32,
    pub app: App,
    pub source: Source,
    pub install: Install,
}

/// Application identity and version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct App {
    pub id: String,
    pub name: String,
    pub version: Version,
}

/// Payload source for the installer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    pub directory: PathBuf,
}

/// Installation preferences.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Install {
    pub scope: InstallScope,
}

/// Scope an installer may target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallScope {
    User,
    Machine,
    Either,
}
