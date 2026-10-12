#![allow(
    non_snake_case,
    non_camel_case_types,
    non_upper_case_globals,
    clippy::all
)]

include!(concat!(env!("OUT_DIR"), "/win32.rs"));

pub use Windows::Win32::HANDLE;
pub use Windows::Win32::{CoTaskMemFree, SHGetKnownFolderPath};

use core::ffi::c_void;
use windows_link::link;

pub const FOLDERID_ProgramFiles: GUID = GUID {
    data1: 0x905e63b6,
    data2: 0xc1bf,
    data3: 0x494e,
    data4: [0xb2, 0x9c, 0x65, 0xb7, 0x32, 0xd3, 0xd2, 0x1a],
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

pub const FOLDERID_PublicDesktop: GUID = GUID {
    data1: 0xC4AA340D,
    data2: 0xF20F,
    data3: 0x4863,
    data4: [0xAF, 0xEF, 0xF8, 0x7E, 0xF2, 0xE6, 0xBA, 0x25],
};

pub type DWORD = u32;
pub type BOOL = i32;
pub type LPCWSTR = *const u16;
pub type LPSECURITY_ATTRIBUTES = *mut c_void;
pub type Dword = DWORD;
pub type Bool = BOOL;
pub type Handle = HANDLE;

pub const GENERIC_WRITE: DWORD = 0x4000_0000;
pub const CREATE_NEW: DWORD = 1;
pub const FILE_ATTRIBUTE_NORMAL: DWORD = 0x80;
pub const FILE_ATTRIBUTE_REPARSE_POINT: DWORD = 0x400;
pub const INVALID_FILE_ATTRIBUTES: DWORD = 0xFFFF_FFFF;
pub const MOVEFILE_REPLACE_EXISTING: DWORD = 0x1;
pub const MOVEFILE_WRITE_THROUGH: DWORD = 0x8;
pub const INVALID_HANDLE_VALUE: HANDLE = -1isize as HANDLE;
pub const TOKEN_QUERY: Dword = 0x0008;
pub const PROCESS_QUERY_LIMITED_INFORMATION: Dword = 0x1000;
pub const SYNCHRONIZE: Dword = 0x0010_0000;
pub const TOKEN_USER: Dword = 1;
pub const TOKEN_ELEVATION: Dword = 20;
pub const SEE_MASK_NOCLOSEPROCESS: Dword = 0x0000_0040;
pub const SW_SHOWNORMAL: i32 = 1;
pub const ERROR_CANCELLED: Dword = 1223;
pub const SDDL_REVISION_1: Dword = 1;

#[repr(C)]
pub struct SidAndAttributes {
    pub Sid: *mut c_void,
    pub Attributes: Dword,
}

#[repr(C)]
pub struct TokenUser {
    pub User: SidAndAttributes,
}

#[repr(C)]
pub struct TokenElevation {
    pub TokenIsElevated: Dword,
}

#[repr(C)]
pub struct SecurityAttributes {
    pub nLength: Dword,
    pub lpSecurityDescriptor: *mut c_void,
    pub bInheritHandle: Bool,
}

#[repr(C)]
pub struct ShellExecuteInfoW {
    pub cbSize: Dword,
    pub fMask: Dword,
    pub hwnd: Handle,
    pub lpVerb: *const u16,
    pub lpFile: *const u16,
    pub lpParameters: *const u16,
    pub lpDirectory: *const u16,
    pub nShow: i32,
    pub hInstApp: Handle,
    pub lpIDList: *mut c_void,
    pub lpClass: *const u16,
    pub hkeyClass: Handle,
    pub dwHotKey: Dword,
    pub hMonitor: Handle,
    pub hProcess: Handle,
}

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

pub fn is_link_or_reparse(path: &std::path::Path) -> Result<bool, std::io::Error> {
    match path.symlink_metadata() {
        Ok(metadata) => Ok(is_link_metadata(&metadata, path)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

pub fn is_link_metadata(metadata: &std::fs::Metadata, path: &std::path::Path) -> bool {
    #[cfg(windows)]
    {
        metadata.file_type().is_symlink() || is_reparse_point(path)
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        metadata.file_type().is_symlink()
    }
}

link!("kernel32.dll" "system" fn CreateFileW(lpfilename: LPCWSTR, lpdwdesiredaccess: DWORD, dwsharemode: DWORD, lpsecurityattributes: LPSECURITY_ATTRIBUTES, dwcreationdisposition: DWORD, dwflagsandattributes: DWORD, htemplatefile: HANDLE) -> HANDLE);
link!("kernel32.dll" "system" fn FlushFileBuffers(hfile: HANDLE) -> BOOL);
link!("kernel32.dll" "system" fn MoveFileExW(lpexistingfilename: LPCWSTR, lpnewfilename: LPCWSTR, dwflags: DWORD) -> BOOL);
link!("kernel32.dll" "system" fn CloseHandle(hobject: HANDLE) -> BOOL);
link!("kernel32.dll" "system" fn GetLastError() -> DWORD);
link!("kernel32.dll" "system" fn GetVolumePathNameW(lpfilename: LPCWSTR, lpvolumepathname: *mut u16, cchbufferlength: DWORD) -> BOOL);
link!("kernel32.dll" "system" fn WaitForSingleObject(hhandle: HANDLE, dwmilliseconds: DWORD) -> DWORD);
link!("kernel32.dll" "system" fn GetFileAttributesW(lpfilename: LPCWSTR) -> DWORD);
link!("kernel32.dll" "system" fn GetCurrentProcess() -> Handle);
link!("kernel32.dll" "system" fn OpenProcess(access: Dword, inherit: Bool, pid: Dword) -> Handle);
link!("kernel32.dll" "system" fn GetNamedPipeClientProcessId(pipe: Handle, pid: *mut Dword) -> Bool);
link!("kernel32.dll" "system" fn GetNamedPipeServerProcessId(pipe: Handle, pid: *mut Dword) -> Bool);
link!("kernel32.dll" "system" fn GetProcessId(process: Handle) -> Dword);
link!("kernel32.dll" "system" fn LocalFree(memory: Handle) -> Handle);
link!("advapi32.dll" "system" fn OpenProcessToken(process: Handle, access: Dword, token: *mut Handle) -> Bool);
link!("advapi32.dll" "system" fn GetTokenInformation(token: Handle, class: Dword, info: *mut c_void, len: Dword, returned: *mut Dword) -> Bool);
link!("advapi32.dll" "system" fn GetLengthSid(sid: *const c_void) -> Dword);
link!("advapi32.dll" "system" fn CopySid(destination_len: Dword, destination: *mut c_void, source: *const c_void) -> Bool);
link!("advapi32.dll" "system" fn ConvertSidToStringSidW(sid: *const c_void, string_sid: *mut *mut u16) -> Bool);
link!("advapi32.dll" "system" fn ConvertStringSecurityDescriptorToSecurityDescriptorW(string: *const u16, revision: Dword, descriptor: *mut *mut c_void, size: *mut Dword) -> Bool);
link!("shell32.dll" "system" fn ShellExecuteExW(info: *mut ShellExecuteInfoW) -> Bool);
