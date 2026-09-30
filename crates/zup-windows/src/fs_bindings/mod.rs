//! Private Win32 bindings for durable file I/O, locking, and Restart Manager.

#![allow(
    non_snake_case,
    non_camel_case_types,
    non_upper_case_globals,
    dead_code,
    clippy::all
)]

include!(concat!(env!("OUT_DIR"), "/win32.rs"));

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
