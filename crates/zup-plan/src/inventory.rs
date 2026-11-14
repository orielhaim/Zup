//! Durable ownership state for one installed application and scope.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use zup_core::ReleaseIdentity;
use zup_core::{
    AppId, BackendResourceId, ComponentId, InstalledPreset, Privilege, RelativePath, ResourceKey,
    SelectedScope, ServiceStart, Sha256Digest, Target,
};
use zup_core::{CommandSpec, TargetPath};

pub const INSTALLATION_RECORD_SCHEMA: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallationState {
    Installed,
    Removed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeploymentSnapshot {
    pub requirements: crate::DeploymentRequirements,
    pub revision: zup_core::ActivationRevision,
    pub transaction: zup_core::TransactionId,
    pub plan_hash: Sha256Digest,
    pub version: semver::Version,
    pub selected_components: Vec<ComponentId>,
    pub release: Option<ReleaseIdentity>,
    pub preset: Option<InstalledPreset>,
    pub materialization: Option<zup_core::MaterializationSpec>,
    pub maintenance: Option<zup_core::MaterializationSpec>,
    #[serde(with = "resource_map")]
    pub resources: BTreeMap<ResourceKey, OwnedResource>,
}

impl DeploymentSnapshot {
    pub fn readers_for(
        &self,
        materialization: &zup_core::MaterializationSpec,
    ) -> Option<zup_core::GenerationReaders> {
        if self.materialization.as_ref() == Some(materialization) {
            Some(self.requirements.generation_readers)
        } else if self.maintenance.as_ref() == Some(materialization) {
            Some(if self.preset.is_none() {
                zup_core::GenerationReaders::NativeExecutables
            } else {
                self.requirements.generation_readers
            })
        } else {
            None
        }
    }

    pub fn selection(&self) -> crate::ActivationSelection {
        crate::ActivationSelection {
            revision: self.revision,
            materialization: self
                .materialization
                .as_ref()
                .map(zup_core::MaterializationSpec::reference),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstallationCommit {
    pub transaction: zup_core::TransactionId,
    pub plan_hash: Sha256Digest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstallationRecord {
    pub commit: InstallationCommit,
    pub retired_deployments: Vec<crate::RetiredDeployment>,
    pub data: BTreeMap<zup_core::ResourceId, crate::OwnedPersistentDirectory>,
    pub id: zup_core::InstallationId,
    pub state: InstallationState,
    pub app_id: AppId,
    pub scope: SelectedScope,
    pub target: Target,
    pub directory: TargetPath,
    pub current: DeploymentSnapshot,
    pub retained: Vec<DeploymentSnapshot>,
}

#[derive(Debug, thiserror::Error)]
pub enum InstallationRecordError {
    #[error("installation record contains an invalid materialization namespace")]
    Namespace,
    #[error("installation record repeats or misorders a deployment")]
    DeploymentOrder,
    #[error("a physical materialization has conflicting retained identities")]
    MaterializationIdentity,
    #[error("release identity does not match the installation")]
    ReleaseIdentity,
    #[error("removed installation still owns active resources")]
    RemovedResources,
    #[error("retained resource authority exceeds installation scope")]
    ResourceAuthority,
    #[error("deployment requirements do not match the installation snapshot")]
    Requirements,
}

#[derive(Debug, thiserror::Error)]
pub enum InstallationRecordCodecError {
    #[error("installation record has an unsupported schema")]
    Schema,
    #[error("installation record integrity check failed")]
    Integrity,
    #[error("installation record JSON is invalid: {0}")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Record(#[from] InstallationRecordError),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecordEnvelope<R> {
    schema: u32,
    record: R,
    integrity: Sha256Digest,
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

impl InstallationRecord {
    pub fn is_installed(&self) -> bool {
        self.state == InstallationState::Installed
    }

    pub fn owns_file(&self, path: &TargetPath, privilege: Privilege) -> bool {
        if path.target() != &self.target || privilege != self.scope.authorization() {
            return false;
        }
        self.current.resources.values().any(|owned| {
            matches!(owned,
            OwnedResource::File { destination, privilege: actual, .. }
                if *actual == privilege && destination.equivalent(path))
        }) || self
            .current
            .materialization
            .iter()
            .chain(self.current.maintenance.iter())
            .any(|materialization| {
                materialization.descriptor().files().iter().any(|file| {
                    materialization
                        .directory()
                        .join(file.path.as_str())
                        .is_ok_and(|member| member.equivalent(path))
                })
            })
    }

    pub fn references_file_object(
        &self,
        key: &ResourceKey,
        object: zup_core::NativeObjectIdentity,
    ) -> bool {
        self.retained
            .iter()
            .chain(std::iter::once(&self.current))
            .any(|snapshot| {
                matches!(snapshot.resources.get(key),
                Some(OwnedResource::File { instance, .. }) if instance.object == object)
            })
    }

    pub fn references_transaction(&self, transaction: zup_core::TransactionId) -> bool {
        self.commit.transaction == transaction || self.retired_deployments.iter().any(|retired| retired.retired_by == transaction
            || retired.snapshot.transaction == transaction
            || retired.snapshot.resources.values().any(|resource| matches!(resource, OwnedResource::File { instance, .. } if instance.origin == transaction)))
            || self.data.values().any(|owned| owned.origin == transaction)
            || self
                .retained
                .iter()
                .chain(std::iter::once(&self.current))
                .any(|snapshot| {
                    snapshot.transaction == transaction
                        || snapshot.resources.values().any(|resource| {
                            matches!(resource,
                    OwnedResource::File { instance, .. } if instance.origin == transaction)
                        })
                })
    }

    pub fn encode(&self) -> Result<Vec<u8>, InstallationRecordCodecError> {
        self.validate()?;
        let integrity = self.integrity()?;
        Ok(serde_json::to_vec(&RecordEnvelope {
            schema: INSTALLATION_RECORD_SCHEMA,
            record: self,
            integrity,
        })?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, InstallationRecordCodecError> {
        let envelope: RecordEnvelope<Self> = serde_json::from_slice(bytes)?;
        if envelope.schema != INSTALLATION_RECORD_SCHEMA {
            return Err(InstallationRecordCodecError::Schema);
        }
        if envelope.integrity != envelope.record.integrity()? {
            return Err(InstallationRecordCodecError::Integrity);
        }
        envelope.record.validate()?;
        Ok(envelope.record)
    }

    fn integrity(&self) -> Result<Sha256Digest, serde_json::Error> {
        use sha2::{Digest, Sha256};
        let bytes = serde_json::to_vec(self)?;
        let mut hash = Sha256::new();
        hash.update(b"zup:installation-record:1\0");
        hash.update(bytes);
        Ok(Sha256Digest::from_hasher(hash))
    }

    pub fn validate(&self) -> Result<(), InstallationRecordError> {
        if self.commit.transaction == self.current.transaction
            && self.commit.plan_hash != self.current.plan_hash
        {
            return Err(InstallationRecordError::DeploymentOrder);
        }
        let mut data_paths: Vec<TargetPath> = Vec::new();
        for (id, owned) in &self.data {
            owned
                .validate(&self.app_id, self.target, self.scope)
                .map_err(|_| InstallationRecordError::ResourceAuthority)?;
            let path = owned
                .ownership
                .binding()
                .path(self.target)
                .map_err(|_| InstallationRecordError::ResourceAuthority)?;
            if owned.installation != self.id
                || id != &owned.directory.id
                || data_paths
                    .iter()
                    .any(|other| other.starts_with(&path) || path.starts_with(other))
            {
                return Err(InstallationRecordError::ResourceAuthority);
            }
            data_paths.push(path);
        }
        if self.directory.target() != &self.target || self.directory.parent().is_none() {
            return Err(InstallationRecordError::Namespace);
        }
        if self.state == InstallationState::Removed
            && (self.current.materialization.is_some()
                || self.current.maintenance.is_some()
                || !self.current.resources.is_empty())
        {
            return Err(InstallationRecordError::RemovedResources);
        }
        if self
            .retained
            .windows(2)
            .any(|pair| pair[0].revision >= pair[1].revision)
            || self
                .retained
                .last()
                .is_some_and(|last| last.revision >= self.current.revision)
        {
            return Err(InstallationRecordError::DeploymentOrder);
        }
        if self
            .retired_deployments
            .windows(2)
            .any(|pair| pair[0].snapshot.revision >= pair[1].snapshot.revision)
            || self.retired_deployments.iter().any(|retired| {
                retired.snapshot.revision >= self.current.revision
                    || retired.retired_by == retired.snapshot.transaction
                    || self
                        .retained
                        .iter()
                        .any(|snapshot| snapshot.revision == retired.snapshot.revision)
            })
        {
            return Err(InstallationRecordError::DeploymentOrder);
        }
        let reserved = self
            .directory
            .join(".zup")
            .map_err(|_| InstallationRecordError::Namespace)?;
        let mut instances = BTreeMap::new();
        let mut transactions = BTreeSet::new();
        for snapshot in self
            .retained
            .iter()
            .chain(
                self.retired_deployments
                    .iter()
                    .map(|retired| &retired.snapshot),
            )
            .chain(std::iter::once(&self.current))
        {
            if snapshot.requirements.validate().is_err()
                || snapshot.requirements.app.id != self.app_id
                || snapshot.requirements.app.version != snapshot.version
                || snapshot.requirements.target != self.target
                || snapshot.requirements.scope != self.scope
                || snapshot.requirements.components != snapshot.selected_components
                || (self.state == InstallationState::Installed || snapshot != &self.current)
                    && snapshot.requirements.preset != snapshot.preset
            {
                return Err(InstallationRecordError::Requirements);
            }
            if let Some(maintenance) = &snapshot.maintenance {
                crate::validate_maintenance_content(maintenance, snapshot.preset.as_ref())
                    .map_err(|_| InstallationRecordError::Requirements)?;
            }
            if snapshot.resources.values().any(|resource| {
                resource.privilege() != self.scope.authorization()
                    || matches!(resource, OwnedResource::File { instance, .. }
                        if instance.object.operating_system() != self.target.operating_system())
            }) {
                return Err(InstallationRecordError::ResourceAuthority);
            }
            if !transactions.insert(snapshot.transaction) {
                return Err(InstallationRecordError::DeploymentOrder);
            }
            if let Some(release) = &snapshot.release
                && (release.validate().is_err()
                    || release.app_id != self.app_id
                    || release.target != self.target
                    || release.version != snapshot.version.to_string())
            {
                return Err(InstallationRecordError::ReleaseIdentity);
            }
            for (spec, namespace) in [
                (
                    snapshot.materialization.as_ref(),
                    zup_core::DEPLOYMENTS_DIRECTORY,
                ),
                (
                    snapshot.maintenance.as_ref(),
                    zup_core::MAINTENANCE_DEPLOYMENTS_DIRECTORY,
                ),
            ] {
                let Some(spec) = spec else {
                    continue;
                };
                let expected = self
                    .directory
                    .join(format!("{namespace}/{}", spec.id()))
                    .map_err(|_| InstallationRecordError::Namespace)?;
                if spec.installation() != self.id
                    || spec.descriptor().target() != self.target
                    || spec.directory() != &expected
                {
                    return Err(InstallationRecordError::Namespace);
                }
                if instances
                    .insert(spec.id(), spec)
                    .is_some_and(|previous| previous != spec)
                {
                    return Err(InstallationRecordError::MaterializationIdentity);
                }
            }
            if snapshot
                .resources
                .values()
                .filter_map(OwnedResource::file_destination)
                .any(|destination| destination.starts_with(&reserved))
            {
                return Err(InstallationRecordError::Namespace);
            }
        }
        Ok(())
    }

    /// The release identity this installation was produced from.
    ///
    /// `None` is a real answer, not a gap: an installation made from a
    /// development manifest has no authenticated graph behind it, and a repair
    /// on one has to say so rather than guess.
    pub fn release_identity(&self) -> Option<&ReleaseIdentity> {
        self.current.release.as_ref()
    }

    /// The preset this installation presents, if it has one.
    pub fn preset(&self) -> Option<&InstalledPreset> {
        self.current.preset.as_ref()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnedService {
    pub registration: zup_core::NativeServiceRegistration,
    pub name: String,
    pub timeouts: zup_core::ServiceTimeouts,
    pub privilege: Privilege,
    pub requires_files: Vec<crate::TargetPath>,
    pub requires_services: Vec<zup_core::ServiceId>,
    pub installed: Box<ServiceState>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", deny_unknown_fields)]
pub enum OwnedResource {
    File {
        instance: zup_core::FixedFileInstance,
        replacement: zup_core::FixedReplacement,
        destination: TargetPath,
        source_relative: RelativePath,
        sha256: Sha256Digest,
        size: u64,
        executable: bool,
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
    Service(OwnedService),
    Backend {
        id: BackendResourceId,
        privilege: Privilege,
        payload: Vec<u8>,
    },
}

impl OwnedResource {
    pub const fn privilege(&self) -> Privilege {
        match self {
            Self::File { privilege, .. }
            | Self::Launcher { privilege, .. }
            | Self::PathEntry { privilege, .. }
            | Self::Protocol { privilege, .. }
            | Self::FileAssociation { privilege, .. }
            | Self::Extension { privilege, .. }
            | Self::Backend { privilege, .. } => *privilege,
            Self::Service(service) => service.privilege,
        }
    }

    /// Where this resource lives, when it is a file.
    pub fn file_destination(&self) -> Option<&TargetPath> {
        match self {
            Self::File { destination, .. } => Some(destination),
            _ => None,
        }
    }

    /// Payload source used for acquisition and repair.
    pub fn source_relative(&self) -> Option<&RelativePath> {
        match self {
            Self::File {
                source_relative, ..
            } => Some(source_relative),
            _ => None,
        }
    }
}

/// Payload digests referenced by the current deployment.
pub fn owned_content_digests(ledger: &InstallationRecord) -> BTreeSet<Sha256Digest> {
    ledger
        .current
        .resources
        .values()
        .filter_map(|resource| match resource {
            OwnedResource::File { sha256, .. } => Some(*sha256),
            _ => None,
        })
        .chain(
            ledger
                .current
                .materialization
                .iter()
                .chain(ledger.current.maintenance.iter())
                .flat_map(|materialization| {
                    materialization
                        .descriptor()
                        .files()
                        .iter()
                        .map(|file| file.sha256)
                }),
        )
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
        state_directories: Vec<zup_core::RelativePath>,
        native_timeouts: zup_core::NativeServiceTimeouts,
        dependencies: Vec<String>,
        failure: zup_core::ServiceFailurePolicy,
        execution: zup_core::ServiceExecution,
        display_name: String,
        command: Box<CommandSpec>,
        start: ServiceStart,
        readiness: zup_core::ServiceReadiness,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceRegistrationChange {
    Registration,
    Command,
    ExecutionIdentity,
    PersistentState,
    Dependencies,
    Readiness,
    StartupPolicy,
    DisplayName,
    FailurePolicy,
    NativeTimeouts,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "runtime", content = "changes", rename_all = "snake_case")]
pub enum ServiceUpdate {
    Unchanged,
    Preserve(Vec<ServiceRegistrationChange>),
    Replace(Vec<ServiceRegistrationChange>),
}

impl ServiceUpdate {
    pub fn preserves_runtime(&self) -> bool {
        !matches!(self, Self::Replace(_))
    }
}

impl ServiceState {
    pub fn update_from(&self, previous: &Self) -> ServiceUpdate {
        use ServiceRegistrationChange as Change;
        if self == previous {
            return ServiceUpdate::Unchanged;
        }
        let (
            Self::Registration {
                state_directories,
                native_timeouts,
                dependencies,
                failure,
                execution,
                display_name,
                command,
                start,
                readiness,
            },
            Self::Registration {
                state_directories: old_directories,
                native_timeouts: old_timeouts,
                dependencies: old_dependencies,
                failure: old_failure,
                execution: old_execution,
                display_name: old_display,
                command: old_command,
                start: old_start,
                readiness: old_readiness,
            },
        ) = (self, previous)
        else {
            return ServiceUpdate::Replace(vec![Change::Registration]);
        };
        let changes: Vec<_> = [
            (command != old_command, Change::Command),
            (
                state_directories != old_directories,
                Change::PersistentState,
            ),
            (execution != old_execution, Change::ExecutionIdentity),
            (dependencies != old_dependencies, Change::Dependencies),
            (readiness != old_readiness, Change::Readiness),
            (start != old_start, Change::StartupPolicy),
            (display_name != old_display, Change::DisplayName),
            (failure != old_failure, Change::FailurePolicy),
            (native_timeouts != old_timeouts, Change::NativeTimeouts),
        ]
        .into_iter()
        .filter(|(changed, _)| *changed)
        .map(|(_, change)| change)
        .collect();
        let systemd = matches!(
            execution,
            zup_core::ServiceExecution::SystemdUser { .. }
                | zup_core::ServiceExecution::SystemdDynamicUser
        );
        let replacement = changes.iter().any(|change| match change {
            Change::DisplayName => false,
            Change::StartupPolicy => {
                *start == ServiceStart::Disabled || *old_start == ServiceStart::Disabled
            }
            Change::FailurePolicy | Change::NativeTimeouts => !systemd,
            _ => true,
        });
        if replacement {
            ServiceUpdate::Replace(changes)
        } else {
            ServiceUpdate::Preserve(changes)
        }
    }
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
