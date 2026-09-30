//! What a person asked the installer to do.
//!
//! Actions are intent, not interaction. There is no "button was pressed", no
//! widget id, and no generic command channel: each variant is one thing the
//! installer can be asked to do, and the host either recognizes it or refuses
//! it. A preset that cannot be understood is a preset that gets nothing done,
//! which is better than one that gets something done nobody asked for.

use serde::{Deserialize, Serialize};

use crate::{ComponentId, InstallScope};

/// One installer operation a person asked for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "action")]
pub enum UiAction {
    /// Install for a different scope than the one currently chosen.
    SetScope { scope: InstallScope },
    /// Turn one component on or off.
    SetComponent {
        component: ComponentId,
        selected: bool,
    },
    /// Install somewhere other than the default, where the application allows it.
    SetInstallDirectory { directory: String },
    /// Recompute what the current choices would change.
    Preview,
    /// Apply the current choices.
    Install,
    /// Check the configured update channel.
    Update,
    /// Change which components an installed application has.
    Modify,
    /// Restore managed resources that no longer match.
    Repair,
    /// Ask to uninstall. The host answers with a confirmation state.
    RequestUninstall,
    /// Confirm an uninstall that was asked for.
    ConfirmUninstall,
    /// Abandon an uninstall that was asked for.
    DismissUninstall,
    /// Stop the running operation at its next safe boundary.
    Cancel,
    /// Re-run the operation that failed or was blocked.
    Retry,
    /// Reveal the session log.
    OpenLog,
    /// Put a diagnostic summary on the clipboard.
    CopyDiagnostics,
    /// The person closed the window.
    Close,
}
