use std::collections::BTreeMap;

use thiserror::Error;
use tracing::{info, info_span};
use zup_core::{
    InstallLocation, LauncherLocation, ResourceKey, SelectedScope, TargetOperatingSystem, Template,
};
use zup_plan::{
    CommandSpec, DesiredFileAssociation, DesiredLauncher, DesiredPathEntry, DesiredProtocol,
    DesiredService, DesiredState, InstallLocationResolver, TargetPath, TemplateResolveError,
    resolve_files, resolve_template_path,
};

#[cfg(windows)]
use crate::host_dirs::WindowsInstallLocationResolver;
use crate::locations::CanonicalWindowsLocations;
use crate::lowering::{TargetPathValidationError, validate_windows_target_path};
use crate::shortcut_name::validate_shortcut_filename;

#[derive(Debug, Clone, Copy)]
pub struct WindowsTargetContext<R = CanonicalWindowsLocations> {
    pub locations: R,
    pub scope: SelectedScope,
}

impl WindowsTargetContext<CanonicalWindowsLocations> {
    pub fn for_build(scope: SelectedScope) -> Self {
        Self {
            locations: CanonicalWindowsLocations,
            scope,
        }
    }
}

#[cfg(windows)]
impl WindowsTargetContext<WindowsInstallLocationResolver> {
    pub fn new(scope: SelectedScope) -> Self {
        Self {
            locations: WindowsInstallLocationResolver,
            scope,
        }
    }
}

impl<R> WindowsTargetContext<R> {
    pub fn with_resolver(locations: R, scope: SelectedScope) -> Self {
        Self { locations, scope }
    }
}

#[derive(Debug, Error)]
pub enum TargetResolveError {
    #[error(transparent)]
    Contract(#[from] zup_plan::ContractError),
    #[error("resolution scope must match the selected plan scope")]
    ScopeMismatch,
    #[error(transparent)]
    Template(#[from] TemplateResolveError),

    #[error(transparent)]
    TargetPath(#[from] zup_plan::TargetPathError),

    #[error("invalid Windows target path for {kind} `{path}` (component `{component}`): {reason}")]
    InvalidTargetPath {
        kind: String,
        path: String,
        component: String,
        reason: String,
    },

    #[error(
        "Windows target collision for {kind}: `{first}` and `{second}` both map to `{identity}` (target identities are case-insensitive)"
    )]
    TargetCollision {
        kind: String,
        first: String,
        second: String,
        identity: String,
    },

    #[error("invalid launcher name `{name}`: {reason}")]
    InvalidLauncherName { name: String, reason: String },

    #[error("target plan size overflow")]
    SizeOverflow,

    #[error("unsupported backend for target `{target}`: Windows target lowering is required")]
    UnsupportedTarget { target: String },
}

pub fn resolve_target<R: InstallLocationResolver>(
    plan: &DesiredState,
    context: &WindowsTargetContext<R>,
) -> Result<DesiredState<zup_plan::TargetPath, zup_plan::TargetPath>, TargetResolveError> {
    let _span = info_span!("resolve_target", scope = %context.scope).entered();
    if plan.target.operating_system() != TargetOperatingSystem::Windows {
        return Err(TargetResolveError::UnsupportedTarget {
            target: plan.target.to_string(),
        });
    }
    if context.scope != plan.scope {
        return Err(TargetResolveError::ScopeMismatch);
    }
    plan.validate()?;
    let scope = context.scope;
    let resolve = |template: &Template, kind: &'static str| {
        let path = resolve_template_path(template, &plan.target, &context.locations, scope)?;
        validate_target_path(kind, &path)?;
        Ok::<TargetPath, TargetResolveError>(path)
    };
    let mut collisions = TargetCollisionIndex::default();

    let install_directory = resolve(&plan.install_directory, "install directory")?;

    let files = resolve_files(plan, resolve)?;

    let mut launchers = Vec::with_capacity(plan.launchers.len());
    for launcher in &plan.launchers {
        validate_shortcut_filename(launcher.name.as_str()).map_err(|reason| {
            TargetResolveError::InvalidLauncherName {
                name: launcher.name.to_string(),
                reason: reason.to_string(),
            }
        })?;

        let location = match launcher.location {
            LauncherLocation::Menu => InstallLocation::Menu,
            LauncherLocation::Desktop => InstallLocation::Desktop,
        };
        let base = context
            .locations
            .resolve(location, scope, &plan.target)
            .map_err(TemplateResolveError::from)?;
        let base = match launcher.location {
            LauncherLocation::Menu => base.join("Programs")?,
            LauncherLocation::Desktop => base,
        };
        let launcher_path = base.join(format!("{}.lnk", launcher.name))?;
        validate_target_path("launcher link path", &launcher_path)?;

        let target = resolve(&launcher.target, "launcher target")?;
        let working_directory = launcher
            .working_directory
            .as_ref()
            .map(|directory| resolve(directory, "launcher working directory"))
            .transpose()?;
        launchers.push(DesiredLauncher {
            key: ResourceKey::Launcher {
                location: launcher.location,
                name: launcher.name.to_string(),
            },
            location: launcher.location,
            name: launcher.name.clone(),
            launcher_path,
            target,
            arguments: launcher.arguments.clone(),
            working_directory,
            privilege: launcher.privilege,
        });
    }

    let mut path_entries = Vec::with_capacity(plan.path_entries.len());
    for entry in &plan.path_entries {
        let value = resolve(&entry.value, "PATH entry")?;
        collisions.register_path_entry(&value)?;
        path_entries.push(DesiredPathEntry {
            key: ResourceKey::PathEntry {
                value: value.to_string(),
            },
            value,

            scope: entry.scope,
            privilege: entry.privilege,
        });
    }

    let mut services = Vec::with_capacity(plan.services.len());
    for service in &plan.services {
        let binary = resolve(&service.command.executable, "service binary")?;
        collisions.register_service(service.id.as_str(), service.name.as_str())?;
        services.push(DesiredService {
            key: ResourceKey::Service {
                id: service.id.clone(),
            },
            id: service.id.clone(),
            name: zup_core::NonEmptyString::new(service.name.as_str().to_lowercase())
                .expect("service name is nonempty"),
            display_name: Some(
                service
                    .display_name
                    .clone()
                    .unwrap_or_else(|| service.name.clone()),
            ),
            command: CommandSpec::new(binary, service.command.arguments.clone()),
            start: service.start,
            privilege: service.privilege,
        });
    }

    let mut protocols = Vec::with_capacity(plan.protocols.len());
    for protocol in &plan.protocols {
        let executable = resolve(&protocol.command.executable, "protocol executable")?;
        collisions.register_protocol(protocol.scheme.as_str())?;
        protocols.push(DesiredProtocol {
            key: ResourceKey::Protocol {
                scheme: protocol.scheme.clone(),
            },
            scheme: protocol.scheme.clone(),
            command: CommandSpec::new(executable, protocol.command.arguments.clone()),

            scope: protocol.scope,
            privilege: protocol.privilege,
        });
    }

    let mut file_associations = Vec::with_capacity(plan.file_associations.len());
    for file_association in &plan.file_associations {
        let executable = resolve(
            &file_association.command.executable,
            "file association executable",
        )?;
        collisions.register_file_association(
            file_association.id.as_str(),
            &file_association.extension.to_string(),
        )?;
        file_associations.push(DesiredFileAssociation {
            key: ResourceKey::FileAssociation {
                id: file_association.id.clone(),
            },
            extension: file_association.extension.clone(),
            id: file_association.id.clone(),
            description: file_association.description.clone(),
            command: CommandSpec::new(executable, Vec::new()),

            scope: file_association.scope,
            privilege: file_association.privilege,
        });
    }

    info!(
        target_files = files.len(),
        launchers = launchers.len(),
        "target resolution complete"
    );

    let prerequisites = plan.prerequisites.clone();
    let mut resolved = DesiredState {
        app: plan.app.clone(),
        target: plan.target,
        scope: plan.scope,
        install_directory,

        preset: None,
        selected_components: plan.selected_components.clone(),
        prerequisites,
        files,
        launchers,
        path_entries,
        services,
        protocols,
        file_associations,
    };
    resolved.canonicalize_keys();
    resolved.validate_resolved()?;
    Ok(resolved)
}

fn validate_target_path(kind: &str, path: &TargetPath) -> Result<(), TargetResolveError> {
    match validate_windows_target_path(path) {
        Ok(()) => Ok(()),
        Err(TargetPathValidationError::InvalidComponent { component, reason }) => {
            Err(TargetResolveError::InvalidTargetPath {
                kind: kind.to_owned(),
                path: path.to_string(),
                component,
                reason,
            })
        }
        Err(TargetPathValidationError::UnsupportedTarget { target }) => {
            Err(TargetResolveError::InvalidTargetPath {
                kind: kind.to_owned(),
                path: path.to_string(),
                component: "<target>".to_owned(),
                reason: format!("target `{target}` is not Windows"),
            })
        }
    }
}

#[derive(Debug, Default)]
struct TargetCollisionIndex {
    path_entries: BTreeMap<String, String>,
    services: BTreeMap<String, String>,
    service_names: BTreeMap<String, String>,
    protocols: BTreeMap<String, String>,
    file_association_ids: BTreeMap<String, String>,
    file_association_extensions: BTreeMap<String, String>,
}

impl TargetCollisionIndex {
    fn register_path_entry(&mut self, path: &TargetPath) -> Result<(), TargetResolveError> {
        insert_identity(
            &mut self.path_entries,
            &path.identity(),
            &path.to_string(),
            "PATH entry",
        )
    }

    fn register_service(&mut self, id: &str, name: &str) -> Result<(), TargetResolveError> {
        insert_identity(&mut self.services, &id.to_lowercase(), id, "service id")?;
        insert_identity(
            &mut self.service_names,
            &name.to_lowercase(),
            name,
            "service name",
        )
    }

    fn register_protocol(&mut self, scheme: &str) -> Result<(), TargetResolveError> {
        insert_identity(
            &mut self.protocols,
            &scheme.to_lowercase(),
            scheme,
            "protocol scheme",
        )
    }

    fn register_file_association(
        &mut self,
        id: &str,
        extension: &str,
    ) -> Result<(), TargetResolveError> {
        insert_identity(
            &mut self.file_association_ids,
            &id.to_lowercase(),
            id,
            "file association id",
        )?;
        insert_identity(
            &mut self.file_association_extensions,
            &extension.to_lowercase(),
            extension,
            "file association extension",
        )
    }
}

fn insert_identity(
    identities: &mut BTreeMap<String, String>,
    identity: &str,
    value: &str,
    kind: &str,
) -> Result<(), TargetResolveError> {
    if let Some(existing) = identities.get(identity) {
        return Err(TargetResolveError::TargetCollision {
            kind: kind.to_owned(),
            first: existing.clone(),
            second: value.to_owned(),
            identity: identity.to_owned(),
        });
    }
    identities.insert(identity.to_owned(), value.to_owned());
    Ok(())
}
