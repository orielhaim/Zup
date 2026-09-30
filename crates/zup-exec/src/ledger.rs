//! Durable ownership state for one installed application and scope.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use zup_core::ReleaseIdentity;
use zup_core::{
    AppId, BackendResourceId, ComponentId, Privilege, RelativePath, ResourceKey, SelectedScope,
    ServiceStart, Sha256Digest, TargetTriple, UiRuntime,
};
use zup_platform::{CommandSpec, TargetPath};

pub const INSTALL_LEDGER_SCHEMA: u32 = 2;

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
    /// The exact release graph that produced this installation.
    ///
    /// This is the field a repair, an update, and a recovery act on. `version` is
    /// what a person reads; this is what is trusted, because two builds can carry
    /// the same `1.4.0` and install different bytes on the same machine.
    ///
    /// It is absent for an installation that was not produced from a release
    /// graph - a development run from a manifest, for instance - and its absence
    /// is why those installations report that they have no release identity
    /// rather than pretending to one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release: Option<ReleaseIdentity>,
    /// The window this installation presents, as durable runtime facts.
    ///
    /// `None` for an installation with no window. Always written, never defaulted
    /// on read: a ledger that omitted the field would be indistinguishable from
    /// one that recorded no window, and a graphical installation that has lost
    /// this has lost the ability to open at all.
    pub ui: Option<UiRuntime>,
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
            release: None,
            ui: None,
            committed_transaction: String::new(),
            resources: BTreeMap::new(),
        }
    }

    /// The release identity this installation was produced from.
    ///
    /// `None` is a real answer, not a gap: an installation made from a
    /// development manifest has no authenticated graph behind it, and a repair
    /// on one has to say so rather than guess.
    pub fn release_identity(&self) -> Option<&ReleaseIdentity> {
        self.release.as_ref()
    }

    /// The window this installation presents, if it has one.
    pub fn ui(&self) -> Option<&UiRuntime> {
        self.ui.as_ref()
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

    /// The content digest behind this resource, when it is content.
    ///
    /// Only a file and a backend carry bytes; a launcher, a path entry, a
    /// protocol handler, a service, and a registry value are machine state that
    /// a release graph does not carry and a repair recreates from the plan. That
    /// distinction is what makes a repair's closure exact rather than "the whole
    /// application, in case".
    pub const fn content_digest(&self) -> Option<Sha256Digest> {
        match self {
            Self::File { sha256, .. } => Some(*sha256),
            _ => None,
        }
    }

    /// Where this resource lives, when it is a file.
    pub fn file_destination(&self) -> Option<&TargetPath> {
        match self {
            Self::File { destination, .. } => Some(destination),
            _ => None,
        }
    }

    /// The portable path a content source would address this resource by.
    ///
    /// This is the join between ownership and content: the ledger says which
    /// *portable* path this installation owns, and that is the key a plan and a
    /// content source both use. Two installations of different releases can own
    /// the same portable path with different digests, and the digest is what
    /// tells them apart.
    pub fn source_relative(&self) -> Option<&RelativePath> {
        match self {
            Self::File {
                source_relative, ..
            } => Some(source_relative),
            _ => None,
        }
    }
}

/// Every content digest an installation owns, in ascending order.
///
/// This is the set a repair may ask for and the set a retention policy keeps.
/// It is derived from ownership rather than from a plan, which is the whole
/// ownership guarantee: a file the machine does not own is not in here, so it
/// cannot be restored by an operation that claims to restore only what this
/// installation created.
pub fn owned_content_digests(ledger: &InstallLedger) -> BTreeSet<Sha256Digest> {
    ledger
        .resources
        .values()
        .filter_map(OwnedResource::content_digest)
        .collect()
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
