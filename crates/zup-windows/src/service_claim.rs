use std::mem::size_of;

use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, HANDLE, HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo, SDDL_REVISION_1,
    SE_REGISTRY_KEY,
};
use windows::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_HEADER, GetAce, GetSecurityDescriptorControl, IsWellKnownSid,
    OBJECT_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SE_DACL_PRESENT, SE_DACL_PROTECTED,
    SECURITY_ATTRIBUTES, WinBuiltinAdministratorsSid, WinBuiltinUsersSid, WinCreatorOwnerSid,
    WinLocalSystemSid,
};
use windows::Win32::System::Registry::{
    HKEY, KEY_ALL_ACCESS, KEY_READ, KEY_WOW64_64KEY, REG_CREATE_KEY_DISPOSITION,
    REG_CREATED_NEW_KEY, REG_LINK, REG_OPTION_NON_VOLATILE, REG_OPTION_OPEN_LINK, REG_SZ,
    RegCreateKeyExW, RegEnumKeyExW, RegEnumValueW, RegFlushKey, RegOpenKeyExW, RegQueryInfoKeyW,
    RegQueryValueExW,
};
use windows::core::PCWSTR;
use windows_registry::{Key, LOCAL_MACHINE};
use windows_service::service::{
    Service, ServiceAccess, ServiceInfo, ServiceSidType, ServiceStartType,
};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use zup_core::{ObservedServiceRegistration, ServiceOwnershipClaim};

#[derive(Debug, thiserror::Error)]
pub enum ClaimError {
    #[error("service ownership name is invalid")]
    Name,
    #[error("service ownership registry {operation} failed with Windows error {code}")]
    Registry { operation: &'static str, code: u32 },
    #[error("service ownership authority is untrusted: {0}")]
    Authority(&'static str),
    #[error("service ownership marker is missing, malformed, or belongs to another installation")]
    Marker,
    #[error("service ownership marker already exists")]
    Occupied,
    #[error("new service must remain disabled until ownership publication succeeds")]
    Enabled,
    #[error(
        "service registry contains data outside deployment ownership; preserve or declare that data before removing the service"
    )]
    PersistentData,
    #[error("service ownership verification failed: {0}")]
    Native(String),
}

struct Descriptor(PSECURITY_DESCRIPTOR);
impl Drop for Descriptor {
    fn drop(&mut self) {
        unsafe {
            let _ = LocalFree(Some(HLOCAL(self.0.0)));
        }
    }
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

fn check(
    status: windows::Win32::Foundation::WIN32_ERROR,
    operation: &'static str,
) -> Result<(), ClaimError> {
    if status == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(ClaimError::Registry {
            operation,
            code: status.0,
        })
    }
}

fn open_unresolved(parent: &Key, name: &str) -> Result<Option<Key>, ClaimError> {
    let text = wide(name);
    let mut raw = HKEY::default();
    let status = unsafe {
        RegOpenKeyExW(
            HKEY(parent.as_raw()),
            PCWSTR(text.as_ptr()),
            Some(REG_OPTION_OPEN_LINK.0),
            KEY_READ | KEY_WOW64_64KEY,
            &mut raw,
        )
    };
    if status == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    check(status, "open")?;
    Ok(Some(unsafe { Key::from_raw(raw.0) }))
}

fn open(parent: &Key, name: &str) -> Result<Option<Key>, ClaimError> {
    let Some(key) = open_unresolved(parent, name)? else {
        return Ok(None);
    };
    let link = wide("SymbolicLinkValue");
    let mut kind = Default::default();
    let status = unsafe {
        RegQueryValueExW(
            HKEY(key.as_raw()),
            PCWSTR(link.as_ptr()),
            None,
            Some(&mut kind),
            None,
            None,
        )
    };
    if status != ERROR_FILE_NOT_FOUND {
        check(status, "registry link observation")?;
        if kind == REG_LINK {
            return Err(ClaimError::Authority("symbolic registry link"));
        }
    }
    Ok(Some(key))
}

fn trusted(sid: PSID) -> bool {
    unsafe {
        IsWellKnownSid(sid, WinLocalSystemSid).as_bool()
            || IsWellKnownSid(sid, WinBuiltinAdministratorsSid).as_bool()
    }
}

fn verify(key: &Key, marker: bool) -> Result<(), ClaimError> {
    let mut owner = PSID::default();
    let mut acl = std::ptr::null_mut();
    let mut raw = PSECURITY_DESCRIPTOR::default();
    check(
        unsafe {
            GetSecurityInfo(
                HANDLE(key.as_raw()),
                SE_REGISTRY_KEY,
                OBJECT_SECURITY_INFORMATION(5),
                Some(&mut owner),
                None,
                Some(&mut acl),
                None,
                Some(&mut raw),
            )
        },
        "security query",
    )?;
    let descriptor = Descriptor(raw);
    if !trusted(owner) || acl.is_null() {
        return Err(ClaimError::Authority(
            "owner or missing access-control list",
        ));
    }
    if marker {
        let mut flags = 0;
        let mut revision = 0;
        unsafe { GetSecurityDescriptorControl(descriptor.0, &mut flags, &mut revision) }
            .map_err(|error| ClaimError::Native(error.to_string()))?;
        if flags & SE_DACL_PROTECTED.0 == 0 || flags & SE_DACL_PRESENT.0 == 0 {
            return Err(ClaimError::Authority(
                "ownership marker DACL is not protected",
            ));
        }
    }
    let mut grants = Vec::new();
    for index in 0..unsafe { (*acl).AceCount } {
        let mut raw = std::ptr::null_mut();
        unsafe { GetAce(acl, index as u32, &mut raw) }
            .map_err(|error| ClaimError::Native(error.to_string()))?;
        let header = unsafe { &*raw.cast::<ACE_HEADER>() };
        if !marker && (header.AceFlags & 8 != 0 || header.AceType == 1) {
            continue;
        }
        if header.AceType != 0 {
            return Err(ClaimError::Authority("unsupported access-control entry"));
        }
        let ace = unsafe { &*raw.cast::<ACCESS_ALLOWED_ACE>() };
        let sid = PSID(unsafe { raw.cast::<u8>().add(size_of::<ACE_HEADER>() + 4) }.cast());
        // CREATOR_OWNER is an inheritance template; opened descendants still require a trusted owner.
        if !marker
            && header.AceFlags & 2 != 0
            && unsafe { IsWellKnownSid(sid, WinCreatorOwnerSid).as_bool() }
        {
            continue;
        }
        if marker {
            if header.AceFlags != 2 {
                return Err(ClaimError::Authority("unexpected marker ACE inheritance"));
            }
            let principal = unsafe {
                if IsWellKnownSid(sid, WinLocalSystemSid).as_bool() {
                    0
                } else if IsWellKnownSid(sid, WinBuiltinAdministratorsSid).as_bool() {
                    1
                } else if IsWellKnownSid(sid, WinBuiltinUsersSid).as_bool() {
                    2
                } else {
                    return Err(ClaimError::Authority("unexpected marker trustee"));
                }
            };
            if ace.Mask
                != if principal == 2 {
                    KEY_READ.0
                } else {
                    KEY_ALL_ACCESS.0
                }
                || grants.contains(&principal)
            {
                return Err(ClaimError::Authority("unexpected marker access rights"));
            }
            grants.push(principal);
        } else if ace.Mask
            & (0x10000 | 0x40000 | 0x80000 | 0x2 | 0x4 | 0x20 | 0x10000000 | 0x40000000)
            != 0
            && !trusted(sid)
        {
            return Err(ClaimError::Authority("ancestor permits foreign mutation"));
        }
    }
    if marker && grants.len() != 3 {
        return Err(ClaimError::Authority("incomplete marker access policy"));
    }
    Ok(())
}

fn service_key(name: &str) -> Result<Key, ClaimError> {
    if name.is_empty() || name.encode_utf16().count() > 256 || name.contains(['\\', '/', '\0']) {
        return Err(ClaimError::Name);
    }
    let system = open(LOCAL_MACHINE, "SYSTEM")?.ok_or(ClaimError::Marker)?;
    verify(&system, false)?;
    // CurrentControlSet is the operating system's active-control-set alias.
    let alias = open_unresolved(&system, "CurrentControlSet")?.ok_or(ClaimError::Marker)?;
    verify(&alias, false)?;
    let control = system
        .open("CurrentControlSet")
        .map_err(|error| ClaimError::Native(error.to_string()))?;
    verify(&control, false)?;
    let services = open(&control, "Services")?.ok_or(ClaimError::Marker)?;
    verify(&services, false)?;
    let service = open(&services, name)?.ok_or(ClaimError::Marker)?;
    verify(&service, false)?;
    Ok(service)
}

fn read_marker(key: &Key) -> Result<ServiceOwnershipClaim, ClaimError> {
    verify(key, true)?;
    let name = wide("Ownership");
    let mut kind = Default::default();
    let mut length = 0;
    let raw = HKEY(key.as_raw());
    check(
        unsafe {
            RegQueryValueExW(
                raw,
                PCWSTR(name.as_ptr()),
                None,
                Some(&mut kind),
                None,
                Some(&mut length),
            )
        },
        "marker size",
    )?;
    if kind != REG_SZ || !(2..=4096).contains(&length) || length % 2 != 0 {
        return Err(ClaimError::Marker);
    }
    let mut value = vec![0u16; length as usize / 2];
    let capacity = length;
    check(
        unsafe {
            RegQueryValueExW(
                raw,
                PCWSTR(name.as_ptr()),
                None,
                Some(&mut kind),
                Some(value.as_mut_ptr().cast()),
                Some(&mut length),
            )
        },
        "marker read",
    )?;
    if kind != REG_SZ || length != capacity || value.pop() != Some(0) || value.contains(&0) {
        return Err(ClaimError::Marker);
    }
    let value = String::from_utf16(&value).map_err(|_| ClaimError::Marker)?;
    serde_json::from_str(&value).map_err(|_| ClaimError::Marker)
}

fn open_marker(name: &str) -> Result<Option<Key>, ClaimError> {
    let service = service_key(name)?;
    let Some(parameters) = open(&service, "Parameters")? else {
        return Ok(None);
    };
    verify(&parameters, false)?;
    open(&parameters, "Zup")
}

pub fn observe(
    name: &str,
    _held_service: &Service,
) -> Result<ObservedServiceRegistration, ClaimError> {
    match open_marker(name)? {
        None => Ok(ObservedServiceRegistration::Unclaimed),
        Some(marker) => {
            read_marker(&marker).map(|claim| ObservedServiceRegistration::Scm { claim })
        }
    }
}

pub fn confirm(
    name: &str,
    _held_service: &Service,
    expected: &ServiceOwnershipClaim,
) -> Result<(), ClaimError> {
    let marker = open_marker(name)?.ok_or(ClaimError::Marker)?;
    if read_marker(&marker)? != *expected {
        return Err(ClaimError::Marker);
    }
    check(
        unsafe { RegFlushKey(HKEY(marker.as_raw())) },
        "ownership confirmation flush",
    )?;
    if read_marker(&marker)? != *expected {
        return Err(ClaimError::Marker);
    }
    Ok(())
}

fn require_modeled_tree(key: &Key, subkeys: &[&str], values: &[&str]) -> Result<(), ClaimError> {
    let raw = HKEY(key.as_raw());
    let mut key_count = 0;
    let mut value_count = 0;
    check(
        unsafe {
            RegQueryInfoKeyW(
                raw,
                None,
                None,
                None,
                Some(&mut key_count),
                None,
                None,
                Some(&mut value_count),
                None,
                None,
                None,
                None,
            )
        },
        "removal census",
    )?;
    if key_count as usize > subkeys.len() || value_count as usize > values.len() {
        return Err(ClaimError::PersistentData);
    }
    for index in 0..key_count {
        let mut name = [0u16; 256];
        let mut length = name.len() as u32;
        check(
            unsafe {
                RegEnumKeyExW(
                    raw,
                    index,
                    Some(windows::core::PWSTR(name.as_mut_ptr())),
                    &mut length,
                    None,
                    None,
                    None,
                    None,
                )
            },
            "removal subkey observation",
        )?;
        let name =
            String::from_utf16(&name[..length as usize]).map_err(|_| ClaimError::PersistentData)?;
        if !subkeys
            .iter()
            .any(|expected| name.eq_ignore_ascii_case(expected))
        {
            return Err(ClaimError::PersistentData);
        }
    }
    for index in 0..value_count {
        let mut name = [0u16; 256];
        let mut length = name.len() as u32;
        check(
            unsafe {
                RegEnumValueW(
                    raw,
                    index,
                    Some(windows::core::PWSTR(name.as_mut_ptr())),
                    &mut length,
                    None,
                    None,
                    None,
                    None,
                )
            },
            "removal value observation",
        )?;
        let name =
            String::from_utf16(&name[..length as usize]).map_err(|_| ClaimError::PersistentData)?;
        if !values
            .iter()
            .any(|expected| name.eq_ignore_ascii_case(expected))
        {
            return Err(ClaimError::PersistentData);
        }
    }
    Ok(())
}

pub fn require_removable(
    name: &str,
    _held_service: &Service,
    expected: &ServiceOwnershipClaim,
) -> Result<(), ClaimError> {
    let service = service_key(name)?;
    require_modeled_tree(
        &service,
        &["Parameters", "Security"],
        &[
            "Type",
            "Start",
            "ErrorControl",
            "ImagePath",
            "Group",
            "Tag",
            "DependOnGroup",
            "DependOnService",
            "ObjectName",
            "DisplayName",
            "Description",
            "ServiceSidType",
            "RequiredPrivileges",
            "PreshutdownTimeout",
            "FailureActions",
            "FailureActionsOnNonCrashFailures",
            "DelayedAutostart",
            "LaunchProtected",
            "WOW64",
        ],
    )?;
    let parameters = open(&service, "Parameters")?.ok_or(ClaimError::Marker)?;
    verify(&parameters, false)?;
    require_modeled_tree(&parameters, &["Zup"], &[])?;
    let marker = open(&parameters, "Zup")?.ok_or(ClaimError::Marker)?;
    require_modeled_tree(&marker, &[], &["Ownership"])?;
    if read_marker(&marker)? != *expected {
        return Err(ClaimError::Marker);
    }
    if let Some(security) = open(&service, "Security")? {
        verify(&security, false)?;
        require_modeled_tree(&security, &[], &["Security"])?;
    }
    Ok(())
}

pub fn create(info: &ServiceInfo, claim: &ServiceOwnershipClaim) -> Result<Service, ClaimError> {
    if info.start_type != ServiceStartType::Disabled {
        return Err(ClaimError::Enabled);
    }
    let name = info.name.to_str().ok_or(ClaimError::Name)?;
    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )
    .map_err(|error| ClaimError::Native(error.to_string()))?;
    let service = manager
        .create_service(
            info,
            ServiceAccess::QUERY_CONFIG
                | ServiceAccess::CHANGE_CONFIG
                | ServiceAccess::DELETE
                | ServiceAccess::START,
        )
        .map_err(|error| ClaimError::Native(error.to_string()))?;
    service
        .set_config_service_sid_info(ServiceSidType::Unrestricted)
        .map_err(|error| ClaimError::Native(error.to_string()))?;
    publish_new(name, &service, claim)?;
    Ok(service)
}

fn publish_new(
    name: &str,
    service: &Service,
    claim: &ServiceOwnershipClaim,
) -> Result<(), ClaimError> {
    if service
        .query_config()
        .map_err(|error| ClaimError::Native(error.to_string()))?
        .start_type
        != ServiceStartType::Disabled
    {
        return Err(ClaimError::Enabled);
    }
    let parent = service_key(name)?;
    let parameters = match open(&parent, "Parameters")? {
        Some(parameters) => parameters,
        None => parent
            .create("Parameters")
            .map_err(|error| ClaimError::Native(error.to_string()))?,
    };
    verify(&parameters, false)?;
    let text = wide("O:BAG:BAD:P(A;CI;KA;;;SY)(A;CI;KA;;;BA)(A;CI;KR;;;BU)");
    let mut raw = PSECURITY_DESCRIPTOR::default();
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(text.as_ptr()),
            SDDL_REVISION_1,
            &mut raw,
            None,
        )
    }
    .map_err(|error| ClaimError::Native(error.to_string()))?;
    let descriptor = Descriptor(raw);
    let security = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0.0,
        bInheritHandle: false.into(),
    };
    let name = wide("Zup");
    let mut raw = HKEY::default();
    let mut disposition = REG_CREATE_KEY_DISPOSITION::default();
    check(
        unsafe {
            RegCreateKeyExW(
                HKEY(parameters.as_raw()),
                PCWSTR(name.as_ptr()),
                None,
                PCWSTR::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_ALL_ACCESS | KEY_WOW64_64KEY,
                Some(&security),
                &mut raw,
                Some(&mut disposition),
            )
        },
        "exclusive marker creation",
    )?;
    let marker = unsafe { Key::from_raw(raw.0) };
    if disposition != REG_CREATED_NEW_KEY {
        return Err(ClaimError::Occupied);
    }
    verify(&marker, true)?;
    let value = serde_json::to_string(claim).map_err(|_| ClaimError::Marker)?;
    marker
        .set_string("Ownership", value)
        .map_err(|error| ClaimError::Native(error.to_string()))?;
    check(
        unsafe { RegFlushKey(HKEY(marker.as_raw())) },
        "marker flush",
    )?;
    if read_marker(&marker)? != *claim {
        return Err(ClaimError::Marker);
    }
    Ok(())
}
