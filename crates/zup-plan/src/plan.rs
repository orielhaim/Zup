//! Desired-state planner: BuildPlan + choices → InstallPlan.

use std::collections::BTreeSet;

use tracing::{info, info_span};
use zup_build::BuildPlan;
use zup_core::{ActionKind, ComponentId, Condition, InstallScope, Privilege, Template};

use crate::error::{PlanError, SelectedScope};
use crate::plan_types::{InstallPlan, PlanSummary};
use crate::request::PlanRequest;
use crate::resolve::{resolve_install_directory, resolve_template};
use crate::resources::{
    PlannedExternalAction, PlannedFile, PlannedFileType, PlannedPathEntry, PlannedProtocol,
    PlannedService, PlannedShortcut,
};
use crate::select::select_components;
use zup_core::ResourceKey;

/// Compute a portable desired installation plan.
///
/// Pure and deterministic. Does not inspect the target machine.
pub fn plan(build: &BuildPlan, request: &PlanRequest) -> Result<InstallPlan, PlanError> {
    let installer = &build.installer;
    let scope = request.scope;

    let span = info_span!(
        "plan",
        scope = %scope,
        enable = request.components.enable.len(),
        disable = request.components.disable.len(),
    );
    let _guard = span.enter();

    check_scope(installer.install.scope, scope)?;

    let selected = select_components(&installer.components, &request.components)?;
    let selected_set: BTreeSet<ComponentId> = selected.iter().cloned().collect();
    info!(
        default_count = installer.components.iter().filter(|c| c.default).count(),
        enable_count = request.components.enable.len(),
        disable_count = request.components.disable.len(),
        selected_count = selected.len(),
        "component selection complete"
    );

    let raw_directory = match (scope, installer.install.scope) {
        (SelectedScope::User, _) => installer
            .install
            .directory
            .user
            .clone()
            .ok_or(PlanError::ScopeRequired { scope })?,
        (SelectedScope::Machine, _) => installer
            .install
            .directory
            .machine
            .clone()
            .ok_or(PlanError::ScopeRequired { scope })?,
    };

    let install_directory = resolve_install_directory(&raw_directory, &installer.app)?;
    let scope_privilege = scope.privilege();

    let mut files = Vec::new();
    for file in &build.files {
        if !is_active(
            file.component.as_ref(),
            file.condition.as_ref(),
            &selected_set,
        ) {
            continue;
        }
        let destination = resolve_template(&file.destination, &installer.app, &install_directory)?;
        files.push(PlannedFile {
            key: ResourceKey::File {
                destination: destination.to_string(),
            },
            source_relative: file.source_relative.clone(),
            destination,
            size: file.size,
            sha256: file.sha256,
            privilege: scope_privilege,
        });
    }

    let mut shortcuts = Vec::new();
    for shortcut in &installer.shortcuts {
        if !is_active(
            shortcut.component.as_ref(),
            shortcut.when.as_ref(),
            &selected_set,
        ) {
            continue;
        }
        let name = shortcut.name.clone();
        shortcuts.push(PlannedShortcut {
            key: ResourceKey::Shortcut {
                location: shortcut.location,
                name: name.to_string(),
            },
            location: shortcut.location,
            name,
            target: resolve_template(&shortcut.target, &installer.app, &install_directory)?,
            arguments: shortcut.arguments.clone(),
            working_directory: shortcut
                .working_directory
                .as_ref()
                .map(|dir| resolve_template(dir, &installer.app, &install_directory))
                .transpose()?,
            privilege: scope_privilege,
        });
    }

    let mut path_entries = Vec::new();
    for entry in &installer.path {
        if !is_active(entry.component.as_ref(), entry.when.as_ref(), &selected_set) {
            continue;
        }
        let value = resolve_template(&entry.value, &installer.app, &install_directory)?;
        path_entries.push(PlannedPathEntry {
            key: ResourceKey::PathEntry {
                value: value.to_string(),
            },
            value,
            privilege: scope_privilege,
        });
    }

    let mut services = Vec::new();
    for service in &installer.services {
        if !is_active(
            service.component.as_ref(),
            service.when.as_ref(),
            &selected_set,
        ) {
            continue;
        }
        services.push(PlannedService {
            key: ResourceKey::Service {
                id: service.id.clone(),
            },
            id: service.id.clone(),
            name: service.name.clone(),
            display_name: service.display_name.clone(),
            binary: resolve_template(&service.binary, &installer.app, &install_directory)?,
            arguments: service.arguments.clone(),
            start: service.start,
            privilege: Privilege::Machine,
        });
    }

    let mut protocols = Vec::new();
    for protocol in &installer.protocols {
        if !is_active(None, protocol.when.as_ref(), &selected_set) {
            continue;
        }
        protocols.push(PlannedProtocol {
            key: ResourceKey::Protocol {
                scheme: protocol.scheme.clone(),
            },
            scheme: protocol.scheme.clone(),
            executable: resolve_template(&protocol.executable, &installer.app, &install_directory)?,
            args: protocol.args.clone(),
            privilege: scope_privilege,
        });
    }

    let mut file_types = Vec::new();
    for file_type in &installer.file_types {
        if !is_active(None, file_type.when.as_ref(), &selected_set) {
            continue;
        }
        file_types.push(PlannedFileType {
            key: ResourceKey::FileType {
                id: file_type.id.clone(),
            },
            extension: file_type.extension.clone(),
            id: file_type.id.clone(),
            description: file_type.description.clone(),
            executable: resolve_template(
                &file_type.executable,
                &installer.app,
                &install_directory,
            )?,
            privilege: scope_privilege,
        });
    }

    let mut actions = Vec::new();
    for action in &installer.actions {
        if !is_active(
            action.component.as_ref(),
            action.when.as_ref(),
            &selected_set,
        ) {
            continue;
        }
        let privilege = action.privilege.unwrap_or(scope_privilege);
        actions.push(PlannedExternalAction {
            key: ResourceKey::ExternalAction {
                id: action.id.clone(),
            },
            id: action.id.clone(),
            kind: action.kind,
            apply: resolve_command(&action.apply, &installer.app, &install_directory)?,
            rollback: action
                .rollback
                .as_ref()
                .map(|command| resolve_command(command, &installer.app, &install_directory))
                .transpose()?,
            uninstall: action
                .uninstall
                .as_ref()
                .map(|command| resolve_command(command, &installer.app, &install_directory))
                .transpose()?,
            privilege,
            opaque: matches!(action.kind, ActionKind::Exec),
        });
    }

    validate_active_collisions(
        &files,
        &shortcuts,
        &path_entries,
        &services,
        &protocols,
        &file_types,
        &actions,
    )?;

    let summary = summarize(
        &files,
        &shortcuts,
        &path_entries,
        &services,
        &protocols,
        &file_types,
        &actions,
        selected.len(),
        scope,
    )?;

    info!(
        active_files = files.len(),
        active_resources = shortcuts.len()
            + path_entries.len()
            + services.len()
            + protocols.len()
            + file_types.len()
            + actions.len(),
        install_bytes = summary.install_bytes,
        requires_elevation = summary.requires_elevation,
        "plan complete"
    );

    Ok(InstallPlan {
        app: installer.app.clone(),
        scope,
        install_directory,
        selected_components: selected,
        files,
        shortcuts,
        path_entries,
        services,
        protocols,
        file_types,
        actions,
        summary,
    })
}

fn resolve_command(
    command: &zup_core::Command,
    app: &zup_core::App,
    install_directory: &Template,
) -> Result<zup_core::Command, PlanError> {
    Ok(zup_core::Command {
        command: resolve_template(&command.command, app, install_directory)?,
        args: command.args.clone(),
    })
}

fn check_scope(allowed: InstallScope, requested: SelectedScope) -> Result<(), PlanError> {
    let ok = matches!(
        (allowed, requested),
        (InstallScope::User, SelectedScope::User)
            | (InstallScope::Machine, SelectedScope::Machine)
            | (InstallScope::Either, _)
    );
    if ok {
        Ok(())
    } else {
        Err(PlanError::ScopeNotAllowed { requested, allowed })
    }
}

fn is_active(
    component: Option<&ComponentId>,
    condition: Option<&Condition>,
    selected: &BTreeSet<ComponentId>,
) -> bool {
    if let Some(id) = component
        && !selected.contains(id)
    {
        return false;
    }
    match condition {
        None => true,
        Some(condition) => condition.evaluate(selected),
    }
}

#[allow(clippy::too_many_arguments)]
fn validate_active_collisions(
    files: &[PlannedFile],
    shortcuts: &[PlannedShortcut],
    path_entries: &[PlannedPathEntry],
    services: &[PlannedService],
    protocols: &[PlannedProtocol],
    file_types: &[PlannedFileType],
    actions: &[PlannedExternalAction],
) -> Result<(), PlanError> {
    let mut file_keys = BTreeSet::new();
    for file in files {
        if !file_keys.insert(file.key.clone()) {
            return Err(PlanError::ActiveFileCollision {
                destination: file.destination.to_string(),
            });
        }
    }

    let mut shortcut_keys = BTreeSet::new();
    for shortcut in shortcuts {
        if !shortcut_keys.insert(shortcut.key.clone()) {
            return Err(PlanError::ActiveShortcutCollision {
                location: shortcut.location.to_string(),
                name: shortcut.name.to_string(),
            });
        }
    }

    let mut path_keys = BTreeSet::new();
    for entry in path_entries {
        if !path_keys.insert(entry.key.clone()) {
            return Err(PlanError::ActivePathCollision {
                value: entry.value.to_string(),
            });
        }
    }

    let mut service_keys = BTreeSet::new();
    for service in services {
        if !service_keys.insert(service.key.clone()) {
            return Err(PlanError::ActiveServiceCollision {
                id: service.id.to_string(),
            });
        }
    }

    let mut protocol_keys = BTreeSet::new();
    for protocol in protocols {
        if !protocol_keys.insert(protocol.key.clone()) {
            return Err(PlanError::ActiveProtocolCollision {
                scheme: protocol.scheme.to_string(),
            });
        }
    }

    let mut file_type_ids = BTreeSet::new();
    let mut file_type_exts = BTreeSet::new();
    for file_type in file_types {
        if !file_type_ids.insert(file_type.key.clone()) {
            return Err(PlanError::ActiveFileTypeCollision {
                id: file_type.id.to_string(),
            });
        }
        if !file_type_exts.insert(file_type.extension.clone()) {
            return Err(PlanError::ActiveExtensionCollision {
                extension: file_type.extension.to_string(),
            });
        }
    }

    let mut action_keys = BTreeSet::new();
    for action in actions {
        if !action_keys.insert(action.key.clone()) {
            return Err(PlanError::ActiveActionCollision {
                id: action.id.to_string(),
            });
        }
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn summarize(
    files: &[PlannedFile],
    shortcuts: &[PlannedShortcut],
    path_entries: &[PlannedPathEntry],
    services: &[PlannedService],
    protocols: &[PlannedProtocol],
    file_types: &[PlannedFileType],
    actions: &[PlannedExternalAction],
    selected_component_count: usize,
    scope: SelectedScope,
) -> Result<PlanSummary, PlanError> {
    let mut install_bytes = 0u64;
    for file in files {
        install_bytes = install_bytes
            .checked_add(file.size)
            .ok_or(PlanError::SizeOverflow)?;
    }

    let resource_count = shortcuts.len()
        + path_entries.len()
        + services.len()
        + protocols.len()
        + file_types.len()
        + actions.len();

    let requires_elevation = scope == SelectedScope::Machine
        || files.iter().any(|r| r.privilege == Privilege::Machine)
        || shortcuts.iter().any(|r| r.privilege == Privilege::Machine)
        || path_entries
            .iter()
            .any(|r| r.privilege == Privilege::Machine)
        || services.iter().any(|r| r.privilege == Privilege::Machine)
        || protocols.iter().any(|r| r.privilege == Privilege::Machine)
        || file_types.iter().any(|r| r.privilege == Privilege::Machine)
        || actions.iter().any(|r| r.privilege == Privilege::Machine);

    Ok(PlanSummary {
        file_count: files.len(),
        install_bytes,
        selected_component_count,
        resource_count,
        opaque_action_count: actions.len(),
        requires_elevation,
    })
}
