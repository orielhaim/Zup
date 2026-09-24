//! Private Win32 bindings for durable file I/O, locking, and Restart Manager.

#![allow(
    non_snake_case,
    non_camel_case_types,
    non_upper_case_globals,
    dead_code,
    clippy::all
)]

include!(concat!(env!("OUT_DIR"), "/known_folders.rs"));

pub use Windows::Win32::HANDLE;

use windows_link::link;

pub type DWORD = u32;
pub type BOOL = i32;
pub type LPCWSTR = *const u16;
pub type LPSECURITY_ATTRIBUTES = *mut core::ffi::c_void;

pub const GENERIC_READ: DWORD = 0x8000_0000;
pub const GENERIC_WRITE: DWORD = 0x4000_0000;
pub const CREATE_NEW: DWORD = 1;
pub const CREATE_ALWAYS: DWORD = 2;
pub const FILE_ATTRIBUTE_NORMAL: DWORD = 0x80;
pub const FILE_ATTRIBUTE_DIRECTORY: DWORD = 0x10;
pub const FILE_ATTRIBUTE_REPARSE_POINT: DWORD = 0x400;
pub const INVALID_FILE_ATTRIBUTES: DWORD = 0xFFFF_FFFF;

pub fn is_reparse_point(path: &std::path::Path) -> bool {
    use std::os::windows::ffi::OsStrExt;
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let attributes = unsafe { GetFileAttributesW(wide.as_ptr()) };
    attributes != INVALID_FILE_ATTRIBUTES && attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
}
pub const MOVEFILE_REPLACE_EXISTING: DWORD = 0x1;
pub const MOVEFILE_WRITE_THROUGH: DWORD = 0x8;
pub const INVALID_HANDLE_VALUE: HANDLE = -1isize as HANDLE;
pub const WAIT_OBJECT_0: DWORD = 0;
pub const INFINITE: DWORD = 0xFFFF_FFFF;

link!("kernel32.dll" "system" fn CreateFileW(
    lpfilename: LPCWSTR,
    lpdwdesiredaccess: DWORD,
    dwsharemode: DWORD,
    lpsecurityattributes: LPSECURITY_ATTRIBUTES,
    dwcreationdisposition: DWORD,
    dwflagsandattributes: DWORD,
    htemplatefile: HANDLE,
) -> HANDLE);
link!("kernel32.dll" "system" fn FlushFileBuffers(hfile: HANDLE) -> BOOL);
link!("kernel32.dll" "system" fn MoveFileExW(
    lpexistingfilename: LPCWSTR,
    lpnewfilename: LPCWSTR,
    dwflags: DWORD,
) -> BOOL);
link!("kernel32.dll" "system" fn CloseHandle(hobject: HANDLE) -> BOOL);
link!("kernel32.dll" "system" fn GetLastError() -> DWORD);
link!("kernel32.dll" "system" fn GetVolumePathNameW(
    lpfilename: LPCWSTR,
    lpvolumepathname: *mut u16,
    cchbufferlength: DWORD,
) -> BOOL);
link!("kernel32.dll" "system" fn CreateMutexW(
    lpmutexattributes: LPSECURITY_ATTRIBUTES,
    binitialowner: BOOL,
    lpname: LPCWSTR,
) -> HANDLE);
link!("kernel32.dll" "system" fn WaitForSingleObject(
    hhandle: HANDLE,
    dwmilliseconds: DWORD,
) -> DWORD);
link!("kernel32.dll" "system" fn ReleaseMutex(hmutex: HANDLE) -> BOOL);
link!("kernel32.dll" "system" fn DeleteFileW(lpfilename: LPCWSTR) -> BOOL);
link!("kernel32.dll" "system" fn RemoveDirectoryW(lppathname: LPCWSTR) -> BOOL);
link!("kernel32.dll" "system" fn CreateDirectoryW(
    lppathname: LPCWSTR,
    lpsecurityattributes: LPSECURITY_ATTRIBUTES,
) -> BOOL);
link!("kernel32.dll" "system" fn GetFileAttributesW(lpfilename: LPCWSTR) -> DWORD);

// FOLDERID constants
pub const FOLDERID_ProgramFiles: GUID = GUID {
    data1: 0x905e63b6,
    data2: 0xc1bf,
    data3: 0x494e,
    data4: [0xb2, 0x9c, 0x65, 0xb7, 0x32, 0xd3, 0xd2, 0x1a],
};
pub const FOLDERID_LocalAppData: GUID = GUID {
    data1: 0xF1B32785,
    data2: 0x6FBA,
    data3: 0x4FCF,
    data4: [0x9D, 0x55, 0x7B, 0x8E, 0x7F, 0x15, 0x70, 0x91],
};
pub const FOLDERID_ProgramData: GUID = GUID {
    data1: 0x62AB5D82,
    data2: 0xFDC1,
    data3: 0x4DC3,
    data4: [0xA9, 0xDD, 0x07, 0x0D, 0x1D, 0x49, 0x5D, 0x97],
};
pub const FOLDERID_StartMenu: GUID = GUID {
    data1: 0x625B53C3,
    data2: 0xAB48,
    data3: 0x4EC1,
    data4: [0xBA, 0x1F, 0xA1, 0xEF, 0x41, 0x46, 0xFC, 0x19],
};
pub const FOLDERID_CommonStartMenu: GUID = GUID {
    data1: 0xA4115719,
    data2: 0xD62E,
    data3: 0x491D,
    data4: [0xAA, 0x7C, 0xE7, 0x4B, 0x8B, 0xE3, 0xB0, 0x67],
};
pub const FOLDERID_Desktop: GUID = GUID {
    data1: 0xB4BFCC3A,
    data2: 0xDB2C,
    data3: 0x424C,
    data4: [0xB0, 0x29, 0x7F, 0xE9, 0x9A, 0x87, 0xC6, 0x41],
};
pub const FOLDERID_PublicDesktop: GUID = GUID {
    data1: 0xC4AA340D,
    data2: 0xF20F,
    data3: 0x4863,
    data4: [0xAF, 0xEF, 0xF8, 0x7E, 0xF2, 0xE6, 0xBA, 0x25],
};
pub const FOLDERID_Programs: GUID = GUID {
    data1: 0xA77F5D77,
    data2: 0x2E2B,
    data3: 0x44C3,
    data4: [0xA6, 0xA2, 0xAB, 0xA6, 0x01, 0x05, 0x4A, 0x51],
};
pub const FOLDERID_CommonPrograms: GUID = GUID {
    data1: 0x0139D44E,
    data2: 0x6AFE,
    data3: 0x49F2,
    data4: [0x86, 0x90, 0x3D, 0xAF, 0xCA, 0xE6, 0xFF, 0xB8],
};
