//! Handle-relative Windows filesystem authority.
//!
//! Directory flushes are not a Windows fsync contract. Protected machine
//! publication flushes a writable volume handle; other mutations need recovery observation.

pub(crate) mod authority;
pub(crate) mod execution;
pub(crate) mod history;

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::mem::{offset_of, size_of};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::io::{AsHandle, AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Component, Path, PathBuf, Prefix};
use std::sync::Arc;

use windows::Win32::Foundation::{
    ERROR_NO_MORE_FILES, ERROR_NO_TOKEN, ERROR_NOT_ALL_ASSIGNED, GetLastError, HANDLE, LocalFree,
};
use windows::Win32::Security::Authorization::{GetSecurityInfo, SE_FILE_OBJECT};
use windows::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_REVISION, AddAccessAllowedAceEx,
    AdjustTokenPrivileges, CreateWellKnownSid, DuplicateTokenEx, EqualSid, GetAce,
    GetSecurityDescriptorControl, InitializeAcl, InitializeSecurityDescriptor,
    LookupPrivilegeValueW, OBJECT_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
    SE_DACL_PRESENT, SE_DACL_PROTECTED, SE_PRIVILEGE_ENABLED, SECURITY_DESCRIPTOR,
    SecurityImpersonation, SetSecurityDescriptorControl, SetSecurityDescriptorDacl,
    SetSecurityDescriptorOwner, TOKEN_ADJUST_PRIVILEGES, TOKEN_DUPLICATE, TOKEN_IMPERSONATE,
    TOKEN_PRIVILEGES, TOKEN_QUERY, TokenImpersonation, WinBuiltinAdministratorsSid,
    WinBuiltinUsersSid, WinCreatorOwnerRightsSid, WinLocalSystemSid,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_DISPOSITION_INFO, FILE_FLAGS_AND_ATTRIBUTES, FILE_GENERIC_READ,
    FILE_GENERIC_WRITE, FILE_ID_INFO, FILE_RENAME_INFO, FILE_SHARE_READ, FILE_SHARE_WRITE,
    FileAttributeTagInfo, FileDispositionInfo, FileFullDirectoryInfo, FileFullDirectoryRestartInfo,
    FileIdInfo, FlushFileBuffers, GetFileInformationByHandleEx, GetFinalPathNameByHandleW,
    OPEN_EXISTING, SetFileInformationByHandle, VOLUME_NAME_GUID,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentThread, OpenProcessToken, OpenThreadToken, SetThreadToken,
};
use windows::core::PCWSTR;
use windows_link::link;
use zup_storage::PublishError;

use crate::host_dirs::HostDirError;

#[derive(Debug, thiserror::Error)]
pub enum FilesystemError {
    #[error(transparent)]
    HostDir(#[from] HostDirError),
    #[error(transparent)]
    Publication(#[from] PublishError),
    #[error("filesystem `{path}`: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

#[derive(Debug, thiserror::Error)]
#[error("{detail}: {source}")]
struct NativeOpenError {
    detail: String,
    #[source]
    source: std::io::Error,
}

impl FilesystemError {
    pub(crate) fn sharing_violation(&self) -> bool {
        let Self::Io { source, .. } = self else {
            return false;
        };
        let code = source.raw_os_error().or_else(|| {
            source
                .get_ref()?
                .downcast_ref::<NativeOpenError>()?
                .source
                .raw_os_error()
        });
        matches!(code, Some(32 | 33))
    }
}

link!("ntdll.dll" "system" fn NtCreateFile(filehandle: *mut HANDLE, desiredaccess: u32, objectattributes: *const ObjectAttributes, iostatusblock: *mut IoStatus, allocationsize: *const i64, fileattributes: u32, shareaccess: u32, createdisposition: u32, createoptions: u32, eabuffer: *const core::ffi::c_void, ealength: u32) -> i32);
link!("ntdll.dll" "system" fn NtSetInformationFile(filehandle: HANDLE, iostatusblock: *mut IoStatus, information: *const core::ffi::c_void, length: u32, class: u32) -> i32);
link!("ntdll.dll" "system" fn NtQueryEaFile(filehandle: HANDLE, iostatusblock: *mut IoStatus, buffer: *mut core::ffi::c_void, length: u32, returnsingleentry: u8, ealist: *const core::ffi::c_void, ealistlength: u32, eaindex: *const u32, restartscan: u8) -> i32);
link!("ntdll.dll" "system" fn RtlNtStatusToDosError(status: i32) -> u32);

const TRUSTED_INSTALLER_SID: &str =
    "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464";
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

// Validated directory handles deny delete sharing so their namespace cannot be moved.
const DIRECTORY_READ: u32 = FILE_LIST_DIRECTORY | FILE_TRAVERSE | 0x80 | READ_CONTROL | SYNCHRONIZE;
const TRAVERSE: u32 = FILE_TRAVERSE | 0x80 | SYNCHRONIZE;

pub(crate) struct PinnedSource {
    pub file: File,
    _directories: Vec<OwnedHandle>,
}

// Pins untrusted input; content authentication is a separate boundary.
pub(crate) fn pin_source(path: &Path) -> Result<PinnedSource, FilesystemError> {
    let (drive, mut names) = split_absolute(path)?;
    let leaf = names
        .pop()
        .ok_or_else(|| io_path(path, "source has no filename"))?;
    let mut directories = vec![open_drive(drive, Open::Directory)?];
    for name in names {
        let directory =
            open_relative(directories.last().unwrap(), &name, Open::Directory, None)?.handle;
        reject_reparse(&directory)?;
        directories.push(directory);
    }
    let file = open_relative(directories.last().unwrap(), &leaf, Open::ObserveFile, None)?.handle;
    reject_reparse(&file)?;
    let file = File::from(file);
    verify_payload_file(&file)?;
    Ok(PinnedSource {
        file,
        _directories: directories,
    })
}

#[derive(Clone, Copy, Debug)]
enum Open {
    Traverse,
    Directory,
    InspectDirectory,
    CreateDirectory,
    CreateExclusiveDirectory,
    MovableDirectory,
    GenerationDirectoryAccess,
    GenerationFileAccess,
    Read,
    ObserveFile,
    MutateFile,
    FenceFile,
    RenameTarget,
    PrepareDirectory,
    DirectoryLease,
    DirectoryPublicationParent,
    DataDirectory,
    CreateDataDirectory,
    Stage,
    Lock,
    RemoveFile,
    #[cfg(test)]
    RemoveDirectory,
}

impl Open {
    fn access(self) -> u32 {
        match self {
            Self::Traverse => TRAVERSE,
            Self::InspectDirectory => TRAVERSE | READ_CONTROL,
            Self::GenerationDirectoryAccess => DIRECTORY_READ | DELETE | 0x0004_0000,
            Self::GenerationFileAccess => 1 | 0x80 | READ_CONTROL | SYNCHRONIZE | 0x0004_0000,
            Self::Directory | Self::CreateDirectory => DIRECTORY_READ,
            Self::CreateExclusiveDirectory | Self::MovableDirectory => DIRECTORY_READ | DELETE,
            Self::RenameTarget | Self::PrepareDirectory => DIRECTORY_READ | 0x2,
            Self::DataDirectory | Self::CreateDataDirectory => DIRECTORY_READ | 8 | 0x0004_0000,
            Self::DirectoryLease => DIRECTORY_READ,
            Self::DirectoryPublicationParent => DIRECTORY_READ | 0x4,
            Self::Read | Self::ObserveFile => 1 | 0x80 | READ_CONTROL | SYNCHRONIZE,
            Self::MutateFile => 1 | DELETE | 0x80 | READ_CONTROL | SYNCHRONIZE,
            Self::Stage | Self::FenceFile => FILE_GENERIC_READ.0 | FILE_GENERIC_WRITE.0 | DELETE,
            Self::Lock => 1 | 2 | 0x80 | READ_CONTROL | SYNCHRONIZE,
            Self::RemoveFile => DELETE | 0x80 | READ_CONTROL | SYNCHRONIZE,
            #[cfg(test)]
            Self::RemoveDirectory => DIRECTORY_READ | DELETE,
        }
    }

    fn disposition(self) -> u32 {
        match self {
            Self::Stage | Self::CreateExclusiveDirectory | Self::CreateDataDirectory => FILE_CREATE,
            Self::CreateDirectory | Self::PrepareDirectory | Self::Lock => FILE_OPEN_IF,
            _ => FILE_OPEN,
        }
    }

    fn options(self) -> u32 {
        FILE_OPEN_REPARSE_POINT_NT
            | FILE_SYNCHRONOUS_IO_NONALERT
            | match self {
                Self::Traverse
                | Self::InspectDirectory
                | Self::Directory
                | Self::RenameTarget
                | Self::PrepareDirectory
                | Self::CreateDirectory
                | Self::CreateExclusiveDirectory
                | Self::MovableDirectory
                | Self::GenerationDirectoryAccess => FILE_DIRECTORY_FILE,
                #[cfg(test)]
                Self::RemoveDirectory => FILE_DIRECTORY_FILE,
                Self::DirectoryLease
                | Self::DirectoryPublicationParent
                | Self::DataDirectory
                | Self::CreateDataDirectory => FILE_DIRECTORY_FILE,
                _ => FILE_NON_DIRECTORY_FILE,
            }
    }

    fn share(self) -> u32 {
        match self {
            Self::Traverse
            | Self::Read
            | Self::CreateExclusiveDirectory
            | Self::MovableDirectory
            | Self::GenerationDirectoryAccess
            | Self::GenerationFileAccess => {
                FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0 | FILE_SHARE_DELETE
            }
            Self::FenceFile => 0,
            Self::Stage | Self::ObserveFile | Self::MutateFile => FILE_SHARE_READ.0,
            Self::DirectoryLease | Self::DirectoryPublicationParent => FILE_SHARE_READ.0,
            _ => FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0,
        }
    }
}

struct Opened {
    handle: OwnedHandle,
    created: bool,
}

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
    UserData,
    Root,
    Administrators,
    Ledger,
    Payload,
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
    flags: u8,
}

#[derive(Clone)]
struct SidBuf(Vec<u32>);

impl SidBuf {
    fn parse(text: &str) -> Result<Self, FilesystemError> {
        use windows::Win32::Security::Authorization::ConvertStringSidToSidW;
        use windows::Win32::Security::GetLengthSid;
        let text: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
        let mut sid = PSID::default();
        unsafe {
            ConvertStringSidToSidW(PCWSTR(text.as_ptr()), &mut sid).map_err(io_other)?;
        }
        let length = unsafe { GetLengthSid(sid) } as usize;
        let mut bytes = vec![0u32; length.div_ceil(4)];
        unsafe {
            std::ptr::copy_nonoverlapping(
                sid.0.cast::<u8>(),
                bytes.as_mut_ptr().cast::<u8>(),
                length,
            );
            let _ = LocalFree(Some(windows::Win32::Foundation::HLOCAL(sid.0)));
        }
        Ok(Self(bytes))
    }

    fn as_psid(&self) -> PSID {
        PSID(self.0.as_ptr() as *mut _)
    }
}

impl SecurityPolicy {
    fn for_kind(kind: PolicyKind, directory: bool) -> Result<Self, FilesystemError> {
        let owner = if kind == PolicyKind::UserData {
            SidBuf::parse(
                crate::transport::UserSid::current()
                    .map_err(|error| io_std(std::io::Error::other(error)))?
                    .display(),
            )?
        } else {
            well_known(WinLocalSystemSid)?
        };
        let inheritance = if directory { INHERIT } else { 0 };
        let mut grants = vec![
            Grant {
                flags: inheritance,
                sid: well_known(WinLocalSystemSid)?,
                mask: 0x001F_01FF,
            },
            Grant {
                flags: inheritance,
                sid: well_known(WinBuiltinAdministratorsSid)?,
                mask: 0x001F_01FF,
            },
            Grant {
                flags: inheritance,
                sid: well_known(WinCreatorOwnerRightsSid)?,
                mask: READ_CONTROL | SYNCHRONIZE,
            },
        ];
        if kind == PolicyKind::UserData {
            grants.push(Grant {
                flags: inheritance,
                sid: owner.clone(),
                mask: 0x001F_01FF,
            });
        }
        if matches!(kind, PolicyKind::Ledger | PolicyKind::Payload) {
            grants.push(Grant {
                flags: inheritance,
                sid: well_known(WinBuiltinUsersSid)?,
                mask: FILE_GENERIC_READ.0
                    | if directory || kind == PolicyKind::Payload {
                        FILE_TRAVERSE
                    } else {
                        0
                    },
            });
        }
        if kind == PolicyKind::Root {
            grants.push(Grant {
                flags: inheritance,
                sid: well_known(WinBuiltinUsersSid)?,
                mask: FILE_TRAVERSE | 0x80 | READ_CONTROL | SYNCHRONIZE,
            });
        }
        Ok(Self { owner, grants })
    }
}

pub struct MachineDir {
    handle: OwnedHandle,
    volume: Arc<OwnedHandle>,
    kind: PolicyKind,
}

impl MachineDir {
    pub fn payload_root(path: &Path, create: bool) -> Result<Self, FilesystemError> {
        if !crate::transport::is_process_elevated()
            .map_err(|error| io_std(std::io::Error::other(error)))?
        {
            return Err(io_path(path, "machine payload requires an elevated token"));
        }
        let (drive, names) = split_absolute(path)?;
        let mut current = open_drive(drive, Open::InspectDirectory)?;
        verify_trusted_ancestor(&current)?;
        let (leaf, parents) = names
            .split_last()
            .ok_or_else(|| io_path(path, "payload root has no leaf"))?;
        for name in parents {
            current = open_relative(&current, name, Open::InspectDirectory, None)?.handle;
            reject_reparse(&current)?;
            verify_trusted_ancestor(&current)?;
        }
        let volume = Arc::new(open_volume(&current)?);
        let opened = open_relative(
            &current,
            leaf,
            if create {
                Open::CreateDirectory
            } else {
                Open::Directory
            },
            create.then_some(PolicyKind::Payload),
        )?;
        reject_reparse(&opened.handle)?;
        verify_handle(&opened.handle, PolicyKind::Payload)?;
        if opened.created {
            flush_handle(&volume)?;
        }
        Ok(Self {
            handle: opened.handle,
            volume,
            kind: PolicyKind::Payload,
        })
    }

    pub(crate) fn generation_child(
        &self,
        name: &str,
        create: bool,
        access: zup_transaction::GenerationAccess,
    ) -> Result<Self, FilesystemError> {
        self.validate()?;
        if create {
            let kind = match access {
                zup_transaction::GenerationAccess::Private => PolicyKind::Administrators,
                zup_transaction::GenerationAccess::Public => PolicyKind::Payload,
                zup_transaction::GenerationAccess::Publishing => {
                    return Err(io_path(
                        Path::new(name),
                        "publication cannot create a directory",
                    ));
                }
            };
            return self.child(name, kind, Open::CreateExclusiveDirectory);
        }
        let opened = open_relative(
            &self.handle,
            OsStr::new(name),
            if access == zup_transaction::GenerationAccess::Public {
                Open::MovableDirectory
            } else {
                Open::GenerationDirectoryAccess
            },
            None,
        )?;
        reject_reparse(&opened.handle)?;
        let kind = generation_policy(&opened.handle, access, true)?;
        Ok(Self {
            handle: opened.handle,
            volume: Arc::clone(&self.volume),
            kind,
        })
    }

    pub(crate) fn open_generation_file(
        &self,
        name: &str,
        access: zup_transaction::GenerationAccess,
    ) -> Result<File, FilesystemError> {
        self.validate()?;
        let opened = open_relative(&self.handle, OsStr::new(name), Open::Read, None)?;
        generation_policy(&opened.handle, access, false)?;
        Ok(File::from(opened.handle))
    }

    pub(crate) fn expose_generation_file(
        &self,
        name: &str,
        expected: (u64, [u8; 16]),
    ) -> Result<(), FilesystemError> {
        self.validate()?;
        let opened = open_relative(
            &self.handle,
            OsStr::new(name),
            Open::GenerationFileAccess,
            None,
        )?;
        generation_policy(
            &opened.handle,
            zup_transaction::GenerationAccess::Publishing,
            false,
        )?;
        if identity(&opened.handle)? != expected {
            return Err(io_path(
                Path::new(name),
                "generation file identity changed before exposure",
            ));
        }
        expose_payload_access(&opened.handle, false)?;
        if identity(&opened.handle)? != expected {
            return Err(io_path(
                Path::new(name),
                "generation file identity changed during exposure",
            ));
        }
        self.sync()
    }

    pub(crate) fn expose_generation_directory(
        mut self,
        expected: (u64, [u8; 16]),
    ) -> Result<Self, FilesystemError> {
        self.validate()?;
        if identity(&self.handle)? != expected {
            return Err(io_path(
                Path::new("."),
                "generation directory identity changed before exposure",
            ));
        }
        if self.kind != PolicyKind::Payload {
            expose_payload_access(&self.handle, true)?;
            self.kind = PolicyKind::Payload;
        }
        self.validate()?;
        self.sync()?;
        Ok(self)
    }

    pub fn object_identity(&self) -> Result<(u64, [u8; 16]), FilesystemError> {
        self.validate()?;
        identity(&self.handle)
    }

    pub fn rename_directory(
        &self,
        source: &str,
        destination: &Self,
        target: &str,
        expected: (u64, [u8; 16]),
    ) -> Result<(), FilesystemError> {
        self.validate()?;
        destination.validate()?;
        if self.kind != destination.kind {
            return Err(io_path(
                Path::new(target),
                "directory publication changes metadata policy",
            ));
        }
        validate_name(OsStr::new(target))?;
        let source = self.child(source, self.kind, Open::MovableDirectory)?;
        if source.object_identity()? != expected {
            return Err(io_path(
                Path::new(target),
                "staging directory identity changed",
            ));
        }
        rename_handle(&source.handle, &destination.handle, target, false)
            .map_err(|error| io_std(std::io::Error::other(error)))?;
        self.sync()?;
        destination.sync()?;
        if destination
            .child(target, self.kind, Open::MovableDirectory)?
            .object_identity()?
            != expected
        {
            return Err(io_path(
                Path::new(target),
                "published directory identity changed",
            ));
        }
        Ok(())
    }

    pub(crate) fn stage_generation_file(
        &self,
        name: &str,
        reader: &mut dyn std::io::Read,
        size: u64,
        digest: zup_core::Sha256Digest,
    ) -> Result<(), FilesystemError> {
        use std::io::{Read, Write};
        self.validate()?;
        validate_name(OsStr::new(name))?;
        let staging = open_relative(
            &self.handle,
            OsStr::new(&format!(".payload-{}", uuid::Uuid::new_v4())),
            Open::Stage,
            Some(PolicyKind::Administrators),
        )?
        .handle;
        let result = (|| {
            let mut file = File::from(staging.try_clone().map_err(io_std)?);
            std::io::copy(&mut reader.take(size.saturating_add(1)), &mut file).map_err(io_std)?;
            file.flush().map_err(io_std)?;
            file.sync_all().map_err(io_std)?;
            use std::io::{Seek, SeekFrom};
            file.seek(SeekFrom::Start(0)).map_err(io_std)?;
            let observed = zup_core::hash_reader(file).map_err(io_std)?;
            if observed != (size, digest) {
                return Err(io_path(
                    Path::new(name),
                    "payload differs from authenticated content",
                ));
            }
            verify_file(&staging, PolicyKind::Administrators)?;
            rename_handle(&staging, &self.handle, name, false)
                .map_err(|error| io_std(std::io::Error::other(error)))?;
            self.sync()?;
            let observed =
                self.open_generation_file(name, zup_transaction::GenerationAccess::Private)?;
            if identity(&OwnedHandle::from(observed))? != identity(&staging)? {
                return Err(io_path(
                    Path::new(name),
                    "payload identity changed after publication",
                ));
            }
            Ok(())
        })();
        // A failed publication is reconciled with the tree census. Never delete a possibly published member.
        result
    }

    pub fn open_root(path: &Path) -> Result<Self, FilesystemError> {
        if !crate::transport::is_process_elevated()
            .map_err(|error| io_std(std::io::Error::other(error)))?
        {
            return Err(io_path(
                path,
                "machine state requires a fully elevated token",
            ));
        }
        let (drive, names) = split_absolute(path)?;
        let mut current = open_drive(drive, Open::Traverse)?;
        let Some((leaf, parents)) = names.split_last() else {
            return Err(io_path(path, "machine state path has no leaf"));
        };
        for name in parents {
            current = open_relative(&current, name, Open::Traverse, None)?.handle;
            reject_reparse(&current)?;
        }
        // Resolve the volume through the pinned ancestor, never through a second drive lookup.
        let volume = Arc::new(open_volume(&current)?);
        let opened = open_relative(
            &current,
            leaf,
            Open::CreateDirectory,
            Some(PolicyKind::Root),
        )?;
        reject_reparse(&opened.handle)?;
        verify_handle(&opened.handle, PolicyKind::Root)?;
        if opened.created {
            flush_handle(&volume)?;
        }
        Ok(Self {
            handle: opened.handle,
            volume,
            kind: PolicyKind::Root,
        })
    }

    pub(crate) fn root_identity(path: &Path) -> Result<(u64, [u8; 16]), FilesystemError> {
        let (drive, components) = split_absolute(path)?;
        let (leaf, parents) = components
            .split_last()
            .ok_or_else(|| io_path(path, "machine state path has no leaf"))?;
        let mut handle = open_drive(drive, Open::Traverse)?;
        for component in parents {
            handle = open_relative(&handle, component, Open::Traverse, None)?.handle;
            reject_reparse(&handle)?;
        }
        let handle = open_relative(&handle, leaf, Open::InspectDirectory, None)?.handle;
        reject_reparse(&handle)?;
        verify_handle(&handle, PolicyKind::Root)?;
        identity(&handle)
    }

    pub fn read_ledger(root: &Path, name: &str) -> Result<Option<Vec<u8>>, FilesystemError> {
        let (drive, components) = split_absolute(root)?;
        let (leaf, parents) = components
            .split_last()
            .ok_or_else(|| io_path(root, "machine state path has no leaf"))?;
        let mut handle = open_drive(drive, Open::Traverse)?;
        for component in parents {
            handle = open_relative(&handle, component, Open::Traverse, None)?.handle;
            reject_reparse(&handle)?;
        }
        handle = open_relative(&handle, leaf, Open::InspectDirectory, None)?.handle;
        reject_reparse(&handle)?;
        verify_handle(&handle, PolicyKind::Root)?;
        handle = open_relative(&handle, OsStr::new("installations"), Open::Directory, None)?.handle;
        reject_reparse(&handle)?;
        verify_handle(&handle, PolicyKind::Ledger)?;
        match open_relative(&handle, OsStr::new(name), Open::Read, None) {
            Ok(opened) => read_file(opened.handle, PolicyKind::Ledger).map(Some),
            Err(FilesystemError::Io { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    fn validate(&self) -> Result<(), FilesystemError> {
        reject_reparse(&self.handle)?;
        verify_handle(&self.handle, self.kind)
    }

    pub fn open_dir(&self, name: &str, kind: PolicyKind) -> Result<Self, FilesystemError> {
        self.child(name, kind, Open::Directory)
    }

    pub fn ensure_dir(&self, name: &str, kind: PolicyKind) -> Result<Self, FilesystemError> {
        self.child(name, kind, Open::CreateDirectory)
    }

    fn child(
        &self,
        name: &str,
        kind: PolicyKind,
        operation: Open,
    ) -> Result<Self, FilesystemError> {
        self.validate()?;
        let opened = open_relative(
            &self.handle,
            OsStr::new(name),
            operation,
            matches!(
                operation,
                Open::CreateDirectory | Open::CreateExclusiveDirectory | Open::CreateDataDirectory
            )
            .then_some(kind),
        )?;
        reject_reparse(&opened.handle)?;
        verify_handle(&opened.handle, kind)?;
        if opened.created {
            self.sync()?;
        }
        Ok(Self {
            handle: opened.handle,
            volume: Arc::clone(&self.volume),
            kind,
        })
    }

    pub fn read(&self, name: &str, kind: PolicyKind) -> Result<Option<Vec<u8>>, FilesystemError> {
        self.read_bounded(name, kind, u64::MAX)
    }

    pub fn read_bounded(
        &self,
        name: &str,
        kind: PolicyKind,
        limit: u64,
    ) -> Result<Option<Vec<u8>>, FilesystemError> {
        self.validate()?;
        let handle = match open_relative(&self.handle, OsStr::new(name), Open::Read, None) {
            Ok(opened) => opened.handle,
            Err(FilesystemError::Io { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        read_file_bounded(handle, kind, limit).map(Some)
    }

    pub fn publish(
        &self,
        name: &str,
        bytes: &[u8],
        create_new: bool,
        kind: PolicyKind,
    ) -> Result<(), PublishError> {
        validate_name(OsStr::new(name)).map_err(not_published)?;
        self.validate().map_err(not_published)?;
        let staging_name = format!(".zup-{}.tmp", uuid::Uuid::new_v4());
        let staging = open_relative(
            &self.handle,
            OsStr::new(&staging_name),
            Open::Stage,
            Some(kind),
        )
        .map_err(not_published)?
        .handle;
        let prepared = (|| {
            #[cfg(test)]
            publication_boundary("staged");
            verify_file(&staging, kind)?;
            let mut file = File::from(staging.try_clone().map_err(io_std)?);
            std::io::Write::write_all(&mut file, bytes).map_err(io_std)?;
            #[cfg(test)]
            publication_boundary("written");
            file.sync_all().map_err(io_std)?;
            #[cfg(test)]
            publication_boundary("file-flushed");
            Ok::<_, FilesystemError>(())
        })();
        if let Err(error) = prepared {
            return Err(cleanup_failure(&staging, error));
        }
        if let Err(mut error) = rename_handle(&staging, &self.handle, name, !create_new) {
            if let PublishError::NotPublished { detail, .. } = &mut error
                && let Err(cleanup) = discard(&staging)
            {
                detail.push_str(&format!("; staging cleanup failed: {cleanup}"));
            }
            return Err(error);
        }
        // After rename, every failure requires reconciliation, even if bytes match.
        let verified = (|| {
            #[cfg(test)]
            publication_boundary("renamed");
            self.sync()?;
            #[cfg(test)]
            publication_boundary("volume-flushed");
            let observed = open_relative(&self.handle, OsStr::new(name), Open::Read, None)?.handle;
            if identity(&staging)? != identity(&observed)? {
                return Err(io_path(Path::new(name), "published file identity changed"));
            }
            if read_file(observed, kind)? != bytes {
                return Err(io_path(Path::new(name), "published file content changed"));
            }
            #[cfg(test)]
            publication_boundary("verified");
            Ok(())
        })();
        verified.map_err(|error: FilesystemError| PublishError::Indeterminate {
            may_have_published: true,
            detail: error.to_string(),
        })
    }

    pub fn open_lock(&self, name: &str) -> Result<File, FilesystemError> {
        self.validate()?;
        let opened = open_relative(
            &self.handle,
            OsStr::new(name),
            Open::Lock,
            Some(PolicyKind::Administrators),
        )?;
        verify_file(&opened.handle, PolicyKind::Administrators)?;
        if opened.created {
            self.sync()?;
        }
        Ok(File::from(opened.handle))
    }

    pub fn remove(&self, name: &str) -> Result<(), FilesystemError> {
        self.validate()?;
        let opened = match open_relative(&self.handle, OsStr::new(name), Open::RemoveFile, None) {
            Ok(opened) => opened,
            Err(FilesystemError::Io { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                return self.sync();
            }
            Err(error) => return Err(error),
        };
        verify_file(&opened.handle, self.kind)?;
        discard(&opened.handle)?;
        drop(opened);
        self.sync()
    }

    pub fn list(&self) -> Result<Vec<String>, FilesystemError> {
        self.validate()?;
        Ok(list_entries(&self.handle)?
            .into_iter()
            .map(|entry| entry.name)
            .collect())
    }

    #[cfg(test)]
    pub fn remove_directory(&self, name: &str) -> Result<(), FilesystemError> {
        self.validate()?;
        let opened =
            match open_relative(&self.handle, OsStr::new(name), Open::RemoveDirectory, None) {
                Ok(opened) => opened,
                Err(FilesystemError::Io { source, .. })
                    if source.kind() == std::io::ErrorKind::NotFound =>
                {
                    return self.sync();
                }
                Err(error) => return Err(error),
            };
        remove_tree(opened.handle)?;
        self.sync()
    }

    pub(crate) fn remove_empty_directory(
        &self,
        name: &str,
        expected: (u64, [u8; 16]),
    ) -> Result<(), FilesystemError> {
        self.validate()?;
        remove_empty_directory(&self.handle, name, expected)?;
        self.sync()
    }

    pub(crate) fn sync(&self) -> Result<(), FilesystemError> {
        #[cfg(test)]
        if std::env::var("ZUP_MACHINE_FAIL_SYNC").is_ok() {
            return Err(io_std(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "injected unsupported volume barrier",
            )));
        }
        flush_handle(&self.volume)
    }
}

fn read_file(handle: OwnedHandle, kind: PolicyKind) -> Result<Vec<u8>, FilesystemError> {
    read_file_bounded(handle, kind, u64::MAX)
}

fn read_file_bounded(
    handle: OwnedHandle,
    kind: PolicyKind,
    limit: u64,
) -> Result<Vec<u8>, FilesystemError> {
    use std::io::Read;
    verify_file(&handle, kind)?;
    let mut bytes = Vec::new();
    File::from(handle)
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(io_std)?;
    if bytes.len() as u64 > limit {
        return Err(io_path(
            Path::new("protected metadata"),
            "record exceeds its read limit",
        ));
    }
    Ok(bytes)
}

#[cfg(test)]
fn remove_tree(handle: OwnedHandle) -> Result<(), FilesystemError> {
    reject_reparse(&handle)?;
    verify_handle(&handle, PolicyKind::Administrators)?;
    if attributes(&handle)? & FILE_ATTRIBUTE_DIRECTORY == 0 {
        return Err(io_path(
            Path::new("journal"),
            "expected a journal directory",
        ));
    }
    for entry in list_entries(&handle)? {
        let operation = if entry.directory {
            Open::RemoveDirectory
        } else {
            Open::RemoveFile
        };
        let child = open_relative(&handle, OsStr::new(&entry.name), operation, None)?.handle;
        reject_reparse(&child)?;
        verify_handle(&child, PolicyKind::Administrators)?;
        if entry.directory {
            remove_tree(child)?;
        } else {
            verify_file(&child, PolicyKind::Administrators)?;
            discard(&child)?;
        }
    }
    discard(&handle)
}

fn not_published(error: FilesystemError) -> PublishError {
    let kind = match &error {
        FilesystemError::Io { source, .. } => Some(source.kind()),
        _ => None,
    };
    PublishError::NotPublished {
        kind,
        detail: error.to_string(),
    }
}

fn cleanup_failure(staging: &OwnedHandle, error: FilesystemError) -> PublishError {
    match discard(staging) {
        Ok(()) => not_published(error),
        Err(cleanup) => {
            let mut failure = not_published(error);
            if let PublishError::NotPublished { detail, .. } = &mut failure {
                detail.push_str(&format!("; staging cleanup failed: {cleanup}"));
            }
            failure
        }
    }
}

pub(crate) fn remove_empty_directory(
    parent: &impl AsHandle,
    name: &str,
    expected: (u64, [u8; 16]),
) -> Result<(), FilesystemError> {
    let parent = parent.as_handle().try_clone_to_owned().map_err(io_std)?;
    let handle = open_relative(&parent, OsStr::new(name), Open::MovableDirectory, None)?.handle;
    reject_reparse(&handle)?;
    if identity(&handle)? != expected || !list_entries(&handle)?.is_empty() {
        return Err(io_path(
            Path::new(name),
            "recovery directory identity or contents changed before removal",
        ));
    }
    discard(&handle)
}

pub(crate) fn identity(handle: &impl AsHandle) -> Result<(u64, [u8; 16]), FilesystemError> {
    let mut info = FILE_ID_INFO::default();
    unsafe {
        GetFileInformationByHandleEx(
            HANDLE(handle.as_handle().as_raw_handle()),
            FileIdInfo,
            (&mut info as *mut FILE_ID_INFO).cast(),
            size_of::<FILE_ID_INFO>() as u32,
        )
        .map_err(io_other)?;
    }
    Ok((info.VolumeSerialNumber, info.FileId.Identifier))
}

pub(crate) fn verify_payload_file(handle: &impl AsHandle) -> Result<(), FilesystemError> {
    use windows::Win32::Storage::FileSystem::{
        FILE_STANDARD_INFO, FILE_STREAM_INFO, FileStandardInfo, FileStreamInfo,
    };
    let handle = HANDLE(handle.as_handle().as_raw_handle());
    let mut standard = FILE_STANDARD_INFO::default();
    let mut buffer = [0u64; 64];
    unsafe {
        GetFileInformationByHandleEx(
            handle,
            FileStandardInfo,
            (&mut standard as *mut FILE_STANDARD_INFO).cast(),
            size_of::<FILE_STANDARD_INFO>() as u32,
        )
        .map_err(io_other)?;
        GetFileInformationByHandleEx(
            handle,
            FileStreamInfo,
            buffer.as_mut_ptr().cast(),
            size_of_val(&buffer) as u32,
        )
        .map_err(io_other)?;
    }
    let stream = unsafe { &*buffer.as_ptr().cast::<FILE_STREAM_INFO>() };
    let expected = [
        b':' as u16,
        b':' as u16,
        b'$' as u16,
        b'D' as u16,
        b'A' as u16,
        b'T' as u16,
        b'A' as u16,
    ];
    if standard.NumberOfLinks != 1
        || standard.DeletePending
        || standard.Directory
        || stream.NextEntryOffset != 0
        || stream.StreamNameLength as usize != size_of_val(&expected)
        || stream.StreamSize != standard.EndOfFile
    {
        return Err(io_path(
            Path::new("payload"),
            "payload must have one name and only its default data stream",
        ));
    }
    let name = unsafe { std::slice::from_raw_parts(stream.StreamName.as_ptr(), expected.len()) };
    if name != expected {
        return Err(io_path(
            Path::new("payload"),
            "payload has an unexpected data stream",
        ));
    }
    Ok(())
}

pub(crate) fn private_directory_identity(
    directory: &private_fs::PrivateDir,
) -> Result<(u64, [u8; 16]), FilesystemError> {
    directory
        .sync_all()
        .map_err(|error| io_std(std::io::Error::other(error)))?;
    identity(&directory.as_handle().try_clone_to_owned().map_err(io_std)?)
}

pub(crate) fn rename_private_directory(
    parent: &private_fs::PrivateDir,
    source: &str,
    destination: &private_fs::PrivateDir,
    target: &str,
    expected: (u64, [u8; 16]),
) -> Result<(), FilesystemError> {
    validate_name(OsStr::new(target))?;
    let directory = parent
        .open_dir(source)
        .map_err(|error| io_std(std::io::Error::other(error)))?;
    if private_directory_identity(&directory)? != expected {
        return Err(io_path(
            Path::new(source),
            "staging directory identity changed",
        ));
    }
    let parent_handle = parent.as_handle().try_clone_to_owned().map_err(io_std)?;
    let source_handle = open_relative(
        &parent_handle,
        OsStr::new(source),
        Open::MovableDirectory,
        None,
    )?
    .handle;
    reject_reparse(&source_handle)?;
    if identity(&source_handle)? != expected {
        return Err(io_path(
            Path::new(source),
            "staging directory identity changed",
        ));
    }
    let destination_handle = destination
        .as_handle()
        .try_clone_to_owned()
        .map_err(io_std)?;
    rename_handle(&source_handle, &destination_handle, target, false)
        .map_err(|error| io_std(std::io::Error::other(error)))?;
    parent
        .sync_all()
        .map_err(|error| io_std(std::io::Error::other(error)))?;
    destination
        .sync_all()
        .map_err(|error| io_std(std::io::Error::other(error)))?;
    let observed = destination
        .open_dir(target)
        .map_err(|error| io_std(std::io::Error::other(error)))?;
    if private_directory_identity(&observed)? != expected {
        return Err(io_path(
            Path::new(target),
            "published directory identity changed",
        ));
    }
    Ok(())
}

fn final_path(directory: &OwnedHandle) -> Result<Vec<u16>, FilesystemError> {
    let mut path = vec![0u16; 32768];
    let length = unsafe {
        GetFinalPathNameByHandleW(
            HANDLE(directory.as_raw_handle()),
            &mut path,
            VOLUME_NAME_GUID,
        )
    } as usize;
    if length == 0 {
        return Err(io_std(std::io::Error::last_os_error()));
    }
    if length >= path.len() {
        return Err(io_path(Path::new("volume"), "volume path is too long"));
    }
    path.truncate(length);
    Ok(path)
}

fn open_volume(directory: &OwnedHandle) -> Result<OwnedHandle, FilesystemError> {
    let mut path = final_path(directory)?;
    let end = path[..]
        .iter()
        .position(|&unit| unit == b'}' as u16)
        .ok_or_else(|| io_path(Path::new("volume"), "expected a local volume GUID"))?;
    path.truncate(end + 1);
    path.push(0);
    let handle = unsafe {
        CreateFileW(
            PCWSTR(path.as_ptr()),
            FILE_GENERIC_WRITE.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        )
    }
    .map_err(io_other)?;
    Ok(unsafe { OwnedHandle::from_raw_handle(handle.0) })
}

fn rename_handle(
    file: &impl AsHandle,
    directory: &impl AsHandle,
    name: &str,
    replace: bool,
) -> Result<(), PublishError> {
    let wide: Vec<u16> = OsStr::new(name).encode_wide().collect();
    let header = offset_of!(FILE_RENAME_INFO, FileName);
    let byte_len = header + wide.len() * 2;
    let mut buffer = vec![0u64; byte_len.div_ceil(8)];
    let mut io = IoStatus {
        status: 0,
        information: 0,
    };
    let status = unsafe {
        let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFO>();
        (*info).Anonymous.ReplaceIfExists = replace;
        (*info).RootDirectory = HANDLE(directory.as_handle().as_raw_handle());
        (*info).FileNameLength = (wide.len() * 2) as u32;
        std::ptr::copy_nonoverlapping(wide.as_ptr(), (*info).FileName.as_mut_ptr(), wide.len());
        // Win32 SetFileInformationByHandle rejects a non-NULL rename RootDirectory.
        NtSetInformationFile(
            HANDLE(file.as_handle().as_raw_handle()),
            &mut io,
            buffer.as_ptr().cast(),
            byte_len as u32,
            10,
        )
    };
    if status >= 0 {
        return Ok(());
    }
    let source = std::io::Error::from_raw_os_error(unsafe { RtlNtStatusToDosError(status) } as i32);
    let detail = format!(
        "NtSetInformationFile(FileRenameInformation) destination={name:?} replace={replace} NTSTATUS={status:#010x}: {source}"
    );
    match status as u32 {
        0xc000_0022 | 0xc000_0035 | 0xc000_0043 | 0xc000_0056 | 0xc000_000d | 0xc000_00bb
        | 0xc000_0033 | 0xc000_0034 | 0xc000_0008 | 0xc000_0121 => {
            Err(PublishError::NotPublished {
                kind: Some(source.kind()),
                detail,
            })
        }
        _ => Err(PublishError::Indeterminate {
            may_have_published: true,
            detail,
        }),
    }
}

pub(crate) fn discard(handle: &impl AsHandle) -> Result<(), FilesystemError> {
    let info = FILE_DISPOSITION_INFO { DeleteFile: true };
    unsafe {
        SetFileInformationByHandle(
            HANDLE(handle.as_handle().as_raw_handle()),
            FileDispositionInfo,
            (&info as *const FILE_DISPOSITION_INFO).cast(),
            size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
        .map_err(io_other)
    }
}

fn flush_handle(handle: &OwnedHandle) -> Result<(), FilesystemError> {
    unsafe { FlushFileBuffers(HANDLE(handle.as_raw_handle())) }.map_err(|error| {
        let error = io_other(error);
        let kind = match &error {
            FilesystemError::Io { source, .. } => source.kind(),
            _ => std::io::ErrorKind::Other,
        };
        FilesystemError::Io {
            path: "volume".into(),
            source: std::io::Error::new(kind, format!("FlushFileBuffers(volume): {error}")),
        }
    })
}

struct DirectoryEntry {
    name: String,
    #[cfg(test)]
    directory: bool,
}

fn list_entries(handle: &OwnedHandle) -> Result<Vec<DirectoryEntry>, FilesystemError> {
    let mut names = Vec::new();
    let mut buffer = vec![0u64; 2048];
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
                (buffer.len() * 8) as u32,
            )
        };
        if let Err(error) = listed {
            if error.code().0 as u32 & 0xffff == ERROR_NO_MORE_FILES.0 {
                break;
            }
            return Err(io_other(error));
        }
        let mut offset = 0usize;
        loop {
            let info = unsafe {
                &*buffer
                    .as_ptr()
                    .cast::<u8>()
                    .add(offset)
                    .cast::<FullDirInfo>()
            };
            let name_len = info.file_name_length as usize / 2;
            let name_ptr = unsafe {
                buffer
                    .as_ptr()
                    .cast::<u8>()
                    .add(offset)
                    .add(offset_of!(FullDirInfo, file_name))
            };
            let wide = unsafe { std::slice::from_raw_parts(name_ptr.cast::<u16>(), name_len) };
            let name = OsString::from_wide(wide);
            if name != "." && name != ".." {
                names.push(DirectoryEntry {
                    name: name
                        .into_string()
                        .map_err(|_| io_path(Path::new("journal"), "non-Unicode entry"))?,
                    #[cfg(test)]
                    directory: info.file_attributes & FILE_ATTRIBUTE_DIRECTORY != 0,
                });
            }
            if info.next_entry_offset == 0 {
                break;
            }
            offset += info.next_entry_offset as usize;
        }
    }
    Ok(names)
}

fn verify_file(handle: &OwnedHandle, kind: PolicyKind) -> Result<(), FilesystemError> {
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

fn verify_handle(handle: &OwnedHandle, kind: PolicyKind) -> Result<(), FilesystemError> {
    let expected =
        SecurityPolicy::for_kind(kind, attributes(handle)? & FILE_ATTRIBUTE_DIRECTORY != 0)?;
    verify_policy(handle, &expected)
}
fn verify_policy(handle: &OwnedHandle, expected: &SecurityPolicy) -> Result<(), FilesystemError> {
    with_security(handle, |owner, dacl, raw| {
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
        if dacl.is_null() {
            return Err(io_path(
                Path::new("machine-state"),
                "machine state has a NULL DACL",
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
            let allowed = unsafe { &*ace_ptr.cast::<ACCESS_ALLOWED_ACE>() };
            let sid =
                PSID(unsafe { (ace_ptr as *mut u8).add(size_of::<ACE_HEADER>() + 4) } as *mut _);
            let position = unmatched.iter().position(|grant| {
                grant.flags == header.AceFlags
                    && grant.mask == allowed.Mask
                    && unsafe { EqualSid(sid, grant.sid.as_psid()).is_ok() }
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
    })
}

fn with_security<T>(
    handle: &impl AsHandle,
    verify: impl FnOnce(PSID, *mut ACL, PSECURITY_DESCRIPTOR) -> Result<T, FilesystemError>,
) -> Result<T, FilesystemError> {
    let mut owner = PSID::default();
    let mut dacl = std::ptr::null_mut();
    let mut raw = PSECURITY_DESCRIPTOR::default();
    let status = unsafe {
        GetSecurityInfo(
            HANDLE(handle.as_handle().as_raw_handle()),
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
        return Err(FilesystemError::Io {
            path: std::path::PathBuf::from("machine-state"),
            source: std::io::Error::from_raw_os_error(status.0 as i32),
        });
    }
    let result = verify(owner, dacl, raw);
    unsafe {
        if !raw.0.is_null() {
            let _ = LocalFree(Some(windows::Win32::Foundation::HLOCAL(raw.0 as *mut _)));
        }
    }
    result
}

fn verify_trusted_ancestor(handle: &OwnedHandle) -> Result<(), FilesystemError> {
    let system = well_known(WinLocalSystemSid)?;
    let administrators = well_known(WinBuiltinAdministratorsSid)?;
    let installer = SidBuf::parse(TRUSTED_INSTALLER_SID)?;
    let trusted = |sid: PSID| {
        [&system, &administrators, &installer]
            .iter()
            .any(|expected| unsafe { EqualSid(sid, expected.as_psid()).is_ok() })
    };
    with_security(handle, |owner, dacl, _| {
        if !trusted(owner) || dacl.is_null() {
            return Err(io_path(
                Path::new("payload-parent"),
                "machine payload ancestor is not owned by a trusted machine principal",
            ));
        }
        let acl = unsafe { &*dacl };
        for index in 0..acl.AceCount {
            let mut ace = std::ptr::null_mut();
            unsafe {
                GetAce(acl, index as u32, &mut ace).map_err(io_other)?;
            }
            let header = unsafe { &*ace.cast::<ACE_HEADER>() };
            if header.AceFlags & 0x08 != 0 || header.AceType == 1 {
                continue;
            }
            if header.AceType != ACCESS_ALLOWED_ACE_TYPE {
                return Err(io_path(
                    Path::new("payload-parent"),
                    "machine payload ancestor has an unsupported access-control entry",
                ));
            }
            let allowed = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
            let sid = PSID(unsafe { ace.cast::<u8>().add(size_of::<ACE_HEADER>() + 4) }.cast());
            // Creating siblings does not authorize replacing an existing protected child.
            let unsafe_rights = DELETE
                | 0x40
                | 0x2
                | 0x10
                | 0x100
                | 0x4_0000
                | 0x8_0000
                | 0x1000_0000
                | 0x4000_0000;
            if allowed.Mask & unsafe_rights != 0 && !trusted(sid) {
                return Err(io_path(
                    Path::new("payload-parent"),
                    "machine payload ancestor permits untrusted mutation",
                ));
            }
        }
        Ok(())
    })
}

fn reject_reparse(handle: &OwnedHandle) -> Result<(), FilesystemError> {
    if attributes(handle)? & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io_path(Path::new("machine-state"), "reparse point"));
    }
    Ok(())
}

fn attributes(handle: &OwnedHandle) -> Result<u32, FilesystemError> {
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

fn open_drive(drive: u8, operation: Open) -> Result<OwnedHandle, FilesystemError> {
    let wide: Vec<u16> = format!("\\??\\{}:\\", char::from(drive))
        .encode_utf16()
        .collect();
    let handle = nt_open(None, &wide, operation, None)?.handle;
    reject_reparse(&handle)?;
    Ok(handle)
}

fn validate_name(name: &OsStr) -> Result<(), FilesystemError> {
    let text = name
        .to_str()
        .ok_or_else(|| io_path(Path::new(name), "non-Unicode component"))?;
    if text.is_empty()
        || text == "."
        || text == ".."
        || text.contains(['/', '\\', ':', '\0'])
        || text.ends_with(['.', ' '])
        || text.encode_utf16().count() > 255
    {
        return Err(io_path(
            Path::new(name),
            "machine child name is not a single normal component",
        ));
    }
    Ok(())
}

fn generation_policy(
    handle: &OwnedHandle,
    access: zup_transaction::GenerationAccess,
    directory: bool,
) -> Result<PolicyKind, FilesystemError> {
    reject_reparse(handle)?;
    let verify = |kind| {
        if directory {
            verify_handle(handle, kind)
        } else {
            verify_file(handle, kind)
        }
    };
    match access {
        zup_transaction::GenerationAccess::Private => {
            verify(PolicyKind::Administrators)?;
            Ok(PolicyKind::Administrators)
        }
        zup_transaction::GenerationAccess::Public => {
            verify(PolicyKind::Payload)?;
            Ok(PolicyKind::Payload)
        }
        zup_transaction::GenerationAccess::Publishing => {
            if verify(PolicyKind::Administrators).is_ok() {
                Ok(PolicyKind::Administrators)
            } else {
                verify(PolicyKind::Payload)?;
                Ok(PolicyKind::Payload)
            }
        }
    }
}

fn expose_payload_access(handle: &OwnedHandle, directory: bool) -> Result<(), FilesystemError> {
    use windows::Win32::Security::{
        Authorization::{SE_FILE_OBJECT, SetSecurityInfo},
        DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, PROTECTED_DACL_SECURITY_INFORMATION,
    };
    let policy = SecurityPolicy::for_kind(PolicyKind::Payload, directory)?;
    let mut descriptor = descriptor_for(&policy)?;
    let mut present = false.into();
    let mut defaulted = false.into();
    let mut dacl = std::ptr::null_mut();
    unsafe {
        GetSecurityDescriptorDacl(
            PSECURITY_DESCRIPTOR(descriptor.as_ptr()?),
            &mut present,
            &mut dacl,
            &mut defaulted,
        )
        .map_err(io_other)?;
        SetSecurityInfo(
            HANDLE(handle.as_raw_handle()),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(dacl),
            None,
        )
        .ok()
        .map_err(io_other)?;
    }
    generation_policy(handle, zup_transaction::GenerationAccess::Public, directory).map(|_| ())
}

fn open_relative(
    parent: &OwnedHandle,
    name: &OsStr,
    operation: Open,
    policy: Option<PolicyKind>,
) -> Result<Opened, FilesystemError> {
    validate_name(name)?;
    nt_open(
        Some(parent),
        &name.encode_wide().collect::<Vec<_>>(),
        operation,
        policy,
    )
}

fn nt_open(
    parent: Option<&OwnedHandle>,
    wide: &[u16],
    operation: Open,
    policy: Option<PolicyKind>,
) -> Result<Opened, FilesystemError> {
    nt_open_with_ea(parent, wide, operation, policy, &[])
}

fn nt_open_with_ea(
    parent: Option<&OwnedHandle>,
    wide: &[u16],
    operation: Open,
    policy: Option<PolicyKind>,
    ea: &[u8],
) -> Result<Opened, FilesystemError> {
    let assign_system_owner = policy.is_some_and(|kind| kind != PolicyKind::UserData);
    let policy = policy
        .map(|kind| SecurityPolicy::for_kind(kind, operation.options() & FILE_DIRECTORY_FILE != 0))
        .transpose()?;
    let mut descriptor = policy.as_ref().map(descriptor_for).transpose()?;
    let mut name = wide.to_vec();
    let string = UnicodeString {
        length: (name.len() * 2) as u16,
        maximum_length: (name.len() * 2) as u16,
        buffer: name.as_mut_ptr(),
    };
    let attributes = ObjectAttributes {
        length: size_of::<ObjectAttributes>() as u32,
        root_directory: parent
            .map(|parent| HANDLE(parent.as_raw_handle()))
            .unwrap_or_default(),
        object_name: &string,
        attributes: OBJ_CASE_INSENSITIVE | OBJ_DONT_REPARSE,
        security_descriptor: descriptor
            .as_mut()
            .map(Descriptor::as_ptr)
            .transpose()?
            .unwrap_or(std::ptr::null_mut()),
        security_quality_of_service: std::ptr::null_mut(),
    };
    let mut handle = HANDLE::default();
    let mut status_block = IoStatus {
        status: 0,
        information: 0,
    };
    // Only object creation needs SYSTEM-owner assignment. Never alter the process token.
    let _privilege = if assign_system_owner {
        Some(RestorePrivilege::acquire()?)
    } else {
        None
    };
    let status = unsafe {
        NtCreateFile(
            &mut handle,
            operation.access(),
            &attributes,
            &mut status_block,
            std::ptr::null(),
            FILE_ATTRIBUTE_NORMAL,
            operation.share(),
            operation.disposition(),
            operation.options(),
            if ea.is_empty() {
                std::ptr::null()
            } else {
                ea.as_ptr().cast()
            },
            ea.len() as u32,
        )
    };
    if status < 0 {
        let source =
            std::io::Error::from_raw_os_error(unsafe { RtlNtStatusToDosError(status) } as i32);
        return Err(FilesystemError::Io {
            path: OsString::from_wide(wide).into(),
            source: std::io::Error::new(
                source.kind(),
                NativeOpenError {
                    detail: format!(
                        "NtCreateFile {operation:?}: NTSTATUS={status:#010x} DesiredAccess={:#010x} ShareAccess={:#x} disposition={} io_information={} existed=unknown-on-failure",
                        operation.access(),
                        operation.share(),
                        operation.disposition(),
                        status_block.information
                    ),
                    source,
                },
            ),
        });
    }
    tracing::trace!(operation = ?operation, component = ?OsString::from_wide(wide), ntstatus = status,
        desired_access = operation.access(), share_access = operation.share(), disposition = operation.disposition(),
        io_information = status_block.information, existed = status_block.information == 1, "NtCreateFile completed");
    Ok(Opened {
        handle: unsafe { OwnedHandle::from_raw_handle(handle.0) },
        created: status_block.information == 2,
    })
}

struct Descriptor {
    _policy: SecurityPolicy,
    _acl: Vec<u32>,
    descriptor: SECURITY_DESCRIPTOR,
}

impl Descriptor {
    fn as_ptr(&mut self) -> Result<*mut core::ffi::c_void, FilesystemError> {
        unsafe {
            SetSecurityDescriptorOwner(
                PSECURITY_DESCRIPTOR(&mut self.descriptor as *mut _ as *mut _),
                Some(self._policy.owner.as_psid()),
                false,
            )
            .map_err(io_other)?;
            SetSecurityDescriptorDacl(
                PSECURITY_DESCRIPTOR(&mut self.descriptor as *mut _ as *mut _),
                true,
                Some(self._acl.as_ptr().cast()),
                false,
            )
            .map_err(io_other)?;
        }
        Ok((&mut self.descriptor as *mut SECURITY_DESCRIPTOR).cast())
    }
}

fn descriptor_for(policy: &SecurityPolicy) -> Result<Descriptor, FilesystemError> {
    let mut acl_size = size_of::<ACL>();
    for grant in &policy.grants {
        acl_size += size_of::<ACCESS_ALLOWED_ACE>() - 4 + grant.sid.0.len() * 4;
    }
    let mut acl = vec![0u32; acl_size.div_ceil(4)];
    unsafe {
        InitializeAcl(
            acl.as_mut_ptr().cast(),
            (acl.len() * 4) as u32,
            ACL_REVISION,
        )
        .map_err(io_other)?;
        for grant in &policy.grants {
            AddAccessAllowedAceEx(
                acl.as_mut_ptr().cast(),
                ACL_REVISION,
                windows::Win32::Security::ACE_FLAGS(grant.flags as u32),
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
) -> Result<SidBuf, FilesystemError> {
    let mut bytes = vec![0u32; 17];
    let mut len = (bytes.len() * 4) as u32;
    unsafe {
        CreateWellKnownSid(kind, None, Some(PSID(bytes.as_mut_ptr().cast())), &mut len)
            .map_err(io_other)?;
    }
    bytes.truncate(len as usize / 4);
    Ok(SidBuf(bytes))
}

struct RestorePrivilege {
    previous: Option<OwnedHandle>,
    _thread_bound: std::marker::PhantomData<std::rc::Rc<()>>,
}

impl RestorePrivilege {
    fn acquire() -> Result<Self, FilesystemError> {
        let mut previous = HANDLE::default();
        let previous = match unsafe {
            OpenThreadToken(
                GetCurrentThread(),
                TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_IMPERSONATE,
                true,
                &mut previous,
            )
        } {
            Ok(()) => Some(unsafe { OwnedHandle::from_raw_handle(previous.0) }),
            Err(error) if error.code().0 as u32 & 0xffff == ERROR_NO_TOKEN.0 => None,
            Err(error) => return Err(io_other(error)),
        };
        let process;
        let source = if let Some(token) = &previous {
            HANDLE(token.as_raw_handle())
        } else {
            let mut token = HANDLE::default();
            unsafe {
                OpenProcessToken(
                    GetCurrentProcess(),
                    TOKEN_DUPLICATE | TOKEN_QUERY,
                    &mut token,
                )
                .map_err(io_other)?;
            }
            process = unsafe { OwnedHandle::from_raw_handle(token.0) };
            HANDLE(process.as_raw_handle())
        };
        let mut duplicate = HANDLE::default();
        unsafe {
            DuplicateTokenEx(
                source,
                TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY | TOKEN_IMPERSONATE | TOKEN_DUPLICATE,
                None,
                SecurityImpersonation,
                TokenImpersonation,
                &mut duplicate,
            )
            .map_err(io_other)?;
        }
        let duplicate = unsafe { OwnedHandle::from_raw_handle(duplicate.0) };
        let mut luid = windows::Win32::Foundation::LUID::default();
        let wide: Vec<u16> = "SeRestorePrivilege\0".encode_utf16().collect();
        unsafe {
            LookupPrivilegeValueW(PCWSTR::null(), PCWSTR(wide.as_ptr()), &mut luid)
                .map_err(io_other)?;
        }
        let privileges = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            Privileges: [windows::Win32::Security::LUID_AND_ATTRIBUTES {
                Luid: luid,
                Attributes: SE_PRIVILEGE_ENABLED,
            }],
        };
        unsafe {
            AdjustTokenPrivileges(
                HANDLE(duplicate.as_raw_handle()),
                false,
                Some(&privileges),
                0,
                None,
                None,
            )
            .map_err(io_other)?;
            if GetLastError() == ERROR_NOT_ALL_ASSIGNED {
                return Err(io_path(
                    Path::new("token"),
                    "SeRestorePrivilege is not assigned to this token",
                ));
            }
            SetThreadToken(None, Some(HANDLE(duplicate.as_raw_handle()))).map_err(io_other)?;
        }
        Ok(Self {
            previous,
            _thread_bound: std::marker::PhantomData,
        })
    }
}

impl Drop for RestorePrivilege {
    fn drop(&mut self) {
        let previous = self
            .previous
            .as_ref()
            .map(|token| HANDLE(token.as_raw_handle()));
        if unsafe { SetThreadToken(None, previous) }.is_err() {
            // Continuing with unintended authority is unsafe, including during unwinding.
            std::process::abort();
        }
    }
}

fn split_absolute(path: &Path) -> Result<(u8, Vec<OsString>), FilesystemError> {
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

fn io_other(error: windows::core::Error) -> FilesystemError {
    let code = error.code().0 as u32;
    if code & 0xffff_0000 == 0x8007_0000 {
        io_std(std::io::Error::from_raw_os_error((code & 0xffff) as i32))
    } else {
        io_std(std::io::Error::other(error.to_string()))
    }
}

fn io_std(source: std::io::Error) -> FilesystemError {
    FilesystemError::Io {
        path: "machine-state".into(),
        source,
    }
}

fn io_path(path: &Path, detail: &str) -> FilesystemError {
    FilesystemError::Io {
        path: path.to_path_buf(),
        source: std::io::Error::new(std::io::ErrorKind::PermissionDenied, detail),
    }
}

#[cfg(test)]
fn publication_boundary(phase: &str) {
    if phase == "renamed"
        && let Some(path) = std::env::var_os("ZUP_MACHINE_TAMPER_SECURITY")
    {
        crate::machine_authority::tests::weaken_dacl(Path::new(&path));
    }
    if std::env::var("ZUP_MACHINE_CRASH").as_deref() == Ok(phase) {
        std::process::exit(73);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use windows::Win32::Security::{
        CreateRestrictedToken, DISABLE_MAX_PRIVILEGE, GetTokenInformation, SID_AND_ATTRIBUTES,
        TokenPrivileges,
    };

    fn root() -> (tempfile::TempDir, MachineDir) {
        let temp = tempfile::tempdir().expect("temporary parent");
        let root =
            MachineDir::open_root(&temp.path().join("state")).expect("elevated machine root");
        (temp, root)
    }

    #[test]
    fn traversal_does_not_request_ancestor_mutation() {
        let drive = std::env::current_dir().expect("cwd");
        let (drive, _) = split_absolute(&drive).expect("local drive");
        open_drive(drive, Open::Traverse).expect("traversal works without write or delete access");
    }

    #[test]
    fn child_names_cannot_escape_the_capability() {
        for name in [
            "",
            ".",
            "..",
            "a/b",
            "a\\b",
            "file:stream",
            "nul\0suffix",
            "a.",
            "a ",
        ] {
            assert!(validate_name(OsStr::new(name)).is_err(), "{name:?}");
        }
    }

    #[test]
    #[ignore = "requires elevated administrator"]
    fn publication_replaces_preserves_collisions_and_reports_sharing_conflicts() {
        let (_temp, root) = root();
        let dir = root
            .ensure_dir("journals", PolicyKind::Administrators)
            .expect("directory");
        dir.publish("record", b"one", true, PolicyKind::Administrators)
            .expect("create");
        let error = dir
            .publish("record", b"collision", true, PolicyKind::Administrators)
            .expect_err("exclusive publication");
        assert!(matches!(
            error,
            PublishError::NotPublished {
                kind: Some(std::io::ErrorKind::AlreadyExists),
                ..
            }
        ));
        assert_eq!(
            dir.read("record", PolicyKind::Administrators)
                .expect("read"),
            Some(b"one".to_vec())
        );
        let held = open_relative(&dir.handle, OsStr::new("record"), Open::Lock, None)
            .expect("reader denying delete sharing");
        let error = dir
            .publish("record", b"two", false, PolicyKind::Administrators)
            .expect_err("sharing conflict");
        assert!(
            matches!(error, PublishError::NotPublished { kind: Some(kind), .. } if kind != std::io::ErrorKind::AlreadyExists)
        );
        drop(held);
        dir.publish("record", b"two", false, PolicyKind::Administrators)
            .expect("replace");
        assert_eq!(
            dir.read("record", PolicyKind::Administrators)
                .expect("read"),
            Some(b"two".to_vec())
        );
        assert_eq!(dir.list().expect("no abandoned staging"), ["record"]);
        drop(dir);
        root.remove_directory("journals").expect("cleanup");
        assert!(root.list().expect("empty").is_empty());
    }

    #[test]
    #[ignore = "requires elevated administrator"]
    fn cleanup_is_delete_only_recursive_and_rejects_changed_security() {
        let (temp, root) = root();
        root.remove_directory("missing")
            .expect("idempotent absent removal");
        assert!(root.list().expect("no directories created").is_empty());
        let dir = root
            .ensure_dir("journal", PolicyKind::Administrators)
            .expect("directory");
        let nested = dir
            .ensure_dir("nested", PolicyKind::Administrators)
            .expect("nested directory");
        nested
            .publish("record", b"data", true, PolicyKind::Administrators)
            .expect("publish");
        let record = temp.path().join("state/journal/nested/record");
        let original = std::fs::metadata(&record)
            .expect("record metadata")
            .permissions();
        let mut readonly = original.clone();
        readonly.set_readonly(true);
        std::fs::set_permissions(&record, readonly).expect("read-only file");
        assert!(
            nested.remove("record").is_err(),
            "native disposition failures must propagate"
        );
        assert!(record.exists());
        std::fs::set_permissions(&record, original).expect("restore attributes");
        drop(nested);
        drop(dir);
        root.remove_directory("journal")
            .expect("recursive deletion");
        assert!(!temp.path().join("state/journal").exists());
        root.ensure_dir("unsafe", PolicyKind::Administrators)
            .expect("directory");
        crate::machine_authority::tests::weaken_dacl(&temp.path().join("state/unsafe"));
        assert!(root.remove_directory("unsafe").is_err());
        assert!(temp.path().join("state/unsafe").exists());
    }

    #[test]
    #[ignore = "requires elevated administrator"]
    fn directory_handles_prevent_namespace_redirection_and_refuse_reparse_points() {
        use std::os::windows::fs::symlink_dir;
        let (temp, root) = root();
        let child = root
            .ensure_dir("pinned", PolicyKind::Administrators)
            .expect("child");
        let source = temp.path().join("state/pinned");
        assert!(std::fs::rename(&source, temp.path().join("moved")).is_err());
        drop(child);
        let outside = temp.path().join("outside");
        std::fs::create_dir(&outside).expect("outside");
        symlink_dir(&outside, temp.path().join("state/link")).expect("symlink");
        assert!(root.open_dir("link", PolicyKind::Administrators).is_err());
        assert!(root.remove_directory("link").is_err());
        assert!(outside.is_dir());
        std::fs::remove_dir(temp.path().join("state/link")).expect("remove link itself");
    }

    #[test]
    #[ignore = "requires elevated administrator"]
    fn publication_recovers_after_process_exit_and_never_hides_an_unsupported_barrier() {
        const TEST: &str = "filesystem::tests::publication_recovers_after_process_exit_and_never_hides_an_unsupported_barrier";
        if let Some(path) = std::env::var_os("ZUP_MACHINE_CRASH_ROOT") {
            let root = MachineDir::open_root(Path::new(&path)).expect("reopen root");
            let journal = root
                .open_dir("journal", PolicyKind::Administrators)
                .expect("journal");
            let result = journal.publish("record", b"new", false, PolicyKind::Administrators);
            if std::env::var("ZUP_MACHINE_FAIL_SYNC").is_ok()
                || std::env::var("ZUP_MACHINE_TAMPER_SECURITY").is_ok()
            {
                assert!(matches!(
                    result,
                    Err(PublishError::Indeterminate {
                        may_have_published: true,
                        ..
                    })
                ));
                assert_eq!(
                    std::fs::read(Path::new(&path).join("journal/record"))
                        .expect("same visible bytes"),
                    b"new"
                );
                return;
            }
            panic!("crash boundary was not reached: {result:?}");
        }
        for phase in [
            "staged",
            "written",
            "file-flushed",
            "renamed",
            "volume-flushed",
            "verified",
            "unsupported-barrier",
            "tampered-security",
        ] {
            let (temp, root) = root();
            let journal = root
                .ensure_dir("journal", PolicyKind::Administrators)
                .expect("journal");
            journal
                .publish("record", b"old", true, PolicyKind::Administrators)
                .expect("initial record");
            let mut child =
                std::process::Command::new(std::env::current_exe().expect("test binary"));
            child
                .args(["--exact", TEST, "--ignored"])
                .env("ZUP_MACHINE_CRASH_ROOT", temp.path().join("state"));
            if phase == "unsupported-barrier" {
                child.env("ZUP_MACHINE_FAIL_SYNC", "1");
            } else if phase == "tampered-security" {
                child.env(
                    "ZUP_MACHINE_TAMPER_SECURITY",
                    temp.path().join("state/journal/record"),
                );
            } else {
                child.env("ZUP_MACHINE_CRASH", phase);
            }
            let status = child.status().expect("child");
            assert_eq!(
                status.code(),
                Some(
                    if matches!(phase, "unsupported-barrier" | "tampered-security") {
                        0
                    } else {
                        73
                    }
                ),
                "{phase}"
            );
            let expected: &[u8] = if matches!(phase, "staged" | "written" | "file-flushed") {
                b"old"
            } else {
                b"new"
            };
            if phase == "tampered-security" {
                assert!(journal.read("record", PolicyKind::Administrators).is_err());
            } else {
                assert_eq!(
                    journal
                        .read("record", PolicyKind::Administrators)
                        .expect("recoverable record")
                        .as_deref(),
                    Some(expected),
                    "{phase}"
                );
            }
            journal
                .publish("record", b"reconciled", false, PolicyKind::Administrators)
                .expect("republication establishes durability");
            drop(journal);
            root.remove_directory("journal")
                .expect("including validated orphan staging files");
        }
    }

    fn process_privileges() -> Vec<u64> {
        let mut raw = HANDLE::default();
        unsafe {
            OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY | TOKEN_DUPLICATE, &mut raw)
                .expect("process token");
        }
        let token = unsafe { OwnedHandle::from_raw_handle(raw.0) };
        let mut size = 0;
        unsafe {
            let _ = GetTokenInformation(
                HANDLE(token.as_raw_handle()),
                TokenPrivileges,
                None,
                0,
                &mut size,
            );
        }
        let mut buffer = vec![0u64; (size as usize).div_ceil(8)];
        unsafe {
            GetTokenInformation(
                HANDLE(token.as_raw_handle()),
                TokenPrivileges,
                Some(buffer.as_mut_ptr().cast()),
                size,
                &mut size,
            )
            .expect("privileges");
        }
        buffer
    }

    #[test]
    #[ignore = "requires elevated administrator"]
    fn concurrent_privilege_scopes_leave_process_and_thread_tokens_unchanged() {
        let before = process_privileges();
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    for _ in 0..20 {
                        let outer = RestorePrivilege::acquire().expect("outer scope");
                        {
                            let _inner = RestorePrivilege::acquire().expect("nested scope");
                        }
                        drop(outer);
                        let mut token = HANDLE::default();
                        let error = unsafe {
                            OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, true, &mut token)
                        }
                        .expect_err("original thread had no impersonation token");
                        assert_eq!(error.code().0 as u32 & 0xffff, ERROR_NO_TOKEN.0);
                    }
                });
            }
        });
        assert_eq!(process_privileges(), before);
    }

    #[test]
    #[ignore = "requires elevated administrator"]
    fn restricted_token_can_read_ledgers_but_cannot_modify_machine_state() {
        let (temp, root) = root();
        let dir = root
            .ensure_dir("installations", PolicyKind::Ledger)
            .expect("ledgers");
        dir.publish("app.json", b"ledger", true, PolicyKind::Ledger)
            .expect("publish");
        root.ensure_dir("transactions", PolicyKind::Administrators)
            .expect("journals");
        root.ensure_dir("payload", PolicyKind::Administrators)
            .expect("protected staging");
        let installed = temp.path().join("state/installed/application.txt");
        let public_payload = root.ensure_dir("installed", PolicyKind::Payload).unwrap();
        public_payload
            .stage_generation_file(
                "application.txt",
                &mut b"two".as_slice(),
                3,
                zup_core::hash_bytes(b"two"),
            )
            .unwrap();
        let inherited = temp.path().join("state/payload/inherited/file");
        std::fs::create_dir_all(inherited.parent().expect("parent"))
            .expect("inherited work directory");
        std::fs::write(&inherited, b"private work").expect("inherited work file");
        let mut raw = HANDLE::default();
        unsafe {
            OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY | TOKEN_DUPLICATE, &mut raw)
                .expect("process token");
        }
        let token = unsafe { OwnedHandle::from_raw_handle(raw.0) };
        let admin = well_known(WinBuiltinAdministratorsSid).expect("admin SID");
        let mut notify = windows::Win32::Foundation::LUID::default();
        let privilege: Vec<u16> = "SeChangeNotifyPrivilege\0".encode_utf16().collect();
        unsafe {
            LookupPrivilegeValueW(PCWSTR::null(), PCWSTR(privilege.as_ptr()), &mut notify)
                .expect("traverse privilege LUID");
        }
        let mut restricted = HANDLE::default();
        unsafe {
            CreateRestrictedToken(
                HANDLE(token.as_raw_handle()),
                DISABLE_MAX_PRIVILEGE,
                Some(&[SID_AND_ATTRIBUTES {
                    Sid: admin.as_psid(),
                    Attributes: 0,
                }]),
                None,
                None,
                &mut restricted,
            )
            .expect("restricted token");
        }
        let restricted = unsafe { OwnedHandle::from_raw_handle(restricted.0) };
        let mut duplicate = HANDLE::default();
        unsafe {
            DuplicateTokenEx(
                HANDLE(restricted.as_raw_handle()),
                TOKEN_QUERY | TOKEN_IMPERSONATE | TOKEN_DUPLICATE,
                None,
                SecurityImpersonation,
                TokenImpersonation,
                &mut duplicate,
            )
            .expect("impersonation token");
        }
        let duplicate = unsafe { OwnedHandle::from_raw_handle(duplicate.0) };
        let mut required = windows::Win32::Security::PRIVILEGE_SET {
            PrivilegeCount: 1,
            Privilege: [windows::Win32::Security::LUID_AND_ATTRIBUTES {
                Luid: notify,
                Attributes: windows::Win32::Security::SE_PRIVILEGE_ENABLED,
            }],
            ..Default::default()
        };
        let mut traverse = windows::core::BOOL::default();
        unsafe {
            windows::Win32::Security::PrivilegeCheck(
                HANDLE(duplicate.as_raw_handle()),
                &mut required,
                &mut traverse,
            )
            .expect("traverse privilege observation");
        }
        assert!(traverse.as_bool());

        unsafe {
            SetThreadToken(None, Some(HANDLE(duplicate.as_raw_handle())))
                .expect("filtered impersonation");
        }
        let _restore = RestorePrivilege {
            previous: None,
            _thread_bound: std::marker::PhantomData,
        };
        assert!(
            std::fs::read(&installed).is_err(),
            "private preparation must deny reads with traverse privilege"
        );
        drop(_restore);
        let identity = {
            let file = public_payload
                .open_generation_file(
                    "application.txt",
                    zup_transaction::GenerationAccess::Private,
                )
                .unwrap();
            super::identity(&OwnedHandle::from(file)).unwrap()
        };
        public_payload
            .expose_generation_file("application.txt", identity)
            .unwrap();
        unsafe {
            SetThreadToken(None, Some(HANDLE(duplicate.as_raw_handle())))
                .expect("filtered impersonation");
        }
        let _restore = RestorePrivilege {
            previous: None,
            _thread_bound: std::marker::PhantomData,
        };
        assert_eq!(
            MachineDir::read_ledger(&temp.path().join("state"), "app.json").expect("public read"),
            Some(b"ledger".to_vec())
        );
        assert_eq!(
            std::fs::read(&installed).expect("installed machine payload is publicly readable"),
            b"two"
        );
        assert!(
            std::fs::OpenOptions::new()
                .write(true)
                .open(&installed)
                .is_err()
        );
        assert!(std::fs::read_dir(temp.path().join("state/transactions")).is_err());
        assert!(
            std::fs::OpenOptions::new()
                .write(true)
                .open(temp.path().join("state/installations/app.json"))
                .is_err()
        );
        use std::os::windows::fs::OpenOptionsExt;
        assert!(
            std::fs::OpenOptions::new()
                .write(true)
                .open(&inherited)
                .is_err()
        );
        assert!(
            std::fs::OpenOptions::new()
                .access_mode(0x0004_0000)
                .open(&inherited)
                .is_err(),
            "inherited OWNER RIGHTS must prevent owner WRITE_DAC access"
        );
        assert!(
            RestorePrivilege::acquire().is_err(),
            "filtered token has no restore privilege"
        );
        assert!(
            dir.publish("forbidden", b"bad", true, PolicyKind::Ledger)
                .is_err()
        );
    }
}
