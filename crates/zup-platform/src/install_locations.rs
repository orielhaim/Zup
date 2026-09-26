use thiserror::Error;
use zup_core::{InstallLocation, SelectedScope, TargetTriple};

use crate::target_path::TargetPath;

#[derive(Debug, Error)]
pub enum InstallLocationError {
    #[error("failed to resolve install location `{location}` for scope `{scope}`")]
    ResolutionFailed {
        location: InstallLocation,
        scope: SelectedScope,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync + 'static>,
    },
}

/// Maps a semantic install location onto a concrete path on one target.
///
/// A resolver is handed the target it must answer for, so it returns a path
/// that is already validated and spelled for that target. Callers never
/// re-interpret or re-parse the result.
pub trait InstallLocationResolver {
    fn resolve(
        &self,
        location: InstallLocation,
        scope: SelectedScope,
        target: &TargetTriple,
    ) -> Result<TargetPath, InstallLocationError>;
}
