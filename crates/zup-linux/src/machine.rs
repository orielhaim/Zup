//! Machine-scope roots and the privileged destination policy.
//!
//! A root worker must not become "write anywhere the manifest asks". This
//! module answers the only two location questions a privileged worker may
//! ever answer affirmatively:
//!
//! ```text
//! /opt/...         application payload
//! /var/lib/zup/... Zup state, ledger, journals, maintenance
//! /var/opt/...     application variable data (SharedData)
//! ```
//!
//! Everything else - `/etc`, `/usr`, `/home`, `/root`, `/tmp`, and every
//! other tree a later typed resource might one day name - is refused here,
//! before any executor sees it. Administrator authentication authorizes Zup
//! to perform its typed installation operations; it is not permission for
//! arbitrary filesystem mutation.
//!
//! # Test isolation without environment overrides
//!
//! [`MachineRoots::new`] takes explicit roots so tests can prove the policy
//! against isolated directories. Production paths always use
//! [`MachineRoots::production`]; no environment variable and no IPC message
//! selects the roots a privileged worker enforces, so an unprivileged caller
//! cannot redirect them.

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

/// Application payload root for machine scope: `/opt`.
pub const MACHINE_PROGRAMS_ROOT: &str = "/opt";
/// Zup machine state root: `/var/lib/zup`.
pub const MACHINE_STATE_ROOT: &str = "/var/lib/zup";
/// Application variable-data root for machine scope: `/var/opt`.
pub const MACHINE_SHARED_DATA_ROOT: &str = "/var/opt";

/// The mode of the machine state root and its public subdirectories.
///
/// Readable and traversable by every account, writable by none but root. The
/// unprivileged installer plans against this state to compute the expected
/// plan digest it binds before Execute, and status inspection reads the same
/// public view - so the ledger and other public metadata live here, while
/// journals and other private transaction state live under a private
/// subdirectory instead of this mode being loosened file by file.
pub const MACHINE_STATE_DIR_MODE: u32 = 0o755;
/// The mode of private machine transaction state (journals, work areas).
pub const MACHINE_PRIVATE_DIR_MODE: u32 = 0o700;
/// The mode of private machine transaction files.
pub const MACHINE_PRIVATE_FILE_MODE: u32 = 0o600;
/// The mode of public machine metadata (ledger documents).
pub const MACHINE_PUBLIC_FILE_MODE: u32 = 0o644;
/// The mode of the machine lock markers.
pub const MACHINE_LOCK_FILE_MODE: u32 = 0o644;

/// The three machine trees a privileged worker may touch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineRoots {
    /// Where application payload goes.
    pub programs: PathBuf,
    /// Where Zup machine state goes.
    pub state: PathBuf,
    /// Where machine application variable data goes.
    pub shared_data: PathBuf,
}

impl MachineRoots {
    /// The production roots. The only roots a privileged worker enforces.
    pub fn production() -> Self {
        Self {
            programs: PathBuf::from(MACHINE_PROGRAMS_ROOT),
            state: PathBuf::from(MACHINE_STATE_ROOT),
            shared_data: PathBuf::from(MACHINE_SHARED_DATA_ROOT),
        }
    }

    /// Explicit roots, for isolated tests.
    ///
    /// A constructor, not a configuration hook: production worker paths call
    /// [`MachineRoots::production`], and nothing an unprivileged process
    /// controls - no environment variable, no IPC field - reaches this.
    pub fn new(programs: PathBuf, state: PathBuf, shared_data: PathBuf) -> Self {
        Self {
            programs,
            state,
            shared_data,
        }
    }
}

/// Which allowed tree a privileged destination belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MachineDestination {
    /// Application payload, under the programs root.
    Programs,
    /// Application variable data, under the shared-data root.
    SharedData,
    /// Zup machine state, under the state root.
    MachineState,
}

/// Why a privileged destination was refused.
#[derive(Debug, thiserror::Error)]
pub enum MachinePathPolicyError {
    #[error("refused privileged path `{path}`: {reason}")]
    Refused { path: String, reason: String },
}

/// Classify one absolute host path against the privileged destination policy.
///
/// Path semantics, not string prefixes: `/optish/app` is not under `/opt`,
/// and `/opt/app/../../etc` never reaches classification because `..` is
/// refused rather than resolved - resolving a hostile path into an allowed
/// one would bless exactly the traversal it attempted.
pub fn authorize_machine_destination(
    host: &Path,
    roots: &MachineRoots,
) -> Result<MachineDestination, MachinePathPolicyError> {
    let refused = |reason: &str| MachinePathPolicyError::Refused {
        path: host.display().to_string(),
        reason: reason.to_owned(),
    };
    if !host.is_absolute() {
        return Err(refused("a privileged destination is absolute"));
    }
    // No normalization: a `..` that survives to this layer is either a bug
    // or an attack, and either way it is refused rather than resolved.
    // `TargetPath` already guarantees canonical spellings; this is the
    // worker's independent check that the guarantee held.
    if host
        .components()
        .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
    {
        return Err(refused("a privileged destination names no `.` or `..`"));
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

/// Authorize an install-directory override for machine scope.
///
/// Overrides stay inside the machine program tree: `--install-dir /etc`
/// must not turn the installer into an arbitrary privileged writer, and
/// neither must its spelling tricks.
pub fn authorize_machine_install_directory(
    host: &Path,
    roots: &MachineRoots,
) -> Result<(), MachinePathPolicyError> {
    match authorize_machine_destination(host, roots)? {
        MachineDestination::Programs => Ok(()),
        MachineDestination::SharedData | MachineDestination::MachineState => {
            Err(MachinePathPolicyError::Refused {
                path: host.display().to_string(),
                reason: "a machine install directory lives under the program tree".to_owned(),
            })
        }
    }
}

/// Why machine state could not be established or trusted.
#[derive(Debug, thiserror::Error)]
pub enum MachineStateError {
    #[error("machine state at `{path}`: {reason}")]
    Refused { path: String, reason: String },

    #[error("machine state I/O at `{path}`: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

/// Create the machine state root from its trusted parent, or verify the
/// existing one.
///
/// Creating `/var/lib/zup` is privileged state mutation, so it is never a
/// `mkdir -p` through an arbitrary walk: the parent must already exist as a
/// real directory, every existing entry on the way down must be the expected
/// type, and symlink or special-file substitutions are refused. New
/// directories get explicit modes rather than inherited umask behavior.
///
/// `expected_uid` is the uid that must own trusted state: `0` in production,
/// the test account's own uid in isolated tests.
pub fn ensure_machine_state_root(
    roots: &MachineRoots,
    expected_uid: u32,
) -> Result<PathBuf, MachineStateError> {
    let parent = roots
        .state
        .parent()
        .ok_or_else(|| MachineStateError::Refused {
            path: roots.state.display().to_string(),
            reason: "a machine state root has a parent".to_owned(),
        })?;
    verify_trusted_parent(parent, expected_uid)?;
    create_or_verify_dir(&roots.state, MACHINE_STATE_DIR_MODE, expected_uid)?;
    // The parent gains an entry; flush it so the name survives a crash.
    crate::fs::sync_directory(parent).map_err(|error| MachineStateError::Refused {
        path: parent.display().to_string(),
        reason: format!("the state parent does not flush: {error}"),
    })?;
    Ok(roots.state.clone())
}

/// Prove the state parent is a real directory the expected owner holds.
///
/// The parent (`/var/lib` in production) is trusted infrastructure, not
/// something this function creates: an absent or substituted parent is a
/// machine this worker does not understand, and understanding it is not
/// this worker's job.
fn verify_trusted_parent(parent: &Path, expected_uid: u32) -> Result<(), MachineStateError> {
    let metadata = std::fs::symlink_metadata(parent).map_err(|source| MachineStateError::Io {
        path: parent.display().to_string(),
        source,
    })?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(MachineStateError::Refused {
            path: parent.display().to_string(),
            reason: "the state parent is a real directory, not a link or a special file".to_owned(),
        });
    }
    verify_owner_and_privacy(parent, &metadata, expected_uid)
}

/// Create `path` with `mode`, or verify the existing entry.
///
/// An existing entry must be a real directory owned by the expected uid and
/// writable by nobody but the owner. A machine ledger replaced by an
/// unprivileged user must not become an instruction to root, and the same
/// holds for the directory that holds it.
fn create_or_verify_dir(
    path: &Path,
    mode: u32,
    expected_uid: u32,
) -> Result<(), MachineStateError> {
    use std::os::unix::fs::DirBuilderExt as _;

    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                return Err(MachineStateError::Refused {
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
                .map_err(|source| MachineStateError::Io {
                    path: path.display().to_string(),
                    source,
                })?;
            // The mode argument is masked by the process umask; applying it
            // again is what makes the privacy unconditional.
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).map_err(
                |source| MachineStateError::Io {
                    path: path.display().to_string(),
                    source,
                },
            )?;
            let metadata =
                std::fs::symlink_metadata(path).map_err(|source| MachineStateError::Io {
                    path: path.display().to_string(),
                    source,
                })?;
            verify_owner_and_privacy(path, &metadata, expected_uid)
        }
        Err(source) => Err(MachineStateError::Io {
            path: path.display().to_string(),
            source,
        }),
    }
}

/// Prove existing state is owned by the expected uid and writable by nobody
/// else.
///
/// Ownership without privacy is half the check: a root-owned directory that
/// the invoking user can write to is a directory whose entries root must not
/// trust, from the ledger down to the lock markers.
fn verify_owner_and_privacy(
    path: &Path,
    metadata: &std::fs::Metadata,
    expected_uid: u32,
) -> Result<(), MachineStateError> {
    if metadata.uid() != expected_uid {
        return Err(MachineStateError::Refused {
            path: path.display().to_string(),
            reason: format!(
                "machine state is owned by uid {}, not {}",
                metadata.uid(),
                expected_uid
            ),
        });
    }
    if metadata.mode() & 0o022 != 0 {
        return Err(MachineStateError::Refused {
            path: path.display().to_string(),
            reason: "machine state is never writable by group or other".to_owned(),
        });
    }
    Ok(())
}

/// Prove a trusted state file is a regular file owned by the expected uid
/// and writable by nobody else.
///
/// The ledger, the lock markers, and the maintenance generation earn trust
/// through this check, not through living under a trusted pathname.
pub fn verify_trusted_state_file(path: &Path, expected_uid: u32) -> Result<(), MachineStateError> {
    let metadata = std::fs::symlink_metadata(path).map_err(|source| MachineStateError::Io {
        path: path.display().to_string(),
        source,
    })?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(MachineStateError::Refused {
            path: path.display().to_string(),
            reason: "trusted machine state is a regular file, not a link or a special file"
                .to_owned(),
        });
    }
    verify_owner_and_privacy(path, &metadata, expected_uid)
}

/// Prove the machine state hierarchy holds no redirection or foreign
/// ownership before anything trusts it.
///
/// Every entry - journals, ledgers, generations, lock markers - must be a
/// real file or directory owned by the expected uid and writable by nobody
/// else. A symlink, a special file, or a foreign-owned entry anywhere in
/// the tree is either planted or corrupt, and either way it is refused
/// before anything reads or writes through it. Same-user attackers cannot
/// replace entries inside a root-owned private hierarchy in the first
/// place; this check makes the assumption explicit instead of load-bearing
/// and silent.
///
/// An absent root is the fresh-machine case, not a redirect: there is
/// nothing to trust yet.
pub fn verify_machine_hierarchy(
    state_root: &Path,
    expected_uid: u32,
) -> Result<(), MachineStateError> {
    crate::fs::refuse_symlink_ancestors(state_root).map_err(|error| {
        MachineStateError::Refused {
            path: state_root.display().to_string(),
            reason: format!("the machine state hierarchy must not pass through a link: {error}"),
        }
    })?;
    match std::fs::symlink_metadata(state_root) {
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(MachineStateError::Io {
                path: state_root.display().to_string(),
                source,
            });
        }
        Ok(metadata) => {
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                return Err(MachineStateError::Refused {
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

/// Prove the machine state structure before normalizing it: no links, no
/// special files, no foreign owners - without judging modes.
///
/// The worker calls this before [`normalize_state_modes`]: journals a
/// previous run left behind carry whatever mode the old umask gave them,
/// and judging them before normalizing would refuse a machine the worker
/// is about to repair. Privacy is enforced by the normalization that
/// follows, which refuses the same evil this refuses.
pub fn verify_machine_structure(
    state_root: &Path,
    expected_uid: u32,
) -> Result<(), MachineStateError> {
    crate::fs::refuse_symlink_ancestors(state_root).map_err(|error| {
        MachineStateError::Refused {
            path: state_root.display().to_string(),
            reason: format!("the machine state hierarchy must not pass through a link: {error}"),
        }
    })?;
    match std::fs::symlink_metadata(state_root) {
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(MachineStateError::Io {
                path: state_root.display().to_string(),
                source,
            });
        }
        Ok(metadata) => {
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                return Err(MachineStateError::Refused {
                    path: state_root.display().to_string(),
                    reason: "machine state is a real directory, not a link or a special file"
                        .to_owned(),
                });
            }
            if metadata.uid() != expected_uid {
                return Err(MachineStateError::Refused {
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

/// Prove one hierarchy level, recursing into real directories.
fn verify_tree(
    directory: &Path,
    expected_uid: u32,
    depth: u32,
    privacy: bool,
) -> Result<(), MachineStateError> {
    if depth == 0 {
        return Err(MachineStateError::Refused {
            path: directory.display().to_string(),
            reason: "machine state is deeper than it should ever be".to_owned(),
        });
    }
    let entries = std::fs::read_dir(directory).map_err(|source| MachineStateError::Io {
        path: directory.display().to_string(),
        source,
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| MachineStateError::Io {
            path: directory.display().to_string(),
            source,
        })?;
        let path = entry.path();
        let metadata =
            std::fs::symlink_metadata(&path).map_err(|source| MachineStateError::Io {
                path: path.display().to_string(),
                source,
            })?;
        if metadata.file_type().is_symlink() {
            return Err(MachineStateError::Refused {
                path: path.display().to_string(),
                reason: "a machine state entry is a symbolic link".to_owned(),
            });
        }
        if !metadata.is_dir() && !metadata.is_file() {
            return Err(MachineStateError::Refused {
                path: path.display().to_string(),
                reason: "a machine state entry is a special file".to_owned(),
            });
        }
        if privacy {
            verify_owner_and_privacy(&path, &metadata, expected_uid)?;
        } else if metadata.uid() != expected_uid {
            return Err(MachineStateError::Refused {
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

/// Prove one application's ledger is trusted state before it is read.
///
/// A machine ledger replaced by an unprivileged user must not become an
/// instruction to root - and an unprivileged planner must not bind a digest
/// from it either. Absence is fine: a machine that never installed this
/// application has no record of it.
pub fn verify_ledger_trust(
    state_root: &Path,
    app_id: &zup_core::AppId,
    expected_uid: u32,
) -> Result<(), MachineStateError> {
    let path = crate::ledger::LinuxLedgerStore::new(state_root)
        .path_for(app_id, zup_core::SelectedScope::Machine);
    match std::fs::symlink_metadata(&path) {
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(MachineStateError::Io {
            path: path.display().to_string(),
            source,
        }),
        Ok(_) => verify_trusted_state_file(&path, expected_uid),
    }
}

/// Normalize the modes of machine state the worker owns: explicit modes,
/// never inherited umask behavior.
///
/// - lock markers become [`MACHINE_LOCK_FILE_MODE`];
/// - `transactions/` becomes private recursively (directories
///   [`MACHINE_PRIVATE_DIR_MODE`], files [`MACHINE_PRIVATE_FILE_MODE`]);
/// - `installations/` and `generated/` become public containers
///   ([`MACHINE_STATE_DIR_MODE`]) holding public metadata
///   ([`MACHINE_PUBLIC_FILE_MODE`]).
///
/// Anything that is not a real file or directory, or not owned by the
/// expected uid, is refused rather than chmodded: normalizing a planted
/// link would bless it.
pub fn normalize_state_modes(
    state_root: &Path,
    expected_uid: u32,
) -> Result<(), MachineStateError> {
    let entries = match std::fs::read_dir(state_root) {
        Ok(entries) => entries,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(MachineStateError::Io {
                path: state_root.display().to_string(),
                source,
            });
        }
    };
    for entry in entries {
        let entry = entry.map_err(|source| MachineStateError::Io {
            path: state_root.display().to_string(),
            source,
        })?;
        let path = entry.path();
        let metadata =
            std::fs::symlink_metadata(&path).map_err(|source| MachineStateError::Io {
                path: path.display().to_string(),
                source,
            })?;
        if metadata.file_type().is_symlink() || (!metadata.is_dir() && !metadata.is_file()) {
            return Err(MachineStateError::Refused {
                path: path.display().to_string(),
                reason: "machine state holds only real files and directories".to_owned(),
            });
        }
        if metadata.uid() != expected_uid {
            return Err(MachineStateError::Refused {
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
                return Err(MachineStateError::Refused {
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
            ("transactions", true) => normalize_tree(
                &path,
                MACHINE_PRIVATE_DIR_MODE,
                MACHINE_PRIVATE_FILE_MODE,
                expected_uid,
            )?,
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

/// Normalize one subtree: directories get `dir_mode`, files get `file_mode`.
fn normalize_tree(
    directory: &Path,
    dir_mode: u32,
    file_mode: u32,
    expected_uid: u32,
) -> Result<(), MachineStateError> {
    let entries = std::fs::read_dir(directory).map_err(|source| MachineStateError::Io {
        path: directory.display().to_string(),
        source,
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| MachineStateError::Io {
            path: directory.display().to_string(),
            source,
        })?;
        let path = entry.path();
        let metadata =
            std::fs::symlink_metadata(&path).map_err(|source| MachineStateError::Io {
                path: path.display().to_string(),
                source,
            })?;
        if metadata.file_type().is_symlink() || (!metadata.is_dir() && !metadata.is_file()) {
            return Err(MachineStateError::Refused {
                path: path.display().to_string(),
                reason: "machine state holds only real files and directories".to_owned(),
            });
        }
        if metadata.uid() != expected_uid {
            return Err(MachineStateError::Refused {
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

fn set_mode(path: &Path, mode: u32) -> Result<(), MachineStateError> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).map_err(|source| {
        MachineStateError::Io {
            path: path.display().to_string(),
            source,
        }
    })
}

/// Normalize the modes of what a committed machine transaction published:
/// the ledger document is public metadata, and the maintenance generation
/// is executable but never writable by anyone but root.
pub fn normalize_published_modes(
    state_root: &Path,
    ledger: &zup_exec::InstallLedger,
) -> Result<(), MachineStateError> {
    let store = crate::ledger::LinuxLedgerStore::new(state_root);
    let ledger_path = store.path_for(&ledger.app_id, ledger.scope);
    std::fs::set_permissions(
        &ledger_path,
        std::fs::Permissions::from_mode(MACHINE_PUBLIC_FILE_MODE),
    )
    .map_err(|source| MachineStateError::Io {
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
        // Only maintenance destinations are normalized: payload modes are
        // the executor's explicit decision, and nothing here re-decides
        // what the transaction published.
        if host.starts_with(state_root) {
            std::fs::set_permissions(&host, std::fs::Permissions::from_mode(0o755)).map_err(
                |source| MachineStateError::Io {
                    path: host.display().to_string(),
                    source,
                },
            )?;
        }
    }
    Ok(())
}

/// Turn an authorized absolute host install directory back into the template
/// the planner resolves.
///
/// The worker plans from templates like every other path; the override is a
/// host path because the policy speaks host paths. Only directories under
/// the enforced program root convert - anything else never reaches here,
/// because the caller authorizes first.
pub fn host_to_install_template(
    host: &Path,
    roots: &MachineRoots,
    target: &zup_core::TargetTriple,
) -> Result<zup_core::Template, MachinePathPolicyError> {
    if host == roots.programs || !host.starts_with(&roots.programs) {
        return Err(MachinePathPolicyError::Refused {
            path: host.display().to_string(),
            reason: "an install directory override lives under the program tree".to_owned(),
        });
    }
    let relative =
        host.strip_prefix(&roots.programs)
            .map_err(|_| MachinePathPolicyError::Refused {
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
                return Err(MachinePathPolicyError::Refused {
                    path: host.display().to_string(),
                    reason: "an install directory override names plain components".to_owned(),
                });
            }
        }
    }
    // The parse validates; the target is accepted to keep one spelling of
    // the round trip at the call sites.
    let _ = target;
    zup_core::Template::parse(&text).map_err(|error| MachinePathPolicyError::Refused {
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
        assert!(
            authorize_machine_destination(&roots.programs.join("./app"), &roots).is_err(),
            "no normalization: `.` is refused, not resolved"
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
        // Twice is fine: the existing root verifies.
        ensure_machine_state_root(&roots, uid).expect("verification");
        // Another account's directory is not trusted state.
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

    fn current_uid() -> u32 {
        rustix::process::getuid().as_raw()
    }
}
