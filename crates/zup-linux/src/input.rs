use zup_core::Privilege;
use zup_exec::{ExecutionPlan, FileOperationKind, OwnedResource, ServiceOperationKind};
use zup_platform::TargetPlan;
use zup_transaction::{
    BackendOperation, FileDelta, FilePrecondition, FileRemoval, FileRemovalKind, FileWork,
    TransactionInput,
};

use crate::error::{ExecError, PlanError};
use crate::integration::GENERATED_PREFIX;
use crate::refresh::RefreshRequest;

pub fn snapshot_services(
    target: &TargetPlan,
    manager: &mut dyn crate::systemd::SystemdManager,
    systemd: &crate::machine::SystemdRoots,
) -> Result<Vec<zup_exec::ObservedService>, ExecError> {
    crate::service_ops::snapshot_services(target, manager, systemd)
}

pub fn requires_service_manager(
    target: &TargetPlan,
    ledger: Option<&zup_exec::InstallLedger>,
) -> bool {
    !target.services.is_empty() || ledger_has_services(ledger)
}

pub fn ledger_has_services(ledger: Option<&zup_exec::InstallLedger>) -> bool {
    ledger.is_some_and(|ledger| {
        ledger
            .resources
            .values()
            .any(|owned| matches!(owned, OwnedResource::Service { .. }))
    })
}

pub fn compile_execution_plan(
    execution: &ExecutionPlan,
    target: &TargetPlan,
) -> Result<TransactionInput, PlanError> {
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

fn compile_files_only(
    execution: &ExecutionPlan,
    target: &TargetPlan,
) -> Result<TransactionInput, PlanError> {
    let mut input = TransactionInput::new(target.target.clone());
    input.selected_components = execution.selected_components.clone();
    input.install_directory = Some(target.install_directory.clone());
    input.uninstall = execution.uninstall;

    input.preset = None;

    let mut mime_directory: Option<String> = None;
    let mut desktop_directory: Option<String> = None;
    let mut mime_after: Vec<zup_core::ResourceKey> = Vec::new();
    let mut desktop_after: Vec<zup_core::ResourceKey> = Vec::new();

    for file in &execution.files {
        let source = file.source_relative.as_str();
        if is_generated_source(source) && !matches!(file.kind, FileOperationKind::NoOp) {
            let (mime, desktop) = refresh_for_generated(source);
            if mime {
                mime_directory = mime_directory.or_else(|| {
                    file.destination
                        .parent()
                        .and_then(|packages| packages.parent())
                        .map(|directory| directory.to_string())
                });
            }
            if desktop {
                desktop_directory = desktop_directory.or_else(|| {
                    file.destination
                        .parent()
                        .map(|directory| directory.to_string())
                });
            }
        }
        input.files.push(FileWork {
            key: file.key.clone(),
            source_relative: file.source_relative.clone(),
            destination: file.destination.clone(),
            precondition: match file.precondition {
                zup_exec::FilePrecondition::Absent => FilePrecondition::Absent,
                zup_exec::FilePrecondition::Exact { size, sha256 } => {
                    FilePrecondition::Exact { size, sha256 }
                }
            },
            expected_sha256: file.expected_sha256,
            expected_size: file.expected_size,
            privilege: file.privilege,
            executable: file.executable,
            delta: match file.kind {
                FileOperationKind::Create => FileDelta::Create,
                FileOperationKind::Replace => FileDelta::Replace,
                FileOperationKind::RestoreOwned => FileDelta::RestoreOwned,
                FileOperationKind::RepairOwned => FileDelta::RepairOwned,
                FileOperationKind::NoOp => FileDelta::NoOp,
                FileOperationKind::Conflict | FileOperationKind::Drift => {
                    return Err(PlanError::Undecided {
                        destination: file.destination.to_string(),
                        kind: file.kind,
                    });
                }
            },
        });
    }

    for removal in &execution.removals {
        if let OwnedResource::File {
            destination,
            sha256,
            size,
            created_directories,
            ..
        } = &removal.owned
        {
            let source = removal
                .owned
                .source_relative()
                .map(|source| source.as_str())
                .unwrap_or_default();
            if is_generated_source(source) {
                let (mime, desktop) = refresh_for_generated(source);
                if mime {
                    mime_directory = mime_directory.or_else(|| {
                        destination
                            .parent()
                            .and_then(|packages| packages.parent())
                            .map(|directory| directory.to_string())
                    });
                    mime_after.push(removal.key.clone());
                }
                if desktop {
                    desktop_directory = desktop_directory
                        .or_else(|| destination.parent().map(|directory| directory.to_string()));
                    desktop_after.push(removal.key.clone());
                }
            }
            input.removals.push(FileRemoval {
                key: removal.key.clone(),
                kind: match removal.kind {
                    zup_exec::RemovalKind::RemoveOwned => FileRemovalKind::RemoveOwned,
                    zup_exec::RemovalKind::Drift => FileRemovalKind::Drift,
                },
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

    if let Some(directory) = mime_directory {
        let request = RefreshRequest::mime(&directory);
        input.backend_operations.push(
            BackendOperation::apply(
                request.key(),
                request.backend_id(),
                Privilege::User,
                request.encode(),
            )
            .with_dependencies(mime_after),
        );
    }
    if let Some(directory) = desktop_directory {
        let request = RefreshRequest::desktop(&directory);
        input.backend_operations.push(
            BackendOperation::apply(
                request.key(),
                request.backend_id(),
                Privilege::User,
                request.encode(),
            )
            .with_dependencies(desktop_after),
        );
    }

    Ok(input)
}

pub struct ServiceCompilation<'a> {
    pub roots: &'a crate::machine::MachineRoots,

    pub systemd: &'a crate::machine::SystemdRoots,

    pub manager: &'a mut dyn crate::systemd::SystemdManager,

    pub force_services: bool,
}

pub fn compile_machine_execution_plan(
    execution: &ExecutionPlan,
    target: &TargetPlan,
    compilation: ServiceCompilation<'_>,
) -> Result<TransactionInput, PlanError> {
    let ServiceCompilation {
        roots,
        systemd,
        manager,
        force_services,
    } = compilation;
    if target.scope != zup_core::SelectedScope::Machine {
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
            OwnedResource::Service { .. } => service_removals.push(removal),
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
    for file in &target.files {
        target_files.insert(file.destination.to_string(), file.executable);
    }

    if !execution.services.is_empty() || !service_removals.is_empty() {
        manager
            .unit_file_state("zup-preflight.service")
            .map(|_| ())
            .or_else(|error| match &error {
                crate::error::IpcError::SystemdRefused { .. } => Ok(()),
                _ => Err(PlanError::Service {
                    unit: String::new(),
                    reason: format!("systemd is unavailable: {error}"),
                }),
            })?;
        crate::service_ops::require_exec_baseline(manager).map_err(into_service)?;
    }

    for op in &execution.services {
        let unit = crate::services::unit_name(&parse_service_id(op)?).map_err(|error| {
            PlanError::Service {
                unit: op.name.clone(),
                reason: error.to_string(),
            }
        })?;
        let canonical =
            crate::machine::authorize_systemd_unit(&unit, systemd).map_err(|error| {
                PlanError::Service {
                    unit: unit.clone(),
                    reason: error.to_string(),
                }
            })?;

        crate::service_ops::refuse_source_symlink(&unit, &canonical).map_err(into_service)?;
        crate::service_ops::check_collisions(
            &unit,
            &canonical,
            &crate::service_ops::load_path_dirs(),
        )
        .map_err(into_service)?;
        check_no_full_override(&unit).map_err(into_service)?;

        crate::service_exec::refuse_foreign_integration(&unit, &canonical).map_err(into_service)?;

        validate_service_binary(op, &target_files, roots, &unit)?;

        let derived = crate::services::DesiredService::derive(&target_service_for(op, target)?)
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

        if matches!(op.kind, ServiceOperationKind::Create) {
            let existing =
                crate::service_ops::read_canonical_source(&canonical).map_err(into_service)?;
            if existing.is_some_and(|bytes| bytes != derived.bytes) {
                return Err(PlanError::Service {
                    unit,
                    reason: "the canonical unit path contains unrelated bytes".into(),
                });
            }
        }

        let previous_policy =
            manager
                .unit_file_state(&unit)
                .map_err(|error| PlanError::Service {
                    unit: unit.clone(),
                    reason: format!("systemd is unavailable: {error}"),
                })?;
        let operation = match op.kind {
            ServiceOperationKind::Create | ServiceOperationKind::UpdateOwned => {
                crate::service_exec::apply_operation(
                    op,
                    &unit,
                    &derived.bytes,
                    true,
                    &previous_policy,
                    false,
                )
                .map_err(into_service)?
            }
            ServiceOperationKind::RestoreOwned => {
                let present = crate::service_ops::read_canonical_source(&canonical)
                    .map_err(into_service)?
                    .is_some();
                if present && !force_services {
                    return Err(PlanError::Service {
                        unit,
                        reason: "the planner could not decide, so there is no transaction to run"
                            .into(),
                    });
                }
                crate::service_exec::apply_operation(
                    op,
                    &unit,
                    &derived.bytes,
                    true,
                    &previous_policy,
                    force_services,
                )
                .map_err(into_service)?
            }
            ServiceOperationKind::Drift if force_services => crate::service_exec::apply_operation(
                op,
                &unit,
                &derived.bytes,
                true,
                &previous_policy,
                true,
            )
            .map_err(into_service)?,
            ServiceOperationKind::NoOp => continue,
            ServiceOperationKind::Conflict | ServiceOperationKind::Drift => {
                return Err(PlanError::Service {
                    unit,
                    reason: "the planner could not decide, so there is no transaction to run"
                        .into(),
                });
            }
        };
        input.backend_operations.push(operation);
    }

    for removal in &service_removals {
        let OwnedResource::Service { name, .. } = &removal.owned else {
            continue;
        };
        let zup_core::ResourceKey::Service { id } = &removal.key else {
            return Err(PlanError::UnsupportedKind {
                kind: "a non-service removal",
            });
        };
        let unit = crate::services::unit_name(id).map_err(|error| PlanError::Service {
            unit: name.clone(),
            reason: error.to_string(),
        })?;
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
        let operation = crate::service_exec::remove_operation(
            &removal.key,
            &unit,
            &removal.owned,
            removal.privilege,
        )
        .map_err(into_service)?;
        input.retired_keys.push(operation.key.clone());
        input.backend_operations.push(operation);
    }

    Ok(input)
}

fn parse_service_id(op: &zup_exec::ServiceOperation) -> Result<zup_core::ServiceId, PlanError> {
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

fn validate_service_binary(
    op: &zup_exec::ServiceOperation,
    target_files: &std::collections::BTreeMap<String, bool>,
    roots: &crate::machine::MachineRoots,
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
    op: &zup_exec::ServiceOperation,
    target: &TargetPlan,
) -> Result<zup_platform::TargetService, PlanError> {
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

fn is_generated_source(source: &str) -> bool {
    source == GENERATED_PREFIX || source.starts_with(&format!("{GENERATED_PREFIX}/"))
}

fn refresh_for_generated(source: &str) -> (bool, bool) {
    let mime = source.starts_with(&format!("{GENERATED_PREFIX}/mime/"));
    let desktop = source.starts_with(&format!("{GENERATED_PREFIX}/applications/"));
    (mime, desktop)
}

#[cfg(all(test, feature = "test-support"))]
mod tests {
    use super::*;
    use crate::test_support::FakeSystemd;
    use zup_core::{ResourceKey, ServiceId, ServiceStart};
    use zup_exec::{ObservedServiceState, ServiceOperation, ServiceOperationKind};
    use zup_platform::{CommandSpec, TargetPath};

    fn target() -> zup_core::TargetTriple {
        zup_core::TargetTriple::parse("x86_64-unknown-linux-gnu").expect("a target")
    }

    fn isolated() -> (
        tempfile::TempDir,
        crate::machine::MachineRoots,
        crate::machine::SystemdRoots,
    ) {
        let base = tempfile::tempdir().expect("an isolated base");
        let roots = crate::machine::MachineRoots::new(
            base.path().join("opt"),
            base.path().join("var/lib/zup"),
            base.path().join("var/opt"),
        );
        let systemd = crate::machine::SystemdRoots::new(base.path().join("units"));
        std::fs::create_dir_all(&systemd.unit_dir).expect("a unit tree");
        (base, roots, systemd)
    }

    fn service_plan(
        base: &tempfile::TempDir,
        start: ServiceStart,
        arguments: Vec<String>,
    ) -> TargetPlan {
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
        let payload = zup_platform::TargetFile {
            key: ResourceKey::File {
                destination: destination.to_string(),
            },
            source_relative: zup_core::RelativePath::new("tool").expect("a path"),
            destination,
            size,
            sha256,
            privilege: Privilege::System,
            executable: true,
        };
        let service = zup_platform::TargetService {
            key: ResourceKey::Service {
                id: ServiceId::new("tool").expect("an id"),
            },
            id: ServiceId::new("tool").expect("an id"),
            name: zup_core::NonEmptyString::new("Tool").expect("a name"),
            display_name: None,
            command: CommandSpec::new(executable, arguments),
            start,
            privilege: Privilege::System,
        };
        TargetPlan {
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
            summary: zup_platform::TargetPlanSummary {
                file_count: 1,
                install_bytes: 3,
                resource_count: 1,
                requires_authorization: true,
                selected_component_count: 0,
                prerequisite_count: 0,
                download_bytes: 0,
            },
            preset: None,
        }
    }

    fn operation_at(
        executable: &TargetPath,
        arguments: Vec<String>,
        start: ServiceStart,
        kind: ServiceOperationKind,
    ) -> ServiceOperation {
        ServiceOperation {
            key: ResourceKey::Service {
                id: ServiceId::new("tool").expect("an id"),
            },
            kind,
            id: "tool".to_owned(),
            name: "Tool".to_owned(),
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
            selected_components: Vec::new(),
            install_directory: None,
            uninstall: false,
            removals: Vec::new(),
            files: Vec::new(),
            launchers: Vec::new(),
            path_entries: Vec::new(),
            services,
            protocols: Vec::new(),
            file_associations: Vec::new(),
            summary: zup_exec::ExecutionSummary::default(),
        }
    }

    fn compile(
        plan: &TargetPlan,
        execution: &ExecutionPlan,
        roots: &crate::machine::MachineRoots,
        systemd: &crate::machine::SystemdRoots,
        manager: &mut dyn crate::systemd::SystemdManager,
        force: bool,
    ) -> Result<TransactionInput, PlanError> {
        compile_machine_execution_plan(
            execution,
            plan,
            ServiceCompilation {
                roots,
                systemd,
                manager,
                force_services: force,
            },
        )
    }

    #[rstest::rstest]
    #[case::create(ServiceOperationKind::Create, 1)]
    #[case::update(ServiceOperationKind::UpdateOwned, 1)]
    #[case::restore(ServiceOperationKind::RestoreOwned, 1)]
    #[case::noop(ServiceOperationKind::NoOp, 0)]
    fn executable_deltas_become_typed_operations(
        #[case] kind: ServiceOperationKind,
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
        let input = compile(&plan, &execution, &roots, &systemd, &mut manager, false)
            .expect("an executable delta compiles");
        assert_eq!(input.backend_operations.len(), expected, "{kind:?}");
        for operation in &input.backend_operations {
            assert!(
                operation
                    .id
                    .as_str()
                    .starts_with(crate::service_ops::SERVICE_BACKEND_PREFIX),
                "typed identity, never a generic call"
            );
        }
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
            ServiceOperationKind::UpdateOwned,
        );
        let execution = execution(vec![desired]);
        let input = compile(&plan, &execution, &roots, &systemd, &mut manager, false)
            .expect("a transition compiles");
        assert_eq!(input.backend_operations.len(), 1, "{to:?}");
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
            ServiceOperationKind::Create,
        )]);
        let mut manager = FakeSystemd::default();
        manager.version = "239".into();
        let error = compile(&plan, &execution, &roots, &systemd, &mut manager, false)
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
    fn drift_needs_force_and_conflict_always_refuses() {
        let (base, roots, systemd) = isolated();
        let plan = service_plan(&base, ServiceStart::Automatic, vec!["--serve".into()]);
        let executable = plan.services[0].command.executable.clone();
        let drift = || {
            execution(vec![operation_at(
                &executable,
                vec!["--serve".into()],
                ServiceStart::Automatic,
                ServiceOperationKind::Drift,
            )])
        };
        let mut manager = FakeSystemd::default();
        assert!(compile(&plan, &drift(), &roots, &systemd, &mut manager, false).is_err());
        let mut manager = FakeSystemd::default();
        assert!(
            compile(&plan, &drift(), &roots, &systemd, &mut manager, true).is_ok(),
            "force restores owned content"
        );
        let conflict = execution(vec![operation_at(
            &executable,
            vec!["--serve".into()],
            ServiceStart::Automatic,
            ServiceOperationKind::Conflict,
        )]);
        let mut manager = FakeSystemd::default();
        assert!(compile(&plan, &conflict, &roots, &systemd, &mut manager, true).is_err());
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
                ServiceOperationKind::Create,
            )])
        };
        let mut manager = FakeSystemd::default();
        let left_input = compile(
            &left,
            &arguments(ServiceStart::Automatic),
            &roots,
            &systemd,
            &mut manager,
            false,
        )
        .expect("a plan compiles");
        let mut manager = FakeSystemd::default();
        let right_input = compile(
            &right,
            &arguments(ServiceStart::Manual),
            &roots,
            &systemd,
            &mut manager,
            false,
        )
        .expect("a plan compiles");
        let left_plan = zup_transaction::compile_transaction(&left_input).expect("a plan");
        let right_plan = zup_transaction::compile_transaction(&right_input).expect("a plan");
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
            ServiceOperationKind::Create,
        )]);
        let mut manager = FakeSystemd::default();
        manager
            .fail_always
            .insert("unit_file_state".to_owned(), "the bus is gone".to_owned());
        assert!(compile(&plan, &execution, &roots, &systemd, &mut manager, false).is_err());
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
            ServiceOperationKind::Create,
        )]);
        let mut manager = FakeSystemd::default();
        assert!(compile(&plan, &execution, &roots, &systemd, &mut manager, false).is_err());
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

    fn ledger_with_services(service: bool) -> Option<zup_exec::InstallLedger> {
        use zup_core::{AppId, SelectedScope};
        let mut ledger = zup_exec::InstallLedger::new(
            AppId::new("com.example.tool").expect("an id"),
            target(),
            SelectedScope::Machine,
        );
        if service {
            ledger.resources.insert(
                ResourceKey::Service {
                    id: ServiceId::new("tool").expect("an id"),
                },
                zup_exec::OwnedResource::Service {
                    name: "Tool".to_owned(),
                    privilege: Privilege::System,
                    previous: zup_exec::ServiceState::Absent,
                    installed: zup_exec::ServiceState::Absent,
                },
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
            !requires_service_manager(&without, None),
            "no desired services and no ledger needs no manager"
        );
        assert!(
            !requires_service_manager(&without, ledger_with_services(false).as_ref()),
            "a file-only ledger needs no manager"
        );
        assert!(
            requires_service_manager(&with, None),
            "desired services need the manager"
        );
        assert!(
            requires_service_manager(&without, ledger_with_services(true).as_ref()),
            "previously owned services need the manager even when the desired state has none"
        );
        assert!(
            requires_service_manager(&with, ledger_with_services(true).as_ref()),
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
            requires_service_manager(&desired, ledger_with_services(true).as_ref()),
            "the one-to-none transition needs the manager"
        );

        let mut execution = execution(vec![]);
        execution.removals.push(zup_exec::RemovalOperation {
            key: ResourceKey::Service {
                id: ServiceId::new("tool").expect("an id"),
            },
            kind: zup_exec::RemovalKind::RemoveOwned,
            scope: zup_core::SelectedScope::Machine,
            privilege: Privilege::System,
            owned: zup_exec::OwnedResource::Service {
                name: "Tool".to_owned(),
                privilege: Privilege::System,
                previous: zup_exec::ServiceState::Absent,
                installed: zup_exec::ServiceState::Absent,
            },
        });
        let mut manager = FakeSystemd::default();
        let input = compile(&desired, &execution, &roots, &systemd, &mut manager, false)
            .expect("a service removal compiles");
        assert_eq!(input.backend_operations.len(), 1);
        assert!(
            input.backend_operations[0]
                .id
                .as_str()
                .starts_with(crate::service_ops::SERVICE_BACKEND_PREFIX),
            "a typed service removal, never a generic call"
        );
    }
}
