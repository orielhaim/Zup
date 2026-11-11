use crate::{error::ExecError, fs::OwnedDirectory};
use rustix::fs::{XattrFlags, fgetxattr, fsetxattr};
use zup_plan::{FilesystemDataBinding, ServiceDataPrincipal};

const ACCESS: &str = "system.posix_acl_access";
const DEFAULT: &str = "system.posix_acl_default";

fn refused(reason: &str) -> ExecError {
    ExecError::refused("persistent access", reason)
}

fn expected(binding: &FilesystemDataBinding) -> Result<Option<Vec<u8>>, ExecError> {
    if binding.writers.is_empty() {
        return Ok(None);
    }
    let mut users = std::collections::BTreeSet::new();
    for writer in &binding.writers {
        let ServiceDataPrincipal::LinuxUser { name, uid } = &writer.principal else {
            return Err(refused("data writer is not a Linux account"));
        };
        if uzers::get_user_by_name(name).is_none_or(|account| account.uid() != *uid) {
            return Err(refused("service account identity changed since planning"));
        }
        users.insert(*uid);
    }
    // Linux POSIX ACL xattr UAPI: version, followed by little-endian tag/permission/id entries.
    let mut bytes = 2u32.to_le_bytes().to_vec();
    let mut entry = |tag: u16, permissions: u16, id: u32| {
        bytes.extend(tag.to_le_bytes());
        bytes.extend(permissions.to_le_bytes());
        bytes.extend(id.to_le_bytes());
    };
    entry(1, 7, u32::MAX);
    for uid in users {
        entry(2, 7, uid);
    }
    entry(4, 0, u32::MAX);
    entry(16, 7, u32::MAX);
    entry(32, 0, u32::MAX);
    Ok(Some(bytes))
}

fn read(directory: &OwnedDirectory, name: &str) -> Result<Option<Vec<u8>>, ExecError> {
    let mut bytes = vec![0u8; 4096];
    match fgetxattr(directory.as_fd(), name, bytes.as_mut_slice()) {
        Ok(size) => {
            bytes.truncate(size);
            Ok(Some(bytes))
        }
        Err(rustix::io::Errno::NODATA) => Ok(None),
        Err(error) => Err(refused(&format!("cannot observe native data ACL: {error}"))),
    }
}

pub(crate) fn verify_preparation(
    directory: &OwnedDirectory,
    binding: &FilesystemDataBinding,
) -> Result<(), ExecError> {
    let state = directory.stat_self()?;
    let expected = expected(binding)?;
    let access = read(directory, ACCESS)?;
    let default = read(directory, DEFAULT)?;
    let private = state.mode & 0o7777 == 0o700 && access.is_none();
    let configured = expected.is_some() && access == expected && state.mode & 0o7777 == 0o770;
    if state.uid != rustix::process::geteuid().as_raw()
        || !(private || configured)
        || default.is_some() && default != expected
        || configured && default != expected
    {
        return Err(refused(
            "data root owner or ACL differs from its initial writer policy",
        ));
    }
    Ok(())
}

pub(crate) fn verify(
    directory: &OwnedDirectory,
    binding: &FilesystemDataBinding,
) -> Result<(), ExecError> {
    verify_preparation(directory, binding)?;
    let expected = expected(binding)?;
    if read(directory, ACCESS)? != expected || read(directory, DEFAULT)? != expected {
        return Err(refused("persistent writer access is not provisioned"));
    }
    Ok(())
}

pub(crate) fn provision(
    directory: &OwnedDirectory,
    binding: &FilesystemDataBinding,
) -> Result<(), ExecError> {
    verify_preparation(directory, binding)?;
    if verify(directory, binding).is_ok() {
        return Ok(());
    }
    let Some(acl) = expected(binding)? else {
        return Err(refused("private root has an unexpected native ACL"));
    };
    if !directory.child_names()?.is_empty() {
        return Err(refused(
            "initial writer grants require an empty private root",
        ));
    }
    fsetxattr(directory.as_fd(), DEFAULT, &acl, XattrFlags::empty())
        .map_err(|error| refused(&format!("cannot provision inherited data ACL: {error}")))?;
    directory.sync()?;
    fsetxattr(directory.as_fd(), ACCESS, &acl, XattrFlags::empty())
        .map_err(|error| refused(&format!("cannot provision data access ACL: {error}")))?;
    directory.sync()?;
    verify(directory, binding)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::{fs::PermissionsExt, process::CommandExt};

    #[test]
    #[ignore = "requires root and the nobody account"]
    fn service_writer_inherits_access_without_owning_the_root() {
        assert_eq!(rustix::process::geteuid().as_raw(), 0);
        let account = uzers::get_user_by_name("nobody").unwrap();
        let temporary = tempfile::tempdir().unwrap();
        std::fs::set_permissions(temporary.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = temporary.path().join("data");
        let directory =
            OwnedDirectory::create(&path, rustix::fs::Mode::from_bits_truncate(0o700)).unwrap();
        let binding = FilesystemDataBinding {
            path: zup_core::TargetPath::new(
                zup_core::Target::host().unwrap(),
                path.to_string_lossy(),
            )
            .unwrap(),
            writers: vec![zup_plan::ServiceDataWriter {
                service: zup_core::ServiceId::new("worker").unwrap(),
                principal: ServiceDataPrincipal::LinuxUser {
                    name: "nobody".into(),
                    uid: account.uid(),
                },
            }],
        };
        let object = directory.stat_self().unwrap().object();
        provision(&directory, &binding).unwrap();
        let database = path.join("database");
        let created = std::process::Command::new("/usr/bin/touch")
            .arg(&database)
            .uid(account.uid())
            .gid(account.primary_group_id())
            .status()
            .unwrap();
        assert!(created.success());
        assert_eq!(directory.stat_self().unwrap().uid, 0);
        assert_eq!(directory.stat_self().unwrap().object(), object);
        provision(&directory, &binding).unwrap();
        assert!(database.exists());
        directory
            .chmod_self(rustix::fs::Mode::from_bits_truncate(0o777))
            .unwrap();
        assert!(verify(&directory, &binding).is_err());
        assert!(provision(&directory, &binding).is_err());
        assert!(database.exists());
    }
}
