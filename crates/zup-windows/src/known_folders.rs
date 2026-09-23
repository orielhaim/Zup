//! Windows Known Folder resolution via `SHGetKnownFolderPath`.

use std::path::PathBuf;

use zup_core::SelectedScope;
use zup_platform::{KnownFolder, KnownFolderError, KnownFolderResolver};

use crate::bindings::{
    CoTaskMemFree, FOLDERID_CommonPrograms, FOLDERID_CommonStartMenu, FOLDERID_Desktop,
    FOLDERID_LocalAppData, FOLDERID_ProgramData, FOLDERID_ProgramFiles, FOLDERID_Programs,
    FOLDERID_PublicDesktop, FOLDERID_StartMenu, GUID, SHGetKnownFolderPath,
};

/// Production known-folder resolver backed by the Win32 Known Folder API.
#[derive(Debug, Default, Clone, Copy)]
pub struct WindowsKnownFolderResolver;

impl KnownFolderResolver for WindowsKnownFolderResolver {
    fn resolve(
        &self,
        folder: KnownFolder,
        scope: SelectedScope,
    ) -> Result<PathBuf, KnownFolderError> {
        let guid = match (folder, scope) {
            (KnownFolder::ProgramFiles, _) => FOLDERID_ProgramFiles,
            (KnownFolder::LocalAppData, _) => FOLDERID_LocalAppData,
            (KnownFolder::ProgramData, _) => FOLDERID_ProgramData,
            (KnownFolder::StartMenu, SelectedScope::User) => FOLDERID_StartMenu,
            (KnownFolder::StartMenu, SelectedScope::Machine) => FOLDERID_CommonStartMenu,
            (KnownFolder::Desktop, SelectedScope::User) => FOLDERID_Desktop,
            (KnownFolder::Desktop, SelectedScope::Machine) => FOLDERID_PublicDesktop,
            (KnownFolder::Programs, SelectedScope::User) => FOLDERID_Programs,
            (KnownFolder::Programs, SelectedScope::Machine) => FOLDERID_CommonPrograms,
        };

        known_folder_path(guid).map_err(|source| KnownFolderError::ResolutionFailed {
            folder,
            scope,
            source: source.into(),
        })
    }
}

fn known_folder_path(guid: GUID) -> Result<PathBuf, WindowsKnownFolderError> {
    let mut path_ptr: *mut u16 = std::ptr::null_mut();

    // SAFETY: `guid` is a valid KNOWNFOLDERID. On success `path_ptr` is a
    // CoTaskMemAlloc'd UTF-16 string we must free exactly once.
    unsafe {
        let hr = SHGetKnownFolderPath(&guid, 0, std::ptr::null_mut(), &mut path_ptr);
        if hr < 0 {
            return Err(WindowsKnownFolderError::HResult(hr));
        }
        if path_ptr.is_null() {
            return Err(WindowsKnownFolderError::NullPath);
        }

        let path = utf16_to_string(path_ptr);
        CoTaskMemFree(path_ptr.cast());

        let path = path.map_err(|_| WindowsKnownFolderError::InvalidUtf16)?;
        if path.is_empty() {
            return Err(WindowsKnownFolderError::EmptyPath);
        }
        Ok(PathBuf::from(path))
    }
}

/// Copy a NUL-terminated UTF-16 string into a Rust `String`.
///
/// # Safety
/// `ptr` must be a valid NUL-terminated UTF-16 buffer.
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

/// Low-level known-folder failure (never part of the public API surface).
#[derive(Debug)]
enum WindowsKnownFolderError {
    HResult(i32),
    NullPath,
    EmptyPath,
    InvalidUtf16,
}

impl std::fmt::Display for WindowsKnownFolderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::HResult(hr) => write!(f, "SHGetKnownFolderPath failed with HRESULT({hr:#x})"),
            Self::NullPath => f.write_str("SHGetKnownFolderPath returned a null path"),
            Self::EmptyPath => f.write_str("SHGetKnownFolderPath returned an empty path"),
            Self::InvalidUtf16 => f.write_str("SHGetKnownFolderPath returned invalid UTF-16"),
        }
    }
}

impl std::error::Error for WindowsKnownFolderError {}
