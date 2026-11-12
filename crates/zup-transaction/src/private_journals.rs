use super::{StoreError, store_fs};
use crate::JournalDirectory;
use private_fs::{
    Durability, FileAccess, FileCreation, OpenOptions, PrivateDir, Publication, Verification,
};
use std::{fs::File, io::Read, path::PathBuf};
use zup_core::NativeObjectIdentity;

pub(super) struct PrivateJournalDirectory(pub PrivateDir);
fn error(error: private_fs::Error) -> StoreError {
    store_fs(PathBuf::from("journals"), error)
}
fn io(error: std::io::Error) -> StoreError {
    StoreError::Io {
        path: "journals".into(),
        source: error,
    }
}
impl JournalDirectory for PrivateJournalDirectory {
    fn open(&self, name: &str) -> Result<Option<Self>, StoreError> {
        match self.0.open_dir(name) {
            Ok(dir) => {
                let dir = Self(dir);
                if !self.same_mount(&dir)? {
                    return Err(io(std::io::Error::other("journal crosses a volume")));
                }
                Ok(Some(dir))
            }
            Err(private_fs::Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(error(e)),
        }
    }
    fn create(&self, name: &str) -> Result<Self, StoreError> {
        self.0.create_dir(name).map(Self).map_err(error)
    }
    fn object(&self) -> Result<NativeObjectIdentity, StoreError> {
        self.0.sync_all().map_err(error)?;
        zup_storage::namespace::identity(&self.0).map_err(io)
    }
    fn same_mount(&self, other: &Self) -> Result<bool, StoreError> {
        zup_storage::namespace::same_mount(&self.0, &other.0).map_err(io)
    }
    fn names(&self) -> Result<Vec<String>, StoreError> {
        self.0
            .entries()
            .map_err(error)?
            .into_iter()
            .map(|name| {
                name.into_string()
                    .map_err(|name| StoreError::InvalidDirectory {
                        name: name.to_string_lossy().into_owned(),
                    })
            })
            .collect()
    }
    fn read(&self, name: &str) -> Result<Option<Vec<u8>>, StoreError> {
        let mut file = match self.0.open_file(name) {
            Ok(file) => file,
            Err(private_fs::Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(None);
            }
            Err(e) => return Err(error(e)),
        };
        if !zup_storage::namespace::same_mount(&self.0, &file).map_err(io)? {
            return Err(io(std::io::Error::other("journal file crosses a mount")));
        }
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).map_err(io)?;
        Ok(Some(bytes))
    }
    fn publish(&self, name: &str, bytes: &[u8], create: bool) -> Result<(), StoreError> {
        zup_storage::publish_full(
            &self.0,
            name,
            bytes,
            if create {
                Publication::CreateNew
            } else {
                Publication::Replace
            },
        )
        .map_err(|source| StoreError::Publication {
            path: name.into(),
            source,
        })
    }
    fn lock_file(&self, name: &str, creation: FileCreation) -> Result<File, StoreError> {
        let file = self
            .0
            .open_file_with(
                name,
                OpenOptions {
                    access: FileAccess::ReadWrite,
                    creation,
                    truncate: false,
                    verification: Verification::OnDemand,
                },
            )
            .map_err(error)?;
        if !zup_storage::namespace::same_mount(&self.0, &file).map_err(io)? {
            return Err(io(std::io::Error::other("journal lock crosses a mount")));
        }
        zup_storage::duplicate(&file).map_err(error)
    }
    fn move_to(
        &self,
        name: &str,
        destination: &Self,
        target: &str,
        object: NativeObjectIdentity,
    ) -> Result<(), StoreError> {
        zup_storage::namespace::rename_directory(&self.0, name, &destination.0, target, object)
            .map_err(|e| StoreError::Publication {
                path: target.into(),
                source: zup_storage::PublishError::Indeterminate {
                    may_have_published: true,
                    detail: e.to_string(),
                },
            })
    }
    fn sync(&self) -> Result<(), StoreError> {
        self.0.sync_all().map_err(error)
    }
    fn retire_reservation(&self, bytes: &[u8]) -> Result<(), StoreError> {
        if self.read("pending.json")?.as_deref() != Some(bytes) {
            return Err(crate::CorruptReason::IdentityMismatch.into());
        }
        self.0
            .remove_file("pending.json", Durability::None)
            .map_err(error)?;
        crate::fault_injection::boundary(
            crate::fault_injection::Boundary::JournalReservationRemoved,
            "journal-creation",
        );
        self.sync()
    }
}
