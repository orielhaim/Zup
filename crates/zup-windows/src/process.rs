#![allow(non_snake_case, dead_code)]

use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use windows_link::link;

use crate::bindings::{Bool, CloseHandle, Dword, GetLastError, Handle, WaitForSingleObject};

const STARTF_USESTDHANDLES: Dword = 0x0000_0100;
const CREATE_UNICODE_ENVIRONMENT: Dword = 0x0000_0400;
const CREATE_NO_WINDOW: Dword = 0x0800_0000;
const CREATE_NEW_PROCESS_GROUP: Dword = 0x0000_0200;
const INFINITE: Dword = 0xffff_ffff;
const STILL_ACTIVE: Dword = 259;
const SW_HIDE: i32 = 0;
const SW_SHOWNORMAL: i32 = 1;

pub const PROC_THREAD_ATTRIBUTE_HANDLE_LIST: usize = 0x0002_0002;

#[repr(C)]
#[derive(Clone, Copy)]
struct StartupInfoExW {
    StartupInfo: StartupInfoW,
    AttributeList: *mut c_void,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct StartupInfoW {
    cb: Dword,
    lpReserved: *mut u16,
    lpDesktop: *mut u16,
    lpTitle: *mut u16,
    dwX: Dword,
    dwY: Dword,
    dwXSize: Dword,
    dwYSize: Dword,
    dwXCountChars: Dword,
    dwYCountChars: Dword,
    dwFillAttribute: Dword,
    dwFlags: Dword,
    wShowWindow: u16,
    cbReserved2: u16,
    lpReserved2: *mut u8,
    hStdInput: Handle,
    hStdOutput: Handle,
    hStdError: Handle,
}

#[repr(C)]
struct ProcessInformation {
    Process: Handle,
    Thread: Handle,
    dwProcessId: Dword,
    dwThreadId: Dword,
}

#[repr(C)]
struct SecurityAttributes {
    nLength: Dword,
    lpSecurityDescriptor: *mut c_void,
    bInheritHandle: Bool,
}

link!("kernel32.dll" "system" fn CreateProcessW(
    application: *const u16,
    command_line: *mut u16,
    process_attributes: *const SecurityAttributes,
    thread_attributes: *const SecurityAttributes,
    inherit_handles: Bool,
    creation_flags: Dword,
    environment: *mut c_void,
    current_directory: *const u16,
    startup_info: *mut StartupInfoExW,
    process_information: *mut ProcessInformation
) -> Bool);
link!("kernel32.dll" "system" fn InitializeProcThreadAttributeList(attributes: *mut c_void, count: Dword, flags: Dword, size: *mut usize) -> Bool);
link!("kernel32.dll" "system" fn UpdateProcThreadAttribute(attributes: *mut c_void, flags: Dword, attribute: usize, value: *mut c_void, size: usize, previous: *mut c_void, return_size: *mut usize) -> Bool);
link!("kernel32.dll" "system" fn DeleteProcThreadAttributeList(attributes: *mut c_void) -> *mut c_void);
link!("kernel32.dll" "system" fn GetExitCodeProcess(process: Handle, code: *mut Dword) -> Bool);
link!("kernel32.dll" "system" fn GetStdHandle(which: Dword) -> Handle);

const STD_INPUT_HANDLE: Dword = 0xffff_fffe;
const STD_OUTPUT_HANDLE: Dword = 0xffff_ffff;
const STD_ERROR_HANDLE: Dword = 0xffff_fffd;
const INVALID_HANDLE_VALUE: Handle = -1isize as Handle;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandOff {
    Silent,

    Console,
}

pub struct ChildProcess {
    process: Handle,
    thread: Handle,
    pid: u32,

    forward: bool,

    reported: Option<i32>,
}

impl ChildProcess {
    pub const fn pid(&self) -> u32 {
        self.pid
    }

    pub fn already_finished(code: i32) -> Self {
        Self {
            process: std::ptr::null_mut(),
            thread: std::ptr::null_mut(),
            pid: 0,
            forward: true,
            reported: Some(code),
        }
    }

    pub fn wait(self) -> i32 {
        if let Some(code) = self.reported {
            return code;
        }
        if !self.forward {
            return 0;
        }
        unsafe {
            WaitForSingleObject(self.process, INFINITE);
            let mut code: Dword = 0;
            if GetExitCodeProcess(self.process, &mut code) == 0 {
                return 1;
            }
            if code == STILL_ACTIVE {
                return 1;
            }
            i32::try_from(code).unwrap_or(1)
        }
    }
}

impl Drop for ChildProcess {
    fn drop(&mut self) {
        unsafe {
            if !self.process.is_null() && self.process != INVALID_HANDLE_VALUE {
                CloseHandle(self.process);
            }
            if !self.thread.is_null() && self.thread != INVALID_HANDLE_VALUE {
                CloseHandle(self.thread);
            }
        }
    }
}

impl std::fmt::Debug for ChildProcess {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ChildProcess")
            .field("pid", &self.pid)
            .field("real", &self.reported.is_none())
            .field("forwards_exit_code", &self.forward)
            .finish()
    }
}

#[derive(Debug, thiserror::Error)]
#[error("starting the native runtime failed at {api} (Win32 {code})")]
pub struct LaunchError {
    pub api: &'static str,
    pub code: u32,
}

fn last_error(api: &'static str) -> LaunchError {
    LaunchError {
        api,
        code: unsafe { GetLastError() },
    }
}

fn wide(value: &str) -> Vec<u16> {
    std::ffi::OsStr::new(value)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn wide_os(value: &std::ffi::OsStr) -> Vec<u16> {
    value.encode_wide().chain(std::iter::once(0)).collect()
}

pub fn launch(
    executable: &Path,
    arguments: &[String],
    handoff: HandOff,
    working_directory: Option<&Path>,
) -> Result<ChildProcess, LaunchError> {
    let application = wide_os(executable.as_os_str());

    let mut command_line = wide(&quote(executable, arguments));

    let mut inherited: Vec<Handle> = Vec::new();
    let mut standard: [Handle; 3] = [std::ptr::null_mut(); 3];
    let mut flags: Dword = 0;
    match handoff {
        HandOff::Silent => {
            flags |= CREATE_NO_WINDOW;
        }
        HandOff::Console => {
            flags |= CREATE_NEW_PROCESS_GROUP;
            for (slot, which) in
                standard
                    .iter_mut()
                    .zip([STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE])
            {
                let handle = unsafe { GetStdHandle(which) };
                if handle.is_null() || handle == INVALID_HANDLE_VALUE {
                    continue;
                }
                *slot = handle;
                inherited.push(handle);
            }
        }
    }

    let mut startup = StartupInfoExW {
        StartupInfo: StartupInfoW {
            cb: std::mem::size_of::<StartupInfoExW>() as Dword,
            ..StartupInfoW::default()
        },
        AttributeList: std::ptr::null_mut(),
    };
    startup.StartupInfo.wShowWindow = match handoff {
        HandOff::Silent => SW_HIDE as u16,
        HandOff::Console => SW_SHOWNORMAL as u16,
    };

    if !inherited.is_empty() {
        startup.StartupInfo.dwFlags |= STARTF_USESTDHANDLES;
        startup.StartupInfo.hStdInput = standard[0];
        startup.StartupInfo.hStdOutput = standard[1];
        startup.StartupInfo.hStdError = standard[2];
    }

    let mut attribute_size = std::mem::size_of::<usize>() * 2;
    unsafe {
        InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut attribute_size);
    }
    let attributes = vec![0u8; attribute_size];
    let attribute_list = attributes.as_ptr() as *mut c_void;
    unsafe {
        if InitializeProcThreadAttributeList(attribute_list, 1, 0, &mut attribute_size) == 0 {
            return Err(last_error("InitializeProcThreadAttributeList"));
        }
        startup.AttributeList = attribute_list;
        let mut handles = inherited.clone();
        UpdateProcThreadAttribute(
            attribute_list,
            0,
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
            handles.as_mut_ptr() as *mut c_void,
            std::mem::size_of::<Handle>() * handles.len().max(1),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
    }

    let current_directory = working_directory.map(|path| wide_os(path.as_os_str()));
    let current_directory = current_directory
        .as_ref()
        .map(|path| path.as_ptr())
        .unwrap_or(std::ptr::null());

    let mut information = ProcessInformation {
        Process: std::ptr::null_mut(),
        Thread: std::ptr::null_mut(),
        dwProcessId: 0,
        dwThreadId: 0,
    };

    let started = unsafe {
        CreateProcessW(
            application.as_ptr(),
            command_line.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1,
            flags | CREATE_UNICODE_ENVIRONMENT,
            std::ptr::null_mut(),
            current_directory,
            &mut startup,
            &mut information,
        )
    };
    unsafe {
        DeleteProcThreadAttributeList(startup.AttributeList);
    }
    if started == 0 {
        return Err(last_error("CreateProcessW"));
    }
    let pid = information.dwProcessId;
    Ok(ChildProcess {
        process: information.Process,
        thread: information.Thread,
        pid,

        forward: handoff == HandOff::Console,
        reported: None,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchRequest {
    pub executable: PathBuf,

    pub arguments: Vec<String>,

    pub handoff: HandOff,

    pub working_directory: Option<PathBuf>,
}

impl LaunchRequest {
    pub fn new(executable: impl Into<PathBuf>, arguments: Vec<String>, handoff: HandOff) -> Self {
        Self {
            executable: executable.into(),
            arguments,
            handoff,
            working_directory: None,
        }
    }

    pub fn command_line(&self) -> String {
        quote(&self.executable, &self.arguments)
    }
}

pub trait Launcher {
    fn launch(&self, request: &LaunchRequest) -> Result<ChildProcess, LaunchError>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct WindowsLauncher;

impl Launcher for WindowsLauncher {
    fn launch(&self, request: &LaunchRequest) -> Result<ChildProcess, LaunchError> {
        launch(
            &request.executable,
            &request.arguments,
            request.handoff,
            request.working_directory.as_deref(),
        )
    }
}

pub fn quote_argument(argument: &str) -> String {
    if !argument.is_empty()
        && !argument.contains([' ', '\t', '\n', '\u{0B}', '"'])
        && !argument.ends_with('\\')
    {
        return argument.to_owned();
    }
    let mut out = String::with_capacity(argument.len() + 2);
    out.push('"');
    let mut backslashes = 0usize;
    for character in argument.chars() {
        if character == '\\' {
            backslashes += 1;
            continue;
        }
        if character == '"' {
            out.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
        } else {
            out.extend(std::iter::repeat_n('\\', backslashes));
        }
        backslashes = 0;
        out.push(character);
    }
    out.extend(std::iter::repeat_n('\\', backslashes * 2));
    out.push('"');
    out
}

fn quote(executable: &Path, arguments: &[String]) -> String {
    let mut out = quote_argument(&executable.to_string_lossy());
    for argument in arguments {
        out.push(' ');
        out.push_str(&quote_argument(argument));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case("simple", "simple")]
    #[case("two words", "\"two words\"")]
    #[case("", "\"\"")]
    #[case("a\"b", "\"a\\\"b\"")]
    #[case("C:\\path\\", "\"C:\\path\\\\\"")]
    #[case("a\\\\", "\"a\\\\\\\\\"")]
    fn arguments_quote_and_escape(#[case] argument: &str, #[case] expected: &str) {
        assert_eq!(quote_argument(argument), expected);
    }
}
