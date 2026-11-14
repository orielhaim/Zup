use super::*;
use std::path::PathBuf;
use zup_core::{NativeObjectIdentity, Sha256Digest};

pub(crate) enum FileAuthority {
    User(crate::transport::UserSid),
    Machine,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum DirectoryAccess {
    CreateDirectories,
    Read,
    CreateFiles,
    PrepareFiles,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EntryAuthority {
    Ancestor,
    OwnedResource,
    DirectoryPublicationParent,
}

pub(crate) enum PayloadAccess {
    Observe,
    Mutate,
}

pub(crate) struct DirectoryAncestry {
    path: PathBuf,
    handles: Vec<OwnedHandle>,
}

impl DirectoryAncestry {
    pub fn open(path: &Path) -> Result<Self, FilesystemError> {
        let (drive, names) = split_absolute(path)?;
        let mut handles = vec![open_drive(drive, Open::Traverse)?];
        for name in names {
            let handle =
                open_relative(handles.last().unwrap(), &name, Open::Directory, None)?.handle;
            reject_reparse(&handle)?;
            handles.push(handle);
        }
        Ok(Self {
            path: path.to_owned(),
            handles,
        })
    }

    pub fn reject_ancestor(&self, excluded: (u64, [u8; 16])) -> Result<(), FilesystemError> {
        for handle in &self.handles {
            if identity(handle)? == excluded {
                return Err(io_path(
                    &self.path,
                    "application resources cannot occupy deployment engine state",
                ));
            }
        }
        Ok(())
    }
}

pub(crate) struct FileParent {
    path: PathBuf,
    directory: OwnedHandle,
    ancestors: Vec<(PathBuf, OwnedHandle)>,
    user: Option<SidBuf>,
    machine: [SidBuf; 3],
    access: DirectoryAccess,
    volume: Option<OwnedHandle>,
}

pub(crate) struct ExclusiveFile(File);
impl ExclusiveFile {
    pub fn native_image(&self) -> Result<bool, FilesystemError> {
        use std::io::{Read, Seek, SeekFrom};
        let mut file = self.0.try_clone().map_err(io_std)?;
        file.seek(SeekFrom::Start(0)).map_err(io_std)?;
        let mut dos = [0u8; 64];
        if file.read(&mut dos).map_err(io_std)? != dos.len() || &dos[..2] != b"MZ" {
            return Ok(false);
        }
        let offset = u32::from_le_bytes(dos[60..64].try_into().unwrap()) as u64;
        if offset > file.metadata().map_err(io_std)?.len().saturating_sub(24) {
            return Ok(false);
        }
        file.seek(SeekFrom::Start(offset)).map_err(io_std)?;
        let mut pe = [0u8; 4];
        file.read_exact(&mut pe).map_err(io_std)?;
        Ok(&pe == b"PE\0\0")
    }
}

impl AsHandle for ExclusiveFile {
    fn as_handle(&self) -> std::os::windows::io::BorrowedHandle<'_> {
        self.0.as_handle()
    }
}

pub(crate) struct DataDirectory {
    handle: OwnedHandle,
    kind: PolicyKind,
    writers: Vec<zup_plan::ServiceDataWriter>,
    volume: Option<OwnedHandle>,
}
impl DataDirectory {
    pub fn identity(&self) -> Result<NativeObjectIdentity, FilesystemError> {
        reject_reparse(&self.handle)?;
        if verify_handle(&self.handle, self.kind).is_err() {
            self.verify_access(&self.writers)?;
        }
        let (volume, file) = identity(&self.handle)?;
        Ok(NativeObjectIdentity::Windows { volume, file })
    }
}

impl DataDirectory {
    fn access_policy(
        &self,
        writers: &[zup_plan::ServiceDataWriter],
    ) -> Result<SecurityPolicy, FilesystemError> {
        let mut policy = SecurityPolicy::for_kind(self.kind, true)?;
        if !writers.is_empty() && self.kind != PolicyKind::Administrators {
            return Err(io_path(
                Path::new("persistent data"),
                "service data requires machine ownership",
            ));
        }
        for writer in writers {
            let zup_plan::ServiceDataPrincipal::WindowsService { name } = &writer.principal else {
                return Err(io_path(
                    Path::new("persistent data"),
                    "writer is not a Windows service",
                ));
            };
            let sid = service_sid(name)?;
            policy.grants.push(Grant {
                sid: sid.clone(),
                mask: 0x0012_01ff,
                flags: 0,
            });
            policy.grants.push(Grant {
                sid,
                mask: 0x0013_01ff,
                flags: INHERIT | 8,
            });
        }
        Ok(policy)
    }
    pub fn verify_access(
        &self,
        writers: &[zup_plan::ServiceDataWriter],
    ) -> Result<(), FilesystemError> {
        reject_reparse(&self.handle)?;
        verify_policy(&self.handle, &self.access_policy(writers)?)
    }
    pub fn provision_access(
        &self,
        writers: &[zup_plan::ServiceDataWriter],
    ) -> Result<(), FilesystemError> {
        if self.verify_access(writers).is_ok() {
            return Ok(());
        }
        verify_handle(&self.handle, self.kind)?;
        if !list_entries(&self.handle)?.is_empty() {
            return Err(io_path(
                Path::new("persistent data"),
                "initial service grants require an empty private root",
            ));
        }
        let policy = self.access_policy(writers)?;
        let descriptor = descriptor_for(&policy)?;
        let status = unsafe {
            windows::Win32::Security::Authorization::SetSecurityInfo(
                HANDLE(self.handle.as_raw_handle()),
                SE_FILE_OBJECT,
                OBJECT_SECURITY_INFORMATION(0x8000_0004),
                None,
                None,
                Some(descriptor._acl.as_ptr().cast()),
                None,
            )
        };
        if status.0 != 0 {
            return Err(io_std(std::io::Error::from_raw_os_error(status.0 as i32)));
        }
        if let Some(volume) = &self.volume {
            flush_handle(volume)?;
        }
        self.verify_access(writers)
    }
}
fn service_sid(name: &str) -> Result<SidBuf, FilesystemError> {
    use windows::Win32::Security::{LookupAccountNameW, SID_NAME_USE};
    let account: Vec<u16> = format!("NT SERVICE\\{name}")
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let mut sid = vec![0u32; 17];
    let mut sid_size = (sid.len() * 4) as u32;
    let mut domain = vec![0u16; 256];
    let mut domain_size = domain.len() as u32;
    let mut kind = SID_NAME_USE::default();
    unsafe {
        LookupAccountNameW(
            PCWSTR::null(),
            PCWSTR(account.as_ptr()),
            Some(PSID(sid.as_mut_ptr().cast())),
            &mut sid_size,
            Some(windows::core::PWSTR(domain.as_mut_ptr())),
            &mut domain_size,
            &mut kind,
        )
    }
    .map_err(io_other)?;
    if sid_size as usize > sid.len() * 4 || sid_size < 8 || !sid_size.is_multiple_of(4) {
        return Err(io_path(
            Path::new("persistent data"),
            "service SID lookup returned an invalid identity",
        ));
    }
    sid.truncate(sid_size as usize / 4);
    Ok(SidBuf(sid))
}

const DATA_CREATION_EA: &[u8] = b"ZUP.PERSISTENT.ORIGIN";
fn data_creation_ea(token: &zup_transaction::PersistentCreationToken) -> Vec<u8> {
    let mut bytes = vec![0; 8];
    bytes[5] = DATA_CREATION_EA.len() as u8;
    bytes[6..8].copy_from_slice(&16u16.to_le_bytes());
    bytes.extend_from_slice(DATA_CREATION_EA);
    bytes.push(0);
    bytes.extend_from_slice(token.bytes());
    bytes
}
impl DataDirectory {
    pub fn authenticates(
        &self,
        token: &zup_transaction::PersistentCreationToken,
    ) -> Result<bool, FilesystemError> {
        self.identity()?;
        let mut query = vec![0u8; 5];
        query[4] = DATA_CREATION_EA.len() as u8;
        query.extend_from_slice(DATA_CREATION_EA);
        query.push(0);
        let mut buffer = [0u8; 128];
        let mut status_block = IoStatus {
            status: 0,
            information: 0,
        };
        let status = unsafe {
            NtQueryEaFile(
                HANDLE(self.handle.as_raw_handle()),
                &mut status_block,
                buffer.as_mut_ptr().cast(),
                buffer.len() as u32,
                1,
                query.as_ptr().cast(),
                query.len() as u32,
                std::ptr::null(),
                1,
            )
        };
        if status == 0xc000_0052u32 as i32 || status == 0xc000_0051u32 as i32 {
            return Ok(false);
        }
        if status < 0 {
            return Err(io_std(std::io::Error::from_raw_os_error(unsafe {
                RtlNtStatusToDosError(status)
            }
                as i32)));
        }
        let expected = data_creation_ea(token);
        Ok(status_block.information == expected.len() && buffer[..expected.len()] == expected)
    }
}

impl FileParent {
    pub fn verify_ancestry(
        &self,
        expected: &[NativeObjectIdentity],
    ) -> Result<(), FilesystemError> {
        let mut remaining = expected.iter();
        let Some(root) = remaining.next() else {
            return Err(io_path(&self.path, "native ancestry is empty"));
        };
        let mut found = false;
        for handle in self
            .ancestors
            .iter()
            .map(|(_, handle)| handle)
            .chain(std::iter::once(&self.directory))
        {
            let (volume, file) = identity(handle)?;
            let object = NativeObjectIdentity::Windows { volume, file };
            if !found {
                if object == *root {
                    found = true;
                }
            } else if remaining.next() != Some(&object) {
                return Err(io_path(&self.path, "generation directory ancestry changed"));
            }
        }
        if !found || remaining.next().is_some() {
            return Err(io_path(
                &self.path,
                "generation directory ancestry is incomplete",
            ));
        }
        Ok(())
    }

    pub fn object_identity(&self) -> Result<(u64, [u8; 16]), FilesystemError> {
        identity(&self.directory)
    }

    pub fn open_data_child(
        &self,
        name: &str,
        writers: &[zup_plan::ServiceDataWriter],
    ) -> Result<DataDirectory, FilesystemError> {
        self.validate()?;
        let kind = if self.user.is_some() {
            PolicyKind::UserData
        } else {
            PolicyKind::Administrators
        };
        validate_name(OsStr::new(name))?;
        let opened = open_relative(&self.directory, OsStr::new(name), Open::DataDirectory, None)?;
        reject_reparse(&opened.handle)?;
        let directory = DataDirectory {
            handle: opened.handle,
            kind,
            writers: writers.to_vec(),
            volume: self
                .volume
                .as_ref()
                .map(OwnedHandle::try_clone)
                .transpose()
                .map_err(io_std)?,
        };
        directory.identity()?;
        Ok(directory)
    }

    pub fn create_authenticated_data_child(
        &self,
        name: &str,
        token: &zup_transaction::PersistentCreationToken,
    ) -> Result<DataDirectory, FilesystemError> {
        self.validate()?;
        validate_name(OsStr::new(name))?;
        let kind = if self.user.is_some() {
            PolicyKind::UserData
        } else {
            PolicyKind::Administrators
        };
        let ea = data_creation_ea(token);
        let opened = nt_open_with_ea(
            Some(&self.directory),
            &OsStr::new(name).encode_wide().collect::<Vec<_>>(),
            Open::CreateDataDirectory,
            Some(kind),
            &ea,
        )?;
        let directory = DataDirectory {
            handle: opened.handle,
            kind,
            writers: Vec::new(),
            volume: self
                .volume
                .as_ref()
                .map(OwnedHandle::try_clone)
                .transpose()
                .map_err(io_std)?,
        };
        directory.identity()?;
        if !directory.authenticates(token)? {
            return Err(io_path(
                &self.path,
                "native data creation proof differs on readback",
            ));
        }
        if let Some(volume) = &self.volume {
            flush_handle(volume)?;
        }
        Ok(directory)
    }

    fn validate(&self) -> Result<(), FilesystemError> {
        let publication = self.access == DirectoryAccess::CreateDirectories;
        for (path, ancestor) in &self.ancestors {
            verify_authority(
                ancestor,
                self.user.as_ref(),
                &self.machine,
                if publication {
                    EntryAuthority::DirectoryPublicationParent
                } else {
                    EntryAuthority::Ancestor
                },
                path,
            )?;
        }
        verify_authority(
            &self.directory,
            self.user.as_ref(),
            &self.machine,
            if publication {
                EntryAuthority::DirectoryPublicationParent
            } else {
                EntryAuthority::OwnedResource
            },
            &self.path,
        )
    }

    pub fn open(
        path: &Path,
        authority: FileAuthority,
        access: DirectoryAccess,
    ) -> Result<Self, FilesystemError> {
        let user = match authority {
            FileAuthority::User(sid) => {
                let current = crate::transport::UserSid::current()
                    .map_err(|error| io_path(path, &error.to_string()))?;
                let elevated = crate::transport::is_process_elevated()
                    .map_err(|error| io_path(path, &error.to_string()))?;
                if current != sid || elevated {
                    return Err(io_path(
                        path,
                        "user deployment requires the originating user's unelevated token; run user maintenance without elevation",
                    ));
                }
                Some(SidBuf::parse(sid.display())?)
            }
            FileAuthority::Machine => None,
        };
        let machine = [
            well_known(WinLocalSystemSid)?,
            well_known(WinBuiltinAdministratorsSid)?,
            SidBuf::parse(TRUSTED_INSTALLER_SID)?,
        ];
        let (drive, names) = split_absolute(path)?;
        let mut ancestors = Vec::with_capacity(names.len());
        let final_open = match access {
            DirectoryAccess::CreateDirectories => Open::DirectoryPublicationParent,
            DirectoryAccess::Read => Open::Directory,
            DirectoryAccess::CreateFiles => Open::RenameTarget,
            DirectoryAccess::PrepareFiles => Open::PrepareDirectory,
        };
        let mut current = open_drive(
            drive,
            if names.is_empty() {
                final_open
            } else {
                if access == DirectoryAccess::CreateDirectories {
                    Open::DirectoryLease
                } else {
                    Open::Directory
                }
            },
        )?;
        let mut observed_path = PathBuf::from(format!("{}:\\", char::from(drive)));
        verify_authority(
            &current,
            user.as_ref(),
            &machine,
            if access == DirectoryAccess::CreateDirectories {
                EntryAuthority::DirectoryPublicationParent
            } else if names.is_empty() {
                EntryAuthority::OwnedResource
            } else {
                EntryAuthority::Ancestor
            },
            &observed_path,
        )?;
        for (index, name) in names.iter().enumerate() {
            let child = open_relative(
                &current,
                name,
                if index + 1 == names.len() {
                    final_open
                } else if access == DirectoryAccess::PrepareFiles {
                    Open::CreateDirectory
                } else {
                    if access == DirectoryAccess::CreateDirectories {
                        Open::DirectoryLease
                    } else {
                        Open::Directory
                    }
                },
                (access == DirectoryAccess::PrepareFiles && user.is_none())
                    .then_some(PolicyKind::Payload),
            )?
            .handle;
            reject_reparse(&child)?;
            let child_path = observed_path.join(name);
            verify_authority(
                &child,
                user.as_ref(),
                &machine,
                if access == DirectoryAccess::CreateDirectories {
                    EntryAuthority::DirectoryPublicationParent
                } else if index + 1 == names.len() {
                    EntryAuthority::OwnedResource
                } else {
                    EntryAuthority::Ancestor
                },
                &child_path,
            )?;
            ancestors.push((observed_path, current));
            current = child;
            observed_path = child_path;
        }
        let volume = if user.is_none() && access != DirectoryAccess::Read {
            Some(open_volume(&current)?)
        } else {
            None
        };
        Ok(Self {
            path: path.to_owned(),
            directory: current,
            ancestors,
            user,
            machine,
            access,
            volume,
        })
    }

    pub fn ntfs(&self) -> Result<bool, FilesystemError> {
        let mut name = [0u16; 32];
        unsafe {
            windows::Win32::Storage::FileSystem::GetVolumeInformationByHandleW(
                HANDLE(self.directory.as_raw_handle()),
                None,
                None,
                None,
                None,
                Some(&mut name),
            )
        }
        .map_err(io_other)?;
        Ok(String::from_utf16_lossy(
            &name[..name
                .iter()
                .position(|value| *value == 0)
                .unwrap_or(name.len())],
        ) == "NTFS")
    }

    pub fn remove_empty_directory(
        &self,
        name: &OsStr,
        expected: NativeObjectIdentity,
    ) -> Result<(), FilesystemError> {
        self.validate()?;
        let directory = open_relative(&self.directory, name, Open::MovableDirectory, None)?.handle;
        reject_reparse(&directory)?;
        let (volume, file) = identity(&directory)?;
        if expected != (NativeObjectIdentity::Windows { volume, file })
            || !list_entries(&directory)?.is_empty()
        {
            return Err(io_path(
                &self.path.join(name),
                "collection directory identity or contents changed",
            ));
        }
        verify_authority(
            &directory,
            self.user.as_ref(),
            &self.machine,
            EntryAuthority::OwnedResource,
            &self.path.join(name),
        )?;
        discard(&directory)?;
        drop(directory);
        if let Some(volume) = &self.volume {
            flush_handle(volume)?;
        }
        Ok(())
    }

    pub fn file_absent(&self, name: &OsStr) -> Result<bool, FilesystemError> {
        self.validate()?;
        match open_relative(&self.directory, name, Open::ObserveFile, None) {
            Ok(opened) => {
                reject_reparse(&opened.handle)?;
                Ok(false)
            }
            Err(FilesystemError::Io { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                Ok(true)
            }
            Err(error) => Err(error),
        }
    }

    pub fn discard_fenced_file(&self, file: ExclusiveFile) -> Result<(), FilesystemError> {
        self.validate()?;
        verify_authority(
            &file,
            self.user.as_ref(),
            &self.machine,
            EntryAuthority::OwnedResource,
            &self.path,
        )?;
        discard(&file)?;
        drop(file);
        if let Some(volume) = &self.volume {
            flush_handle(volume)?;
        }
        Ok(())
    }

    pub fn open_owned_file(
        &self,
        name: &OsStr,
        object: NativeObjectIdentity,
        size: u64,
        sha256: Sha256Digest,
        access: PayloadAccess,
    ) -> Result<File, FilesystemError> {
        let mode = match access {
            PayloadAccess::Observe => Open::ObserveFile,
            PayloadAccess::Mutate => Open::MutateFile,
        };
        self.open_expected_file(name, object, size, sha256, mode)
    }

    pub fn fence_owned_file(
        &self,
        name: &OsStr,
        object: NativeObjectIdentity,
        size: u64,
        sha256: Sha256Digest,
    ) -> Result<ExclusiveFile, FilesystemError> {
        self.open_expected_file(name, object, size, sha256, Open::FenceFile)
            .map(ExclusiveFile)
    }

    fn open_expected_file(
        &self,
        name: &OsStr,
        object: NativeObjectIdentity,
        size: u64,
        sha256: Sha256Digest,
        mode: Open,
    ) -> Result<File, FilesystemError> {
        self.validate()?;
        let handle = open_relative(&self.directory, name, mode, None)?.handle;
        reject_reparse(&handle)?;
        let path = self.path.join(name);
        verify_authority(
            &handle,
            self.user.as_ref(),
            &self.machine,
            EntryAuthority::OwnedResource,
            &path,
        )?;
        let mut file = File::from(handle);
        let observed = crate::inspect::inspect_open_file(&path, &mut file)
            .map_err(|error| io_path(&path, &error.to_string()))?;
        if !matches!(observed, zup_plan::ObservedFileState::File {
            object: actual, size: actual_size, sha256: actual_digest, ..
        } if actual == object && actual_size == size && actual_digest == sha256)
        {
            return Err(io_path(&path, "file is not the recorded owned instance"));
        }
        Ok(file)
    }

    pub fn stage_payload(
        &self,
        name: &OsStr,
        source: &mut dyn std::io::Read,
        size: u64,
        sha256: Sha256Digest,
    ) -> Result<NativeObjectIdentity, FilesystemError> {
        use std::io::{Read, Seek, SeekFrom, Write};
        self.validate()?;
        if !matches!(
            self.access,
            DirectoryAccess::CreateFiles | DirectoryAccess::PrepareFiles
        ) {
            return Err(io_path(
                &self.path,
                "directory capability does not authorize staging",
            ));
        }
        let opened = open_relative(
            &self.directory,
            name,
            Open::Stage,
            self.user.is_none().then_some(PolicyKind::Payload),
        )?;
        let mut file = File::from(opened.handle);
        let path = self.path.join(name);
        let result = (|| {
            verify_authority(
                &file,
                self.user.as_ref(),
                &self.machine,
                EntryAuthority::OwnedResource,
                &path,
            )?;
            std::io::copy(&mut source.take(size.saturating_add(1)), &mut file).map_err(io_std)?;
            file.flush().map_err(io_std)?;
            file.sync_all().map_err(io_std)?;
            if let Some(volume) = &self.volume {
                flush_handle(volume)?;
            }
            file.seek(SeekFrom::Start(0)).map_err(io_std)?;
            let observed = crate::inspect::inspect_open_file(&path, &mut file)
                .map_err(|error| io_path(&path, &error.to_string()))?;
            match observed {
                zup_plan::ObservedFileState::File {
                    object,
                    size: actual_size,
                    sha256: actual_hash,
                    ..
                } if size == actual_size && sha256 == actual_hash => Ok(object),
                _ => Err(io_path(
                    &path,
                    "staged payload differs from authenticated content",
                )),
            }
        })();
        match result {
            Ok(object) => Ok(object),
            Err(error) => match discard(&file) {
                Ok(()) => Err(error),
                Err(cleanup) => Err(io_path(
                    &path,
                    &format!("{error}; staged instance cleanup failed: {cleanup}"),
                )),
            },
        }
    }

    pub fn move_fenced_file(
        &self,
        file: &ExclusiveFile,
        name: &str,
    ) -> Result<(), FilesystemError> {
        self.validate()?;
        validate_name(OsStr::new(name))?;
        if !matches!(
            self.access,
            DirectoryAccess::CreateFiles | DirectoryAccess::PrepareFiles
        ) {
            return Err(io_path(
                &self.path,
                "file publication requires a creation capability",
            ));
        }
        verify_authority(
            file,
            self.user.as_ref(),
            &self.machine,
            EntryAuthority::OwnedResource,
            &self.path.join(name),
        )?;
        if identity(file)?.0 != identity(&self.directory)?.0 {
            return Err(io_path(&self.path, "file publication crosses volumes"));
        }
        rename_handle(file, &self.directory, name, false)?;
        if let Some(volume) = &self.volume {
            flush_handle(volume)?;
        }
        Ok(())
    }

    pub fn move_owned_file(
        &self,
        name: &OsStr,
        object: NativeObjectIdentity,
        size: u64,
        sha256: Sha256Digest,
        target: &Self,
        target_name: &OsStr,
    ) -> Result<(), FilesystemError> {
        validate_name(target_name)?;
        let file = self.open_owned_file(
            name,
            object,
            size,
            sha256,
            crate::filesystem::authority::PayloadAccess::Mutate,
        )?;
        for (path, ancestor) in &target.ancestors {
            verify_authority(
                ancestor,
                target.user.as_ref(),
                &target.machine,
                EntryAuthority::Ancestor,
                path,
            )?;
        }
        verify_authority(
            &target.directory,
            target.user.as_ref(),
            &target.machine,
            EntryAuthority::OwnedResource,
            &target.path,
        )?;
        if !matches!(
            target.access,
            DirectoryAccess::CreateFiles | DirectoryAccess::PrepareFiles
        ) {
            return Err(io_path(
                &target.path,
                "directory capability does not authorize file creation",
            ));
        }
        let destination_identity = identity(&target.directory)?;
        if identity(&file)?.0 != destination_identity.0
            || match (&self.user, &target.user) {
                (None, None) => false,
                (Some(source), Some(target)) => unsafe {
                    EqualSid(source.as_psid(), target.as_psid()).is_err()
                },
                _ => true,
            }
        {
            return Err(io_path(
                &target.path,
                "file move crosses filesystem or deployment authority",
            ));
        }
        let target_name = target_name
            .to_str()
            .ok_or_else(|| io_path(&target.path, "file name is not Unicode"))?;
        rename_handle(&file, &target.directory, target_name, false)?;
        if let Some(volume) = &target.volume {
            flush_handle(volume).map_err(|error| PublishError::Indeterminate {
                may_have_published: true,
                detail: format!("file moved but namespace flush failed: {error}"),
            })?;
        }
        let observed = open_relative(&target.directory, OsStr::new(target_name), Open::Read, None)
            .and_then(|opened| identity(&opened.handle));
        match observed {
            Ok(found)
                if Some(found)
                    == match object {
                        NativeObjectIdentity::Windows { volume, file } => Some((volume, file)),
                        _ => None,
                    } =>
            {
                Ok(())
            }
            _ => Err(PublishError::Indeterminate {
                may_have_published: true,
                detail: format!(
                    "moved file identity cannot be confirmed at {}",
                    target.path.join(target_name).display()
                ),
            }
            .into()),
        }
    }
}

fn verify_authority(
    handle: &impl AsHandle,
    user: Option<&SidBuf>,
    machine: &[SidBuf; 3],
    policy: EntryAuthority,
    path: &Path,
) -> Result<(), FilesystemError> {
    let trusted = |sid| {
        machine
            .iter()
            .chain(user)
            .any(|principal| unsafe { EqualSid(sid, principal.as_psid()).is_ok() })
    };
    let owner_rights = well_known(WinCreatorOwnerRightsSid)?;
    with_security(handle, |owner, dacl, _| {
        let owner_valid = if policy == EntryAuthority::OwnedResource {
            user.map_or_else(
                || trusted(owner),
                |user| unsafe { EqualSid(owner, user.as_psid()).is_ok() },
            )
        } else {
            trusted(owner)
        };
        if !owner_valid || dacl.is_null() {
            return Err(io_path(
                path,
                "filesystem owner does not match deployment authority",
            ));
        }
        for index in 0..unsafe { (*dacl).AceCount } {
            let mut ace = std::ptr::null_mut();
            unsafe {
                GetAce(dacl, index as u32, &mut ace).map_err(io_other)?;
            }
            let header = unsafe { &*ace.cast::<ACE_HEADER>() };
            if header.AceFlags & 0x08 != 0 || header.AceType == 1 {
                continue;
            }
            if header.AceType != ACCESS_ALLOWED_ACE_TYPE {
                return Err(io_path(path, "unsupported filesystem access-control entry"));
            }
            let grant = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
            let sid = PSID(unsafe { ace.cast::<u8>().add(size_of::<ACE_HEADER>() + 4) }.cast());
            let mutation = DELETE
                | 0x40
                | 0x4_0000
                | 0x8_0000
                | 0x1000_0000
                | if policy != EntryAuthority::DirectoryPublicationParent {
                    0x10 | 0x100 | 0x4000_0000
                } else {
                    0
                }
                | if policy == EntryAuthority::OwnedResource {
                    0x2 | 0x4
                } else {
                    0
                };
            let current_owner = unsafe { EqualSid(sid, owner_rights.as_psid()).is_ok() };
            if grant.Mask & mutation != 0 && !trusted(sid) && !current_owner {
                return Err(io_path(
                    path,
                    "filesystem permissions allow mutation outside deployment authority",
                ));
            }
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[rstest::rstest]
    #[case(false)]
    #[case(true)]
    fn owner_rights_remain_bound_to_the_verified_owner(#[case] foreign_writer: bool) {
        use std::os::windows::fs::OpenOptionsExt;
        use windows::Win32::Security::{
            DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
        };
        let root = tempfile::tempdir_in(crate::host_dirs::user_data().unwrap()).unwrap();
        let file = std::fs::OpenOptions::new()
            .access_mode(0x0006_0000)
            .share_mode(7)
            .custom_flags(0x0220_0000)
            .open(root.path())
            .unwrap();
        let mut policy = SecurityPolicy::for_kind(PolicyKind::UserData, true).unwrap();
        let owner_rights = well_known(WinCreatorOwnerRightsSid).unwrap();
        for grant in &mut policy.grants {
            if unsafe { EqualSid(grant.sid.as_psid(), owner_rights.as_psid()).is_ok() } {
                grant.mask = 0x001f_01ff;
            }
        }
        if foreign_writer {
            policy.grants.push(Grant {
                flags: 0,
                sid: well_known(WinBuiltinUsersSid).unwrap(),
                mask: 2,
            });
        }
        let descriptor = descriptor_for(&policy).unwrap();
        unsafe {
            windows::Win32::Security::Authorization::SetSecurityInfo(
                HANDLE(file.as_raw_handle()),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                None,
                None,
                Some(descriptor._acl.as_ptr().cast()),
                None,
            )
        }
        .ok()
        .unwrap();
        drop(file);
        let result = FileParent::open(
            root.path(),
            FileAuthority::User(crate::transport::UserSid::current().unwrap()),
            DirectoryAccess::CreateFiles,
        );
        assert_eq!(result.is_ok(), !foreign_writer);
    }

    #[test]
    #[ignore = "requires elevated administrator"]
    fn service_data_grants_preserve_root_identity_and_refuse_permission_migration() {
        let shared = crate::host_dirs::shared_data().unwrap();
        let name = format!("ZupServiceDataProbe-{}", uuid::Uuid::new_v4().simple());
        let parent = FileParent::open(
            &shared,
            FileAuthority::Machine,
            DirectoryAccess::CreateDirectories,
        )
        .unwrap();
        let token: zup_transaction::PersistentCreationToken =
            serde_json::from_value(serde_json::json!(uuid::Uuid::new_v4())).unwrap();
        let created = parent
            .create_authenticated_data_child(&name, &token)
            .unwrap();
        let object = created.identity().unwrap();
        drop(created);
        let writers = vec![zup_plan::ServiceDataWriter {
            service: zup_core::ServiceId::new("worker").unwrap(),
            principal: zup_plan::ServiceDataPrincipal::WindowsService {
                name: "EventLog".into(),
            },
        }];
        let root = parent.open_data_child(&name, &writers).unwrap();
        root.provision_access(&writers).unwrap();
        assert_eq!(root.identity().unwrap(), object);
        assert!(root.verify_access(&[]).is_err());
        let database = shared.join(&name).join("database");
        std::fs::write(&database, b"persistent contents").unwrap();
        root.provision_access(&writers).unwrap();
        assert!(root.provision_access(&[]).is_err());
        assert_eq!(std::fs::read(&database).unwrap(), b"persistent contents");
        drop(root);
        drop(parent);
        std::fs::remove_dir_all(shared.join(&name)).unwrap();
    }

    #[test]
    #[ignore = "requires elevated administrator"]
    fn shared_data_creation_excludes_parent_writers_and_authenticates_identity() {
        use std::os::windows::fs::OpenOptionsExt;
        let shared = crate::host_dirs::shared_data().unwrap();
        let mut write_parent = std::fs::OpenOptions::new();
        write_parent
            .write(true)
            .share_mode(7)
            .custom_flags(0x02000000);
        drop(write_parent.open(&shared).unwrap());
        let parent = FileParent::open(
            &shared,
            FileAuthority::Machine,
            DirectoryAccess::CreateDirectories,
        )
        .unwrap();
        assert!(write_parent.open(&shared).is_err());
        let name = format!("ZupDataProbe-{}", uuid::Uuid::new_v4().simple());
        let token: zup_transaction::PersistentCreationToken =
            serde_json::from_value(serde_json::json!(uuid::Uuid::new_v4())).unwrap();
        let root = parent
            .create_authenticated_data_child(&name, &token)
            .unwrap();
        let expected = root.identity().unwrap();
        assert!(root.authenticates(&token).unwrap());
        let foreign: zup_transaction::PersistentCreationToken =
            serde_json::from_value(serde_json::json!(uuid::Uuid::new_v4())).unwrap();
        assert!(!root.authenticates(&foreign).unwrap());
        assert!(
            parent
                .create_authenticated_data_child(&name, &token)
                .is_err()
        );
        drop(root);
        let root = parent.open_data_child(&name, &[]).unwrap();
        assert_eq!(root.identity().unwrap(), expected);
        let path = shared.join(&name);
        std::fs::write(path.join("database"), b"mutable").unwrap();
        drop(root);
        drop(parent);
        drop(write_parent.open(&shared).unwrap());
        assert_eq!(path.parent(), Some(shared.as_path()));
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    #[ignore = "requires elevated administrator"]
    fn machine_staging_preserves_public_payload_policy_and_native_identity() {
        let root = tempfile::tempdir_in(std::env::var_os("ProgramFiles").unwrap()).unwrap();
        let payload = root.path().join("payload");
        let directory = MachineDir::payload_root(&payload, true).unwrap();
        let parent = FileParent::open(
            &payload,
            FileAuthority::Machine,
            DirectoryAccess::CreateFiles,
        )
        .unwrap();
        let bytes = b"public machine payload";
        let sha256 = zup_core::hash_bytes(bytes);
        let object = parent
            .stage_payload(
                OsStr::new("staged.bin"),
                &mut bytes.as_slice(),
                bytes.len() as u64,
                sha256,
            )
            .unwrap();
        let opened = open_relative(
            &parent.directory,
            OsStr::new("staged.bin"),
            Open::Read,
            None,
        )
        .unwrap();
        verify_file(&opened.handle, PolicyKind::Payload).unwrap();
        drop(opened);
        assert!(
            parent
                .stage_payload(
                    OsStr::new("staged.bin"),
                    &mut b"foreign".as_slice(),
                    7,
                    zup_core::hash_bytes(b"foreign")
                )
                .is_err()
        );
        parent
            .move_owned_file(
                OsStr::new("staged.bin"),
                object,
                bytes.len() as u64,
                sha256,
                &parent,
                OsStr::new("installed.bin"),
            )
            .unwrap();
        let file = directory
            .open_generation_file("installed.bin", zup_transaction::GenerationAccess::Public)
            .unwrap();
        assert_eq!(
            zup_core::hash_reader(file).unwrap(),
            (bytes.len() as u64, sha256)
        );
        let replacement = b"replacement machine payload";
        let replacement_hash = zup_core::hash_bytes(replacement);
        let replacement_object = parent
            .stage_payload(
                OsStr::new("replacement.bin"),
                &mut replacement.as_slice(),
                replacement.len() as u64,
                replacement_hash,
            )
            .unwrap();
        let old = parent
            .fence_owned_file(
                OsStr::new("installed.bin"),
                object,
                bytes.len() as u64,
                sha256,
            )
            .unwrap();
        let new = parent
            .fence_owned_file(
                OsStr::new("replacement.bin"),
                replacement_object,
                replacement.len() as u64,
                replacement_hash,
            )
            .unwrap();
        parent.move_fenced_file(&old, "retired.bin").unwrap();
        parent.move_fenced_file(&new, "installed.bin").unwrap();
        parent.move_fenced_file(&new, "replacement.bin").unwrap();
        parent.move_fenced_file(&old, "installed.bin").unwrap();
        drop(old);
        drop(new);
        let replacement = parent
            .open_owned_file(
                OsStr::new("replacement.bin"),
                replacement_object,
                replacement.len() as u64,
                replacement_hash,
                PayloadAccess::Mutate,
            )
            .unwrap();
        discard(&replacement).unwrap();
        drop(replacement);
        let file = parent
            .open_owned_file(
                OsStr::new("installed.bin"),
                object,
                bytes.len() as u64,
                sha256,
                crate::filesystem::authority::PayloadAccess::Mutate,
            )
            .unwrap();
        discard(&file).unwrap();
    }

    #[test]
    fn a_held_owned_file_excludes_writer_and_namespace_changes() {
        let root = tempfile::tempdir_in(crate::host_dirs::user_data().unwrap()).unwrap();
        let path = root.path().join("application.bin");
        std::fs::write(&path, b"application").unwrap();
        let zup_plan::ObservedFileState::File {
            object,
            size,
            sha256,
            ..
        } = crate::inspect_file(&path).unwrap()
        else {
            unreachable!()
        };
        let parent = FileParent::open(
            root.path(),
            FileAuthority::User(crate::transport::UserSid::current().unwrap()),
            DirectoryAccess::Read,
        )
        .unwrap();
        let held = parent
            .open_owned_file(
                OsStr::new("application.bin"),
                object,
                size,
                sha256,
                crate::filesystem::authority::PayloadAccess::Mutate,
            )
            .unwrap();
        assert!(std::fs::write(&path, b"changed").is_err());
        assert!(std::fs::rename(&path, root.path().join("moved.bin")).is_err());
        assert!(std::fs::rename(root.path(), root.path().with_extension("moved")).is_err());
        drop(held);
        assert_eq!(std::fs::read(&path).unwrap(), b"application");
    }

    #[test]
    fn handle_relative_move_preserves_the_instance_and_refuses_an_occupied_name() {
        let root = tempfile::tempdir_in(crate::host_dirs::user_data().unwrap()).unwrap();
        let target = root.path().join("target");
        std::fs::create_dir(&target).unwrap();
        let source = root.path().join("owned.bin");
        std::fs::write(&source, b"application").unwrap();
        std::fs::write(target.join("occupied.bin"), b"application").unwrap();
        let zup_plan::ObservedFileState::File {
            object,
            size,
            sha256,
            ..
        } = crate::inspect_file(&source).unwrap()
        else {
            unreachable!()
        };
        let user = crate::transport::UserSid::current().unwrap();
        let parent = FileParent::open(
            root.path(),
            FileAuthority::User(user.clone()),
            DirectoryAccess::CreateFiles,
        )
        .unwrap();
        let destination = FileParent::open(
            &target,
            FileAuthority::User(user),
            DirectoryAccess::CreateFiles,
        )
        .unwrap();
        assert!(
            parent
                .move_owned_file(
                    OsStr::new("owned.bin"),
                    object,
                    size,
                    sha256,
                    &destination,
                    OsStr::new("occupied.bin")
                )
                .is_err()
        );
        assert_eq!(std::fs::read(&source).unwrap(), b"application");
        assert_eq!(
            std::fs::read(target.join("occupied.bin")).unwrap(),
            b"application"
        );
        parent
            .move_owned_file(
                OsStr::new("owned.bin"),
                object,
                size,
                sha256,
                &destination,
                OsStr::new("saved.bin"),
            )
            .unwrap();
        assert!(!source.exists());
        let zup_plan::ObservedFileState::File { object: moved, .. } =
            crate::inspect_file(&target.join("saved.bin")).unwrap()
        else {
            unreachable!()
        };
        assert_eq!(moved, object);
        destination
            .move_owned_file(
                OsStr::new("saved.bin"),
                object,
                size,
                sha256,
                &parent,
                OsStr::new("owned.bin"),
            )
            .unwrap();
        let zup_plan::ObservedFileState::File {
            object: restored, ..
        } = crate::inspect_file(&source).unwrap()
        else {
            unreachable!()
        };
        assert_eq!(restored, object);
    }

    #[test]
    fn borrowed_parent_authority_rejects_user_machine_confusion_and_foreign_creation() {
        let root = tempfile::tempdir_in(crate::host_dirs::user_data().unwrap()).unwrap();
        let user = crate::transport::UserSid::current().unwrap();
        assert!(
            FileParent::open(root.path(), FileAuthority::Machine, DirectoryAccess::Read).is_err()
        );
        let windows = std::env::var_os("SystemRoot").unwrap();
        assert!(
            FileParent::open(
                Path::new(&windows),
                FileAuthority::User(user.clone()),
                DirectoryAccess::Read
            )
            .is_err()
        );
        let output = std::process::Command::new("icacls.exe")
            .arg(root.path())
            .args(["/grant", "*S-1-1-0:(WD)"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            FileParent::open(
                root.path(),
                FileAuthority::User(user),
                DirectoryAccess::Read
            )
            .is_err()
        );
    }
}
