use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use zup_bootstrap::{
    BootstrapId, BootstrapKey, BootstrapOperation, BootstrapPlan, BootstrapState,
    BootstrapStateStore, BoundBootstrapPlan, Quarantine,
};
use zup_core::SelectedScope;
use zup_plan::InstallPlan;
use zup_runtime::BootstrapRequest;

use crate::bundle_packager::EmbeddedBundle;

#[derive(Debug, thiserror::Error)]
pub enum BootstrapPlanError {
    #[error("prerequisite plan: {0}")]
    Plan(String),
    #[error("prerequisite detection: {0}")]
    Detection(String),
    #[error("prerequisite quarantine: {0}")]
    Quarantine(String),
    #[error("prerequisite reservation: {0}")]
    Reservation(String),
    #[error("clear prerequisite staging: {0}")]
    ClearStaging(String),
    #[error("embedded prerequisite: {0}")]
    Embedded(String),
    #[error("stage prerequisite: {0}")]
    Stage(String),
    #[error("download runtime: {0}")]
    DownloadRuntime(String),
    #[error("download prerequisite: {0}")]
    Download(String),
    #[error("verify prerequisite: {0}")]
    Verify(String),
    #[error("bind prerequisite plan: {0}")]
    Bind(String),
    #[error("state root: {0}")]
    StateRoot(String),
}

pub fn peer_user_state_root() -> Result<PathBuf, BootstrapPlanError> {
    crate::machine_state::default_state_root(SelectedScope::User)
        .map_err(|error| BootstrapPlanError::StateRoot(error.to_string()))
}

pub fn prepare(
    install: &InstallPlan,
    state_root: &Path,
    scope: SelectedScope,
    bundle: Option<&EmbeddedBundle>,
) -> Result<Option<BootstrapRequest>, BootstrapPlanError> {
    if install.prerequisites.is_empty() {
        return Ok(None);
    }
    let operations = install
        .prerequisites
        .iter()
        .map(|prerequisite| BootstrapOperation {
            id: prerequisite.id.clone(),
            name: prerequisite.name.to_string(),
            target: prerequisite.target,
            requirement: prerequisite.requirement.clone(),
            package: prerequisite.package.clone(),
            installer: prerequisite.installer.clone(),
        })
        .collect::<Vec<_>>();
    let plan = BootstrapPlan::new(
        BootstrapKey {
            app_id: install.app.id.clone(),
            app_version: install.app.version.clone(),
            scope,
            target: install.target,
        },
        operations,
    )
    .map_err(|error| BootstrapPlanError::Plan(error.to_string()))?;
    let id = BootstrapId::for_plan(&plan);
    let mut assessment = BootstrapState::new(&plan);
    assessment.id = id;
    zup_bootstrap::assess(
        &plan,
        &crate::prerequisites::WindowsPrerequisiteDetector,
        &mut assessment,
    )
    .map_err(|error| BootstrapPlanError::Detection(error.to_string()))?;

    let quarantine_root = if scope == SelectedScope::Machine {
        peer_user_state_root()?
            .join("bootstrap-acquisition")
            .join(id.as_uuid().to_string())
    } else {
        state_root
            .join("bootstrap")
            .join("quarantine")
            .join(id.as_uuid().to_string())
    };
    let quarantine = Quarantine::with_file_system(
        &quarantine_root,
        crate::bootstrap_fs::windows_bootstrap_file_system(),
    )
    .map_err(|error| BootstrapPlanError::Quarantine(error.to_string()))?;
    if assessment.remaining.is_empty() {
        let _ = zup_bootstrap::FilesystemBootstrapStateStore::with_file_system(
            state_root,
            crate::bootstrap_fs::windows_bootstrap_file_system(),
        )
        .remove(id);
        let bound = BoundBootstrapPlan::with_id(id, plan, BTreeMap::new())
            .map_err(|error| BootstrapPlanError::Bind(error.to_string()))?;
        return Ok(Some(BootstrapRequest {
            plan: bound,
            state_root: state_root.to_path_buf(),
            quarantine_root,
        }));
    }

    let mut artifacts = BTreeMap::new();
    for operation in &plan.operations {
        if !assessment.remaining.contains(&operation.id) {
            continue;
        }
        let reservation = quarantine
            .reserve(
                &operation.id,
                operation.package.filename(),
                operation.package.size(),
            )
            .map_err(|error| BootstrapPlanError::Reservation(error.to_string()))?;
        quarantine
            .remove_partial(&reservation)
            .map_err(|error| BootstrapPlanError::ClearStaging(error.to_string()))?;
        let artifact = match &operation.package {
            zup_core::PrerequisitePackage::Embedded { .. } => {
                let bundle = bundle.ok_or_else(|| {
                    BootstrapPlanError::Embedded(format!(
                        "this lifecycle needs the embedded prerequisite `{}` and there is no \
                         package to read it from",
                        operation.name
                    ))
                })?;
                let bytes = bundle
                    .prerequisite_bytes(&operation.id)
                    .map_err(|error| BootstrapPlanError::Embedded(error.to_string()))?;
                quarantine
                    .stage_bytes(&reservation, &bytes, operation.package.digest())
                    .map_err(|error| BootstrapPlanError::Stage(error.to_string()))?
            }
            zup_core::PrerequisitePackage::Remote {
                url, sha256, size, ..
            } => {
                let tokio = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|error| BootstrapPlanError::DownloadRuntime(error.to_string()))?;
                tokio
                    .block_on(zup_update::fetch_pinned(
                        url,
                        *sha256,
                        *size,
                        &reservation.partial_path,
                        zup_core::MAX_PREREQUISITE_PACKAGE_BYTES,
                    ))
                    .map_err(|error| BootstrapPlanError::Download(error.to_string()))?;
                quarantine
                    .publish(&reservation, *sha256)
                    .map_err(|error| BootstrapPlanError::Verify(error.to_string()))?
            }
        };
        artifacts.insert(operation.id.clone(), artifact);
    }
    let bound = BoundBootstrapPlan::with_id(id, plan, artifacts)
        .map_err(|error| BootstrapPlanError::Bind(error.to_string()))?;
    Ok(Some(BootstrapRequest {
        plan: bound,
        state_root: state_root.to_path_buf(),
        quarantine_root,
    }))
}
