use crate::error::PathError;
use crate::locations::MachineRoots;
use crate::services::SYSTEMD_UNIT_DIR;
use std::path::{Component, Path, PathBuf};

pub const SYSTEMD_UNIT_FILE_MODE: u32 = 0o644;

pub const MACHINE_STATE_DIR_MODE: u32 = 0o755;

pub const MACHINE_PRIVATE_DIR_MODE: u32 = 0o700;

pub const MACHINE_PRIVATE_FILE_MODE: u32 = 0o600;

pub const MACHINE_PUBLIC_FILE_MODE: u32 = 0o644;

pub const MACHINE_LOCK_FILE_MODE: u32 = 0o644;

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

pub fn resolve_machine_state_root(
    requested: Option<&Path>,
    roots: &MachineRoots,
) -> Result<PathBuf, PathError> {
    let state = requested
        .map(Path::to_path_buf)
        .unwrap_or_else(|| roots.state.clone());
    if state != roots.state {
        return Err(PathError::StateRefused {
            path: state.display().to_string(),
            reason: format!(
                "machine state is the worker's own state root ({}), never a client path",
                roots.state.display()
            ),
        });
    }
    Ok(state)
}

pub fn ensure_machine_state_dir(state_root: &Path, expected_uid: u32) -> Result<(), PathError> {
    if assess_machine_state_root(state_root, expected_uid, true)?
        == MachineStateReadiness::Uninitialized
    {
        create_or_verify_dir(state_root, MACHINE_STATE_DIR_MODE, expected_uid)?;
    }
    verify_machine_hierarchy(state_root, expected_uid)?;

    let parent = state_root.parent().ok_or_else(|| PathError::StateRefused {
        path: state_root.display().to_string(),
        reason: "a machine state root has a parent".to_owned(),
    })?;
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
    let owned = crate::fs::OwnedDirectory::open(parent)?;
    verify_owner_and_privacy_fd(&owned, parent, expected_uid)
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
            let owned =
                crate::fs::OwnedDirectory::open(path).map_err(|error| PathError::StateRefused {
                    path: path.display().to_string(),
                    reason: format!("machine state is a reachable directory: {error}"),
                })?;
            verify_owner_and_privacy_fd(&owned, path, expected_uid)
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

            verify_created(path, mode, expected_uid)
        }
        Err(source) => Err(PathError::Io {
            path: path.display().to_string(),
            source,
        }),
    }
}

fn verify_created(path: &Path, mode: u32, expected_uid: u32) -> Result<(), PathError> {
    let owned = crate::fs::OwnedDirectory::open(path).map_err(|error| PathError::StateRefused {
        path: path.display().to_string(),
        reason: format!("machine state is a reachable directory: {error}"),
    })?;
    let rustix_mode = rustix::fs::Mode::from_bits_truncate(mode);
    owned
        .chmod_self(rustix_mode)
        .map_err(|error| PathError::StateRefused {
            path: path.display().to_string(),
            reason: format!("machine state keeps its mode: {error}"),
        })?;
    verify_owner_and_privacy_fd(&owned, path, expected_uid)
}

fn verify_owner_and_privacy_fd(
    owned: &crate::fs::OwnedDirectory,
    path: &Path,
    expected_uid: u32,
) -> Result<(), PathError> {
    let stat = owned.stat_self()?;
    if stat.uid != expected_uid {
        return Err(PathError::StateRefused {
            path: path.display().to_string(),
            reason: format!(
                "machine state is owned by uid {}, not {}",
                stat.uid, expected_uid
            ),
        });
    }
    if stat.mode & 0o022 != 0 {
        return Err(PathError::StateRefused {
            path: path.display().to_string(),
            reason: "machine state is never writable by group or other".to_owned(),
        });
    }
    Ok(())
}

pub fn verify_trusted_state_file(path: &Path, expected_uid: u32) -> Result<(), PathError> {
    let refused = |reason: &str| PathError::StateRefused {
        path: path.display().to_string(),
        reason: reason.to_owned(),
    };
    let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    else {
        return Err(refused("trusted machine state has a parent directory"));
    };
    let Some(name) = path
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
    else {
        return Err(refused("trusted machine state names its file"));
    };
    let directory =
        crate::fs::OwnedDirectory::open(parent).map_err(|error| PathError::StateRefused {
            path: path.display().to_string(),
            reason: format!("trusted machine state is reachable: {error}"),
        })?;
    let current = directory.stream_regular(name).map_err(|_| {
        refused("trusted machine state is a regular file, not a link or a special file")
    })?;
    if current.uid != expected_uid {
        return Err(PathError::StateRefused {
            path: path.display().to_string(),
            reason: format!(
                "machine state is owned by uid {}, not {}",
                current.uid, expected_uid
            ),
        });
    }
    if current.mode & 0o022 != 0 {
        return Err(refused("machine state is never writable by group or other"));
    }
    Ok(())
}

pub fn verify_machine_hierarchy(state_root: &Path, expected_uid: u32) -> Result<(), PathError> {
    require_initialized(
        assess_machine_state_root(state_root, expected_uid, true)?,
        state_root,
    )
}

pub fn verify_machine_structure(state_root: &Path, expected_uid: u32) -> Result<(), PathError> {
    require_initialized(
        assess_machine_state_root(state_root, expected_uid, false)?,
        state_root,
    )
}

pub fn verify_machine_install_roots(
    roots: &MachineRoots,
    expected_uid: u32,
) -> Result<(), PathError> {
    for root in [&roots.programs, &roots.shared_data] {
        crate::fs::verify_ancestor_trust(root, expected_uid).map_err(|error| {
            PathError::StateRefused {
                path: root.display().to_string(),
                reason: format!("a machine install root is not trustworthy: {error}"),
            }
        })?;
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MachineStateReadiness {
    Uninitialized,
    Initialized,
}

pub fn assess_machine_state_root(
    state_root: &Path,
    expected_uid: u32,
    privacy: bool,
) -> Result<MachineStateReadiness, PathError> {
    crate::fs::anchor_reachable(state_root).map_err(|error| PathError::StateRefused {
        path: state_root.display().to_string(),
        reason: format!("the machine state hierarchy must not pass through a link: {error}"),
    })?;
    let owned = match crate::fs::OwnedDirectory::open(state_root) {
        Ok(owned) => owned,
        Err(PathError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
            verify_creation_parent(state_root, expected_uid)?;
            return Ok(MachineStateReadiness::Uninitialized);
        }
        Err(error) => return Err(error),
    };
    verify_initialized_root(&owned, state_root, expected_uid, privacy)?;
    verify_tree(state_root, expected_uid, 8, privacy)?;
    Ok(MachineStateReadiness::Initialized)
}

fn require_initialized(
    readiness: MachineStateReadiness,
    state_root: &Path,
) -> Result<(), PathError> {
    match readiness {
        MachineStateReadiness::Initialized => Ok(()),
        MachineStateReadiness::Uninitialized => Err(PathError::StateRefused {
            path: state_root.display().to_string(),
            reason: "machine state is not initialized".to_owned(),
        }),
    }
}

fn verify_creation_parent(state_root: &Path, expected_uid: u32) -> Result<(), PathError> {
    let Some(parent) = state_root
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    else {
        return Err(PathError::StateRefused {
            path: state_root.display().to_string(),
            reason: "a machine state root has a parent".to_owned(),
        });
    };
    verify_trusted_parent(parent, expected_uid)
}

fn verify_initialized_root(
    owned: &crate::fs::OwnedDirectory,
    state_root: &Path,
    expected_uid: u32,
    privacy: bool,
) -> Result<(), PathError> {
    let stat = owned.stat_self()?;
    if stat.kind != crate::fs::EntryKind::Directory {
        return Err(PathError::StateRefused {
            path: state_root.display().to_string(),
            reason: "machine state is a real directory, not a link or a special file".to_owned(),
        });
    }
    if stat.uid != expected_uid {
        return Err(PathError::StateRefused {
            path: state_root.display().to_string(),
            reason: format!(
                "machine state is owned by uid {}, not {}",
                stat.uid, expected_uid
            ),
        });
    }
    if privacy && stat.mode & 0o022 != 0 {
        return Err(PathError::StateRefused {
            path: state_root.display().to_string(),
            reason: "machine state is never writable by group or other".to_owned(),
        });
    }
    Ok(())
}

fn verify_tree(
    directory: &Path,
    expected_uid: u32,
    depth: u32,
    privacy: bool,
) -> Result<(), PathError> {
    let owned =
        crate::fs::OwnedDirectory::open(directory).map_err(|error| PathError::StateRefused {
            path: directory.display().to_string(),
            reason: format!("machine state is a reachable directory: {error}"),
        })?;
    verify_open(&owned, directory, expected_uid, depth, privacy)
}

fn verify_open(
    owned: &crate::fs::OwnedDirectory,
    display: &Path,
    expected_uid: u32,
    depth: u32,
    privacy: bool,
) -> Result<(), PathError> {
    if depth == 0 {
        return Err(PathError::StateRefused {
            path: display.display().to_string(),
            reason: "machine state is deeper than it should ever be".to_owned(),
        });
    }
    for name in owned
        .child_names()
        .map_err(|error| PathError::StateRefused {
            path: display.display().to_string(),
            reason: format!("machine state does not list: {error}"),
        })?
    {
        let path = display.join(&name);
        let stat = rustix::fs::statat(
            owned.as_fd(),
            name.as_str(),
            rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
        )
        .map_err(|error| PathError::errno(&path, error))?;
        let kind = crate::fs::EntryKind::from_raw_mode(stat.st_mode);
        if kind == crate::fs::EntryKind::Symlink {
            return Err(PathError::StateRefused {
                path: path.display().to_string(),
                reason: "a machine state entry is a symbolic link".to_owned(),
            });
        }
        if kind != crate::fs::EntryKind::Directory && kind != crate::fs::EntryKind::Regular {
            return Err(PathError::StateRefused {
                path: path.display().to_string(),
                reason: "a machine state entry is a special file".to_owned(),
            });
        }
        if privacy {
            if stat.st_uid != expected_uid || stat.st_mode & 0o022 != 0 {
                return Err(PathError::StateRefused {
                    path: path.display().to_string(),
                    reason: "machine state is privately owned".to_owned(),
                });
            }
        } else if stat.st_uid != expected_uid {
            return Err(PathError::StateRefused {
                path: path.display().to_string(),
                reason: format!(
                    "machine state is owned by uid {}, not {}",
                    stat.st_uid, expected_uid
                ),
            });
        }
        if kind == crate::fs::EntryKind::Directory {
            let (child, dev, ino) =
                owned
                    .open_child_verified(&name)
                    .map_err(|error| PathError::StateRefused {
                        path: path.display().to_string(),
                        reason: format!("machine state holds a stable directory: {error}"),
                    })?;
            let _ = (dev, ino);
            verify_open(&child, &path, expected_uid, depth - 1, privacy)?;
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
    let owned = match crate::fs::OwnedDirectory::open(state_root) {
        Ok(owned) => owned,
        Err(PathError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    for name in owned.child_names().map_err(|error| PathError::Io {
        path: state_root.display().to_string(),
        source: std::io::Error::other(error.to_string()),
    })? {
        let path = state_root.join(&name);
        let stat = owned.stat_child(&name)?;
        if stat.kind == crate::fs::EntryKind::Symlink
            || (stat.kind != crate::fs::EntryKind::Directory
                && stat.kind != crate::fs::EntryKind::Regular)
        {
            return Err(PathError::StateRefused {
                path: path.display().to_string(),
                reason: "machine state holds only real files and directories".to_owned(),
            });
        }
        if stat.uid != expected_uid {
            return Err(PathError::StateRefused {
                path: path.display().to_string(),
                reason: format!(
                    "machine state is owned by uid {}, not {}",
                    stat.uid, expected_uid
                ),
            });
        }
        if name.ends_with(".lock") {
            if stat.kind != crate::fs::EntryKind::Regular {
                return Err(PathError::StateRefused {
                    path: path.display().to_string(),
                    reason: "a lock marker is a regular file".to_owned(),
                });
            }
            owned.chmod_child(
                &name,
                rustix::fs::Mode::from_bits_truncate(MACHINE_LOCK_FILE_MODE),
            )?;
            continue;
        }
        match (name.as_str(), stat.kind) {
            ("transactions" | "locks" | "objects", crate::fs::EntryKind::Directory) => {
                owned.chmod_child(
                    &name,
                    rustix::fs::Mode::from_bits_truncate(MACHINE_PRIVATE_DIR_MODE),
                )?;
                let (child, _, _) =
                    owned
                        .open_child_verified(&name)
                        .map_err(|error| PathError::StateRefused {
                            path: path.display().to_string(),
                            reason: format!("machine state holds a stable directory: {error}"),
                        })?;
                normalize_open(
                    &child,
                    MACHINE_PRIVATE_DIR_MODE,
                    MACHINE_PRIVATE_FILE_MODE,
                    expected_uid,
                )?
            }
            ("installations", crate::fs::EntryKind::Directory) => {
                owned.chmod_child(
                    &name,
                    rustix::fs::Mode::from_bits_truncate(MACHINE_STATE_DIR_MODE),
                )?;
                let (child, _, _) =
                    owned
                        .open_child_verified(&name)
                        .map_err(|error| PathError::StateRefused {
                            path: path.display().to_string(),
                            reason: format!("machine state holds a stable directory: {error}"),
                        })?;
                normalize_open(
                    &child,
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

fn normalize_open(
    owned: &crate::fs::OwnedDirectory,
    dir_mode: u32,
    file_mode: u32,
    expected_uid: u32,
) -> Result<(), PathError> {
    let display = owned.path().to_path_buf();
    for name in owned.child_names().map_err(|error| PathError::Io {
        path: display.display().to_string(),
        source: std::io::Error::other(error.to_string()),
    })? {
        let path = display.join(&name);
        let stat = owned.stat_child(&name)?;
        if stat.kind == crate::fs::EntryKind::Symlink
            || (stat.kind != crate::fs::EntryKind::Directory
                && stat.kind != crate::fs::EntryKind::Regular)
        {
            return Err(PathError::StateRefused {
                path: path.display().to_string(),
                reason: "machine state holds only real files and directories".to_owned(),
            });
        }
        if stat.uid != expected_uid {
            return Err(PathError::StateRefused {
                path: path.display().to_string(),
                reason: format!(
                    "machine state is owned by uid {}, not {}",
                    stat.uid, expected_uid
                ),
            });
        }
        if stat.kind == crate::fs::EntryKind::Directory {
            owned.chmod_child(&name, rustix::fs::Mode::from_bits_truncate(dir_mode))?;
            let (child, _, _) =
                owned
                    .open_child_verified(&name)
                    .map_err(|error| PathError::StateRefused {
                        path: path.display().to_string(),
                        reason: format!("machine state holds a stable directory: {error}"),
                    })?;
            normalize_open(&child, dir_mode, file_mode, expected_uid)?;
        } else {
            owned.chmod_child(&name, rustix::fs::Mode::from_bits_truncate(file_mode))?;
        }
    }
    Ok(())
}

fn set_mode(path: &Path, mode: u32) -> Result<(), PathError> {
    let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    else {
        return Err(PathError::Refused {
            path: path.display().to_string(),
            reason: "a mode change names its parent directory".to_owned(),
        });
    };
    let Some(name) = path
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
    else {
        return Err(PathError::Refused {
            path: path.display().to_string(),
            reason: "a mode change names its file".to_owned(),
        });
    };
    let owned = crate::fs::OwnedDirectory::open(parent)?;
    owned.chmod_child(name, rustix::fs::Mode::from_bits_truncate(mode))
}

pub fn normalize_published_modes(
    state_root: &Path,
    ledger: &zup_exec::InstallLedger,
) -> Result<(), PathError> {
    let store = crate::ledger::LinuxLedgerStore::new(state_root);
    let ledger_path = store.path_for(&ledger.app_id, ledger.scope);
    set_mode(&ledger_path, MACHINE_PUBLIC_FILE_MODE)?;
    for owned in ledger.resources.values() {
        let zup_exec::OwnedResource::File { destination, .. } = owned else {
            continue;
        };
        let Ok(host) = crate::lowering::to_host_path(destination) else {
            continue;
        };

        if host.starts_with(state_root) {
            set_mode(&host, 0o755)?;
        }
    }
    Ok(())
}

pub fn host_to_install_template(
    host: &Path,
    roots: &MachineRoots,
    target: &zup_core::Target,
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
    use std::os::unix::fs::PermissionsExt as _;

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

    #[test]
    fn install_root_ancestry_is_trusted() {
        let base = tempfile::tempdir().expect("an isolated base");
        let roots = MachineRoots::new(
            base.path().join("opt"),
            base.path().join("var/lib/zup"),
            base.path().join("var/opt"),
        );
        for root in [&roots.programs, &roots.shared_data] {
            std::fs::create_dir_all(root).expect("a root");
        }
        let uid = current_uid();
        let owned = crate::fs::OwnedDirectory::open(base.path()).expect("the base opens");
        owned
            .verify_descendants(Path::new("opt"), uid)
            .expect("private owned ancestry verifies");
        owned
            .verify_descendants(Path::new("var/opt"), uid)
            .expect("nested private owned ancestry verifies");

        let writable = base.path().join("opt");
        let mut permissions = std::fs::metadata(&writable).expect("stat").permissions();
        permissions.set_mode(0o775);
        std::fs::set_permissions(&writable, permissions).expect("chmod");
        assert!(
            owned.verify_descendants(Path::new("opt"), uid).is_err(),
            "a group-writable ancestor refuses machine installation"
        );

        assert!(
            owned
                .verify_descendants(Path::new("opt"), uid.wrapping_add(1))
                .is_err(),
            "ancestry owned by someone else refuses"
        );
        assert!(
            owned.verify_descendants(Path::new("../opt"), uid).is_err(),
            "a walk never escapes its base"
        );
        assert!(
            crate::fs::verify_ancestor_trust(Path::new("opt/acme"), uid).is_err(),
            "a relative ancestry is refused without touching the filesystem"
        );
    }

    #[test]
    fn an_absent_state_root_is_uninitialized_not_valid() {
        let (_base, roots) = isolated();
        std::fs::create_dir_all(roots.state.parent().expect("a parent")).expect("a parent");
        let uid = current_uid();
        assert_eq!(
            assess_machine_state_root(&roots.state, uid, true).expect("an assessment runs"),
            MachineStateReadiness::Uninitialized,
            "absence with a trusted parent is plannable, not initialized"
        );
        assert!(
            verify_machine_hierarchy(&roots.state, uid).is_err(),
            "absence never counts as a valid initialized state"
        );
        ensure_machine_state_dir(&roots.state, uid).expect("a first installation initializes");
        assert_eq!(
            assess_machine_state_root(&roots.state, uid, true).expect("an assessment runs"),
            MachineStateReadiness::Initialized,
        );
        verify_machine_hierarchy(&roots.state, uid).expect("the initialized state verifies");
    }

    #[test]
    fn an_absent_parent_is_an_error_not_an_absent_state() {
        let (_base, roots) = isolated();
        let uid = current_uid();
        match assess_machine_state_root(&roots.state, uid, true) {
            Err(PathError::Io { source, .. }) => assert_eq!(
                source.kind(),
                std::io::ErrorKind::NotFound,
                "a missing parent stays a missing parent, never a plannable absence"
            ),
            other => panic!("a missing parent refuses distinctly: {other:?}"),
        }
        assert!(
            verify_machine_hierarchy(&roots.state, uid).is_err(),
            "and it is never a valid initialized state"
        );
    }

    #[test]
    fn a_sticky_world_writable_ancestor_stays_trusted() {
        let base = tempfile::tempdir().expect("an isolated base");
        let owned = crate::fs::OwnedDirectory::open(base.path()).expect("the base opens");
        let uid = current_uid();
        for (mode, trusted) in [
            (0o755, true),
            (0o775, false),
            (0o1777, true),
            (0o0777, false),
        ] {
            let child = base.path().join(format!("child-{mode:o}"));
            std::fs::create_dir_all(&child).expect("a child");
            let mut permissions = std::fs::metadata(&child).expect("stat").permissions();
            permissions.set_mode(mode);
            std::fs::set_permissions(&child, permissions).expect("chmod");
            let name = child
                .file_name()
                .expect("a name")
                .to_string_lossy()
                .into_owned();
            let name = std::path::Path::new(&name);
            assert_eq!(
                owned.verify_descendants(name, uid).is_ok(),
                trusted,
                "mode {mode:o} without an owner change"
            );
        }
    }

    #[test]
    fn a_root_owned_ancestry_verifies_for_root() {
        assert!(
            crate::fs::verify_ancestor_trust(Path::new("/"), 0).is_ok(),
            "the filesystem root anchors privileged ancestry"
        );
    }

    fn current_uid() -> u32 {
        rustix::process::getuid().as_raw()
    }
}
