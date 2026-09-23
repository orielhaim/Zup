//! Durable ownership state for one installed application and scope.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use zup_core::{
    AppId, ComponentId, RelativePath, ResourceKey, SelectedScope, ServiceStart, Sha256Digest,
};
use zup_platform::{CommandSpec, TargetPath};

pub const INSTALL_LEDGER_SCHEMA: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallLedger {
    pub schema: u32,
    pub app_id: AppId,
    pub scope: SelectedScope,
    pub version: semver::Version,
    pub selected_components: Vec<ComponentId>,
    pub committed_transaction: String,
    #[serde(with = "resource_map")]
    pub resources: BTreeMap<ResourceKey, OwnedResource>,
}

mod resource_map {
    use super::*;
    use serde::de::Error as _;
    use serde::{Deserializer, Serializer};

    pub fn serialize<S: Serializer>(
        value: &BTreeMap<ResourceKey, OwnedResource>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        value.iter().collect::<Vec<_>>().serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<BTreeMap<ResourceKey, OwnedResource>, D::Error> {
        let entries = Vec::<(ResourceKey, OwnedResource)>::deserialize(deserializer)?;
        let mut map = BTreeMap::new();
        for (key, value) in entries {
            if map.insert(key, value).is_some() {
                return Err(D::Error::custom("duplicate ledger resource"));
            }
        }
        Ok(map)
    }
}

impl InstallLedger {
    pub fn new(app_id: AppId, scope: SelectedScope) -> Self {
        Self {
            schema: INSTALL_LEDGER_SCHEMA,
            app_id,
            scope,
            version: semver::Version::new(0, 0, 0),
            selected_components: Vec::new(),
            committed_transaction: String::new(),
            resources: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum OwnedResource {
    File {
        destination: TargetPath,
        source_relative: RelativePath,
        sha256: Sha256Digest,
        size: u64,
        created_directories: Vec<TargetPath>,
    },
    Shortcut {
        link_path: TargetPath,
        previous: ShortcutState,
        installed: ShortcutState,
    },
    PathEntry {
        value: TargetPath,
        value_type: String,
    },
    Protocol {
        previous: ProtocolState,
        installed: ProtocolState,
    },
    ProgId {
        previous: ProgIdState,
        installed: ProgIdState,
    },
    Extension {
        previous: ExtensionState,
        installed: ExtensionState,
    },
    Service {
        name: String,
        previous: ServiceState,
        installed: ServiceState,
    },
    UninstallEntry {
        scope: SelectedScope,
        state: UninstallEntryState,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UninstallEntryState {
    pub values: BTreeMap<String, UninstallEntryValue>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value")]
pub enum UninstallEntryValue {
    String(String),
    Dword(u32),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShortcutState {
    Absent,
    Link {
        target: TargetPath,
        arguments: Vec<String>,
        working_directory: Option<TargetPath>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceState {
    Absent,
    Registration {
        display_name: String,
        command: CommandSpec,
        start: ServiceStart,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtocolState {
    Absent,
    Registration { command: CommandSpec },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProgIdState {
    Absent,
    Registration {
        description: Option<String>,
        command: CommandSpec,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionState {
    Absent,
    Mapped { prog_id: String },
}
