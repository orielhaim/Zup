use std::collections::{BTreeMap, BTreeSet};

use tracing::{info, info_span};
use zup_core::{BuildPlan, TargetBuildPlan};
use zup_core::{
    ComponentId, Condition, InstallScope, PluginBinding, PluginId, Privilege, ResourceKey,
};

use crate::error::{PlanError, SelectedScope};
use crate::plan_types::DesiredState;
use crate::plugins::{
    CancellationQuery, CollisionIndex, MAX_PLUGIN_STRING_BYTES, PlannedInstallation,
    PluginExecutor, PluginPlanningContext, merge_plugin_proposal, sort_resources,
};
use crate::request::PlanRequest;
use crate::resolve::{resolve_install_directory, resolve_template};
use crate::resources::{
    DesiredFile, DesiredFileAssociation, DesiredLauncher, DesiredPathEntry, DesiredProtocol,
    DesiredService, PlannedPrerequisite,
};
use crate::select::select_components;

struct PreparedPlan {
    plan: DesiredState,
    active_plugins: Vec<PluginBinding>,
}

fn requested_target<'a>(
    build: &'a BuildPlan,
    request: &PlanRequest,
) -> Result<&'a TargetBuildPlan, PlanError> {
    build
        .by_target(&request.target)
        .ok_or(PlanError::UnknownBuildTarget {
            target: request.target,
        })
}

pub fn plan(
    build: &BuildPlan,
    request: &PlanRequest,
    mut executor: Option<&mut dyn PluginExecutor>,
    cancellation: &dyn CancellationQuery,
) -> Result<PlannedInstallation, PlanError> {
    let target = requested_target(build, request)?;
    let mut prepared = prepare_plan(target, request)?;
    if let Some(executor) = executor.as_ref()
        && executor.target() != &prepared.plan.target
    {
        return Err(PlanError::PluginTargetMismatch {
            expected: prepared.plan.target,
            found: *executor.target(),
        });
    }
    if cancellation.is_cancelled() {
        return Err(PlanError::Cancelled);
    }
    let context = PluginPlanningContext {
        app: prepared.plan.app.clone(),
        install_directory: prepared.plan.install_directory.clone(),
        scope: prepared.plan.scope,
        selected_components: prepared.plan.selected_components.clone(),
        target: prepared.plan.target,
    };
    let mut collisions = CollisionIndex::from_plan(&prepared.plan)?;
    let mut generated_files = Vec::new();
    let mut total_resources = 0usize;
    let mut total_generated_bytes = 0u64;

    for binding in &prepared.active_plugins {
        if cancellation.is_cancelled() {
            return Err(PlanError::Cancelled);
        }
        let proposal = executor
            .as_deref_mut()
            .ok_or_else(|| PlanError::PluginPlanningRequired {
                plugin_id: binding.id.clone(),
            })?
            .plan(binding, &context, cancellation)
            .map_err(|failure| match failure {
                crate::PluginFailure::Cancelled => PlanError::Cancelled,
                failure => PlanError::PluginExecutionFailed {
                    plugin_id: binding.id.clone(),
                    failure,
                },
            })?;
        if cancellation.is_cancelled() {
            return Err(PlanError::Cancelled);
        }
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

    let service_names: BTreeMap<_, _> = prepared
        .plan
        .services
        .iter()
        .map(|service| {
            (
                ResourceKey::Service {
                    id: service.id.clone(),
                }
                .canonical_for(&prepared.plan.target),
                service.name.to_string(),
            )
        })
        .collect();
    for service in &mut prepared.plan.services {
        service.dependency_names = service
            .requires_services
            .iter()
            .map(|id| {
                service_names
                    .get(
                        &ResourceKey::Service { id: id.clone() }
                            .canonical_for(&prepared.plan.target),
                    )
                    .cloned()
                    .unwrap_or_default()
            })
            .collect();
    }
    prepared.plan.canonicalize_keys();
    sort_resources(&mut prepared.plan, &mut generated_files);
    prepared.plan.summary()?;
    prepared.plan.validate()?;
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
    for group in &installer.component_groups {
        if group.selection == zup_core::SelectionRequirement::Explicit
            && !installer.components.iter().any(|component| {
                !component.required
                    && component.group.as_ref() == Some(&group.id)
                    && selected_set.contains(&component.id)
            })
        {
            return Err(PlanError::ComponentGroupSelectionRequired {
                group: group.id.to_string(),
            });
        }
    }

    info!(
        default_count = installer.components.iter().filter(|c| c.default).count(),
        enable_count = request.components.enable.len(),
        disable_count = request.components.disable.len(),
        selected_count = selected.len(),
        "component selection complete"
    );

    let raw_directory = if let Some(directory) = &request.install_directory {
        match directory {
            crate::InstallDirectory::Override(_) if !installer.install.allow_directory_override => {
                return Err(PlanError::InstallDirectoryOverrideNotAllowed);
            }
            crate::InstallDirectory::Override(path) | crate::InstallDirectory::Installed(path) => {
                path.clone()
            }
        }
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

    let generation_files: Vec<_> = build
        .files
        .iter()
        .filter(|file| {
            is_active(
                file.component.as_ref(),
                file.condition.as_ref(),
                &selected_set,
            )
        })
        .filter_map(|file| match &file.placement {
            zup_core::PayloadPlacement::Generation { path } => Some(zup_core::GenerationFile {
                path: path.clone(),
                size: file.size,
                sha256: file.sha256,
                executable: file.executable,
            }),
            zup_core::PayloadPlacement::Fixed { .. } => None,
        })
        .collect();
    let generation = if generation_files.is_empty() {
        None
    } else {
        let descriptor = zup_core::GenerationDescriptor::new(installer.target, generation_files)?;
        let reused = request
            .previous
            .and_then(|previous| previous.materialization)
            .filter(|previous| {
                request.generation_policy == crate::GenerationPolicy::ReuseVerified
                    && previous.tree == descriptor.tree_id()
            });
        let materialization = reused.map_or(request.materialization, |previous| previous.id);
        let suffix = zup_core::RelativePath::new(format!(
            "{}/{}",
            zup_core::DEPLOYMENTS_DIRECTORY,
            materialization
        ))
        .expect("UUID and static deployment directory form a relative path");
        let sources = build
            .files
            .iter()
            .filter(|file| {
                is_active(
                    file.component.as_ref(),
                    file.condition.as_ref(),
                    &selected_set,
                )
            })
            .filter_map(|file| match &file.placement {
                zup_core::PayloadPlacement::Generation { path } => Some((
                    path.clone(),
                    crate::GenerationSource {
                        key: zup_core::ResourceKey::File {
                            id: zup_core::FileResourceId::Payload {
                                id: file.id.clone(),
                            },
                        },
                        source_relative: file.source_relative.clone(),
                    },
                )),
                _ => None,
            })
            .collect();
        Some(crate::PlannedGeneration {
            installation: request.installation,
            id: materialization,
            descriptor,
            directory: install_directory.join_relative(&suffix),
            publication: crate::GenerationPublication::Application {
                previous: request.previous,
                revision: match request.previous {
                    Some(previous) => previous.revision.next()?,
                    None => zup_core::ActivationRevision::FIRST,
                },
            },
            preparation: if reused.is_some() {
                crate::GenerationPreparation::ReuseVerified
            } else {
                crate::GenerationPreparation::Materialize
            },
            sources,
        })
    };
    let payload_directory = generation.as_ref().map(|generation| &generation.descriptor);
    let payload_root = zup_core::Template::parse("${payload}").expect("static template");
    let resolve = |template: &zup_core::Template| {
        resolve_template(
            template,
            &installer.app,
            &install_directory,
            payload_directory,
        )
    };
    let resolve_payload = |file: &zup_core::ResolvedFile| {
        let template = match &file.placement {
            zup_core::PayloadPlacement::Fixed { path, .. } => path.clone(),
            zup_core::PayloadPlacement::Generation { path } => payload_root.join_relative(path),
        };
        resolve(&template)
    };
    let mut files = Vec::new();
    for file in &build.files {
        if !is_active(
            file.component.as_ref(),
            file.condition.as_ref(),
            &selected_set,
        ) {
            continue;
        }
        let destination = resolve_payload(file)?;
        files.push(DesiredFile {
            key: ResourceKey::File {
                id: zup_core::FileResourceId::Payload {
                    id: file.id.clone(),
                },
            },
            source_relative: file.source_relative.clone(),
            placement: match file.placement {
                zup_core::PayloadPlacement::Generation { .. } => {
                    zup_core::PayloadPlacement::Generation { path: destination }
                }
                zup_core::PayloadPlacement::Fixed { replacement, .. } => {
                    zup_core::PayloadPlacement::Fixed {
                        path: destination,
                        replacement,
                    }
                }
            },
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
        launchers.push(DesiredLauncher {
            key: ResourceKey::Launcher {
                id: zup_core::DeclaredResourceId::Application {
                    id: launcher.id.clone(),
                },
            },
            location: launcher.location,
            name,
            target: resolve(&launcher.target)?,
            arguments: launcher.arguments.clone(),
            working_directory: launcher
                .working_directory
                .as_ref()
                .map(resolve)
                .transpose()?,
            privilege: default_privilege,

            launcher_path: (),
        });
    }

    let mut path_entries = Vec::new();
    for entry in &installer.path {
        if !is_active(entry.component.as_ref(), entry.when.as_ref(), &selected_set) {
            continue;
        }
        let value = resolve(&entry.value)?;
        path_entries.push(DesiredPathEntry {
            key: ResourceKey::PathEntry {
                id: zup_core::DeclaredResourceId::Application {
                    id: entry.id.clone(),
                },
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
        let mut requires_files = service
            .requires_files
            .iter()
            .map(resolve)
            .collect::<Result<Vec<_>, _>>()?;
        for component in &service.requires_components {
            if !selected_set.contains(component) {
                return Err(crate::ContractError::Capability(format!(
                    "service `{}` requires unselected component `{component}`",
                    service.id
                ))
                .into());
            }
            for file in build.files.iter().filter(|file| {
                file.component.as_ref() == Some(component)
                    && is_active(
                        file.component.as_ref(),
                        file.condition.as_ref(),
                        &selected_set,
                    )
            }) {
                requires_files.push(resolve_payload(file)?);
            }
        }
        requires_files.sort_by_key(ToString::to_string);
        requires_files.dedup();
        services.push(DesiredService {
            state_directories: installer
                .install
                .data
                .iter()
                .filter_map(|data| match &data.location {
                    zup_core::PersistentLocation::Service { service: id }
                        if id == &service.id
                            && matches!(
                                service.execution,
                                zup_core::ServiceExecution::SystemdUser { .. }
                                    | zup_core::ServiceExecution::SystemdDynamicUser
                            ) =>
                    {
                        Some(crate::service_state_name(&installer.app.id, id, &data.id))
                    }
                    _ => None,
                })
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect(),
            requires_services: service.requires_services.clone(),
            dependency_names: Vec::new(),
            failure: service.failure,
            execution: service.execution.clone(),
            lifecycle: service.lifecycle.resolved(&service.execution),
            readiness: service.readiness,
            requires_files,
            key: ResourceKey::Service {
                id: service.id.clone(),
            },
            id: service.id.clone(),
            name: service
                .execution
                .native_name(&installer.app.id, &service.id, service.native_name.as_ref())
                .map_err(|reason| {
                    crate::ContractError::Capability(format!("service {}: {reason}", service.id))
                })?,
            display_name: Some(service.display_name.clone().unwrap_or_else(|| {
                zup_core::NonEmptyString::new(service.id.as_str())
                    .expect("service identity is nonempty")
            })),
            start: service.start,
            privilege: Privilege::System,
            command: zup_core::CommandSpec::new(
                resolve(&service.binary)?,
                service.arguments.clone(),
            ),
        });
    }

    let mut protocols = Vec::new();
    for protocol in &installer.protocols {
        if !is_active(None, protocol.when.as_ref(), &selected_set) {
            continue;
        }
        protocols.push(DesiredProtocol {
            key: ResourceKey::Protocol {
                scheme: protocol.scheme.clone(),
            },
            scheme: protocol.scheme.clone(),
            scope,
            privilege: default_privilege,
            command: zup_core::CommandSpec::new(
                resolve(&protocol.executable)?,
                protocol.args.clone(),
            ),
        });
    }

    let mut file_associations = Vec::new();
    for file_association in &installer.file_associations {
        if !is_active(None, file_association.when.as_ref(), &selected_set) {
            continue;
        }
        file_associations.push(DesiredFileAssociation {
            key: ResourceKey::FileAssociation {
                id: file_association.id.clone(),
            },
            extension: file_association.extension.clone(),
            id: file_association.id.clone(),
            description: file_association.description.clone(),
            scope,
            privilege: default_privilege,
            command: zup_core::CommandSpec::new(resolve(&file_association.executable)?, Vec::new()),
        });
    }

    for directory in &installer.install.data {
        if directory.location.path().is_some_and(|path| {
            path.contains_variable(zup_core::Variable::Payload)
                || path.contains_variable(zup_core::Variable::AppVersion)
        }) {
            return Err(crate::ContractError::Capability(
                "persistent data paths must remain stable across deployments".into(),
            )
            .into());
        }
    }
    let data = installer
        .install
        .data
        .iter()
        .map(|directory| directory.map_path(resolve))
        .collect::<Result<_, _>>()?;
    let plan = DesiredState {
        generation_readers: installer.install.generation_readers,
        requirements: (),
        maintenance: None,
        installation: request.installation,
        generation,
        app: installer.app.clone(),
        target: installer.target,
        scope,
        install_directory,
        selected_components: selected,
        data,
        prerequisites,
        files,
        launchers,
        path_entries,
        services,
        protocols,
        file_associations,

        preset: None,
    };
    let summary = plan.summary()?;

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
            return Err(PlanError::ResourceCollision {
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
        install_bytes = summary.install_bytes,
        requires_authorization = summary.requires_authorization,
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
