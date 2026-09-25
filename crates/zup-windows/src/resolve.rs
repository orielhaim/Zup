//! Resolve `InstallPlan` into a concrete Windows `TargetPlan`.

use thiserror::Error;
use tracing::{info, info_span};
use zup_core::{PrerequisitePackage, ResourceKey, SelectedScope, ShortcutLocation};
use zup_plan::InstallPlan;
use zup_platform::{
    CommandSpec, KnownFolder, KnownFolderResolver, TargetFile, TargetFileType, TargetPath,
    TargetPathEntry, TargetPlan, TargetPlanSummary, TargetPrerequisite, TargetProtocol,
    TargetService, TargetShortcut, TemplateResolveError, resolve_template_path,
};

use crate::cmdline;
use crate::known_folders::WindowsKnownFolderResolver;
use crate::shortcut_name::validate_shortcut_filename;

/// Concrete Windows target context used for path resolution.
#[derive(Debug, Clone, Copy)]
pub struct WindowsTargetContext<R = WindowsKnownFolderResolver> {
    pub known: R,
    pub scope: SelectedScope,
}

impl WindowsTargetContext<WindowsKnownFolderResolver> {
    pub fn new(scope: SelectedScope) -> Self {
        Self {
            known: WindowsKnownFolderResolver,
            scope,
        }
    }
}

impl<R> WindowsTargetContext<R> {
    pub fn with_resolver(known: R, scope: SelectedScope) -> Self {
        Self { known, scope }
    }
}

/// Errors produced while resolving an `InstallPlan` for a Windows target.
#[derive(Debug, Error)]
pub enum TargetResolveError {
    #[error(transparent)]
    Template(#[from] TemplateResolveError),

    #[error(transparent)]
    TargetPath(#[from] zup_platform::TargetPathError),

    #[error("invalid shortcut name `{name}`: {reason}")]
    InvalidShortcutName { name: String, reason: String },

    #[error("target plan size overflow")]
    SizeOverflow,
}

/// Resolve portable desired state into machine-concrete target paths.
///
/// Zero filesystem I/O. Leaves no template variables in filesystem destinations.
pub fn resolve_target<R: KnownFolderResolver>(
    plan: &InstallPlan,
    context: &WindowsTargetContext<R>,
) -> Result<TargetPlan, TargetResolveError> {
    let _span = info_span!("resolve_target", scope = %context.scope).entered();

    let scope = context.scope;
    let path = |template: &zup_core::Template| {
        resolve_template_path(template, &context.known, scope).map_err(TargetResolveError::from)
    };

    let install_directory = path(&plan.install_directory)?;

    let prerequisites = plan
        .prerequisites
        .iter()
        .map(|prerequisite| TargetPrerequisite {
            id: prerequisite.id.clone(),
            name: prerequisite.name.clone(),
            target: prerequisite.target,
            detector: prerequisite.detector.clone(),
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
        let destination = path(&file.destination)?;
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

    let mut shortcuts = Vec::with_capacity(plan.shortcuts.len());
    for shortcut in &plan.shortcuts {
        validate_shortcut_filename(shortcut.name.as_str()).map_err(|reason| {
            TargetResolveError::InvalidShortcutName {
                name: shortcut.name.to_string(),
                reason,
            }
        })?;

        let folder = match shortcut.location {
            ShortcutLocation::StartMenu => KnownFolder::Programs,
            ShortcutLocation::Desktop => KnownFolder::Desktop,
        };
        let base = context
            .known
            .resolve(folder, scope)
            .map_err(TemplateResolveError::KnownFolder)?;
        let mut link_path = base;
        link_path.push(format!("{}.lnk", shortcut.name));
        let link_path = TargetPath::new(link_path)?;

        shortcuts.push(TargetShortcut {
            key: ResourceKey::Shortcut {
                location: shortcut.location,
                name: shortcut.name.to_string(),
            },
            location: shortcut.location,
            name: shortcut.name.clone(),
            link_path,
            target: path(&shortcut.target)?,
            arguments: shortcut.arguments.clone(),
            working_directory: shortcut.working_directory.as_ref().map(&path).transpose()?,
            privilege: shortcut.privilege,
        });
    }

    let mut path_entries = Vec::with_capacity(plan.path_entries.len());
    for entry in &plan.path_entries {
        let value = path(&entry.value)?;
        path_entries.push(TargetPathEntry {
            key: ResourceKey::PathEntry {
                value: value.to_string(),
            },
            value,
            scope: entry.privilege_scope(),
            privilege: entry.privilege,
        });
    }

    let mut services = Vec::with_capacity(plan.services.len());
    for service in &plan.services {
        services.push(TargetService {
            key: ResourceKey::Service {
                id: service.id.clone(),
            },
            id: service.id.clone(),
            name: service.name.clone(),
            display_name: service.display_name.clone(),
            command: CommandSpec::new(path(&service.binary)?, service.arguments.clone()),
            start: service.start,
            privilege: service.privilege,
        });
    }

    let mut protocols = Vec::with_capacity(plan.protocols.len());
    for protocol in &plan.protocols {
        protocols.push(TargetProtocol {
            key: ResourceKey::Protocol {
                scheme: protocol.scheme.clone(),
            },
            scheme: protocol.scheme.clone(),
            command: CommandSpec::new(path(&protocol.executable)?, protocol.args.clone()),
            scope: protocol.privilege_scope(),
            privilege: protocol.privilege,
        });
    }

    let mut file_types = Vec::with_capacity(plan.file_types.len());
    for file_type in &plan.file_types {
        file_types.push(TargetFileType {
            key: ResourceKey::FileType {
                id: file_type.id.clone(),
            },
            extension: file_type.extension.clone(),
            id: file_type.id.clone(),
            description: file_type.description.clone(),
            command: CommandSpec::new(path(&file_type.executable)?, Vec::new()),
            scope: file_type.privilege_scope(),
            privilege: file_type.privilege,
        });
    }

    let resource_count =
        shortcuts.len() + path_entries.len() + services.len() + protocols.len() + file_types.len();

    let summary = TargetPlanSummary {
        file_count: files.len(),
        install_bytes,
        resource_count,
        requires_elevation: plan.summary.requires_elevation,
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
        shortcuts = shortcuts.len(),
        "target resolution complete"
    );

    let _ = cmdline::format_command_line;
    let _ = TargetPath::new;

    Ok(TargetPlan {
        app: plan.app.clone(),
        scope: plan.scope,
        install_directory,
        selected_components: plan.selected_components.clone(),
        prerequisites,
        files,
        shortcuts,
        path_entries,
        services,
        protocols,
        file_types,
        summary,
    })
}

trait PrivilegeScope {
    fn privilege_scope(&self) -> SelectedScope;
}

impl PrivilegeScope for zup_plan::PlannedPathEntry {
    fn privilege_scope(&self) -> SelectedScope {
        match self.privilege {
            zup_core::Privilege::User => SelectedScope::User,
            zup_core::Privilege::Machine => SelectedScope::Machine,
        }
    }
}

impl PrivilegeScope for zup_plan::PlannedProtocol {
    fn privilege_scope(&self) -> SelectedScope {
        match self.privilege {
            zup_core::Privilege::User => SelectedScope::User,
            zup_core::Privilege::Machine => SelectedScope::Machine,
        }
    }
}

impl PrivilegeScope for zup_plan::PlannedFileType {
    fn privilege_scope(&self) -> SelectedScope {
        match self.privilege {
            zup_core::Privilege::User => SelectedScope::User,
            zup_core::Privilege::Machine => SelectedScope::Machine,
        }
    }
}
