//! Handle-relative Windows machine filesystem.
//!
//! Callers hold a verified directory handle. Child creates, reads, renames, and
//! deletes name that handle as their root. `FlushFileBuffers` on a directory
//! requires a writable handle (`FILE_WRITE_DATA` / `GENERIC_WRITE`); a read-only
//! directory handle fails with `ERROR_ACCESS_DENIED` and does not sync metadata.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::mem::{offset_of, size_of};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Component, Path, Prefix};
use std::sync::atomic::{AtomicU32, Ordering};

use windows::Win32::Foundation::{
    ERROR_NO_MORE_FILES, ERROR_NOT_ALL_ASSIGNED, GetLastError, HANDLE, LocalFree,
};
use windows::Win32::Security::Authorization::{GetSecurityInfo, SE_FILE_OBJECT};
use windows::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_REVISION, AddAccessAllowedAceEx,
    AdjustTokenPrivileges, CONTAINER_INHERIT_ACE, CreateWellKnownSid, EqualSid, GetAce,
    GetSecurityDescriptorControl, InitializeAcl, InitializeSecurityDescriptor,
    LookupPrivilegeValueW, OBJECT_INHERIT_ACE, OBJECT_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
    PSID, SE_DACL_PRESENT, SE_DACL_PROTECTED, SE_PRIVILEGE_ENABLED, SECURITY_DESCRIPTOR,
    SetSecurityDescriptorControl, SetSecurityDescriptorDacl, SetSecurityDescriptorOwner,
    TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY, WinBuiltinAdministratorsSid,
    WinBuiltinUsersSid, WinCreatorOwnerRightsSid, WinLocalSystemSid,
};
use windows::Win32::Storage::FileSystem::{
    FILE_DISPOSITION_INFO, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_RENAME_INFO,
    FILE_SHARE_READ, FILE_SHARE_WRITE, FileAttributeTagInfo, FileDispositionInfo,
    FileFullDirectoryInfo, FileFullDirectoryRestartInfo, FileRenameInfo, FlushFileBuffers,
    GetFileInformationByHandleEx, SetFileInformationByHandle,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::core::PCWSTR;
use windows_link::link;
use zup_transaction::{PublishError, VisiblePublish, settle_visible};

use crate::machine_state::MachineStateError;

link!("ntdll.dll" "system" fn NtCreateFile(filehandle: *mut HANDLE, desiredaccess: u32, objectattributes: *const ObjectAttributes, iostatusblock: *mut IoStatus, allocationsize: *const i64, fileattributes: u32, shareaccess: u32, createdisposition: u32, createoptions: u32, eabuffer: *const core::ffi::c_void, ealength: u32) -> i32);
link!("ntdll.dll" "system" fn RtlNtStatusToDosError(status: i32) -> u32);

const FILE_OPEN: u32 = 1;
const FILE_CREATE: u32 = 2;
const FILE_OPEN_IF: u32 = 3;
const FILE_DIRECTORY_FILE: u32 = 1;
const FILE_NON_DIRECTORY_FILE: u32 = 0x40;
const FILE_SYNCHRONOUS_IO_NONALERT: u32 = 0x20;
const FILE_OPEN_REPARSE_POINT_NT: u32 = 0x0020_0000;
const OBJ_CASE_INSENSITIVE: u32 = 0x40;
const OBJ_DONT_REPARSE: u32 = 0x1000;
const FILE_LIST_DIRECTORY: u32 = 1;
const FILE_ADD_FILE: u32 = 2;
const FILE_ADD_SUBDIRECTORY: u32 = 4;
const FILE_TRAVERSE: u32 = 0x20;
const DELETE: u32 = 0x1_0000;
const FILE_SHARE_DELETE: u32 = 4;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;
const READ_CONTROL: u32 = 0x2_0000;
const SYNCHRONIZE: u32 = 0x10_0000;
const INHERITED_ACE: u8 = 0x10;
const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
const INHERIT: u8 = 0x3;

/// Directory handles that will be flushed must be writable. `FlushFileBuffers`
/// rejects `FILE_GENERIC_READ` alone.
const DIR_ACCESS: u32 = FILE_GENERIC_READ.0
    | FILE_GENERIC_WRITE.0
    | FILE_LIST_DIRECTORY
    | FILE_ADD_FILE
    | FILE_ADD_SUBDIRECTORY
    | FILE_TRAVERSE
    | DELETE
    | READ_CONTROL
    | SYNCHRONIZE;

#[repr(C)]
struct UnicodeString {
    length: u16,
    maximum_length: u16,
    buffer: *mut u16,
}

#[repr(C)]
struct ObjectAttributes {
    length: u32,
    root_directory: HANDLE,
    object_name: *const UnicodeString,
    attributes: u32,
    security_descriptor: *mut core::ffi::c_void,
    security_quality_of_service: *mut core::ffi::c_void,
}

#[repr(C)]
struct IoStatus {
    status: isize,
    information: usize,
}

#[repr(C)]
struct FullDirInfo {
    next_entry_offset: u32,
    file_index: u32,
    creation_time: i64,
    last_access_time: i64,
    last_write_time: i64,
    change_time: i64,
    end_of_file: i64,
    allocation_size: i64,
    file_attributes: u32,
    file_name_length: u32,
    ea_size: u32,
    file_name: u16,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PolicyKind {
    Root,
    Administrators,
    Ledger,
}

#[derive(Clone)]
pub struct SecurityPolicy {
    owner: SidBuf,
    grants: Vec<Grant>,
}

#[derive(Clone)]
struct Grant {
    sid: SidBuf,
    mask: u32,
}

#[derive(Clone)]
struct SidBuf(Vec<u8>);

impl SidBuf {
    fn as_psid(&self) -> PSID {
        PSID(self.0.as_ptr() as *mut _)
    }
}

impl SecurityPolicy {
    pub fn for_kind(kind: PolicyKind) -> Result<Self, MachineStateError> {
        let owner = well_known(WinLocalSystemSid)?;
        let mut grants = vec![
            Grant {
                sid: well_known(WinLocalSystemSid)?,
                mask: 0x001F_01FF,
            },
            Grant {
                sid: well_known(WinBuiltinAdministratorsSid)?,
                mask: 0x001F_01FF,
            },
            Grant {
                sid: well_known(WinCreatorOwnerRightsSid)?,
                mask: READ_CONTROL | SYNCHRONIZE,
            },
        ];
        if kind == PolicyKind::Ledger {
            grants.push(Grant {
                sid: well_known(WinBuiltinUsersSid)?,
                mask: FILE_GENERIC_READ.0 | SYNCHRONIZE,
            });
        }
        if kind == PolicyKind::Root {
            grants.push(Grant {
                sid: well_known(WinBuiltinUsersSid)?,
                mask: FILE_TRAVERSE | 0x80 | READ_CONTROL | SYNCHRONIZE,
            });
        }
        Ok(Self { owner, grants })
    }
}

pub struct MachineDir {
    handle: OwnedHandle,
}

impl MachineDir {
    pub fn open_root(path: &Path) -> Result<Self, MachineStateError> {
        let _privilege = RestorePrivilege::acquire()?;
        let (drive, names) = split_absolute(path)?;
        let mut current = open_drive(drive)?;
        reject_reparse(&current)?;
        let Some((leaf, parents)) = names.split_last() else {
            return Err(io_path(path, "machine state path has no leaf"));
        };
        for name in parents {
            current = open_relative(&current, name, true, FILE_OPEN, None, DIR_ACCESS)?;
            reject_reparse(&current)?;
        }
        let created = open_relative(
            &current,
            leaf,
            true,
            FILE_OPEN_IF,
            Some(&SecurityPolicy::for_kind(PolicyKind::Root)?),
            DIR_ACCESS,
        )?;
        reject_reparse(&created)?;
        verify_handle(&created, PolicyKind::Root)?;
        flush_handle(&current)?;
        flush_handle(&created)?;
        Ok(Self { handle: created })
    }

    pub fn open_dir(&self, name: &str) -> Result<Self, MachineStateError> {
        let child = open_relative(
            &self.handle,
            OsStr::new(name),
            true,
            FILE_OPEN,
            None,
            DIR_ACCESS,
        )?;
        reject_reparse(&child)?;
        verify_handle(&child, PolicyKind::Administrators)?;
        Ok(Self { handle: child })
    }

    pub fn ensure_dir(&self, name: &str, kind: PolicyKind) -> Result<Self, MachineStateError> {
        let _privilege = RestorePrivilege::acquire()?;
        let policy = SecurityPolicy::for_kind(kind)?;
        let child = open_relative(
            &self.handle,
            OsStr::new(name),
            true,
            FILE_OPEN_IF,
            Some(&policy),
            DIR_ACCESS,
        )?;
        reject_reparse(&child)?;
        verify_handle(&child, kind)?;
        flush_handle(&self.handle)?;
        Ok(Self { handle: child })
    }

    pub fn read(&self, name: &str, kind: PolicyKind) -> Result<Option<Vec<u8>>, MachineStateError> {
        match open_relative(
            &self.handle,
            OsStr::new(name),
            false,
            FILE_OPEN,
            None,
            FILE_GENERIC_READ.0 | READ_CONTROL,
        ) {
            Ok(handle) => {
                verify_file(&handle, kind)?;
                let mut file = File::from(handle);
                let mut bytes = Vec::new();
                std::io::Read::read_to_end(&mut file, &mut bytes).map_err(|source| {
                    MachineStateError::Io {
                        path: std::path::PathBuf::from(name),
                        source,
                    }
                })?;
                Ok(Some(bytes))
            }
            Err(MachineStateError::Io { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    pub fn publish(
        &self,
        name: &str,
        bytes: &[u8],
        create_new: bool,
        kind: PolicyKind,
    ) -> Result<(), PublishError> {
        let _privilege =
            RestorePrivilege::acquire().map_err(|error| PublishError::NotPublished {
                kind: Some(std::io::ErrorKind::PermissionDenied),
                detail: error.to_string(),
            })?;
        let policy =
            SecurityPolicy::for_kind(kind).map_err(|error| PublishError::NotPublished {
                kind: Some(std::io::ErrorKind::PermissionDenied),
                detail: error.to_string(),
            })?;
        let staging_name = staging_name();
        let staging = open_relative(
            &self.handle,
            OsStr::new(&staging_name),
            false,
            FILE_CREATE,
            Some(&policy),
            FILE_GENERIC_READ.0 | FILE_GENERIC_WRITE.0 | DELETE | READ_CONTROL,
        )
        .map_err(|error| PublishError::NotPublished {
            kind: Some(std::io::ErrorKind::Other),
            detail: error.to_string(),
        })?;
        let write_result = (|| {
            let mut file = File::from(staging.try_clone()?);
            std::io::Write::write_all(&mut file, bytes)?;
            file.sync_all()?;
            Ok::<(), std::io::Error>(())
        })();
        if let Err(error) = write_result {
            discard(&staging);
            return Err(PublishError::NotPublished {
                kind: Some(error.kind()),
                detail: error.to_string(),
            });
        }
        if let Err(error) = rename_handle(&staging, &self.handle, name, !create_new) {
            discard(&staging);
            return Err(PublishError::NotPublished {
                kind: Some(std::io::ErrorKind::AlreadyExists),
                detail: error.to_string(),
            });
        }
        let directory_durable = flush_handle(&self.handle).is_ok();
        let observed = self.read(name, kind).ok().flatten();
        let security_ok = observed.is_some();
        settle_visible(VisiblePublish {
            security_ok,
            bytes_match: observed.as_deref() == Some(bytes),
            directory_durable,
        })
    }

    pub fn open_lock(&self, name: &str) -> Result<File, MachineStateError> {
        let _privilege = RestorePrivilege::acquire()?;
        let policy = SecurityPolicy::for_kind(PolicyKind::Administrators)?;
        let handle = open_relative(
            &self.handle,
            OsStr::new(name),
            false,
            FILE_OPEN_IF,
            Some(&policy),
            FILE_GENERIC_READ.0 | FILE_GENERIC_WRITE.0 | READ_CONTROL,
        )?;
        verify_file(&handle, PolicyKind::Administrators)?;
        Ok(File::from(handle))
    }

    pub fn remove(&self, name: &str) -> Result<(), MachineStateError> {
        let handle = open_relative(
            &self.handle,
            OsStr::new(name),
            false,
            FILE_OPEN,
            None,
            DELETE | READ_CONTROL | FILE_GENERIC_READ.0,
        )?;
        discard(&handle);
        flush_handle(&self.handle)?;
        Ok(())
    }

    pub fn list(&self) -> Result<Vec<String>, MachineStateError> {
        list_names(&self.handle)
    }

    pub fn remove_directory(&self, name: &str) -> Result<(), MachineStateError> {
        let child = self.ensure_dir(name, PolicyKind::Administrators)?;
        for entry in child.list()? {
            if child.read(&entry, PolicyKind::Administrators)?.is_some() {
                child.remove(&entry)?;
            } else {
                child.remove_directory(&entry)?;
            }
        }
        let handle = open_relative(
            &self.handle,
            OsStr::new(name),
            true,
            FILE_OPEN,
            None,
            DELETE | DIR_ACCESS,
        )?;
        discard(&handle);
        flush_handle(&self.handle)?;
        Ok(())
    }
}

fn staging_name() -> String {
    static NONCE: AtomicU32 = AtomicU32::new(0);
    let nonce = NONCE.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!(".journal-{}-{nonce:x}-{nanos:x}.tmp", std::process::id())
}

fn rename_handle(
    file: &OwnedHandle,
    directory: &OwnedHandle,
    name: &str,
    replace: bool,
) -> Result<(), MachineStateError> {
    let wide: Vec<u16> = OsStr::new(name).encode_wide().collect();
    let header = offset_of!(FILE_RENAME_INFO, FileName);
    let mut buffer = vec![0u8; header + wide.len() * 2];
    unsafe {
        let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFO>();
        (*info).Anonymous.ReplaceIfExists = replace;
        (*info).RootDirectory = HANDLE(directory.as_raw_handle());
        (*info).FileNameLength = (wide.len() * 2) as u32;
        std::ptr::copy_nonoverlapping(wide.as_ptr(), (*info).FileName.as_mut_ptr(), wide.len());
        SetFileInformationByHandle(
            HANDLE(file.as_raw_handle()),
            FileRenameInfo,
            buffer.as_ptr().cast(),
            buffer.len() as u32,
        )
        .map_err(io_other)
    }
}

fn discard(handle: &OwnedHandle) {
    let info = FILE_DISPOSITION_INFO { DeleteFile: true };
    unsafe {
        let _ = SetFileInformationByHandle(
            HANDLE(handle.as_raw_handle()),
            FileDispositionInfo,
            (&info as *const FILE_DISPOSITION_INFO).cast(),
            size_of::<FILE_DISPOSITION_INFO>() as u32,
        );
    }
}

fn flush_handle(handle: &OwnedHandle) -> Result<(), MachineStateError> {
    unsafe { FlushFileBuffers(HANDLE(handle.as_raw_handle())) }.map_err(io_other)
}

fn list_names(handle: &OwnedHandle) -> Result<Vec<String>, MachineStateError> {
    let mut names = Vec::new();
    let mut buffer = vec![0u8; 16 * 1024];
    let mut first = true;
    loop {
        let class = if first {
            FileFullDirectoryRestartInfo
        } else {
            FileFullDirectoryInfo
        };
        first = false;
        let listed = unsafe {
            GetFileInformationByHandleEx(
                HANDLE(handle.as_raw_handle()),
                class,
                buffer.as_mut_ptr().cast(),
                buffer.len() as u32,
            )
        };
        if let Err(error) = listed {
            if error.code().0 as u32 == ERROR_NO_MORE_FILES.0 {
                break;
            }
            return Err(io_other(error));
        }
        let mut offset = 0usize;
        loop {
            let info = unsafe { &*buffer.as_ptr().add(offset).cast::<FullDirInfo>() };
            let name_len = info.file_name_length as usize / 2;
            let name_ptr = unsafe {
                buffer
                    .as_ptr()
                    .add(offset)
                    .add(offset_of!(FullDirInfo, file_name))
            };
            let wide = unsafe { std::slice::from_raw_parts(name_ptr.cast::<u16>(), name_len) };
            let name = OsString::from_wide(wide);
            if name != "." && name != ".." {
                names.push(name.to_string_lossy().into_owned());
            }
            if info.next_entry_offset == 0 {
                break;
            }
            offset += info.next_entry_offset as usize;
        }
    }
    Ok(names)
}

fn verify_file(handle: &OwnedHandle, kind: PolicyKind) -> Result<(), MachineStateError> {
    let attributes = attributes(handle)?;
    if attributes & FILE_ATTRIBUTE_DIRECTORY != 0 || attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
    {
        return Err(io_path(
            Path::new("machine-state"),
            "machine file is not a regular file",
        ));
    }
    verify_handle(handle, kind)
}

fn verify_handle(handle: &OwnedHandle, kind: PolicyKind) -> Result<(), MachineStateError> {
    let expected = SecurityPolicy::for_kind(kind)?;
    let mut owner = PSID::default();
    let mut dacl = std::ptr::null_mut();
    let mut raw = PSECURITY_DESCRIPTOR::default();
    let status = unsafe {
        GetSecurityInfo(
            HANDLE(handle.as_raw_handle()),
            SE_FILE_OBJECT,
            OBJECT_SECURITY_INFORMATION(0x1 | 0x4),
            Some(&mut owner),
            None,
            Some(&mut dacl),
            None,
            Some(&mut raw),
        )
    };
    if status.0 != 0 {
        return Err(MachineStateError::Io {
            path: std::path::PathBuf::from("machine-state"),
            source: std::io::Error::from_raw_os_error(status.0 as i32),
        });
    }
    let result = (|| {
        if unsafe { EqualSid(owner, expected.owner.as_psid()) }.is_err() {
            return Err(io_path(
                Path::new("machine-state"),
                "machine state owner is not SYSTEM",
            ));
        }
        let mut control = 0u16;
        let mut revision = 0u32;
        unsafe {
            GetSecurityDescriptorControl(raw, &mut control, &mut revision).map_err(io_other)?;
        }
        if control & SE_DACL_PROTECTED.0 == 0 || control & SE_DACL_PRESENT.0 == 0 {
            return Err(io_path(
                Path::new("machine-state"),
                "machine state DACL is not protected",
            ));
        }
        let acl = unsafe { &*dacl.cast::<ACL>() };
        if acl.AceCount as usize != expected.grants.len() {
            return Err(io_path(
                Path::new("machine-state"),
                "machine state DACL has an unexpected ACE count",
            ));
        }
        let mut unmatched = expected.grants.clone();
        for index in 0..acl.AceCount {
            let mut ace_ptr = std::ptr::null_mut();
            unsafe { GetAce(acl, index as u32, &mut ace_ptr).map_err(io_other)? };
            let header = unsafe { &*ace_ptr.cast::<ACE_HEADER>() };
            if header.AceType != ACCESS_ALLOWED_ACE_TYPE || header.AceFlags & INHERITED_ACE != 0 {
                return Err(io_path(
                    Path::new("machine-state"),
                    "machine state DACL has an unexpected ACE",
                ));
            }
            if header.AceFlags != INHERIT {
                return Err(io_path(
                    Path::new("machine-state"),
                    "machine state ACE inheritance does not match the policy",
                ));
            }
            let allowed = unsafe { &*ace_ptr.cast::<ACCESS_ALLOWED_ACE>() };
            let sid =
                PSID(unsafe { (ace_ptr as *mut u8).add(size_of::<ACE_HEADER>() + 4) } as *mut _);
            let position = unmatched.iter().position(|grant| {
                grant.mask == allowed.Mask && unsafe { EqualSid(sid, grant.sid.as_psid()).is_ok() }
            });
            let Some(position) = position else {
                return Err(io_path(
                    Path::new("machine-state"),
                    "machine state DACL grants an unexpected trustee",
                ));
            };
            unmatched.swap_remove(position);
        }
        if !unmatched.is_empty() {
            return Err(io_path(
                Path::new("machine-state"),
                "machine state DACL is missing a required trustee",
            ));
        }
        Ok(())
    })();
    unsafe {
        if !raw.0.is_null() {
            let _ = LocalFree(Some(windows::Win32::Foundation::HLOCAL(raw.0 as *mut _)));
        }
    }
    result
}

fn reject_reparse(handle: &OwnedHandle) -> Result<(), MachineStateError> {
    if attributes(handle)? & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io_path(Path::new("machine-state"), "reparse point"));
    }
    Ok(())
}

fn attributes(handle: &OwnedHandle) -> Result<u32, MachineStateError> {
    let mut info = windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_TAG_INFO::default();
    unsafe {
        GetFileInformationByHandleEx(
            HANDLE(handle.as_raw_handle()),
            FileAttributeTagInfo,
            (&mut info as *mut windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_TAG_INFO).cast(),
            size_of::<windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_TAG_INFO>() as u32,
        )
        .map_err(io_other)?;
    }
    Ok(info.FileAttributes)
}

fn open_drive(drive: u8) -> Result<OwnedHandle, MachineStateError> {
    let wide: Vec<u16> = format!("\\??\\{}:\\", char::from_u32(drive as u32).unwrap_or('C'))
        .encode_utf16()
        .collect();
    nt_open(None, &wide, true, FILE_OPEN, None, DIR_ACCESS)
}

fn open_relative(
    parent: &OwnedHandle,
    name: &OsStr,
    directory: bool,
    disposition: u32,
    policy: Option<&SecurityPolicy>,
    access: u32,
) -> Result<OwnedHandle, MachineStateError> {
    if name.is_empty() || name.to_string_lossy().contains(['/', '\\']) {
        return Err(io_path(
            Path::new(name),
            "machine child name is not a single component",
        ));
    }
    let wide: Vec<u16> = name.encode_wide().collect();
    nt_open(Some(parent), &wide, directory, disposition, policy, access)
}

fn nt_open(
    parent: Option<&OwnedHandle>,
    wide: &[u16],
    directory: bool,
    disposition: u32,
    policy: Option<&SecurityPolicy>,
    access: u32,
) -> Result<OwnedHandle, MachineStateError> {
    let mut descriptor = policy.map(descriptor_for).transpose()?;
    let mut name = wide.to_vec();
    let mut string = UnicodeString {
        length: (name.len() * 2) as u16,
        maximum_length: (name.len() * 2) as u16,
        buffer: name.as_mut_ptr(),
    };
    let attributes = ObjectAttributes {
        length: size_of::<ObjectAttributes>() as u32,
        root_directory: parent
            .map(|parent| HANDLE(parent.as_raw_handle()))
            .unwrap_or(HANDLE(std::ptr::null_mut())),
        object_name: &string,
        attributes: OBJ_CASE_INSENSITIVE | OBJ_DONT_REPARSE,
        security_descriptor: descriptor
            .as_mut()
            .map(|descriptor| descriptor.as_ptr())
            .unwrap_or(std::ptr::null_mut()),
        security_quality_of_service: std::ptr::null_mut(),
    };
    let _ = &mut string;
    let options = FILE_OPEN_REPARSE_POINT_NT
        | FILE_SYNCHRONOUS_IO_NONALERT
        | if directory {
            FILE_DIRECTORY_FILE
        } else {
            FILE_NON_DIRECTORY_FILE
        };
    let share = if directory {
        FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0 | FILE_SHARE_DELETE
    } else if access & DELETE != 0 && disposition == FILE_CREATE {
        0
    } else {
        FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0
    };
    let mut handle = HANDLE(std::ptr::null_mut());
    let mut status_block = IoStatus {
        status: 0,
        information: 0,
    };
    let status = unsafe {
        NtCreateFile(
            &mut handle,
            access,
            &attributes,
            &mut status_block,
            std::ptr::null(),
            if directory {
                FILE_ATTRIBUTE_DIRECTORY
            } else {
                FILE_ATTRIBUTE_NORMAL
            },
            share,
            disposition,
            options,
            std::ptr::null(),
            0,
        )
    };
    if status < 0 {
        let dos = unsafe { RtlNtStatusToDosError(status) };
        return Err(MachineStateError::Io {
            path: std::path::PathBuf::from("machine-state"),
            source: std::io::Error::from_raw_os_error(dos as i32),
        });
    }
    Ok(unsafe { OwnedHandle::from_raw_handle(handle.0) })
}

struct Descriptor {
    _policy: SecurityPolicy,
    _acl: Vec<u8>,
    descriptor: SECURITY_DESCRIPTOR,
}

impl Descriptor {
    fn as_ptr(&mut self) -> *mut core::ffi::c_void {
        unsafe {
            SetSecurityDescriptorOwner(
                PSECURITY_DESCRIPTOR(&mut self.descriptor as *mut _ as *mut _),
                Some(self._policy.owner.as_psid()),
                false,
            )
            .ok();
            SetSecurityDescriptorDacl(
                PSECURITY_DESCRIPTOR(&mut self.descriptor as *mut _ as *mut _),
                true,
                Some(self._acl.as_ptr().cast()),
                false,
            )
            .ok();
        }
        (&mut self.descriptor as *mut SECURITY_DESCRIPTOR).cast()
    }
}

fn descriptor_for(policy: &SecurityPolicy) -> Result<Descriptor, MachineStateError> {
    let mut acl_size = size_of::<ACL>();
    for grant in &policy.grants {
        acl_size += size_of::<ACCESS_ALLOWED_ACE>() - 4 + grant.sid.0.len();
    }
    let mut acl = vec![0u8; acl_size];
    unsafe {
        InitializeAcl(acl.as_mut_ptr().cast(), acl.len() as u32, ACL_REVISION).map_err(io_other)?;
        for grant in &policy.grants {
            AddAccessAllowedAceEx(
                acl.as_mut_ptr().cast(),
                ACL_REVISION,
                OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE,
                grant.mask,
                grant.sid.as_psid(),
            )
            .map_err(io_other)?;
        }
    }
    let mut descriptor = SECURITY_DESCRIPTOR::default();
    unsafe {
        InitializeSecurityDescriptor(PSECURITY_DESCRIPTOR(&mut descriptor as *mut _ as *mut _), 1)
            .map_err(io_other)?;
        SetSecurityDescriptorControl(
            PSECURITY_DESCRIPTOR(&mut descriptor as *mut _ as *mut _),
            SE_DACL_PROTECTED,
            SE_DACL_PROTECTED,
        )
        .map_err(io_other)?;
    }
    Ok(Descriptor {
        _policy: policy.clone(),
        _acl: acl,
        descriptor,
    })
}

fn well_known(
    kind: windows::Win32::Security::WELL_KNOWN_SID_TYPE,
) -> Result<SidBuf, MachineStateError> {
    let mut bytes = vec![0u8; 68];
    let mut len = bytes.len() as u32;
    unsafe {
        CreateWellKnownSid(kind, None, Some(PSID(bytes.as_mut_ptr().cast())), &mut len)
            .map_err(io_other)?;
    }
    bytes.truncate(len as usize);
    Ok(SidBuf(bytes))
}

static RESTORE_HOLDERS: AtomicU32 = AtomicU32::new(0);
static RESTORE_LUID: std::sync::Mutex<Option<windows::Win32::Foundation::LUID>> =
    std::sync::Mutex::new(None);

struct RestorePrivilege;

impl RestorePrivilege {
    fn acquire() -> Result<Self, MachineStateError> {
        if !crate::transport::is_process_elevated().unwrap_or(false) {
            return Err(io_path(
                Path::new("machine-state"),
                "machine state requires a fully elevated token",
            ));
        }
        let previous = RESTORE_HOLDERS.fetch_add(1, Ordering::SeqCst);
        if previous == 0
            && let Err(error) = enable_restore()
        {
            RESTORE_HOLDERS.fetch_sub(1, Ordering::SeqCst);
            return Err(error);
        }
        Ok(Self)
    }
}

impl Drop for RestorePrivilege {
    fn drop(&mut self) {
        if RESTORE_HOLDERS.fetch_sub(1, Ordering::SeqCst) == 1 {
            let _ = disable_restore();
        }
    }
}

fn enable_restore() -> Result<(), MachineStateError> {
    adjust_restore(SE_PRIVILEGE_ENABLED, true)
}

fn disable_restore() -> Result<(), MachineStateError> {
    adjust_restore(
        windows::Win32::Security::TOKEN_PRIVILEGES_ATTRIBUTES(0),
        false,
    )
}

fn adjust_restore(
    attributes: windows::Win32::Security::TOKEN_PRIVILEGES_ATTRIBUTES,
    require_assigned: bool,
) -> Result<(), MachineStateError> {
    let mut token = HANDLE::default();
    unsafe {
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &mut token,
        )
        .map_err(io_other)?;
    }
    let mut luid_slot = RESTORE_LUID
        .lock()
        .map_err(|_| io_path(Path::new("machine-state"), "privilege state is unavailable"))?;
    if luid_slot.is_none() {
        let mut wide: Vec<u16> = OsStr::new("SeRestorePrivilege").encode_wide().collect();
        wide.push(0);
        let mut luid = windows::Win32::Foundation::LUID::default();
        unsafe {
            LookupPrivilegeValueW(PCWSTR::null(), PCWSTR(wide.as_ptr()), &mut luid)
                .map_err(io_other)?;
        }
        *luid_slot = Some(luid);
    }
    let luid = luid_slot.expect("luid");
    drop(luid_slot);
    let privileges = TOKEN_PRIVILEGES {
        PrivilegeCount: 1,
        Privileges: [windows::Win32::Security::LUID_AND_ATTRIBUTES {
            Luid: luid,
            Attributes: attributes,
        }],
    };
    unsafe {
        AdjustTokenPrivileges(token, false, Some(&privileges), 0, None, None).map_err(io_other)?;
        let assigned = GetLastError();
        let _ = windows::Win32::Foundation::CloseHandle(token);
        if require_assigned && assigned == ERROR_NOT_ALL_ASSIGNED {
            return Err(io_path(
                Path::new("machine-state"),
                "SeRestorePrivilege is not assigned to this token",
            ));
        }
    }
    Ok(())
}

fn split_absolute(path: &Path) -> Result<(u8, Vec<OsString>), MachineStateError> {
    let mut components = path.components();
    let drive = match components.next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::Disk(drive) | Prefix::VerbatimDisk(drive) => drive,
            _ => return Err(io_path(path, "machine state must be a local disk path")),
        },
        _ => return Err(io_path(path, "machine state must be absolute")),
    };
    if !matches!(components.next(), Some(Component::RootDir)) {
        return Err(io_path(path, "machine state must be absolute"));
    }
    let mut names = Vec::new();
    for component in components {
        match component {
            Component::Normal(name) => names.push(name.to_os_string()),
            _ => {
                return Err(io_path(
                    path,
                    "machine state path is not a normal hierarchy",
                ));
            }
        }
    }
    Ok((drive, names))
}

fn io_other(error: impl std::fmt::Display) -> MachineStateError {
    MachineStateError::Io {
        path: std::path::PathBuf::from("machine-state"),
        source: std::io::Error::other(error.to_string()),
    }
}

fn io_path(path: &Path, detail: &str) -> MachineStateError {
    MachineStateError::Io {
        path: path.to_path_buf(),
        source: std::io::Error::new(std::io::ErrorKind::PermissionDenied, detail),
    }
}
