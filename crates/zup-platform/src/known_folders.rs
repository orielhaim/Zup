//! Known-folder abstraction for target path resolution.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use zup_core::SelectedScope;

/// Known folder variables supported by install templates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KnownFolder {
    ProgramFiles,
    LocalAppData,
    ProgramData,
    /// Scope-aware Start Menu (user or common).
    StartMenu,
    /// Scope-aware Desktop (user or public).
    Desktop,
    /// Scope-aware Start Menu → Programs (user or common).
    Programs,
}

/// Errors produced while resolving a known folder.
#[derive(Debug, Error)]
pub enum KnownFolderError {
    /// The platform API failed to resolve the folder.
    #[error("failed to resolve known folder `{folder:?}` for scope `{scope}`")]
    ResolutionFailed {
        folder: KnownFolder,
        scope: SelectedScope,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync + 'static>,
    },
}

/// Resolves known folders for a concrete target machine.
pub trait KnownFolderResolver {
    /// Resolve `folder` in the context of `scope`.
    fn resolve(
        &self,
        folder: KnownFolder,
        scope: SelectedScope,
    ) -> Result<PathBuf, KnownFolderError>;
}

/// Map a template variable to a known folder, if it is one.
pub fn known_folder_for_variable(variable: zup_core::Variable) -> Option<KnownFolder> {
    use zup_core::Variable as V;
    match variable {
        V::KnownProgramFiles => Some(KnownFolder::ProgramFiles),
        V::KnownLocalAppData => Some(KnownFolder::LocalAppData),
        V::KnownProgramData => Some(KnownFolder::ProgramData),
        V::KnownStartMenu => Some(KnownFolder::StartMenu),
        V::KnownDesktop => Some(KnownFolder::Desktop),
        V::AppId | V::AppName | V::AppVersion | V::Install => None,
    }
}
