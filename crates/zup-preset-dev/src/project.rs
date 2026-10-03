//! What a preset project is.
//!
//! A preset is a normal Cargo project, so this is Cargo's own answer rather than
//! one Zup keeps beside it: the root package, the binary, and the workspace's
//! target directory. A preset that needed a Zup-specific manifest to be found by
//! the tool that develops it would not be the normal Rust project the SDK's whole
//! argument claims a preset is.
//!
//! The only thing Zup adds is where the session's own state goes, and that is not
//! this file's business: the runtime it runs in decides that, because two
//! previews of two different things cannot share one directory.

use std::path::{Path, PathBuf};

/// The preset project `zup preset dev` is developing.
#[derive(Debug, Clone)]
pub struct Project {
    /// The directory holding this package's `Cargo.toml`.
    pub root: PathBuf,
    /// The package name, which is the preset's name.
    pub name: String,
    /// The version, which is the preset's version.
    pub version: String,
    /// The binary Cargo builds for this host.
    pub binary: String,
    /// The target's own target directory, which is where Cargo writes.
    pub target: PathBuf,
}

/// Why a directory is not a preset project.
#[derive(Debug, thiserror::Error)]
pub enum ProjectError {
    #[error("could not read Cargo metadata for `{0}`: {1}")]
    Metadata(PathBuf, String),
    #[error("`{0}` holds no Cargo package of its own; a preset is a project")]
    NoPackage(PathBuf),
    #[error("the package in `{0}` has no binary; a preset is an executable")]
    NoBinary(PathBuf),
    #[error("the package in `{0}` has more than one binary; `zup preset dev` does not guess")]
    AmbiguousBinary(PathBuf),
}

impl Project {
    /// Read the preset project rooted at `root`.
    pub fn read(root: &Path) -> Result<Self, ProjectError> {
        let canonical = std::fs::canonicalize(root)
            .map_err(|error| ProjectError::Metadata(root.to_path_buf(), error.to_string()))?;
        let mut command = cargo_metadata::MetadataCommand::new();
        command.no_deps().current_dir(&canonical);
        let metadata = command
            .exec()
            .map_err(|error| ProjectError::Metadata(canonical.clone(), error.to_string()))?;

        let package = metadata
            .packages
            .iter()
            .find(|package| {
                package
                    .manifest_path
                    .parent()
                    .and_then(|directory| directory.as_std_path().canonicalize().ok())
                    .is_some_and(|directory| directory == canonical)
            })
            .ok_or_else(|| ProjectError::NoPackage(canonical.clone()))?;

        let binaries: Vec<&str> = package
            .targets
            .iter()
            .filter(|target| target.kind.iter().any(|kind| kind.to_string() == "bin"))
            .map(|target| target.name.as_str())
            .collect();
        let binary = match binaries.as_slice() {
            [one] => *one,
            [] => return Err(ProjectError::NoBinary(canonical.clone())),
            _ => return Err(ProjectError::AmbiguousBinary(canonical.clone())),
        };

        Ok(Self {
            root: canonical,
            name: package.name.to_string(),
            version: package.version.to_string(),
            binary: binary.to_owned(),
            target: metadata.target_directory.into_std_path_buf(),
        })
    }
}
