//! Narrow Win32 bindings used by the elevation transport.

#![allow(non_snake_case, non_camel_case_types, dead_code)]

use core::ffi::c_void;
use windows_link::link;

pub type Bool = i32;
pub type Dword = u32;
pub type Handle = *mut c_void;

pub const TOKEN_QUERY: Dword = 0x0008;
pub const PROCESS_QUERY_LIMITED_INFORMATION: Dword = 0x1000;
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

link!("advapi32.dll" "system" fn OpenProcessToken(process: Handle, access: Dword, token: *mut Handle) -> Bool);
link!("advapi32.dll" "system" fn GetTokenInformation(token: Handle, class: Dword, info: *mut c_void, len: Dword, returned: *mut Dword) -> Bool);
link!("advapi32.dll" "system" fn GetLengthSid(sid: *const c_void) -> Dword);
link!("advapi32.dll" "system" fn CopySid(destination_len: Dword, destination: *mut c_void, source: *const c_void) -> Bool);
link!("advapi32.dll" "system" fn ConvertSidToStringSidW(sid: *const c_void, string_sid: *mut *mut u16) -> Bool);
link!("advapi32.dll" "system" fn ConvertStringSecurityDescriptorToSecurityDescriptorW(string: *const u16, revision: Dword, descriptor: *mut *mut c_void, size: *mut Dword) -> Bool);
link!("kernel32.dll" "system" fn GetCurrentProcess() -> Handle);
link!("kernel32.dll" "system" fn OpenProcess(access: Dword, inherit: Bool, pid: Dword) -> Handle);
link!("kernel32.dll" "system" fn GetNamedPipeClientProcessId(pipe: Handle, pid: *mut Dword) -> Bool);
link!("kernel32.dll" "system" fn GetNamedPipeServerProcessId(pipe: Handle, pid: *mut Dword) -> Bool);
link!("kernel32.dll" "system" fn GetProcessId(process: Handle) -> Dword);
link!("kernel32.dll" "system" fn GetLastError() -> Dword);
link!("kernel32.dll" "system" fn LocalFree(memory: Handle) -> Handle);
link!("shell32.dll" "system" fn ShellExecuteExW(info: *mut ShellExecuteInfoW) -> Bool);
