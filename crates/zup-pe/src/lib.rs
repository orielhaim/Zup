//! Portable Executable headers and resources.
//!
//! This is the native implementation, and it is deliberately small. Two things
//! need it and neither can use a generic object-file rewriter:
//!
//! - **Composition** writes resources into an image that is about to be signed.
//!   Rewriting a PE through a general library would have to reproduce section
//!   alignment, the resource directory, and the certificate table exactly, and
//!   the certificate table is what Authenticode later covers.
//! - **Reading** an image's own resources is how a program finds the artifact it
//!   was built into.
//!
//! A build-time inspector answers a different question — what format and machine
//! an arbitrary template is — and belongs in `zup-build`, which is portable.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use thiserror::Error;
use windows_link::link;

/// The resource type every zup container stores its documents under.
pub const RESOURCE_TYPE_RCDATA: u16 = 10;

/// The resource identifier the artifact index occupies.
pub const RESOURCE_ID_INDEX: usize = 1;

/// The first resource identifier after the index, where a container's own
/// document sequence begins.
pub const RESOURCE_ID_BLOB_START: usize = 2;

/// Largest size one resource may have, because a resource length is a 32-bit
/// field in the resource directory.
pub const MAX_RESOURCE_SIZE: u64 = u32::MAX as u64;

/// Largest resource identifier a container may use.
pub const MAX_RESOURCE_ID: usize = u16::MAX as usize - 1;

/// The machine type a PE header names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Machine {
    I386,
    Amd64,
    Arm64,
    /// A machine type this build does not model.
    Other(u16),
}

impl Machine {
    /// The stable token for this machine type, which is also the architecture
    /// name a target triple uses where they agree.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::I386 => "i686",
            Self::Amd64 => "x86_64",
            Self::Arm64 => "aarch64",
            Self::Other(_) => "other",
        }
    }
}

impl std::fmt::Display for Machine {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Other(raw) => write!(out, "machine 0x{raw:04x}"),
            machine => out.write_str(machine.as_str()),
        }
    }
}

/// The subsystem a PE header names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Subsystem {
    Console,
    Gui,
    /// A subsystem this build does not model.
    Other(u16),
}

/// The parts of a PE header zup reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeHeader {
    pub machine: Machine,
    pub subsystem: Subsystem,
    raw_machine: u16,
    raw_subsystem: u16,
    security_offset: u64,
}

impl PeHeader {
    /// Whether the image already carries an Authenticode certificate table.
    pub fn is_signed(&self, file: &mut File) -> Result<bool, PeError> {
        file.seek(SeekFrom::Start(self.security_offset))?;
        let mut certificate = [0u8; 8];
        file.read_exact(&mut certificate)?;
        Ok(certificate != [0; 8])
    }
}

/// Failures produced by the PE reader and writer.
#[derive(Debug, Error)]
pub enum PeError {
    #[error("PE I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("image is truncated, corrupt, or not a Portable Executable")]
    Invalid,
    #[error("executable has no resource {0}")]
    MissingResource(usize),
    #[error("resource data is {size} bytes; the limit is {limit} bytes")]
    ResourceTooLarge { size: u64, limit: u64 },
    #[error("container would use {count} resources; the limit is {limit}")]
    TooManyResources { count: usize, limit: usize },
    #[error("resource API failed with error {0}")]
    ResourceApi(u32),
    #[error("resource APIs are available only on Windows")]
    ResourcesUnavailable,
    #[error("cannot allocate {size} bytes while processing image resources")]
    Allocation { size: u64 },
}

const PE_SUBSYSTEM_OFFSET: u64 = 68;
const PE_MIN_OPTIONAL_HEADER_SIZE: u64 = PE_SUBSYSTEM_OFFSET + 2;
const IMAGE_SUBSYSTEM_GUI: u16 = 2;
const IMAGE_SUBSYSTEM_CUI: u16 = 3;

/// Read the header fields zup needs from an image.
pub fn read_pe_header(path: &Path) -> Result<PeHeader, PeError> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    if len < 0x40 {
        return Err(PeError::Invalid);
    }
    let mut dos = [0u8; 0x40];
    file.read_exact(&mut dos)?;
    if &dos[..2] != b"MZ" {
        return Err(PeError::Invalid);
    }
    let pe = u32::from_le_bytes(dos[0x3c..0x40].try_into().unwrap()) as u64;
    if pe < 0x40 || pe.checked_add(24).is_none_or(|end| end > len) {
        return Err(PeError::Invalid);
    }
    file.seek(SeekFrom::Start(pe))?;
    let mut signature = [0u8; 4];
    file.read_exact(&mut signature)?;
    if &signature != b"PE\0\0" {
        return Err(PeError::Invalid);
    }
    let mut coff = [0u8; 20];
    file.read_exact(&mut coff)?;
    let raw_machine = u16::from_le_bytes(coff[..2].try_into().unwrap());
    let section_count = u16::from_le_bytes(coff[2..4].try_into().unwrap());
    let optional_offset = pe + 24;
    let optional_len = u16::from_le_bytes(coff[16..18].try_into().unwrap()) as u64;
    let optional_end = optional_offset
        .checked_add(optional_len)
        .ok_or(PeError::Invalid)?;
    if optional_len < PE_MIN_OPTIONAL_HEADER_SIZE || optional_end > len || section_count == 0 {
        return Err(PeError::Invalid);
    }
    file.seek(SeekFrom::Start(optional_offset))?;
    let mut magic = [0u8; 2];
    file.read_exact(&mut magic)?;
    let data_directory_offset = match u16::from_le_bytes(magic) {
        0x10b => 96,
        0x20b => 112,
        _ => return Err(PeError::Invalid),
    };
    let security_offset = optional_offset
        .checked_add(data_directory_offset)
        .and_then(|offset| offset.checked_add(8 * 4))
        .ok_or(PeError::Invalid)?;
    if security_offset
        .checked_add(8)
        .is_none_or(|end| end > optional_end)
    {
        return Err(PeError::Invalid);
    }
    file.seek(SeekFrom::Start(optional_offset + PE_SUBSYSTEM_OFFSET))?;
    let mut subsystem = [0u8; 2];
    file.read_exact(&mut subsystem)?;
    let raw_subsystem = u16::from_le_bytes(subsystem);
    let section_end = optional_end
        .checked_add(
            u64::from(section_count)
                .checked_mul(40)
                .ok_or(PeError::Invalid)?,
        )
        .ok_or(PeError::Invalid)?;
    if section_end > len {
        return Err(PeError::Invalid);
    }
    Ok(PeHeader {
        machine: match raw_machine {
            0x014c => Machine::I386,
            0x8664 => Machine::Amd64,
            0xaa64 => Machine::Arm64,
            other => Machine::Other(other),
        },
        subsystem: match raw_subsystem {
            IMAGE_SUBSYSTEM_CUI => Subsystem::Console,
            IMAGE_SUBSYSTEM_GUI => Subsystem::Gui,
            other => Subsystem::Other(other),
        },
        raw_machine,
        raw_subsystem,
        security_offset,
    })
}

/// The raw machine value, for a caller that maps it to its own vocabulary.
pub const fn raw_machine(header: &PeHeader) -> u16 {
    header.raw_machine
}

/// The raw subsystem value.
pub const fn raw_subsystem(header: &PeHeader) -> u16 {
    header.raw_subsystem
}

/// Whether an image is already signed, which composition must refuse to change.
pub fn is_signed(path: &Path) -> Result<bool, PeError> {
    let header = read_pe_header(path)?;
    header.is_signed(&mut File::open(path)?)
}

/// Whether an image starts with a DOS signature.
pub fn looks_like_pe(path: &Path) -> bool {
    let Ok(mut file) = File::open(path) else {
        return false;
    };
    let mut magic = [0u8; 2];
    file.read_exact(&mut magic).is_ok() && &magic == b"MZ"
}

/// One document a container writes, addressed by its resource identifier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceDocument {
    pub id: usize,
    pub bytes: Vec<u8>,
}

/// Check that a document set is writable: identifiers start where the caller
/// says, do not collide, and fit both the resource length and identifier bounds.
pub fn check_documents(documents: &[ResourceDocument], first_id: usize) -> Result<(), PeError> {
    if documents.iter().any(|document| {
        document.id > MAX_RESOURCE_ID || document.bytes.len() as u64 > MAX_RESOURCE_SIZE
    }) {
        return Err(PeError::TooManyResources {
            count: documents.len(),
            limit: MAX_RESOURCE_ID,
        });
    }
    let mut seen = std::collections::BTreeSet::new();
    for document in documents {
        if document.id < first_id || !seen.insert(document.id) {
            return Err(PeError::TooManyResources {
                count: documents.len(),
                limit: MAX_RESOURCE_ID,
            });
        }
    }
    Ok(())
}

/// Write `documents` into a copy of `executable` at `output`.
///
/// The copy happens first and the resources are applied to the copy, so a
/// failure part-way through never leaves a half-written artifact. Signing happens
/// after this returns, because Authenticode covers the embedded resources.
#[cfg(windows)]
pub fn write_resources(
    executable: &Path,
    output: &Path,
    documents: &[ResourceDocument],
) -> Result<(), PeError> {
    use std::os::windows::ffi::OsStrExt;

    type Handle = *mut core::ffi::c_void;
    type Bool = i32;
    type Dword = u32;

    link!("kernel32.dll" "system" fn BeginUpdateResourceW(filename: *const u16, delete_existing: Bool) -> Handle);
    link!("kernel32.dll" "system" fn UpdateResourceW(update: Handle, resource_type: *const u16, name: *const u16, language: u16, data: *const core::ffi::c_void, size: Dword) -> Bool);
    link!("kernel32.dll" "system" fn EndUpdateResourceW(update: Handle, discard: Bool) -> Bool);
    link!("kernel32.dll" "system" fn GetLastError() -> Dword);

    check_documents(documents, RESOURCE_ID_INDEX)?;
    if output.exists() || output == executable {
        return Err(PeError::Invalid);
    }
    std::fs::copy(executable, output)?;
    let wide: Vec<u16> = output.as_os_str().encode_wide().chain(Some(0)).collect();
    let update = unsafe { BeginUpdateResourceW(wide.as_ptr(), 0) };
    if update.is_null() {
        let _ = std::fs::remove_file(output);
        return Err(PeError::ResourceApi(unsafe { GetLastError() }));
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
        return Err(PeError::ResourceApi(error));
    }
    if unsafe { EndUpdateResourceW(update, 0) } == 0 {
        let error = unsafe { GetLastError() };
        let _ = std::fs::remove_file(output);
        return Err(PeError::ResourceApi(error));
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn write_resources(
    _executable: &Path,
    _output: &Path,
    _documents: &[ResourceDocument],
) -> Result<(), PeError> {
    Err(PeError::ResourcesUnavailable)
}

/// Read one resource document out of an image.
#[cfg(windows)]
pub fn read_resource(path: &Path, id: usize) -> Result<Vec<u8>, PeError> {
    use std::{os::windows::ffi::OsStrExt, ptr};

    type Handle = *mut core::ffi::c_void;
    type Dword = u32;

    link!("kernel32.dll" "system" fn LoadLibraryExW(filename: *const u16, file: Handle, flags: Dword) -> Handle);
    link!("kernel32.dll" "system" fn FindResourceW(module: Handle, name: *const u16, resource_type: *const u16) -> Handle);
    link!("kernel32.dll" "system" fn LoadResource(module: Handle, resource: Handle) -> Handle);
    link!("kernel32.dll" "system" fn SizeofResource(module: Handle, resource: Handle) -> Dword);
    link!("kernel32.dll" "system" fn LockResource(resource: Handle) -> *const core::ffi::c_void);
    link!("kernel32.dll" "system" fn FreeLibrary(module: Handle) -> i32);
    link!("kernel32.dll" "system" fn GetLastError() -> Dword);

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let module = unsafe { LoadLibraryExW(wide.as_ptr(), ptr::null_mut(), 0x0000_0002) };
    if module.is_null() {
        return Err(PeError::ResourceApi(unsafe { GetLastError() }));
    }
    let resource =
        unsafe { FindResourceW(module, id as *const u16, RESOURCE_TYPE_RCDATA as *const u16) };
    let result = if resource.is_null() {
        Err(PeError::MissingResource(id))
    } else {
        let size = unsafe { SizeofResource(module, resource) } as usize;
        if size == 0 {
            Err(PeError::Invalid)
        } else {
            let loaded = unsafe { LoadResource(module, resource) };
            let data = if loaded.is_null() {
                ptr::null()
            } else {
                unsafe { LockResource(loaded) }
            };
            if data.is_null() {
                Err(PeError::ResourceApi(unsafe { GetLastError() }))
            } else {
                let mut bytes = Vec::new();
                bytes
                    .try_reserve_exact(size)
                    .map_err(|_| PeError::Allocation { size: size as u64 })?;
                bytes.extend_from_slice(unsafe {
                    std::slice::from_raw_parts(data.cast::<u8>(), size)
                });
                Ok(bytes)
            }
        }
    };
    unsafe {
        FreeLibrary(module);
    }
    result
}

#[cfg(not(windows))]
pub fn read_resource(_path: &Path, id: usize) -> Result<Vec<u8>, PeError> {
    Err(PeError::ResourcesUnavailable)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(subsystem: u16) -> Vec<u8> {
        let mut bytes = vec![0u8; 0x170];
        bytes[..2].copy_from_slice(b"MZ");
        bytes[0x3c..0x40].copy_from_slice(&0x40u32.to_le_bytes());
        bytes[0x40..0x44].copy_from_slice(b"PE\0\0");
        bytes[0x44..0x46].copy_from_slice(&0x8664u16.to_le_bytes());
        bytes[0x46..0x48].copy_from_slice(&1u16.to_le_bytes());
        bytes[0x54..0x56].copy_from_slice(&240u16.to_le_bytes());
        bytes[0x58..0x5a].copy_from_slice(&0x20bu16.to_le_bytes());
        bytes[0x9c..0x9e].copy_from_slice(&subsystem.to_le_bytes());
        bytes
    }

    fn write(bytes: &[u8], name: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(name);
        std::fs::write(&path, bytes).unwrap();
        (directory, path)
    }

    #[test]
    fn machine_and_subsystem_are_read_from_the_header() {
        let (_dir, path) = write(&image(3), "cui.exe");
        let header = read_pe_header(&path).unwrap();
        assert_eq!(header.machine, Machine::Amd64);
        assert_eq!(header.subsystem, Subsystem::Console);

        let (_dir, path) = write(&image(2), "gui.exe");
        assert_eq!(read_pe_header(&path).unwrap().subsystem, Subsystem::Gui);

        let (_dir, path) = write(&image(9), "odd.exe");
        assert_eq!(
            read_pe_header(&path).unwrap().subsystem,
            Subsystem::Other(9)
        );
    }

    #[test]
    fn an_unsigned_image_reports_itself_as_unsigned() {
        let (_dir, path) = write(&image(2), "gui.exe");
        assert!(!is_signed(&path).unwrap());
    }

    #[test]
    fn a_truncated_or_foreign_file_is_rejected() {
        let (_dir, path) = write(b"not an image", "x.bin");
        assert!(matches!(read_pe_header(&path), Err(PeError::Invalid)));
        assert!(!looks_like_pe(&path));

        let (_dir, path) = write(&image(2)[..0x50], "short.exe");
        assert!(matches!(read_pe_header(&path), Err(PeError::Invalid)));
    }

    #[test]
    fn document_identifiers_must_be_unique_and_within_bounds() {
        let ok = vec![ResourceDocument {
            id: RESOURCE_ID_BLOB_START,
            bytes: b"a".to_vec(),
        }];
        assert!(check_documents(&ok, RESOURCE_ID_INDEX).is_ok());
        assert!(
            check_documents(&ok, RESOURCE_ID_BLOB_START).is_ok(),
            "the index and the blob sequence share one identifier space"
        );
        assert!(
            check_documents(&ok, RESOURCE_ID_BLOB_START + 1).is_err(),
            "a document below the first writable identifier is refused"
        );
        let duplicated = vec![
            ResourceDocument {
                id: 4,
                bytes: b"a".to_vec(),
            },
            ResourceDocument {
                id: 4,
                bytes: b"b".to_vec(),
            },
        ];
        assert!(check_documents(&duplicated, RESOURCE_ID_BLOB_START).is_err());
    }
}
