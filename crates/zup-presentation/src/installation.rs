use std::collections::HashSet;
use std::fmt::Write;

use serde::{Deserialize, Serialize};
use zup_core::{
    ActivationRevision, DeploymentContractId, InstallationId, MaterializationId, PayloadTreeId,
    SelectedScope, Target,
};
use zup_plan::{
    DeploymentSnapshot, InstallationRecord, InstallationRecordError, InstallationState,
    OwnedResource,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeploymentVersion {
    pub revision: ActivationRevision,
    pub version: String,
    pub contract: DeploymentContractId,
    pub content: Option<PayloadTreeId>,
    pub materialization: Option<MaterializationId>,
}

impl From<&DeploymentSnapshot> for DeploymentVersion {
    fn from(snapshot: &DeploymentSnapshot) -> Self {
        Self {
            revision: snapshot.revision,
            version: snapshot.version.to_string(),
            contract: snapshot.requirements.identity(),
            content: snapshot
                .materialization
                .as_ref()
                .map(|generation| generation.reference().tree),
            materialization: snapshot
                .materialization
                .as_ref()
                .map(|generation| generation.id()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallationReport {
    pub installation: InstallationId,
    pub application: String,
    pub scope: SelectedScope,
    pub target: Target,
    pub directory: String,
    pub state: InstallationState,
    pub current: DeploymentVersion,
    pub retained: Vec<DeploymentVersion>,
    pub retired: Vec<DeploymentVersion>,
    pub generation_bytes: u64,
    pub maintenance_bytes: u64,
    pub fixed_file_bytes: u64,
    pub persistent_directories: Vec<String>,
}

impl InstallationReport {
    pub fn from_record(record: &InstallationRecord) -> Result<Self, InstallationRecordError> {
        record.validate()?;
        let mut generations = HashSet::new();
        let mut fixed = HashSet::new();
        let mut generation_bytes = 0u64;
        let mut maintenance_bytes = 0u64;
        let mut fixed_file_bytes = 0u64;
        for snapshot in std::iter::once(&record.current)
            .chain(&record.retained)
            .chain(
                record
                    .retired_deployments
                    .iter()
                    .map(|retired| &retired.snapshot),
            )
        {
            if let Some(generation) = &snapshot.materialization
                && generations.insert(generation.id())
            {
                generation_bytes = generation_bytes.saturating_add(generation.descriptor().size());
            }
            if let Some(generation) = &snapshot.maintenance
                && generations.insert(generation.id())
            {
                maintenance_bytes =
                    maintenance_bytes.saturating_add(generation.descriptor().size());
            }
            for resource in snapshot.resources.values() {
                if let OwnedResource::File { instance, size, .. } = resource
                    && fixed.insert(instance.object)
                {
                    fixed_file_bytes = fixed_file_bytes.saturating_add(*size);
                }
            }
        }
        Ok(Self {
            installation: record.id,
            application: record.app_id.to_string(),
            scope: record.scope,
            target: record.target,
            directory: record.directory.to_string(),
            state: record.state,
            current: (&record.current).into(),
            retained: record.retained.iter().map(Into::into).collect(),
            retired: record
                .retired_deployments
                .iter()
                .map(|retired| (&retired.snapshot).into())
                .collect(),
            generation_bytes,
            maintenance_bytes,
            fixed_file_bytes,
            persistent_directories: record
                .data
                .values()
                .map(|owned| {
                    owned
                        .ownership
                        .binding()
                        .path(record.target)
                        .map(|path| path.to_string())
                })
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| InstallationRecordError::ResourceAuthority)?
                .into_iter()
                .collect(),
        })
    }

    pub fn human(&self) -> String {
        let mut output = String::new();
        let _ = writeln!(
            output,
            "{} ({}, {})",
            self.application, self.scope, self.target
        );
        let _ = writeln!(output, "Installation: {}", self.installation);
        let _ = writeln!(output, "Location: {}", self.directory);
        let _ = writeln!(
            output,
            "State: {}",
            match self.state {
                InstallationState::Installed => "installed",
                InstallationState::Removed => "removed; retained ownership remains",
            }
        );
        let _ = writeln!(
            output,
            "Current: {} (revision {})",
            self.current.version, self.current.revision
        );
        for retained in self.retained.iter().rev() {
            let _ = writeln!(
                output,
                "Retained: {} (revision {})",
                retained.version, retained.revision
            );
        }
        for retired in &self.retired {
            let _ = writeln!(
                output,
                "Retired: {} (revision {}; no longer a rollback target)",
                retired.version, retired.revision
            );
        }
        let _ = writeln!(
            output,
            "Recorded application payload: {}",
            zup_core::format_bytes(self.generation_bytes)
        );
        let _ = writeln!(
            output,
            "Recorded maintenance payload: {}",
            zup_core::format_bytes(self.maintenance_bytes)
        );
        let _ = writeln!(
            output,
            "Recorded fixed instances: {}",
            zup_core::format_bytes(self.fixed_file_bytes)
        );
        let _ = writeln!(
            output,
            "Recorded sizes can include collected history; they exclude mutable data, metadata, allocation overhead and unfinished transactions."
        );
        for directory in &self.persistent_directories {
            let _ = writeln!(output, "Preserved data: {directory}");
        }
        output
    }
}
