use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use zup_core::ServiceStart;
use zup_plan::{ObservedService, ObservedServiceState, OwnedResource, ServiceOperation};

use crate::error::ExecError;
use crate::fs::{EntryKind, OwnedDirectory};
use crate::locations::MachineRoots;
use crate::machine::{SystemdRoots, authorize_systemd_unit};
use crate::services::parse_unit;
use crate::systemd::{SystemdManager, UnitChange};

pub const MAX_UNIT_BYTES: usize = 64 * 1024;

pub fn plan_requires_manager(plan: &zup_plan::DeploymentPlan) -> bool {
    plan.nodes.iter().any(|node| {
        matches!(
            node.kind,
            zup_plan::NodeKind::ServiceControl { .. } | zup_plan::NodeKind::NativeService { .. }
        )
    })
}

pub fn bind_startup_inhibition(input: &mut zup_plan::DeploymentEffects) {
    for operation in &mut input.native_services {
        let guarded = input.service_controls.iter().any(|control| {
            control.key == *operation.key() && control.before_configuration != control.configuration
        });
        if guarded {
            match operation {
                zup_plan::NativeServiceOperation::Configure { activation, .. }
                | zup_plan::NativeServiceOperation::Remove { activation, .. } => {
                    *activation = zup_plan::ServiceActivation::Inhibited
                }
            }
        }
    }
}

pub fn desired_policy(start: ServiceStart) -> &'static str {
    match start {
        ServiceStart::Automatic => "enabled",
        ServiceStart::Manual => "disabled",
        ServiceStart::Disabled => "masked",
    }
}

pub fn policy_for_state(state: &str) -> Option<ServiceStart> {
    match state {
        "enabled" => Some(ServiceStart::Automatic),
        "disabled" => Some(ServiceStart::Manual),
        "masked" => Some(ServiceStart::Disabled),
        _ => None,
    }
}

pub fn require_exec_baseline(manager: &mut dyn SystemdManager) -> Result<u32, ExecError> {
    let raw = manager.version().map_err(ExecError::from)?;
    match crate::services::parse_manager_version(&raw) {
        Some(major) if major >= crate::services::MINIMUM_SYSTEMD_VERSION => Ok(major),
        Some(major) => Err(ExecError::refused(
            "",
            format!(
                "systemd {major} is older than the minimum {} for managed service units",
                crate::services::MINIMUM_SYSTEMD_VERSION,
            ),
        )),
        None => Err(ExecError::refused(
            "",
            format!("systemd reports an unparsable version `{raw}`"),
        )),
    }
}

pub fn snapshot_services(
    target: &zup_plan::BoundDeployment,
    manager: &mut dyn SystemdManager,
    systemd: &SystemdRoots,
) -> Result<Vec<ObservedService>, ExecError> {
    let mut out = Vec::with_capacity(target.services.len());
    for service in &target.services {
        let unit = service.name.to_string();
        let path = authorize_systemd_unit(&unit, systemd)
            .map_err(|error| ExecError::refused(&unit, error.to_string()))?;
        let state = observe_unit(&unit, &path, target.target, manager)?;
        out.push(ObservedService {
            registration: if state == ObservedServiceState::Absent {
                zup_core::ObservedServiceRegistration::Unclaimed
            } else {
                zup_core::ObservedServiceRegistration::SourceFile
            },
            key: service.key.clone(),
            id: service.id.clone(),
            state,
        });
    }
    Ok(out)
}

pub(crate) fn observe_unit(
    unit: &str,
    path: &Path,
    target: zup_core::Target,
    manager: &mut dyn SystemdManager,
) -> Result<ObservedServiceState, ExecError> {
    let bytes = read_regular_no_follow(path)?;
    let Some(bytes) = bytes else {
        return Ok(ObservedServiceState::Absent);
    };

    let Ok(crate::services::UnitDefinition {
        state_directories,
        native_timeouts,
        failure,
        dependencies,
        display_name,
        argv,
        readiness,
        execution,
    }) = parse_unit(&bytes, target, unit)
    else {
        return Ok(ObservedServiceState::InvalidService);
    };
    if argv.is_empty() {
        return Ok(ObservedServiceState::InvalidService);
    }
    let state_text = manager
        .unit_file_state(unit)
        .map_err(|error| ExecError::Ambiguous {
            unit: unit.to_owned(),
            reason: format!("unit-file state is unknown: {error}"),
        })?;
    let Some(start) = policy_for_state(&state_text) else {
        return Ok(ObservedServiceState::InvalidService);
    };
    let executable = argv[0].clone();
    let target = zup_core::TargetPath::new(target, &executable);
    let command = match target {
        Ok(executable) => zup_core::CommandSpec::new(executable, argv[1..].to_vec()),
        Err(_) => return Ok(ObservedServiceState::InvalidService),
    };
    let runtime = manager.runtime(unit).map_err(ExecError::from)?;
    let loaded = manager.load_unit(unit).map_err(ExecError::from)?;
    if runtime.argv.as_ref().is_some_and(|loaded| *loaded != argv)
        || (runtime.argv.is_none() && runtime.state != zup_plan::ServiceRuntimeState::Stopped)
        || loaded.need_daemon_reload
        || crate::systemd_policy::validate(&loaded.drop_in_paths).is_err()
        || if start == ServiceStart::Disabled {
            state_text != "masked"
                || (runtime.state == zup_plan::ServiceRuntimeState::Stopped
                    && loaded.load_state != "masked"
                    && loaded.fragment_path != "/dev/null")
        } else {
            loaded.load_state != "loaded"
                || Path::new(&loaded.fragment_path) != path
                || runtime.argv.as_ref() != Some(&argv)
                || runtime.execution.as_ref() != Some(&execution)
                || runtime.state_directories
                    != state_directories
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                || runtime.failure != failure
                || runtime.native_timeouts != Some(native_timeouts)
                || runtime.service_type
                    != match readiness {
                        zup_core::ServiceReadiness::Manager => "exec",
                        zup_core::ServiceReadiness::Notify => "notify",
                    }
                || runtime
                    .dependencies
                    .iter()
                    .collect::<std::collections::BTreeSet<_>>()
                    != dependencies
                        .iter()
                        .collect::<std::collections::BTreeSet<_>>()
        }
    {
        return Ok(ObservedServiceState::InvalidService);
    }
    Ok(ObservedServiceState::Service {
        state_directories,
        native_timeouts,
        failure,
        dependencies,
        execution,
        readiness,
        display_name,
        command: Box::new(command),
        start,
        runtime_state: Some(if runtime.pending_job {
            zup_plan::ServiceRuntimeState::Unknown
        } else {
            runtime.state
        }),
    })
}

pub fn read_canonical_source(canonical: &Path) -> Result<Option<Vec<u8>>, ExecError> {
    read_regular_no_follow(canonical)
}

fn read_regular_no_follow(path: &Path) -> Result<Option<Vec<u8>>, ExecError> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| {
            ExecError::refused(
                &path.to_string_lossy(),
                "unit source has no parent directory",
            )
        })?;
    let directory = match OwnedDirectory::open(parent) {
        Ok(directory) => directory,
        Err(error) if error.is_not_found() => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            ExecError::refused(
                &path.to_string_lossy(),
                "unit source has no valid file name",
            )
        })?;
    match directory.kind_or_absent(name)? {
        None => Ok(None),
        Some(EntryKind::Regular) => {
            let mut file = directory.open_regular_read(name)?;
            use std::io::Read;
            let mut bytes = Vec::new();
            file.by_ref()
                .take(MAX_UNIT_BYTES as u64 + 1)
                .read_to_end(&mut bytes)
                .map_err(|error| crate::error::PathError::io(path, error))?;
            if bytes.len() > MAX_UNIT_BYTES {
                return Err(ExecError::refused(
                    name,
                    "unit source exceeds its size limit",
                ));
            }
            Ok(Some(bytes))
        }
        Some(_) => Err(ExecError::conflict(
            name,
            "unit source is not a regular file",
        )),
    }
}

pub fn check_no_full_override(unit: &str, dir: &Path) -> Result<(), ExecError> {
    let override_path = dir.join(unit);
    let metadata = match std::fs::symlink_metadata(&override_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(crate::error::PathError::io(&override_path, error).into()),
    };
    if metadata.file_type().is_symlink()
        && std::fs::read_link(&override_path)
            .is_ok_and(|target| target.to_string_lossy() == "/dev/null")
    {
        return Ok(());
    }
    Err(ExecError::conflict(
        unit,
        "an administrator override shadows the unit; remove it first",
    ))
}

pub fn admin_override_dir() -> PathBuf {
    PathBuf::from("/etc/systemd/system")
}

pub fn load_path_dirs() -> Vec<PathBuf> {
    [
        "/etc/systemd/system",
        "/run/systemd/system",
        "/usr/local/lib/systemd/system",
        "/usr/lib/systemd/system",
    ]
    .into_iter()
    .map(PathBuf::from)
    .collect()
}

pub fn check_collisions(
    unit: &str,
    canonical: &Path,
    dirs: &[PathBuf],
    service: Option<&ServiceOperation>,
) -> Result<(), ExecError> {
    let owns_mask = service.is_some_and(|service| {
        matches!(
            service.kind,
            zup_plan::IntegrationChangeKind::UpdateOwned
                | zup_plan::IntegrationChangeKind::RestoreOwned
                | zup_plan::IntegrationChangeKind::RepairOwned
                | zup_plan::IntegrationChangeKind::NoOp
                | zup_plan::IntegrationChangeKind::Drift
        ) && (matches!(
            service.previous,
            ObservedServiceState::Service {
                start: ServiceStart::Disabled,
                ..
            }
        ) || service.kind == zup_plan::IntegrationChangeKind::RestoreOwned
            && service.start == ServiceStart::Disabled)
    });
    for dir in dirs {
        let candidate = dir.join(unit);
        if candidate == canonical {
            continue;
        }
        match std::fs::symlink_metadata(&candidate) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => {
                return Err(ExecError::Ambiguous {
                    unit: unit.to_owned(),
                    reason: format!("cannot inspect `{}`", candidate.display()),
                });
            }
            Ok(metadata) => {
                if owns_mask
                    && candidate == admin_override_dir().join(unit)
                    && metadata.file_type().is_symlink()
                    && std::fs::read_link(&candidate)
                        .map_err(|error| crate::error::PathError::io(&candidate, error))?
                        == Path::new("/dev/null")
                {
                    continue;
                }
                if metadata.file_type().is_file() || metadata.file_type().is_symlink() {
                    return Err(ExecError::conflict(
                        unit,
                        format!(
                            "unit `{unit}` already exists at `{}` outside Zup ownership",
                            candidate.display()
                        ),
                    ));
                }
                return Err(ExecError::conflict(
                    unit,
                    format!("unit `{unit}` is shadowed at `{}`", candidate.display()),
                ));
            }
        }
    }
    Ok(())
}

pub fn validate_missing_source_residue(
    unit: &str,
    canonical: &Path,
    owned: Option<&OwnedResource>,
) -> Result<(), ExecError> {
    let startup = match owned {
        Some(OwnedResource::Service(zup_plan::OwnedService { installed, .. })) => {
            match installed.as_ref() {
                zup_plan::ServiceState::Registration { start, .. } => Some(*start),
                _ => None,
            }
        }
        _ => None,
    };
    validate_missing_source_residue_at(
        unit,
        canonical,
        startup,
        &admin_override_dir(),
        Path::new("/run/systemd/system"),
    )
}

fn validate_missing_source_residue_at(
    unit: &str,
    canonical: &Path,
    startup: Option<ServiceStart>,
    persistent: &Path,
    runtime: &Path,
) -> Result<(), ExecError> {
    for (path, permitted_target) in [
        (
            persistent.join(unit),
            (startup == Some(ServiceStart::Disabled)).then_some(Path::new("/dev/null")),
        ),
        (
            persistent.join("multi-user.target.wants").join(unit),
            (startup == Some(ServiceStart::Automatic)).then_some(canonical),
        ),
        (runtime.join(unit), None),
        (runtime.join("multi-user.target.wants").join(unit), None),
    ] {
        match std::fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(crate::error::PathError::io(&path, error).into()),
            Ok(metadata) => {
                if !metadata.file_type().is_symlink()
                    || permitted_target.is_none()
                    || std::fs::read_link(&path)
                        .map_err(|error| crate::error::PathError::io(&path, error))?
                        .as_path()
                        != permitted_target.expect("checked target")
                {
                    return Err(ExecError::conflict(
                        unit,
                        format!("unowned native service residue at `{}`", path.display()),
                    ));
                }
            }
        }
    }
    Ok(())
}

pub fn refuse_source_symlink(unit: &str, canonical: &Path) -> Result<(), ExecError> {
    if let Some(parent) = canonical.parent().filter(|p| !p.as_os_str().is_empty()) {
        crate::fs::anchor_reachable(parent).map_err(|error| {
            ExecError::refused(
                unit,
                format!("unit directory passes through a link: {error}"),
            )
        })?;
    }
    match std::fs::symlink_metadata(canonical) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(ExecError::Ambiguous {
            unit: unit.to_owned(),
            reason: format!("cannot inspect unit source: {error}"),
        }),
        Ok(metadata) if metadata.file_type().is_symlink() => Err(ExecError::conflict(
            unit,
            "the unit source path is a symbolic link, which Zup never follows",
        )),
        Ok(metadata) if metadata.is_file() => Ok(()),
        Ok(_) => Err(ExecError::conflict(
            unit,
            "the unit source path is not a regular file",
        )),
    }
}

pub fn validate_executable(
    command: &zup_core::CommandSpec,
    target_files: &BTreeMap<String, bool>,
    roots: &MachineRoots,
    expected_uid: u32,
    filesystem: bool,
) -> Result<PathBuf, ExecError> {
    let host = crate::lowering::to_host_path(&command.executable).map_err(|error| {
        ExecError::refused("", format!("service binary is not a Linux path: {error}"))
    })?;
    if !host.is_absolute() {
        return Err(ExecError::refused("", "a service binary is absolute"));
    }
    let destination = match crate::machine::authorize_machine_destination(&host, roots) {
        Ok(crate::machine::MachineDestination::Programs) => host.clone(),
        Ok(_) => {
            return Err(ExecError::refused(
                "",
                "a service binary lives under the machine program tree",
            ));
        }
        Err(error) => return Err(ExecError::refused("", error.to_string())),
    };
    let key = command.executable.to_string();
    match target_files.get(&key) {
        Some(true) => {}
        Some(false) => {
            return Err(ExecError::refused(
                "",
                "a service binary is a Zup-owned executable payload",
            ));
        }
        None => {
            return Err(ExecError::refused(
                "",
                "a service binary corresponds to a Zup-owned machine payload (`/bin/sh` and other foreign paths are refused)",
            ));
        }
    }
    crate::fs::anchor_reachable(&host)
        .map_err(|error| ExecError::refused("", format!("service binary: {error}")))?;
    if !filesystem {
        return Ok(destination);
    }
    validate_executable_bits(&host, expected_uid).map(|_| destination)
}

pub fn validate_executable_live(
    command: &zup_core::CommandSpec,
    roots: &MachineRoots,
    expected_uid: u32,
) -> Result<PathBuf, ExecError> {
    let host = crate::lowering::to_host_path(&command.executable).map_err(|error| {
        ExecError::refused("", format!("service binary is not a Linux path: {error}"))
    })?;
    if !host.is_absolute() {
        return Err(ExecError::refused("", "a service binary is absolute"));
    }
    match crate::machine::authorize_machine_destination(&host, roots) {
        Ok(crate::machine::MachineDestination::Programs) => {}
        Ok(_) => {
            return Err(ExecError::refused(
                "",
                "a service binary lives under the machine program tree",
            ));
        }
        Err(error) => return Err(ExecError::refused("", error.to_string())),
    }
    validate_executable_bits(&host, expected_uid).map(|_| host)
}

fn validate_executable_bits(host: &Path, expected_uid: u32) -> Result<(), ExecError> {
    use std::os::unix::fs::MetadataExt as _;

    let metadata = std::fs::symlink_metadata(host).map_err(|error| {
        ExecError::refused(
            "",
            format!("service binary at `{}`: {error}", host.display()),
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(ExecError::refused(
            "",
            "a service binary is a regular file, not a link",
        ));
    }
    if metadata.uid() != expected_uid {
        return Err(ExecError::refused(
            "",
            format!(
                "a service binary is owned by uid {}, not {expected_uid}",
                metadata.uid()
            ),
        ));
    }
    if metadata.mode() & 0o022 != 0 {
        return Err(ExecError::refused(
            "",
            "a service binary is never writable below root",
        ));
    }
    if metadata.mode() & 0o111 == 0 {
        return Err(ExecError::refused(
            "",
            "a service binary is marked executable",
        ));
    }
    Ok(())
}

pub fn validate_changes(unit: &str, changes: &[UnitChange]) -> Result<(), ExecError> {
    if changes.len() > 64 {
        return Err(ExecError::Ambiguous {
            unit: unit.to_owned(),
            reason: "systemd reports more changes than one unit owns".into(),
        });
    }
    for change in changes {
        let relevant = change.source.contains(unit) || change.destination.contains(unit);
        if !relevant {
            return Err(ExecError::Ambiguous {
                unit: unit.to_owned(),
                reason: format!(
                    "systemd reports an unexpected change: {} {} {}",
                    change.kind, change.source, change.destination
                ),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[rstest::rstest]
    #[case(None, false, false)]
    #[case(Some(ServiceStart::Automatic), false, true)]
    #[case(Some(ServiceStart::Manual), false, false)]
    #[case(Some(ServiceStart::Disabled), true, true)]
    #[case(Some(ServiceStart::Automatic), true, false)]
    fn missing_sources_do_not_transfer_native_residue_ownership(
        #[case] startup: Option<ServiceStart>,
        #[case] mask: bool,
        #[case] accepted: bool,
    ) {
        let root = tempfile::tempdir().unwrap();
        let persistent = root.path().join("etc");
        let runtime = root.path().join("run");
        let canonical = root.path().join("vendor/zup-owned.service");
        let unit = "zup-owned.service";
        let path = if mask {
            persistent.join(unit)
        } else {
            persistent.join("multi-user.target.wants").join(unit)
        };
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let target = if mask {
            Path::new("/dev/null")
        } else {
            canonical.as_path()
        };
        std::os::unix::fs::symlink(target, &path).unwrap();
        assert_eq!(
            validate_missing_source_residue_at(unit, &canonical, startup, &persistent, &runtime)
                .is_ok(),
            accepted
        );
        assert_eq!(std::fs::read_link(&path).unwrap(), target);
    }

    fn isolated() -> (tempfile::TempDir, MachineRoots, SystemdRoots) {
        let base = tempfile::tempdir().expect("an isolated base");
        let roots = MachineRoots::new(
            base.path().join("opt"),
            base.path().join("var/lib/zup"),
            base.path().join("var/opt"),
            base.path().join("environment.d"),
        );
        let systemd = SystemdRoots::new(base.path().join("units"));
        std::fs::create_dir_all(&systemd.unit_dir).expect("a unit tree");
        (base, roots, systemd)
    }

    fn command_for(executable: &str) -> zup_core::CommandSpec {
        let target = zup_core::Target::parse("linux-x64").expect("a target");
        zup_core::CommandSpec::new(
            zup_core::TargetPath::new(target, executable).expect("a path"),
            Vec::new(),
        )
    }

    #[test]
    fn inspecting_service_sources_never_hides_unsafe_state() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source.service");
        assert!(read_canonical_source(&source).unwrap().is_none());
        std::fs::write(&source, vec![b'x'; MAX_UNIT_BYTES + 1]).unwrap();
        assert!(read_canonical_source(&source).is_err());
        std::fs::remove_file(&source).unwrap();
        std::os::unix::fs::symlink(root.path().join("foreign"), &source).unwrap();
        assert!(read_canonical_source(&source).is_err());
        std::fs::remove_file(&source).unwrap();
        std::fs::write(&source, b"a file cannot be a parent directory").unwrap();
        assert!(read_canonical_source(&source.join("child")).is_err());
    }

    #[test]
    fn persistent_states_map_and_runtime_variants_do_not() {
        assert_eq!(policy_for_state("enabled"), Some(ServiceStart::Automatic));
        assert_eq!(policy_for_state("disabled"), Some(ServiceStart::Manual));
        assert_eq!(policy_for_state("masked"), Some(ServiceStart::Disabled));

        for state in [
            "enabled-runtime",
            "masked-runtime",
            "linked",
            "linked-runtime",
            "static",
            "indirect",
            "generated",
            "transient",
            "bad",
            "",
        ] {
            assert_eq!(policy_for_state(state), None, "{state} must not map");
        }
    }

    #[test]
    fn unit_paths_authorize_narrowly() {
        let (_base, _roots, systemd) = isolated();
        let path = authorize_systemd_unit("zup-tool-abc123.service", &systemd).expect("a unit");
        assert_eq!(path, systemd.unit_dir.join("zup-tool-abc123.service"));
        for evil in [
            "tool",
            "../etc/passwd.service",
            "a/b.service",
            "x\0.service",
            ".service",
        ] {
            assert!(
                authorize_systemd_unit(evil, &systemd).is_err(),
                "{evil:?} must be refused"
            );
        }

        assert!(authorize_systemd_unit("", &systemd).is_err());
    }

    #[test]
    fn same_name_units_elsewhere_are_conflicts() {
        let (_base, _roots, systemd) = isolated();
        let unit = "zup-tool-conflict.service";
        let canonical = systemd.unit_dir.join(unit);
        for dir in ["etc", "run", "usr-lib"] {
            let other = _base.path().join(dir);
            std::fs::create_dir_all(&other).expect("a load-path stand-in");
            std::fs::write(other.join(unit), b"[Unit]\n").expect("an unrelated unit");
            let dirs = vec![
                _base.path().join("etc"),
                _base.path().join("run"),
                systemd.unit_dir.clone(),
                _base.path().join("usr-lib"),
            ];
            assert!(
                check_collisions(unit, &canonical, &dirs, None).is_err(),
                "a unit in {dir} must conflict"
            );
            std::fs::remove_file(other.join(unit)).expect("cleanup");
        }
        let dirs = vec![
            _base.path().join("etc"),
            _base.path().join("run"),
            systemd.unit_dir.clone(),
            _base.path().join("usr-lib"),
        ];
        assert!(check_collisions(unit, &canonical, &dirs, None).is_ok());
    }

    #[test]
    fn a_source_symlink_is_never_followed() {
        let (_base, _roots, systemd) = isolated();
        let unit = "zup-tool-link.service";
        let canonical = systemd.unit_dir.join(unit);
        let elsewhere = _base.path().join("elsewhere.service");
        std::fs::write(&elsewhere, b"[Unit]\n").expect("a target");
        std::os::unix::fs::symlink(&elsewhere, &canonical).expect("a planted link");
        assert!(refuse_source_symlink(unit, &canonical).is_err());
        assert_eq!(
            std::fs::read(&elsewhere)
                .expect("the target survives")
                .as_slice(),
            b"[Unit]\n"
        );
    }

    #[test]
    fn foreign_and_untrusted_binaries_are_refused() {
        let (base, roots, _systemd) = isolated();
        let uid = rustix::process::getuid().as_raw();
        let files = BTreeMap::new();

        for foreign in ["/bin/sh", "/home/user/tool", "/tmp/tool"] {
            let command = command_for(foreign);
            assert!(
                validate_executable(&command, &files, &roots, uid, false).is_err(),
                "{foreign} is never a service binary"
            );
        }

        let stranger = base.path().join("opt").join("stranger");
        std::fs::create_dir_all(&stranger).expect("a directory");
        let missing = stranger.join("tool").to_string_lossy().into_owned();
        assert!(
            validate_executable(&command_for(&missing), &files, &roots, uid, false).is_err(),
            "an unowned program-tree path refuses"
        );
    }

    #[test]
    fn an_admin_override_shadows_the_unit() {
        let dir = tempfile::tempdir().expect("an isolated admin layer");
        let unit = "zup-tool-admin.service";
        assert!(check_no_full_override(unit, dir.path()).is_ok());
        std::fs::write(dir.path().join(unit), b"[Unit]\n").expect("an override");
        assert!(check_no_full_override(unit, dir.path()).is_err());
    }

    #[test]
    fn unexpected_manager_changes_refuse() {
        let unit = "zup-tool-a.service";
        let foreign = crate::systemd::UnitChange::bound(
            "symlink",
            "/etc/systemd/system/other.service",
            "/usr/lib/systemd/system/other.service",
        )
        .expect("a bounded change");
        assert!(validate_changes(unit, &[foreign]).is_err());
        let owned = crate::systemd::UnitChange::bound(
            "symlink",
            "/etc/systemd/system/multi-user.target.wants/zup-tool-a.service",
            "/usr/local/lib/systemd/system/zup-tool-a.service",
        )
        .expect("a bounded change");
        assert!(validate_changes(unit, &[owned]).is_ok());
    }

    #[test]
    fn ownership_bits_are_enforced_on_the_live_file() {
        let (base, roots, _systemd) = isolated();
        let uid = rustix::process::getuid().as_raw();
        let dir = base.path().join("opt").join("acme");
        std::fs::create_dir_all(&dir).expect("a directory");
        let host = dir.join("tool");
        std::fs::write(&host, b"elf").expect("a binary");
        let text = host.to_string_lossy().into_owned();
        let mut files = BTreeMap::new();
        files.insert(text.clone(), true);
        let command = command_for(&text);
        use std::os::unix::fs::PermissionsExt as _;

        std::fs::set_permissions(&host, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        if uid == 0 {
            rustix::fs::chown(&host, Some(rustix::process::Uid::from_raw(65534)), None)
                .expect("re-own the fixture");
        }
        assert!(
            validate_executable(&command, &files, &roots, 0, true).is_err(),
            "a user-owned binary never becomes a system service"
        );
        if uid == 0 {
            rustix::fs::chown(&host, Some(rustix::process::Uid::from_raw(0)), None)
                .expect("restore ownership");
        }

        std::fs::set_permissions(&host, std::fs::Permissions::from_mode(0o777)).expect("chmod");
        assert!(
            validate_executable(&command, &files, &roots, uid, true).is_err(),
            "a world-writable binary never becomes a system service"
        );

        std::fs::set_permissions(&host, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let link = dir.join("link");
        std::os::unix::fs::symlink(&host, &link).expect("a link");
        let link_text = link.to_string_lossy().into_owned();
        let mut link_files = BTreeMap::new();
        link_files.insert(link_text.clone(), true);
        assert!(
            validate_executable(&command_for(&link_text), &link_files, &roots, uid, true).is_err(),
            "a symlink binary is never followed"
        );

        assert!(
            validate_executable(&command, &files, &roots, uid, true).is_ok(),
            "a trusted binary validates"
        );
    }
}
