use crate::error::PathError;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

pub const MACHINE_PROGRAMS_ROOT: &str = "/opt";

pub const MACHINE_STATE_ROOT: &str = "/var/lib/zup";

pub const MACHINE_SHARED_DATA_ROOT: &str = "/var/opt";

pub const SYSTEMD_UNIT_DIR: &str = "/usr/local/lib/systemd/system";

pub const SYSTEMD_UNIT_FILE_MODE: u32 = 0o644;

pub const MACHINE_STATE_DIR_MODE: u32 = 0o755;

pub const MACHINE_PRIVATE_DIR_MODE: u32 = 0o700;

pub const MACHINE_PRIVATE_FILE_MODE: u32 = 0o600;

pub const MACHINE_PUBLIC_FILE_MODE: u32 = 0o644;

pub const MACHINE_LOCK_FILE_MODE: u32 = 0o644;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineRoots {
    pub programs: PathBuf,

    pub state: PathBuf,

    pub shared_data: PathBuf,
}

impl MachineRoots {
    pub fn production() -> Self {
        Self {
            programs: PathBuf::from(MACHINE_PROGRAMS_ROOT),
            state: PathBuf::from(MACHINE_STATE_ROOT),
            shared_data: PathBuf::from(MACHINE_SHARED_DATA_ROOT),
        }
    }

    pub fn new(programs: PathBuf, state: PathBuf, shared_data: PathBuf) -> Self {
        Self {
            programs,
            state,
            shared_data,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MachineDestination {
    Programs,

    SharedData,

    MachineState,
}

pub fn authorize_machine_destination(
    host: &Path,
    roots: &MachineRoots,
) -> Result<MachineDestination, PathError> {
    let refused = |reason: &str| PathError::PolicyRefused {
        path: host.display().to_string(),
        reason: reason.to_owned(),
    };
    if !host.is_absolute() {
        return Err(refused("a privileged destination is absolute"));
    }

    if host
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(refused("a privileged destination names no `..`"));
    }
    if host == roots.programs || host == roots.state || host == roots.shared_data {
        return Err(refused("a privileged destination is never a root itself"));
    }
    if host.starts_with(&roots.programs) {
        return Ok(MachineDestination::Programs);
    }
    if host.starts_with(&roots.shared_data) {
        return Ok(MachineDestination::SharedData);
    }
    if host.starts_with(&roots.state) {
        return Ok(MachineDestination::MachineState);
    }
    Err(refused(
        "outside the machine payload, variable-data, and Zup state trees",
    ))
}

pub fn authorize_machine_install_directory(
    host: &Path,
    roots: &MachineRoots,
) -> Result<(), PathError> {
    match authorize_machine_destination(host, roots)? {
        MachineDestination::Programs => Ok(()),
        MachineDestination::SharedData | MachineDestination::MachineState => {
            Err(PathError::PolicyRefused {
                path: host.display().to_string(),
                reason: "a machine install directory lives under the program tree".to_owned(),
            })
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemdRoots {
    pub unit_dir: PathBuf,
}

impl SystemdRoots {
    pub fn production() -> Self {
        Self {
            unit_dir: PathBuf::from(SYSTEMD_UNIT_DIR),
        }
    }

    pub fn new(unit_dir: PathBuf) -> Self {
        Self { unit_dir }
    }
}

pub fn authorize_systemd_unit(unit: &str, roots: &SystemdRoots) -> Result<PathBuf, PathError> {
    let refused = |reason: &str| PathError::PolicyRefused {
        path: format!("{}/{}", roots.unit_dir.display(), unit),
        reason: reason.to_owned(),
    };
    if !unit.ends_with(".service") || unit.contains('/') || unit.contains('\0') {
        return Err(refused("a service unit is a single `<name>.service` file"));
    }
    let stem = unit.strip_suffix(".service").unwrap_or_default();
    if stem.is_empty() {
        return Err(refused("a service unit names a unit"));
    }
    if unit.bytes().any(|b| b == b'\n' || b == b'\r' || b < 0x20) {
        return Err(refused("a service unit name holds no control characters"));
    }
    let path = roots.unit_dir.join(unit);
    if path.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::CurDir | Component::Prefix(_)
        )
    }) {
        return Err(refused("a service unit path names no `.` or `..`"));
    }
    Ok(path)
}

pub fn ensure_machine_state_root(
    roots: &MachineRoots,
    expected_uid: u32,
) -> Result<PathBuf, PathError> {
    ensure_machine_state_dir(&roots.state, expected_uid)?;
    Ok(roots.state.clone())
}

pub fn ensure_machine_state_dir(state_root: &Path, expected_uid: u32) -> Result<(), PathError> {
    let parent = state_root.parent().ok_or_else(|| PathError::StateRefused {
        path: state_root.display().to_string(),
        reason: "a machine state root has a parent".to_owned(),
    })?;
    verify_trusted_parent(parent, expected_uid)?;
    create_or_verify_dir(state_root, MACHINE_STATE_DIR_MODE, expected_uid)?;

    crate::fs::sync_directory(parent).map_err(|error| PathError::StateRefused {
        path: parent.display().to_string(),
        reason: format!("the state parent does not flush: {error}"),
    })?;
    Ok(())
}

pub fn ensure_transactions_dir(state_root: &Path, expected_uid: u32) -> Result<PathBuf, PathError> {
    let directory = state_root.join("transactions");
    create_or_verify_dir(&directory, MACHINE_PRIVATE_DIR_MODE, expected_uid)?;
    Ok(directory)
}

fn verify_trusted_parent(parent: &Path, expected_uid: u32) -> Result<(), PathError> {
    let metadata = std::fs::symlink_metadata(parent).map_err(|source| PathError::Io {
        path: parent.display().to_string(),
        source,
    })?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(PathError::StateRefused {
            path: parent.display().to_string(),
            reason: "the state parent is a real directory, not a link or a special file".to_owned(),
        });
    }
    verify_owner_and_privacy(parent, &metadata, expected_uid)
}

fn create_or_verify_dir(path: &Path, mode: u32, expected_uid: u32) -> Result<(), PathError> {
    use std::os::unix::fs::DirBuilderExt as _;

    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                return Err(PathError::StateRefused {
                    path: path.display().to_string(),
                    reason: "machine state is a real directory, not a link or a special file"
                        .to_owned(),
                });
            }
            verify_owner_and_privacy(path, &metadata, expected_uid)
        }
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            std::fs::DirBuilder::new()
                .recursive(false)
                .mode(mode)
                .create(path)
                .map_err(|source| PathError::Io {
                    path: path.display().to_string(),
                    source,
                })?;

            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).map_err(
                |source| PathError::Io {
                    path: path.display().to_string(),
                    source,
                },
            )?;
            let metadata = std::fs::symlink_metadata(path).map_err(|source| PathError::Io {
                path: path.display().to_string(),
                source,
            })?;
            verify_owner_and_privacy(path, &metadata, expected_uid)
        }
        Err(source) => Err(PathError::Io {
            path: path.display().to_string(),
            source,
        }),
    }
}

fn verify_owner_and_privacy(
    path: &Path,
    metadata: &std::fs::Metadata,
    expected_uid: u32,
) -> Result<(), PathError> {
    if metadata.uid() != expected_uid {
        return Err(PathError::StateRefused {
            path: path.display().to_string(),
            reason: format!(
                "machine state is owned by uid {}, not {}",
                metadata.uid(),
                expected_uid
            ),
        });
    }
    if metadata.mode() & 0o022 != 0 {
        return Err(PathError::StateRefused {
            path: path.display().to_string(),
            reason: "machine state is never writable by group or other".to_owned(),
        });
    }
    Ok(())
}

pub fn verify_trusted_state_file(path: &Path, expected_uid: u32) -> Result<(), PathError> {
    let metadata = std::fs::symlink_metadata(path).map_err(|source| PathError::Io {
        path: path.display().to_string(),
        source,
    })?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(PathError::StateRefused {
            path: path.display().to_string(),
            reason: "trusted machine state is a regular file, not a link or a special file"
                .to_owned(),
        });
    }
    verify_owner_and_privacy(path, &metadata, expected_uid)
}

pub fn verify_machine_hierarchy(state_root: &Path, expected_uid: u32) -> Result<(), PathError> {
    crate::fs::refuse_symlink_ancestors(state_root).map_err(|error| PathError::StateRefused {
        path: state_root.display().to_string(),
        reason: format!("the machine state hierarchy must not pass through a link: {error}"),
    })?;
    match std::fs::symlink_metadata(state_root) {
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(PathError::Io {
                path: state_root.display().to_string(),
                source,
            });
        }
        Ok(metadata) => {
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                return Err(PathError::StateRefused {
                    path: state_root.display().to_string(),
                    reason: "machine state is a real directory, not a link or a special file"
                        .to_owned(),
                });
            }
            verify_owner_and_privacy(state_root, &metadata, expected_uid)?;
        }
    }
    verify_tree(state_root, expected_uid, 8, true)
}

pub fn verify_machine_structure(state_root: &Path, expected_uid: u32) -> Result<(), PathError> {
    crate::fs::refuse_symlink_ancestors(state_root).map_err(|error| PathError::StateRefused {
        path: state_root.display().to_string(),
        reason: format!("the machine state hierarchy must not pass through a link: {error}"),
    })?;
    match std::fs::symlink_metadata(state_root) {
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(PathError::Io {
                path: state_root.display().to_string(),
                source,
            });
        }
        Ok(metadata) => {
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                return Err(PathError::StateRefused {
                    path: state_root.display().to_string(),
                    reason: "machine state is a real directory, not a link or a special file"
                        .to_owned(),
                });
            }
            if metadata.uid() != expected_uid {
                return Err(PathError::StateRefused {
                    path: state_root.display().to_string(),
                    reason: format!(
                        "machine state is owned by uid {}, not {}",
                        metadata.uid(),
                        expected_uid
                    ),
                });
            }
        }
    }
    verify_tree(state_root, expected_uid, 8, false)
}

fn verify_tree(
    directory: &Path,
    expected_uid: u32,
    depth: u32,
    privacy: bool,
) -> Result<(), PathError> {
    if depth == 0 {
        return Err(PathError::StateRefused {
            path: directory.display().to_string(),
            reason: "machine state is deeper than it should ever be".to_owned(),
        });
    }
    let entries = std::fs::read_dir(directory).map_err(|source| PathError::Io {
        path: directory.display().to_string(),
        source,
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| PathError::Io {
            path: directory.display().to_string(),
            source,
        })?;
        let path = entry.path();
        let metadata = std::fs::symlink_metadata(&path).map_err(|source| PathError::Io {
            path: path.display().to_string(),
            source,
        })?;
        if metadata.file_type().is_symlink() {
            return Err(PathError::StateRefused {
                path: path.display().to_string(),
                reason: "a machine state entry is a symbolic link".to_owned(),
            });
        }
        if !metadata.is_dir() && !metadata.is_file() {
            return Err(PathError::StateRefused {
                path: path.display().to_string(),
                reason: "a machine state entry is a special file".to_owned(),
            });
        }
        if privacy {
            verify_owner_and_privacy(&path, &metadata, expected_uid)?;
        } else if metadata.uid() != expected_uid {
            return Err(PathError::StateRefused {
                path: path.display().to_string(),
                reason: format!(
                    "machine state is owned by uid {}, not {}",
                    metadata.uid(),
                    expected_uid
                ),
            });
        }
        if metadata.is_dir() {
            verify_tree(&path, expected_uid, depth - 1, privacy)?;
        }
    }
    Ok(())
}

pub fn verify_ledger_trust(
    state_root: &Path,
    app_id: &zup_core::AppId,
    expected_uid: u32,
) -> Result<(), PathError> {
    let path = crate::ledger::LinuxLedgerStore::new(state_root)
        .path_for(app_id, zup_core::SelectedScope::Machine);
    match std::fs::symlink_metadata(&path) {
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(PathError::Io {
            path: path.display().to_string(),
            source,
        }),
        Ok(_) => verify_trusted_state_file(&path, expected_uid),
    }
}

pub fn normalize_state_modes(state_root: &Path, expected_uid: u32) -> Result<(), PathError> {
    let entries = match std::fs::read_dir(state_root) {
        Ok(entries) => entries,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(PathError::Io {
                path: state_root.display().to_string(),
                source,
            });
        }
    };
    for entry in entries {
        let entry = entry.map_err(|source| PathError::Io {
            path: state_root.display().to_string(),
            source,
        })?;
        let path = entry.path();
        let metadata = std::fs::symlink_metadata(&path).map_err(|source| PathError::Io {
            path: path.display().to_string(),
            source,
        })?;
        if metadata.file_type().is_symlink() || (!metadata.is_dir() && !metadata.is_file()) {
            return Err(PathError::StateRefused {
                path: path.display().to_string(),
                reason: "machine state holds only real files and directories".to_owned(),
            });
        }
        if metadata.uid() != expected_uid {
            return Err(PathError::StateRefused {
                path: path.display().to_string(),
                reason: format!(
                    "machine state is owned by uid {}, not {}",
                    metadata.uid(),
                    expected_uid
                ),
            });
        }
        if path
            .extension()
            .is_some_and(|extension| extension == "lock")
        {
            if !metadata.is_file() {
                return Err(PathError::StateRefused {
                    path: path.display().to_string(),
                    reason: "a lock marker is a regular file".to_owned(),
                });
            }
            set_mode(&path, MACHINE_LOCK_FILE_MODE)?;
            continue;
        }
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        match (name, metadata.is_dir()) {
            ("transactions", true) => {
                set_mode(&path, MACHINE_PRIVATE_DIR_MODE)?;
                normalize_tree(
                    &path,
                    MACHINE_PRIVATE_DIR_MODE,
                    MACHINE_PRIVATE_FILE_MODE,
                    expected_uid,
                )?
            }
            ("installations" | "generated", true) => {
                set_mode(&path, MACHINE_STATE_DIR_MODE)?;
                normalize_tree(
                    &path,
                    MACHINE_STATE_DIR_MODE,
                    MACHINE_PUBLIC_FILE_MODE,
                    expected_uid,
                )?;
            }
            _ => {}
        }
    }
    Ok(())
}

fn normalize_tree(
    directory: &Path,
    dir_mode: u32,
    file_mode: u32,
    expected_uid: u32,
) -> Result<(), PathError> {
    let entries = std::fs::read_dir(directory).map_err(|source| PathError::Io {
        path: directory.display().to_string(),
        source,
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| PathError::Io {
            path: directory.display().to_string(),
            source,
        })?;
        let path = entry.path();
        let metadata = std::fs::symlink_metadata(&path).map_err(|source| PathError::Io {
            path: path.display().to_string(),
            source,
        })?;
        if metadata.file_type().is_symlink() || (!metadata.is_dir() && !metadata.is_file()) {
            return Err(PathError::StateRefused {
                path: path.display().to_string(),
                reason: "machine state holds only real files and directories".to_owned(),
            });
        }
        if metadata.uid() != expected_uid {
            return Err(PathError::StateRefused {
                path: path.display().to_string(),
                reason: format!(
                    "machine state is owned by uid {}, not {}",
                    metadata.uid(),
                    expected_uid
                ),
            });
        }
        if metadata.is_dir() {
            set_mode(&path, dir_mode)?;
            normalize_tree(&path, dir_mode, file_mode, expected_uid)?;
        } else {
            set_mode(&path, file_mode)?;
        }
    }
    Ok(())
}

fn set_mode(path: &Path, mode: u32) -> Result<(), PathError> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).map_err(|source| {
        PathError::Io {
            path: path.display().to_string(),
            source,
        }
    })
}

pub fn normalize_published_modes(
    state_root: &Path,
    ledger: &zup_exec::InstallLedger,
) -> Result<(), PathError> {
    let store = crate::ledger::LinuxLedgerStore::new(state_root);
    let ledger_path = store.path_for(&ledger.app_id, ledger.scope);
    std::fs::set_permissions(
        &ledger_path,
        std::fs::Permissions::from_mode(MACHINE_PUBLIC_FILE_MODE),
    )
    .map_err(|source| PathError::Io {
        path: ledger_path.display().to_string(),
        source,
    })?;
    for owned in ledger.resources.values() {
        let zup_exec::OwnedResource::File { destination, .. } = owned else {
            continue;
        };
        let Ok(host) = crate::lowering::to_host_path(destination) else {
            continue;
        };

        if host.starts_with(state_root) {
            std::fs::set_permissions(&host, std::fs::Permissions::from_mode(0o755)).map_err(
                |source| PathError::Io {
                    path: host.display().to_string(),
                    source,
                },
            )?;
        }
    }
    Ok(())
}

pub fn host_to_install_template(
    host: &Path,
    roots: &MachineRoots,
    target: &zup_core::TargetTriple,
) -> Result<zup_core::Template, PathError> {
    if host == roots.programs || !host.starts_with(&roots.programs) {
        return Err(PathError::PolicyRefused {
            path: host.display().to_string(),
            reason: "an install directory override lives under the program tree".to_owned(),
        });
    }
    let relative = host
        .strip_prefix(&roots.programs)
        .map_err(|_| PathError::PolicyRefused {
            path: host.display().to_string(),
            reason: "an install directory override lives under the program tree".to_owned(),
        })?;
    let mut text = "${location.programs}".to_owned();
    for component in relative.components() {
        match component {
            Component::Normal(name) => {
                text.push('/');
                text.push_str(&name.to_string_lossy());
            }
            _ => {
                return Err(PathError::PolicyRefused {
                    path: host.display().to_string(),
                    reason: "an install directory override names plain components".to_owned(),
                });
            }
        }
    }

    let _ = target;
    zup_core::Template::parse(&text).map_err(|error| PathError::PolicyRefused {
        path: host.display().to_string(),
        reason: error.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn isolated() -> (tempfile::TempDir, MachineRoots) {
        let base = tempfile::tempdir().expect("an isolated base");
        let roots = MachineRoots::new(
            base.path().join("opt"),
            base.path().join("var/lib/zup"),
            base.path().join("var/opt"),
        );
        (base, roots)
    }

    #[test]
    fn payload_variable_data_and_state_classify() {
        let (_base, roots) = isolated();
        assert_eq!(
            authorize_machine_destination(&roots.programs.join("Acme/tool"), &roots)
                .expect("payload"),
            MachineDestination::Programs
        );
        assert_eq!(
            authorize_machine_destination(&roots.shared_data.join("Acme/data"), &roots)
                .expect("variable data"),
            MachineDestination::SharedData
        );
        assert_eq!(
            authorize_machine_destination(&roots.state.join("installations/x.json"), &roots)
                .expect("state"),
            MachineDestination::MachineState
        );
    }

    #[test]
    fn unrelated_trees_are_refused() {
        let (_base, roots) = isolated();
        for path in [
            PathBuf::from("/etc/passwd"),
            PathBuf::from("/usr/bin/tool"),
            PathBuf::from("/home/user/tool"),
            PathBuf::from("/root/tool"),
            PathBuf::from("/tmp/tool"),
            PathBuf::from("/"),
            roots.programs.clone(),
            roots.state.clone(),
        ] {
            assert!(
                authorize_machine_destination(&path, &roots).is_err(),
                "{} must be refused",
                path.display()
            );
        }
    }

    #[test]
    fn prefix_tricks_and_traversals_are_refused() {
        let (_base, roots) = isolated();
        let sibling = roots.programs.with_extension("ish").join("app");
        assert!(authorize_machine_destination(&sibling, &roots).is_err());
        assert!(
            authorize_machine_destination(&roots.programs.join("../etc/passwd"), &roots).is_err()
        );

        assert_eq!(
            authorize_machine_destination(&roots.programs.join("./app"), &roots)
                .expect("`.` is absorbed, not refused"),
            MachineDestination::Programs
        );
        assert!(
            authorize_machine_destination(Path::new("opt/Acme"), &roots).is_err(),
            "relative paths are refused"
        );
    }

    #[test]
    fn install_directories_stay_inside_the_program_tree() {
        let (_base, roots) = isolated();
        authorize_machine_install_directory(&roots.programs.join("Acme"), &roots)
            .expect("an application directory");
        assert!(
            authorize_machine_install_directory(&roots.state.join("Acme"), &roots).is_err(),
            "state is not an install directory"
        );
        assert!(
            authorize_machine_install_directory(&PathBuf::from("/etc"), &roots).is_err(),
            "nor is anywhere else"
        );
        assert!(
            authorize_machine_install_directory(&roots.programs, &roots).is_err(),
            "nor is the program root itself"
        );
    }

    #[test]
    fn state_creation_verifies_and_enforces_modes() {
        let (base, roots) = isolated();
        std::fs::create_dir_all(roots.state.parent().expect("a parent")).expect("a parent");
        let uid = current_uid();
        let state = ensure_machine_state_root(&roots, uid).expect("creation");
        assert_eq!(state, roots.state);
        let mode = std::fs::symlink_metadata(&state)
            .expect("stat")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, MACHINE_STATE_DIR_MODE);

        ensure_machine_state_root(&roots, uid).expect("verification");

        assert!(ensure_machine_state_root(&roots, uid.wrapping_add(1)).is_err());
        let _ = base;
    }

    #[test]
    fn a_link_where_state_belongs_is_refused() {
        let (base, roots) = isolated();
        std::fs::create_dir_all(roots.state.parent().expect("a parent")).expect("a parent");
        let elsewhere = base.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).expect("a target");
        std::os::unix::fs::symlink(&elsewhere, &roots.state).expect("a planted link");
        assert!(ensure_machine_state_root(&roots, current_uid()).is_err());
    }

    #[test]
    fn a_world_writable_state_directory_is_refused() {
        let (_base, roots) = isolated();
        std::fs::create_dir_all(roots.state.parent().expect("a parent")).expect("a parent");
        std::fs::create_dir_all(&roots.state).expect("a directory");
        let mut permissions = std::fs::metadata(&roots.state).expect("stat").permissions();
        permissions.set_mode(0o777);
        std::fs::set_permissions(&roots.state, permissions).expect("chmod");
        assert!(ensure_machine_state_root(&roots, current_uid()).is_err());
    }

    #[test]
    fn trusted_files_must_be_regular_and_private() {
        let base = tempfile::tempdir().expect("a base");
        let uid = current_uid();
        let file = base.path().join("ledger.json");
        std::fs::write(&file, b"{}").expect("a file");
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        verify_trusted_state_file(&file, uid).expect("a root-owned private file verifies");
        let link = base.path().join("link.json");
        std::os::unix::fs::symlink(&file, &link).expect("a link");
        assert!(verify_trusted_state_file(&link, uid).is_err());
    }

    #[test]
    fn normalization_enforces_the_ownership_model() {
        let (_base, roots) = isolated();
        std::fs::create_dir_all(roots.state.parent().expect("a parent")).expect("a parent");
        let uid = current_uid();
        let transactions = roots.state.join("transactions");
        let record = transactions.join("some-id");
        std::fs::create_dir_all(&record).expect("a tree");
        std::fs::write(record.join("transaction.json"), b"{}").expect("a file");
        let installations = roots.state.join("installations");
        std::fs::create_dir_all(&installations).expect("a tree");
        std::fs::write(installations.join("owned.json"), b"{}").expect("a file");
        let lock = roots.state.join("zup-install-test-machine.lock");
        std::fs::write(&lock, b"").expect("a lock");
        for path in [&transactions, &record, &installations] {
            let mut permissions = std::fs::metadata(path).expect("stat").permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(path, permissions).expect("chmod");
        }
        normalize_state_modes(&roots.state, uid).expect("normalizes");
        let mode = |path: &Path| {
            std::fs::symlink_metadata(path)
                .expect("stat")
                .permissions()
                .mode()
                & 0o777
        };
        assert_eq!(mode(&transactions), MACHINE_PRIVATE_DIR_MODE);
        assert_eq!(mode(&record), MACHINE_PRIVATE_DIR_MODE);
        assert_eq!(
            mode(&record.join("transaction.json")),
            MACHINE_PRIVATE_FILE_MODE
        );
        assert_eq!(mode(&installations), MACHINE_STATE_DIR_MODE);
        assert_eq!(
            mode(&installations.join("owned.json")),
            MACHINE_PUBLIC_FILE_MODE
        );
        assert_eq!(mode(&lock), MACHINE_LOCK_FILE_MODE);
    }

    fn current_uid() -> u32 {
        rustix::process::getuid().as_raw()
    }
}
