//! Turning a portable execution plan into a transaction input.
//!
//! The mapping is mechanical - one file decision becomes one unit of
//! transaction work - and it is also a boundary. Everything this backend
//! cannot execute is refused here, before compilation, with the kind named:
//! a conflict the planner could not resolve, a drifted resource the ledger no
//! longer explains, or any non-file operation that reached this far despite
//! the capability gate. A transaction input with backend operations in it
//! would compile cleanly and then fail at apply time, which is exactly the
//! install-files-then-fail-on-services outcome the capability boundary exists
//! to prevent.
//!
//! Generated integration files (desktop entries, MIME packages) lower as
//! ordinary files. When such a file is created, replaced, repaired, or
//! removed, the derived database it feeds must be regenerated: a typed
//! refresh operation joins the transaction, ordered after the files by the
//! transaction graph.

use zup_core::Privilege;
use zup_exec::{ExecutionPlan, FileOperationKind, OwnedResource, ServiceOperationKind};
use zup_platform::TargetPlan;
use zup_transaction::{
    BackendOperation, FileDelta, FilePrecondition, FileRemoval, FileRemovalKind, FileWork,
    TransactionInput,
};

use crate::integration::GENERATED_PREFIX;
use crate::refresh::RefreshRequest;

/// Why an execution plan cannot become a Linux transaction input.
#[derive(Debug, thiserror::Error)]
pub enum LinuxInputError {
    #[error(
        "file `{destination}` is {kind:?}: the planner could not decide, so there is no transaction to run"
    )]
    Undecided {
        destination: String,
        kind: FileOperationKind,
    },

    #[error("cannot transact {kind}: no Linux mechanism executes it in this phase")]
    Unsupported { kind: &'static str },

    #[error("cannot transact service `{unit}`: {reason}")]
    Service { unit: String, reason: String },
}

/// Compile an execution plan into a transaction input for Linux.
///
/// Files and file removals lower directly; every other operation kind is
/// refused rather than dropped. Dropping one would install the files and
/// silently skip the rest, which reads as success and is not.
///
/// Machine-scope services use [`compile_machine_execution_plan`] instead:
/// this entry point keeps refusing them, which is what user scope (and any
/// caller without a systemd manager) must do.
pub fn compile_execution_plan(
    execution: &ExecutionPlan,
    target: &TargetPlan,
) -> Result<TransactionInput, LinuxInputError> {
    let input = compile_files_only(execution, target)?;
    for (kind, count) in [
        ("launcher", execution.launchers.len()),
        ("PATH entry", execution.path_entries.len()),
        ("service", execution.services.len()),
        ("URI protocol", execution.protocols.len()),
        ("file association", execution.file_associations.len()),
    ] {
        if count > 0 {
            return Err(LinuxInputError::Unsupported { kind });
        }
    }
    for removal in &execution.removals {
        if !matches!(
            &removal.owned,
            OwnedResource::File { .. } | OwnedResource::Backend { .. }
        ) {
            return Err(LinuxInputError::Unsupported {
                kind: "a non-file removal",
            });
        }
    }
    Ok(input)
}

fn compile_files_only(
    execution: &ExecutionPlan,
    target: &TargetPlan,
) -> Result<TransactionInput, LinuxInputError> {
    let mut input = TransactionInput::new(target.target.clone());
    input.selected_components = execution.selected_components.clone();
    input.install_directory = Some(target.install_directory.clone());
    input.uninstall = execution.uninstall;
    // Console and headless installers present no window, so no preset runtime
    // travels with the transaction. A preset here would be bytes without a
    // presenter, which is content without a consumer.
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
                    return Err(LinuxInputError::Undecided {
                        destination: file.destination.to_string(),
                        kind: file.kind,
                    });
                }
            },
        });
    }

    for removal in &execution.removals {
        // Non-file removals are the caller's decision: the user-scope
        // entry point refuses them below, and the machine entry point
        // compiles service removals into typed backend removals.
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

    // The derived databases regenerate from the authoritative sources above:
    // one MIME refresh when a package source changed, one desktop refresh
    // when a desktop entry changed. A refresh that follows removals names
    // them as dependencies, so the database regenerates from the removed
    // world rather than from the sources about to be deleted. No other
    // operation kind reaches this backend, so these are the only refreshes a
    // Linux transaction can hold.
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

/// What machine-scope service compilation needs beyond the plans.
pub struct ServiceCompilation<'a> {
    /// Machine roots for the executable policy check.
    pub roots: &'a crate::machine::MachineRoots,
    /// Unit-source roots (production, or isolated in tests).
    pub systemd: &'a crate::machine::SystemdRoots,
    /// Live manager for the runtime capability preflight and for reading
    /// current unit-file state. Planning fails before filesystem mutation
    /// when systemd is unavailable and services are present.
    pub manager: &'a mut dyn crate::systemd::SystemdManager,
    /// Whether a service `Drift` may be restored: explicit force repair
    /// re-applies owned content, never administrator overrides.
    pub force_services: bool,
}

/// Compile an execution plan holding machine services into a transaction.
///
/// Files lower exactly as [`compile_execution_plan`]; every executable
/// service delta (create, update, restore - and drift under explicit force)
/// becomes one typed service backend operation, and every owned service
/// removal becomes one typed service removal. Conflicts, unforced drift,
/// and any other non-file operation still refuse: dropping one would
/// install the files and silently skip the rest.
pub fn compile_machine_execution_plan(
    execution: &ExecutionPlan,
    target: &TargetPlan,
    compilation: ServiceCompilation<'_>,
) -> Result<TransactionInput, LinuxInputError> {
    let ServiceCompilation {
        roots,
        systemd,
        manager,
        force_services,
    } = compilation;
    if target.scope != zup_core::SelectedScope::Machine {
        return Err(LinuxInputError::Unsupported {
            kind: "a machine service plan in user scope",
        });
    }
    // Reuse the file compilation, then add services. The shared helper
    // refuses services, so strip them for the file pass and compile them
    // below with ownership semantics.
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
    // Launchers, PATH entries, protocols, and associations never reach a
    // machine transaction; services do, through the typed operations below.
    for (kind, count) in [
        ("launcher", execution.launchers.len()),
        ("PATH entry", execution.path_entries.len()),
        ("URI protocol", execution.protocols.len()),
        ("file association", execution.file_associations.len()),
    ] {
        if count > 0 {
            return Err(LinuxInputError::Unsupported { kind });
        }
    }

    // Payload correspondence index: every service binary must resolve to a
    // Zup-owned executable payload in this same transaction.
    let mut target_files = std::collections::BTreeMap::new();
    for file in &target.files {
        target_files.insert(file.destination.to_string(), file.executable);
    }
    // systemd answers before anything is planned against it: a plan that
    // names services without a reachable manager is refused here, before
    // the transaction exists, rather than after the files install. The
    // `Type=exec` baseline rides the same gate: there is no silent
    // fallback to `simple`.
    if !execution.services.is_empty() || !service_removals.is_empty() {
        manager
            .unit_file_state("zup-preflight.service")
            .map(|_| ())
            .or_else(|error| match &error {
                // `unknown unit` proves the manager answered; anything else
                // (no bus, no name, timeout) is the preflight failing.
                crate::systemd::SystemdError::Refused { .. } => Ok(()),
                _ => Err(LinuxInputError::Service {
                    unit: String::new(),
                    reason: format!("systemd is unavailable: {error}"),
                }),
            })?;
        crate::service_ops::require_exec_baseline(manager).map_err(into_service)?;
    }

    for op in &execution.services {
        let unit = crate::services::unit_name(&parse_service_id(op)?).map_err(|error| {
            LinuxInputError::Service {
                unit: op.name.clone(),
                reason: error.to_string(),
            }
        })?;
        let canonical =
            crate::machine::authorize_systemd_unit(&unit, systemd).map_err(|error| {
                LinuxInputError::Service {
                    unit: unit.clone(),
                    reason: error.to_string(),
                }
            })?;
        // Ownership preflight before the transaction exists: collisions,
        // planted symlinks, full administrator overrides, and unsafe
        // executables refuse here, with the unit named.
        crate::service_ops::refuse_source_symlink(&unit, &canonical).map_err(into_service)?;
        crate::service_ops::check_collisions(
            &unit,
            &canonical,
            &crate::service_ops::load_path_dirs(),
        )
        .map_err(into_service)?;
        check_no_full_override(&unit).map_err(into_service)?;
        // Foreign systemd integration refuses before the transaction
        // exists: an alias, an extra dependency, or a runtime link would
        // survive owned-link removal and keep the unit enabled behind the
        // plan's back.
        crate::service_exec::refuse_foreign_integration(&unit, &canonical).map_err(into_service)?;
        // Policy half of executable validation (no filesystem trust at
        // plan time); the worker revalidates the live filesystem.
        validate_service_binary(op, &target_files, roots, &unit)?;
        // The desired bytes render now so the plan digest binds them: an
        // attacker cannot Prepare one service and Execute another.
        let derived = crate::services::DesiredService::derive(&target_service_for(op, target)?)
            .map_err(|error| LinuxInputError::Service {
                unit: unit.clone(),
                reason: error.to_string(),
            })?;
        if derived.unit != unit {
            return Err(LinuxInputError::Service {
                unit,
                reason: "a service payload does not match its operation".into(),
            });
        }
        // A fresh install never overwrites unrelated bytes: the canonical
        // path holding anything but the desired source is a conflict, even
        // when the snapshot could not parse it. Owned updates and forced
        // restores overwrite owned content instead.
        if matches!(op.kind, ServiceOperationKind::Create) {
            let existing =
                crate::service_ops::read_canonical_source(&canonical).map_err(into_service)?;
            if existing.is_some_and(|bytes| bytes != derived.bytes) {
                return Err(LinuxInputError::Service {
                    unit,
                    reason: "the canonical unit path contains unrelated bytes".into(),
                });
            }
        }
        // The previous persistent policy rides the journal: source
        // absence never implies a policy, because policy persists on
        // its own. Read live now; apply re-reads before mutating.
        let previous_policy =
            manager
                .unit_file_state(&unit)
                .map_err(|error| LinuxInputError::Service {
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
                // A deleted source restores; a damaged-but-present source
                // is a conflict without force, restored with it. The
                // planner cannot tell them apart (both observe absent),
                // so the live source decides here.
                let present = crate::service_ops::read_canonical_source(&canonical)
                    .map_err(into_service)?
                    .is_some();
                if present && !force_services {
                    return Err(LinuxInputError::Service {
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
                return Err(LinuxInputError::Service {
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
            return Err(LinuxInputError::Unsupported {
                kind: "a non-service removal",
            });
        };
        let unit = crate::services::unit_name(id).map_err(|error| LinuxInputError::Service {
            unit: name.clone(),
            reason: error.to_string(),
        })?;
        let canonical =
            crate::machine::authorize_systemd_unit(&unit, systemd).map_err(|error| {
                LinuxInputError::Service {
                    unit: unit.clone(),
                    reason: error.to_string(),
                }
            })?;
        crate::service_ops::refuse_source_symlink(&unit, &canonical).map_err(into_service)?;
        check_no_full_override(&unit).map_err(into_service)?;
        // Foreign integration refuses the retirement up front too:
        // removing Zup's links under a live alias would leave the unit
        // enabled with no source, so the administrator cleans up first.
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

fn parse_service_id(
    op: &zup_exec::ServiceOperation,
) -> Result<zup_core::ServiceId, LinuxInputError> {
    zup_core::ServiceId::new(&op.id).map_err(|error| LinuxInputError::Service {
        unit: op.name.clone(),
        reason: format!("service id: {error}"),
    })
}

fn into_service(error: crate::service_ops::ServiceError) -> LinuxInputError {
    match error {
        crate::service_ops::ServiceError::Refused { unit, reason }
        | crate::service_ops::ServiceError::Conflict { unit, reason }
        | crate::service_ops::ServiceError::Drift { unit, reason }
        | crate::service_ops::ServiceError::Ambiguous { unit, reason } => {
            LinuxInputError::Service { unit, reason }
        }
        crate::service_ops::ServiceError::Systemd(error) => LinuxInputError::Service {
            unit: String::new(),
            reason: error.to_string(),
        },
    }
}

/// A full higher-precedence administrator override shadows the source.
/// Repair must not delete it and install must not claim beneath it.
fn check_no_full_override(unit: &str) -> Result<(), crate::service_ops::ServiceError> {
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
) -> Result<(), LinuxInputError> {
    crate::service_ops::validate_executable(&op.command, target_files, roots, 0, false)
        .map(|_| ())
        .map_err(|error| match error {
            crate::service_ops::ServiceError::Refused { reason, .. }
            | crate::service_ops::ServiceError::Conflict { reason, .. }
            | crate::service_ops::ServiceError::Drift { reason, .. }
            | crate::service_ops::ServiceError::Ambiguous { reason, .. } => {
                LinuxInputError::Service {
                    unit: unit.to_owned(),
                    reason,
                }
            }
            crate::service_ops::ServiceError::Systemd(error) => LinuxInputError::Service {
                unit: unit.to_owned(),
                reason: error.to_string(),
            },
        })
}

/// Rebuild the target service one operation was planned from, so the unit
/// bytes render deterministically from the same identity the snapshot and
/// the planner used.
fn target_service_for(
    op: &zup_exec::ServiceOperation,
    target: &TargetPlan,
) -> Result<zup_platform::TargetService, LinuxInputError> {
    let planned = target
        .services
        .iter()
        .find(|service| service.key == op.key)
        .ok_or_else(|| LinuxInputError::Service {
            unit: op.name.clone(),
            reason: "the service is not in the target plan".into(),
        })?;
    Ok(planned.clone())
}

/// Whether a payload source is generated integration content rather than
/// package content.
fn is_generated_source(source: &str) -> bool {
    source == GENERATED_PREFIX || source.starts_with(&format!("{GENERATED_PREFIX}/"))
}

/// Which derived database a generated source feeds, by its stable prefix.
/// Unknown generated names refresh nothing: a future generated kind must opt
/// into its refresh explicitly rather than inherit one.
fn refresh_for_generated(source: &str) -> (bool, bool) {
    let mime = source.starts_with(&format!("{GENERATED_PREFIX}/mime/"));
    let desktop = source.starts_with(&format!("{GENERATED_PREFIX}/applications/"));
    (mime, desktop)
}

#[cfg(test)]
mod tests {
    use super::*;
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

    /// One machine target plan with a single service whose binary is a
    /// payload file of the same plan.
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
    ) -> Result<TransactionInput, LinuxInputError> {
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

    /// Every executable delta compiles to exactly one typed backend
    /// operation; `NoOp` compiles to none.
    #[test]
    fn executable_deltas_become_typed_operations() {
        let (base, roots, systemd) = isolated();
        let plan = service_plan(&base, ServiceStart::Automatic, vec!["--serve".into()]);
        let executable = plan.services[0].command.executable.clone();
        for (kind, expected) in [
            (ServiceOperationKind::Create, 1),
            (ServiceOperationKind::UpdateOwned, 1),
            (ServiceOperationKind::RestoreOwned, 1),
            (ServiceOperationKind::NoOp, 0),
        ] {
            let mut manager = crate::systemd::FakeSystemd::default();
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
    }

    /// All nine start-policy transitions compile: source and policy may
    /// both change, and neither restarts anything (there is no start API
    /// on the narrow manager surface to call).
    #[test]
    fn every_start_policy_transition_compiles() {
        use ServiceStart::{Automatic, Disabled, Manual};
        for (from, to) in [
            (Automatic, Automatic),
            (Automatic, Manual),
            (Automatic, Disabled),
            (Manual, Automatic),
            (Manual, Manual),
            (Manual, Disabled),
            (Disabled, Automatic),
            (Disabled, Manual),
            (Disabled, Disabled),
        ] {
            let (base, roots, systemd) = isolated();
            let plan = service_plan(&base, to, vec!["--serve".into()]);
            let executable = plan.services[0].command.executable.clone();
            let mut manager = crate::systemd::FakeSystemd::default();
            let desired = operation_at(
                &executable,
                vec!["--serve".into()],
                to,
                ServiceOperationKind::UpdateOwned,
            );
            let execution = execution(vec![desired]);
            let input = compile(&plan, &execution, &roots, &systemd, &mut manager, false)
                .expect("a transition compiles");
            assert_eq!(input.backend_operations.len(), 1, "{from:?} -> {to:?}");
        }
    }

    /// A manager older than the `Type=exec` baseline refuses planning
    /// before any mutation, with no silent fallback to `simple`.
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
        let mut manager = crate::systemd::FakeSystemd::default();
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

    /// Drift refuses without force and restores with it; conflict always
    /// refuses. Force resolves owned-content conflicts, never
    /// administrator overrides.
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
        let mut manager = crate::systemd::FakeSystemd::default();
        assert!(compile(&plan, &drift(), &roots, &systemd, &mut manager, false).is_err());
        let mut manager = crate::systemd::FakeSystemd::default();
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
        let mut manager = crate::systemd::FakeSystemd::default();
        assert!(compile(&plan, &conflict, &roots, &systemd, &mut manager, true).is_err());
    }

    /// The plan digest binds the service: changing the start policy
    /// changes the prepared fingerprint, so an attacker cannot Prepare one
    /// service and Execute another.
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
        let mut manager = crate::systemd::FakeSystemd::default();
        let left_input = compile(
            &left,
            &arguments(ServiceStart::Automatic),
            &roots,
            &systemd,
            &mut manager,
            false,
        )
        .expect("a plan compiles");
        let mut manager = crate::systemd::FakeSystemd::default();
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

    /// No systemd, no service transaction: the preflight fails before
    /// filesystem mutation, and the unit tree stays empty.
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
        let mut manager = crate::systemd::FakeSystemd::default();
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

    /// A user-scope plan holding services refuses: user units stay
    /// unsupported even at the transaction boundary.
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
        let mut manager = crate::systemd::FakeSystemd::default();
        assert!(compile(&plan, &execution, &roots, &systemd, &mut manager, false).is_err());
    }

    #[test]
    fn observed_services_snapshot_through_the_fake() {
        let (base, roots, systemd) = isolated();
        let _ = roots;
        let plan = service_plan(&base, ServiceStart::Automatic, vec!["--serve".into()]);
        let mut manager = crate::systemd::FakeSystemd::default();
        let observed =
            crate::snapshot::snapshot_services(&plan, &mut manager, &systemd).expect("a snapshot");
        assert_eq!(observed.len(), 1);
        assert_eq!(
            observed[0].state,
            ObservedServiceState::Absent,
            "an uninstalled service observes absent"
        );
    }
}
