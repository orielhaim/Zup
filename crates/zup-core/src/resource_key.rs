//! Stable logical resource identity.

use serde::{Deserialize, Serialize};

use crate::ids::{ActionId, FileTypeId, ProtocolScheme, ServiceId};
use crate::model::{FileExtension, ShortcutLocation};

/// Stable logical identity for one planned resource.
///
/// Persisted across versions for ownership, repair, uninstall, and upgrade
/// matching. Never use runtime UUIDs here.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceKey {
    File {
        destination: String,
    },
    Shortcut {
        location: ShortcutLocation,
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
    FileType {
        id: FileTypeId,
    },
    FileTypeExtension {
        extension: FileExtension,
    },
    ExternalAction {
        id: ActionId,
    },
}
