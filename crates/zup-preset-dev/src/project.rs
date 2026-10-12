use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Project {
    pub root: PathBuf,
    pub name: String,
    pub version: String,
    pub binary: String,
    pub target: PathBuf,
}

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
