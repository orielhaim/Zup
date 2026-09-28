//! Windows durable file primitives and installation lock.
//!
//! Guarantees used here:
//! - **atomic namespace transition**: `MoveFileExW` publishes a fully-written
//!   temp file in one rename
//! - **power-loss durability**: `FlushFileBuffers` on the temp file, then
//!   `MOVEFILE_WRITE_THROUGH` on publish
//! - **process-crash recovery**: handled by the transaction journal on top
//!
//! If a durability barrier cannot be satisfied the operation returns
//! `DurableError::Unavailable` rather than lying.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use thiserror::Error;

use crate::fs_bindings::{
    self, CREATE_NEW, CloseHandle, CreateFileW, FlushFileBuffers, GENERIC_WRITE, GetLastError,
    GetVolumePathNameW, HANDLE, INVALID_HANDLE_VALUE, MOVEFILE_REPLACE_EXISTING,
    MOVEFILE_WRITE_THROUGH, MoveFileExW,
};

static DURABLE_TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Durability errors. `Unavailable` means the platform cannot meet the contract.
#[derive(Debug, Error)]
pub enum DurableError {
    #[error("durable operation unavailable: {0}")]
    Unavailable(String),

    #[error("I/O failed at `{path}`: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("win32 failure at `{path}`: {message}")]
    Win32 { path: String, message: String },

    #[error("installation busy")]
    InstallationBusy,
}

/// Write `contents` to `path` with power-loss durability.
///
/// Order: write temp → flush temp → durable publish → success.
pub fn write_durable(path: &Path, contents: &[u8]) -> Result<(), DurableError> {
    let tmp = temp_sibling(path);
    write_new_file(&tmp, contents)?;
    if let Err(e) = publish_replace(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// Create `path` only if absent, with durable publication.
pub fn create_durable(path: &Path, contents: &[u8]) -> Result<(), DurableError> {
    let tmp = temp_sibling(path);
    write_new_file(&tmp, contents)?;
    if path.symlink_metadata().is_ok() {
        let _ = std::fs::remove_file(&tmp);
        return Err(DurableError::Win32 {
            path: path.display().to_string(),
            message: "destination already exists".into(),
        });
    }
    if let Err(e) = publish_replace(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// Durably move `from` → `to` (same volume), replacing `to` if present.
pub fn move_durable(from: &Path, to: &Path) -> Result<(), DurableError> {
    publish_replace(from, to)
}

/// Copy a file to a new path and durably publish it without replacing an
/// existing artifact. The temporary file is a sibling so publication stays
/// on the destination volume.
pub fn copy_new_durable(source: &Path, destination: &Path) -> Result<(), DurableError> {
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent).map_err(|source| DurableError::Io {
            path: parent.display().to_string(),
            source,
        })?;
    }
    if destination.exists() {
        return Err(DurableError::Win32 {
            path: destination.display().to_string(),
            message: "destination already exists".into(),
        });
    }
    let name = destination
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file");
    let temp = destination.with_file_name(format!(".{name}.zup-tmp-{}", std::process::id()));
    let result = (|| {
        let mut input = std::fs::File::open(source).map_err(|error| DurableError::Io {
            path: source.display().to_string(),
            source: error,
        })?;
        let mut output = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .map_err(|source| DurableError::Io {
                path: temp.display().to_string(),
                source,
            })?;
        std::io::copy(&mut input, &mut output).map_err(|source| DurableError::Io {
            path: temp.display().to_string(),
            source,
        })?;
        output.sync_all().map_err(|source| DurableError::Io {
            path: temp.display().to_string(),
            source,
        })?;
        if destination.exists() {
            return Err(DurableError::Win32 {
                path: destination.display().to_string(),
                message: "destination already exists".into(),
            });
        }
        publish_new(&temp, destination)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

fn temp_sibling(path: &Path) -> PathBuf {
    let sequence = DURABLE_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    path.with_file_name(format!(".zup-{:x}-{sequence:x}", std::process::id()))
}

fn write_new_file(path: &Path, contents: &[u8]) -> Result<(), DurableError> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(|source| DurableError::Io {
            path: parent.display().to_string(),
            source,
        })?;
    }

    let wide = to_wide_path(path);
    // SAFETY: `wide` is a valid NUL-terminated UTF-16 string.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_WRITE,
            0,
            std::ptr::null_mut(),
            CREATE_NEW,
            fs_bindings::FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(win32_err(path, "CreateFileW"));
    }

    let result = (|| {
        write_all(handle, contents)?;
        flush(handle)
    })();
    // SAFETY: handle opened above; closed exactly once.
    unsafe {
        CloseHandle(handle);
    }
    result
}

fn write_all(handle: HANDLE, contents: &[u8]) -> Result<(), DurableError> {
    use std::os::windows::io::{FromRawHandle, IntoRawHandle};
    // SAFETY: borrow the raw handle for the write; we keep ownership.
    let mut file = unsafe { std::fs::File::from_raw_handle(handle) };
    let result =
        std::io::Write::write_all(&mut file, contents).map_err(|source| DurableError::Io {
            path: "<write>".into(),
            source,
        });
    let _ = file.into_raw_handle();
    result
}

fn flush(handle: HANDLE) -> Result<(), DurableError> {
    // SAFETY: handle is a valid open file handle.
    let ok = unsafe { FlushFileBuffers(handle) };
    if ok == 0 {
        return Err(DurableError::Win32 {
            path: "<flush>".into(),
            message: format!("FlushFileBuffers failed ({})", unsafe { GetLastError() }),
        });
    }
    Ok(())
}

fn publish_replace(from: &Path, to: &Path) -> Result<(), DurableError> {
    let from_w = to_wide_path(from);
    let to_w = to_wide_path(to);
    // SAFETY: both paths are valid NUL-terminated UTF-16 strings.
    let ok = unsafe {
        MoveFileExW(
            from_w.as_ptr(),
            to_w.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if ok == 0 {
        return Err(DurableError::Win32 {
            path: to.display().to_string(),
            message: format!("MoveFileExW failed ({})", unsafe { GetLastError() }),
        });
    }
    Ok(())
}

fn publish_new(from: &Path, to: &Path) -> Result<(), DurableError> {
    let from_w = to_wide_path(from);
    let to_w = to_wide_path(to);
    // SAFETY: both paths are valid NUL-terminated UTF-16 strings.
    let ok = unsafe { MoveFileExW(from_w.as_ptr(), to_w.as_ptr(), MOVEFILE_WRITE_THROUGH) };
    if ok == 0 {
        return Err(DurableError::Win32 {
            path: to.display().to_string(),
            message: format!("MoveFileExW failed ({})", unsafe { GetLastError() }),
        });
    }
    Ok(())
}

/// Resolve the volume root backing `path` (same-volume staging).
pub fn volume_root(path: &Path) -> Result<PathBuf, DurableError> {
    let wide_in = to_wide(&path.display().to_string());
    let mut wide_out = vec![0u16; 512];
    // SAFETY: buffers are valid.
    let ok = unsafe {
        GetVolumePathNameW(
            wide_in.as_ptr(),
            wide_out.as_mut_ptr(),
            wide_out.len() as u32,
        )
    };
    if ok == 0 {
        return Err(DurableError::Win32 {
            path: path.display().to_string(),
            message: format!("GetVolumePathNameW failed ({})", unsafe { GetLastError() }),
        });
    }
    let len = wide_out.iter().position(|&c| c == 0).unwrap_or(0);
    let s = String::from_utf16_lossy(&wide_out[..len]);
    Ok(PathBuf::from(s))
}

/// Single-writer installation lock (cooperating processes only).
///
/// Uses `std::fs::File::try_lock` so two sessions in one process also exclude
/// each other. This is coordination, not a security boundary: it stops two
/// lifecycle operations from mutating one installation's ledger and work
/// directory at the same time, and it says nothing about an adversary.
///
/// # What the identity is, and what it deliberately is not
///
/// The key is `(application, scope)`. Two installs of different applications do
/// not block each other, and a user-scope and a machine-scope install of the same
/// application do not either — they are different installations with different
/// ledgers, different install directories, and different uninstall entries, and
/// serialising them would make an unrelated second install wait for no reason.
///
/// It is *not* keyed by target or by version. Those are properties of one
/// operation, not of the installation, and a key that changed as a plan changed
/// would let two operations hold "the" lock for the same installation at once.
///
/// A crash releases the lock through the OS's handle lifetime, so there is no
/// stale-PID cleanup to get wrong and no window where a dead process's lock
/// outlives it.
#[derive(Debug)]
pub struct InstallationLock {
    file: std::fs::File,
    key: String,
}

/// What one installation's lock is for.
///
/// A value rather than two format strings, because the key is written in four
/// places — a parent session, an elevated worker, a bootstrap phase, and an
/// uninstall — and four spellings of one lock key is four chances for a parent
/// and its worker to disagree about which installation they are serializing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockScope {
    /// A lifecycle operation on an installed application.
    Lifecycle,
    /// A prerequisite bootstrap, which happens before an application exists.
    Bootstrap,
}

impl LockScope {
    /// The prefix that keeps a bootstrap lock from being mistaken for a
    /// lifecycle one on the same installation.
    const fn prefix(self) -> &'static str {
        match self {
            Self::Lifecycle => "zup-install",
            Self::Bootstrap => "zup-bootstrap",
        }
    }
}

impl InstallationLock {
    /// Lock identity for one installation.
    pub fn lock_key(app_id: &str, scope: &str) -> String {
        format!("zup-install-{}-{}", sanitize(app_id), sanitize(scope))
    }

    /// The lock for one installation and one kind of operation.
    ///
    /// This is the only place a lifecycle lock key is spelled, and the reason the
    /// bootstrap and transaction paths can be checked against each other: a parent
    /// and the worker it elevates call this with the same arguments and get the
    /// same file, on the same volume, whether or not either of them knows the
    /// other's existence.
    pub fn key_for(app_id: &str, scope: &str, kind: LockScope) -> String {
        match kind {
            LockScope::Lifecycle => Self::lock_key(app_id, scope),
            LockScope::Bootstrap => {
                format!("{}-{}-{}", kind.prefix(), sanitize(app_id), sanitize(scope))
            }
        }
    }

    /// The scope token a `SelectedScope` contributes to the key.
    pub fn scope_token(scope: &str) -> String {
        sanitize(scope)
    }

    /// Try to acquire the named lock; `Ok(None)` means another session holds it.
    ///
    /// The error names the state root rather than the lock file. The root is a
    /// directory a user can find; the file inside it is an implementation detail,
    /// and a message about an implementation detail sends people looking in the
    /// wrong place.
    pub fn try_acquire(state_root: &Path, key: &str) -> Result<Option<Self>, DurableError> {
        std::fs::create_dir_all(state_root).map_err(|source| DurableError::Io {
            path: state_root.display().to_string(),
            source,
        })?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(state_root.join(format!("{key}.lock")))
            .map_err(|source| DurableError::Io {
                path: state_root.display().to_string(),
                source,
            })?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self {
                file,
                key: key.to_owned(),
            })),
            Err(_) => Ok(None),
        }
    }

    /// Remove the lock marker after uninstall when no cooperating process
    /// currently holds it. The file is deleted while its byte-range lock is
    /// held so a new installer cannot race the cleanup.
    pub fn remove_if_unheld(state_root: &Path, key: &str) -> Result<(), DurableError> {
        let Some(lock) = Self::try_acquire(state_root, key)? else {
            return Ok(());
        };
        let path = state_root.join(format!("{key}.lock"));
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(DurableError::Io {
                    path: path.display().to_string(),
                    source,
                });
            }
        }
        drop(lock);
        Ok(())
    }

    /// The key this lock was taken under.
    pub fn key(&self) -> &str {
        &self.key
    }
}

impl Drop for InstallationLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn to_wide_path(path: &Path) -> Vec<u16> {
    let value = path.to_string_lossy().replace('/', "\\");
    let value = if value.starts_with(r"\\?\") {
        value
    } else if let Some(value) = value.strip_prefix(r"\\") {
        format!(r"\\?\UNC\{value}")
    } else if path.is_absolute() {
        format!(r"\\?\{value}")
    } else {
        value
    };
    to_wide(&value)
}

fn win32_err(path: &Path, api: &str) -> DurableError {
    DurableError::Win32 {
        path: path.display().to_string(),
        message: format!("{api} failed ({})", unsafe { GetLastError() }),
    }
}
