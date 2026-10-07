//! Applying one typed service operation: filesystem truth plus manager state.
//!
//! Every entry point revalidates ownership immediately before mutating:
//! the unit name is re-derived from the service identity (never trusted
//! from the payload alone), the rendered bytes are re-derived from the
//! operation (the journal cannot smuggle a foreign unit past a worker that
//! renders its own), collisions and administrator overrides are refused,
//! and the executable is revalidated against the live filesystem. D-Bus
//! force flags stay off: a collision remains visible.
//!
//! Ordering inside one apply is write → reload → policy; rollback restores
//! the authoritative source first, reloads, then restores the previous
//! policy, so systemd never observes a policy for a source that is absent.
//!
//! The worker never starts, stops, or restarts services: installing an
//! `Automatic` service registers boot policy, it does not execute
//! application code.

use zup_core::{Privilege, ResourceKey, ServiceStart};
use zup_exec::{ObservedServiceState, OwnedResource, ServiceOperation};
use zup_platform::{CommandSpec, TargetService};

use crate::fs::{EntryKind, OwnedDirectory};
use crate::machine::{MachineRoots, SystemdRoots, authorize_systemd_unit};
use crate::service_ops::{
    ServiceError, ServicePayload, ServiceReceipt, check_collisions, desired_policy, encode_payload,
    load_path_dirs, policy_for_state, refuse_source_symlink, validate_changes,
    validate_executable_live,
};
use crate::services::{render_unit, unit_name};
use crate::systemd::{SystemdManager, UnitChange};

/// What the executor needs beyond the journal node: roots, a manager, and
/// the uid that must own trusted executables (0 in production).
pub struct ServiceContext<'a> {
    pub roots: &'a MachineRoots,
    pub systemd: &'a SystemdRoots,
    pub manager: &'a mut dyn SystemdManager,
    pub expected_uid: u32,
}

/// Rendered identity for one operation, re-derived - never trusted.
#[derive(Debug, Clone)]
struct Derived {
    unit: String,
    bytes: Vec<u8>,
    command: CommandSpec,
    start: ServiceStart,
}

fn derive_operation(op: &ServiceOperation) -> Result<Derived, ServiceError> {
    let id = zup_core::ServiceId::new(&op.id).map_err(|error| ServiceError::Refused {
        unit: op.name.clone(),
        reason: format!("service id: {error}"),
    })?;
    let unit = unit_name(&id).map_err(|error| ServiceError::Refused {
        unit: op.name.clone(),
        reason: error.to_string(),
    })?;
    let name = zup_core::NonEmptyString::new(&op.name).map_err(|error| ServiceError::Refused {
        unit: unit.clone(),
        reason: format!("service name: {error}"),
    })?;
    let display =
        zup_core::NonEmptyString::new(&op.display_name).map_err(|error| ServiceError::Refused {
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
    let bytes =
        render_unit(&synthetic, &op.display_name).map_err(|error| ServiceError::Refused {
            unit: unit.clone(),
            reason: error.to_string(),
        })?;
    if bytes.len() > crate::service_ops::MAX_UNIT_BYTES {
        return Err(ServiceError::Refused {
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

/// Render the bytes one ledger registration describes.
fn render_registration(
    unit: &str,
    display: &str,
    command: &CommandSpec,
    start: ServiceStart,
    key: &ResourceKey,
    id: &zup_core::ServiceId,
    name: &zup_core::NonEmptyString,
) -> Result<Vec<u8>, ServiceError> {
    let display =
        zup_core::NonEmptyString::new(display).map_err(|error| ServiceError::Refused {
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
    .map_err(|error| ServiceError::Refused {
        unit: unit.to_owned(),
        reason: error.to_string(),
    })
}

/// Live truth for one unit: source digest plus persistent policy.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Observed {
    source: Option<([u8; 32], u64)>,
    policy: String,
}

fn observe(
    unit: &str,
    canonical: &std::path::Path,
    manager: &mut dyn SystemdManager,
) -> Result<Observed, ServiceError> {
    let source = read_source(canonical)?;
    let policy = manager
        .unit_file_state(unit)
        .unwrap_or_else(|_| "unknown".to_owned());
    Ok(Observed { source, policy })
}

fn read_source(canonical: &std::path::Path) -> Result<Option<([u8; 32], u64)>, ServiceError> {
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
                let (size, digest) = zup_core::hash_reader(bytes.as_slice()).map_err(|_| {
                    ServiceError::Ambiguous {
                        unit: name.clone(),
                        reason: "a unit source does not hash".into(),
                    }
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

/// The source bytes one observed state describes, if any.
fn previous_source_bytes(
    unit: &str,
    op: &ServiceOperation,
) -> Result<Option<Vec<u8>>, ServiceError> {
    match &op.previous {
        ObservedServiceState::Absent => Ok(None),
        ObservedServiceState::Service {
            display_name,
            command,
            start,
            ..
        } => {
            let name =
                zup_core::NonEmptyString::new(&op.name).map_err(|error| ServiceError::Refused {
                    unit: unit.to_owned(),
                    reason: format!("service name: {error}"),
                })?;
            let id = zup_core::ServiceId::new(&op.id).map_err(|error| ServiceError::Refused {
                unit: unit.to_owned(),
                reason: format!("service id: {error}"),
            })?;
            render_registration(unit, display_name, command, *start, &op.key, &id, &name).map(Some)
        }
    }
}

/// Prove the plan still describes the world before mutating it.
///
/// A resume is not a substitution: when the source already holds the
/// intended bytes with the policy still pending (a previous attempt that
/// crashed between write and policy), the remaining steps replay
/// idempotently instead of refusing.
fn check_unchanged_resume(
    derived: &Derived,
    before: &Observed,
    op: &ServiceOperation,
    previous_policy: &str,
    force: bool,
) -> Result<(), ServiceError> {
    if before.source.map(|(d, _)| hex(&d)) == Some(hex(&digest_of(&derived.bytes).0)) {
        return Ok(());
    }
    // Explicit force repair overwrites owned-but-damaged content. Every
    // other ownership proof (collisions, overrides, symlinks, binary
    // trust) already ran above and still refuses with force.
    if force {
        return Ok(());
    }
    check_unchanged(&derived.unit, before, op, previous_policy)
}

/// Prove the plan still describes the world before mutating it.
fn check_unchanged(
    unit: &str,
    before: &Observed,
    op: &ServiceOperation,
    previous_policy: &str,
) -> Result<(), ServiceError> {
    let wanted_source = previous_source_bytes(unit, op)?;
    let wanted_digest = wanted_source.as_ref().map(|bytes| hex(&digest_of(bytes).0));
    if before.source.map(|(d, _)| hex(&d)) != wanted_digest || before.policy != previous_policy {
        // A fresh install expects absence; anything present is a collision
        // the preflight names more precisely, so report plainly here.
        return Err(ServiceError::Refused {
            unit: unit.to_owned(),
            reason: "the service changed since planning".into(),
        });
    }
    Ok(())
}

/// Prepare-time ownership proof: no mutation, only refusal.
fn prepare(
    op: &ServiceOperation,
    payload_bytes: &[u8],
) -> Result<(Derived, ServicePayload), ServiceError> {
    let derived = derive_operation(op)?;
    let payload: ServicePayload =
        crate::service_ops::decode_payload(payload_bytes).map_err(|error| {
            ServiceError::Refused {
                unit: derived.unit.clone(),
                reason: error.to_string(),
            }
        })?;
    let ServicePayload::Apply {
        service,
        unit,
        unit_bytes,
        binary_owned,
        ..
    } = &payload
    else {
        return Err(ServiceError::Refused {
            unit: derived.unit.clone(),
            reason: "a service apply node holds apply intent".into(),
        });
    };
    if *unit != derived.unit || *unit_bytes != derived.bytes || *service != *op || !binary_owned {
        return Err(ServiceError::Refused {
            unit: derived.unit.clone(),
            reason: "a service payload does not match its operation".into(),
        });
    }
    Ok((derived, payload))
}

/// Apply one service: write source, reload, reconcile policy, verify.
pub fn apply(
    op: &ServiceOperation,
    payload_bytes: &[u8],
    context: &mut ServiceContext<'_>,
) -> Result<ServiceReceipt, ServiceError> {
    let (derived, payload) = prepare(op, payload_bytes)?;
    let ServicePayload::Apply {
        previous_policy: planned_policy,
        force: planned_force,
        ..
    } = &payload
    else {
        return Err(ServiceError::Refused {
            unit: derived.unit.clone(),
            reason: "a service apply node holds apply intent".into(),
        });
    };
    let planned_policy = planned_policy.clone();
    let planned_force = *planned_force;
    let canonical = authorize_systemd_unit(&derived.unit, context.systemd).map_err(|error| {
        ServiceError::Refused {
            unit: derived.unit.clone(),
            reason: error.to_string(),
        }
    })?;
    refuse_source_symlink(&derived.unit, &canonical)?;
    check_collisions(&derived.unit, &canonical, &load_path_dirs())?;
    // The payload correspondence rode the journal (proven in `prepare`
    // from the payload flag); the live ownership bits are re-proven here
    // against the world as it is now.
    validate_executable_live(&derived.command, context.roots, context.expected_uid)?;
    // systemd must answer before anything mutates: writing a unit no
    // manager will ever load is exactly the half-installation the
    // capability preflight exists to prevent.
    let probe = context
        .manager
        .unit_file_state(&derived.unit)
        .map_err(ServiceError::from)?;
    if policy_for_state(&probe).is_none() {
        return Err(ServiceError::Refused {
            unit: derived.unit.clone(),
            reason: format!("systemd reports unit-file state `{probe}`, which Zup does not own"),
        });
    }
    let before = observe(&derived.unit, &canonical, context.manager)?;
    // The previous half is what planning journaled, not a guess: source
    // absence never implies a policy, because policy persists on its own.
    let previous_source = before.source.map(|(digest, _)| hex(&digest));
    // Idempotent resume: a previous attempt may have completed the
    // mutation while losing the reply. When the world already holds the
    // intended source and policy, report it rather than refusing on the
    // change-since-planning check below.
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
    // The previous bytes for compensation: a policy failure below must
    // not leave a half-written unit behind to poison the next plan.
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

/// Reload, reconcile policy, validate returned changes, and verify the
/// installed unit: the fallible tail of [`apply`] after the source write.
fn apply_reload_policy_verify(
    derived: &Derived,
    canonical: &std::path::Path,
    context: &mut ServiceContext<'_>,
) -> Result<Vec<UnitChange>, ServiceError> {
    context.manager.reload().map_err(ServiceError::from)?;
    let mut changes = Vec::new();
    apply_policy(&derived.unit, derived.start, context.manager, &mut changes)?;
    for change in &changes {
        validate_changes(&derived.unit, std::slice::from_ref(change))?;
    }
    verify_installed(derived, canonical, context.manager)?;
    Ok(changes)
}

/// Restore the previous source after a failed tail: remove what was
/// absent, rewrite what was there, reload best-effort. A failure here is
/// reported through the original error's path (reconciliation classifies
/// whatever remains); this best-effort pass only narrows the window.
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

/// Write (or replace) the canonical unit source: `0644`, no-follow,
/// exclusive when the destination must be absent.
fn write_source(
    unit: &str,
    canonical: &std::path::Path,
    bytes: &[u8],
    must_be_absent: bool,
) -> Result<(), ServiceError> {
    let Some(parent) = canonical.parent().filter(|p| !p.as_os_str().is_empty()) else {
        return Err(ServiceError::refused(
            unit,
            "a unit source has a parent directory",
        ));
    };
    refuse_source_symlink(unit, canonical)?;
    if OwnedDirectory::open(parent).is_err() {
        std::fs::create_dir_all(parent).map_err(|error| ServiceError::Refused {
            unit: unit.to_owned(),
            reason: format!("the unit directory cannot be created: {error}"),
        })?;
    }
    let directory = OwnedDirectory::open(parent).map_err(|error| ServiceError::Refused {
        unit: unit.to_owned(),
        reason: format!("the unit directory cannot be opened: {error}"),
    })?;
    let name = canonical
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty())
        .ok_or_else(|| ServiceError::refused(unit, "a unit source has a file name"))?;
    use rustix::fs::Mode;
    if must_be_absent {
        directory
            .create_durable_exclusive(&name, bytes, Mode::from_bits_truncate(0o644))
            .map_err(|error| ServiceError::Refused {
                unit: unit.to_owned(),
                reason: format!("the unit source cannot be created: {error}"),
            })?;
    } else {
        directory
            .write_durable(&name, bytes, Mode::from_bits_truncate(0o644))
            .map_err(|error| ServiceError::Refused {
                unit: unit.to_owned(),
                reason: format!("the unit source cannot be written: {error}"),
            })?;
    }
    crate::fs::sync_directory(parent).map_err(|error| ServiceError::Refused {
        unit: unit.to_owned(),
        reason: format!("the unit directory does not flush: {error}"),
    })?;
    Ok(())
}

/// Reconcile persistent policy without ever starting or stopping anything.
///
/// Mask transitions unmask first where required; a broad disable that would
/// delete unrelated administrator enablement refuses (fail closed) rather
/// than silently removing state Zup does not own. The owned link is the
/// single `multi-user.target.wants` symlink this renderer's `[Install]`
/// section creates; anything else wanting the unit is preserved by
/// refusing.
fn apply_policy(
    unit: &str,
    start: ServiceStart,
    manager: &mut dyn SystemdManager,
    changes: &mut Vec<UnitChange>,
) -> Result<(), ServiceError> {
    match start {
        ServiceStart::Automatic => {
            let state = manager.unit_file_state(unit).map_err(ServiceError::from)?;
            if state == "masked" || state == "masked-runtime" {
                changes.extend(manager.unmask(unit).map_err(ServiceError::from)?);
            }
            changes.extend(manager.enable(unit).map_err(ServiceError::from)?);
        }
        ServiceStart::Manual => {
            let state = manager.unit_file_state(unit).map_err(ServiceError::from)?;
            if state == "masked" || state == "masked-runtime" {
                changes.extend(manager.unmask(unit).map_err(ServiceError::from)?);
            }
            if state == "enabled" || state == "enabled-runtime" {
                refuse_unrelated_enablement(unit)?;
                changes.extend(manager.disable(unit).map_err(ServiceError::from)?);
            }
        }
        ServiceStart::Disabled => {
            let state = manager.unit_file_state(unit).map_err(ServiceError::from)?;
            if state == "enabled" || state == "enabled-runtime" {
                refuse_unrelated_enablement(unit)?;
            }
            if state == "enabled" || state == "enabled-runtime" {
                changes.extend(manager.disable(unit).map_err(ServiceError::from)?);
            }
            changes.extend(manager.mask(unit).map_err(ServiceError::from)?);
        }
    }
    Ok(())
}

/// Refuse when administrator-added enablement exists beyond the single
/// link Zup owns: a broad disable would delete it, so the transition
/// fails closed instead.
fn refuse_unrelated_enablement(unit: &str) -> Result<(), ServiceError> {
    let extras = extra_enablement_links(unit);
    if extras.is_empty() {
        return Ok(());
    }
    Err(ServiceError::Ambiguous {
        unit: unit.to_owned(),
        reason: format!(
            "refusing broad disable: unrelated enablement exists: {}",
            extras
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    })
}

/// Administrator-owned enablement links for one unit: every
/// `/etc/systemd/system/*.wants/<unit>` symlink except the single
/// `multi-user.target.wants` link this backend owns.
fn extra_enablement_links(unit: &str) -> Vec<std::path::PathBuf> {
    let mut extras = Vec::new();
    let Ok(layer) = std::fs::read_dir("/etc/systemd/system") else {
        return extras;
    };
    for entry in layer.flatten() {
        let path = entry.path();
        // Only `.wants` directories hold enablement links.
        if path.extension().and_then(|extension| extension.to_str()) != Some("wants") {
            continue;
        }
        // The one this backend owns is never extra.
        if path.file_name().and_then(|name| name.to_str()) == Some("multi-user.target.wants") {
            continue;
        }
        let candidate = path.join(unit);
        if std::fs::symlink_metadata(&candidate)
            .is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            extras.push(candidate);
        }
    }
    extras
}

/// Verify systemd resolves the unit as Zup installed it: the fragment is
/// the canonical source (a mask resolves to `/dev/null` by design), and
/// the persistent state matches the desired start policy.
fn verify_installed(
    derived: &Derived,
    canonical: &std::path::Path,
    manager: &mut dyn SystemdManager,
) -> Result<(), ServiceError> {
    let info = manager
        .load_unit(&derived.unit)
        .map_err(ServiceError::from)?;
    let wanted = desired_policy(derived.start);
    if info.unit_file_state != wanted {
        return Err(ServiceError::Refused {
            unit: derived.unit.clone(),
            reason: format!(
                "systemd reports unit-file state `{}`, want `{wanted}`",
                info.unit_file_state
            ),
        });
    }
    if derived.start == ServiceStart::Disabled {
        if info.load_state != "masked" && info.fragment_path != "/dev/null" {
            return Err(ServiceError::Refused {
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
        return Err(ServiceError::Refused {
            unit: derived.unit.clone(),
            reason: format!(
                "systemd loads `{}` instead of the Zup source",
                info.fragment_path
            ),
        });
    }
    if info.load_state != "loaded" {
        return Err(ServiceError::Refused {
            unit: derived.unit.clone(),
            reason: format!("the unit loads as `{}`, not `loaded`", info.load_state),
        });
    }
    Ok(())
}

/// Roll back one apply: the installed state must still hold (else something
/// changed after installation), then the previous source returns, systemd
/// reloads, and the previous policy returns.
pub fn rollback_apply(
    payload: &ServicePayload,
    receipt: &ServiceReceipt,
    context: &mut ServiceContext<'_>,
) -> Result<(), ServiceError> {
    let ServicePayload::Apply {
        service: op,
        unit: payload_unit,
        ..
    } = payload
    else {
        return Err(ServiceError::Refused {
            unit: receipt.unit.clone(),
            reason: "a service apply rollback holds apply intent".into(),
        });
    };
    let derived = derive_operation(op)?;
    if receipt.unit != derived.unit || *payload_unit != derived.unit {
        return Err(ServiceError::refused(
            &derived.unit,
            "a receipt for another unit",
        ));
    }
    let canonical = authorize_systemd_unit(&derived.unit, context.systemd).map_err(|error| {
        ServiceError::Refused {
            unit: derived.unit.clone(),
            reason: error.to_string(),
        }
    })?;
    let current = observe(&derived.unit, &canonical, context.manager)?;
    if current.source.map(|(d, _)| hex(&d)) != receipt.installed_source_sha256
        || current.policy != receipt.installed_policy
    {
        return Err(ServiceError::Refused {
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
                zup_core::NonEmptyString::new(&op.name).map_err(|error| ServiceError::Refused {
                    unit: derived.unit.clone(),
                    reason: format!("service name: {error}"),
                })?;
            let id = zup_core::ServiceId::new(&op.id).map_err(|error| ServiceError::Refused {
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
    context.manager.reload().map_err(ServiceError::from)?;
    restore_policy(&derived.unit, &receipt.previous_policy, context.manager)?;
    Ok(())
}

/// Restore a previous persistent policy by name.
fn restore_policy(
    unit: &str,
    previous: &str,
    manager: &mut dyn SystemdManager,
) -> Result<(), ServiceError> {
    match previous {
        "enabled" => {
            manager.unmask(unit).map_err(ServiceError::from)?;
            manager.enable(unit).map_err(ServiceError::from)?;
        }
        "disabled" => {
            manager.unmask(unit).map_err(ServiceError::from)?;
            let _ = manager.disable(unit).map_err(ServiceError::from)?;
        }
        "masked" => {
            manager.mask(unit).map_err(ServiceError::from)?;
        }
        _ => {
            manager.unmask(unit).map_err(ServiceError::from)?;
        }
    }
    Ok(())
}

fn remove_file_no_follow(unit: &str, canonical: &std::path::Path) -> Result<(), ServiceError> {
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
                .map_err(|error| ServiceError::Refused {
                    unit: unit.to_owned(),
                    reason: format!("the unit source cannot be removed: {error}"),
                })?;
            directory.sync().map_err(|error| ServiceError::Refused {
                unit: unit.to_owned(),
                reason: format!("the unit directory does not flush: {error}"),
            })?;
            Ok(())
        }
        Ok(Some(_)) => Err(ServiceError::conflict(
            unit,
            "the unit source path is no longer a regular file",
        )),
        Err(_) => Ok(()),
    }
}

/// Remove one service: owned enablement/mask retire, the canonical source
/// goes, systemd reloads. Administrator drop-ins, full `/etc` overrides,
/// and unrelated unit files are never touched; a full override refuses the
/// removal rather than being deleted.
pub fn apply_remove(
    key: &ResourceKey,
    unit: &str,
    owned: &OwnedResource,
    context: &mut ServiceContext<'_>,
) -> Result<ServiceReceipt, ServiceError> {
    let OwnedResource::Service {
        name, installed, ..
    } = owned
    else {
        return Err(ServiceError::refused(
            unit,
            "a service removal names a service",
        ));
    };
    let ResourceKey::Service { id } = key else {
        return Err(ServiceError::refused(
            unit,
            "a service removal names a service key",
        ));
    };
    let expected = unit_name(id).map_err(|error| ServiceError::Refused {
        unit: unit.to_owned(),
        reason: error.to_string(),
    })?;
    if expected != unit {
        return Err(ServiceError::refused(unit, "a removal for another unit"));
    }
    let canonical =
        authorize_systemd_unit(unit, context.systemd).map_err(|error| ServiceError::Refused {
            unit: unit.to_owned(),
            reason: error.to_string(),
        })?;
    refuse_source_symlink(unit, &canonical)?;
    // A full administrator override shadows the source: removing Zup's
    // source under it would leave the admin file dangling while claiming
    // retirement. Refuse and report instead.
    crate::service_ops::check_no_full_override(unit, &crate::service_ops::admin_override_dir())
        .map_err(|error| ServiceError::Refused {
            unit: unit.to_owned(),
            reason: error.to_string(),
        })?;
    let zup_exec::ServiceState::Registration {
        display_name,
        command,
        start,
    } = installed
    else {
        return Err(ServiceError::refused(
            unit,
            "owned service state is a registration",
        ));
    };
    let service_name =
        zup_core::NonEmptyString::new(name).map_err(|error| ServiceError::Refused {
            unit: unit.to_owned(),
            reason: format!("service name: {error}"),
        })?;
    let installed_bytes =
        render_registration(unit, display_name, command, *start, key, id, &service_name)?;
    // Idempotent resume: an earlier attempt may have retired everything
    // while losing the reply.
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
    // Retire owned policy first, then the source, then reload: systemd
    // never observes a policy for a source that is already gone.
    let mut changes = Vec::new();
    let state = context
        .manager
        .unit_file_state(unit)
        .map_err(ServiceError::from)?;
    if state == "masked" || state == "masked-runtime" {
        changes.extend(context.manager.unmask(unit).map_err(ServiceError::from)?);
    }
    if state == "enabled" || state == "enabled-runtime" {
        refuse_unrelated_enablement(unit)?;
        changes.extend(context.manager.disable(unit).map_err(ServiceError::from)?);
    }
    remove_file_no_follow(unit, &canonical)?;
    context.manager.reload().map_err(ServiceError::from)?;
    let after = observe(unit, &canonical, context.manager)?;
    if after.source.is_some() {
        return Err(ServiceError::Refused {
            unit: unit.to_owned(),
            reason: "the unit source is still there".into(),
        });
    }
    if after.policy == "enabled" || after.policy == "masked" {
        return Err(ServiceError::Refused {
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

/// Roll back one removal: the source returns, systemd reloads, the previous
/// policy returns.
pub fn rollback_remove(
    key: &ResourceKey,
    unit: &str,
    owned: &OwnedResource,
    receipt: &ServiceReceipt,
    context: &mut ServiceContext<'_>,
) -> Result<(), ServiceError> {
    let OwnedResource::Service {
        name, installed, ..
    } = owned
    else {
        return Err(ServiceError::refused(
            unit,
            "a service removal names a service",
        ));
    };
    let ResourceKey::Service { id } = key else {
        return Err(ServiceError::refused(
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
        return Err(ServiceError::refused(
            unit,
            "owned service state is a registration",
        ));
    };
    let canonical =
        authorize_systemd_unit(unit, context.systemd).map_err(|error| ServiceError::Refused {
            unit: unit.to_owned(),
            reason: error.to_string(),
        })?;
    let current = observe(unit, &canonical, context.manager)?;
    if current.source.is_some() {
        return Err(ServiceError::Refused {
            unit: unit.to_owned(),
            reason: "the service changed after removal".into(),
        });
    }
    let service_name =
        zup_core::NonEmptyString::new(name).map_err(|error| ServiceError::Refused {
            unit: unit.to_owned(),
            reason: format!("service name: {error}"),
        })?;
    let bytes = render_registration(unit, display_name, command, *start, key, id, &service_name)?;
    write_source(unit, &canonical, &bytes, true)?;
    context.manager.reload().map_err(ServiceError::from)?;
    restore_policy(unit, &receipt.previous_policy, context.manager)?;
    Ok(())
}

/// Confirm an applied node still holds its receipt before commit.
pub fn verify_receipt(
    receipt: &ServiceReceipt,
    context: &mut ServiceContext<'_>,
) -> Result<(), ServiceError> {
    let canonical = authorize_systemd_unit(&receipt.unit, context.systemd).map_err(|error| {
        ServiceError::Refused {
            unit: receipt.unit.clone(),
            reason: error.to_string(),
        }
    })?;
    let current = observe(&receipt.unit, &canonical, context.manager)?;
    if current.source.map(|(d, _)| hex(&d)) != receipt.installed_source_sha256 {
        return Err(ServiceError::Refused {
            unit: receipt.unit.clone(),
            reason: "the unit source is not the installed one".into(),
        });
    }
    if current.policy != receipt.installed_policy {
        return Err(ServiceError::Refused {
            unit: receipt.unit.clone(),
            reason: "the persistent policy is not the installed one".into(),
        });
    }
    Ok(())
}

/// Classify a crashed node: installed, not applied, or ambiguous.
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
) -> Result<(ServiceReconcile, Option<ServiceReceipt>), ServiceError> {
    let derived = derive_operation(op)?;
    let canonical = authorize_systemd_unit(&derived.unit, context.systemd).map_err(|error| {
        ServiceError::Refused {
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
                    // The source holds the intended bytes while the policy
                    // is still pending: resume rather than recover.
                    Ok((ServiceReconcile::NotApplied, None))
                } else {
                    Ok((ServiceReconcile::Ambiguous, None))
                }
            }
        }
        None => {
            if installed {
                // Interrupted after the mutation completed: rebuild the
                // receipt from the observed world. The previous half is
                // what the operation recorded, not a guess.
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
                // Not applied covers both the untouched previous world
                // and a source that already holds the intended bytes with
                // the policy still pending (resume rather than recover).
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

/// Confirm a removal still holds before commit: the source stays absent
/// and no persistent policy crept back.
pub fn verify_remove_receipt(
    receipt: &ServiceReceipt,
    context: &mut ServiceContext<'_>,
) -> Result<(), ServiceError> {
    let canonical = authorize_systemd_unit(&receipt.unit, context.systemd).map_err(|error| {
        ServiceError::Refused {
            unit: receipt.unit.clone(),
            reason: error.to_string(),
        }
    })?;
    let current = observe(&receipt.unit, &canonical, context.manager)?;
    if current.source.is_some() {
        return Err(ServiceError::Refused {
            unit: receipt.unit.clone(),
            reason: "the unit source came back after removal".into(),
        });
    }
    if current.policy != receipt.installed_policy {
        return Err(ServiceError::Refused {
            unit: receipt.unit.clone(),
            reason: "the persistent policy is not the retired one".into(),
        });
    }
    Ok(())
}

/// Classify a crashed removal: retired, not removed, or ambiguous.
pub fn reconcile_remove(
    unit: &str,
    _owned: &OwnedResource,
    receipt: Option<&ServiceReceipt>,
    context: &mut ServiceContext<'_>,
) -> Result<(ServiceReconcile, Option<ServiceReceipt>), ServiceError> {
    let canonical =
        authorize_systemd_unit(unit, context.systemd).map_err(|error| ServiceError::Refused {
            unit: unit.to_owned(),
            reason: error.to_string(),
        })?;
    let current = observe(unit, &canonical, context.manager)?;
    match receipt {
        Some(receipt) => {
            if current.source.is_none() && current.policy == receipt.installed_policy {
                Ok((ServiceReconcile::Applied, Some(receipt.clone())))
            } else {
                // Present again, or retired to another policy: the removal
                // did not establish what the receipt claims.
                Ok((ServiceReconcile::Ambiguous, None))
            }
        }
        None => {
            // Removal is idempotent either way: present re-removes, absent
            // retires again cleanly, so an interrupted removal replays
            // rather than recovers.
            Ok((ServiceReconcile::NotApplied, None))
        }
    }
}

/// Decode one backend node's payload or fail closed.
pub fn payload_for(
    node: &zup_transaction::TransactionNode,
) -> Result<ServicePayload, ServiceError> {
    let missing = || ServiceError::Refused {
        unit: node.id.to_string(),
        reason: "a service node without its payload".into(),
    };
    let backend = node.meta.backend.clone().ok_or_else(missing)?;
    let payload = crate::service_ops::decode_payload(&backend.payload).map_err(|error| {
        ServiceError::Refused {
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
        return Err(ServiceError::Refused {
            unit: node.id.to_string(),
            reason: "a service node whose identity is not its payload".into(),
        });
    }
    Ok(payload)
}

/// Build the apply backend operation for one executable service delta.
pub fn apply_operation(
    op: &ServiceOperation,
    unit: &str,
    unit_bytes: &[u8],
    binary_owned: bool,
    previous_policy: &str,
    force: bool,
) -> Result<zup_transaction::BackendOperation, ServiceError> {
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
        payload: encode_payload(&payload).map_err(|error| ServiceError::Refused {
            unit: unit.to_owned(),
            reason: error.to_string(),
        })?,
        dependencies: Vec::new(),
    })
}

/// Build the removal backend operation for one owned service.
pub fn remove_operation(
    key: &ResourceKey,
    unit: &str,
    owned: &OwnedResource,
    privilege: Privilege,
) -> Result<zup_transaction::BackendOperation, ServiceError> {
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
        payload: encode_payload(&payload).map_err(|error| ServiceError::Refused {
            unit: unit.to_owned(),
            reason: error.to_string(),
        })?,
        dependencies: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
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

    /// A D-Bus enable failure compensates its own source write: the
    /// failed install leaves neither a half-written unit nor a policy
    /// behind, so the next plan sees the previous world, not drift.
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
        let mut manager = crate::systemd::FakeSystemd::default();
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

    /// An unmapped unit-file state refuses before mutation: runtime-only
    /// and foreign states are never accepted as persistent policy.
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
        let mut manager = crate::systemd::FakeSystemd::default();
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

    /// Every start policy applies through the fake manager: source bytes
    /// update, enablement changes, masks change, and no StartUnit ever
    /// runs (the fake has none to call).
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
            let mut manager = crate::systemd::FakeSystemd::default();
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
}
