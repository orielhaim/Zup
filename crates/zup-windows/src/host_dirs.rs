//! Where things are on this machine.
//!
//! Two questions live here and they are not the same question.
//!
//! A *standard host directory* - the per-user data root, the desktop - is a
//! convention the operating system publishes on every platform, and `directories`
//! is what publishes it. Nothing here re-derives one.
//!
//! An *install location* - Program Files, ProgramData, the Start Menu, the
//! public desktop - is Windows installation policy rather than a convention, so
//! it is read through the Known Folder API. No cross-platform crate models it,
//! because a machine-wide data root and a Start Menu are not portable concepts.

use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};

use directories::{BaseDirs, UserDirs};
use zup_core::{InstallLocation, SelectedScope, TargetTriple};
use zup_platform::{InstallLocationError, InstallLocationResolver, TargetPath};

use crate::bindings::{
    CoTaskMemFree, FOLDERID_CommonStartMenu, FOLDERID_ProgramData, FOLDERID_ProgramFiles,
    FOLDERID_PublicDesktop, FOLDERID_StartMenu, GUID, SHGetKnownFolderPath,
};

/// The per-user data root: where this user's own application data belongs.
///
/// Also the base of the user scope's state root, so an installed application's
/// data directory and zup's own state agree about where "this user's data" is.
pub fn user_data() -> Result<PathBuf, HostDirError> {
    BaseDirs::new()
        .map(|dirs| dirs.data_local_dir().to_path_buf())
        .ok_or(HostDirError::Unreported("a user data directory"))
}

/// The desktop of the user running this process.
pub fn user_desktop() -> Result<PathBuf, HostDirError> {
    UserDirs::new()
        .and_then(|dirs| dirs.desktop_dir().map(Path::to_path_buf))
        .ok_or(HostDirError::Unreported("a desktop directory"))
}

/// The machine-wide data root every installed user shares.
///
/// Not a standard host directory: it outlives any one user's account, so writing
/// into it is the elevated worker's job and a per-user tool has no business
/// creating it.
pub fn shared_data() -> Result<PathBuf, HostDirError> {
    known_folder(FOLDERID_ProgramData)
}

/// Why this machine would not name a directory.
#[derive(Debug, thiserror::Error)]
pub enum HostDirError {
    /// The operating system reported no such directory.
    #[error("this machine reports no {0}")]
    Unreported(&'static str),

    /// The Known Folder API refused to answer.
    #[error("the known folder could not be read: HRESULT({hr:#x})")]
    HResult { hr: i32 },

    /// A successful call that named no path, which the API contract forbids.
    #[error("the known folder resolved to no path")]
    Unnamed,
}

/// Maps a semantic install location onto a concrete path on one target.
#[derive(Debug, Default, Clone, Copy)]
pub struct WindowsInstallLocationResolver;

impl InstallLocationResolver for WindowsInstallLocationResolver {
    fn resolve(
        &self,
        location: InstallLocation,
        scope: SelectedScope,
        target: &TargetTriple,
    ) -> Result<TargetPath, InstallLocationError> {
        let failed = |source: HostDirError| InstallLocationError::ResolutionFailed {
            location,
            scope,
            source: Box::new(source),
        };
        let path = match (location, scope) {
            (InstallLocation::UserData, _) => user_data().map_err(failed)?,
            (InstallLocation::Desktop, SelectedScope::User) => user_desktop().map_err(failed)?,
            (InstallLocation::Programs, _) => {
                known_folder(FOLDERID_ProgramFiles).map_err(failed)?
            }
            (InstallLocation::SharedData, _) => shared_data().map_err(failed)?,
            (InstallLocation::Menu, SelectedScope::User) => {
                known_folder(FOLDERID_StartMenu).map_err(failed)?
            }
            (InstallLocation::Menu, SelectedScope::Machine) => {
                known_folder(FOLDERID_CommonStartMenu).map_err(failed)?
            }
            (InstallLocation::Desktop, SelectedScope::Machine) => {
                known_folder(FOLDERID_PublicDesktop).map_err(failed)?
            }
        };
        TargetPath::new(target, path.to_string_lossy()).map_err(|error| {
            InstallLocationError::ResolutionFailed {
                location,
                scope,
                source: Box::new(error),
            }
        })
    }
}

/// The path a Known Folder names, or why it named none.
///
/// A `CoTaskMem` allocation is only ever released here, so the path is copied
/// out and the original freed before anything else can fail.
fn known_folder(guid: GUID) -> Result<PathBuf, HostDirError> {
    let mut found: *mut u16 = std::ptr::null_mut();
    // SAFETY: the last argument is a valid writable pointer to a `*mut u16`.
    let outcome = unsafe { SHGetKnownFolderPath(&guid, 0, std::ptr::null_mut(), &mut found) };
    if outcome < 0 {
        return Err(HostDirError::HResult { hr: outcome });
    }
    if found.is_null() {
        return Err(HostDirError::Unnamed);
    }
    // A path is not text: a component can hold a character that is not valid
    // UTF-8, so it is widened rather than decoded, and a decode failure would
    // reject a directory that exists.
    // SAFETY: a successful call hands back a NUL-terminated UTF-16 string.
    let path = unsafe {
        let wide = std::slice::from_raw_parts(found, nul_terminated_length(found));
        let path = PathBuf::from(OsString::from_wide(wide));
        CoTaskMemFree(found.cast());
        path
    };
    Ok(path)
}

/// SAFETY: `ptr` must be a NUL-terminated UTF-16 string.
unsafe fn nul_terminated_length(ptr: *const u16) -> usize {
    let mut length = 0usize;
    // SAFETY: the terminator bounds the scan.
    while unsafe { *ptr.add(length) } != 0 {
        length += 1;
    }
    length
}
