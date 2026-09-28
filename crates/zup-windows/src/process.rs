//! Starting the native runtime, with the narrowest inheritance that works.
//!
//! A thin bootstrapper has to start the installer it just verified, and the way
//! it does that is the whole difference between one installer window and two.
//! The rules this module follows:
//!
//! - **`CreateProcessW`, never a shell.** No `cmd.exe`, no command string, no
//!   `PATH` lookup, no self-replacement. The executable path is the one the
//!   acquisition verified, and it is passed as a path.
//! - **Inheritance is a list, not a flag.** `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`
//!   names the handles the child may keep. Broad inheritance (`bInheritHandles`
//!   with an unrestricted list) would hand a network-facing installer process
//!   every handle the bootstrapper happened to hold, which includes the TUF
//!   datastore and the content cache's open writers.
//! - **A windowed handoff inherits nothing.** The bootstrapper is itself a
//!   windowed process with no console, so there is nothing to pass on, and the
//!   child creates its own window. The user sees one installer.
//! - **A console handoff inherits exactly the three standard handles.** That is
//!   what makes `stdout` and `stderr` survive the handoff, and nothing else does.
//!
//! # No process-tree correctness dependency
//!
//! On Windows a child is independent once `CreateProcess` returns. The child
//! does not read anything from this process, and the installation it performs is
//! journalled and committed by the transaction engine in the child's own
//! context. So waiting here is a *presentation* choice - to forward an exit code
//! and to keep a console attached - and never a correctness one. A bootstrapper
//! that is killed mid-install leaves an installation that is either committed or
//! recoverable, which is the same guarantee the offline artifact has always had.

// The Win32 structures below mirror `STARTUPINFOEXW` and friends exactly. Their
// field names are the C names, because a renamed field is a field a reader
// cannot check against the documentation.
#![allow(non_snake_case, dead_code)]

use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use windows_link::link;

use crate::transport_bindings::{
    Bool, CloseHandle, Dword, GetLastError, Handle, WaitForSingleObject,
};

const STARTF_USESTDHANDLES: Dword = 0x0000_0100;
const CREATE_UNICODE_ENVIRONMENT: Dword = 0x0000_0400;
const CREATE_NO_WINDOW: Dword = 0x0800_0000;
const CREATE_NEW_PROCESS_GROUP: Dword = 0x0000_0200;
const INFINITE: Dword = 0xffff_ffff;
const STILL_ACTIVE: Dword = 259;
const SW_HIDE: i32 = 0;
const SW_SHOWNORMAL: i32 = 1;

/// `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`.
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

/// What the child should see of this process's console.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandOff {
    /// No window, no console, no inherited handles.
    ///
    /// This is the GUI handoff. The bootstrapper shows nothing, the runtime
    /// opens its own window, and the user sees one installer.
    Silent,
    /// The three standard handles, and nothing else.
    ///
    /// This is the console and headless handoff. `stdout` and `stderr` survive,
    /// which is what a headless consumer's JSONL stream depends on.
    Console,
}

/// A started child process.
///
/// Holding this is optional in the strongest sense: the child does not need the
/// parent. `wait` is a presentation call.
pub struct ChildProcess {
    process: Handle,
    thread: Handle,
    pid: u32,
    /// Whether the caller asked to be told when it finishes.
    forward: bool,
    /// A code a launcher reports instead of waiting.
    ///
    /// `None` means a real child. `Some` means the [`Launcher`] that produced
    /// this decided the child's outcome itself, which is not a test-only
    /// capability: an interface whose return type an implementor cannot construct
    /// is not an interface. A dry run, a recorded launcher, and a test are three
    /// answers to the same question - what *would* you start - and they all need
    /// to hand back a child.
    reported: Option<i32>,
}

impl ChildProcess {
    /// The child's process id, for a diagnostic.
    pub const fn pid(&self) -> u32 {
        self.pid
    }

    /// A child that has already finished with `code`.
    ///
    /// For a [`Launcher`] that does not start a process. The process id is zero,
    /// which is the honest answer: nothing was started, and a diagnostic that
    /// printed a real-looking id for a launch that never happened would be worse
    /// than one that printed nothing.
    pub fn already_finished(code: i32) -> Self {
        Self {
            process: std::ptr::null_mut(),
            thread: std::ptr::null_mut(),
            pid: 0,
            forward: true,
            reported: Some(code),
        }
    }

    /// Wait for the child and return its exit code.
    ///
    /// A child killed by a signal has no exit code; the caller gets the same
    /// value a shell would report, which is what a caller's own reporting
    /// expects.
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
    /// The process id and whether the child is real, and never a handle value.
    ///
    /// A handle printed in a panic is a handle that ends up in a log, and a
    /// handle in a log is a value another process can guess. The two facts worth
    /// printing are which process this is and whether there is one.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ChildProcess")
            .field("pid", &self.pid)
            .field("real", &self.reported.is_none())
            .field("forwards_exit_code", &self.forward)
            .finish()
    }
}

/// Why a child could not be started.
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

/// Start `executable` with `arguments`.
///
/// The executable is passed as an application name *and* as the first token of
/// the command line, which is what `CreateProcessW` documents for an unquoted
/// path: the two must agree, and passing the path only in the command line
/// would make it a `PATH` lookup.
pub fn launch(
    executable: &Path,
    arguments: &[String],
    handoff: HandOff,
    working_directory: Option<&Path>,
) -> Result<ChildProcess, LaunchError> {
    let application = wide_os(executable.as_os_str());
    // A mutable buffer is required: `CreateProcessW` is documented to be able
    // to modify the command line in place.
    let mut command_line = wide(&quote(executable, arguments));

    let mut inherited: Vec<Handle> = Vec::new();
    let mut standard: [Handle; 3] = [std::ptr::null_mut(); 3];
    let mut flags: Dword = 0;
    match handoff {
        HandOff::Silent => {
            // A windowed child needs no console and no handles. `CREATE_NO_WINDOW`
            // rather than `DETACHED_PROCESS`: the latter would also detach the
            // process from the launcher's console for a *console* build of the
            // bootstrapper, and this is the branch that says "GUI".
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
                    // A process with no console - a service, a scheduled task, a
                    // session with no interactive desktop - has no standard
                    // handles. That is not a failure: the child gets what this
                    // process has, which is nothing, and the caller finds out by
                    // the child's own output. Indexing into a list that is empty
                    // here would turn "no console" into a panic, which is the
                    // worst possible answer to it.
                    continue;
                }
                *slot = handle;
                inherited.push(handle);
            }
        }
    }

    let mut startup = StartupInfoExW {
        StartupInfo: StartupInfoW {
            // The size covers the extended structure, which is what
            // `STARTUPINFOEXW` requires and what tells `CreateProcessW` to read
            // `AttributeList` at all.
            cb: std::mem::size_of::<StartupInfoExW>() as Dword,
            ..StartupInfoW::default()
        },
        AttributeList: std::ptr::null_mut(),
    };
    startup.StartupInfo.wShowWindow = match handoff {
        HandOff::Silent => SW_HIDE as u16,
        HandOff::Console => SW_SHOWNORMAL as u16,
    };
    // `STARTF_USESTDHANDLES` is set only when there is something to use. Setting
    // it with null handles is documented to leave the child with no standard
    // handles *and* to ignore the attribute list's own answer, which would turn a
    // partial console into a fully silent child.
    if !inherited.is_empty() {
        startup.StartupInfo.dwFlags |= STARTF_USESTDHANDLES;
        startup.StartupInfo.hStdInput = standard[0];
        startup.StartupInfo.hStdOutput = standard[1];
        startup.StartupInfo.hStdError = standard[2];
    }

    // The attribute list exists for exactly one reason: to make inheritance a
    // list. Without it the only way to inherit the console handles is
    // `bInheritHandles = TRUE` with no restriction, which is broad inheritance.
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
    // `bInheritHandles` is set whenever a handle list is present, even if that
    // list is empty: the list is what constrains it, and the documented
    // behaviour of an empty list with the attribute present is that nothing is
    // inherited.
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
        // The console handoff waits so the caller gets a coherent result and the
        // terminal experience is preserved; the silent one does not, because a
        // user who closes the installer window should not have a hidden process
        // sitting on the exit code.
        forward: handoff == HandOff::Console,
        reported: None,
    })
}

/// Everything a launch is, as data.
///
/// A request rather than four positional arguments because a caller has to be
/// able to *inspect* one: the security property this module exists for is which
/// handles a child inherits and which bytes it is given, and a property about a
/// value cannot be tested without being able to hold the value. A
/// [`Launcher`] that records requests and starts nothing is how that is tested
/// without a child process, and it is why the product's handoff has an E2E story
/// that does not depend on spawning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchRequest {
    /// The verified runtime image. Passed as an application name, so it is never
    /// a `PATH` lookup and never a shell's idea of a command.
    pub executable: PathBuf,
    /// The arguments, already quoted when rendered.
    pub arguments: Vec<String>,
    /// What the child sees of this process's console.
    pub handoff: HandOff,
    /// The child's working directory, when it is not inherited.
    pub working_directory: Option<PathBuf>,
}

impl LaunchRequest {
    /// A request that inherits nothing and changes nothing else.
    pub fn new(executable: impl Into<PathBuf>, arguments: Vec<String>, handoff: HandOff) -> Self {
        Self {
            executable: executable.into(),
            arguments,
            handoff,
            working_directory: None,
        }
    }

    /// The command line this request renders to.
    ///
    /// Exposed because the rendered line *is* the interface: if this is wrong,
    /// the child receives something other than what was asked for, and a test
    /// that only counted arguments would not notice.
    pub fn command_line(&self) -> String {
        quote(&self.executable, &self.arguments)
    }
}

/// Something that can start a process.
///
/// `ChildProcess` is concrete rather than an associated type because a caller
/// that injected a launcher wants to be able to wait on what it started, and
/// because the only implementation is this module's. A test implementation
/// returns a child that has already exited.
pub trait Launcher {
    /// Start `request`.
    fn launch(&self, request: &LaunchRequest) -> Result<ChildProcess, LaunchError>;
}

/// The real launcher: `CreateProcessW` with a handle list.
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

/// Quote one argument for a Windows command line.
///
/// The rules are `CommandLineToArgvW`'s, and getting them wrong is how an
/// installer's arguments turn into something else. A path with no space and no
/// quote is passed through; everything else is quoted, and a trailing backslash
/// is doubled so the closing quote is not absorbed into an escape.
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

    #[test]
    fn an_argument_with_a_space_is_quoted() {
        assert_eq!(quote_argument("simple"), "simple");
        assert_eq!(quote_argument("two words"), "\"two words\"");
        assert_eq!(quote_argument(""), "\"\"");
    }

    #[test]
    fn a_quote_inside_an_argument_is_escaped() {
        assert_eq!(quote_argument("a\"b"), "\"a\\\"b\"");
        // A backslash before a non-quote is literal; a run before the closing
        // quote is doubled, or the quote would be absorbed into the escape.
        assert_eq!(quote_argument("C:\\path\\"), "\"C:\\path\\\\\"");
        assert_eq!(quote_argument("a\\\\"), "\"a\\\\\\\\\"");
    }
}
