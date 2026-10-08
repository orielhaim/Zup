use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use zup_core::{BackendResourceId, ResourceKey, ServiceStart};
use zup_exec::{ObservedService, ObservedServiceState, OwnedResource, ServiceOperation};
use zup_platform::TargetPlan;

use crate::error::ExecError;
use crate::fs::{EntryKind, OwnedDirectory};
use crate::machine::{MachineRoots, SystemdRoots, authorize_systemd_unit};
use crate::services::{parse_unit, unit_name};
use crate::systemd::{SystemdManager, UnitChange};

pub const SERVICE_BACKEND_PREFIX: &str = "linux:service:";

pub const MAX_UNIT_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ServicePayload {
    Apply {
        service: ServiceOperation,
        unit: String,
        unit_bytes: Vec<u8>,

        binary_owned: bool,

        previous_policy: String,

        force: bool,
    },
    Remove {
        key: ResourceKey,
        unit: String,
        owned: OwnedResource,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ServiceReceipt {
    pub unit: String,
    pub previous_source_sha256: Option<String>,
    pub installed_source_sha256: Option<String>,
    pub previous_policy: String,
    pub installed_policy: String,
    pub reloaded: bool,
    pub changes: Vec<UnitChange>,
}

pub fn backend_id_for_unit(unit: &str) -> BackendResourceId {
    BackendResourceId::new(format!("{SERVICE_BACKEND_PREFIX}{unit}"))
        .expect("a service backend id is non-empty")
}

pub fn backend_key_for_unit(unit: &str) -> ResourceKey {
    ResourceKey::Backend {
        id: backend_id_for_unit(unit),
    }
}

pub fn ledger_key_for_payload(payload: &ServicePayload) -> ResourceKey {
    match payload {
        ServicePayload::Apply { service, .. } => service.key.clone(),
        ServicePayload::Remove { key, .. } => key.clone(),
    }
}

pub fn encode_payload(payload: &ServicePayload) -> Result<Vec<u8>, ExecError> {
    if matches!(payload, ServicePayload::Apply { unit_bytes, .. } if unit_bytes.len() > MAX_UNIT_BYTES)
    {
        return Err(ExecError::refused("", "a unit source exceeds its bound"));
    }
    serde_json::to_vec(payload)
        .map_err(|error| ExecError::refused("", format!("service payload: {error}")))
}

pub fn decode_payload(bytes: &[u8]) -> Result<ServicePayload, ExecError> {
    if bytes.len() > zup_transaction::MAX_BACKEND_PAYLOAD_BYTES {
        return Err(ExecError::refused(
            "",
            "a service payload exceeds its bound",
        ));
    }
    serde_json::from_slice(bytes)
        .map_err(|error| ExecError::refused("", format!("invalid service payload: {error}")))
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
                "systemd {major} is older than the minimum {} for `Type=exec` service units",
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
    target: &TargetPlan,
    manager: &mut dyn SystemdManager,
    systemd: &SystemdRoots,
) -> Result<Vec<ObservedService>, ExecError> {
    let mut out = Vec::with_capacity(target.services.len());
    for service in &target.services {
        let unit =
            unit_name(&service.id).map_err(|error| ExecError::refused("", error.to_string()))?;
        let path = authorize_systemd_unit(&unit, systemd)
            .map_err(|error| ExecError::refused(&unit, error.to_string()))?;
        let state = observe_unit(&unit, &path, manager)?;
        out.push(ObservedService {
            key: service.key.clone(),
            id: service.id.clone(),
            state,
        });
    }
    Ok(out)
}

fn observe_unit(
    unit: &str,
    path: &Path,
    manager: &mut dyn SystemdManager,
) -> Result<ObservedServiceState, ExecError> {
    let bytes = read_regular_no_follow(path)?;
    let Some(bytes) = bytes else {
        return Ok(ObservedServiceState::Absent);
    };

    let Ok((display_name, argv)) = parse_unit(&bytes) else {
        return Ok(ObservedServiceState::Absent);
    };
    if argv.is_empty() {
        return Ok(ObservedServiceState::Absent);
    }
    let state_text = manager
        .unit_file_state(unit)
        .map_err(|error| ExecError::Ambiguous {
            unit: unit.to_owned(),
            reason: format!("unit-file state is unknown: {error}"),
        })?;
    let Some(start) = policy_for_state(&state_text) else {
        return Ok(ObservedServiceState::Absent);
    };
    let executable = argv[0].clone();
    let target = zup_platform::TargetPath::new(target_triple_for(&executable), &executable);
    let command = match target {
        Ok(executable) => zup_platform::CommandSpec::new(executable, argv[1..].to_vec()),
        Err(_) => return Ok(ObservedServiceState::Absent),
    };
    Ok(ObservedServiceState::Service {
        display_name,
        command,
        start,
        runtime_state: None,
    })
}

fn target_triple_for(_executable: &str) -> zup_core::TargetTriple {
    zup_core::TargetTriple::parse("x86_64-unknown-linux-gnu").expect("a Linux target")
}

pub fn read_canonical_source(canonical: &Path) -> Result<Option<Vec<u8>>, ExecError> {
    read_regular_no_follow(canonical)
}

fn read_regular_no_follow(path: &Path) -> Result<Option<Vec<u8>>, ExecError> {
    let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) else {
        return Ok(None);
    };
    let directory = match OwnedDirectory::open(parent) {
        Ok(directory) => directory,
        Err(_) => return Ok(None),
    };
    let name = match path.file_name().map(|n| n.to_string_lossy().into_owned()) {
        Some(name) if !name.is_empty() => name,
        _ => return Ok(None),
    };
    match directory.kind_or_absent(&name) {
        Ok(None) => Ok(None),
        Ok(Some(EntryKind::Regular)) => match directory.read_regular(&name) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(_) => Ok(None),
        },
        Ok(Some(_)) | Err(_) => Ok(None),
    }
}

pub fn check_no_full_override(unit: &str, dir: &Path) -> Result<(), ExecError> {
    let override_path = dir.join(unit);
    let metadata = match std::fs::symlink_metadata(&override_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Ok(()),
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

pub fn check_collisions(unit: &str, canonical: &Path, dirs: &[PathBuf]) -> Result<(), ExecError> {
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

pub fn refuse_source_symlink(unit: &str, canonical: &Path) -> Result<(), ExecError> {
    if let Some(parent) = canonical.parent().filter(|p| !p.as_os_str().is_empty()) {
        crate::fs::refuse_symlink_ancestors(parent).map_err(|error| {
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
    command: &zup_platform::CommandSpec,
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
    crate::fs::refuse_symlink_ancestors(&host)
        .map_err(|error| ExecError::refused("", format!("service binary: {error}")))?;
    if !filesystem {
        return Ok(destination);
    }
    validate_executable_bits(&host, expected_uid).map(|_| destination)
}

pub fn validate_executable_live(
    command: &zup_platform::CommandSpec,
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

    fn isolated() -> (tempfile::TempDir, MachineRoots, SystemdRoots) {
        let base = tempfile::tempdir().expect("an isolated base");
        let roots = MachineRoots::new(
            base.path().join("opt"),
            base.path().join("var/lib/zup"),
            base.path().join("var/opt"),
        );
        let systemd = SystemdRoots::new(base.path().join("units"));
        std::fs::create_dir_all(&systemd.unit_dir).expect("a unit tree");
        (base, roots, systemd)
    }

    fn command_for(executable: &str) -> zup_platform::CommandSpec {
        let target = zup_core::TargetTriple::parse("x86_64-unknown-linux-gnu").expect("a target");
        zup_platform::CommandSpec::new(
            zup_platform::TargetPath::new(target, executable).expect("a path"),
            Vec::new(),
        )
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
                check_collisions(unit, &canonical, &dirs).is_err(),
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
        assert!(check_collisions(unit, &canonical, &dirs).is_ok());
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
