use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use zup_core::{
    App, ComponentId, FileAssociationId, FileExtension, LauncherLocation, NonEmptyString,
    PluginBinding, PluginId, PrerequisitePackage, Privilege, ProtocolScheme, RelativePath,
    ResourceKey, SelectedScope, ServiceId, ServiceStart, Sha256Digest, TargetTriple, Template,
};

use crate::error::PlanError;
use crate::plan_types::{InstallPlan, PlanSummary};
use crate::resolve::resolve_template;
use crate::resources::{
    PlannedFile, PlannedFileAssociation, PlannedLauncher, PlannedPathEntry, PlannedProtocol,
    PlannedService,
};

pub const MAX_PLUGIN_RESOURCES: usize = 4096;
pub const MAX_PLUGIN_GENERATED_FILE_BYTES: usize = 1024 * 1024;
pub const MAX_PLUGIN_GENERATED_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_PLUGIN_TEXT_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_PLUGIN_STRING_BYTES: usize = 64 * 1024;
pub const MAX_PLUGIN_ARGUMENTS: usize = 256;
pub const MAX_PLUGIN_ARGUMENT_BYTES: usize = 1024 * 1024;
pub const MAX_PLUGIN_ERROR_BYTES: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginPlanningContext {
    pub app: App,
    pub install_directory: Template,
    pub scope: SelectedScope,
    pub selected_components: Vec<ComponentId>,
    pub target: TargetTriple,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "resource", rename_all = "snake_case", deny_unknown_fields)]
pub enum PluginResource {
    GeneratedFile {
        destination: String,
        contents: Vec<u8>,
    },
    Launcher {
        location: LauncherLocation,
        name: String,
        target: String,
        arguments: Vec<String>,
        working_directory: Option<String>,
    },
    PathEntry {
        value: String,
    },
    Service {
        id: String,
        name: String,
        display_name: Option<String>,
        binary: String,
        arguments: Vec<String>,
        start: ServiceStart,
    },
    Protocol {
        scheme: String,
        executable: String,
        args: Vec<String>,
    },
    FileAssociation {
        extension: String,
        id: String,
        description: Option<String>,
        executable: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct PluginResourceProposal {
    pub resources: Vec<PluginResource>,
}

impl PluginResourceProposal {
    pub fn new(resources: Vec<PluginResource>) -> Self {
        Self { resources }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error, Serialize, Deserialize)]
pub enum PluginFailure {
    #[error("plugin rejected planning with `{code}`: {message}")]
    Rejected { code: String, message: String },
    #[error("plugin planning was cancelled")]
    Cancelled,
    #[error("plugin trapped: {message}")]
    Trap { message: String },
    #[error("plugin exhausted its fuel budget")]
    FuelExhausted,
    #[error("plugin exceeded a sandbox resource limit")]
    MemoryLimit,
    #[error("plugin planning timed out")]
    Timeout,
    #[error("plugin output exceeded its limit: {message}")]
    OutputLimit { message: String },
    #[error("plugin returned invalid output: {message}")]
    InvalidOutput { message: String },
    #[error("plugin target `{found}` does not match expected target `{expected}`")]
    TargetMismatch {
        expected: TargetTriple,
        found: TargetTriple,
    },
    #[error("plugin invocation failed internally: {message}")]
    Internal { message: String },
}

impl PluginFailure {
    pub fn rejected(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Rejected {
            code: sanitize_error(code.into()),
            message: sanitize_error(message.into()),
        }
    }

    pub fn trap(message: impl Into<String>) -> Self {
        Self::Trap {
            message: sanitize_error(message.into()),
        }
    }

    pub fn fuel_exhausted() -> Self {
        Self::FuelExhausted
    }

    pub fn memory_limit() -> Self {
        Self::MemoryLimit
    }

    pub fn timeout() -> Self {
        Self::Timeout
    }

    pub fn output_limit(message: impl Into<String>) -> Self {
        Self::OutputLimit {
            message: sanitize_error(message.into()),
        }
    }

    pub fn invalid_output(message: impl Into<String>) -> Self {
        Self::InvalidOutput {
            message: sanitize_error(message.into()),
        }
    }

    pub fn target_mismatch(expected: TargetTriple, found: TargetTriple) -> Self {
        Self::TargetMismatch { expected, found }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::Internal {
            message: sanitize_error(message.into()),
        }
    }
}

impl From<String> for PluginFailure {
    fn from(message: String) -> Self {
        Self::internal(message)
    }
}

impl From<&str> for PluginFailure {
    fn from(message: &str) -> Self {
        Self::internal(message)
    }
}

pub fn sanitize_error(message: String) -> String {
    let mut output = String::with_capacity(MAX_PLUGIN_ERROR_BYTES);
    for character in message.chars() {
        if output.len() >= MAX_PLUGIN_ERROR_BYTES {
            break;
        }
        if character.is_control() {
            output.push(' ');
        } else if character.len_utf8() <= MAX_PLUGIN_ERROR_BYTES - output.len() {
            output.push(character);
        } else {
            break;
        }
    }
    output
}

pub trait CancellationQuery: Send + Sync {
    fn is_cancelled(&self) -> bool;
}

impl<F> CancellationQuery for F
where
    F: Fn() -> bool + Send + Sync + ?Sized,
{
    fn is_cancelled(&self) -> bool {
        self()
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct NeverCancelled;

impl CancellationQuery for NeverCancelled {
    fn is_cancelled(&self) -> bool {
        false
    }
}

pub trait PluginExecutor {
    fn target(&self) -> &TargetTriple;

    fn plan(
        &mut self,
        binding: &PluginBinding,
        context: &PluginPlanningContext,
        cancellation: &dyn CancellationQuery,
    ) -> Result<PluginResourceProposal, PluginFailure>;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GeneratedFile {
    pub source_relative: RelativePath,
    pub destination: Template,
    pub size: u64,
    pub sha256: Sha256Digest,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedInstallation {
    pub plan: InstallPlan,
    pub generated_files: Vec<GeneratedFile>,
}

#[derive(Clone)]
struct CollisionOrigin {
    plugin_id: Option<PluginId>,
    resource: String,
}

pub(crate) struct CollisionIndex {
    files: BTreeMap<String, CollisionOrigin>,
    file_sources: BTreeMap<String, CollisionOrigin>,
    launchers: BTreeMap<String, CollisionOrigin>,
    paths: BTreeMap<String, CollisionOrigin>,
    services: BTreeMap<String, CollisionOrigin>,
    protocols: BTreeMap<String, CollisionOrigin>,
    file_association_ids: BTreeMap<String, CollisionOrigin>,
    file_association_extensions: BTreeMap<String, CollisionOrigin>,
}

impl CollisionIndex {
    pub(crate) fn from_plan(plan: &InstallPlan) -> Result<Self, PlanError> {
        let mut index = Self {
            files: BTreeMap::new(),
            file_sources: BTreeMap::new(),
            launchers: BTreeMap::new(),
            paths: BTreeMap::new(),
            services: BTreeMap::new(),
            protocols: BTreeMap::new(),
            file_association_ids: BTreeMap::new(),
            file_association_extensions: BTreeMap::new(),
        };

        for file in &plan.files {
            index.insert_manifest_file(
                &file.destination.to_string(),
                file.source_relative.as_str(),
            )?;
        }
        for launcher in &plan.launchers {
            index.insert_launcher(
                &launcher.location.to_string(),
                launcher.name.as_ref(),
                None,
                "manifest launcher",
            )?;
        }
        for path in &plan.path_entries {
            index.insert_path(&path.value.to_string(), None, "manifest search-path entry")?;
        }
        for service in &plan.services {
            index.insert_service(service.id.as_ref(), None, "manifest service")?;
        }
        for protocol in &plan.protocols {
            index.insert_protocol(protocol.scheme.as_ref(), None, "manifest protocol")?;
        }
        for file_association in &plan.file_associations {
            index.insert_file_association(
                file_association.id.as_ref(),
                &file_association.extension.to_string(),
                None,
                "manifest file association",
            )?;
        }
        Ok(index)
    }

    fn insert_manifest_file(&mut self, destination: &str, source: &str) -> Result<(), PlanError> {
        let identity = logical_identity(destination);
        if let Some(existing) = self.files.get(&identity) {
            return Err(collision_error(None, "manifest file", &identity, existing));
        }
        self.files.insert(
            identity,
            CollisionOrigin {
                plugin_id: None,
                resource: "manifest file".to_owned(),
            },
        );
        self.file_sources
            .entry(logical_identity(source))
            .or_insert_with(|| CollisionOrigin {
                plugin_id: None,
                resource: "manifest file".to_owned(),
            });
        Ok(())
    }

    fn insert_file(
        &mut self,
        destination: &str,
        source: Option<&str>,
        plugin_id: Option<&PluginId>,
        resource: &str,
    ) -> Result<(), PlanError> {
        let identity = logical_identity(destination);
        if let Some(existing) = self.files.get(&identity) {
            return Err(collision_error(plugin_id, resource, &identity, existing));
        }
        self.files.insert(
            identity,
            CollisionOrigin {
                plugin_id: plugin_id.cloned(),
                resource: resource.to_owned(),
            },
        );
        if let Some(source) = source {
            let source_identity = logical_identity(source);
            if let Some(existing) = self.file_sources.get(&source_identity) {
                return Err(collision_error(
                    plugin_id,
                    "generated file source",
                    &source_identity,
                    existing,
                ));
            }
            self.file_sources.insert(
                source_identity,
                CollisionOrigin {
                    plugin_id: plugin_id.cloned(),
                    resource: resource.to_owned(),
                },
            );
        }
        Ok(())
    }

    fn insert_launcher(
        &mut self,
        location: &str,
        name: &str,
        plugin_id: Option<&PluginId>,
        resource: &str,
    ) -> Result<(), PlanError> {
        let identity = format!("launcher:{}:{}", location, logical_identity(name));
        if let Some(existing) = self.launchers.get(&identity) {
            return Err(collision_error(plugin_id, resource, &identity, existing));
        }
        self.launchers.insert(
            identity,
            CollisionOrigin {
                plugin_id: plugin_id.cloned(),
                resource: resource.to_owned(),
            },
        );
        Ok(())
    }

    fn insert_path(
        &mut self,
        value: &str,
        plugin_id: Option<&PluginId>,
        resource: &str,
    ) -> Result<(), PlanError> {
        let identity = logical_identity(value);
        if let Some(existing) = self.paths.get(&identity) {
            return Err(collision_error(plugin_id, resource, &identity, existing));
        }
        self.paths.insert(
            identity,
            CollisionOrigin {
                plugin_id: plugin_id.cloned(),
                resource: resource.to_owned(),
            },
        );
        Ok(())
    }

    fn insert_service(
        &mut self,
        id: &str,
        plugin_id: Option<&PluginId>,
        resource: &str,
    ) -> Result<(), PlanError> {
        let identity = logical_identity(id);
        if let Some(existing) = self.services.get(&identity) {
            return Err(collision_error(plugin_id, resource, &identity, existing));
        }
        self.services.insert(
            identity,
            CollisionOrigin {
                plugin_id: plugin_id.cloned(),
                resource: resource.to_owned(),
            },
        );
        Ok(())
    }

    fn insert_protocol(
        &mut self,
        scheme: &str,
        plugin_id: Option<&PluginId>,
        resource: &str,
    ) -> Result<(), PlanError> {
        let identity = logical_identity(scheme);
        if let Some(existing) = self.protocols.get(&identity) {
            return Err(collision_error(plugin_id, resource, &identity, existing));
        }
        self.protocols.insert(
            identity,
            CollisionOrigin {
                plugin_id: plugin_id.cloned(),
                resource: resource.to_owned(),
            },
        );
        Ok(())
    }

    fn insert_file_association(
        &mut self,
        id: &str,
        extension: &str,
        plugin_id: Option<&PluginId>,
        resource: &str,
    ) -> Result<(), PlanError> {
        let id_identity = format!("file-association:{}", logical_identity(id));
        if let Some(existing) = self.file_association_ids.get(&id_identity) {
            return Err(collision_error(
                plugin_id,
                "file association id",
                &id_identity,
                existing,
            ));
        }
        let extension_identity =
            format!("file-association-extension:{}", logical_identity(extension));
        if let Some(existing) = self.file_association_extensions.get(&extension_identity) {
            return Err(collision_error(
                plugin_id,
                "file association extension",
                &extension_identity,
                existing,
            ));
        }
        self.file_association_ids.insert(
            id_identity,
            CollisionOrigin {
                plugin_id: plugin_id.cloned(),
                resource: resource.to_owned(),
            },
        );
        self.file_association_extensions.insert(
            extension_identity,
            CollisionOrigin {
                plugin_id: plugin_id.cloned(),
                resource: resource.to_owned(),
            },
        );
        Ok(())
    }
}

struct GeneratedFileInput {
    destination: String,
    contents: Vec<u8>,
}

struct LauncherInput {
    location: LauncherLocation,
    name: String,
    target: String,
    arguments: Vec<String>,
    working_directory: Option<String>,
}

struct ServiceInput {
    id: String,
    name: String,
    display_name: Option<String>,
    binary: String,
    arguments: Vec<String>,
    start: ServiceStart,
}

struct ProtocolInput {
    scheme: String,
    executable: String,
    args: Vec<String>,
}

struct FileAssociationInput {
    extension: String,
    id: String,
    description: Option<String>,
    executable: String,
}

struct MergeContext<'a> {
    plan: &'a mut InstallPlan,
    generated_files: &'a mut Vec<GeneratedFile>,
    collisions: &'a mut CollisionIndex,
    binding: &'a PluginBinding,
    total_generated_bytes: &'a mut u64,
}

fn add_text_size(total: &mut u64, value: &str) -> Option<()> {
    let size = u64::try_from(value.len()).ok()?;
    *total = total.checked_add(size)?;
    Some(())
}

fn service_start_text(start: ServiceStart) -> &'static str {
    match start {
        ServiceStart::Automatic => "automatic",
        ServiceStart::Manual => "manual",
        ServiceStart::Disabled => "disabled",
    }
}

fn plugin_text_size(resources: &[PluginResource]) -> Option<u64> {
    let mut total = 0u64;
    for resource in resources {
        match resource {
            PluginResource::GeneratedFile { destination, .. } => {
                add_text_size(&mut total, destination)?;
            }
            PluginResource::Launcher {
                location,
                name,
                target,
                arguments,
                working_directory,
            } => {
                add_text_size(&mut total, &location.to_string())?;
                add_text_size(&mut total, name)?;
                add_text_size(&mut total, target)?;
                for argument in arguments {
                    add_text_size(&mut total, argument)?;
                }
                if let Some(working_directory) = working_directory {
                    add_text_size(&mut total, working_directory)?;
                }
            }
            PluginResource::PathEntry { value } => add_text_size(&mut total, value)?,
            PluginResource::Service {
                id,
                name,
                display_name,
                binary,
                arguments,
                start,
            } => {
                add_text_size(&mut total, id)?;
                add_text_size(&mut total, name)?;
                if let Some(display_name) = display_name {
                    add_text_size(&mut total, display_name)?;
                }
                add_text_size(&mut total, binary)?;
                for argument in arguments {
                    add_text_size(&mut total, argument)?;
                }
                add_text_size(&mut total, service_start_text(*start))?;
            }
            PluginResource::Protocol {
                scheme,
                executable,
                args,
            } => {
                add_text_size(&mut total, scheme)?;
                add_text_size(&mut total, executable)?;
                for argument in args {
                    add_text_size(&mut total, argument)?;
                }
            }
            PluginResource::FileAssociation {
                extension,
                id,
                description,
                executable,
            } => {
                add_text_size(&mut total, extension)?;
                add_text_size(&mut total, id)?;
                if let Some(description) = description {
                    add_text_size(&mut total, description)?;
                }
                add_text_size(&mut total, executable)?;
            }
        }
    }
    Some(total)
}

pub(crate) fn merge_plugin_proposal(
    plan: &mut InstallPlan,
    generated_files: &mut Vec<GeneratedFile>,
    collisions: &mut CollisionIndex,
    binding: &PluginBinding,
    proposal: PluginResourceProposal,
    total_resources: &mut usize,
    total_generated_bytes: &mut u64,
) -> Result<(), PlanError> {
    let plugin_id = &binding.id;
    if PluginId::new(plugin_id.as_str()).is_err() {
        return Err(PlanError::PluginResourceRejected {
            plugin_id: plugin_id.clone(),
            resource: "plugin id".to_owned(),
            reason: "plugin id is malformed".to_owned(),
        });
    }
    let text_size =
        plugin_text_size(&proposal.resources).ok_or_else(|| PlanError::PluginResourceLimit {
            plugin_id: plugin_id.clone(),
            resource: "text output".to_owned(),
            actual: u64::MAX,
            limit: MAX_PLUGIN_TEXT_BYTES as u64,
        })?;
    if text_size > MAX_PLUGIN_TEXT_BYTES as u64 {
        return Err(PlanError::PluginResourceLimit {
            plugin_id: plugin_id.clone(),
            resource: "text output".to_owned(),
            actual: text_size,
            limit: MAX_PLUGIN_TEXT_BYTES as u64,
        });
    }
    if proposal.resources.len() > MAX_PLUGIN_RESOURCES {
        return Err(PlanError::PluginResourceLimit {
            plugin_id: plugin_id.clone(),
            resource: "resources".to_owned(),
            actual: proposal.resources.len() as u64,
            limit: MAX_PLUGIN_RESOURCES as u64,
        });
    }
    let new_resource_count = total_resources
        .checked_add(proposal.resources.len())
        .ok_or_else(|| PlanError::PluginResourceLimit {
            plugin_id: plugin_id.clone(),
            resource: "resources".to_owned(),
            actual: u64::MAX,
            limit: MAX_PLUGIN_RESOURCES as u64,
        })?;
    if new_resource_count > MAX_PLUGIN_RESOURCES {
        return Err(PlanError::PluginResourceLimit {
            plugin_id: plugin_id.clone(),
            resource: "resources".to_owned(),
            actual: new_resource_count as u64,
            limit: MAX_PLUGIN_RESOURCES as u64,
        });
    }
    *total_resources = new_resource_count;
    let mut merge = MergeContext {
        plan,
        generated_files,
        collisions,
        binding,
        total_generated_bytes,
    };

    for (index, resource) in proposal.resources.into_iter().enumerate() {
        let resource_name = format!("resource[{index}]");
        match resource {
            PluginResource::GeneratedFile {
                destination,
                contents,
            } => {
                merge.merge_generated_file(
                    &resource_name,
                    GeneratedFileInput {
                        destination,
                        contents,
                    },
                )?;
            }
            PluginResource::Launcher {
                location,
                name,
                target,
                arguments,
                working_directory,
            } => {
                merge.merge_launcher(
                    &resource_name,
                    LauncherInput {
                        location,
                        name,
                        target,
                        arguments,
                        working_directory,
                    },
                )?;
            }
            PluginResource::PathEntry { value } => {
                merge.merge_path(&resource_name, value)?;
            }
            PluginResource::Service {
                id,
                name,
                display_name,
                binary,
                arguments,
                start,
            } => {
                merge.merge_service(
                    &resource_name,
                    ServiceInput {
                        id,
                        name,
                        display_name,
                        binary,
                        arguments,
                        start,
                    },
                )?;
            }
            PluginResource::Protocol {
                scheme,
                executable,
                args,
            } => {
                merge.merge_protocol(
                    &resource_name,
                    ProtocolInput {
                        scheme,
                        executable,
                        args,
                    },
                )?;
            }
            PluginResource::FileAssociation {
                extension,
                id,
                description,
                executable,
            } => {
                merge.merge_file_association(
                    &resource_name,
                    FileAssociationInput {
                        extension,
                        id,
                        description,
                        executable,
                    },
                )?;
            }
        }
    }
    Ok(())
}

impl MergeContext<'_> {
    fn merge_generated_file(
        &mut self,
        resource_name: &str,
        input: GeneratedFileInput,
    ) -> Result<(), PlanError> {
        let GeneratedFileInput {
            destination,
            contents,
        } = input;
        let plugin_id = &self.binding.id;
        check_string(plugin_id, &destination, resource_name, "destination", false)?;
        if contents.len() > MAX_PLUGIN_GENERATED_FILE_BYTES {
            return Err(PlanError::PluginResourceLimit {
                plugin_id: plugin_id.clone(),
                resource: resource_name.to_owned(),
                actual: contents.len() as u64,
                limit: MAX_PLUGIN_GENERATED_FILE_BYTES as u64,
            });
        }
        let size = u64::try_from(contents.len()).map_err(|_| PlanError::PluginResourceLimit {
            plugin_id: plugin_id.clone(),
            resource: resource_name.to_owned(),
            actual: u64::MAX,
            limit: MAX_PLUGIN_GENERATED_FILE_BYTES as u64,
        })?;
        let new_total = self
            .total_generated_bytes
            .checked_add(size)
            .ok_or_else(|| PlanError::PluginResourceLimit {
                plugin_id: plugin_id.clone(),
                resource: "generated bytes".to_owned(),
                actual: u64::MAX,
                limit: MAX_PLUGIN_GENERATED_BYTES as u64,
            })?;
        if new_total > MAX_PLUGIN_GENERATED_BYTES as u64 {
            return Err(PlanError::PluginResourceLimit {
                plugin_id: plugin_id.clone(),
                resource: "generated bytes".to_owned(),
                actual: new_total,
                limit: MAX_PLUGIN_GENERATED_BYTES as u64,
            });
        }
        *self.total_generated_bytes = new_total;

        let destination = parse_template(plugin_id, resource_name, &destination, "destination")?;
        let destination =
            resolve_template(&destination, &self.plan.app, &self.plan.install_directory)?;
        check_resolved_template(plugin_id, resource_name, &destination, "destination")?;
        let destination_identity = logical_identity(&destination.to_string());
        let destination_hash = digest_hex(destination_identity.as_bytes());
        let source_name = format!("{destination_hash}.bin");
        let source_relative =
            RelativePath::from_components([zup_core::PLUGIN_PAYLOAD_ROOT, source_name.as_str()])
                .map_err(|error| PlanError::PluginResourceRejected {
                    plugin_id: plugin_id.clone(),
                    resource: resource_name.to_owned(),
                    reason: error.to_string(),
                })?;
        self.collisions.insert_file(
            &destination.to_string(),
            Some(source_relative.as_str()),
            Some(plugin_id),
            resource_name,
        )?;

        let sha256 = digest(&contents);
        let privilege = self.plan.scope.authorization();
        let planned_destination = destination.clone();
        self.plan.files.push(PlannedFile {
            key: ResourceKey::File {
                destination: destination.to_string(),
            },
            source_relative: source_relative.clone(),
            destination: planned_destination.clone(),
            size,
            sha256,
            privilege,
            executable: false,
        });
        self.generated_files.push(GeneratedFile {
            source_relative,
            destination: planned_destination,
            size,
            sha256,
            bytes: contents,
        });
        Ok(())
    }

    fn merge_launcher(
        &mut self,
        resource_name: &str,
        input: LauncherInput,
    ) -> Result<(), PlanError> {
        let LauncherInput {
            location,
            name,
            target,
            arguments,
            working_directory,
        } = input;
        let plugin_id = &self.binding.id;
        let name = parse_non_empty(plugin_id, resource_name, &name, "name")?;
        let target = parse_template(plugin_id, resource_name, &target, "target")?;
        let target = resolve_template(&target, &self.plan.app, &self.plan.install_directory)?;
        check_resolved_template(plugin_id, resource_name, &target, "target")?;
        let arguments = check_arguments(plugin_id, resource_name, arguments, "arguments")?;
        let working_directory = working_directory
            .map(|value| {
                let value = parse_template(plugin_id, resource_name, &value, "working directory")?;
                let value = resolve_template(&value, &self.plan.app, &self.plan.install_directory)?;
                check_resolved_template(plugin_id, resource_name, &value, "working directory")?;
                Ok(value)
            })
            .transpose()?;
        self.collisions.insert_launcher(
            &location.to_string(),
            name.as_str(),
            Some(plugin_id),
            resource_name,
        )?;
        self.plan.launchers.push(PlannedLauncher {
            key: ResourceKey::Launcher {
                location,
                name: name.to_string(),
            },
            location,
            name,
            target,
            arguments,
            working_directory,
            privilege: self.plan.scope.authorization(),
        });
        Ok(())
    }

    fn merge_path(&mut self, resource_name: &str, value: String) -> Result<(), PlanError> {
        let plugin_id = &self.binding.id;
        let value = parse_template(plugin_id, resource_name, &value, "value")?;
        let value = resolve_template(&value, &self.plan.app, &self.plan.install_directory)?;
        check_resolved_template(plugin_id, resource_name, &value, "value")?;
        self.collisions
            .insert_path(&value.to_string(), Some(plugin_id), resource_name)?;
        let scope = self.plan.scope;
        self.plan.path_entries.push(PlannedPathEntry {
            key: ResourceKey::PathEntry {
                value: value.to_string(),
            },
            value,
            scope,
            privilege: scope.authorization(),
        });
        Ok(())
    }

    fn merge_service(&mut self, resource_name: &str, input: ServiceInput) -> Result<(), PlanError> {
        let ServiceInput {
            id,
            name,
            display_name,
            binary,
            arguments,
            start,
        } = input;
        let plugin_id = &self.binding.id;
        let id: ServiceId = parse_id(plugin_id, resource_name, &id, "service id")?;
        let name = parse_non_empty(plugin_id, resource_name, &name, "name")?;
        let display_name = display_name
            .map(|value| parse_non_empty(plugin_id, resource_name, &value, "display name"))
            .transpose()?;
        let binary = parse_template(plugin_id, resource_name, &binary, "binary")?;
        let binary = resolve_template(&binary, &self.plan.app, &self.plan.install_directory)?;
        check_resolved_template(plugin_id, resource_name, &binary, "binary")?;
        let arguments = check_arguments(plugin_id, resource_name, arguments, "arguments")?;
        self.collisions
            .insert_service(id.as_str(), Some(plugin_id), resource_name)?;
        self.plan.services.push(PlannedService {
            key: ResourceKey::Service { id: id.clone() },
            id,
            name,
            display_name,
            binary,
            arguments,
            start,
            privilege: Privilege::System,
        });
        Ok(())
    }

    fn merge_protocol(
        &mut self,
        resource_name: &str,
        input: ProtocolInput,
    ) -> Result<(), PlanError> {
        let ProtocolInput {
            scheme,
            executable,
            args,
        } = input;
        let plugin_id = &self.binding.id;
        let scheme = parse_scheme(plugin_id, resource_name, &scheme)?;
        let executable = parse_template(plugin_id, resource_name, &executable, "executable")?;
        let executable =
            resolve_template(&executable, &self.plan.app, &self.plan.install_directory)?;
        check_resolved_template(plugin_id, resource_name, &executable, "executable")?;
        let args = check_arguments(plugin_id, resource_name, args, "arguments")?;
        self.collisions
            .insert_protocol(scheme.as_str(), Some(plugin_id), resource_name)?;
        self.plan.protocols.push(PlannedProtocol {
            key: ResourceKey::Protocol {
                scheme: scheme.clone(),
            },
            scheme,
            executable,
            args,
            scope: self.plan.scope,
            privilege: self.plan.scope.authorization(),
        });
        Ok(())
    }

    fn merge_file_association(
        &mut self,
        resource_name: &str,
        input: FileAssociationInput,
    ) -> Result<(), PlanError> {
        let FileAssociationInput {
            extension,
            id,
            description,
            executable,
        } = input;
        let plugin_id = &self.binding.id;
        let extension = parse_extension(plugin_id, resource_name, &extension)?;
        let id: FileAssociationId = parse_id(plugin_id, resource_name, &id, "file association id")?;
        let description = description
            .map(|value| check_optional_string(plugin_id, resource_name, &value, "description"))
            .transpose()?;
        let executable = parse_template(plugin_id, resource_name, &executable, "executable")?;
        let executable =
            resolve_template(&executable, &self.plan.app, &self.plan.install_directory)?;
        check_resolved_template(plugin_id, resource_name, &executable, "executable")?;
        self.collisions.insert_file_association(
            id.as_str(),
            extension.as_str(),
            Some(plugin_id),
            resource_name,
        )?;
        self.plan.file_associations.push(PlannedFileAssociation {
            key: ResourceKey::FileAssociation { id: id.clone() },
            extension,
            id,
            description,
            executable,
            scope: self.plan.scope,
            privilege: self.plan.scope.authorization(),
        });
        Ok(())
    }
}

fn parse_non_empty(
    plugin_id: &PluginId,
    resource_name: &str,
    value: &str,
    field: &str,
) -> Result<NonEmptyString, PlanError> {
    check_string(plugin_id, value, resource_name, field, true)?;
    NonEmptyString::new(value).map_err(|error| rejected(plugin_id, resource_name, field, error))
}

fn parse_id<T>(
    plugin_id: &PluginId,
    resource_name: &str,
    value: &str,
    field: &str,
) -> Result<T, PlanError>
where
    T: TryFrom<String, Error = zup_core::ValueError>,
{
    check_string(plugin_id, value, resource_name, field, true)?;
    T::try_from(value.to_owned()).map_err(|error| rejected(plugin_id, resource_name, field, error))
}

fn parse_extension(
    plugin_id: &PluginId,
    resource_name: &str,
    value: &str,
) -> Result<FileExtension, PlanError> {
    check_string(plugin_id, value, resource_name, "extension", true)?;
    FileExtension::new(value)
        .map_err(|error| rejected(plugin_id, resource_name, "extension", error))
}

fn parse_scheme(
    plugin_id: &PluginId,
    resource_name: &str,
    value: &str,
) -> Result<ProtocolScheme, PlanError> {
    check_string(plugin_id, value, resource_name, "scheme", true)?;
    ProtocolScheme::new(value).map_err(|error| rejected(plugin_id, resource_name, "scheme", error))
}

fn parse_template(
    plugin_id: &PluginId,
    resource_name: &str,
    value: &str,
    field: &str,
) -> Result<Template, PlanError> {
    check_string(plugin_id, value, resource_name, field, true)?;
    let template =
        Template::parse(value).map_err(|error| rejected(plugin_id, resource_name, field, error))?;
    if template.is_empty() {
        return Err(rejected(
            plugin_id,
            resource_name,
            field,
            "template must not be empty",
        ));
    }
    Ok(template)
}

fn check_resolved_template(
    plugin_id: &PluginId,
    resource_name: &str,
    value: &Template,
    field: &str,
) -> Result<(), PlanError> {
    let text = value.to_string();
    check_string(plugin_id, &text, resource_name, field, true)
}

fn check_arguments(
    plugin_id: &PluginId,
    resource_name: &str,
    values: Vec<String>,
    field: &str,
) -> Result<Vec<String>, PlanError> {
    if values.len() > MAX_PLUGIN_ARGUMENTS {
        return Err(PlanError::PluginResourceLimit {
            plugin_id: plugin_id.clone(),
            resource: resource_name.to_owned(),
            actual: values.len() as u64,
            limit: MAX_PLUGIN_ARGUMENTS as u64,
        });
    }
    let mut total = 0usize;
    for value in &values {
        check_string(plugin_id, value, resource_name, field, false)?;
        total = total
            .checked_add(value.len())
            .ok_or_else(|| PlanError::PluginResourceLimit {
                plugin_id: plugin_id.clone(),
                resource: resource_name.to_owned(),
                actual: u64::MAX,
                limit: MAX_PLUGIN_ARGUMENT_BYTES as u64,
            })?;
    }
    if total > MAX_PLUGIN_ARGUMENT_BYTES {
        return Err(PlanError::PluginResourceLimit {
            plugin_id: plugin_id.clone(),
            resource: resource_name.to_owned(),
            actual: total as u64,
            limit: MAX_PLUGIN_ARGUMENT_BYTES as u64,
        });
    }
    Ok(values)
}

fn check_optional_string(
    plugin_id: &PluginId,
    resource_name: &str,
    value: &str,
    field: &str,
) -> Result<String, PlanError> {
    check_string(plugin_id, value, resource_name, field, false)?;
    Ok(value.to_owned())
}

fn check_string(
    plugin_id: &PluginId,
    value: &str,
    resource_name: &str,
    field: &str,
    nonempty: bool,
) -> Result<(), PlanError> {
    if nonempty && value.is_empty() {
        return Err(rejected(
            plugin_id,
            resource_name,
            field,
            "string must not be empty",
        ));
    }
    if value.as_bytes().contains(&0) {
        return Err(rejected(
            plugin_id,
            resource_name,
            field,
            "string must not contain NUL",
        ));
    }
    if value.len() > MAX_PLUGIN_STRING_BYTES {
        return Err(PlanError::PluginResourceLimit {
            plugin_id: plugin_id.clone(),
            resource: resource_name.to_owned(),
            actual: value.len() as u64,
            limit: MAX_PLUGIN_STRING_BYTES as u64,
        });
    }
    Ok(())
}

fn rejected(
    plugin_id: &PluginId,
    resource_name: &str,
    field: &str,
    reason: impl std::fmt::Display,
) -> PlanError {
    PlanError::PluginResourceRejected {
        plugin_id: plugin_id.clone(),
        resource: format!("{resource_name}.{field}"),
        reason: reason.to_string(),
    }
}

fn collision_error(
    plugin_id: Option<&PluginId>,
    resource: &str,
    identity: &str,
    existing: &CollisionOrigin,
) -> PlanError {
    PlanError::PluginResourceCollision {
        plugin_id: plugin_id.cloned(),
        resource: resource.to_owned(),
        identity: identity.to_owned(),
        existing_plugin_id: existing.plugin_id.clone(),
        existing_resource: existing.resource.clone(),
    }
}

fn logical_identity(value: &str) -> String {
    value.to_owned()
}

fn digest(bytes: &[u8]) -> zup_core::Sha256Digest {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    zup_core::Sha256Digest::from_hasher(hasher)
}

fn digest_hex(bytes: &[u8]) -> String {
    digest(bytes).to_hex()
}

pub(crate) fn sort_resources(plan: &mut InstallPlan, generated_files: &mut [GeneratedFile]) {
    plan.files.sort_by(|left, right| {
        left.key
            .cmp(&right.key)
            .then_with(|| left.source_relative.cmp(&right.source_relative))
    });
    plan.launchers
        .sort_by(|left, right| left.key.cmp(&right.key));
    plan.path_entries
        .sort_by(|left, right| left.key.cmp(&right.key));
    plan.services
        .sort_by(|left, right| left.key.cmp(&right.key));
    plan.protocols
        .sort_by(|left, right| left.key.cmp(&right.key));
    plan.file_associations.sort_by(|left, right| {
        left.key
            .cmp(&right.key)
            .then_with(|| left.extension.cmp(&right.extension))
    });
    generated_files.sort_by(|left, right| {
        left.destination
            .to_string()
            .cmp(&right.destination.to_string())
            .then_with(|| left.source_relative.cmp(&right.source_relative))
    });
}

pub(crate) fn summarize_plan(
    plan: &InstallPlan,
    selected_component_count: usize,
) -> Result<PlanSummary, PlanError> {
    let mut install_bytes = 0u64;
    for file in &plan.files {
        install_bytes = install_bytes
            .checked_add(file.size)
            .ok_or(PlanError::SizeOverflow)?;
    }
    let download_bytes = plan
        .prerequisites
        .iter()
        .try_fold(0u64, |total, prerequisite| match &prerequisite.package {
            PrerequisitePackage::Remote {
                size: Some(size), ..
            } => total.checked_add(*size).ok_or(PlanError::SizeOverflow),
            PrerequisitePackage::Remote { .. } | PrerequisitePackage::Embedded { .. } => Ok(total),
        })?;
    let resource_count = plan.launchers.len()
        + plan.path_entries.len()
        + plan.services.len()
        + plan.protocols.len()
        + plan.file_associations.len();
    let requires_authorization = plan
        .prerequisites
        .iter()
        .any(|resource| resource.installer.privilege == Privilege::System)
        || plan
            .files
            .iter()
            .any(|resource| resource.privilege == Privilege::System)
        || plan
            .launchers
            .iter()
            .any(|resource| resource.privilege == Privilege::System)
        || plan
            .path_entries
            .iter()
            .any(|resource| resource.privilege == Privilege::System)
        || plan
            .services
            .iter()
            .any(|resource| resource.privilege == Privilege::System)
        || plan
            .protocols
            .iter()
            .any(|resource| resource.privilege == Privilege::System)
        || plan
            .file_associations
            .iter()
            .any(|resource| resource.privilege == Privilege::System);
    Ok(PlanSummary {
        file_count: plan.files.len(),
        install_bytes,
        selected_component_count,
        resource_count,
        requires_authorization,
        prerequisite_count: plan.prerequisites.len(),
        download_bytes,
    })
}
