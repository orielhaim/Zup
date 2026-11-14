use super::*;
use std::io::Read;

/// Owns the native file and namespace used by prerequisite verification and execution.
pub(crate) struct ExecutionSource {
    file: File,
    path: PathBuf,
    parents: Vec<OwnedHandle>,
    staged: Option<StagedSource>,
}

struct StagedSource {
    directory: MachineDir,
    parent: MachineDir,
    name: String,
    filename: String,
    identity: (u64, [u8; 16]),
}

impl ExecutionSource {
    pub fn acquire(source: &Path, filename: &str) -> Result<Self, FilesystemError> {
        validate_name(OsStr::new(filename))?;
        let (drive, names) = split_absolute(source)?;
        let (leaf, ancestors) = names
            .split_last()
            .ok_or_else(|| io_path(source, "installer source has no filename"))?;
        let mut parents = vec![open_drive(drive, Open::Directory)?];
        for name in ancestors {
            let directory =
                open_relative(parents.last().unwrap(), name, Open::Directory, None)?.handle;
            reject_reparse(&directory)?;
            parents.push(directory);
        }
        let opened = open_relative(parents.last().unwrap(), leaf, Open::ObserveFile, None)?.handle;
        reject_reparse(&opened)?;
        if attributes(&opened)? & FILE_ATTRIBUTE_DIRECTORY != 0 {
            return Err(io_path(source, "installer source is not a regular file"));
        }
        let mut file = File::from(opened);
        if file.metadata().map_err(io_std)?.len() > zup_core::MAX_PREREQUISITE_PACKAGE_BYTES {
            return Err(io_path(source, "installer exceeds size limit"));
        }
        if !crate::transport::is_process_elevated()
            .map_err(|error| io_std(std::io::Error::other(error)))?
        {
            let path = execution_path(&file)?;
            return Ok(Self {
                file,
                path,
                parents,
                staged: None,
            });
        }

        // Acquisition is untrusted. Copy into a new inode created with a protected DACL.
        let root_path = crate::machine_state::machine_base()?;
        let (root_drive, components) = split_absolute(&root_path)?;
        let (leaf, ancestors) = components
            .split_last()
            .ok_or_else(|| io_path(&root_path, "missing state root"))?;
        let mut trusted = vec![open_drive(root_drive, Open::InspectDirectory)?];
        verify_trusted_ancestor(trusted.last().unwrap())?;
        for name in ancestors {
            let directory =
                open_relative(trusted.last().unwrap(), name, Open::InspectDirectory, None)?.handle;
            reject_reparse(&directory)?;
            verify_trusted_ancestor(&directory)?;
            trusted.push(directory);
        }
        let opened = open_relative(
            trusted.last().unwrap(),
            leaf,
            Open::CreateDirectory,
            Some(PolicyKind::Root),
        )?;
        reject_reparse(&opened.handle)?;
        verify_handle(&opened.handle, PolicyKind::Root)?;
        let root = MachineDir {
            volume: Arc::new(open_volume(&opened.handle)?),
            handle: opened.handle,
            kind: PolicyKind::Root,
        };
        if opened.created {
            root.sync()?;
        }
        let parent = root.ensure_dir("prerequisite-execution", PolicyKind::Administrators)?;
        let name = uuid::Uuid::new_v4().to_string();
        let directory = parent.child(
            &name,
            PolicyKind::Administrators,
            Open::CreateExclusiveDirectory,
        )?;
        let directory_identity = identity(&directory.handle)?;
        trusted.push(root.handle.try_clone().map_err(io_std)?);
        let staging = open_relative(
            &directory.handle,
            OsStr::new(filename),
            Open::Stage,
            Some(PolicyKind::Administrators),
        )?
        .handle;
        verify_file(&staging, PolicyKind::Administrators)?;
        let mut output = File::from(staging);
        let prepared = (|| {
            let size = std::io::copy(
                &mut (&mut file).take(zup_core::MAX_PREREQUISITE_PACKAGE_BYTES + 1),
                &mut output,
            )
            .map_err(io_std)?;
            if size > zup_core::MAX_PREREQUISITE_PACKAGE_BYTES {
                return Err(io_path(source, "installer exceeds size limit"));
            }
            output.sync_all().map_err(io_std)?;
            directory.sync()?;
            let expected = identity(&output)?;
            // Full sharing is intentional: MSI's service may open the database later.
            // The protected DACL, fresh inode and pinned parents provide custody.
            let held =
                open_relative(&directory.handle, OsStr::new(filename), Open::Read, None)?.handle;
            verify_file(&held, PolicyKind::Administrators)?;
            if identity(&held)? != expected {
                return Err(io_path(source, "staged installer identity changed"));
            }
            let path = execution_path(&held)?;
            Ok((held, path))
        })();
        let (held, path) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                if let Err(cleanup) = discard(&output) {
                    tracing::warn!(%cleanup, "prerequisite staging cleanup failed");
                }
                drop(output);
                drop(directory);
                if let Err(cleanup) = parent.remove_empty_directory(&name, directory_identity) {
                    tracing::warn!(%cleanup, "prerequisite staging directory retained");
                }
                return Err(error);
            }
        };
        drop(output);
        Ok(Self {
            file: File::from(held),
            path,
            parents: trusted,
            staged: Some(StagedSource {
                directory,
                parent,
                name,
                filename: filename.to_owned(),
                identity: directory_identity,
            }),
        })
    }

    pub fn file(&mut self) -> &mut File {
        &mut self.file
    }
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn validate(&self) -> Result<(), FilesystemError> {
        for parent in &self.parents {
            reject_reparse(parent)?;
            if self.staged.is_some() {
                verify_trusted_ancestor(parent)?;
            }
        }
        if let Some(staged) = &self.staged {
            staged.parent.validate()?;
            staged.directory.validate()?;
            let directory = open_relative(
                &staged.parent.handle,
                OsStr::new(&staged.name),
                Open::Directory,
                None,
            )?
            .handle;
            reject_reparse(&directory)?;
            if identity(&directory)? != staged.identity {
                return Err(io_path(&self.path, "execution directory identity changed"));
            }
            let observed = open_relative(
                &staged.directory.handle,
                OsStr::new(&staged.filename),
                Open::Read,
                None,
            )?
            .handle;
            verify_file(&observed, PolicyKind::Administrators)?;
            if identity(&observed)? != identity(&self.file)? {
                return Err(io_path(&self.path, "execution source identity changed"));
            }
        }
        use std::os::windows::fs::OpenOptionsExt;
        let observed = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(windows::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT.0)
            .open(&self.path)
            .map_err(io_std)?;
        if identity(&observed)? != identity(&self.file)? {
            return Err(io_path(
                &self.path,
                "native execution path identity changed",
            ));
        }
        // The MSI service resolves DOS names in the global device namespace.
        let global = PathBuf::from(format!(r"\\?\GLOBALROOT\GLOBAL??\{}", self.path.display()));
        let observed = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(windows::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT.0)
            .open(global)
            .map_err(io_std)?;
        if identity(&observed)? != identity(&self.file)? {
            return Err(io_path(
                &self.path,
                "MSI service namespace resolves a different source",
            ));
        }
        Ok(())
    }

    pub fn cleanup(mut self) -> Result<(), FilesystemError> {
        self.validate()?;
        if let Some(staged) = self.staged.take() {
            let file_identity = identity(&self.file)?;
            drop(self.file);
            let remove = open_relative(
                &staged.directory.handle,
                OsStr::new(&staged.filename),
                Open::RemoveFile,
                None,
            )?
            .handle;
            verify_file(&remove, PolicyKind::Administrators)?;
            if identity(&remove)? != file_identity {
                return Err(io_path(&self.path, "cleanup source identity changed"));
            }
            discard(&remove)?;
            drop(remove);
            drop(staged.directory);
            staged
                .parent
                .remove_empty_directory(&staged.name, staged.identity)?;
        }
        Ok(())
    }
}

fn execution_path(handle: &impl AsHandle) -> Result<PathBuf, FilesystemError> {
    let mut path = vec![0u16; 32768];
    let length = unsafe {
        GetFinalPathNameByHandleW(
            HANDLE(handle.as_handle().as_raw_handle()),
            &mut path,
            windows::Win32::Storage::FileSystem::VOLUME_NAME_DOS,
        )
    } as usize;
    if length == 0 || length >= path.len() {
        return Err(io_path(
            Path::new("installer"),
            "cannot resolve execution path",
        ));
    }
    let path = PathBuf::from(OsString::from_wide(&path[..length]));
    let text = path
        .to_str()
        .ok_or_else(|| io_path(&path, "installer path is not Unicode"))?;
    let dos = text
        .strip_prefix(r"\\?\")
        .ok_or_else(|| io_path(&path, "installer requires a local DOS volume"))?;
    if dos.as_bytes().get(1) != Some(&b':') || dos.len() >= 260 {
        return Err(io_path(&path, "installer requires a short local DOS path"));
    }
    Ok(PathBuf::from(dos))
}
