use std::io::{Read, Seek};
use std::path::Path;

use rustix::fs::Mode;
use zup_core::NativeObjectIdentity;
use zup_plan::MetadataFile;
use zup_transaction::{HistoryFilesystem, StateNamespace};

use crate::fs::OwnedDirectory;

pub(crate) struct LinuxHistoryFilesystem(pub crate::state::StateAuthority);

fn display(error: impl std::fmt::Display) -> String {
    error.to_string()
}

fn child(
    parent: &OwnedDirectory,
    name: &str,
    create: bool,
) -> Result<Option<OwnedDirectory>, String> {
    let directory = if parent.kind_or_absent(name).map_err(display)?.is_some() {
        parent.open_child(name).map_err(display)?
    } else if create {
        parent
            .create_exclusive_directory(name, Mode::from_bits_truncate(0o700))
            .map_err(display)?
    } else {
        return Ok(None);
    };
    let stat = directory.stat_self().map_err(display)?;
    if stat.uid != rustix::process::geteuid().as_raw() || stat.mode != 0o700 {
        return Err("history metadata directory ownership or permissions changed".into());
    }
    Ok(Some(directory))
}

impl HistoryFilesystem for LinuxHistoryFilesystem {
    type Directory = OwnedDirectory;

    fn namespace(
        &self,
        namespace: StateNamespace,
        create: bool,
    ) -> Result<Option<OwnedDirectory>, String> {
        let root = OwnedDirectory::open(&self.0.root).map_err(display)?;
        root.verify_descendants(Path::new("."), rustix::process::geteuid().as_raw())
            .map_err(display)?;
        let name = match namespace {
            StateNamespace::Transactions => "transactions",
            StateNamespace::History => "history",
            StateNamespace::Materializations(_) => "materializations",
            StateNamespace::Data => "data",
            StateNamespace::Collections => "collections",
        };
        let Some(parent) = child(&root, name, create)? else {
            return Ok(None);
        };
        if let StateNamespace::Materializations(installation) = namespace {
            child(&parent, &installation.to_string(), create)
        } else {
            Ok(Some(parent))
        }
    }

    fn open_directory(
        &self,
        parent: &OwnedDirectory,
        name: &str,
    ) -> Result<Option<OwnedDirectory>, String> {
        child(parent, name, false)
    }
    fn object(directory: &OwnedDirectory) -> Result<NativeObjectIdentity, String> {
        directory
            .stat_self()
            .map(|stat| stat.object())
            .map_err(display)
    }
    fn names(directory: &OwnedDirectory) -> Result<Vec<String>, String> {
        directory.child_names().map_err(display)
    }

    fn read_file(
        directory: &OwnedDirectory,
        name: &str,
    ) -> Result<Option<(MetadataFile, Vec<u8>)>, String> {
        if directory.kind_or_absent(name).map_err(display)?.is_none() {
            return Ok(None);
        }
        let mut file = directory.stream_regular(name).map_err(display)?;
        if file.uid != rustix::process::geteuid().as_raw()
            || file.mode & 0o077 != 0
            || file.size > 64 * 1024 * 1024
        {
            return Err(
                "history metadata file violates its private ownership or size contract".into(),
            );
        }
        let observed = MetadataFile {
            object: file.object(),
            size: file.size,
            sha256: file.sha256,
        };
        file.file.rewind().map_err(display)?;
        let mut bytes = Vec::new();
        file.file
            .take(64 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map_err(display)?;
        if bytes.len() as u64 != observed.size
            || zup_core::hash_bytes(&bytes) != observed.sha256
            || directory.stat_child(name).map_err(display)?.object() != observed.object
        {
            return Err("history metadata changed during observation".into());
        }
        Ok(Some((observed, bytes)))
    }

    fn move_directory(
        &self,
        parent: &OwnedDirectory,
        name: &str,
        destination: &OwnedDirectory,
        object: NativeObjectIdentity,
    ) -> Result<(), String> {
        let directory = parent.open_child(name).map_err(display)?;
        if Self::object(&directory)? != object {
            return Err("journal directory changed before archival".into());
        }
        parent
            .rename_noreplace_to(name, destination, name)
            .map_err(display)?;
        if Self::object(&destination.open_child(name).map_err(display)?)? != object {
            return Err("journal directory changed during archival".into());
        }
        Ok(())
    }

    fn remove_file(
        directory: &OwnedDirectory,
        name: &str,
        expected: &MetadataFile,
    ) -> Result<(), String> {
        let Some((observed, _)) = Self::read_file(directory, name)? else {
            return Err("history file disappeared before removal".into());
        };
        if observed != *expected {
            return Err("history file changed before removal".into());
        }
        directory.remove_file(name).map_err(display)?;
        directory.sync().map_err(display)
    }

    fn remove_directory(
        parent: &OwnedDirectory,
        name: &str,
        object: NativeObjectIdentity,
    ) -> Result<(), String> {
        let directory = parent.open_child(name).map_err(display)?;
        if Self::object(&directory)? != object
            || !directory.child_names().map_err(display)?.is_empty()
        {
            return Err("history directory identity or contents changed before removal".into());
        }
        parent.remove_empty_directory(name).map_err(display)?;
        parent.sync().map_err(display)
    }
}
