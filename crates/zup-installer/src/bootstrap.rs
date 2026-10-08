use std::collections::BTreeMap;
use std::path::Path;

use zup_bootstrap::{
    BootstrapId, BootstrapKey, BootstrapOperation, BootstrapPlan, BootstrapState,
    BootstrapStateStore, BoundBootstrapPlan, Quarantine,
};
use zup_core::SelectedScope;
use zup_plan::InstallPlan;

use crate::maintenance as state;

pub fn prepare(
    install: &InstallPlan,
    state_root: &Path,
    scope: SelectedScope,
    bundle: Option<&zup_windows::EmbeddedBundle>,
) -> miette::Result<Option<zup_runtime::BootstrapRequest>> {
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
            target: install.target.clone(),
        },
        operations,
    )
    .map_err(|error| miette::miette!("prerequisite plan: {error}"))?;
    let id = BootstrapId::for_plan(&plan);
    let mut assessment = BootstrapState::new(&plan);
    assessment.id = id;
    zup_bootstrap::assess(
        &plan,
        &zup_windows::WindowsPrerequisiteDetector,
        &mut assessment,
    )
    .map_err(|error| miette::miette!("prerequisite detection: {error}"))?;

    let quarantine_root = if scope == SelectedScope::Machine {
        state::peer_user_state_root()?
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
        zup_windows::windows_bootstrap_file_system(),
    )
    .map_err(|error| miette::miette!("prerequisite quarantine: {error}"))?;
    if assessment.remaining.is_empty() {
        let _ = zup_bootstrap::FilesystemBootstrapStateStore::with_file_system(
            state_root,
            zup_windows::windows_bootstrap_file_system(),
        )
        .remove(id);
        let bound = BoundBootstrapPlan::with_id(id, plan, BTreeMap::new())
            .map_err(|error| miette::miette!("bind prerequisite plan: {error}"))?;
        return Ok(Some(zup_runtime::BootstrapRequest {
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
            .map_err(|error| miette::miette!("prerequisite reservation: {error}"))?;
        quarantine
            .remove_partial(&reservation)
            .map_err(|error| miette::miette!("clear prerequisite staging: {error}"))?;
        let artifact = match &operation.package {
            zup_core::PrerequisitePackage::Embedded { .. } => {
                let bundle = bundle.ok_or_else(|| {
                    miette::miette!(
                        "this lifecycle needs the embedded prerequisite `{}` and there is no \
                         package to read it from",
                        operation.name
                    )
                })?;
                let bytes = bundle
                    .prerequisite_bytes(&operation.id)
                    .map_err(|error| miette::miette!("embedded prerequisite: {error}"))?;
                quarantine
                    .stage_bytes(&reservation, &bytes, operation.package.digest())
                    .map_err(|error| miette::miette!("stage prerequisite: {error}"))?
            }
            zup_core::PrerequisitePackage::Remote {
                url, sha256, size, ..
            } => {
                let tokio = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|error| miette::miette!("download runtime: {error}"))?;
                tokio
                    .block_on(zup_update::fetch_pinned(
                        url,
                        *sha256,
                        *size,
                        &reservation.partial_path,
                        zup_core::MAX_PREREQUISITE_PACKAGE_BYTES,
                    ))
                    .map_err(|error| miette::miette!("download prerequisite: {error}"))?;
                quarantine
                    .publish(&reservation, *sha256)
                    .map_err(|error| miette::miette!("verify prerequisite: {error}"))?
            }
        };
        artifacts.insert(operation.id.clone(), artifact);
    }
    let bound = BoundBootstrapPlan::with_id(id, plan, artifacts)
        .map_err(|error| miette::miette!("bind prerequisite plan: {error}"))?;
    Ok(Some(zup_runtime::BootstrapRequest {
        plan: bound,
        state_root: state_root.to_path_buf(),
        quarantine_root,
    }))
}
