//! Windows process, token, and elevation primitives for the worker transport.

use std::ffi::c_void;
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;
#[cfg(feature = "test-launcher")]
use std::os::windows::io::IntoRawHandle;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::path::Path;
use std::ptr::{null, null_mut};

use crate::transport_bindings as win;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum TransportError {
    #[error("Windows API `{api}` failed with error {code}")]
    Win32 { api: &'static str, code: u32 },
    #[error("security descriptor failed: {0}")]
    SecurityDescriptorFailed(String),
    #[error("parent PID mismatch: expected {expected}, found {found}")]
    ParentPidMismatch { expected: u32, found: u32 },
    #[error("worker PID mismatch: expected {expected}, found {found}")]
    WorkerPidMismatch { expected: u32, found: u32 },
    #[error("worker is not elevated")]
    WorkerNotElevated,
    #[error("elevation was cancelled")]
    ElevationCancelled,
    #[error("worker launch failed: {0}")]
    Launch(#[source] std::io::Error),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserSid {
    bytes: Vec<u8>,
    display: String,
}

impl UserSid {
    pub fn current() -> Result<Self, TransportError> {
        Self::from_token(&current_process_token()?)
    }
    pub fn for_process(pid: u32) -> Result<Self, TransportError> {
        // SAFETY: OpenProcess validates pid; inheritance is disabled.
        let process = unsafe { win::OpenProcess(win::PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if process.is_null() {
            return Err(last_error("OpenProcess"));
        }
        // SAFETY: OpenProcess returned an owned handle.
        let process = unsafe { OwnedHandle::from_raw_handle(process.cast()) };
        let mut token = null_mut();
        // SAFETY: process is live and output storage is valid.
        if unsafe {
            win::OpenProcessToken(process.as_raw_handle().cast(), win::TOKEN_QUERY, &mut token)
        } == 0
        {
            return Err(last_error("OpenProcessToken"));
        }
        // SAFETY: OpenProcessToken returned an owned handle.
        let token = unsafe { OwnedHandle::from_raw_handle(token.cast()) };
        Self::from_token(&token)
    }
    fn from_token(token: &OwnedHandle) -> Result<Self, TransportError> {
        let mut required = 0;
        // SAFETY: null buffer is the documented size-query form.
        unsafe {
            win::GetTokenInformation(
                token.as_raw_handle().cast(),
                win::TOKEN_USER,
                null_mut(),
                0,
                &mut required,
            );
        }
        if required < size_of::<win::TokenUser>() as u32 {
            return Err(last_error("GetTokenInformation(TokenUser size)"));
        }
        let mut buffer = vec![0u8; required as usize];
        // SAFETY: buffer has the size returned by the preceding API call.
        if unsafe {
            win::GetTokenInformation(
                token.as_raw_handle().cast(),
                win::TOKEN_USER,
                buffer.as_mut_ptr().cast(),
                required,
                &mut required,
            )
        } == 0
        {
            return Err(last_error("GetTokenInformation(TokenUser)"));
        }
        // SAFETY: a successful TokenUser query initializes TOKEN_USER and its SID pointer.
        let sid = unsafe { (*(buffer.as_ptr().cast::<win::TokenUser>())).User.Sid };
        // SAFETY: sid comes from the validated token buffer.
        let sid_len = unsafe { win::GetLengthSid(sid) };
        if sid_len == 0 {
            return Err(last_error("GetLengthSid"));
        }
        let mut bytes = vec![0u8; sid_len as usize];
        // SAFETY: destination is sid_len bytes and source is a valid SID.
        if unsafe { win::CopySid(sid_len, bytes.as_mut_ptr().cast(), sid) } == 0 {
            return Err(last_error("CopySid"));
        }
        let display = sid_to_string(bytes.as_ptr().cast())?;
        Ok(Self { bytes, display })
    }
    pub fn display(&self) -> &str {
        &self.display
    }
}

pub(crate) struct PipeSecurityDescriptor {
    descriptor: *mut c_void,
    attributes: Box<win::SecurityAttributes>,
}

impl PipeSecurityDescriptor {
    pub(crate) fn new(user: &UserSid) -> Result<Self, TransportError> {
        let sddl = format!("D:P(A;;GA;;;{})(A;;GA;;;BA)(A;;GA;;;SY)", user.display);
        let wide = wide(&sddl);
        let mut descriptor = null_mut();
        // SAFETY: wide is NUL terminated and output storage is valid.
        if unsafe {
            win::ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide.as_ptr(),
                win::SDDL_REVISION_1,
                &mut descriptor,
                null_mut(),
            )
        } == 0
        {
            return Err(last_error(
                "ConvertStringSecurityDescriptorToSecurityDescriptorW",
            ));
        }
        let attributes = Box::new(win::SecurityAttributes {
            nLength: size_of::<win::SecurityAttributes>() as u32,
            lpSecurityDescriptor: descriptor,
            bInheritHandle: 0,
        });
        Ok(Self {
            descriptor,
            attributes,
        })
    }
    pub(crate) fn as_raw(&self) -> RawHandle {
        (&*self.attributes as *const win::SecurityAttributes)
            .cast_mut()
            .cast()
    }
}

impl Drop for PipeSecurityDescriptor {
    fn drop(&mut self) {
        /* SAFETY: descriptor was allocated by LocalAlloc. */
        unsafe {
            win::LocalFree(self.descriptor);
        }
    }
}

pub fn pipe_name(session_id: &str) -> String {
    let sanitized: String = session_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .take(40)
        .collect();
    format!("zup-{sanitized}")
}
pub fn pipe_path(name: &str) -> String {
    format!(r"\\.\pipe\{name}")
}

pub struct ProcessHandle {
    _handle: OwnedHandle,
    pid: u32,
}
impl ProcessHandle {
    pub fn pid(&self) -> u32 {
        self.pid
    }
}

pub fn is_process_elevated() -> Result<bool, TransportError> {
    let token = current_process_token()?;
    let mut elevation = win::TokenElevation { TokenIsElevated: 0 };
    let mut returned = 0;
    // SAFETY: elevation points to a correctly sized TOKEN_ELEVATION.
    if unsafe {
        win::GetTokenInformation(
            token.as_raw_handle().cast(),
            win::TOKEN_ELEVATION,
            (&mut elevation as *mut win::TokenElevation).cast(),
            size_of::<win::TokenElevation>() as u32,
            &mut returned,
        )
    } == 0
    {
        return Err(last_error("GetTokenInformation(TokenElevation)"));
    }
    Ok(elevation.TokenIsElevated != 0)
}

/// Wait until a process exits. A missing process means it has already exited.
pub fn wait_for_process_exit(pid: u32) -> Result<(), TransportError> {
    let process = unsafe { win::OpenProcess(win::SYNCHRONIZE, 0, pid) };
    if process.is_null() {
        return Ok(());
    }
    let result = unsafe { win::WaitForSingleObject(process, u32::MAX) };
    unsafe {
        win::CloseHandle(process);
    }
    if result == 0 {
        Ok(())
    } else {
        Err(last_error("WaitForSingleObject"))
    }
}

pub fn verify_server_pid(raw: isize, expected: u32) -> Result<(), TransportError> {
    let mut found = 0;
    // SAFETY: raw is a connected named-pipe handle owned by the caller.
    if unsafe { win::GetNamedPipeServerProcessId(raw as win::Handle, &mut found) } == 0 {
        return Err(last_error("GetNamedPipeServerProcessId"));
    }
    if found != expected {
        return Err(TransportError::ParentPidMismatch { expected, found });
    }
    Ok(())
}
pub fn verify_client_pid(raw: isize, expected: u32) -> Result<(), TransportError> {
    let mut found = 0;
    // SAFETY: raw is a connected named-pipe handle owned by the caller.
    if unsafe { win::GetNamedPipeClientProcessId(raw as win::Handle, &mut found) } == 0 {
        return Err(last_error("GetNamedPipeClientProcessId"));
    }
    if found != expected {
        return Err(TransportError::WorkerPidMismatch { expected, found });
    }
    Ok(())
}

pub fn launch_elevated_worker(
    executable: &Path,
    parameters: &str,
) -> Result<ProcessHandle, TransportError> {
    let verb = wide("runas");
    let executable = wide_os(executable.as_os_str());
    let parameters = wide(parameters);
    let mut info = win::ShellExecuteInfoW {
        cbSize: size_of::<win::ShellExecuteInfoW>() as u32,
        fMask: win::SEE_MASK_NOCLOSEPROCESS,
        hwnd: null_mut(),
        lpVerb: verb.as_ptr(),
        lpFile: executable.as_ptr(),
        lpParameters: parameters.as_ptr(),
        lpDirectory: null(),
        nShow: win::SW_SHOWNORMAL,
        hInstApp: null_mut(),
        lpIDList: null_mut(),
        lpClass: null(),
        hkeyClass: null_mut(),
        dwHotKey: 0,
        hMonitor: null_mut(),
        hProcess: null_mut(),
    };
    // SAFETY: strings remain alive and the struct matches SHELLEXECUTEINFOW.
    if unsafe { win::ShellExecuteExW(&mut info) } == 0 {
        let code = last_error_code();
        return if code == win::ERROR_CANCELLED {
            Err(TransportError::ElevationCancelled)
        } else {
            Err(TransportError::Win32 {
                api: "ShellExecuteExW",
                code,
            })
        };
    }
    if info.hProcess.is_null() {
        return Err(TransportError::SecurityDescriptorFailed(
            "ShellExecuteExW returned no process handle".into(),
        ));
    }
    // SAFETY: SEE_MASK_NOCLOSEPROCESS transfers one owned process handle.
    let handle = unsafe { OwnedHandle::from_raw_handle(info.hProcess.cast()) };
    // SAFETY: handle is a live process handle.
    let pid = unsafe { win::GetProcessId(handle.as_raw_handle().cast()) };
    if pid == 0 {
        return Err(last_error("GetProcessId"));
    }
    Ok(ProcessHandle {
        _handle: handle,
        pid,
    })
}

#[doc(hidden)]
#[cfg(feature = "test-launcher")]
pub fn launch_worker_for_test(
    executable: &Path,
    bootstrap: &str,
) -> Result<ProcessHandle, TransportError> {
    let child = std::process::Command::new(executable)
        .arg(bootstrap)
        .spawn()
        .map_err(TransportError::Launch)?;
    let pid = child.id();
    let raw = child.into_raw_handle();
    // SAFETY: into_raw_handle transfers the process handle out of Child.
    let handle = unsafe { OwnedHandle::from_raw_handle(raw) };
    Ok(ProcessHandle {
        _handle: handle,
        pid,
    })
}

fn current_process_token() -> Result<OwnedHandle, TransportError> {
    let mut token = null_mut();
    // SAFETY: output pointer is valid and GetCurrentProcess returns a pseudo-handle.
    if unsafe { win::OpenProcessToken(win::GetCurrentProcess(), win::TOKEN_QUERY, &mut token) } == 0
    {
        return Err(last_error("OpenProcessToken"));
    }
    // SAFETY: OpenProcessToken returned an owned kernel handle.
    Ok(unsafe { OwnedHandle::from_raw_handle(token.cast()) })
}
fn sid_to_string(sid: *const c_void) -> Result<String, TransportError> {
    let mut string = null_mut();
    // SAFETY: sid is valid and the API allocates a NUL-terminated result.
    if unsafe { win::ConvertSidToStringSidW(sid, &mut string) } == 0 {
        return Err(last_error("ConvertSidToStringSidW"));
    }
    let mut len = 0; /* SAFETY: string is NUL terminated. */
    while unsafe { *string.add(len) } != 0 {
        len += 1;
    }
    // SAFETY: the scan found the allocation terminator.
    let display = String::from_utf16(unsafe { std::slice::from_raw_parts(string, len) })
        .map_err(|_| TransportError::SecurityDescriptorFailed("invalid SID string".into()))?;
    // SAFETY: string was allocated by LocalAlloc.
    unsafe {
        win::LocalFree(string.cast());
    }
    Ok(display)
}
fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain([0]).collect()
}
fn wide_os(value: &std::ffi::OsStr) -> Vec<u16> {
    value.encode_wide().chain([0]).collect()
}
fn last_error(api: &'static str) -> TransportError {
    TransportError::Win32 {
        api,
        code: last_error_code(),
    }
}
fn last_error_code() -> u32 {
    /* SAFETY: no preconditions. */
    unsafe { win::GetLastError() }
}
