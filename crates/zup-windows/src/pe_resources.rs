use std::path::Path;

use zup_pe::{
    MAX_RESOURCE_ID, MAX_RESOURCE_SIZE, PeError, RESOURCE_ID_INDEX, ResourceDocument,
    resources::{
        RESOURCE_TYPE_RCDATA, read_resource as read_document, update_icons, update_rcdata,
    },
};

pub fn write_resources(
    executable: &Path,
    output: &Path,
    documents: &[ResourceDocument],
) -> Result<(), ResourceError> {
    zup_pe::check_documents(documents, RESOURCE_ID_INDEX).map_err(ResourceError::Pe)?;
    if output.exists() || output == executable {
        return Err(ResourceError::Missing);
    }
    let template = std::fs::read(executable).map_err(|_| ResourceError::Missing)?;
    let composed = update_rcdata(&template, documents).map_err(ResourceError::Pe)?;
    std::fs::write(output, &composed).map_err(|_| ResourceError::Missing)?;
    Ok(())
}

pub fn read_resource(path: &Path, id: usize) -> Result<Vec<u8>, ResourceError> {
    if id > MAX_RESOURCE_ID {
        return Err(ResourceError::Absent(id));
    }
    let image = std::fs::read(path).map_err(|_| ResourceError::Absent(id))?;
    let document =
        read_document(&image, RESOURCE_TYPE_RCDATA, id as u32).map_err(ResourceError::Pe)?;
    document.ok_or(ResourceError::Absent(id))
}

pub fn apply_icon(
    executable: &Path,
    images: &[Vec<u8>],
    group: &[u8],
) -> Result<(), ResourceError> {
    if images.is_empty() || group.is_empty() {
        return Err(ResourceError::Missing);
    }
    if images.len() > MAX_RESOURCE_ID {
        return Err(ResourceError::Pe(PeError::TooManyResources {
            count: images.len(),
            limit: MAX_RESOURCE_ID,
        }));
    }
    if images
        .iter()
        .any(|image| image.len() as u64 > MAX_RESOURCE_SIZE)
        || group.len() as u64 > MAX_RESOURCE_SIZE
    {
        return Err(ResourceError::Pe(PeError::ResourceTooLarge {
            size: MAX_RESOURCE_SIZE + 1,
            limit: MAX_RESOURCE_SIZE,
        }));
    }
    let image = std::fs::read(executable).map_err(|_| ResourceError::Missing)?;
    let stamped = update_icons(&image, images, group).map_err(ResourceError::Pe)?;
    std::fs::write(executable, &stamped).map_err(|_| ResourceError::Missing)?;
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum ResourceError {
    #[error("executable has no resource {0}")]
    Absent(usize),
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
        assert!(zup_pe::check_documents(&seen, RESOURCE_ID_INDEX).is_ok());
        seen.push(document(RESOURCE_ID_INDEX + 1));
        assert!(zup_pe::check_documents(&seen, RESOURCE_ID_INDEX).is_err());
        assert!(
            zup_pe::check_documents(&[document(0)], RESOURCE_ID_INDEX).is_err(),
            "a document below the first identifier would collide with the index"
        );
    }

    #[cfg(windows)]
    #[test]
    fn the_loader_reads_a_composed_tree() {
        use std::os::windows::ffi::OsStrExt;
        use windows_link::link;

        type Handle = *mut core::ffi::c_void;
        type Dword = u32;
        link!("kernel32.dll" "system" fn LoadLibraryExW(filename: *const u16, file: Handle, flags: Dword) -> Handle);
        link!("kernel32.dll" "system" fn FindResourceW(module: Handle, name: *const u16, resource_type: *const u16) -> Handle);
        link!("kernel32.dll" "system" fn LoadResource(module: Handle, resource: Handle) -> Handle);
        link!("kernel32.dll" "system" fn SizeofResource(module: Handle, resource: Handle) -> Dword);
        link!("kernel32.dll" "system" fn LockResource(resource: Handle) -> *const core::ffi::c_void);
        link!("kernel32.dll" "system" fn FreeLibrary(module: Handle) -> i32);

        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("loader.exe");
        let documents = vec![
            ResourceDocument {
                id: 1,
                bytes: b"loader-index".to_vec(),
            },
            ResourceDocument {
                id: 7,
                bytes: vec![9u8; 3000],
            },
        ];
        write_resources(&std::env::current_exe().unwrap(), &output, &documents).unwrap();

        let wide: Vec<u16> = output.as_os_str().encode_wide().chain(Some(0)).collect();
        let module = unsafe { LoadLibraryExW(wide.as_ptr(), std::ptr::null_mut(), 2) };
        assert!(!module.is_null());
        for document in &documents {
            let found =
                unsafe { FindResourceW(module, document.id as *const u16, 10 as *const u16) };
            assert!(
                !found.is_null(),
                "the loader finds resource {}",
                document.id
            );
            let size = unsafe { SizeofResource(module, found) } as usize;
            assert_eq!(size, document.bytes.len());
            let loaded = unsafe { LoadResource(module, found) };
            let data = unsafe { LockResource(loaded) };
            assert!(!data.is_null());
            let bytes = unsafe { std::slice::from_raw_parts(data.cast::<u8>(), size) };
            assert_eq!(bytes, document.bytes.as_slice());
        }
        assert!(
            unsafe { FindResourceW(module, 3 as *const u16, 10 as *const u16) }.is_null(),
            "an identifier that was never written stays absent"
        );
        unsafe {
            FreeLibrary(module);
        }
    }
}
