use windows::Win32::{
    Foundation::{
        DuplicateHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HLOCAL, LocalFree,
        SetLastError,
    },
    Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        SDDL_REVISION_1, SE_KERNEL_OBJECT, SetSecurityInfo,
    },
    Security::{
        GetTokenInformation, LookupAccountNameW, PSID, SECURITY_ATTRIBUTES, SID_NAME_USE,
        TOKEN_QUERY, TOKEN_USER, TokenUser,
    },
    System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    },
    System::Threading::{GetCurrentProcess, OpenProcessToken},
};
use windows::core::{Owned, PCWSTR, PWSTR};

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

unsafe fn sid_text(sid: PSID) -> windows::core::Result<String> {
    let mut text = PWSTR::null();
    unsafe {
        ConvertSidToStringSidW(sid, &mut text)?;
    }
    let result = unsafe { text.to_string() };
    unsafe {
        LocalFree(Some(HLOCAL(text.0.cast())));
    }
    Ok(result?)
}

pub fn create(service: &str, name: &str) -> windows::core::Result<Owned<HANDLE>> {
    use windows::Win32::Security::{
        DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, PROTECTED_DACL_SECURITY_INFORMATION,
        PSECURITY_DESCRIPTOR,
    };
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)?;
        let token = Owned::new(token);
        let mut bytes = [0u64; 64];
        let mut required = 0;
        GetTokenInformation(
            *token,
            TokenUser,
            Some(bytes.as_mut_ptr().cast()),
            size_of_val(&bytes) as u32,
            &mut required,
        )?;
        let owner = sid_text((*bytes.as_ptr().cast::<TOKEN_USER>()).User.Sid)?;
        let mut sid = [0u64; 16];
        let mut sid_size = size_of_val(&sid) as u32;
        let mut domain = [0u16; 256];
        let mut domain_size = domain.len() as u32;
        let mut use_kind = SID_NAME_USE::default();
        let account = wide(&format!("NT SERVICE\\{service}"));
        LookupAccountNameW(
            None,
            PCWSTR(account.as_ptr()),
            Some(PSID(sid.as_mut_ptr().cast())),
            &mut sid_size,
            Some(PWSTR(domain.as_mut_ptr())),
            &mut domain_size,
            &mut use_kind,
        )?;
        let service_sid = sid_text(PSID(sid.as_mut_ptr().cast()))?;
        let descriptor = |mask| -> windows::core::Result<PSECURITY_DESCRIPTOR> {
            let text = wide(&format!(
                "O:{owner}D:P(A;;0x0012000c;;;SY)(A;;0x0012000c;;;BA)(A;;RC;;;OW)(A;;{mask};;;{service_sid})"
            ));
            let mut result = PSECURITY_DESCRIPTOR::default();
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(text.as_ptr()),
                SDDL_REVISION_1,
                &mut result,
                None,
            )?;
            Ok(result)
        };
        let initial = descriptor("0x001f003f")?;
        let attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: initial.0,
            bInheritHandle: false.into(),
        };
        let name = wide(name);
        SetLastError(windows::Win32::Foundation::ERROR_SUCCESS);
        let created = CreateJobObjectW(Some(&attributes), PCWSTR(name.as_ptr()));
        let existed = GetLastError() == ERROR_ALREADY_EXISTS;
        LocalFree(Some(HLOCAL(initial.0)));
        let job = Owned::new(created?);
        if existed {
            return Err(windows::core::Error::from_hresult(
                ERROR_ALREADY_EXISTS.to_hresult(),
            ));
        }
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        SetInformationJobObject(
            *job,
            JobObjectExtendedLimitInformation,
            &limits as *const _ as *const _,
            size_of_val(&limits) as u32,
        )?;
        AssignProcessToJobObject(*job, GetCurrentProcess())?;
        let final_policy = descriptor("0x0012000c")?;
        let mut present = false.into();
        let mut defaulted = false.into();
        let mut dacl = std::ptr::null_mut();
        GetSecurityDescriptorDacl(final_policy, &mut present, &mut dacl, &mut defaulted)?;
        let secured = SetSecurityInfo(
            *job,
            SE_KERNEL_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(dacl),
            None,
        );
        LocalFree(Some(HLOCAL(final_policy.0)));
        secured.ok()?;
        let mut restricted = HANDLE::default();
        DuplicateHandle(
            GetCurrentProcess(),
            *job,
            GetCurrentProcess(),
            &mut restricted,
            0x0012000c,
            false,
            Default::default(),
        )?;
        Ok(Owned::new(restricted))
    }
}

pub fn mutation_denied(name: &str) -> windows::core::Result<bool> {
    use windows::Win32::{Foundation::ERROR_ACCESS_DENIED, System::JobObjects::OpenJobObjectW};
    let name = wide(name);
    for rights in [0x1, 0x2, 0x4_0000] {
        match unsafe { OpenJobObjectW(rights, false, PCWSTR(name.as_ptr())) } {
            Ok(handle) => {
                let _handle = unsafe { Owned::new(handle) };
                return Ok(false);
            }
            Err(error) if error.code() == ERROR_ACCESS_DENIED.to_hresult() => {}
            Err(error) => return Err(error),
        }
    }
    Ok(true)
}
