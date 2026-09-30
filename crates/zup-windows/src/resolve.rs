use std::collections::BTreeMap;

use thiserror::Error;
use tracing::{info, info_span};
use zup_core::{
    InstallLocation, LauncherLocation, PrerequisitePackage, ResourceKey, SelectedScope,
    TargetOperatingSystem, Template,
};
use zup_plan::InstallPlan;
use zup_platform::{
    CommandSpec, InstallLocationResolver, TargetFile, TargetFileAssociation, TargetLauncher,
    TargetPath, TargetPathEntry, TargetPlan, TargetPlanSummary, TargetPrerequisite, TargetProtocol,
    TargetService, TemplateResolveError, resolve_template_path,
};

use crate::host_dirs::WindowsInstallLocationResolver;
use crate::lowering::{
    TargetPathValidationError, validate_windows_target_path, windows_target_path_identity,
};
use crate::shortcut_name::validate_shortcut_filename;

#[derive(Debug, Clone, Copy)]
pub struct WindowsTargetContext<R = WindowsInstallLocationResolver> {
    pub locations: R,
    pub scope: SelectedScope,
}

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
    Template(#[from] TemplateResolveError),

    #[error(transparent)]
    TargetPath(#[from] zup_platform::TargetPathError),

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
    plan: &InstallPlan,
    context: &WindowsTargetContext<R>,
) -> Result<TargetPlan, TargetResolveError> {
    let _span = info_span!("resolve_target", scope = %context.scope).entered();
    if plan.target.operating_system() != TargetOperatingSystem::Windows {
        return Err(TargetResolveError::UnsupportedTarget {
            target: plan.target.to_string(),
        });
    }
    let scope = context.scope;
    let resolve = |template: &Template, kind: &str| {
        let path = resolve_template_path(template, &plan.target, &context.locations, scope)?;
        validate_target_path(kind, &path)?;
        Ok::<TargetPath, TargetResolveError>(path)
    };
    let mut collisions = TargetCollisionIndex::default();

    let install_directory = resolve(&plan.install_directory, "install directory")?;
    collisions.register_owned(
        &install_directory,
        "install directory",
        OwnedPathKind::Directory,
    )?;

    let prerequisites = plan
        .prerequisites
        .iter()
        .map(|prerequisite| TargetPrerequisite {
            id: prerequisite.id.clone(),
            name: prerequisite.name.clone(),
            target: prerequisite.target,
            requirement: prerequisite.requirement.clone(),
            package: prerequisite.package.clone(),
            installer: prerequisite.installer.clone(),
        })
        .collect();

    let mut files = Vec::with_capacity(plan.files.len());
    let mut install_bytes = 0u64;
    for file in &plan.files {
        install_bytes = install_bytes
            .checked_add(file.size)
            .ok_or(TargetResolveError::SizeOverflow)?;
        let destination = resolve(&file.destination, "file destination")?;
        collisions.register_owned(&destination, "file destination", OwnedPathKind::File)?;
        files.push(TargetFile {
            key: ResourceKey::File {
                destination: destination.to_string(),
            },
            source_relative: file.source_relative.clone(),
            destination,
            size: file.size,
            sha256: file.sha256,
            privilege: file.privilege,
        });
    }

    let mut launchers = Vec::with_capacity(plan.launchers.len());
    for launcher in &plan.launchers {
        validate_shortcut_filename(launcher.name.as_str()).map_err(|reason| {
            TargetResolveError::InvalidLauncherName {
                name: launcher.name.to_string(),
                reason,
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
        collisions.register_owned(&launcher_path, "launcher link path", OwnedPathKind::File)?;

        let target = resolve(&launcher.target, "launcher target")?;
        let working_directory = launcher
            .working_directory
            .as_ref()
            .map(|directory| resolve(directory, "launcher working directory"))
            .transpose()?;
        launchers.push(TargetLauncher {
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
        path_entries.push(TargetPathEntry {
            key: ResourceKey::PathEntry {
                value: value.to_string(),
            },
            value,
            // The owning search path comes from the plan, not from privilege.
            scope: entry.scope,
            privilege: entry.privilege,
        });
    }

    let mut services = Vec::with_capacity(plan.services.len());
    for service in &plan.services {
        let binary = resolve(&service.binary, "service binary")?;
        collisions.register_service(service.id.as_str())?;
        services.push(TargetService {
            key: ResourceKey::Service {
                id: service.id.clone(),
            },
            id: service.id.clone(),
            name: service.name.clone(),
            display_name: service.display_name.clone(),
            command: CommandSpec::new(binary, service.arguments.clone()),
            start: service.start,
            privilege: service.privilege,
        });
    }

    let mut protocols = Vec::with_capacity(plan.protocols.len());
    for protocol in &plan.protocols {
        let executable = resolve(&protocol.executable, "protocol executable")?;
        collisions.register_protocol(protocol.scheme.as_str())?;
        protocols.push(TargetProtocol {
            key: ResourceKey::Protocol {
                scheme: protocol.scheme.clone(),
            },
            scheme: protocol.scheme.clone(),
            command: CommandSpec::new(executable, protocol.args.clone()),
            // The host store comes from the plan, not from privilege.
            scope: protocol.scope,
            privilege: protocol.privilege,
        });
    }

    let mut file_associations = Vec::with_capacity(plan.file_associations.len());
    for file_association in &plan.file_associations {
        let executable = resolve(&file_association.executable, "file association executable")?;
        collisions.register_file_association(
            file_association.id.as_str(),
            &file_association.extension.to_string(),
        )?;
        file_associations.push(TargetFileAssociation {
            key: ResourceKey::FileAssociation {
                id: file_association.id.clone(),
            },
            extension: file_association.extension.clone(),
            id: file_association.id.clone(),
            description: file_association.description.clone(),
            command: CommandSpec::new(executable, Vec::new()),
            // The host store comes from the plan, not from privilege.
            scope: file_association.scope,
            privilege: file_association.privilege,
        });
    }

    let resource_count = launchers.len()
        + path_entries.len()
        + services.len()
        + protocols.len()
        + file_associations.len();
    let summary = TargetPlanSummary {
        file_count: files.len(),
        install_bytes,
        resource_count,
        requires_authorization: plan.summary.requires_authorization,
        selected_component_count: plan.selected_components.len(),
        prerequisite_count: plan.prerequisites.len(),
        download_bytes: plan
            .prerequisites
            .iter()
            .filter_map(|item| match &item.package {
                PrerequisitePackage::Remote { size, .. } => *size,
                PrerequisitePackage::Embedded { .. } => None,
            })
            .sum(),
    };

    info!(
        target_files = files.len(),
        launchers = launchers.len(),
        "target resolution complete"
    );

    Ok(TargetPlan {
        app: plan.app.clone(),
        target: plan.target.clone(),
        scope: plan.scope,
        install_directory,
        // A build plan says which preset; it does not say which bytes. The
        // executable is content, and content is resolved by whoever supplies the
        // payload - the caller that attaches this installation's runtime copy.
        ui: None,
        selected_components: plan.selected_components.clone(),
        prerequisites,
        files,
        launchers,
        path_entries,
        services,
        protocols,
        file_associations,
        summary,
    })
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
        Err(TargetPathValidationError::DevicePath { path }) => {
            Err(TargetResolveError::InvalidTargetPath {
                kind: kind.to_owned(),
                path,
                component: "<device>".to_owned(),
                reason: "device namespace paths are not supported".to_owned(),
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

#[derive(Debug, Clone, Copy)]
enum OwnedPathKind {
    Directory,
    File,
}

#[derive(Debug)]
struct OwnedPath {
    path: String,
    kind: OwnedPathKind,
}

#[derive(Debug, Default)]
struct TargetCollisionIndex {
    owned_paths: BTreeMap<String, OwnedPath>,
    path_entries: BTreeMap<String, String>,
    services: BTreeMap<String, String>,
    protocols: BTreeMap<String, String>,
    file_association_ids: BTreeMap<String, String>,
    file_association_extensions: BTreeMap<String, String>,
}

impl TargetCollisionIndex {
    fn register_owned(
        &mut self,
        path: &TargetPath,
        kind: &str,
        path_kind: OwnedPathKind,
    ) -> Result<(), TargetResolveError> {
        let identity = windows_target_path_identity(path);
        let display = path.to_string();
        let is_directory = matches!(path_kind, OwnedPathKind::Directory);
        if let Some(existing) = self.owned_paths.get(&identity) {
            return Err(TargetResolveError::TargetCollision {
                kind: kind.to_owned(),
                first: existing.path.clone(),
                second: display,
                identity,
            });
        }

        for (existing_identity, existing) in &self.owned_paths {
            let existing_is_directory = matches!(existing.kind, OwnedPathKind::Directory);
            let hierarchy_conflict = if existing_is_directory && !is_directory {
                is_ancestor(&identity, existing_identity)
            } else if !existing_is_directory && is_directory {
                is_ancestor(existing_identity, &identity)
            } else if !existing_is_directory && !is_directory {
                is_ancestor(&identity, existing_identity)
                    || is_ancestor(existing_identity, &identity)
            } else {
                false
            };
            if hierarchy_conflict {
                return Err(TargetResolveError::TargetCollision {
                    kind: kind.to_owned(),
                    first: existing.path.clone(),
                    second: display,
                    identity: existing_identity.clone(),
                });
            }
        }

        self.owned_paths.insert(
            identity.clone(),
            OwnedPath {
                path: display,
                kind: path_kind,
            },
        );
        Ok(())
    }

    fn register_path_entry(&mut self, path: &TargetPath) -> Result<(), TargetResolveError> {
        insert_identity(
            &mut self.path_entries,
            &windows_target_path_identity(path),
            &path.to_string(),
            "PATH entry",
        )
    }

    fn register_service(&mut self, id: &str) -> Result<(), TargetResolveError> {
        insert_identity(&mut self.services, &id.to_lowercase(), id, "service id")
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

fn is_ancestor(ancestor: &str, descendant: &str) -> bool {
    let ancestor = ancestor.trim_end_matches('\\');
    descendant.len() > ancestor.len()
        && descendant
            .get(..ancestor.len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(ancestor))
        && descendant.as_bytes().get(ancestor.len()) == Some(&b'\\')
}
