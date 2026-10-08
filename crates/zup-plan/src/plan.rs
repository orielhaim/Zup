use std::collections::{BTreeMap, BTreeSet};

use tracing::{info, info_span};
use zup_core::{BuildPlan, TargetBuildPlan};
use zup_core::{
    ComponentId, Condition, InstallScope, PluginBinding, PluginId, Privilege, ResourceKey,
};

use crate::error::{PlanError, SelectedScope};
use crate::plan_types::InstallPlan;
use crate::plugins::{
    CancellationQuery, CollisionIndex, MAX_PLUGIN_STRING_BYTES, PlannedInstallation,
    PluginExecutor, PluginPlanningContext, merge_plugin_proposal, sort_resources, summarize_plan,
};
use crate::request::PlanRequest;
use crate::resolve::{resolve_install_directory, resolve_template};
use crate::resources::{
    PlannedFile, PlannedFileAssociation, PlannedLauncher, PlannedPathEntry, PlannedPrerequisite,
    PlannedProtocol, PlannedService,
};
use crate::select::select_components;

struct PreparedPlan {
    plan: InstallPlan,
    active_plugins: Vec<PluginBinding>,
}

pub fn plan(build: &BuildPlan, request: &PlanRequest) -> Result<InstallPlan, PlanError> {
    let target = requested_target(build, request)?;
    let prepared = prepare_plan(target, request)?;
    if let Some(binding) = prepared.active_plugins.first() {
        return Err(PlanError::PluginPlanningRequired {
            plugin_id: binding.id.clone(),
        });
    }
    Ok(prepared.plan)
}

pub fn plan_without_plugins(
    build: &BuildPlan,
    request: &PlanRequest,
) -> Result<InstallPlan, PlanError> {
    Ok(prepare_plan(requested_target(build, request)?, request)?.plan)
}

fn requested_target<'a>(
    build: &'a BuildPlan,
    request: &PlanRequest,
) -> Result<&'a TargetBuildPlan, PlanError> {
    build
        .target_by_triple(&request.target)
        .ok_or_else(|| PlanError::UnknownBuildTarget {
            target: request.target.clone(),
        })
}

pub fn plan_with_plugins<E>(
    build: &BuildPlan,
    request: &PlanRequest,
    executor: &mut E,
    cancellation: &dyn CancellationQuery,
) -> Result<PlannedInstallation, PlanError>
where
    E: PluginExecutor + ?Sized,
{
    let target = requested_target(build, request)?;
    let mut prepared = prepare_plan(target, request)?;
    if executor.target() != &prepared.plan.target {
        return Err(PlanError::PluginTargetMismatch {
            expected: prepared.plan.target.clone(),
            found: executor.target().clone(),
        });
    }
    if prepared.active_plugins.is_empty() {
        return Ok(PlannedInstallation {
            plan: prepared.plan,
            generated_files: Vec::new(),
        });
    }

    let context = PluginPlanningContext {
        app: prepared.plan.app.clone(),
        install_directory: prepared.plan.install_directory.clone(),
        scope: prepared.plan.scope,
        selected_components: prepared.plan.selected_components.clone(),
        target: prepared.plan.target.clone(),
    };
    let mut collisions = CollisionIndex::from_plan(&prepared.plan)?;
    let mut generated_files = Vec::new();
    let mut total_resources = 0usize;
    let mut total_generated_bytes = 0u64;

    for binding in &prepared.active_plugins {
        if cancellation.is_cancelled() {
            return Err(PlanError::PluginCancelled {
                plugin_id: binding.id.clone(),
            });
        }
        let proposal = executor
            .plan(binding, &context, cancellation)
            .map_err(|failure| PlanError::PluginExecutionFailed {
                plugin_id: binding.id.clone(),
                failure,
            })?;
        merge_plugin_proposal(
            &mut prepared.plan,
            &mut generated_files,
            &mut collisions,
            binding,
            proposal,
            &mut total_resources,
            &mut total_generated_bytes,
        )?;
    }

    sort_resources(&mut prepared.plan, &mut generated_files);
    prepared.plan.summary =
        summarize_plan(&prepared.plan, prepared.plan.selected_components.len())?;
    Ok(PlannedInstallation {
        plan: prepared.plan,
        generated_files,
    })
}

fn prepare_plan(build: &TargetBuildPlan, request: &PlanRequest) -> Result<PreparedPlan, PlanError> {
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

    let raw_directory = if let Some(directory) = &request.install_directory {
        if !installer.install.allow_directory_override {
            return Err(PlanError::InstallDirectoryOverrideNotAllowed);
        }
        directory.clone()
    } else {
        match (scope, installer.install.scope) {
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
        }
    };

    let install_directory = resolve_install_directory(&raw_directory, &installer.app)?;
    let default_privilege = scope.authorization();

    let prerequisites = installer
        .prerequisites
        .iter()
        .filter(|prerequisite| {
            is_active(
                prerequisite.component.as_ref(),
                prerequisite.when.as_ref(),
                &selected_set,
            )
        })
        .map(PlannedPrerequisite::from_prerequisite)
        .collect::<Vec<_>>();

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
            privilege: default_privilege,
            executable: file.executable,
        });
    }

    let mut launchers = Vec::new();
    for launcher in &installer.launchers {
        if !is_active(
            launcher.component.as_ref(),
            launcher.when.as_ref(),
            &selected_set,
        ) {
            continue;
        }
        let name = launcher.name.clone();
        launchers.push(PlannedLauncher {
            key: ResourceKey::Launcher {
                location: launcher.location,
                name: name.to_string(),
            },
            location: launcher.location,
            name,
            target: resolve_template(&launcher.target, &installer.app, &install_directory)?,
            arguments: launcher.arguments.clone(),
            working_directory: launcher
                .working_directory
                .as_ref()
                .map(|dir| resolve_template(dir, &installer.app, &install_directory))
                .transpose()?,
            privilege: default_privilege,
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
            scope,
            privilege: default_privilege,
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
            privilege: Privilege::System,
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
            scope,
            privilege: default_privilege,
        });
    }

    let mut file_associations = Vec::new();
    for file_association in &installer.file_associations {
        if !is_active(None, file_association.when.as_ref(), &selected_set) {
            continue;
        }
        file_associations.push(PlannedFileAssociation {
            key: ResourceKey::FileAssociation {
                id: file_association.id.clone(),
            },
            extension: file_association.extension.clone(),
            id: file_association.id.clone(),
            description: file_association.description.clone(),
            executable: resolve_template(
                &file_association.executable,
                &installer.app,
                &install_directory,
            )?,
            scope,
            privilege: default_privilege,
        });
    }

    validate_active_collisions(
        &files,
        &launchers,
        &path_entries,
        &services,
        &protocols,
        &file_associations,
    )?;

    let mut plan = InstallPlan {
        app: installer.app.clone(),
        target: installer.target.clone(),
        scope,
        install_directory,
        selected_components: selected,
        prerequisites,
        files,
        launchers,
        path_entries,
        services,
        protocols,
        file_associations,
        summary: crate::plan_types::PlanSummary {
            file_count: 0,
            install_bytes: 0,
            selected_component_count: 0,
            resource_count: 0,
            requires_authorization: false,
            prerequisite_count: 0,
            download_bytes: 0,
        },
    };
    plan.summary = summarize_plan(&plan, plan.selected_components.len())?;

    for plugin in &installer.plugins {
        if plugin.id.as_str().len() > MAX_PLUGIN_STRING_BYTES {
            return Err(PlanError::PluginResourceRejected {
                plugin_id: plugin.id.clone(),
                resource: "plugin id".to_owned(),
                reason: format!("plugin id exceeds {} bytes", MAX_PLUGIN_STRING_BYTES),
            });
        }
    }

    let mut plugin_ids: BTreeMap<String, PluginId> = BTreeMap::new();
    let mut active_plugins = Vec::new();
    for plugin in &installer.plugins {
        if !is_active(
            plugin.component.as_ref(),
            plugin.when.as_ref(),
            &selected_set,
        ) {
            continue;
        }
        let identity = plugin.id.as_str().to_owned();
        if let Some(existing) = plugin_ids.get(&identity) {
            return Err(PlanError::PluginResourceCollision {
                plugin_id: Some(plugin.id.clone()),
                resource: "plugin id".to_owned(),
                identity,
                existing_plugin_id: Some(existing.clone()),
                existing_resource: "plugin declaration".to_owned(),
            });
        }
        plugin_ids.insert(identity, plugin.id.clone());
        active_plugins.push(plugin.clone());
    }

    info!(
        active_files = plan.files.len(),
        active_resources = plan.launchers.len()
            + plan.path_entries.len()
            + plan.services.len()
            + plan.protocols.len()
            + plan.file_associations.len(),
        install_bytes = plan.summary.install_bytes,
        requires_authorization = plan.summary.requires_authorization,
        "plan complete"
    );

    Ok(PreparedPlan {
        plan,
        active_plugins,
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

fn validate_active_collisions(
    files: &[PlannedFile],
    launchers: &[PlannedLauncher],
    path_entries: &[PlannedPathEntry],
    services: &[PlannedService],
    protocols: &[PlannedProtocol],
    file_associations: &[PlannedFileAssociation],
) -> Result<(), PlanError> {
    let mut file_keys = BTreeSet::new();
    for file in files {
        if !file_keys.insert(file.key.clone()) {
            return Err(PlanError::ActiveFileCollision {
                destination: file.destination.to_string(),
            });
        }
    }

    let mut launcher_keys = BTreeSet::new();
    for launcher in launchers {
        if !launcher_keys.insert(launcher.key.clone()) {
            return Err(PlanError::ActiveLauncherCollision {
                location: launcher.location.to_string(),
                name: launcher.name.to_string(),
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

    let mut file_association_ids = BTreeSet::new();
    let mut file_association_exts = BTreeSet::new();
    for file_association in file_associations {
        if !file_association_ids.insert(file_association.key.clone()) {
            return Err(PlanError::ActiveFileAssociationCollision {
                id: file_association.id.to_string(),
            });
        }
        if !file_association_exts.insert(file_association.extension.clone()) {
            return Err(PlanError::ActiveExtensionCollision {
                extension: file_association.extension.to_string(),
            });
        }
    }

    Ok(())
}
