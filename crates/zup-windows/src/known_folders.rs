use zup_core::{InstallLocation, SelectedScope, TargetTriple};
use zup_platform::{InstallLocationError, InstallLocationResolver, TargetPath};

use crate::bindings::{
    CoTaskMemFree, FOLDERID_CommonStartMenu, FOLDERID_Desktop, FOLDERID_LocalAppData,
    FOLDERID_ProgramData, FOLDERID_ProgramFiles, FOLDERID_PublicDesktop, FOLDERID_StartMenu, GUID,
    SHGetKnownFolderPath,
};

#[derive(Debug, Default, Clone, Copy)]
pub struct WindowsInstallLocationResolver;

impl InstallLocationResolver for WindowsInstallLocationResolver {
    fn resolve(
        &self,
        location: InstallLocation,
        scope: SelectedScope,
        target: &TargetTriple,
    ) -> Result<TargetPath, InstallLocationError> {
        let failed = |source: Box<dyn std::error::Error + Send + Sync + 'static>| {
            InstallLocationError::ResolutionFailed {
                location,
                scope,
                source,
            }
        };
        let guid = folder_guid(location, scope);
        let path = known_folder_path(guid).map_err(|source| failed(source.into()))?;
        TargetPath::new(target, &path).map_err(|source| failed(source.into()))
    }
}

fn folder_guid(location: InstallLocation, scope: SelectedScope) -> GUID {
    match (location, scope) {
        (InstallLocation::Programs, _) => FOLDERID_ProgramFiles,
        (InstallLocation::UserData, _) => FOLDERID_LocalAppData,
        (InstallLocation::SharedData, _) => FOLDERID_ProgramData,
        (InstallLocation::Menu, SelectedScope::User) => FOLDERID_StartMenu,
        (InstallLocation::Menu, SelectedScope::Machine) => FOLDERID_CommonStartMenu,
        (InstallLocation::Desktop, SelectedScope::User) => FOLDERID_Desktop,
        (InstallLocation::Desktop, SelectedScope::Machine) => FOLDERID_PublicDesktop,
    }
}

fn known_folder_path(guid: GUID) -> Result<String, WindowsInstallLocationError> {
    let mut path_ptr: *mut u16 = std::ptr::null_mut();

    unsafe {
        let hr = SHGetKnownFolderPath(&guid, 0, std::ptr::null_mut(), &mut path_ptr);
        if hr < 0 {
            return Err(WindowsInstallLocationError::HResult(hr));
        }
        if path_ptr.is_null() {
            return Err(WindowsInstallLocationError::NullPath);
        }

        let path = utf16_to_string(path_ptr);
        CoTaskMemFree(path_ptr.cast());

        let path = path.map_err(|_| WindowsInstallLocationError::InvalidUtf16)?;
        if path.is_empty() {
            return Err(WindowsInstallLocationError::EmptyPath);
        }
        if path.contains('\0') {
            return Err(WindowsInstallLocationError::InvalidPath);
        }
        Ok(path)
    }
}

unsafe fn utf16_to_string(ptr: *const u16) -> Result<String, ()> {
    unsafe {
        let mut len = 0usize;
        while *ptr.add(len) != 0 {
            len += 1;
        }
        let slice = std::slice::from_raw_parts(ptr, len);
        String::from_utf16(slice).map_err(|_| ())
    }
}

#[derive(Debug)]
enum WindowsInstallLocationError {
    HResult(i32),
    NullPath,
    EmptyPath,
    InvalidUtf16,
    InvalidPath,
}

impl std::fmt::Display for WindowsInstallLocationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::HResult(hr) => write!(f, "SHGetKnownFolderPath failed with HRESULT({hr:#x})"),
            Self::NullPath => f.write_str("SHGetKnownFolderPath returned a null path"),
            Self::EmptyPath => f.write_str("SHGetKnownFolderPath returned an empty path"),
            Self::InvalidUtf16 => f.write_str("SHGetKnownFolderPath returned invalid UTF-16"),
            Self::InvalidPath => f.write_str("SHGetKnownFolderPath returned an invalid path"),
        }
    }
}

impl std::error::Error for WindowsInstallLocationError {}
