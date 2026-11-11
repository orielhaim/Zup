use serde::{Deserialize, Serialize};
use zup_core::{AppId, SelectedScope, Sha256Digest, Target, hash_bytes};

use crate::{ExecutionPlan, InstallationRecord};

/// Binds decisions to their desired content and the installation generation inspected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeploymentBinding {
    pub requirements: crate::DeploymentRequirements,
    pub installation: zup_core::InstallationId,
    pub observed_activation: Option<crate::ActivationSelection>,
    pub app_id: AppId,
    pub target: Target,
    pub scope: SelectedScope,
    pub desired_sha256: Option<Sha256Digest>,
    pub observed_transaction: Option<zup_core::TransactionId>,
}

#[derive(Debug, thiserror::Error)]
#[error("deployment authorization does not match the desired state or installation generation")]
pub struct BindingMismatch;

impl DeploymentBinding {
    pub fn desired(desired: &crate::BoundDeployment, ledger: Option<&InstallationRecord>) -> Self {
        Self {
            requirements: desired.requirements.clone(),
            installation: desired.installation,
            observed_activation: ledger.map(|ledger| ledger.current.selection()),
            app_id: desired.app.id.clone(),
            target: desired.target,
            scope: desired.scope,
            desired_sha256: Some(fingerprint(desired)),
            observed_transaction: ledger.map(|ledger| ledger.commit.transaction),
        }
    }

    pub(crate) fn recorded(ledger: &InstallationRecord) -> Self {
        Self {
            requirements: ledger.current.requirements.clone(),
            installation: ledger.id,
            observed_activation: Some(ledger.current.selection()),
            app_id: ledger.app_id.clone(),
            target: ledger.target,
            scope: ledger.scope,
            desired_sha256: None,
            observed_transaction: Some(ledger.commit.transaction),
        }
    }
}

impl ExecutionPlan {
    pub fn validate_desired(
        &self,
        desired: Option<&crate::BoundDeployment>,
    ) -> Result<&DeploymentBinding, BindingMismatch> {
        let binding = self.binding.as_ref().ok_or(BindingMismatch)?;
        if let Some(desired) = desired {
            desired.validate_resolved().map_err(|_| BindingMismatch)?;
        }
        match (self.transition, desired) {
            (crate::DeploymentTransition::Deploy, Some(desired))
                if binding.requirements == desired.requirements
                    && binding.app_id == desired.app.id
                    && binding.installation == desired.installation
                    && binding.target == desired.target
                    && binding.scope == desired.scope
                    && binding.desired_sha256 == Some(fingerprint(desired)) => {}
            (crate::DeploymentTransition::Remove, desired)
                if binding.desired_sha256.is_none()
                    && desired.is_none_or(|desired| {
                        binding.app_id == desired.app.id
                            && binding.target == desired.target
                            && binding.scope == desired.scope
                    }) => {}
            (crate::DeploymentTransition::Maintain, None) if binding.desired_sha256.is_none() => {}
            _ => return Err(BindingMismatch),
        }
        if let Some(desired) = desired
            && !self.transition.is_remove()
        {
            let mut keys = std::collections::BTreeSet::new();
            for effect in &self.files {
                let resource = desired
                    .fixed_files()
                    .find(|file| file.key == effect.key)
                    .ok_or(BindingMismatch)?;
                if !keys.insert(&effect.key)
                    || &effect.destination != resource.destination()
                    || effect.source_relative != resource.source_relative
                    || effect.expected_sha256 != resource.sha256
                    || effect.expected_size != resource.size
                    || effect.privilege != resource.privilege
                    || effect.executable != resource.executable
                    || resource.placement.fixed_replacement() != Some(effect.replacement)
                {
                    return Err(BindingMismatch);
                }
            }
            for effect in &self.services {
                let resource = desired
                    .services
                    .iter()
                    .find(|service| service.key == effect.key)
                    .ok_or(BindingMismatch)?;
                if !keys.insert(&effect.key)
                    || effect.id != resource.id.as_str()
                    || effect.registration
                        != zup_core::NativeServiceRegistration::for_service(
                            &resource.execution,
                            desired.installation,
                        )
                    || effect.name != resource.name.as_str()
                    || effect.display_name
                        != resource
                            .display_name
                            .as_ref()
                            .unwrap_or(&resource.name)
                            .as_str()
                    || effect.command != resource.command
                    || effect.lifecycle.resolved(&effect.execution)
                        != resource.lifecycle.resolved(&resource.execution)
                    || effect.readiness != resource.readiness
                    || effect.execution != resource.execution
                    || effect.failure != resource.failure
                    || effect.requires_files != resource.requires_files
                    || effect.requires_services != resource.requires_services
                    || effect.dependency_names != resource.dependency_names
                    || effect.start != resource.start
                    || effect.privilege != resource.privilege
                {
                    return Err(BindingMismatch);
                }
            }
            for effect in &self.launchers {
                let resource = desired
                    .launchers
                    .iter()
                    .find(|launcher| launcher.key == effect.key)
                    .ok_or(BindingMismatch)?;
                if !keys.insert(&effect.key)
                    || effect.launcher_path != resource.launcher_path
                    || effect.icon
                        != desired
                            .maintenance
                            .as_ref()
                            .and_then(|generation| generation.maintenance_runtime())
                    || effect.target != resource.target
                    || effect.arguments != resource.arguments
                    || effect.working_directory != resource.working_directory
                    || effect.privilege != resource.privilege
                {
                    return Err(BindingMismatch);
                }
            }
            for effect in &self.path_entries {
                let resource = desired
                    .path_entries
                    .iter()
                    .find(|entry| entry.key == effect.key)
                    .ok_or(BindingMismatch)?;
                if !keys.insert(&effect.key)
                    || effect.value != resource.value
                    || effect.scope != resource.scope
                    || effect.privilege != resource.privilege
                {
                    return Err(BindingMismatch);
                }
            }
            for effect in &self.protocols {
                let resource = desired
                    .protocols
                    .iter()
                    .find(|protocol| protocol.key == effect.key)
                    .ok_or(BindingMismatch)?;
                if !keys.insert(&effect.key)
                    || effect.scheme != resource.scheme
                    || effect.command != resource.command
                    || effect.scope != resource.scope
                    || effect.privilege != resource.privilege
                {
                    return Err(BindingMismatch);
                }
            }
            for effect in &self.file_associations {
                let resource = desired
                    .file_associations
                    .iter()
                    .find(|association| association.key == effect.key)
                    .ok_or(BindingMismatch)?;
                if !keys.insert(&effect.key)
                    || effect.id != resource.id.as_str()
                    || effect.extension != resource.extension.as_str()
                    || effect.description != resource.description
                    || effect.command != resource.command
                    || effect.scope != resource.scope
                    || effect.privilege != resource.privilege
                {
                    return Err(BindingMismatch);
                }
            }
        }
        Ok(binding)
    }

    pub fn validate_generation(
        &self,
        ledger: Option<&InstallationRecord>,
    ) -> Result<(), BindingMismatch> {
        let binding = self.binding.as_ref().ok_or(BindingMismatch)?;
        if binding.observed_transaction != ledger.map(|ledger| ledger.commit.transaction)
            || binding.observed_activation != ledger.map(|ledger| ledger.current.selection())
            || ledger.is_some_and(|ledger| {
                binding.installation != ledger.id
                    || binding.app_id != ledger.app_id
                    || binding.target != ledger.target
                    || binding.scope != ledger.scope
            })
        {
            return Err(BindingMismatch);
        }
        Ok(())
    }
}

fn fingerprint(desired: &crate::BoundDeployment) -> Sha256Digest {
    // This contract has no map keys or fallible custom serializers.
    hash_bytes(&serde_json::to_vec(desired).expect("deployment contract serializes"))
}
