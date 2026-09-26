//! Durable ownership state for one installed application and scope.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use zup_core::{
    AppId, BackendResourceId, ComponentId, Privilege, RelativePath, ResourceKey, SelectedScope,
    ServiceStart, Sha256Digest, TargetTriple,
};
use zup_platform::{CommandSpec, TargetPath};

pub const INSTALL_LEDGER_SCHEMA: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallLedger {
    pub schema: u32,
    pub app_id: AppId,
    pub scope: SelectedScope,
    pub target: TargetTriple,
    pub version: semver::Version,
    pub selected_components: Vec<ComponentId>,
    #[serde(default)]
    pub install_directory: Option<TargetPath>,
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
    pub fn new(app_id: AppId, target: TargetTriple, scope: SelectedScope) -> Self {
        Self {
            schema: INSTALL_LEDGER_SCHEMA,
            app_id,
            scope,
            target,
            version: semver::Version::new(0, 0, 0),
            selected_components: Vec::new(),
            install_directory: None,
            committed_transaction: String::new(),
            resources: BTreeMap::new(),
        }
    }
}

/// One resource this installation owns, plus the authority that created it.
///
/// `privilege` is recorded at install time and is the only correct source for
/// the authorization of a later removal: it survives scope changes, and it does
/// not have to be re-derived from a plan that no longer exists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum OwnedResource {
    File {
        destination: TargetPath,
        source_relative: RelativePath,
        sha256: Sha256Digest,
        size: u64,
        created_directories: Vec<TargetPath>,
        privilege: Privilege,
    },
    Launcher {
        launcher_path: TargetPath,
        privilege: Privilege,
        previous: LauncherState,
        installed: LauncherState,
    },
    PathEntry {
        value: TargetPath,
        /// Host spelling of the search path that owns the entry, as stored.
        value_type: String,
        privilege: Privilege,
    },
    Protocol {
        privilege: Privilege,
        previous: ProtocolState,
        installed: ProtocolState,
    },
    FileAssociation {
        privilege: Privilege,
        previous: FileAssociationState,
        installed: FileAssociationState,
    },
    Extension {
        privilege: Privilege,
        previous: ExtensionState,
        installed: ExtensionState,
    },
    Service {
        name: String,
        privilege: Privilege,
        previous: ServiceState,
        installed: ServiceState,
    },
    Backend {
        id: BackendResourceId,
        privilege: Privilege,
        payload: Vec<u8>,
    },
}

impl OwnedResource {
    /// Authority this installation used when it created the resource.
    pub const fn privilege(&self) -> Privilege {
        match self {
            Self::File { privilege, .. }
            | Self::Launcher { privilege, .. }
            | Self::PathEntry { privilege, .. }
            | Self::Protocol { privilege, .. }
            | Self::FileAssociation { privilege, .. }
            | Self::Extension { privilege, .. }
            | Self::Service { privilege, .. }
            | Self::Backend { privilege, .. } => *privilege,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LauncherState {
    Absent,
    Launcher {
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
pub enum FileAssociationState {
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
    Mapped { association_id: String },
}
