use serde::{Deserialize, Serialize};
use zup_core::{
    InstallationId, NativeServiceRegistration, Privilege, ResourceKey, Sha256Digest, Target,
    hash_bytes,
};

use crate::{
    DeploymentPlanError, IntegrationChangeKind, OperationId, OwnedService, ServiceOperation,
    ServiceState,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceActivation {
    Declared,
    Inhibited,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum NativeServiceOperation {
    Configure {
        service: Box<ServiceOperation>,
        #[serde(deserialize_with = "serde::Deserialize::deserialize")]
        previous: Option<Box<OwnedService>>,
        activation: ServiceActivation,
    },
    Remove {
        key: ResourceKey,
        service: Box<OwnedService>,
        activation: ServiceActivation,
    },
}

impl NativeServiceOperation {
    pub fn native_name(&self) -> &str {
        match self {
            Self::Configure { service, .. } => &service.name,
            Self::Remove { service, .. } => &service.name,
        }
    }

    pub fn key(&self) -> &ResourceKey {
        match self {
            Self::Configure { service, .. } => &service.key,
            Self::Remove { key, .. } => key,
        }
    }

    pub fn source_key(&self) -> Option<ResourceKey> {
        if self.registration() != NativeServiceRegistration::SourceFile {
            return None;
        }
        let ResourceKey::Service { id } = self.key() else {
            return None;
        };
        Some(ResourceKey::File {
            id: zup_core::FileResourceId::Native {
                id: zup_core::NativeFileId::ServiceSource { id: id.clone() },
            },
        })
    }

    pub fn source_node<'a>(
        &self,
        nodes: &'a [crate::DeploymentNode],
    ) -> Result<Option<&'a crate::DeploymentNode>, DeploymentPlanError> {
        let Some(source) = self.source_key() else {
            return Ok(None);
        };
        let matches: Vec<_> = nodes
            .iter()
            .filter(|node| match (&node.kind, self) {
                (
                    crate::NodeKind::FileMutation { key, .. } | crate::NodeKind::RetainFile { key },
                    Self::Configure { .. },
                ) => key == &source,
                (crate::NodeKind::FileRemoval { key }, Self::Remove { .. }) => key == &source,
                _ => false,
            })
            .collect();
        if matches.len() != 1 || matches[0].meta.privilege != Some(Privilege::System) {
            return Err(DeploymentPlanError::InvalidNode {
                id: self.id().to_string(),
                reason: "native service requires exactly one owned source file effect".into(),
            });
        }
        Ok(Some(matches[0]))
    }

    pub fn id(&self) -> OperationId {
        OperationId::resource(
            match self {
                Self::Configure { .. } => "service_configure",
                Self::Remove { .. } => "service_remove",
            },
            self.key(),
        )
    }

    pub fn fingerprint(&self) -> Sha256Digest {
        let mut preimage = b"zup:native-service:1\0".to_vec();
        preimage.extend(serde_json::to_vec(self).expect("native service intent serializes"));
        hash_bytes(&preimage)
    }

    pub fn contract(&self) -> OwnedService {
        match self {
            Self::Configure { service, .. } => OwnedService {
                registration: service.registration,
                name: service.name.clone(),
                timeouts: service.lifecycle.timeouts_for(&service.execution),
                privilege: service.privilege,
                requires_files: service.requires_files.clone(),
                requires_services: service.requires_services.clone(),
                installed: Box::new(service.desired_configuration()),
            },
            Self::Remove { service, .. } => service.as_ref().clone(),
        }
    }

    pub fn configurations(&self) -> (Option<ServiceState>, ServiceState) {
        let (before, after, activation) = match self {
            Self::Configure {
                service,
                activation,
                ..
            } => (
                service.previous.configuration(),
                service.desired_configuration(),
                activation,
            ),
            Self::Remove {
                service,
                activation,
                ..
            } => (
                Some(service.installed.as_ref().clone()),
                ServiceState::Absent,
                activation,
            ),
        };
        if *activation == ServiceActivation::Declared {
            return (before, after);
        }
        (
            before.map(|state| crate::disabled_startup(&state).unwrap_or(state)),
            crate::disabled_startup(&after).unwrap_or(after),
        )
    }

    pub fn privilege(&self) -> Privilege {
        match self {
            Self::Configure { service, .. } => service.privilege,
            Self::Remove { service, .. } => service.privilege,
        }
    }

    pub fn registration(&self) -> NativeServiceRegistration {
        match self {
            Self::Configure { service, .. } => service.registration,
            Self::Remove { service, .. } => service.registration,
        }
    }

    pub fn validate(
        &self,
        installation: InstallationId,
        target: Target,
    ) -> Result<(), DeploymentPlanError> {
        let invalid = |reason: &str| DeploymentPlanError::InvalidNode {
            id: self.id().to_string(),
            reason: reason.to_owned(),
        };
        if !matches!(self.key(), ResourceKey::Service { .. })
            || self.key().canonical_for(&target) != *self.key()
            || !self.registration().belongs_to(installation, target)
            || self.privilege() != Privilege::System
        {
            return Err(invalid(
                "native service identity, installation, platform, or authority is inconsistent",
            ));
        }
        let contract = self.contract();
        if contract.name.is_empty()
            || contract.name.encode_utf16().count() > 256
            || contract
                .name
                .chars()
                .any(|character| character.is_control() || matches!(character, '/' | '\\'))
            || contract.requires_files.len() > 256
            || contract.requires_services.len() > 256
            || contract
                .requires_files
                .iter()
                .any(|path| path.target() != &target)
            || contract
                .requires_files
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                != contract.requires_files.len()
            || contract
                .requires_services
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                != contract.requires_services.len()
        {
            return Err(invalid(
                "native service name or resource requirements are invalid",
            ));
        }
        let ServiceState::Registration {
            command,
            execution,
            native_timeouts,
            failure,
            readiness,
            ..
        } = contract.installed.as_ref()
        else {
            return Err(invalid(
                "native service requires a registered configuration",
            ));
        };
        if command.executable.target() != &target
            || command
                .arguments
                .iter()
                .any(|argument| argument.contains('\0'))
            || execution
                .validate_policies(*failure, *readiness, target)
                .is_err()
            || contract.timeouts.validate_for(execution).is_err()
            || contract.timeouts.native() != *native_timeouts
        {
            return Err(invalid(
                "native service execution or timeout contract is invalid",
            ));
        }
        if let Self::Configure {
            service,
            previous: owned,
            ..
        } = self
        {
            if service.conflict.is_some()
                || !matches!(
                    service.kind,
                    IntegrationChangeKind::Create
                        | IntegrationChangeKind::NoOp
                        | IntegrationChangeKind::UpdateOwned
                        | IntegrationChangeKind::RestoreOwned
                        | IntegrationChangeKind::RepairOwned
                )
            {
                return Err(invalid("native service has unresolved conflicts or drift"));
            }
            let previous = service.previous.configuration();
            if let Some(owned) = owned
                && (owned.registration != contract.registration
                    || owned.privilege != contract.privilege
                    || owned.name != contract.name
                    || !matches!(owned.installed.as_ref(), ServiceState::Registration { command, .. } if command.executable.target() == &target))
            {
                return Err(invalid(
                    "previous service ownership does not match its registration",
                ));
            }
            let valid = match service.kind {
                IntegrationChangeKind::Create => {
                    owned.is_none() && previous == Some(ServiceState::Absent)
                }
                IntegrationChangeKind::RestoreOwned => {
                    owned.is_some() && previous == Some(ServiceState::Absent)
                }
                IntegrationChangeKind::NoOp => {
                    owned
                        .as_ref()
                        .is_some_and(|owned| owned.installed == contract.installed)
                        && previous.as_ref() == Some(contract.installed.as_ref())
                }
                IntegrationChangeKind::UpdateOwned => owned
                    .as_ref()
                    .is_some_and(|owned| previous.as_ref() == Some(owned.installed.as_ref())),
                IntegrationChangeKind::RepairOwned => {
                    owned.is_some() && previous != Some(ServiceState::Absent)
                }
                _ => false,
            };
            if !valid {
                return Err(invalid(
                    "native service change does not match its observed configuration",
                ));
            }
            service
                .lifecycle
                .validate(service.start, &service.execution)
                .map_err(invalid)?;
        }
        Ok(())
    }
}
