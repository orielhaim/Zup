use serde::{Deserialize, Serialize};

use crate::{ComponentId, InstallScope};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "action")]
pub enum Action {
    SetScope {
        scope: InstallScope,
    },
    SetComponent {
        component: ComponentId,
        selected: bool,
    },
    SetInstallDirectory {
        directory: String,
    },
    ResetInstallDirectory,
    Install,
    Update,
    Modify,
    Repair,
    RequestUninstall,
    ConfirmUninstall,
    DismissUninstall,
    Cancel,
    Retry,
    OpenLog,
    CopyDiagnostics,
    Launch,
    Close,
}
