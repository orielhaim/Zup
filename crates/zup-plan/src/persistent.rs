use serde::{Deserialize, Serialize};
use zup_core::{NativeObjectIdentity, PersistentDirectory, Privilege, TargetPath, TransactionId};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ServiceDataPrincipal {
    WindowsService { name: String },
    LinuxUser { name: String, uid: u32 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceDataWriter {
    pub service: zup_core::ServiceId,
    pub principal: ServiceDataPrincipal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FilesystemDataBinding {
    pub path: TargetPath,
    pub writers: Vec<ServiceDataWriter>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SystemdStateBinding {
    pub application: zup_core::AppId,
    pub service: zup_core::ServiceId,
    pub unit: String,
    pub name: zup_core::RelativePath,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PersistentBinding {
    Filesystem(FilesystemDataBinding),
    SystemdState(SystemdStateBinding),
}

impl PersistentBinding {
    pub fn path(&self, target: zup_core::Target) -> Result<TargetPath, zup_core::TargetPathError> {
        match self {
            Self::Filesystem(binding) => Ok(binding.path.clone()),
            Self::SystemdState(binding) => {
                TargetPath::new(target, format!("/var/lib/{}", binding.name))
            }
        }
    }
}

pub fn service_state_name(
    application: &zup_core::AppId,
    service: &zup_core::ServiceId,
    id: &zup_core::ResourceId,
) -> zup_core::RelativePath {
    let mut identity = b"zup:service-state:1\0".to_vec();
    identity.extend(
        serde_json::to_vec(&(application, service, id)).expect("service state identity serializes"),
    );
    zup_core::RelativePath::new(format!("zup-{}", zup_core::hash_bytes(&identity)))
        .expect("native state namespace is a single generated name")
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PersistentOwnership {
    Filesystem {
        binding: FilesystemDataBinding,
        object: NativeObjectIdentity,
    },
    SystemdState {
        binding: SystemdStateBinding,
    },
}

impl PersistentOwnership {
    pub fn binding(&self) -> PersistentBinding {
        match self {
            Self::Filesystem { binding, .. } => PersistentBinding::Filesystem(binding.clone()),
            Self::SystemdState { binding } => PersistentBinding::SystemdState(binding.clone()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersistentClaimSelection {
    pub installation: zup_core::InstallationId,
    pub directory: TargetPath,
    pub owners: std::collections::BTreeMap<zup_core::ResourceId, OwnedPersistentDirectory>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnedPersistentDirectory {
    pub installation: zup_core::InstallationId,
    pub directory: PersistentDirectory<TargetPath>,
    pub ownership: PersistentOwnership,
    pub origin: TransactionId,
}

impl OwnedPersistentDirectory {
    pub fn validate(
        &self,
        application: &zup_core::AppId,
        target: zup_core::Target,
        scope: zup_core::SelectedScope,
    ) -> Result<(), String> {
        if matches!(&self.ownership, PersistentOwnership::SystemdState { binding } if &binding.application != application)
            || matches!(self.ownership, PersistentOwnership::Filesystem { object, .. } if object.operating_system() != target.operating_system())
        {
            return Err("persistent ownership differs from the application or target".into());
        }
        PersistentDirectoryIntent {
            installation: self.installation,
            directory: self.directory.clone(),
            binding: self.ownership.binding(),
            previous: None,
            privilege: scope.authorization(),
        }
        .validate(target, scope)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersistentDirectoryIntent {
    pub installation: zup_core::InstallationId,
    pub directory: PersistentDirectory<TargetPath>,
    pub binding: PersistentBinding,
    pub previous: Option<OwnedPersistentDirectory>,
    pub privilege: Privilege,
}

impl PersistentDirectoryIntent {
    pub fn matches_requirement(
        &self,
        required: &PersistentDirectory,
        target: zup_core::Target,
    ) -> bool {
        required
            .map_path(|path| TargetPath::new(target, path.to_string()))
            .is_ok_and(|directory| directory == self.directory)
    }
    pub fn validate(
        &self,
        target: zup_core::Target,
        scope: zup_core::SelectedScope,
    ) -> Result<(), String> {
        if self.privilege != scope.authorization() {
            return Err("persistent directory authority differs from its installation".into());
        }
        match (&self.directory.location, &self.binding) {
            (
                zup_core::PersistentLocation::Path { path, access },
                PersistentBinding::Filesystem(binding),
            ) => {
                if path.target() != &target || !path.equivalent(&binding.path) {
                    return Err("persistent path binding differs from its declaration".into());
                }
                let expected: std::collections::BTreeSet<_> = match access {
                    zup_core::PersistentAccess::Owner => Default::default(),
                    zup_core::PersistentAccess::Services { services } => {
                        services.services().collect()
                    }
                };
                if expected
                    != binding
                        .writers
                        .iter()
                        .map(|writer| &writer.service)
                        .collect()
                {
                    return Err("persistent service grants differ from their declaration".into());
                }
            }
            (
                zup_core::PersistentLocation::Service { service },
                PersistentBinding::Filesystem(binding),
            ) => {
                if scope != zup_core::SelectedScope::Machine
                    || binding.path.target() != &target
                    || binding.writers.len() != 1
                    || binding.writers[0].service != *service
                {
                    return Err("native service data requires its sole authorized writer".into());
                }
            }
            (
                zup_core::PersistentLocation::Service { service },
                PersistentBinding::SystemdState(binding),
            ) => {
                if target.operating_system() != zup_core::TargetOperatingSystem::Linux
                    || scope != zup_core::SelectedScope::Machine
                    || binding.service != *service
                    || !binding.unit.ends_with(".service")
                    || binding.unit.len() > 256
                    || binding
                        .unit
                        .chars()
                        .any(|c| c.is_control() || matches!(c, '/' | '\\'))
                    || binding.name
                        != service_state_name(&binding.application, service, &self.directory.id)
                {
                    return Err(
                        "systemd state claim differs from its installation and service".into(),
                    );
                }
            }
            _ => return Err("persistent placement cannot change provisioning strategies".into()),
        }
        if let PersistentBinding::Filesystem(binding) = &self.binding {
            let mut services = std::collections::BTreeSet::new();
            if binding.writers.len() > 256
                || !binding.writers.is_empty() && scope != zup_core::SelectedScope::Machine
            {
                return Err("persistent service grants require bounded machine authority".into());
            }
            for writer in &binding.writers {
                if !services.insert(&writer.service) {
                    return Err("persistent service grants repeat a service".into());
                }
                match &writer.principal {
                    ServiceDataPrincipal::WindowsService { name }
                        if target.is_windows()
                            && !name.is_empty()
                            && name.encode_utf16().count() <= 256
                            && !name
                                .chars()
                                .any(|c| c.is_control() || matches!(c, '/' | '\\')) => {}
                    ServiceDataPrincipal::LinuxUser { name, uid }
                        if target.operating_system() == zup_core::TargetOperatingSystem::Linux
                            && *uid != u32::MAX
                            && zup_core::ServiceExecution::OpenrcUser {
                                user: name.clone(),
                                group: None,
                            }
                            .validate_for(target)
                            .is_ok() => {}
                    _ => return Err("persistent writer identity differs from the target".into()),
                }
            }
        }
        if self.previous.as_ref().is_some_and(|previous| {
            previous.installation != self.installation
                || previous.directory.id != self.directory.id
                || previous.directory.location != self.directory.location
                || previous.ownership.binding() != self.binding
                || matches!(previous.ownership, PersistentOwnership::Filesystem { object, .. } if object.operating_system() != target.operating_system())
                || !self
                    .directory
                    .compatibility
                    .accepts(&previous.directory.compatibility)
        }) {
            return Err("persistent identity, placement, writer policy, or data format differs from its recorded owner; use a new data resource for a permission migration".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PersistentDirectoryStage {
    Prepare,
    Access,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersistentDirectoryOperation {
    pub intent: PersistentDirectoryIntent,
    pub stage: PersistentDirectoryStage,
}

impl PersistentDirectoryOperation {
    pub fn validate(
        &self,
        target: zup_core::Target,
        scope: zup_core::SelectedScope,
    ) -> Result<(), String> {
        self.intent.validate(target, scope)
    }
    pub fn id(&self) -> crate::OperationId {
        let stage = match self.stage {
            PersistentDirectoryStage::Prepare => "prepare",
            PersistentDirectoryStage::Access => "access",
        };
        crate::OperationId::new(format!("data_{stage}:{}", self.intent.directory.id))
    }
}

pub fn bind_service_arguments(
    target: &mut crate::BoundDeployment,
    bindings: &std::collections::BTreeMap<zup_core::ResourceId, PersistentBinding>,
) -> Result<(), crate::ContractError> {
    let service_names: std::collections::BTreeMap<_, _> = target
        .services
        .iter()
        .map(|service| (service.id.clone(), service.name.to_string()))
        .collect();
    for service in &mut target.services {
        for argument in &mut service.command.arguments {
            match zup_core::ServiceArgument::parse(argument)
                .map_err(|error| crate::ContractError::Capability(error.to_string()))?
            {
                zup_core::ServiceArgument::Literal(_) => {}
                zup_core::ServiceArgument::NativeName(id) => {
                    *argument = service_names.get(&id).cloned().ok_or_else(|| {
                        crate::ContractError::Capability(format!(
                            "service {} references unavailable service {id}",
                            service.id
                        ))
                    })?;
                }
                zup_core::ServiceArgument::DataDirectory(id) => {
                    if !target.data.iter().any(|data| {
                        data.id == id
                            && data.location.services().any(|writer| writer == &service.id)
                    }) {
                        return Err(crate::ContractError::Capability(format!(
                            "service {} cannot reference undeclared or inaccessible data {id}",
                            service.id
                        )));
                    }
                    let binding = bindings.get(&id).ok_or_else(|| {
                        crate::ContractError::Capability(format!(
                            "data {id} has no native path binding"
                        ))
                    })?;
                    *argument = binding
                        .path(target.target)
                        .map_err(|error| crate::ContractError::Capability(error.to_string()))?
                        .to_string();
                }
                zup_core::ServiceArgument::WindowsJob(id) => {
                    if id != service.id
                        || !matches!(
                            service.execution,
                            zup_core::ServiceExecution::WindowsJob { .. }
                        )
                    {
                        return Err(crate::ContractError::Capability(
                            "a Windows job argument must reference its own SCM service".into(),
                        ));
                    }
                    *argument = zup_core::service_job_name(target.installation, &id);
                }
            }
        }
    }
    Ok(())
}
