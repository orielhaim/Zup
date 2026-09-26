//! Stable logical resource identity.

use serde::{Deserialize, Serialize};

use crate::ids::{BackendResourceId, FileAssociationId, ProtocolScheme, ServiceId};
use crate::model::{FileExtension, LauncherLocation};

/// Stable logical identity for one planned resource.
///
/// Persisted across versions for ownership, repair, uninstall, and upgrade
/// matching. Never use runtime UUIDs here.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceKey {
    Maintenance {
        app_id: String,
        version: String,
        destination: String,
    },
    Backend {
        id: BackendResourceId,
    },
    File {
        destination: String,
    },
    Launcher {
        location: LauncherLocation,
        name: String,
    },
    PathEntry {
        value: String,
    },
    Service {
        id: ServiceId,
    },
    Protocol {
        scheme: ProtocolScheme,
    },
    FileAssociation {
        id: FileAssociationId,
    },
    FileAssociationExtension {
        extension: FileExtension,
    },
}
