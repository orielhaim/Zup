use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use thiserror::Error;
use typed_path::{
    Utf8Component, Utf8WindowsPath, Utf8WindowsPrefix, constants::windows::SEPARATOR_STR,
};

use crate::bindings::{
    self, CREATE_NEW, CloseHandle, CreateFileW, FlushFileBuffers, GENERIC_WRITE, GetLastError,
    GetVolumePathNameW, HANDLE, INVALID_HANDLE_VALUE, MOVEFILE_REPLACE_EXISTING,
    MOVEFILE_WRITE_THROUGH, MoveFileExW,
};

static DURABLE_TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

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

pub fn write_durable(path: &Path, contents: &[u8]) -> Result<(), DurableError> {
    let tmp = temp_sibling(path);
    write_new_file(&tmp, contents)?;
    if let Err(e) = publish_replace(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

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

pub fn move_durable(from: &Path, to: &Path) -> Result<(), DurableError> {
    publish_replace(from, to)
}

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
            bindings::FILE_ATTRIBUTE_NORMAL,
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

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn to_wide_path(path: &Path) -> Vec<u16> {
    let text = path.to_string_lossy();
    let lexical = Utf8WindowsPath::new(&text);
    let verbatim = match lexical.components().prefix_kind() {
        Some(prefix) if prefix.is_verbatim() => text.into_owned(),
        Some(prefix) => verbatim_prefix(prefix)
            .map(|prefix| verbatim_spelling(&prefix, lexical))
            .unwrap_or_else(|_| text.into_owned()),

        None if lexical.has_root() => verbatim_spelling("\\\\?\\", lexical),
        None => text.into_owned(),
    };
    to_wide(&verbatim)
}

fn verbatim_prefix(prefix: Utf8WindowsPrefix<'_>) -> Result<String, &'static str> {
    match prefix {
        Utf8WindowsPrefix::Disk(drive) => Ok(format!("\\\\?\\{drive}:")),
        Utf8WindowsPrefix::UNC(server, share) => Ok(format!("\\\\?\\UNC\\{server}\\{share}")),
        _ => Err("path is not rooted at a drive or a network share"),
    }
}

fn verbatim_spelling(prefix: &str, path: &Utf8WindowsPath) -> String {
    let mut text = prefix.to_owned();
    for component in path.components() {
        if component.is_normal() {
            text.push_str(SEPARATOR_STR);
            text.push_str(component.as_str());
        }
    }
    text
}

fn win32_err(path: &Path, api: &str) -> DurableError {
    DurableError::Win32 {
        path: path.display().to_string(),
        message: format!("{api} failed ({})", unsafe { GetLastError() }),
    }
}
