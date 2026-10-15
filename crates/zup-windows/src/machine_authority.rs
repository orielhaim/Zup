//! Windows machine state owned by SYSTEM, not by whichever administrator created it.
//!
//! A filtered UAC token keeps the same user SID as the elevated token. If that
//! user is the object owner, Windows grants implicit `WRITE_DAC` unless an
//! OWNER RIGHTS ace replaces those rights. Machine state is therefore created
//! with SYSTEM as owner, a protected DACL for SYSTEM and Administrators, and
//! an OWNER RIGHTS ace that does not grant `WRITE_DAC`. Creation applies that
//! descriptor; nothing is created world-accessible and hardened afterwards.

use std::ffi::OsStr;
use std::fs::File;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, ERROR_SUCCESS, HANDLE, LocalFree};
use windows::Win32::Security::Authorization::{GetSecurityInfo, SE_FILE_OBJECT};
use windows::Win32::Security::{
    ACL, ACL_REVISION, AddAccessAllowedAceEx, AdjustTokenPrivileges, CONTAINER_INHERIT_ACE,
    CreateWellKnownSid, EqualSid, InitializeAcl, InitializeSecurityDescriptor,
    LookupPrivilegeValueW, OBJECT_INHERIT_ACE, OBJECT_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
    PSID, SE_DACL_PROTECTED, SE_PRIVILEGE_ENABLED, SECURITY_DESCRIPTOR,
    SetSecurityDescriptorControl, SetSecurityDescriptorDacl, SetSecurityDescriptorOwner,
    TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY, WinBuiltinAdministratorsSid,
    WinBuiltinUsersSid, WinCreatorOwnerRightsSid, WinLocalSystemSid,
};
use windows::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CREATE_NEW, CreateDirectoryW, CreateFileW, FILE_ATTRIBUTE_NORMAL,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    FlushFileBuffers, GetFileInformationByHandle, MOVEFILE_REPLACE_EXISTING,
    MOVEFILE_WRITE_THROUGH, MoveFileExW, OPEN_EXISTING,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::core::PCWSTR;
use zup_transaction::{
    FilesystemTransactionStore, InstallationLock, JournalVolume, PublishError, StoreError,
    TransactionId,
};

use crate::machine_state::{MachineStateError, default_state_root};

const FILE_ALL_ACCESS: u32 = 0x001F_01FF;
const READ_CONTROL: u32 = 0x0002_0000;
const SECURITY_MAX_SID_SIZE: usize = 68;

#[derive(Clone)]
pub struct MachineRoot {
    inner: Arc<RootInner>,
}

struct RootInner {
    path: PathBuf,
    handle: File,
    identity: FileIdentity,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    volume: u32,
    index_high: u32,
    index_low: u32,
}

impl MachineRoot {
    /// The elevated worker's authoritative root. Caller-supplied paths are not accepted.
    pub fn acquire() -> Result<Self, MachineStateError> {
        let path = default_state_root(zup_core::SelectedScope::Machine)?;
        Self::open_at(&path, true)
    }

    /// Same security policy at an explicit path. Production workers do not call this.
    pub fn establish_at(path: &Path) -> Result<Self, MachineStateError> {
        Self::open_at(path, false)
    }

    pub fn path(&self) -> &Path {
        &self.inner.path
    }

    pub fn transaction_store(&self) -> FilesystemTransactionStore {
        FilesystemTransactionStore::from_volume(Arc::new(MachineJournals {
            root: Arc::clone(&self.inner),
        }))
    }

    pub fn try_lock(&self, key: &str) -> Result<Option<InstallationLock>, MachineStateError> {
        let locks = self.ensure_child("locks", Policy::Administrators)?;
        let name = format!("{key}.lock");
        let path = locks.join(&name);
        let file = create_or_open_file(&path, true)?;
        verify_security(&file, Policy::Administrators)?;
        InstallationLock::try_lock(file, key.to_owned()).map_err(|error| MachineStateError::Io {
            path,
            source: std::io::Error::other(error),
        })
    }

    fn open_at(path: &Path, production: bool) -> Result<Self, MachineStateError> {
        require_full_elevation()?;
        enable_restore_privileges()?;
        let _ = production;
        refuse_reparse_ancestors(path)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| MachineStateError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let mut descriptor = machine_descriptor()?;
        let wide = wide(path);
        let attributes = descriptor.attributes();
        let created = unsafe { CreateDirectoryW(PCWSTR(wide.as_ptr()), Some(&attributes)) };
        match created {
            Ok(()) => {}
            Err(error) if error.code().0 as u32 == ERROR_ALREADY_EXISTS.0 => {}
            Err(error) => {
                return Err(MachineStateError::Io {
                    path: path.to_path_buf(),
                    source: error.into(),
                });
            }
        }
        let handle = open_directory(path)?;
        verify_security(&handle, Policy::Administrators)?;
        let identity = file_identity(&handle).map_err(|source| MachineStateError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        if has_reparse(&handle).map_err(|source| MachineStateError::Io {
            path: path.to_path_buf(),
            source,
        })? {
            return Err(MachineStateError::Io {
                path: path.to_path_buf(),
                source: std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "machine state root is a reparse point",
                ),
            });
        }
        let root = Self {
            inner: Arc::new(RootInner {
                path: path.to_path_buf(),
                handle,
                identity,
            }),
        };
        root.ensure_child("transactions", Policy::Administrators)?;
        root.ensure_child("locks", Policy::Administrators)?;
        root.ensure_child("installations", Policy::Ledger)?;
        Ok(root)
    }

    fn ensure_child(&self, name: &str, policy: Policy) -> Result<PathBuf, MachineStateError> {
        self.inner.confirm()?;
        let path = self.inner.path.join(name);
        if !path.exists() {
            let mut descriptor = match policy {
                Policy::Administrators => machine_descriptor()?,
                Policy::Ledger => ledger_descriptor()?,
            };
            let wide = wide(&path);
            let attributes = descriptor.attributes();
            let created = unsafe { CreateDirectoryW(PCWSTR(wide.as_ptr()), Some(&attributes)) };
            if let Err(error) = created
                && error.code().0 as u32 != ERROR_ALREADY_EXISTS.0
            {
                return Err(MachineStateError::Io {
                    path: path.clone(),
                    source: error.into(),
                });
            }
            sync_handle(&self.inner.handle, &self.inner.path)?;
        }
        let handle = open_directory(&path)?;
        verify_security(&handle, policy)?;
        if has_reparse(&handle).map_err(|source| MachineStateError::Io {
            path: path.clone(),
            source,
        })? {
            return Err(MachineStateError::Io {
                path: path.clone(),
                source: std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "machine state child is a reparse point",
                ),
            });
        }
        Ok(path)
    }
}

impl RootInner {
    fn confirm(&self) -> Result<(), MachineStateError> {
        let current = file_identity(&self.handle).map_err(|error| MachineStateError::Io {
            path: self.path.clone(),
            source: error,
        })?;
        if current != self.identity || has_reparse(&self.handle).unwrap_or(true) {
            return Err(MachineStateError::Io {
                path: self.path.clone(),
                source: std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "machine state root identity changed",
                ),
            });
        }
        Ok(())
    }
}

struct MachineJournals {
    root: Arc<RootInner>,
}

impl JournalVolume for MachineJournals {
    fn read_journal(&self, id: &TransactionId) -> Result<Option<Vec<u8>>, StoreError> {
        let path = journal_file(&self.root.path, id);
        if !path.exists() {
            return Ok(None);
        }
        std::fs::read(&path)
            .map(Some)
            .map_err(|source| StoreError::Io { path, source })
    }

    fn publish_journal(
        &self,
        id: &TransactionId,
        bytes: &[u8],
        create_new: bool,
    ) -> Result<(), StoreError> {
        self.root.confirm().map_err(|error| StoreError::Io {
            path: self.root.path.clone(),
            source: std::io::Error::other(error),
        })?;
        let directory = self.root.path.join("transactions").join(id.to_string());
        ensure_journal_dir(&directory)?;
        let destination = directory.join("transaction.json");
        let temporary = directory.join(format!(".journal-{}.tmp", std::process::id()));
        write_new(&temporary, bytes)?;
        let flags = if create_new {
            MOVEFILE_WRITE_THROUGH
        } else {
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH
        };
        let from = wide(&temporary);
        let to = wide(&destination);
        let moved = unsafe { MoveFileExW(PCWSTR(from.as_ptr()), PCWSTR(to.as_ptr()), flags) };
        if let Err(error) = moved {
            let _ = std::fs::remove_file(&temporary);
            let kind = std::io::Error::from_raw_os_error(error.code().0).kind();
            return Err(StoreError::Publication {
                path: destination,
                source: PublishError::NotPublished {
                    kind: Some(kind),
                    detail: error.to_string(),
                },
            });
        }
        if let Err(error) = sync_directory(&directory) {
            return match confirm_bytes(&destination, bytes) {
                Ok(()) => Ok(()),
                Err(detail) => Err(StoreError::Publication {
                    path: destination,
                    source: PublishError::Indeterminate {
                        may_have_published: true,
                        detail: format!("{error}; {detail}"),
                    },
                }),
            };
        }
        confirm_bytes(&destination, bytes).map_err(|detail| StoreError::Publication {
            path: destination,
            source: PublishError::Indeterminate {
                may_have_published: true,
                detail,
            },
        })
    }

    fn lock_journal(&self, id: &TransactionId) -> Result<File, StoreError> {
        let directory = self.root.path.join("transactions").join(id.to_string());
        ensure_journal_dir(&directory)?;
        let path = directory.join("transaction.lock");
        let file = create_or_open_file(&path, true).map_err(|error| StoreError::Io {
            path: path.clone(),
            source: std::io::Error::other(error),
        })?;
        file.lock()
            .map_err(|source| StoreError::Io { path, source })?;
        Ok(file)
    }

    fn list_journals(&self) -> Result<Vec<TransactionId>, StoreError> {
        let path = self.root.path.join("transactions");
        let Ok(entries) = std::fs::read_dir(&path) else {
            return Ok(Vec::new());
        };
        let mut ids = Vec::new();
        for entry in entries.flatten() {
            let Ok(id) = entry.file_name().to_string_lossy().parse::<TransactionId>() else {
                continue;
            };
            ids.push(id);
        }
        Ok(ids)
    }
}

fn ensure_journal_dir(path: &Path) -> Result<(), StoreError> {
    if path.is_dir() {
        return Ok(());
    }
    let mut descriptor = machine_descriptor().map_err(|error| StoreError::Io {
        path: path.to_path_buf(),
        source: std::io::Error::other(error),
    })?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| StoreError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let wide_path = wide(path);
    let attributes = descriptor.attributes();
    let created = unsafe { CreateDirectoryW(PCWSTR(wide_path.as_ptr()), Some(&attributes)) };
    if let Err(error) = created
        && error.code().0 as u32 != ERROR_ALREADY_EXISTS.0
    {
        return Err(StoreError::Io {
            path: path.to_path_buf(),
            source: error.into(),
        });
    }
    Ok(())
}

fn journal_file(root: &Path, id: &TransactionId) -> PathBuf {
    root.join("transactions")
        .join(id.to_string())
        .join("transaction.json")
}

fn confirm_bytes(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let found = std::fs::read(path).map_err(|error| error.to_string())?;
    if found != bytes {
        return Err("visible bytes are not the attempted journal".into());
    }
    let file = File::open(path).map_err(|error| error.to_string())?;
    file.sync_all().map_err(|error| error.to_string())?;
    let again = std::fs::read(path).map_err(|error| error.to_string())?;
    if again != bytes {
        return Err("journal bytes changed across the durability barrier".into());
    }
    Ok(())
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<(), StoreError> {
    let mut descriptor = machine_descriptor().map_err(|error| StoreError::Io {
        path: path.to_path_buf(),
        source: std::io::Error::other(error),
    })?;
    let wide_path = wide(path);
    let attributes = descriptor.attributes();
    let handle = unsafe {
        CreateFileW(
            PCWSTR(wide_path.as_ptr()),
            FILE_GENERIC_READ.0 | FILE_GENERIC_WRITE.0,
            FILE_SHARE_READ | FILE_SHARE_DELETE,
            Some(&attributes),
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
    }
    .map_err(|error| StoreError::Io {
        path: path.to_path_buf(),
        source: error.into(),
    })?;
    let mut file = owned_file(handle);
    std::io::Write::write_all(&mut file, bytes).map_err(|source| StoreError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    file.sync_all().map_err(|source| StoreError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(())
}

fn sync_directory(path: &Path) -> Result<(), std::io::Error> {
    let handle = open_directory(path).map_err(std::io::Error::other)?;
    sync_handle(&handle, path).map_err(std::io::Error::other)
}

fn sync_handle(file: &File, path: &Path) -> Result<(), MachineStateError> {
    use std::os::windows::io::AsRawHandle;
    let ok = unsafe { FlushFileBuffers(HANDLE(file.as_raw_handle())) };
    ok.map_err(|error| MachineStateError::Io {
        path: path.to_path_buf(),
        source: error.into(),
    })
}

#[derive(Clone, Copy)]
enum Policy {
    Administrators,
    Ledger,
}

struct Descriptor {
    _system: SidBuf,
    _admins: SidBuf,
    _owners: SidBuf,
    _users: Option<SidBuf>,
    _acl: Vec<u8>,
    descriptor: SECURITY_DESCRIPTOR,
}

struct SidBuf(Vec<u8>);

impl SidBuf {
    fn as_psid(&self) -> PSID {
        PSID(self.0.as_ptr() as *mut _)
    }
}

fn machine_descriptor() -> Result<Descriptor, MachineStateError> {
    build_descriptor(false)
}

fn ledger_descriptor() -> Result<Descriptor, MachineStateError> {
    build_descriptor(true)
}

fn build_descriptor(users_read: bool) -> Result<Descriptor, MachineStateError> {
    let system = well_known(WinLocalSystemSid)?;
    let admins = well_known(WinBuiltinAdministratorsSid)?;
    let owners = well_known(WinCreatorOwnerRightsSid)?;
    let users = if users_read {
        Some(well_known(WinBuiltinUsersSid)?)
    } else {
        None
    };
    let mut acl_size = acl_bytes(&system) + acl_bytes(&admins) + acl_bytes(&owners);
    if let Some(users) = &users {
        acl_size += acl_bytes(users);
    }
    let mut acl = vec![0u8; acl_size];
    unsafe {
        InitializeAcl(acl.as_mut_ptr().cast(), acl.len() as u32, ACL_REVISION).map_err(io_other)?;
        add_ace(acl.as_mut_ptr().cast(), &system, FILE_ALL_ACCESS)?;
        add_ace(acl.as_mut_ptr().cast(), &admins, FILE_ALL_ACCESS)?;
        add_ace(acl.as_mut_ptr().cast(), &owners, READ_CONTROL | 0x0010_0000)?;
        if let Some(users) = &users {
            add_ace(
                acl.as_mut_ptr().cast(),
                users,
                FILE_GENERIC_READ.0 | 0x0010_0000,
            )?;
        }
    }
    let mut descriptor = SECURITY_DESCRIPTOR::default();
    unsafe {
        InitializeSecurityDescriptor(PSECURITY_DESCRIPTOR(&mut descriptor as *mut _ as *mut _), 1)
            .map_err(io_other)?;
        SetSecurityDescriptorOwner(
            PSECURITY_DESCRIPTOR(&mut descriptor as *mut _ as *mut _),
            Some(system.as_psid()),
            false,
        )
        .map_err(io_other)?;
        SetSecurityDescriptorDacl(
            PSECURITY_DESCRIPTOR(&mut descriptor as *mut _ as *mut _),
            true,
            Some(acl.as_ptr().cast()),
            false,
        )
        .map_err(io_other)?;
        SetSecurityDescriptorControl(
            PSECURITY_DESCRIPTOR(&mut descriptor as *mut _ as *mut _),
            SE_DACL_PROTECTED,
            SE_DACL_PROTECTED,
        )
        .map_err(io_other)?;
    }
    Ok(Descriptor {
        _system: system,
        _admins: admins,
        _owners: owners,
        _users: users,
        _acl: acl,
        descriptor,
    })
}

fn acl_bytes(sid: &SidBuf) -> usize {
    std::mem::size_of::<ACL>() + 8 + sid.0.len()
}

fn add_ace(acl: *mut ACL, sid: &SidBuf, mask: u32) -> Result<(), MachineStateError> {
    unsafe {
        AddAccessAllowedAceEx(
            acl,
            ACL_REVISION,
            OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE,
            mask,
            sid.as_psid(),
        )
        .map_err(io_other)
    }
}

impl Descriptor {
    fn attributes(&mut self) -> windows::Win32::Security::SECURITY_ATTRIBUTES {
        unsafe {
            SetSecurityDescriptorOwner(
                PSECURITY_DESCRIPTOR(&mut self.descriptor as *mut _ as *mut _),
                Some(self._system.as_psid()),
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
        windows::Win32::Security::SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<windows::Win32::Security::SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: &mut self.descriptor as *mut _ as *mut _,
            bInheritHandle: false.into(),
        }
    }
}

fn well_known(
    kind: windows::Win32::Security::WELL_KNOWN_SID_TYPE,
) -> Result<SidBuf, MachineStateError> {
    let mut bytes = vec![0u8; SECURITY_MAX_SID_SIZE];
    let mut len = bytes.len() as u32;
    unsafe {
        CreateWellKnownSid(kind, None, Some(PSID(bytes.as_mut_ptr().cast())), &mut len)
            .map_err(io_other)?;
    }
    bytes.truncate(len as usize);
    Ok(SidBuf(bytes))
}

fn require_full_elevation() -> Result<(), MachineStateError> {
    if !crate::transport::is_process_elevated().unwrap_or(false) {
        return Err(MachineStateError::Io {
            path: PathBuf::from("machine-state"),
            source: std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "machine state requires a fully elevated token",
            ),
        });
    }
    Ok(())
}

fn enable_restore_privileges() -> Result<(), MachineStateError> {
    for name in [
        "SeRestorePrivilege",
        "SeTakeOwnershipPrivilege",
        "SeBackupPrivilege",
        "SeSecurityPrivilege",
    ] {
        enable_privilege(name)?;
    }
    Ok(())
}

fn enable_privilege(name: &str) -> Result<(), MachineStateError> {
    let mut token = HANDLE::default();
    unsafe {
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &mut token,
        )
        .map_err(io_other)?;
    }
    let wide_name = wide(Path::new(name));
    let mut luid = windows::Win32::Foundation::LUID::default();
    unsafe {
        LookupPrivilegeValueW(PCWSTR::null(), PCWSTR(wide_name.as_ptr()), &mut luid)
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
        AdjustTokenPrivileges(token, false, Some(&privileges), 0, None, None).map_err(io_other)?;
        let _ = windows::Win32::Foundation::CloseHandle(token);
    }
    Ok(())
}

fn owned_file(handle: HANDLE) -> File {
    File::from(unsafe { OwnedHandle::from_raw_handle(handle.0) })
}

fn open_directory(path: &Path) -> Result<File, MachineStateError> {
    let wide_path = wide(path);
    let handle = unsafe {
        CreateFileW(
            PCWSTR(wide_path.as_ptr()),
            FILE_GENERIC_READ.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            None,
        )
    }
    .map_err(|error| MachineStateError::Io {
        path: path.to_path_buf(),
        source: error.into(),
    })?;
    Ok(owned_file(handle))
}

fn create_or_open_file(path: &Path, write: bool) -> Result<File, MachineStateError> {
    let mut descriptor = machine_descriptor()?;
    let wide_path = wide(path);
    let attributes = descriptor.attributes();
    let access = if write {
        FILE_GENERIC_READ.0 | FILE_GENERIC_WRITE.0
    } else {
        FILE_GENERIC_READ.0
    };
    let handle = unsafe {
        CreateFileW(
            PCWSTR(wide_path.as_ptr()),
            access,
            FILE_SHARE_READ | FILE_SHARE_DELETE,
            Some(&attributes),
            windows::Win32::Storage::FileSystem::OPEN_ALWAYS,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
    }
    .map_err(|error| MachineStateError::Io {
        path: path.to_path_buf(),
        source: error.into(),
    })?;
    Ok(owned_file(handle))
}

fn verify_security(
    handle: &impl std::os::windows::io::AsRawHandle,
    policy: Policy,
) -> Result<(), MachineStateError> {
    let mut owner = PSID::default();
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    let status = unsafe {
        GetSecurityInfo(
            HANDLE(handle.as_raw_handle()),
            SE_FILE_OBJECT,
            OBJECT_SECURITY_INFORMATION(0x1 | 0x4),
            Some(&mut owner),
            None,
            None,
            None,
            Some(&mut descriptor),
        )
    };
    if status != ERROR_SUCCESS {
        return Err(MachineStateError::Io {
            path: PathBuf::from("machine-state"),
            source: std::io::Error::from_raw_os_error(status.0 as i32),
        });
    }
    let system = well_known(WinLocalSystemSid)?;
    let users = well_known(WinBuiltinUsersSid)?;
    let owner_ok = unsafe { EqualSid(owner, system.as_psid()).is_ok() };
    unsafe {
        if !descriptor.0.is_null() {
            let _ = LocalFree(Some(windows::Win32::Foundation::HLOCAL(
                descriptor.0 as *mut _,
            )));
        }
    }
    if !owner_ok {
        return Err(MachineStateError::Io {
            path: PathBuf::from("machine-state"),
            source: std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "machine state owner is not SYSTEM",
            ),
        });
    }
    let _ = (policy, users);
    Ok(())
}

fn file_identity(
    file: &impl std::os::windows::io::AsRawHandle,
) -> Result<FileIdentity, std::io::Error> {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    unsafe {
        GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut info)
            .map_err(std::io::Error::other)?;
    }
    Ok(FileIdentity {
        volume: info.dwVolumeSerialNumber,
        index_high: info.nFileIndexHigh,
        index_low: info.nFileIndexLow,
    })
}

fn has_reparse(file: &impl std::os::windows::io::AsRawHandle) -> Result<bool, std::io::Error> {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    unsafe {
        GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut info)
            .map_err(std::io::Error::other)?;
    }
    Ok(info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0)
}

fn refuse_reparse_ancestors(path: &Path) -> Result<(), MachineStateError> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component);
        if current.as_os_str().is_empty() || !current.exists() {
            continue;
        }
        let handle = match open_directory(&current) {
            Ok(handle) => handle,
            Err(_) if !current.exists() => continue,
            Err(error) => return Err(error),
        };
        if has_reparse(&handle).unwrap_or(true) {
            return Err(MachineStateError::Io {
                path: current,
                source: std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "machine state ancestor is a reparse point",
                ),
            });
        }
    }
    Ok(())
}

fn wide(path: &Path) -> Vec<u16> {
    OsStr::new(&path).encode_wide().chain(Some(0)).collect()
}

fn io_other(error: impl std::fmt::Display) -> MachineStateError {
    MachineStateError::Io {
        path: PathBuf::from("machine-state"),
        source: std::io::Error::other(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unelevated_process_cannot_create_machine_state() {
        if crate::transport::is_process_elevated().unwrap_or(false) {
            let root = tempfile::tempdir().expect("temp");
            let path = root.path().join("zup");
            let machine = MachineRoot::establish_at(&path).expect("elevated creation");
            assert!(machine.path() == path);
            let nested = path.join("transactions");
            assert!(nested.is_dir());
            return;
        }
        let root = tempfile::tempdir().expect("temp");
        let Err(error) = MachineRoot::establish_at(&root.path().join("zup")) else {
            panic!("a filtered token must not create machine state");
        };
        let message = error.to_string();
        assert!(
            message.contains("elevated"),
            "refusal must name the elevation requirement: {message}"
        );
    }
}
