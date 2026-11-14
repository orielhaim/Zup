use std::collections::BTreeMap;
use windows::Win32::{
    Foundation::{ERROR_FILE_NOT_FOUND, HANDLE, HLOCAL, LocalFree},
    Security::{
        ACCESS_ALLOWED_ACE, ACE_HEADER,
        Authorization::{ConvertSidToStringSidW, GetSecurityInfo, SE_KERNEL_OBJECT},
        GetAce, GetSecurityDescriptorControl, LookupAccountNameW, PSECURITY_DESCRIPTOR, PSID,
        SE_DACL_PRESENT, SE_DACL_PROTECTED, SID_NAME_USE,
    },
    System::{
        JobObjects::{
            IsProcessInJob, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JobObjectBasicAccountingInformation, JobObjectExtendedLimitInformation, OpenJobObjectW,
            QueryInformationJobObject,
        },
        Threading::{
            OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
            QueryFullProcessImageNameW,
        },
    },
};
use windows::core::{Owned, PCWSTR, PWSTR};
use zup_plan::{ServiceControlOperation, ServiceScopeObservation, WindowsJobState};

const OBSERVE: u32 = 0x0002_0004;
const FINAL_ACCESS: u32 = 0x0012_000c;

pub(crate) struct ServiceScope {
    handle: Owned<HANDLE>,
    member: String,
    owner: String,
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

struct Descriptor(PSECURITY_DESCRIPTOR);
impl Drop for Descriptor {
    fn drop(&mut self) {
        unsafe {
            let _ = LocalFree(Some(HLOCAL(self.0.0)));
        }
    }
}

fn sid_text(sid: PSID) -> Result<String, String> {
    let mut text = PWSTR::null();
    unsafe { ConvertSidToStringSidW(sid, &mut text) }.map_err(|e| e.to_string())?;
    let result = unsafe { text.to_string() }.map_err(|e| e.to_string());
    unsafe {
        let _ = LocalFree(Some(HLOCAL(text.0.cast())));
    }
    result
}

fn service_sid(name: &str) -> Result<String, String> {
    let name = wide(&format!("NT SERVICE\\{name}"));
    let mut bytes = [0u64; 16];
    let mut size = size_of_val(&bytes) as u32;
    let mut domain = [0u16; 256];
    let mut domain_size = domain.len() as u32;
    let mut kind = SID_NAME_USE::default();
    unsafe {
        LookupAccountNameW(
            None,
            PCWSTR(name.as_ptr()),
            Some(PSID(bytes.as_mut_ptr().cast())),
            &mut size,
            Some(PWSTR(domain.as_mut_ptr())),
            &mut domain_size,
            &mut kind,
        )
    }
    .map_err(|e| e.to_string())?;
    sid_text(PSID(bytes.as_mut_ptr().cast()))
}

fn expected_owner(operation: &ServiceControlOperation) -> Result<&'static str, String> {
    use zup_core::{ServiceExecution, WindowsServiceAccount};
    match &operation.configuration {
        zup_plan::ServiceState::Registration {
            execution: ServiceExecution::WindowsJob { account },
            ..
        } => Ok(match account {
            WindowsServiceAccount::LocalSystem => "S-1-5-18",
            WindowsServiceAccount::LocalService => "S-1-5-19",
            WindowsServiceAccount::NetworkService => "S-1-5-20",
        }),
        _ => Err("Windows process scope requires SCM registration".into()),
    }
}

fn policy(
    handle: HANDLE,
    operation: &ServiceControlOperation,
    expected_owner: &str,
) -> Result<(), String> {
    let mut owner = PSID::default();
    let mut acl = std::ptr::null_mut();
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    unsafe {
        GetSecurityInfo(
            handle,
            SE_KERNEL_OBJECT,
            windows::Win32::Security::OBJECT_SECURITY_INFORMATION(5),
            Some(&mut owner),
            None,
            Some(&mut acl),
            None,
            Some(&mut descriptor),
        )
    }
    .ok()
    .map_err(|e| e.to_string())?;
    let descriptor = Descriptor(descriptor);
    if acl.is_null() || sid_text(owner)? != expected_owner {
        return Err("service job owner or access policy is untrusted".into());
    }
    let mut flags = Default::default();
    let mut revision = 0;
    unsafe { GetSecurityDescriptorControl(descriptor.0, &mut flags, &mut revision) }
        .map_err(|e| e.to_string())?;
    if flags & (SE_DACL_PRESENT | SE_DACL_PROTECTED).0 != (SE_DACL_PRESENT | SE_DACL_PROTECTED).0 {
        return Err("service job access policy must be protected".into());
    }
    let expected = BTreeMap::from([
        ("S-1-5-18".to_owned(), FINAL_ACCESS),
        ("S-1-5-32-544".to_owned(), FINAL_ACCESS),
        ("S-1-3-4".to_owned(), 0x0002_0000),
        (service_sid(&operation.native_name)?, FINAL_ACCESS),
    ]);
    let mut observed = BTreeMap::new();
    let count = unsafe { (*acl).AceCount };
    if usize::from(count) != expected.len() {
        return Err("service job contains unexpected permission entries".into());
    }
    for index in 0..u32::from(count) {
        let mut raw = std::ptr::null_mut();
        unsafe { GetAce(acl, index, &mut raw) }.map_err(|e| e.to_string())?;
        let header = unsafe { &*raw.cast::<ACE_HEADER>() };
        if header.AceType != 0
            || header.AceFlags != 0
            || usize::from(header.AceSize) < size_of::<ACCESS_ALLOWED_ACE>()
        {
            return Err("service job contains an unsupported permission entry".into());
        }
        let ace = unsafe { &*raw.cast::<ACCESS_ALLOWED_ACE>() };
        let sid = sid_text(PSID((&raw const ace.SidStart).cast_mut().cast()))?;
        if observed.insert(sid, ace.Mask).is_some() {
            return Err("service job permission entries are duplicated".into());
        }
    }
    if observed != expected {
        return Err("service job permissions allow mutation or omit required authority".into());
    }
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    unsafe {
        QueryInformationJobObject(
            Some(handle),
            JobObjectExtendedLimitInformation,
            (&raw mut limits).cast(),
            size_of_val(&limits) as u32,
            None,
        )
    }
    .map_err(|e| e.to_string())?;
    if limits.BasicLimitInformation.LimitFlags != JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE {
        return Err("service job must kill on final close and prohibit all breakaways".into());
    }
    Ok(())
}

impl ServiceScope {
    pub(crate) fn open(operation: &ServiceControlOperation) -> Result<Option<Self>, String> {
        let zup_core::ResourceKey::Service { id } = &operation.key else {
            return Err("service process scope requires logical identity".into());
        };
        let claim = operation
            .registration
            .claim(id.clone())
            .ok_or("service job requires installation ownership")?;
        let name = wide(&zup_core::service_job_name(claim.installation, id));
        let handle = match unsafe { OpenJobObjectW(OBSERVE, false, PCWSTR(name.as_ptr())) } {
            Ok(handle) => unsafe { Owned::new(handle) },
            Err(e) if e.code() == ERROR_FILE_NOT_FOUND.to_hresult() => return Ok(None),
            Err(e) => return Err(format!("service job is unavailable: {e}")),
        };
        let owner = expected_owner(operation)?.to_owned();
        policy(*handle, operation, &owner)?;
        Ok(Some(Self {
            handle,
            member: String::new(),
            owner,
        }))
    }

    pub(crate) fn observe(
        &self,
        operation: &ServiceControlOperation,
    ) -> Result<WindowsJobState, String> {
        policy(*self.handle, operation, &self.owner)?;
        let mut accounting = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
        unsafe {
            QueryInformationJobObject(
                Some(*self.handle),
                JobObjectBasicAccountingInformation,
                (&raw mut accounting).cast(),
                size_of_val(&accounting) as u32,
                None,
            )
        }
        .map_err(|e| e.to_string())?;
        Ok(
            match std::num::NonZeroU32::new(accounting.ActiveProcesses) {
                Some(processes) => WindowsJobState::Occupied { processes },
                None => WindowsJobState::Empty,
            },
        )
    }

    pub(crate) fn bind_member(
        &mut self,
        operation: &ServiceControlOperation,
        pid: u32,
        generation: &str,
        executable: &zup_core::TargetPath,
    ) -> Result<(), String> {
        let process = unsafe {
            Owned::new(
                OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)
                    .map_err(|e| e.to_string())?,
            )
        };
        if super::service_control::process_generation(pid, *process)? != generation {
            return Err("service process changed during scope observation".into());
        }
        let mut member = Default::default();
        unsafe { IsProcessInJob(*process, Some(*self.handle), &mut member) }
            .map_err(|e| e.to_string())?;
        if !member.as_bool() {
            return Err("SCM process has not joined its declared native job".into());
        }
        let mut path = vec![0u16; 32768];
        let mut length = path.len() as u32;
        unsafe {
            QueryFullProcessImageNameW(
                *process,
                PROCESS_NAME_WIN32,
                PWSTR(path.as_mut_ptr()),
                &mut length,
            )
        }
        .map_err(|e| e.to_string())?;
        let path = String::from_utf16(&path[..length as usize]).map_err(|e| e.to_string())?;
        if !std::path::Path::new(&path)
            .as_os_str()
            .eq_ignore_ascii_case(crate::lowering::host_path(executable).as_os_str())
        {
            return Err("running service image differs from its pinned deployment path".into());
        }
        policy(*self.handle, operation, &self.owner)?;
        self.member = generation.to_owned();
        Ok(())
    }

    pub(crate) fn was_member(&self, generation: &str) -> bool {
        !self.member.is_empty() && self.member == generation
    }
}

pub(crate) fn observation(state: WindowsJobState) -> Result<ServiceScopeObservation, String> {
    Ok(ServiceScopeObservation::WindowsJob {
        boot_sequence: boot_sequence()?,
        state,
    })
}

pub(crate) fn boot_sequence() -> Result<u32, String> {
    use windows::Win32::System::{
        LibraryLoader::{GetModuleHandleW, GetProcAddress},
        Threading::GetCurrentProcess,
    };
    type Query =
        unsafe extern "system" fn(HANDLE, u32, *mut std::ffi::c_void, u32, *mut u32) -> i32;
    // The telemetry header is documented; the native query is capability-tested and may be unavailable.
    unsafe {
        let module = GetModuleHandleW(windows::core::w!("ntdll.dll")).map_err(|e| e.to_string())?;
        let address = GetProcAddress(module, windows::core::s!("NtQueryInformationProcess"))
            .ok_or("native process telemetry query is unavailable")?;
        let query: Query = std::mem::transmute(address);
        let mut bytes = vec![0u64; 16384];
        let mut returned = 0;
        let status = query(
            GetCurrentProcess(),
            64,
            bytes.as_mut_ptr().cast(),
            size_of_val(bytes.as_slice()) as u32,
            &mut returned,
        );
        let data =
            std::slice::from_raw_parts(bytes.as_ptr().cast::<u8>(), size_of_val(bytes.as_slice()));
        let word = |offset| {
            u32::from_ne_bytes(data[offset..offset + 4].try_into().expect("header bounds"))
        };
        if status != 0
            || returned < 96
            || returned as usize > data.len()
            || word(0) != 96
            || word(0) > returned
            || word(4) != std::process::id()
        {
            return Err("native process boot sequence is unsupported; service recovery cannot be authorized".into());
        }
        let generation =
            super::service_control::process_generation(std::process::id(), GetCurrentProcess())?;
        let created = u64::from_ne_bytes(data[16..24].try_into().expect("header bounds"));
        if generation
            != format!(
                "{}:{}:{}",
                std::process::id(),
                created >> 32,
                created as u32
            )
        {
            return Err("native telemetry does not identify the current process".into());
        }
        Ok(word(60))
    }
}
