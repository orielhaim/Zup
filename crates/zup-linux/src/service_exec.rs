use zup_core::{Privilege, ResourceKey, ServiceStart};
use zup_exec::{ObservedServiceState, OwnedResource, ServiceOperation};
use zup_platform::{CommandSpec, TargetService};

use crate::error::ExecError;
use crate::fs::{EntryKind, OwnedDirectory};
use crate::machine::{MachineRoots, SystemdRoots, authorize_systemd_unit};
use crate::service_ops::{
    ServicePayload, ServiceReceipt, check_collisions, desired_policy, encode_payload,
    load_path_dirs, policy_for_state, refuse_source_symlink, validate_changes,
    validate_executable_live,
};
use crate::services::{render_unit, unit_name};
use crate::systemd::{SystemdManager, UnitChange};

pub struct ServiceContext<'a> {
    pub roots: &'a MachineRoots,
    pub systemd: &'a SystemdRoots,
    pub manager: &'a mut dyn SystemdManager,
    pub expected_uid: u32,
}

#[derive(Debug, Clone)]
struct Derived {
    unit: String,
    bytes: Vec<u8>,
    command: CommandSpec,
    start: ServiceStart,
}

fn derive_operation(op: &ServiceOperation) -> Result<Derived, ExecError> {
    let id = zup_core::ServiceId::new(&op.id).map_err(|error| ExecError::Refused {
        unit: op.name.clone(),
        reason: format!("service id: {error}"),
    })?;
    let unit = unit_name(&id).map_err(|error| ExecError::Refused {
        unit: op.name.clone(),
        reason: error.to_string(),
    })?;
    let name = zup_core::NonEmptyString::new(&op.name).map_err(|error| ExecError::Refused {
        unit: unit.clone(),
        reason: format!("service name: {error}"),
    })?;
    let display =
        zup_core::NonEmptyString::new(&op.display_name).map_err(|error| ExecError::Refused {
            unit: unit.clone(),
            reason: format!("service display name: {error}"),
        })?;
    let synthetic = TargetService {
        key: op.key.clone(),
        id,
        name,
        display_name: Some(display),
        command: op.command.clone(),
        start: op.start,
        privilege: op.privilege,
    };
    let bytes = render_unit(&synthetic, &op.display_name).map_err(|error| ExecError::Refused {
        unit: unit.clone(),
        reason: error.to_string(),
    })?;
    if bytes.len() > crate::service_ops::MAX_UNIT_BYTES {
        return Err(ExecError::Refused {
            unit,
            reason: "a unit source exceeds its bound".into(),
        });
    }
    Ok(Derived {
        unit,
        bytes,
        command: op.command.clone(),
        start: op.start,
    })
}

fn render_registration(
    unit: &str,
    display: &str,
    command: &CommandSpec,
    start: ServiceStart,
    key: &ResourceKey,
    id: &zup_core::ServiceId,
    name: &zup_core::NonEmptyString,
) -> Result<Vec<u8>, ExecError> {
    let display = zup_core::NonEmptyString::new(display).map_err(|error| ExecError::Refused {
        unit: unit.to_owned(),
        reason: format!("service display name: {error}"),
    })?;
    let synthetic = TargetService {
        key: key.clone(),
        id: id.clone(),
        name: name.clone(),
        display_name: Some(display),
        command: command.clone(),
        start,
        privilege: Privilege::System,
    };
    render_unit(
        &synthetic,
        synthetic.display_name.as_ref().expect("just set").as_str(),
    )
    .map_err(|error| ExecError::Refused {
        unit: unit.to_owned(),
        reason: error.to_string(),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Observed {
    source: Option<([u8; 32], u64)>,
    policy: String,
}

fn observe(
    unit: &str,
    canonical: &std::path::Path,
    manager: &mut dyn SystemdManager,
) -> Result<Observed, ExecError> {
    let source = read_source(canonical)?;
    let policy = manager
        .unit_file_state(unit)
        .unwrap_or_else(|_| "unknown".to_owned());
    Ok(Observed { source, policy })
}

fn read_source(canonical: &std::path::Path) -> Result<Option<([u8; 32], u64)>, ExecError> {
    let Some(parent) = canonical.parent().filter(|p| !p.as_os_str().is_empty()) else {
        return Ok(None);
    };
    let Ok(directory) = OwnedDirectory::open(parent) else {
        return Ok(None);
    };
    let Some(name) = canonical
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
    else {
        return Ok(None);
    };
    match directory.kind_or_absent(&name) {
        Ok(Some(EntryKind::Regular)) => match directory.read_regular(&name) {
            Ok(bytes) => {
                let (size, digest) =
                    zup_core::hash_reader(bytes.as_slice()).map_err(|_| ExecError::Ambiguous {
                        unit: name.clone(),
                        reason: "a unit source does not hash".into(),
                    })?;
                Ok(Some((*digest.as_bytes(), size)))
            }
            Err(_) => Ok(None),
        },
        _ => Ok(None),
    }
}

fn digest_of(bytes: &[u8]) -> ([u8; 32], u64) {
    let (size, digest) = zup_core::hash_reader(bytes).expect("a unit renders hashable bytes");
    (*digest.as_bytes(), size)
}

fn hex(digest: &[u8; 32]) -> String {
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

fn previous_source_bytes(unit: &str, op: &ServiceOperation) -> Result<Option<Vec<u8>>, ExecError> {
    match &op.previous {
        ObservedServiceState::Absent => Ok(None),
        ObservedServiceState::Service {
            display_name,
            command,
            start,
            ..
        } => {
            let name =
                zup_core::NonEmptyString::new(&op.name).map_err(|error| ExecError::Refused {
                    unit: unit.to_owned(),
                    reason: format!("service name: {error}"),
                })?;
            let id = zup_core::ServiceId::new(&op.id).map_err(|error| ExecError::Refused {
                unit: unit.to_owned(),
                reason: format!("service id: {error}"),
            })?;
            render_registration(unit, display_name, command, *start, &op.key, &id, &name).map(Some)
        }
    }
}

fn check_unchanged_resume(
    derived: &Derived,
    before: &Observed,
    op: &ServiceOperation,
    previous_policy: &str,
    force: bool,
) -> Result<(), ExecError> {
    if before.source.map(|(d, _)| hex(&d)) == Some(hex(&digest_of(&derived.bytes).0)) {
        return Ok(());
    }

    if force {
        return Ok(());
    }
    check_unchanged(&derived.unit, before, op, previous_policy)
}

fn check_unchanged(
    unit: &str,
    before: &Observed,
    op: &ServiceOperation,
    previous_policy: &str,
) -> Result<(), ExecError> {
    let wanted_source = previous_source_bytes(unit, op)?;
    let wanted_digest = wanted_source.as_ref().map(|bytes| hex(&digest_of(bytes).0));
    if before.source.map(|(d, _)| hex(&d)) != wanted_digest || before.policy != previous_policy {
        return Err(ExecError::Refused {
            unit: unit.to_owned(),
            reason: "the service changed since planning".into(),
        });
    }
    Ok(())
}

fn prepare(
    op: &ServiceOperation,
    payload_bytes: &[u8],
) -> Result<(Derived, ServicePayload), ExecError> {
    let derived = derive_operation(op)?;
    let payload: ServicePayload =
        crate::service_ops::decode_payload(payload_bytes).map_err(|error| ExecError::Refused {
            unit: derived.unit.clone(),
            reason: error.to_string(),
        })?;
    let ServicePayload::Apply {
        service,
        unit,
        unit_bytes,
        binary_owned,
        ..
    } = &payload
    else {
        return Err(ExecError::Refused {
            unit: derived.unit.clone(),
            reason: "a service apply node holds apply intent".into(),
        });
    };
    if *unit != derived.unit || *unit_bytes != derived.bytes || *service != *op || !binary_owned {
        return Err(ExecError::Refused {
            unit: derived.unit.clone(),
            reason: "a service payload does not match its operation".into(),
        });
    }
    Ok((derived, payload))
}

pub fn apply(
    op: &ServiceOperation,
    payload_bytes: &[u8],
    context: &mut ServiceContext<'_>,
) -> Result<ServiceReceipt, ExecError> {
    let (derived, payload) = prepare(op, payload_bytes)?;
    let ServicePayload::Apply {
        previous_policy: planned_policy,
        force: planned_force,
        ..
    } = &payload
    else {
        return Err(ExecError::Refused {
            unit: derived.unit.clone(),
            reason: "a service apply node holds apply intent".into(),
        });
    };
    let planned_policy = planned_policy.clone();
    let planned_force = *planned_force;
    let canonical = authorize_systemd_unit(&derived.unit, context.systemd).map_err(|error| {
        ExecError::Refused {
            unit: derived.unit.clone(),
            reason: error.to_string(),
        }
    })?;
    refuse_source_symlink(&derived.unit, &canonical)?;
    check_collisions(&derived.unit, &canonical, &load_path_dirs())?;

    validate_executable_live(&derived.command, context.roots, context.expected_uid)?;

    let probe = context
        .manager
        .unit_file_state(&derived.unit)
        .map_err(ExecError::from)?;
    if policy_for_state(&probe).is_none() {
        return Err(ExecError::Refused {
            unit: derived.unit.clone(),
            reason: format!("systemd reports unit-file state `{probe}`, which Zup does not own"),
        });
    }

    crate::service_ops::require_exec_baseline(context.manager)?;
    refuse_foreign_integration(&derived.unit, &canonical)?;
    let before = observe(&derived.unit, &canonical, context.manager)?;

    let previous_source = before.source.map(|(digest, _)| hex(&digest));

    let wanted_digest = hex(&digest_of(&derived.bytes).0);
    let wanted_policy = desired_policy(derived.start).to_owned();
    if before.source.map(|(d, _)| hex(&d)) == Some(wanted_digest.clone())
        && before.policy == wanted_policy
    {
        verify_installed(&derived, &canonical, context.manager)?;
        return Ok(ServiceReceipt {
            unit: derived.unit.clone(),
            previous_source_sha256: previous_source,
            installed_source_sha256: Some(wanted_digest),
            previous_policy: planned_policy,
            installed_policy: wanted_policy,
            reloaded: true,
            changes: Vec::new(),
        });
    }
    check_unchanged_resume(&derived, &before, op, &planned_policy, planned_force)?;
    let must_be_absent = before.source.is_none();

    let previous_bytes_rendered = previous_source_bytes(&derived.unit, op)?;
    write_source(&derived.unit, &canonical, &derived.bytes, must_be_absent)?;
    let mutated = apply_reload_policy_verify(&derived, &canonical, context);
    if let Err(error) = mutated {
        compensate_source(
            &derived.unit,
            &canonical,
            previous_bytes_rendered.as_deref(),
            context,
        );
        return Err(error);
    }
    let changes = mutated.expect("compensated above on error");
    Ok(ServiceReceipt {
        unit: derived.unit.clone(),
        previous_source_sha256: previous_source,
        installed_source_sha256: Some(hex(&digest_of(&derived.bytes).0)),
        previous_policy: planned_policy,
        installed_policy: desired_policy(derived.start).to_owned(),
        reloaded: true,
        changes,
    })
}

fn apply_reload_policy_verify(
    derived: &Derived,
    canonical: &std::path::Path,
    context: &mut ServiceContext<'_>,
) -> Result<Vec<UnitChange>, ExecError> {
    context.manager.reload().map_err(ExecError::from)?;
    let mut changes = Vec::new();
    apply_policy(
        &derived.unit,
        canonical,
        derived.start,
        context.manager,
        &mut changes,
    )?;
    for change in &changes {
        validate_changes(&derived.unit, std::slice::from_ref(change))?;
    }
    context.manager.reload().map_err(ExecError::from)?;
    verify_installed(derived, canonical, context.manager)?;
    Ok(changes)
}

fn compensate_source(
    unit: &str,
    canonical: &std::path::Path,
    previous: Option<&[u8]>,
    context: &mut ServiceContext<'_>,
) {
    match previous {
        None => {
            let _ = remove_file_no_follow(unit, canonical);
        }
        Some(bytes) => {
            let _ = write_source(unit, canonical, bytes, false);
        }
    }
    let _ = context.manager.reload();
}

fn write_source(
    unit: &str,
    canonical: &std::path::Path,
    bytes: &[u8],
    must_be_absent: bool,
) -> Result<(), ExecError> {
    let Some(parent) = canonical.parent().filter(|p| !p.as_os_str().is_empty()) else {
        return Err(ExecError::refused(
            unit,
            "a unit source has a parent directory",
        ));
    };
    refuse_source_symlink(unit, canonical)?;
    if OwnedDirectory::open(parent).is_err() {
        std::fs::create_dir_all(parent).map_err(|error| ExecError::Refused {
            unit: unit.to_owned(),
            reason: format!("the unit directory cannot be created: {error}"),
        })?;
    }
    let directory = OwnedDirectory::open(parent).map_err(|error| ExecError::Refused {
        unit: unit.to_owned(),
        reason: format!("the unit directory cannot be opened: {error}"),
    })?;
    let name = canonical
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty())
        .ok_or_else(|| ExecError::refused(unit, "a unit source has a file name"))?;
    use rustix::fs::Mode;
    if must_be_absent {
        directory
            .create_durable_exclusive(&name, bytes, Mode::from_bits_truncate(0o644))
            .map_err(|error| ExecError::Refused {
                unit: unit.to_owned(),
                reason: format!("the unit source cannot be created: {error}"),
            })?;
    } else {
        directory
            .write_durable(&name, bytes, Mode::from_bits_truncate(0o644))
            .map_err(|error| ExecError::Refused {
                unit: unit.to_owned(),
                reason: format!("the unit source cannot be written: {error}"),
            })?;
    }
    crate::fs::sync_directory(parent).map_err(|error| ExecError::Refused {
        unit: unit.to_owned(),
        reason: format!("the unit directory does not flush: {error}"),
    })?;
    Ok(())
}

fn apply_policy(
    unit: &str,
    canonical: &std::path::Path,
    start: ServiceStart,
    manager: &mut dyn SystemdManager,
    changes: &mut Vec<UnitChange>,
) -> Result<(), ExecError> {
    let canonical_text = canonical.to_string_lossy().into_owned();
    match start {
        ServiceStart::Automatic => {
            let state = manager.unit_file_state(unit).map_err(ExecError::from)?;
            if state == "masked" || state == "masked-runtime" {
                changes.extend(manager.unmask(unit).map_err(ExecError::from)?);
            }
            changes.extend(manager.enable(unit).map_err(ExecError::from)?);
        }
        ServiceStart::Manual => {
            let state = manager.unit_file_state(unit).map_err(ExecError::from)?;
            if state == "masked" || state == "masked-runtime" {
                changes.extend(manager.unmask(unit).map_err(ExecError::from)?);
            }
            if state == "enabled" || state == "enabled-runtime" {
                changes.extend(
                    manager
                        .remove_owned_enablement(unit, &canonical_text)
                        .map_err(ExecError::from)?,
                );
            }
        }
        ServiceStart::Disabled => {
            let state = manager.unit_file_state(unit).map_err(ExecError::from)?;
            if state == "enabled" || state == "enabled-runtime" {
                changes.extend(
                    manager
                        .remove_owned_enablement(unit, &canonical_text)
                        .map_err(ExecError::from)?,
                );
            }
            changes.extend(manager.mask(unit).map_err(ExecError::from)?);
        }
    }
    Ok(())
}

pub(crate) fn refuse_foreign_integration(
    unit: &str,
    canonical: &std::path::Path,
) -> Result<(), ExecError> {
    let foreign = find_foreign_integration(
        unit,
        canonical,
        std::path::Path::new("/etc/systemd/system"),
        std::path::Path::new("/run/systemd/system"),
    );
    if foreign.is_empty() {
        return Ok(());
    }
    Err(ExecError::Ambiguous {
        unit: unit.to_owned(),
        reason: format!(
            "unrelated systemd integration exists, refusing to change enablement around it: {}",
            foreign
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    })
}

fn find_foreign_integration(
    unit: &str,
    canonical: &std::path::Path,
    etc_root: &std::path::Path,
    run_root: &std::path::Path,
) -> Vec<std::path::PathBuf> {
    fn is_symlink(path: &std::path::Path) -> bool {
        std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink())
    }

    fn is_real_dir(path: &std::path::Path) -> bool {
        std::fs::symlink_metadata(path)
            .is_ok_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
    }

    let canonical_text = canonical.to_string_lossy();
    let mut foreign = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    let mut push = |path: std::path::PathBuf| {
        if seen.insert(path.clone()) {
            foreign.push(path);
        }
    };

    for root in [etc_root, run_root] {
        let Ok(layer) = std::fs::read_dir(root) else {
            continue;
        };
        for entry in layer.flatten() {
            let path = entry.path();
            let Some(name) = path
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_owned)
            else {
                continue;
            };
            if !is_real_dir(&path) {
                if name == unit && is_symlink(&path) {
                    let target = std::fs::read_link(&path).unwrap_or_default();
                    if target.to_string_lossy() != canonical_text
                        && target.to_string_lossy() != "/dev/null"
                    {
                        push(path.clone());
                    }
                }

                if name != unit
                    && name.ends_with(".service")
                    && is_symlink(&path)
                    && std::fs::read_link(&path)
                        .is_ok_and(|target| target.to_string_lossy() == canonical_text)
                {
                    push(path.clone());
                }
                continue;
            }

            let stem = name
                .strip_suffix(".wants")
                .or_else(|| name.strip_suffix(".requires"));
            if stem.is_none() {
                continue;
            }
            let candidate = path.join(unit);
            if candidate == etc_root.join("multi-user.target.wants").join(unit) {
                continue;
            }
            if is_symlink(&candidate) {
                push(candidate);
            }
        }
    }
    foreign.sort();
    foreign
}

fn verify_installed(
    derived: &Derived,
    canonical: &std::path::Path,
    manager: &mut dyn SystemdManager,
) -> Result<(), ExecError> {
    let info = manager.load_unit(&derived.unit).map_err(ExecError::from)?;
    let wanted = desired_policy(derived.start);
    if info.unit_file_state != wanted {
        return Err(ExecError::Refused {
            unit: derived.unit.clone(),
            reason: format!(
                "systemd reports unit-file state `{}`, want `{wanted}`",
                info.unit_file_state
            ),
        });
    }
    if derived.start == ServiceStart::Disabled {
        if info.load_state != "masked" && info.fragment_path != "/dev/null" {
            return Err(ExecError::Refused {
                unit: derived.unit.clone(),
                reason: format!(
                    "a masked unit loads as `{}`, not from `/dev/null`",
                    info.fragment_path
                ),
            });
        }
        return Ok(());
    }
    if info.fragment_path != canonical.to_string_lossy() {
        return Err(ExecError::Refused {
            unit: derived.unit.clone(),
            reason: format!(
                "systemd loads `{}` instead of the Zup source",
                info.fragment_path
            ),
        });
    }
    if info.load_state != "loaded" {
        return Err(ExecError::Refused {
            unit: derived.unit.clone(),
            reason: format!("the unit loads as `{}`, not `loaded`", info.load_state),
        });
    }
    Ok(())
}

pub fn rollback_apply(
    payload: &ServicePayload,
    receipt: &ServiceReceipt,
    context: &mut ServiceContext<'_>,
) -> Result<(), ExecError> {
    let ServicePayload::Apply {
        service: op,
        unit: payload_unit,
        ..
    } = payload
    else {
        return Err(ExecError::Refused {
            unit: receipt.unit.clone(),
            reason: "a service apply rollback holds apply intent".into(),
        });
    };
    let derived = derive_operation(op)?;
    if receipt.unit != derived.unit || *payload_unit != derived.unit {
        return Err(ExecError::refused(
            &derived.unit,
            "a receipt for another unit",
        ));
    }
    let canonical = authorize_systemd_unit(&derived.unit, context.systemd).map_err(|error| {
        ExecError::Refused {
            unit: derived.unit.clone(),
            reason: error.to_string(),
        }
    })?;
    let current = observe(&derived.unit, &canonical, context.manager)?;
    if current.source.map(|(d, _)| hex(&d)) != receipt.installed_source_sha256
        || current.policy != receipt.installed_policy
    {
        return Err(ExecError::Refused {
            unit: derived.unit.clone(),
            reason: "the service changed after installation".into(),
        });
    }
    match &op.previous {
        ObservedServiceState::Absent => {
            remove_file_no_follow(&derived.unit, &canonical)?;
        }
        ObservedServiceState::Service {
            display_name,
            command,
            start,
            ..
        } => {
            let name =
                zup_core::NonEmptyString::new(&op.name).map_err(|error| ExecError::Refused {
                    unit: derived.unit.clone(),
                    reason: format!("service name: {error}"),
                })?;
            let id = zup_core::ServiceId::new(&op.id).map_err(|error| ExecError::Refused {
                unit: derived.unit.clone(),
                reason: format!("service id: {error}"),
            })?;
            let bytes = render_registration(
                &derived.unit,
                display_name,
                command,
                *start,
                &op.key,
                &id,
                &name,
            )?;
            write_source(&derived.unit, &canonical, &bytes, false)?;
        }
    }
    context.manager.reload().map_err(ExecError::from)?;
    restore_policy(
        &derived.unit,
        &canonical,
        &receipt.previous_policy,
        context.manager,
    )?;
    Ok(())
}

fn restore_policy(
    unit: &str,
    canonical: &std::path::Path,
    previous: &str,
    manager: &mut dyn SystemdManager,
) -> Result<(), ExecError> {
    match previous {
        "enabled" => {
            manager.unmask(unit).map_err(ExecError::from)?;
            manager.enable(unit).map_err(ExecError::from)?;
        }
        "disabled" => {
            manager.unmask(unit).map_err(ExecError::from)?;
            manager
                .remove_owned_enablement(unit, &canonical.to_string_lossy())
                .map_err(ExecError::from)?;
        }
        "masked" => {
            manager.mask(unit).map_err(ExecError::from)?;
        }
        _ => {
            manager.unmask(unit).map_err(ExecError::from)?;
        }
    }
    manager.reload().map_err(ExecError::from)?;
    Ok(())
}

fn remove_file_no_follow(unit: &str, canonical: &std::path::Path) -> Result<(), ExecError> {
    let Some(parent) = canonical.parent().filter(|p| !p.as_os_str().is_empty()) else {
        return Ok(());
    };
    let Ok(directory) = OwnedDirectory::open(parent) else {
        return Ok(());
    };
    let Some(name) = canonical
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
    else {
        return Ok(());
    };
    match directory.kind_or_absent(&name) {
        Ok(None) => Ok(()),
        Ok(Some(EntryKind::Regular)) => {
            directory
                .remove_file(&name)
                .map_err(|error| ExecError::Refused {
                    unit: unit.to_owned(),
                    reason: format!("the unit source cannot be removed: {error}"),
                })?;
            directory.sync().map_err(|error| ExecError::Refused {
                unit: unit.to_owned(),
                reason: format!("the unit directory does not flush: {error}"),
            })?;
            Ok(())
        }
        Ok(Some(_)) => Err(ExecError::conflict(
            unit,
            "the unit source path is no longer a regular file",
        )),
        Err(_) => Ok(()),
    }
}

pub fn apply_remove(
    key: &ResourceKey,
    unit: &str,
    owned: &OwnedResource,
    context: &mut ServiceContext<'_>,
) -> Result<ServiceReceipt, ExecError> {
    let OwnedResource::Service {
        name, installed, ..
    } = owned
    else {
        return Err(ExecError::refused(
            unit,
            "a service removal names a service",
        ));
    };
    let ResourceKey::Service { id } = key else {
        return Err(ExecError::refused(
            unit,
            "a service removal names a service key",
        ));
    };
    let expected = unit_name(id).map_err(|error| ExecError::Refused {
        unit: unit.to_owned(),
        reason: error.to_string(),
    })?;
    if expected != unit {
        return Err(ExecError::refused(unit, "a removal for another unit"));
    }
    let canonical =
        authorize_systemd_unit(unit, context.systemd).map_err(|error| ExecError::Refused {
            unit: unit.to_owned(),
            reason: error.to_string(),
        })?;
    refuse_source_symlink(unit, &canonical)?;

    crate::service_ops::check_no_full_override(unit, &crate::service_ops::admin_override_dir())
        .map_err(|error| ExecError::Refused {
            unit: unit.to_owned(),
            reason: error.to_string(),
        })?;
    let zup_exec::ServiceState::Registration {
        display_name,
        command,
        start,
    } = installed
    else {
        return Err(ExecError::refused(
            unit,
            "owned service state is a registration",
        ));
    };
    let service_name = zup_core::NonEmptyString::new(name).map_err(|error| ExecError::Refused {
        unit: unit.to_owned(),
        reason: format!("service name: {error}"),
    })?;
    let installed_bytes =
        render_registration(unit, display_name, command, *start, key, id, &service_name)?;

    let retired = observe(unit, &canonical, context.manager)?;
    if retired.source.is_none() && retired.policy != "enabled" && retired.policy != "masked" {
        return Ok(ServiceReceipt {
            unit: unit.to_owned(),
            previous_source_sha256: Some(hex(&digest_of(&installed_bytes).0)),
            installed_source_sha256: None,
            previous_policy: retired.policy.clone(),
            installed_policy: retired.policy,
            reloaded: true,
            changes: Vec::new(),
        });
    }

    refuse_foreign_integration(unit, &canonical)?;
    let mut changes = Vec::new();
    let state = context
        .manager
        .unit_file_state(unit)
        .map_err(ExecError::from)?;
    if state == "masked" || state == "masked-runtime" {
        changes.extend(context.manager.unmask(unit).map_err(ExecError::from)?);
    }
    if state == "enabled" || state == "enabled-runtime" {
        changes.extend(
            context
                .manager
                .remove_owned_enablement(unit, &canonical.to_string_lossy())
                .map_err(ExecError::from)?,
        );
    }
    remove_file_no_follow(unit, &canonical)?;
    context.manager.reload().map_err(ExecError::from)?;
    let after = observe(unit, &canonical, context.manager)?;
    if after.source.is_some() {
        return Err(ExecError::Refused {
            unit: unit.to_owned(),
            reason: "the unit source is still there".into(),
        });
    }
    if after.policy == "enabled" || after.policy == "masked" {
        return Err(ExecError::Refused {
            unit: unit.to_owned(),
            reason: format!("persistent state `{}` survives removal", after.policy),
        });
    }
    Ok(ServiceReceipt {
        unit: unit.to_owned(),
        previous_source_sha256: Some(hex(&digest_of(&installed_bytes).0)),
        installed_source_sha256: None,
        previous_policy: state,
        installed_policy: after.policy,
        reloaded: true,
        changes,
    })
}

pub fn rollback_remove(
    key: &ResourceKey,
    unit: &str,
    owned: &OwnedResource,
    receipt: &ServiceReceipt,
    context: &mut ServiceContext<'_>,
) -> Result<(), ExecError> {
    let OwnedResource::Service {
        name, installed, ..
    } = owned
    else {
        return Err(ExecError::refused(
            unit,
            "a service removal names a service",
        ));
    };
    let ResourceKey::Service { id } = key else {
        return Err(ExecError::refused(
            unit,
            "a service removal names a service key",
        ));
    };
    let zup_exec::ServiceState::Registration {
        display_name,
        command,
        start,
    } = installed
    else {
        return Err(ExecError::refused(
            unit,
            "owned service state is a registration",
        ));
    };
    let canonical =
        authorize_systemd_unit(unit, context.systemd).map_err(|error| ExecError::Refused {
            unit: unit.to_owned(),
            reason: error.to_string(),
        })?;
    let current = observe(unit, &canonical, context.manager)?;
    if current.source.is_some() {
        return Err(ExecError::Refused {
            unit: unit.to_owned(),
            reason: "the service changed after removal".into(),
        });
    }
    let service_name = zup_core::NonEmptyString::new(name).map_err(|error| ExecError::Refused {
        unit: unit.to_owned(),
        reason: format!("service name: {error}"),
    })?;
    let bytes = render_registration(unit, display_name, command, *start, key, id, &service_name)?;
    write_source(unit, &canonical, &bytes, true)?;
    context.manager.reload().map_err(ExecError::from)?;
    restore_policy(unit, &canonical, &receipt.previous_policy, context.manager)?;
    Ok(())
}

pub fn verify_receipt(
    receipt: &ServiceReceipt,
    context: &mut ServiceContext<'_>,
) -> Result<(), ExecError> {
    let canonical = authorize_systemd_unit(&receipt.unit, context.systemd).map_err(|error| {
        ExecError::Refused {
            unit: receipt.unit.clone(),
            reason: error.to_string(),
        }
    })?;
    let current = observe(&receipt.unit, &canonical, context.manager)?;
    if current.source.map(|(d, _)| hex(&d)) != receipt.installed_source_sha256 {
        return Err(ExecError::Refused {
            unit: receipt.unit.clone(),
            reason: "the unit source is not the installed one".into(),
        });
    }
    if current.policy != receipt.installed_policy {
        return Err(ExecError::Refused {
            unit: receipt.unit.clone(),
            reason: "the persistent policy is not the installed one".into(),
        });
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceReconcile {
    Applied,
    NotApplied,
    Ambiguous,
}

pub fn reconcile_apply(
    op: &ServiceOperation,
    previous_policy: &str,
    receipt: Option<&ServiceReceipt>,
    context: &mut ServiceContext<'_>,
) -> Result<(ServiceReconcile, Option<ServiceReceipt>), ExecError> {
    let derived = derive_operation(op)?;
    let canonical = authorize_systemd_unit(&derived.unit, context.systemd).map_err(|error| {
        ExecError::Refused {
            unit: derived.unit.clone(),
            reason: error.to_string(),
        }
    })?;
    let current = observe(&derived.unit, &canonical, context.manager)?;
    let intended_digest = hex(&digest_of(&derived.bytes).0);
    let intended_policy = desired_policy(derived.start).to_owned();
    let current_digest = current.source.map(|(d, _)| hex(&d));
    let installed = current_digest.clone() == Some(intended_digest.clone())
        && current.policy == intended_policy;
    match receipt {
        Some(receipt) => {
            if current_digest.clone() == receipt.installed_source_sha256
                && current.policy == receipt.installed_policy
            {
                Ok((ServiceReconcile::Applied, Some(receipt.clone())))
            } else {
                let previous_source = previous_source_bytes(&derived.unit, op)?;
                let previous_digest = previous_source
                    .as_ref()
                    .map(|bytes| hex(&digest_of(bytes).0));
                if current_digest.clone() == previous_digest && current.policy == previous_policy {
                    Ok((ServiceReconcile::NotApplied, None))
                } else if installed {
                    Ok((ServiceReconcile::Applied, Some(receipt.clone())))
                } else if current_digest == Some(intended_digest.clone()) {
                    Ok((ServiceReconcile::NotApplied, None))
                } else {
                    Ok((ServiceReconcile::Ambiguous, None))
                }
            }
        }
        None => {
            if installed {
                let previous_source = previous_source_bytes(&derived.unit, op)?;
                Ok((
                    ServiceReconcile::Applied,
                    Some(ServiceReceipt {
                        unit: derived.unit.clone(),
                        previous_source_sha256: previous_source
                            .as_ref()
                            .map(|bytes| hex(&digest_of(bytes).0)),
                        installed_source_sha256: Some(intended_digest),
                        previous_policy: previous_policy.to_owned(),
                        installed_policy: intended_policy,
                        reloaded: true,
                        changes: Vec::new(),
                    }),
                ))
            } else {
                let previous_source = previous_source_bytes(&derived.unit, op)?;
                let previous_digest = previous_source
                    .as_ref()
                    .map(|bytes| hex(&digest_of(bytes).0));

                let resume = current_digest.clone() == previous_digest
                    && current.policy == previous_policy
                    || current_digest == Some(intended_digest.clone());
                if resume {
                    Ok((ServiceReconcile::NotApplied, None))
                } else {
                    Ok((ServiceReconcile::Ambiguous, None))
                }
            }
        }
    }
}

pub fn verify_remove_receipt(
    receipt: &ServiceReceipt,
    context: &mut ServiceContext<'_>,
) -> Result<(), ExecError> {
    let canonical = authorize_systemd_unit(&receipt.unit, context.systemd).map_err(|error| {
        ExecError::Refused {
            unit: receipt.unit.clone(),
            reason: error.to_string(),
        }
    })?;
    let current = observe(&receipt.unit, &canonical, context.manager)?;
    if current.source.is_some() {
        return Err(ExecError::Refused {
            unit: receipt.unit.clone(),
            reason: "the unit source came back after removal".into(),
        });
    }
    if current.policy != receipt.installed_policy {
        return Err(ExecError::Refused {
            unit: receipt.unit.clone(),
            reason: "the persistent policy is not the retired one".into(),
        });
    }
    Ok(())
}

pub fn reconcile_remove(
    unit: &str,
    _owned: &OwnedResource,
    receipt: Option<&ServiceReceipt>,
    context: &mut ServiceContext<'_>,
) -> Result<(ServiceReconcile, Option<ServiceReceipt>), ExecError> {
    let canonical =
        authorize_systemd_unit(unit, context.systemd).map_err(|error| ExecError::Refused {
            unit: unit.to_owned(),
            reason: error.to_string(),
        })?;
    let current = observe(unit, &canonical, context.manager)?;
    match receipt {
        Some(receipt) => {
            if current.source.is_none() && current.policy == receipt.installed_policy {
                Ok((ServiceReconcile::Applied, Some(receipt.clone())))
            } else {
                Ok((ServiceReconcile::Ambiguous, None))
            }
        }
        None => Ok((ServiceReconcile::NotApplied, None)),
    }
}

pub fn payload_for(node: &zup_transaction::TransactionNode) -> Result<ServicePayload, ExecError> {
    let missing = || ExecError::Refused {
        unit: node.id.to_string(),
        reason: "a service node without its payload".into(),
    };
    let backend = node.meta.backend.clone().ok_or_else(missing)?;
    let payload = crate::service_ops::decode_payload(&backend.payload).map_err(|error| {
        ExecError::Refused {
            unit: node.id.to_string(),
            reason: error.to_string(),
        }
    })?;
    let (expected_key, expected_id) = match &payload {
        ServicePayload::Apply { unit, .. } | ServicePayload::Remove { unit, .. } => (
            crate::service_ops::backend_key_for_unit(unit),
            crate::service_ops::backend_id_for_unit(unit),
        ),
    };
    if backend.key != expected_key || backend.id != expected_id {
        return Err(ExecError::Refused {
            unit: node.id.to_string(),
            reason: "a service node whose identity is not its payload".into(),
        });
    }
    Ok(payload)
}

pub fn apply_operation(
    op: &ServiceOperation,
    unit: &str,
    unit_bytes: &[u8],
    binary_owned: bool,
    previous_policy: &str,
    force: bool,
) -> Result<zup_transaction::BackendOperation, ExecError> {
    let key = crate::service_ops::backend_key_for_unit(unit);
    let id = crate::service_ops::backend_id_for_unit(unit);
    let payload = ServicePayload::Apply {
        service: op.clone(),
        unit: unit.to_owned(),
        unit_bytes: unit_bytes.to_vec(),
        binary_owned,
        previous_policy: previous_policy.to_owned(),
        force,
    };
    Ok(zup_transaction::BackendOperation {
        key,
        id,
        privilege: op.privilege,
        intent: zup_transaction::BackendOperationIntent::Apply,
        payload: encode_payload(&payload).map_err(|error| ExecError::Refused {
            unit: unit.to_owned(),
            reason: error.to_string(),
        })?,
        dependencies: Vec::new(),
    })
}

pub fn remove_operation(
    key: &ResourceKey,
    unit: &str,
    owned: &OwnedResource,
    privilege: Privilege,
) -> Result<zup_transaction::BackendOperation, ExecError> {
    let backend_key = crate::service_ops::backend_key_for_unit(unit);
    let id = crate::service_ops::backend_id_for_unit(unit);
    let payload = ServicePayload::Remove {
        key: key.clone(),
        unit: unit.to_owned(),
        owned: owned.clone(),
    };
    Ok(zup_transaction::BackendOperation {
        key: backend_key,
        id,
        privilege,
        intent: zup_transaction::BackendOperationIntent::Remove,
        payload: encode_payload(&payload).map_err(|error| ExecError::Refused {
            unit: unit.to_owned(),
            reason: error.to_string(),
        })?,
        dependencies: Vec::new(),
    })
}

#[cfg(all(test, feature = "test-support"))]
mod tests {
    use super::*;
    use crate::test_support::FakeSystemd;
    use std::path::PathBuf;

    fn operation_at(
        id: &str,
        executable: &zup_platform::TargetPath,
        start: ServiceStart,
    ) -> ServiceOperation {
        ServiceOperation {
            key: ResourceKey::Service {
                id: zup_core::ServiceId::new(id).unwrap(),
            },
            kind: zup_exec::ServiceOperationKind::Create,
            id: id.to_owned(),
            name: "Tool".to_owned(),
            display_name: "Tool".to_owned(),
            command: CommandSpec::new(executable.clone(), vec!["--serve".into()]),
            start,
            privilege: Privilege::System,
            previous: ObservedServiceState::Absent,
            conflict: None,
        }
    }

    fn isolated_binary(dir: &tempfile::TempDir) -> (zup_platform::TargetPath, PathBuf) {
        let target = zup_core::TargetTriple::parse("x86_64-unknown-linux-gnu").unwrap();
        let host = dir.path().join("opt").join("acme").join("tool");
        std::fs::create_dir_all(host.parent().unwrap()).unwrap();
        std::fs::write(&host, b"elf").unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&host, std::fs::Permissions::from_mode(0o755)).unwrap();
        let text = host.to_string_lossy().into_owned();
        (zup_platform::TargetPath::new(target, &text).unwrap(), host)
    }

    #[test]
    fn payloads_round_trip_with_identity() {
        let dir = tempfile::tempdir().unwrap();
        let (executable, _) = isolated_binary(&dir);
        let op = operation_at("tool", &executable, ServiceStart::Automatic);
        let unit = unit_name(&zup_core::ServiceId::new("tool").unwrap()).unwrap();
        let backend = apply_operation(&op, &unit, b"[Unit]\n", true, "disabled", false).unwrap();
        assert_eq!(backend.key, crate::service_ops::backend_key_for_unit(&unit));
        let payload = crate::service_ops::decode_payload(&backend.payload).unwrap();
        assert_eq!(crate::service_ops::ledger_key_for_payload(&payload), op.key);
    }

    #[test]
    fn an_enable_failure_compensates() {
        let dir = tempfile::tempdir().unwrap();
        let (executable, _) = isolated_binary(&dir);
        let roots = MachineRoots::new(
            dir.path().join("opt"),
            dir.path().join("var/lib/zup"),
            dir.path().join("var/opt"),
        );
        let systemd = SystemdRoots::new(dir.path().join("units"));
        std::fs::create_dir_all(&systemd.unit_dir).unwrap();
        let op = operation_at("tool", &executable, ServiceStart::Automatic);
        let derived = derive_operation(&op).unwrap();
        let backend =
            apply_operation(&op, &derived.unit, &derived.bytes, true, "disabled", false).unwrap();
        let node_payload = backend.payload.clone();
        let mut manager = FakeSystemd::default();
        manager.seed_fragment(
            &derived.unit,
            &systemd.unit_dir.join(&derived.unit).to_string_lossy(),
        );
        manager
            .fail_next
            .insert("enable".to_owned(), "the manager restarted".to_owned());
        let uid = rustix::process::getuid().as_raw();
        let mut context = ServiceContext {
            roots: &roots,
            systemd: &systemd,
            manager: &mut manager,
            expected_uid: uid,
        };
        assert!(apply(&op, &node_payload, &mut context).is_err());
        assert!(
            !systemd.unit_dir.join(&derived.unit).exists(),
            "the compensated source is gone"
        );
        assert_eq!(
            context.manager.unit_file_state(&derived.unit).unwrap(),
            "disabled",
            "no policy was applied"
        );
    }

    #[test]
    fn an_unmapped_unit_file_state_refuses_before_mutation() {
        let dir = tempfile::tempdir().unwrap();
        let (executable, _) = isolated_binary(&dir);
        let roots = MachineRoots::new(
            dir.path().join("opt"),
            dir.path().join("var/lib/zup"),
            dir.path().join("var/opt"),
        );
        let systemd = SystemdRoots::new(dir.path().join("units"));
        std::fs::create_dir_all(&systemd.unit_dir).unwrap();
        let op = operation_at("tool", &executable, ServiceStart::Automatic);
        let derived = derive_operation(&op).unwrap();
        let backend =
            apply_operation(&op, &derived.unit, &derived.bytes, true, "disabled", false).unwrap();
        let node_payload = backend.payload.clone();
        let mut manager = FakeSystemd::default();
        manager.seed(&derived.unit, "enabled-runtime", "/nowhere.service");
        let uid = rustix::process::getuid().as_raw();
        let mut context = ServiceContext {
            roots: &roots,
            systemd: &systemd,
            manager: &mut manager,
            expected_uid: uid,
        };
        assert!(apply(&op, &node_payload, &mut context).is_err());
        assert!(
            !systemd.unit_dir.join(&derived.unit).exists(),
            "nothing was written before the refusal"
        );
    }

    #[test]
    fn start_policies_apply_with_distinct_persistent_state() {
        for (start, expected) in [
            (ServiceStart::Automatic, "enabled"),
            (ServiceStart::Manual, "disabled"),
            (ServiceStart::Disabled, "masked"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let (executable, _) = isolated_binary(&dir);
            let roots = MachineRoots::new(
                dir.path().join("opt"),
                dir.path().join("var/lib/zup"),
                dir.path().join("var/opt"),
            );
            let systemd = SystemdRoots::new(dir.path().join("units"));
            std::fs::create_dir_all(&systemd.unit_dir).unwrap();
            let op = operation_at("tool", &executable, start);
            let derived = derive_operation(&op).unwrap();
            let backend =
                apply_operation(&op, &derived.unit, &derived.bytes, true, "disabled", false)
                    .unwrap();
            let node_payload = backend.payload.clone();
            let mut manager = FakeSystemd::default();
            manager.seed(
                &derived.unit,
                "disabled",
                &systemd.unit_dir.join(&derived.unit).to_string_lossy(),
            );
            let uid = rustix::process::getuid().as_raw();
            let mut context = ServiceContext {
                roots: &roots,
                systemd: &systemd,
                manager: &mut manager,
                expected_uid: uid,
            };
            let receipt = apply(&op, &node_payload, &mut context).expect("a service applies");
            assert_eq!(receipt.installed_policy, expected);
            assert!(receipt.reloaded);
            assert_eq!(
                context.manager.unit_file_state(&derived.unit).unwrap(),
                expected
            );
            let source = systemd.unit_dir.join(&derived.unit);
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&source).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o644, "unit sources install 0644");
            verify_receipt(&receipt, &mut context).expect("the receipt verifies");
        }
    }

    mod foreign_integration {
        use super::*;

        fn layer(base: &tempfile::TempDir, name: &str) -> PathBuf {
            let dir = base.path().join(name);
            std::fs::create_dir_all(&dir).unwrap();
            dir
        }

        fn scan(
            base: &tempfile::TempDir,
            unit: &str,
            canonical: &std::path::Path,
        ) -> Vec<std::path::PathBuf> {
            find_foreign_integration(
                unit,
                canonical,
                &base.path().join("etc"),
                &base.path().join("run"),
            )
        }

        const UNIT: &str = "zup-scan.service";
        const CANONICAL: &str = "/usr/local/lib/systemd/system/zup-scan.service";

        #[test]
        fn a_clean_tree_reports_nothing() {
            let base = tempfile::tempdir().unwrap();
            layer(&base, "etc");
            layer(&base, "run");
            let canonical = std::path::Path::new(CANONICAL);
            assert!(scan(&base, UNIT, canonical).is_empty());
        }

        #[test]
        fn the_owned_wants_link_is_excluded() {
            let base = tempfile::tempdir().unwrap();
            let etc = layer(&base, "etc");
            layer(&base, "run");
            let owned = etc.join("multi-user.target.wants");
            std::fs::create_dir_all(&owned).unwrap();
            std::os::unix::fs::symlink(CANONICAL, owned.join(UNIT)).unwrap();
            let canonical = std::path::Path::new(CANONICAL);
            assert!(scan(&base, UNIT, canonical).is_empty());
        }

        #[rstest::rstest]
        #[case::foreign_requires("etc", "some.target.requires", UNIT, CANONICAL)]
        #[case::foreign_wants("etc", "graphical.target.wants", UNIT, CANONICAL)]
        #[case::direct_alias("etc", "", "zup-scan-alias.service", CANONICAL)]
        #[case::runtime_run_link("run", "multi-user.target.wants", UNIT, CANONICAL)]
        #[case::same_name_elsewhere("etc", "", UNIT, "/usr/lib/systemd/system/zup-scan.service")]
        fn foreign_integration_is_named(
            #[case] tree: &str,
            #[case] sub: &str,
            #[case] name: &str,
            #[case] target: &str,
        ) {
            let base = tempfile::tempdir().unwrap();
            let root = layer(&base, tree);
            for other in ["etc", "run"] {
                if other != tree {
                    layer(&base, other);
                }
            }
            let dir = if sub.is_empty() { root } else { root.join(sub) };
            std::fs::create_dir_all(&dir).unwrap();
            let link = dir.join(name);
            std::os::unix::fs::symlink(target, &link).unwrap();
            let canonical = std::path::Path::new(CANONICAL);
            assert_eq!(scan(&base, UNIT, canonical), vec![link]);
        }

        #[test]
        fn a_mask_link_is_policy_not_integration() {
            let base = tempfile::tempdir().unwrap();
            let etc = layer(&base, "etc");
            layer(&base, "run");
            std::os::unix::fs::symlink("/dev/null", etc.join(UNIT)).unwrap();
            let canonical = std::path::Path::new(CANONICAL);
            assert!(scan(&base, UNIT, canonical).is_empty());
        }

        #[test]
        fn a_symlinked_wants_dir_is_never_descended() {
            let base = tempfile::tempdir().unwrap();
            let etc = layer(&base, "etc");
            layer(&base, "run");
            let elsewhere = base.path().join("elsewhere");
            std::fs::create_dir_all(&elsewhere).unwrap();
            std::fs::write(elsewhere.join(UNIT), b"[Unit]\n").unwrap();
            std::os::unix::fs::symlink(&elsewhere, etc.join("multi-user.target.wants")).unwrap();
            let canonical = std::path::Path::new(CANONICAL);
            assert!(
                scan(&base, UNIT, canonical).is_empty(),
                "a redirected wants directory is not traversed"
            );
        }
    }
}
