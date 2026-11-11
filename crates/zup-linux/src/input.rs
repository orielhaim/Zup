#[cfg(all(test, feature = "test-support"))]
use zup_core::Privilege;
use zup_plan::{DeploymentEffects, FileRemoval, NativeCacheKind, NativeCacheOperation};
use zup_plan::{ExecutionPlan, FileOperationKind, IntegrationChangeKind, OwnedResource};

use crate::error::{ExecError, PlanError};

pub fn snapshot_services(
    target: &zup_plan::BoundDeployment,
    manager: &mut dyn crate::systemd::SystemdManager,
    systemd: &crate::machine::SystemdRoots,
) -> Result<Vec<zup_plan::ObservedService>, ExecError> {
    crate::service_ops::snapshot_services(target, manager, systemd)
}

pub fn requires_service_manager(
    target: Option<&zup_plan::BoundDeployment>,
    ledger: Option<&zup_plan::InstallationRecord>,
) -> bool {
    target.is_some_and(|target| !target.services.is_empty()) || ledger_has_services(ledger)
}

pub fn ledger_has_services(ledger: Option<&zup_plan::InstallationRecord>) -> bool {
    ledger.is_some_and(|ledger| {
        ledger
            .current
            .resources
            .values()
            .any(|owned| matches!(owned, OwnedResource::Service(_)))
    })
}

pub fn compile_execution_plan(
    execution: &ExecutionPlan,
    target: Option<&zup_plan::BoundDeployment>,
) -> Result<DeploymentEffects, PlanError> {
    let input = compile_files_only(execution, target)?;
    for (kind, count) in [
        ("launcher", execution.launchers.len()),
        ("PATH entry", execution.path_entries.len()),
        ("service", execution.services.len()),
        ("URI protocol", execution.protocols.len()),
        ("file association", execution.file_associations.len()),
    ] {
        if count > 0 {
            return Err(PlanError::UnsupportedKind { kind });
        }
    }
    for removal in &execution.removals {
        if !matches!(
            &removal.owned,
            OwnedResource::File { .. } | OwnedResource::Backend { .. }
        ) {
            return Err(PlanError::UnsupportedKind {
                kind: "a non-file removal",
            });
        }
    }
    Ok(input)
}

pub(crate) fn compile_files_only(
    execution: &ExecutionPlan,
    target: Option<&zup_plan::BoundDeployment>,
) -> Result<DeploymentEffects, PlanError> {
    let binding = execution.validate_desired(target)?;
    let mut input = DeploymentEffects::new(binding.installation, binding.requirements.clone());
    input.observed_transaction = binding.observed_transaction;
    input.observed = binding.observed_activation;
    input.selected_components = execution.selected_components.clone();
    input.install_directory = execution.install_directory.clone();
    input.transition = execution.transition;
    input.preset = if execution.transition.is_remove() {
        None
    } else {
        target.and_then(|target| target.preset.clone())
    };

    let mut caches = std::collections::BTreeMap::new();
    for file in &execution.files {
        if !matches!(file.kind, FileOperationKind::NoOp) {
            add_cache_directories(&mut caches, &file.key, &file.destination);
        }
        input.files.push(file.clone());
    }

    for removal in &execution.removals {
        if let OwnedResource::File {
            instance,
            destination,
            sha256,
            size,
            created_directories,
            ..
        } = &removal.owned
        {
            if removal.kind != zup_plan::RemovalKind::Drift {
                add_cache_directories(&mut caches, &removal.key, destination);
            }
            input.removals.push(FileRemoval {
                instance: *instance,
                key: removal.key.clone(),
                kind: removal.kind,
                scope: removal.scope,
                privilege: removal.privilege,
                destination: destination.clone(),
                sha256: *sha256,
                size: *size,
                created_directories: created_directories.clone(),
            });
            input.retired_keys.push(removal.key.clone());
        }
    }

    for ((kind, _), directory) in caches {
        let mut operation = NativeCacheOperation {
            kind,
            directory,
            sources: Default::default(),
        };
        for file in input
            .files
            .iter()
            .filter(|file| file.kind != FileOperationKind::NoOp)
        {
            if operation.includes_source(&file.destination) {
                operation.sources.insert(file.key.clone());
            }
        }
        for removal in input
            .removals
            .iter()
            .filter(|removal| removal.kind != zup_plan::RemovalKind::Drift)
        {
            if operation.includes_source(&removal.destination) {
                operation.sources.insert(removal.key.clone());
            }
        }
        input.native_caches.push(operation);
    }

    input.bind_generations(target)?;
    Ok(input)
}

fn add_cache_directories(
    caches: &mut std::collections::BTreeMap<(NativeCacheKind, String), zup_core::TargetPath>,
    key: &zup_core::ResourceKey,
    destination: &zup_core::TargetPath,
) {
    let (mime, desktop) = match key {
        zup_core::ResourceKey::File {
            id: zup_core::FileResourceId::Native { id },
        } => match id {
            zup_core::NativeFileId::MimeDefinitions => (true, false),
            zup_core::NativeFileId::DesktopEntry {
                location: zup_core::LauncherLocation::Menu,
                ..
            }
            | zup_core::NativeFileId::ProtocolHandler
            | zup_core::NativeFileId::FileAssociationHandler => (false, true),
            _ => (false, false),
        },
        _ => (false, false),
    };
    let candidate = if mime {
        destination
            .parent()
            .and_then(|packages| packages.parent())
            .map(|path| (NativeCacheKind::Mime, path))
    } else if desktop {
        destination
            .parent()
            .map(|path| (NativeCacheKind::DesktopApplications, path))
    } else {
        None
    };
    if let Some((kind, directory)) = candidate {
        caches.insert((kind, directory.identity()), directory);
    }
}

pub struct ServiceCompilation<'a> {
    pub ledger: Option<&'a zup_plan::InstallationRecord>,
    pub roots: &'a crate::locations::MachineRoots,

    pub systemd: &'a crate::machine::SystemdRoots,

    pub manager: &'a mut dyn crate::systemd::SystemdManager,
}

pub fn compile_machine_execution_plan(
    execution: &ExecutionPlan,
    target: Option<&zup_plan::BoundDeployment>,
    compilation: ServiceCompilation<'_>,
) -> Result<DeploymentEffects, PlanError> {
    let ServiceCompilation {
        ledger,
        roots,
        systemd,
        manager,
    } = compilation;
    if execution.validate_desired(target)?.scope != zup_core::SelectedScope::Machine {
        return Err(PlanError::UnsupportedKind {
            kind: "a machine service plan in user scope",
        });
    }

    let mut files_only = execution.clone();
    files_only.services.clear();
    let mut files_only_removals = Vec::new();
    let mut service_removals = Vec::new();
    for removal in files_only.removals.drain(..) {
        match &removal.owned {
            OwnedResource::Service(_) => service_removals.push(removal),
            _ => files_only_removals.push(removal),
        }
    }
    files_only.removals = files_only_removals;
    let mut input = compile_files_only(&files_only, target)?;

    for (kind, count) in [
        ("launcher", execution.launchers.len()),
        ("PATH entry", execution.path_entries.len()),
        ("URI protocol", execution.protocols.len()),
        ("file association", execution.file_associations.len()),
    ] {
        if count > 0 {
            return Err(PlanError::UnsupportedKind { kind });
        }
    }

    let mut target_files = std::collections::BTreeMap::new();
    for file in target.into_iter().flat_map(|target| &target.files) {
        target_files.insert(file.destination().to_string(), file.executable);
    }

    if !execution.services.is_empty() || !service_removals.is_empty() {
        crate::service_ops::require_exec_baseline(manager).map_err(into_service)?;
    }

    let affected_services = execution.affected_services(ledger);
    for op in &execution.services {
        if matches!(
            op.kind,
            IntegrationChangeKind::Drift | IntegrationChangeKind::Conflict
        ) {
            return Err(PlanError::Service {
                unit: op.name.clone(),
                reason:
                    "service registration is contested; resolve its ownership before activation"
                        .into(),
            });
        }
        let owned = ledger.and_then(|ledger| ledger.current.resources.get(&op.key));
        let payload_changed = affected_services.contains(&op.key);
        if (payload_changed || op.kind != IntegrationChangeKind::NoOp)
            && (owned.is_some() || op.previous == zup_plan::ObservedServiceState::Absent)
        {
            let unit = op.name.clone();
            let generation = manager.generation().map_err(|error| PlanError::Service {
                unit: unit.clone(),
                reason: error.to_string(),
            })?;
            let runtime = Some(manager.runtime(&unit).map_err(|error| PlanError::Service {
                unit: unit.clone(),
                reason: error.to_string(),
            })?);
            if op.previous == zup_plan::ObservedServiceState::Absent
                && runtime.as_ref().is_some_and(|runtime| {
                    runtime.state != zup_plan::ServiceRuntimeState::Stopped || runtime.pending_job
                })
            {
                return Err(PlanError::Service {
                    unit,
                    reason:
                        "a missing service definition still has an active or pending native runtime"
                            .into(),
                });
            }
            let stopped_damaged = op.previous == zup_plan::ObservedServiceState::InvalidService
                && runtime.as_ref().is_some_and(|runtime| {
                    runtime.state == zup_plan::ServiceRuntimeState::Stopped && !runtime.pending_job
                });
            if stopped_damaged {
                let Some(OwnedResource::Service(zup_plan::OwnedService { installed, .. })) = owned
                else {
                    return Err(PlanError::Service {
                        unit,
                        reason: "damaged registration has no service ownership".into(),
                    });
                };
                if op.kind != IntegrationChangeKind::RepairOwned
                    || **installed != op.desired_configuration()
                {
                    return Err(PlanError::Service {
                        unit,
                        reason: "damaged registration requires explicit owned repair".into(),
                    });
                }
                input
                    .service_controls
                    .push(zup_plan::ServiceControlOperation {
                        requires_files: op
                            .requires_files
                            .iter()
                            .chain(owned.into_iter().flat_map(|resource| match resource {
                                zup_plan::OwnedResource::Service(service) => {
                                    service.requires_files.iter()
                                }
                                _ => [].iter(),
                            }))
                            .cloned()
                            .collect(),
                        registration: zup_core::NativeServiceRegistration::SourceFile,
                        intent: zup_plan::ServiceControlIntent::Reobserve,
                        key: op.key.clone(),
                        native_name: unit,
                        privilege: op.privilege,
                        stage: zup_plan::ServiceControlStage::Restore,
                        before_configuration: installed.as_ref().clone(),
                        configuration: installed.as_ref().clone(),
                        owned_configuration: installed.as_ref().clone(),
                        before: zup_plan::ServiceTerminalState::Stopped,
                        after: zup_plan::ServiceTerminalState::Stopped,
                        manager_generation: generation,
                        before_execution_generation: "stopped".into(),
                        process_scope: zup_plan::ServiceScopeObservation::NativeManager,
                        timeouts: op.lifecycle.timeouts_for(&op.execution),
                        dependencies: Vec::new(),
                    });
            } else {
                let execution_generation = runtime
                    .map_or_else(|| "stopped".into(), |runtime| runtime.execution_generation);
                input.service_controls.extend(
                    zup_plan::plan_service_controls(
                        op,
                        owned,
                        payload_changed,
                        unit.clone(),
                        generation,
                        execution_generation,
                        zup_plan::ServiceScopeObservation::NativeManager,
                    )
                    .map_err(|reason| PlanError::Service { unit, reason })?,
                );
            }
        }
        let unit = op.name.clone();
        let canonical =
            crate::machine::authorize_systemd_unit(&unit, systemd).map_err(|error| {
                PlanError::Service {
                    unit: unit.clone(),
                    reason: error.to_string(),
                }
            })?;
        let previous_source =
            crate::service_ops::read_canonical_source(&canonical).map_err(into_service)?;
        if previous_source.is_none() {
            crate::service_ops::validate_missing_source_residue(&unit, &canonical, owned)
                .map_err(into_service)?;
        } else if op.kind == IntegrationChangeKind::Create {
            return Err(PlanError::Service {
                unit,
                reason: "an unowned service definition already exists".into(),
            });
        }

        crate::service_ops::refuse_source_symlink(&unit, &canonical).map_err(into_service)?;
        crate::service_ops::check_collisions(
            &unit,
            &canonical,
            &crate::service_ops::load_path_dirs(),
            Some(op),
        )
        .map_err(into_service)?;
        check_no_full_override(&unit).map_err(into_service)?;

        crate::service_exec::refuse_foreign_integration(&unit, &canonical).map_err(into_service)?;

        validate_service_binary(op, &target_files, roots, &unit)?;

        let derived = crate::services::SystemdUnit::derive(&target_service_for(
            op,
            target.ok_or(PlanError::UnsupportedKind {
                kind: "service changes without desired state",
            })?,
        )?)
        .map_err(|error| PlanError::Service {
            unit: unit.clone(),
            reason: error.to_string(),
        })?;
        if derived.unit != unit {
            return Err(PlanError::Service {
                unit,
                reason: "a service payload does not match its operation".into(),
            });
        }

        if matches!(op.kind, IntegrationChangeKind::Create)
            && previous_source
                .as_ref()
                .is_some_and(|bytes| bytes != &derived.bytes)
        {
            return Err(PlanError::Service {
                unit,
                reason: "the canonical unit path contains unrelated bytes".into(),
            });
        }

        let source_key = zup_core::ResourceKey::File {
            id: zup_core::FileResourceId::Native {
                id: zup_core::NativeFileId::ServiceSource {
                    id: parse_service_id(op)?,
                },
            },
        };
        let source = input
            .files
            .iter()
            .find(|file| file.key == source_key)
            .ok_or_else(|| PlanError::Service {
                unit: unit.clone(),
                reason: "service definition is missing its file effect".into(),
            })?;
        if source.destination.as_str() != canonical.to_string_lossy()
            || source.expected_sha256 != zup_core::hash_bytes(&derived.bytes)
            || source.expected_size != derived.bytes.len() as u64
            || source.executable
        {
            return Err(PlanError::Service {
                unit,
                reason: "service source differs from its native declaration".into(),
            });
        }
        let mut service = op.clone();
        service.lifecycle = service.lifecycle.resolved(&service.execution);
        input
            .native_services
            .push(zup_plan::NativeServiceOperation::Configure {
                service: Box::new(service),
                previous: match owned {
                    Some(OwnedResource::Service(service)) => Some(Box::new(service.clone())),
                    None => None,
                    _ => {
                        return Err(PlanError::Service {
                            unit,
                            reason: "service ownership type mismatch".into(),
                        });
                    }
                },
                activation: zup_plan::ServiceActivation::Declared,
            });
    }

    for removal in &service_removals {
        let OwnedResource::Service(
            owned_service @ zup_plan::OwnedService {
                timeouts,
                name,
                installed,
                privilege,
                ..
            },
        ) = &removal.owned
        else {
            continue;
        };
        let zup_core::ResourceKey::Service { .. } = &removal.key else {
            return Err(PlanError::UnsupportedKind {
                kind: "a non-service removal",
            });
        };
        let unit = name.clone();
        let runtime = manager.runtime(&unit).map_err(|error| PlanError::Service {
            unit: unit.clone(),
            reason: error.to_string(),
        })?;
        match runtime.state {
            zup_plan::ServiceRuntimeState::Running | zup_plan::ServiceRuntimeState::Stopped => {
                input
                    .service_controls
                    .push(zup_plan::ServiceControlOperation {
                        registration: zup_core::NativeServiceRegistration::SourceFile,
                        intent: zup_plan::ServiceControlIntent::Transition,
                        key: removal.key.clone(),
                        requires_files: owned_service.requires_files.clone(),
                        native_name: unit.clone(),
                        privilege: *privilege,
                        stage: zup_plan::ServiceControlStage::Quiesce,
                        before_configuration: installed.as_ref().clone(),
                        configuration: zup_plan::disabled_startup(installed).expect("registration"),
                        owned_configuration: installed.as_ref().clone(),
                        before: if runtime.state == zup_plan::ServiceRuntimeState::Running {
                            zup_plan::ServiceTerminalState::Running
                        } else {
                            zup_plan::ServiceTerminalState::Stopped
                        },
                        after: zup_plan::ServiceTerminalState::Stopped,
                        manager_generation: runtime.manager_generation,
                        before_execution_generation: runtime.execution_generation,
                        process_scope: zup_plan::ServiceScopeObservation::NativeManager,
                        timeouts: *timeouts,
                        dependencies: Vec::new(),
                    })
            }
            _ => {
                return Err(PlanError::Service {
                    unit,
                    reason: "cannot remove a service with ambiguous runtime state".into(),
                });
            }
        }
        let canonical =
            crate::machine::authorize_systemd_unit(&unit, systemd).map_err(|error| {
                PlanError::Service {
                    unit: unit.clone(),
                    reason: error.to_string(),
                }
            })?;
        crate::service_ops::refuse_source_symlink(&unit, &canonical).map_err(into_service)?;
        check_no_full_override(&unit).map_err(into_service)?;

        crate::service_exec::refuse_foreign_integration(&unit, &canonical).map_err(into_service)?;
        input.retired_keys.push(removal.key.clone());
        input
            .native_services
            .push(zup_plan::NativeServiceOperation::Remove {
                key: removal.key.clone(),
                service: Box::new(owned_service.clone()),
                activation: zup_plan::ServiceActivation::Declared,
            });
    }

    zup_plan::bind_service_dependencies(&mut input.service_controls, execution, ledger).map_err(
        |reason| PlanError::Service {
            unit: "dependency graph".into(),
            reason,
        },
    )?;
    crate::service_ops::bind_startup_inhibition(&mut input);
    input.bind_generations(target)?;
    Ok(input)
}

fn parse_service_id(op: &zup_plan::ServiceOperation) -> Result<zup_core::ServiceId, PlanError> {
    zup_core::ServiceId::new(&op.id).map_err(|error| PlanError::Service {
        unit: op.name.clone(),
        reason: format!("service id: {error}"),
    })
}

fn into_service(error: crate::error::ExecError) -> PlanError {
    match error {
        crate::error::ExecError::Refused { unit, reason }
        | crate::error::ExecError::Conflict { unit, reason }
        | crate::error::ExecError::Drift { unit, reason }
        | crate::error::ExecError::Ambiguous { unit, reason } => {
            PlanError::Service { unit, reason }
        }
        other => PlanError::Service {
            unit: String::new(),
            reason: other.to_string(),
        },
    }
}

fn check_no_full_override(unit: &str) -> Result<(), crate::error::ExecError> {
    crate::service_ops::check_no_full_override(unit, &crate::service_ops::admin_override_dir())
}

fn validate_service_binary(
    op: &zup_plan::ServiceOperation,
    target_files: &std::collections::BTreeMap<String, bool>,
    roots: &crate::locations::MachineRoots,
    unit: &str,
) -> Result<(), PlanError> {
    crate::service_ops::validate_executable(&op.command, target_files, roots, 0, false)
        .map(|_| ())
        .map_err(|error| match error {
            crate::error::ExecError::Refused { reason, .. }
            | crate::error::ExecError::Conflict { reason, .. }
            | crate::error::ExecError::Drift { reason, .. }
            | crate::error::ExecError::Ambiguous { reason, .. } => PlanError::Service {
                unit: unit.to_owned(),
                reason,
            },
            other => PlanError::Service {
                unit: unit.to_owned(),
                reason: other.to_string(),
            },
        })
}

fn target_service_for(
    op: &zup_plan::ServiceOperation,
    target: &zup_plan::BoundDeployment,
) -> Result<zup_plan::DesiredService<zup_core::TargetPath>, PlanError> {
    let planned = target
        .services
        .iter()
        .find(|service| service.key == op.key)
        .ok_or_else(|| PlanError::Service {
            unit: op.name.clone(),
            reason: "the service is not in the target plan".into(),
        })?;
    Ok(planned.clone())
}

#[cfg(all(test, feature = "test-support"))]
mod tests {
    use super::*;
    use crate::test_support::FakeSystemd;
    use zup_core::CommandSpec;
    use zup_core::TargetPath;
    use zup_core::{ResourceKey, ServiceId, ServiceStart};
    use zup_plan::{IntegrationChangeKind, ObservedServiceState, ServiceOperation};

    fn target() -> zup_core::Target {
        zup_core::Target::parse("linux-x64").expect("a target")
    }

    fn isolated() -> (
        tempfile::TempDir,
        crate::locations::MachineRoots,
        crate::machine::SystemdRoots,
    ) {
        let base = tempfile::tempdir().expect("an isolated base");
        let roots = crate::locations::MachineRoots::new(
            base.path().join("opt"),
            base.path().join("var/lib/zup"),
            base.path().join("var/opt"),
            base.path().join("environment.d"),
        );
        let systemd = crate::machine::SystemdRoots::new(base.path().join("units"));
        std::fs::create_dir_all(&systemd.unit_dir).expect("a unit tree");
        (base, roots, systemd)
    }

    fn service_plan(
        base: &tempfile::TempDir,
        start: ServiceStart,
        arguments: Vec<String>,
    ) -> zup_plan::BoundDeployment {
        let binary_host = base.path().join("opt").join("acme").join("tool");
        let app = zup_core::App {
            id: zup_core::AppId::new("com.example.tool").expect("an id"),
            name: zup_core::NonEmptyString::new("Tool").expect("a name"),
            version: semver::Version::parse("1.0.0").expect("a version"),
            publisher: None,
            main: None,
            description: None,
        };
        let executable = TargetPath::new(target(), binary_host.to_string_lossy()).expect("a path");
        let destination = executable.clone();
        let (size, sha256) = zup_core::hash_reader(b"elf".as_slice()).expect("bytes hash");
        let payload = zup_plan::DesiredFile {
            key: ResourceKey::File {
                id: zup_core::FileResourceId::Payload {
                    id: zup_core::PayloadId::File {
                        declaration: zup_core::ResourceId::new("fixture-0").unwrap(),
                    },
                },
            },
            source_relative: zup_core::RelativePath::new("tool").expect("a path"),
            placement: zup_core::PayloadPlacement::Fixed {
                path: destination,
                replacement: zup_core::FixedReplacement::Atomic,
            },
            size,
            sha256,
            privilege: Privilege::System,
            executable: true,
        };
        let service = zup_plan::DesiredService {
            state_directories: Vec::new(),
            failure: Default::default(),
            execution: zup_core::ServiceExecution::SystemdUser {
                user: "root".into(),
                group: None,
            },
            requires_files: Vec::new(),
            requires_services: Vec::new(),
            dependency_names: Vec::new(),
            lifecycle: Default::default(),
            readiness: Default::default(),
            key: ResourceKey::Service {
                id: ServiceId::new("tool").expect("an id"),
            },
            id: ServiceId::new("tool").expect("an id"),
            name: zup_core::NonEmptyString::new("tool.service").expect("a name"),
            display_name: Some(zup_core::NonEmptyString::new("Tool").unwrap()),
            command: CommandSpec::new(executable, arguments),
            start,
            privilege: Privilege::System,
        };
        zup_plan::DesiredState {
            generation_readers: Default::default(),
            data: Vec::new(),
            requirements: (),
            maintenance: None,
            installation: "00000000-0000-7000-8000-000000000001".parse().unwrap(),
            generation: None,
            app,
            target: target(),
            scope: zup_core::SelectedScope::Machine,
            install_directory: TargetPath::new(
                target(),
                base.path().join("opt").join("acme").to_string_lossy(),
            )
            .expect("a path"),
            selected_components: Vec::new(),
            prerequisites: Vec::new(),
            files: vec![payload],
            launchers: Vec::new(),
            path_entries: Vec::new(),
            services: vec![service],
            protocols: Vec::new(),
            file_associations: Vec::new(),

            preset: None,
        }
        .bind_fixed()
        .expect("fixed deployment requirements")
    }

    fn operation_at(
        executable: &TargetPath,
        arguments: Vec<String>,
        start: ServiceStart,
        kind: IntegrationChangeKind,
    ) -> ServiceOperation {
        ServiceOperation {
            state_directories: Vec::new(),
            registration: zup_core::NativeServiceRegistration::SourceFile,
            failure: Default::default(),
            execution: zup_core::ServiceExecution::SystemdUser {
                user: "root".into(),
                group: None,
            },
            requires_files: Vec::new(),
            requires_services: Vec::new(),
            dependency_names: Vec::new(),
            lifecycle: Default::default(),
            readiness: Default::default(),
            key: ResourceKey::Service {
                id: ServiceId::new("tool").expect("an id"),
            },
            kind,
            id: "tool".to_owned(),
            name: "tool.service".to_owned(),
            display_name: "Tool".to_owned(),
            command: CommandSpec::new(executable.clone(), arguments),
            start,
            privilege: Privilege::System,
            previous: ObservedServiceState::Absent,
            conflict: None,
        }
    }

    fn execution(services: Vec<ServiceOperation>) -> ExecutionPlan {
        ExecutionPlan {
            binding: None,
            selected_components: Vec::new(),
            install_directory: None,
            transition: zup_plan::DeploymentTransition::Deploy,
            removals: Vec::new(),
            files: Vec::new(),
            launchers: Vec::new(),
            path_entries: Vec::new(),
            services,
            protocols: Vec::new(),
            file_associations: Vec::new(),
        }
    }

    fn compile(
        plan: &zup_plan::BoundDeployment,
        execution: &ExecutionPlan,
        roots: &crate::locations::MachineRoots,
        systemd: &crate::machine::SystemdRoots,
        manager: &mut dyn crate::systemd::SystemdManager,
    ) -> Result<DeploymentEffects, PlanError> {
        let mut execution = execution.clone();
        let mut plan = plan.clone();
        for mut source in crate::integration::service_sources(plan.target, &plan.services)? {
            source.destination = zup_core::TargetPath::new(
                plan.target,
                systemd
                    .unit_dir
                    .join(source.destination.file_name().unwrap())
                    .to_string_lossy(),
            )
            .unwrap();
            execution.files.push(zup_plan::FileOperation {
                key: source.key.clone(),
                source_relative: zup_core::RelativePath::new(&source.source_relative).unwrap(),
                destination: source.destination.clone(),
                replacement: zup_core::FixedReplacement::Quiescent,
                precondition: zup_plan::FilePrecondition::Absent,
                expected_size: source.size,
                expected_sha256: source.sha256,
                privilege: Privilege::System,
                executable: source.executable,
                kind: zup_plan::FileOperationKind::Create,
                conflict: None,
            });
            plan.files.push(zup_plan::DesiredFile {
                key: source.key,
                source_relative: zup_core::RelativePath::new(source.source_relative).unwrap(),
                placement: zup_core::PayloadPlacement::Fixed {
                    path: source.destination,
                    replacement: zup_core::FixedReplacement::Quiescent,
                },
                size: source.size,
                sha256: source.sha256,
                privilege: Privilege::System,
                executable: source.executable,
            });
        }
        let plan = plan.bind_fixed()?;
        execution.binding = Some(zup_plan::DeploymentBinding::desired(&plan, None));
        compile_machine_execution_plan(
            &execution,
            Some(&plan),
            ServiceCompilation {
                ledger: None,
                roots,
                systemd,
                manager,
            },
        )
    }

    #[rstest::rstest]
    #[case::create(IntegrationChangeKind::Create, 1)]
    #[case::update(IntegrationChangeKind::UpdateOwned, 1)]
    #[case::restore(IntegrationChangeKind::RestoreOwned, 1)]
    #[case::unchanged_registration(IntegrationChangeKind::NoOp, 1)]
    fn executable_deltas_become_typed_operations(
        #[case] kind: IntegrationChangeKind,
        #[case] expected: usize,
    ) {
        let (base, roots, systemd) = isolated();
        let plan = service_plan(&base, ServiceStart::Automatic, vec!["--serve".into()]);
        let executable = plan.services[0].command.executable.clone();
        let mut manager = FakeSystemd::default();
        let operation = operation_at(
            &executable,
            vec!["--serve".into()],
            ServiceStart::Automatic,
            kind,
        );
        let execution = execution(vec![operation]);
        let input = compile(&plan, &execution, &roots, &systemd, &mut manager)
            .expect("an executable delta compiles");
        assert_eq!(input.native_services.len(), expected, "{kind:?}");
        assert!(input.backend_operations.is_empty());
    }

    #[rstest::rstest]
    #[case::automatic(ServiceStart::Automatic)]
    #[case::manual(ServiceStart::Manual)]
    #[case::disabled(ServiceStart::Disabled)]
    fn every_start_policy_transition_compiles(#[case] to: ServiceStart) {
        let (base, roots, systemd) = isolated();
        let plan = service_plan(&base, to, vec!["--serve".into()]);
        let executable = plan.services[0].command.executable.clone();
        let mut manager = FakeSystemd::default();
        let desired = operation_at(
            &executable,
            vec!["--serve".into()],
            to,
            IntegrationChangeKind::UpdateOwned,
        );
        let execution = execution(vec![desired]);
        let input = compile(&plan, &execution, &roots, &systemd, &mut manager)
            .expect("a transition compiles");
        assert_eq!(input.native_services.len(), 1, "{to:?}");
    }

    #[test]
    fn an_old_manager_refuses_before_mutation() {
        let (base, roots, systemd) = isolated();
        let plan = service_plan(&base, ServiceStart::Automatic, vec!["--serve".into()]);
        let executable = plan.services[0].command.executable.clone();
        let execution = execution(vec![operation_at(
            &executable,
            vec!["--serve".into()],
            ServiceStart::Automatic,
            IntegrationChangeKind::Create,
        )]);
        let mut manager = FakeSystemd::default();
        manager.version = "239".into();
        let error = compile(&plan, &execution, &roots, &systemd, &mut manager)
            .expect_err("systemd 239 cannot run Type=exec units");
        assert!(error.to_string().contains("239"), "{error}");
        assert!(
            std::fs::read_dir(&systemd.unit_dir)
                .expect("the unit tree reads")
                .next()
                .is_none(),
            "nothing was written before the baseline refused"
        );
    }

    #[test]
    fn only_explicit_owned_repairs_can_restore_drift() {
        let (base, roots, systemd) = isolated();
        let plan = service_plan(&base, ServiceStart::Automatic, vec!["--serve".into()]);
        let executable = plan.services[0].command.executable.clone();
        let drift = || {
            execution(vec![operation_at(
                &executable,
                vec!["--serve".into()],
                ServiceStart::Automatic,
                IntegrationChangeKind::Drift,
            )])
        };
        let mut manager = FakeSystemd::default();
        assert!(compile(&plan, &drift(), &roots, &systemd, &mut manager).is_err());
        let mut manager = FakeSystemd::default();
        assert!(
            compile(
                &plan,
                &execution(vec![operation_at(
                    &executable,
                    vec!["--serve".into()],
                    ServiceStart::Automatic,
                    IntegrationChangeKind::RepairOwned
                )]),
                &roots,
                &systemd,
                &mut manager
            )
            .is_ok(),
            "force restores owned content"
        );
        let conflict = execution(vec![operation_at(
            &executable,
            vec!["--serve".into()],
            ServiceStart::Automatic,
            IntegrationChangeKind::Conflict,
        )]);
        let mut manager = FakeSystemd::default();
        assert!(compile(&plan, &conflict, &roots, &systemd, &mut manager).is_err());
    }

    #[test]
    fn service_content_participates_in_the_plan_digest() {
        let (base, roots, systemd) = isolated();
        let left = service_plan(&base, ServiceStart::Automatic, vec!["--serve".into()]);
        let right = service_plan(&base, ServiceStart::Manual, vec!["--serve".into()]);
        let executable = left.services[0].command.executable.clone();
        let arguments = |start: ServiceStart| {
            execution(vec![operation_at(
                &executable,
                vec!["--serve".into()],
                start,
                IntegrationChangeKind::Create,
            )])
        };
        let mut manager = FakeSystemd::default();
        let mut left_input = compile(
            &left,
            &arguments(ServiceStart::Automatic),
            &roots,
            &systemd,
            &mut manager,
        )
        .expect("a plan compiles");
        let mut manager = FakeSystemd::default();
        let mut right_input = compile(
            &right,
            &arguments(ServiceStart::Manual),
            &roots,
            &systemd,
            &mut manager,
        )
        .expect("a plan compiles");
        left_input.install_directory = Some(left.install_directory.clone());
        right_input.install_directory = Some(right.install_directory.clone());
        let left_plan = zup_plan::compile_deployment(&left_input).expect("a plan");
        let right_plan = zup_plan::compile_deployment(&right_input).expect("a plan");
        assert_ne!(
            left_plan.fingerprint().to_hex(),
            right_plan.fingerprint().to_hex()
        );
    }

    #[test]
    fn an_unreachable_manager_fails_before_mutation() {
        let (base, roots, systemd) = isolated();
        let plan = service_plan(&base, ServiceStart::Automatic, vec!["--serve".into()]);
        let executable = plan.services[0].command.executable.clone();
        let execution = execution(vec![operation_at(
            &executable,
            vec!["--serve".into()],
            ServiceStart::Automatic,
            IntegrationChangeKind::Create,
        )]);
        let mut manager = FakeSystemd::default();
        manager
            .fail_always
            .insert("runtime".to_owned(), "the bus is gone".to_owned());
        assert!(compile(&plan, &execution, &roots, &systemd, &mut manager).is_err());
        assert!(
            std::fs::read_dir(&systemd.unit_dir)
                .expect("the unit tree reads")
                .next()
                .is_none(),
            "nothing was written before the preflight failed"
        );
    }

    #[test]
    fn user_scope_services_refuse() {
        let (base, roots, systemd) = isolated();
        let mut plan = service_plan(&base, ServiceStart::Automatic, vec!["--serve".into()]);
        plan.scope = zup_core::SelectedScope::User;
        let executable = plan.services[0].command.executable.clone();
        let execution = execution(vec![operation_at(
            &executable,
            vec!["--serve".into()],
            ServiceStart::Automatic,
            IntegrationChangeKind::Create,
        )]);
        let mut manager = FakeSystemd::default();
        assert!(compile(&plan, &execution, &roots, &systemd, &mut manager).is_err());
    }

    #[test]
    fn observed_services_snapshot_through_the_fake() {
        let (base, roots, systemd) = isolated();
        let _ = roots;
        let plan = service_plan(&base, ServiceStart::Automatic, vec!["--serve".into()]);
        let mut manager = FakeSystemd::default();
        let observed = snapshot_services(&plan, &mut manager, &systemd).expect("a snapshot");
        assert_eq!(observed.len(), 1);
        assert_eq!(
            observed[0].state,
            ObservedServiceState::Absent,
            "an uninstalled service observes absent"
        );
    }

    fn ledger_with_services(service: bool) -> Option<zup_plan::InstallationRecord> {
        use zup_core::{AppId, SelectedScope};
        let mut ledger = {
            let fixture_commit = zup_plan::InstallationCommit {
                transaction: zup_core::TransactionId::new_v7(),
                plan_hash: zup_core::hash_bytes(b"service input fixture"),
            };
            zup_plan::InstallationRecord {
                commit: fixture_commit,
                retired_deployments: Vec::new(),
                data: Default::default(),
                id: "00000000-0000-7000-8000-000000000001".parse().unwrap(),
                state: zup_plan::InstallationState::Installed,
                app_id: AppId::new("com.example.tool").unwrap(),
                target: target(),
                scope: SelectedScope::Machine,
                directory: TargetPath::new(target(), "/opt/tool").unwrap(),
                current: zup_plan::DeploymentSnapshot {
                    maintenance: None,
                    requirements: crate::requirements_fixture::requirements(
                        target(),
                        SelectedScope::Machine,
                        "com.example.tool",
                        "1.0.0",
                    ),
                    revision: zup_core::ActivationRevision::FIRST,
                    transaction: fixture_commit.transaction,
                    plan_hash: fixture_commit.plan_hash,
                    version: "1.0.0".parse().unwrap(),
                    selected_components: Vec::new(),
                    release: None,
                    preset: None,
                    materialization: None,
                    resources: Default::default(),
                },
                retained: Vec::new(),
            }
        };
        if service {
            ledger.current.resources.insert(
                ResourceKey::Service {
                    id: ServiceId::new("tool").expect("an id"),
                },
                zup_plan::OwnedResource::Service(zup_plan::OwnedService {
                    registration: zup_core::NativeServiceRegistration::SourceFile,
                    timeouts: zup_core::ServiceTimeouts::for_execution(
                        &zup_core::ServiceExecution::SystemdDynamicUser,
                    ),
                    requires_files: Vec::new(),
                    requires_services: Vec::new(),
                    name: "tool.service".to_owned(),
                    privilege: Privilege::System,
                    installed: Box::new(zup_plan::ServiceState::Absent),
                }),
            );
        }
        Some(ledger)
    }

    #[test]
    fn the_manager_decision_covers_desired_owned_and_removals() {
        let (base, _, _) = isolated();
        let with = service_plan(&base, ServiceStart::Automatic, vec!["--serve".into()]);
        let mut without = with.clone();
        without.services.clear();

        assert!(
            !requires_service_manager(Some(&without), None),
            "no desired services and no ledger needs no manager"
        );
        assert!(
            !requires_service_manager(Some(&without), ledger_with_services(false).as_ref()),
            "a file-only ledger needs no manager"
        );
        assert!(
            requires_service_manager(Some(&with), None),
            "desired services need the manager"
        );
        assert!(
            requires_service_manager(Some(&without), ledger_with_services(true).as_ref()),
            "previously owned services need the manager even when the desired state has none"
        );
        assert!(
            requires_service_manager(None, ledger_with_services(true).as_ref()),
            "uninstall observes owned services without a desired graph"
        );
        assert!(
            requires_service_manager(Some(&with), ledger_with_services(true).as_ref()),
            "desired plus owned services need the manager"
        );
    }

    #[test]
    fn removing_the_final_service_compiles_to_a_typed_removal() {
        let (base, roots, systemd) = isolated();
        let with = service_plan(&base, ServiceStart::Automatic, vec!["--serve".into()]);
        let mut desired = with.clone();
        desired.services.clear();
        assert!(
            requires_service_manager(Some(&desired), ledger_with_services(true).as_ref()),
            "the one-to-none transition needs the manager"
        );

        let mut execution = execution(vec![]);
        execution.removals.push(zup_plan::RemovalOperation {
            key: ResourceKey::Service {
                id: ServiceId::new("tool").expect("an id"),
            },
            kind: zup_plan::RemovalKind::RemoveOwned,
            scope: zup_core::SelectedScope::Machine,
            privilege: Privilege::System,
            owned: zup_plan::OwnedResource::Service(zup_plan::OwnedService {
                registration: zup_core::NativeServiceRegistration::SourceFile,
                timeouts: zup_core::ServiceTimeouts::for_execution(
                    &zup_core::ServiceExecution::SystemdDynamicUser,
                ),
                requires_files: Vec::new(),
                requires_services: Vec::new(),
                name: "tool.service".to_owned(),
                privilege: Privilege::System,
                installed: Box::new(
                    operation_at(
                        &with.services[0].command.executable,
                        vec!["--serve".into()],
                        ServiceStart::Automatic,
                        IntegrationChangeKind::Create,
                    )
                    .desired_configuration(),
                ),
            }),
        });
        let mut manager = FakeSystemd::default();
        let input = compile(&desired, &execution, &roots, &systemd, &mut manager)
            .expect("a service removal compiles");
        assert_eq!(input.native_services.len(), 1);
        assert!(matches!(
            input.native_services.as_slice(),
            [zup_plan::NativeServiceOperation::Remove { .. }]
        ));
    }
}

#[cfg(test)]
mod override_tests {
    #[test]
    fn an_override_shadows_and_absence_passes() {
        let dir = tempfile::tempdir().expect("an isolated admin layer");
        let unit = "zup-tool-override.service";
        assert!(crate::service_ops::check_no_full_override(unit, dir.path()).is_ok());
        std::fs::write(dir.path().join(unit), b"[Unit]\n").expect("an override");
        assert!(crate::service_ops::check_no_full_override(unit, dir.path()).is_err());
    }
}
