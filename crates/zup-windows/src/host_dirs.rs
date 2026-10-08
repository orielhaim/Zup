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

pub fn user_data() -> Result<PathBuf, HostDirError> {
    BaseDirs::new()
        .map(|dirs| dirs.data_local_dir().to_path_buf())
        .ok_or(HostDirError::Unreported("a user data directory"))
}

pub fn user_desktop() -> Result<PathBuf, HostDirError> {
    UserDirs::new()
        .and_then(|dirs| dirs.desktop_dir().map(Path::to_path_buf))
        .ok_or(HostDirError::Unreported("a desktop directory"))
}

pub fn shared_data() -> Result<PathBuf, HostDirError> {
    known_folder(FOLDERID_ProgramData)
}

#[derive(Debug, thiserror::Error)]
pub enum HostDirError {
    #[error("this machine reports no {0}")]
    Unreported(&'static str),

    #[error("the known folder could not be read: HRESULT({hr:#x})")]
    HResult { hr: i32 },

    #[error("the known folder resolved to no path")]
    Unnamed,
}

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
