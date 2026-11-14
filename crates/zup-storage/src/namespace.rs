//! Identity-checked, no-replace publication of private directory capabilities.
use private_fs::PrivateDir;
use std::io;
use zup_core::NativeObjectIdentity;

#[cfg(target_os = "linux")]
pub fn identity(handle: &impl std::os::fd::AsFd) -> io::Result<NativeObjectIdentity> {
    let stat = rustix::fs::fstat(handle)?;
    Ok(NativeObjectIdentity::Linux {
        device: stat.st_dev,
        inode: stat.st_ino,
    })
}

#[cfg(windows)]
pub fn identity(handle: &impl std::os::windows::io::AsHandle) -> io::Result<NativeObjectIdentity> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::{
        Foundation::HANDLE,
        Storage::FileSystem::{FILE_ID_INFO, FileIdInfo, GetFileInformationByHandleEx},
    };
    let mut info = FILE_ID_INFO::default();
    // The output is a live, correctly sized FILE_ID_INFO for this retained handle.
    unsafe {
        GetFileInformationByHandleEx(
            HANDLE(handle.as_handle().as_raw_handle()),
            FileIdInfo,
            (&mut info as *mut FILE_ID_INFO).cast(),
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        )
    }
    .map_err(io::Error::other)?;
    Ok(NativeObjectIdentity::Windows {
        volume: info.VolumeSerialNumber,
        file: info.FileId.Identifier,
    })
}

#[cfg(target_os = "linux")]
pub fn same_mount(
    left: &impl std::os::fd::AsFd,
    right: &impl std::os::fd::AsFd,
) -> io::Result<bool> {
    use rustix::fs::{AtFlags, StatxFlags, statx};
    let left = statx(left, "", AtFlags::EMPTY_PATH, StatxFlags::MNT_ID)?;
    let right = statx(right, "", AtFlags::EMPTY_PATH, StatxFlags::MNT_ID)?;
    if left.stx_mask & StatxFlags::MNT_ID.bits() == 0
        || right.stx_mask & StatxFlags::MNT_ID.bits() == 0
    {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "journal mount identity unavailable",
        ));
    }
    Ok(left.stx_mnt_id == right.stx_mnt_id)
}

#[cfg(windows)]
pub fn same_mount(
    left: &impl std::os::windows::io::AsHandle,
    right: &impl std::os::windows::io::AsHandle,
) -> io::Result<bool> {
    Ok(volume(identity(left)?) == volume(identity(right)?))
}

pub fn rename_directory(
    parent: &PrivateDir,
    name: &str,
    destination: &PrivateDir,
    target: &str,
    expected: NativeObjectIdentity,
) -> io::Result<()> {
    for component in [name, target] {
        if component.is_empty()
            || component == "."
            || component == ".."
            || component.contains(['/', '\\', ':', '\0'])
            || component.ends_with(['.', ' '])
            || component.len() > 255
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid journal component",
            ));
        }
    }
    let source = parent.open_dir(name).map_err(io::Error::other)?;
    if identity(&source)? != expected
        || !same_mount(parent, &source)?
        || !same_mount(parent, destination)?
    {
        return Err(io::Error::other(
            "journal namespace identity or volume changed",
        ));
    }
    #[cfg(target_os = "linux")]
    rustix::fs::renameat_with(
        parent,
        name,
        destination,
        target,
        rustix::fs::RenameFlags::NOREPLACE,
    )?;
    #[cfg(windows)]
    windows_rename(parent, name, &source, destination, target, expected)?;
    // The caller owns the publication barrier and ambiguous acknowledgement.
    Ok(())
}

pub fn volume(object: NativeObjectIdentity) -> u64 {
    match object {
        NativeObjectIdentity::Linux { device, .. } => device,
        NativeObjectIdentity::Windows { volume, .. } => volume,
    }
}

#[cfg(windows)]
fn windows_rename(
    parent: &PrivateDir,
    name: &str,
    source: &PrivateDir,
    destination: &PrivateDir,
    target: &str,
    expected: NativeObjectIdentity,
) -> io::Result<()> {
    use std::{
        ffi::c_void,
        mem::{offset_of, size_of},
        os::windows::{ffi::OsStrExt, io::AsRawHandle},
    };
    use windows::Win32::{Foundation::HANDLE, Storage::FileSystem::FILE_RENAME_INFO};
    #[repr(C)]
    struct IoStatus {
        status: isize,
        information: usize,
    }
    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn NtSetInformationFile(
            file: HANDLE,
            status: *mut IoStatus,
            info: *const c_void,
            size: u32,
            class: u32,
        ) -> i32;
        fn RtlNtStatusToDosError(status: i32) -> u32;
    }
    let reopened = open_directory(parent, name, 0x0001_0000 | 0x0010_0000 | 0x0002_0000 | 0x80)?;
    if identity(&reopened)? != expected {
        return Err(io::Error::other("journal directory changed before rename"));
    }
    let target_handle = open_directory(
        destination,
        "",
        0x4 | 0x2 | 0x0010_0000 | 0x0002_0000 | 0x80,
    )?;
    if identity(&target_handle)? != identity(destination)? || identity(source)? != expected {
        return Err(io::Error::other("journal publication parent changed"));
    }
    let wide: Vec<u16> = std::ffi::OsStr::new(target).encode_wide().collect();
    let bytes = offset_of!(FILE_RENAME_INFO, FileName) + wide.len() * 2;
    let mut buffer = vec![0u64; bytes.div_ceil(size_of::<u64>())];
    let mut status = IoStatus {
        status: 0,
        information: 0,
    };
    // The aligned buffer holds the header and the complete UTF-16 component.
    let result = unsafe {
        let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFO>();
        (*info).Anonymous.ReplaceIfExists = false;
        (*info).RootDirectory = HANDLE(target_handle.as_raw_handle());
        (*info).FileNameLength = (wide.len() * 2) as u32;
        std::ptr::copy_nonoverlapping(wide.as_ptr(), (*info).FileName.as_mut_ptr(), wide.len());
        NtSetInformationFile(
            HANDLE(reopened.as_raw_handle()),
            &mut status,
            buffer.as_ptr().cast(),
            bytes as u32,
            10,
        )
    };
    if result < 0 {
        return Err(io::Error::from_raw_os_error(
            unsafe { RtlNtStatusToDosError(result) } as i32,
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn open_directory(
    parent: &PrivateDir,
    name: &str,
    access: u32,
) -> io::Result<std::os::windows::io::OwnedHandle> {
    use std::{
        ffi::c_void,
        os::windows::{
            ffi::OsStrExt,
            io::{AsHandle, AsRawHandle, FromRawHandle, OwnedHandle},
        },
    };
    #[repr(C)]
    struct Unicode {
        length: u16,
        maximum: u16,
        buffer: *mut u16,
    }
    #[repr(C)]
    struct Attributes {
        length: u32,
        root: *mut c_void,
        name: *const Unicode,
        flags: u32,
        security: *mut c_void,
        quality: *mut c_void,
    }
    #[repr(C)]
    struct Status {
        status: isize,
        information: usize,
    }
    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn NtCreateFile(
            handle: *mut *mut c_void,
            access: u32,
            attributes: *const Attributes,
            status: *mut Status,
            allocation: *const i64,
            file_attributes: u32,
            share: u32,
            disposition: u32,
            options: u32,
            ea: *const c_void,
            ea_length: u32,
        ) -> i32;
        fn RtlNtStatusToDosError(status: i32) -> u32;
    }
    let mut wide: Vec<u16> = std::ffi::OsStr::new(name).encode_wide().collect();
    let length = u16::try_from(wide.len() * 2).map_err(io::Error::other)?;
    let string = Unicode {
        length,
        maximum: length,
        buffer: wide.as_mut_ptr(),
    };
    let attributes = Attributes {
        length: std::mem::size_of::<Attributes>() as u32,
        root: parent.as_handle().as_raw_handle(),
        name: &string,
        flags: 0x40 | 0x1000,
        security: std::ptr::null_mut(),
        quality: std::ptr::null_mut(),
    };
    let mut status = Status {
        status: 0,
        information: 0,
    };
    let mut handle = std::ptr::null_mut();
    // All descriptor pointers and output storage remain live through the synchronous call.
    let result = unsafe {
        NtCreateFile(
            &mut handle,
            access,
            &attributes,
            &mut status,
            std::ptr::null(),
            0,
            7,
            1,
            0x0020_0000 | 0x20 | 1,
            std::ptr::null(),
            0,
        )
    };
    if result < 0 {
        return Err(io::Error::from_raw_os_error(
            unsafe { RtlNtStatusToDosError(result) } as i32,
        ));
    }
    Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
}
