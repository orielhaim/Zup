use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::bindings::{
    self, CREATE_NEW, CloseHandle, ConvertStringSecurityDescriptorToSecurityDescriptorW,
    CreateFileW, DACL_SECURITY_INFORMATION, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_OPEN_REPARSE_POINT,
    FlushFileBuffers, GENERIC_WRITE, GetLastError, INVALID_HANDLE_VALUE, LocalFree,
    SDDL_REVISION_1, SetFileSecurityW,
};

static SESSION_SEQUENCE: AtomicU64 = AtomicU64::new(1);

pub struct StagedPreset {
    pub root: PathBuf,
    pub executable: PathBuf,
    pub assets: BTreeMap<String, String>,
}

pub fn stage_session(
    executable_name: &str,
    files: &[(&str, &[u8])],
) -> Result<StagedPreset, String> {
    let root = private_root()?;
    let outcome = (|| {
        let mut executable = None;
        let mut assets = BTreeMap::new();
        for (name, bytes) in files {
            let path = place(&root, name, bytes)?;
            zup_preset_host::confirm_bytes(&path, bytes).map_err(|error| error.to_string())?;
            if *name == executable_name {
                executable = Some(path);
            } else {
                assets.insert((*name).to_owned(), crate::plain_path_text(&path));
            }
        }
        let executable =
            executable.ok_or_else(|| "the staged preset has no executable".to_owned())?;
        Ok(StagedPreset {
            root: root.clone(),
            executable,
            assets,
        })
    })();
    if outcome.is_err() {
        let _ = std::fs::remove_dir_all(&root);
    }
    outcome
}

fn private_root() -> Result<PathBuf, String> {
    let parent = std::env::temp_dir();
    if crate::bindings::is_reparse_point(&parent) {
        return Err("the preset staging parent is a reparse point".into());
    }
    let sequence = SESSION_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let root = parent.join(format!(
        "zup-preset-{:x}-{:x}-{:x}",
        std::process::id(),
        nanos,
        sequence
    ));
    if root.symlink_metadata().is_ok() {
        return Err("the preset staging directory already exists".into());
    }
    std::fs::create_dir(&root).map_err(|error| error.to_string())?;
    if let Err(error) = protect_directory(&root) {
        let _ = std::fs::remove_dir(&root);
        return Err(error);
    }
    if bindings::is_reparse_point(&root) {
        let _ = std::fs::remove_dir(&root);
        return Err("the preset staging directory is a reparse point".into());
    }
    Ok(root)
}

fn place(root: &Path, name: &str, bytes: &[u8]) -> Result<PathBuf, String> {
    let relative = std::path::Path::new(name);
    if relative.is_absolute()
        || relative.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir | std::path::Component::RootDir
            )
        })
    {
        return Err(format!(
            "preset asset `{name}` leaves the staging directory"
        ));
    }
    let destination = root.join(relative);
    if let Some(parent) = destination.parent().filter(|parent| *parent != root) {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        protect_directory(parent)?;
    }
    if destination.symlink_metadata().is_ok() || bindings::is_reparse_point(&destination) {
        return Err(format!(
            "preset asset `{name}` already occupies the staging path"
        ));
    }
    write_exclusive(&destination, bytes)?;
    if bindings::is_reparse_point(&destination) {
        let _ = std::fs::remove_file(&destination);
        return Err(format!("preset asset `{name}` was redirected"));
    }
    Ok(destination)
}

fn write_exclusive(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let wide = wide_path(path);
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_WRITE,
            0,
            std::ptr::null_mut(),
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(format!("create `{}`: {}", path.display(), unsafe {
            GetLastError()
        }));
    }
    let result = (|| {
        use std::io::Write as _;
        use std::os::windows::io::{FromRawHandle, IntoRawHandle};
        let mut file = unsafe { std::fs::File::from_raw_handle(handle) };
        let write = file.write_all(bytes).map_err(|error| error.to_string());
        let _ = file.into_raw_handle();
        write?;
        if unsafe { FlushFileBuffers(handle) } == 0 {
            return Err(format!("flush `{}`", path.display()));
        }
        Ok(())
    })();
    unsafe {
        CloseHandle(handle);
    }
    result
}

fn protect_directory(path: &Path) -> Result<(), String> {
    let descriptor = current_user_descriptor()?;
    let wide = wide_path(path);
    let ok = unsafe { SetFileSecurityW(wide.as_ptr(), DACL_SECURITY_INFORMATION, descriptor) };
    unsafe {
        LocalFree(descriptor as bindings::Handle);
    }
    if ok == 0 {
        return Err(format!("restrict `{}`: {}", path.display(), unsafe {
            GetLastError()
        }));
    }
    Ok(())
}

fn current_user_descriptor() -> Result<*mut std::ffi::c_void, String> {
    let sddl: Vec<u16> = "D:P(A;;FA;;;OW)"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut descriptor = std::ptr::null_mut();
    let ok = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 || descriptor.is_null() {
        return Err("the preset staging descriptor could not be built".into());
    }
    Ok(descriptor)
}

fn wide_path(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}
