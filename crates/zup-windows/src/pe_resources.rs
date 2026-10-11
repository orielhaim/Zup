use std::path::Path;

use windows_link::link;
use zup_pe::{
    MAX_RESOURCE_ID, MAX_RESOURCE_SIZE, PeError, RESOURCE_ID_INDEX, RESOURCE_TYPE_RCDATA,
    ResourceDocument, check_documents,
};

type Handle = *mut core::ffi::c_void;
type Bool = i32;
type Dword = u32;

link!("kernel32.dll" "system" fn BeginUpdateResourceW(filename: *const u16, delete_existing: Bool) -> Handle);
link!("kernel32.dll" "system" fn UpdateResourceW(update: Handle, resource_type: *const u16, name: *const u16, language: u16, data: *const core::ffi::c_void, size: Dword) -> Bool);
link!("kernel32.dll" "system" fn EndUpdateResourceW(update: Handle, discard: Bool) -> Bool);
link!("kernel32.dll" "system" fn LoadLibraryExW(filename: *const u16, file: Handle, flags: Dword) -> Handle);
link!("kernel32.dll" "system" fn FindResourceW(module: Handle, name: *const u16, resource_type: *const u16) -> Handle);
link!("kernel32.dll" "system" fn LoadResource(module: Handle, resource: Handle) -> Handle);
link!("kernel32.dll" "system" fn SizeofResource(module: Handle, resource: Handle) -> Dword);
link!("kernel32.dll" "system" fn LockResource(resource: Handle) -> *const core::ffi::c_void);
link!("kernel32.dll" "system" fn FreeLibrary(module: Handle) -> i32);
link!("kernel32.dll" "system" fn GetLastError() -> Dword);

const LOAD_LIBRARY_AS_DATAFILE: Dword = 0x0000_0002;

pub fn write_resources(
    executable: &Path,
    output: &Path,
    documents: &[ResourceDocument],
) -> Result<(), ResourceError> {
    use std::os::windows::ffi::OsStrExt;

    check_documents(documents, RESOURCE_ID_INDEX).map_err(ResourceError::Pe)?;
    if output.exists() || output == executable {
        return Err(ResourceError::Missing);
    }
    std::fs::copy(executable, output).map_err(|_| ResourceError::Missing)?;
    let wide: Vec<u16> = output.as_os_str().encode_wide().chain(Some(0)).collect();
    let update = unsafe { BeginUpdateResourceW(wide.as_ptr(), 0) };
    if update.is_null() {
        let _ = std::fs::remove_file(output);
        return Err(ResourceError::Api(unsafe { GetLastError() }));
    }
    let result = (|| {
        for document in documents {
            let size = u32::try_from(document.bytes.len()).map_err(|_| 87u32)?;
            let ok = unsafe {
                UpdateResourceW(
                    update,
                    RESOURCE_TYPE_RCDATA as *const u16,
                    document.id as *const u16,
                    0,
                    document.bytes.as_ptr().cast(),
                    size,
                )
            };
            if ok == 0 {
                return Err(unsafe { GetLastError() });
            }
        }
        Ok(())
    })();
    if let Err(error) = result {
        unsafe {
            EndUpdateResourceW(update, 1);
        }
        let _ = std::fs::remove_file(output);
        return Err(ResourceError::Api(error));
    }
    if unsafe { EndUpdateResourceW(update, 0) } == 0 {
        let error = unsafe { GetLastError() };
        let _ = std::fs::remove_file(output);
        return Err(ResourceError::Api(error));
    }
    Ok(())
}

pub fn read_resource(path: &Path, id: usize) -> Result<Vec<u8>, ResourceError> {
    use std::{os::windows::ffi::OsStrExt, ptr};

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let module =
        unsafe { LoadLibraryExW(wide.as_ptr(), ptr::null_mut(), LOAD_LIBRARY_AS_DATAFILE) };
    if module.is_null() {
        return Err(ResourceError::Api(unsafe { GetLastError() }));
    }
    let resource =
        unsafe { FindResourceW(module, id as *const u16, RESOURCE_TYPE_RCDATA as *const u16) };
    let result = if resource.is_null() {
        Err(ResourceError::Absent(id))
    } else {
        let size = unsafe { SizeofResource(module, resource) } as usize;
        if size == 0 {
            Err(ResourceError::Absent(id))
        } else {
            let loaded = unsafe { LoadResource(module, resource) };
            let data = if loaded.is_null() {
                ptr::null()
            } else {
                unsafe { LockResource(loaded) }
            };
            if data.is_null() {
                Err(ResourceError::Api(unsafe { GetLastError() }))
            } else {
                let mut bytes = Vec::new();
                bytes
                    .try_reserve_exact(size)
                    .map_err(|_| ResourceError::Absent(id))?;
                bytes.extend_from_slice(unsafe {
                    std::slice::from_raw_parts(data.cast::<u8>(), size)
                });
                if bytes.len() as u64 > MAX_RESOURCE_SIZE || id > MAX_RESOURCE_ID {
                    Err(ResourceError::Absent(id))
                } else {
                    Ok(bytes)
                }
            }
        }
    };
    unsafe {
        FreeLibrary(module);
    }
    result
}

const ICON_RESOURCE: u16 = 3;

const ICON_GROUP_RESOURCE: u16 = 14;

pub fn apply_icon(
    executable: &Path,
    images: &[Vec<u8>],
    group: &[u8],
) -> Result<(), ResourceError> {
    use std::os::windows::ffi::OsStrExt;

    if images.is_empty() || group.is_empty() {
        return Err(ResourceError::Missing);
    }
    if images.len() > MAX_RESOURCE_ID {
        return Err(ResourceError::Pe(PeError::TooManyResources {
            count: images.len(),
            limit: MAX_RESOURCE_ID,
        }));
    }
    let wide: Vec<u16> = executable
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let update = unsafe { BeginUpdateResourceW(wide.as_ptr(), 0) };
    if update.is_null() {
        return Err(ResourceError::Api(unsafe { GetLastError() }));
    }
    let result = (|| {
        for (index, image) in images.iter().enumerate() {
            write_typed(update, ICON_RESOURCE, index + 1, image)?;
        }
        write_typed(update, ICON_GROUP_RESOURCE, 1, group)?;
        Ok(())
    })();
    if let Err(error) = result {
        unsafe {
            EndUpdateResourceW(update, 1);
        }
        return Err(error);
    }
    if unsafe { EndUpdateResourceW(update, 0) } == 0 {
        return Err(ResourceError::Api(unsafe { GetLastError() }));
    }
    Ok(())
}

fn write_typed(update: Handle, kind: u16, id: usize, bytes: &[u8]) -> Result<(), ResourceError> {
    let size = u32::try_from(bytes.len()).map_err(|_| ResourceError::Api(87))?;
    if u64::from(size) > MAX_RESOURCE_SIZE || id > MAX_RESOURCE_ID {
        return Err(ResourceError::Pe(PeError::ResourceTooLarge {
            size: bytes.len() as u64,
            limit: MAX_RESOURCE_SIZE,
        }));
    }
    let ok = unsafe {
        UpdateResourceW(
            update,
            kind as *const u16,
            id as *const u16,
            0,
            bytes.as_ptr().cast(),
            size,
        )
    };
    if ok == 0 {
        return Err(ResourceError::Api(unsafe { GetLastError() }));
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum ResourceError {
    #[error("executable has no resource {0}")]
    Absent(usize),
    #[error("the resource API failed with error {0}")]
    Api(u32),
    #[error("image resources: {0}")]
    Pe(#[from] PeError),
    #[error("the artifact is already present, or is the source of the copy")]
    Missing,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unwritable_document_set_is_refused() {
        let document = |id: usize| ResourceDocument {
            id,
            bytes: vec![0u8; 1],
        };
        let mut seen = vec![document(RESOURCE_ID_INDEX), document(RESOURCE_ID_INDEX + 1)];
        assert!(check_documents(&seen, RESOURCE_ID_INDEX).is_ok());
        seen.push(document(RESOURCE_ID_INDEX + 1));
        assert!(check_documents(&seen, RESOURCE_ID_INDEX).is_err());
        assert!(
            check_documents(&[document(0)], RESOURCE_ID_INDEX).is_err(),
            "a document below the first identifier would collide with the index"
        );
    }
}
