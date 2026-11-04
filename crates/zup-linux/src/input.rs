use zup_core::Privilege;
use zup_plan::{
    DesiredState, ExecutionPlan, FileOperationKind, IntegrationChangeKind, OwnedResource,
};

use zup_transaction::{BackendOperation, FileRemoval, TransactionInput};

use crate::error::{ExecError, PlanError};
use crate::integration::GENERATED_PREFIX;
use crate::refresh::RefreshRequest;

pub fn snapshot_services(
    target: &DesiredState<zup_plan::TargetPath, zup_plan::TargetPath>,
    manager: &mut dyn crate::systemd::SystemdManager,
    systemd: &crate::machine::SystemdRoots,
) -> Result<Vec<zup_plan::ObservedService>, ExecError> {
    crate::service_ops::snapshot_services(target, manager, systemd)
}

pub fn requires_service_manager(
    target: Option<&DesiredState<zup_plan::TargetPath, zup_plan::TargetPath>>,
    ledger: Option<&zup_plan::InstallLedger>,
) -> bool {
    target.is_some_and(|target| !target.services.is_empty()) || ledger_has_services(ledger)
}

pub fn ledger_has_services(ledger: Option<&zup_plan::InstallLedger>) -> bool {
    ledger.is_some_and(|ledger| {
        ledger
            .resources
            .values()
            .any(|owned| matches!(owned, OwnedResource::Service { .. }))
    })
}

pub fn compile_execution_plan(
    execution: &ExecutionPlan,
    target: Option<&DesiredState<zup_plan::TargetPath, zup_plan::TargetPath>>,
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
    target: Option<&DesiredState<zup_plan::TargetPath, zup_plan::TargetPath>>,
) -> Result<TransactionInput, PlanError> {
    let binding = execution.validate_desired(target)?;
    let mut input = TransactionInput::new(binding.target);
    input.observed_transaction = binding.observed_transaction;
    input.selected_components = execution.selected_components.clone();
    input.install_directory = execution.install_directory.clone();
    input.uninstall = execution.uninstall;
    input.preset = if execution.uninstall {
        None
    } else {
        target.and_then(|target| target.preset.clone())
    };

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
        input.files.push(file.clone());
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
    pub roots: &'a crate::locations::MachineRoots,

    pub systemd: &'a crate::machine::SystemdRoots,

    pub manager: &'a mut dyn crate::systemd::SystemdManager,
}

pub fn compile_machine_execution_plan(
    execution: &ExecutionPlan,
    target: Option<&DesiredState<zup_plan::TargetPath, zup_plan::TargetPath>>,
    compilation: ServiceCompilation<'_>,
) -> Result<TransactionInput, PlanError> {
    let ServiceCompilation {
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
    for file in target.into_iter().flat_map(|target| &target.files) {
        target_files.insert(file.destination.to_string(), file.executable);
    }

    if !execution.services.is_empty() || !service_removals.is_empty() {
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

        let previous_source =
            crate::service_ops::read_canonical_source(&canonical).map_err(into_service)?;
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

        let previous_policy =
            manager
                .unit_file_state(&unit)
                .map_err(|error| PlanError::Service {
                    unit: unit.clone(),
                    reason: format!("systemd is unavailable: {error}"),
                })?;
        let operation = match op.kind {
            IntegrationChangeKind::Create | IntegrationChangeKind::UpdateOwned => {
                crate::service_exec::apply_operation(
                    op,
                    &unit,
                    &derived.bytes,
                    true,
                    &previous_policy,
                    previous_source.as_deref(),
                )
                .map_err(into_service)?
            }
            IntegrationChangeKind::RestoreOwned | IntegrationChangeKind::RepairOwned => {
                if op.kind == IntegrationChangeKind::RestoreOwned && previous_source.is_some() {
                    return Err(PlanError::Service {
                        unit,
                        reason: "a missing service was recreated after observation".into(),
                    });
                }
                crate::service_exec::apply_operation(
                    op,
                    &unit,
                    &derived.bytes,
                    true,
                    &previous_policy,
                    previous_source.as_deref(),
                )
                .map_err(into_service)?
            }
            IntegrationChangeKind::NoOp => continue,
            IntegrationChangeKind::Conflict | IntegrationChangeKind::Drift => {
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
    target: &DesiredState<zup_plan::TargetPath, zup_plan::TargetPath>,
) -> Result<zup_plan::DesiredService<zup_plan::TargetPath>, PlanError> {
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
    use zup_plan::{CommandSpec, TargetPath};
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
    ) -> DesiredState<zup_plan::TargetPath, zup_plan::TargetPath> {
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
                destination: destination.to_string(),
            },
            source_relative: zup_core::RelativePath::new("tool").expect("a path"),
            destination,
            size,
            sha256,
            privilege: Privilege::System,
            executable: true,
        };
        let service = zup_plan::DesiredService {
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
        DesiredState {
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
    }

    fn operation_at(
        executable: &TargetPath,
        arguments: Vec<String>,
        start: ServiceStart,
        kind: IntegrationChangeKind,
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
            binding: None,
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
        }
    }

    fn compile(
        plan: &DesiredState<zup_plan::TargetPath, zup_plan::TargetPath>,
        execution: &ExecutionPlan,
        roots: &crate::locations::MachineRoots,
        systemd: &crate::machine::SystemdRoots,
        manager: &mut dyn crate::systemd::SystemdManager,
    ) -> Result<TransactionInput, PlanError> {
        let mut execution = execution.clone();
        execution.binding = Some(zup_plan::DeploymentBinding::desired(plan, None));
        compile_machine_execution_plan(
            &execution,
            Some(plan),
            ServiceCompilation {
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
    #[case::noop(IntegrationChangeKind::NoOp, 0)]
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
            IntegrationChangeKind::UpdateOwned,
        );
        let execution = execution(vec![desired]);
        let input = compile(&plan, &execution, &roots, &systemd, &mut manager)
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
        let left_input = compile(
            &left,
            &arguments(ServiceStart::Automatic),
            &roots,
            &systemd,
            &mut manager,
        )
        .expect("a plan compiles");
        let mut manager = FakeSystemd::default();
        let right_input = compile(
            &right,
            &arguments(ServiceStart::Manual),
            &roots,
            &systemd,
            &mut manager,
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
            IntegrationChangeKind::Create,
        )]);
        let mut manager = FakeSystemd::default();
        manager
            .fail_always
            .insert("unit_file_state".to_owned(), "the bus is gone".to_owned());
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

    fn ledger_with_services(service: bool) -> Option<zup_plan::InstallLedger> {
        use zup_core::{AppId, SelectedScope};
        let mut ledger = zup_plan::InstallLedger::new(
            AppId::new("com.example.tool").expect("an id"),
            target(),
            SelectedScope::Machine,
            zup_core::TransactionId::new_v7(),
        );
        if service {
            ledger.resources.insert(
                ResourceKey::Service {
                    id: ServiceId::new("tool").expect("an id"),
                },
                zup_plan::OwnedResource::Service {
                    name: "Tool".to_owned(),
                    privilege: Privilege::System,
                    previous: zup_plan::ServiceState::Absent,
                    installed: zup_plan::ServiceState::Absent,
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
            owned: zup_plan::OwnedResource::Service {
                name: "Tool".to_owned(),
                privilege: Privilege::System,
                previous: zup_plan::ServiceState::Absent,
                installed: zup_plan::ServiceState::Absent,
            },
        });
        let mut manager = FakeSystemd::default();
        let input = compile(&desired, &execution, &roots, &systemd, &mut manager)
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
