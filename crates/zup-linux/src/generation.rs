use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use rustix::fs::Mode;
use zup_bundle::PayloadSource;
use zup_core::{
    GenerationFile, MaterializationTree, NativeObjectIdentity, Privilege, TransactionId,
};
use zup_plan::GenerationIntent;
use zup_transaction::{
    GenerationAccess, GenerationFilesystem, MaterializationRecord, TransactionRecord,
};

use crate::error::ExecError;
use crate::fs::{EntryKind, OwnedDirectory};
use crate::lowering::to_host_path;

pub(crate) struct LinuxGenerationFilesystem {
    state: crate::state::StateAuthority,
}

impl LinuxGenerationFilesystem {
    pub fn new(state: crate::state::StateAuthority) -> Self {
        Self { state }
    }
    fn metadata(
        &self,
        intent: &GenerationIntent,
        create: bool,
    ) -> Result<(OwnedDirectory, String), ExecError> {
        let root = OwnedDirectory::open(&self.state.root)?;
        root.verify_descendants(Path::new("."), rustix::process::geteuid().as_raw())?;
        let directory = child(
            &root,
            "materializations",
            Mode::from_bits_truncate(0o700),
            create,
        )?;
        let directory = child(
            &directory,
            &intent.materialization.installation().to_string(),
            Mode::from_bits_truncate(0o700),
            create,
        )?;
        Ok((directory, format!("{}.json", intent.materialization.id())))
    }
}

impl GenerationFilesystem for LinuxGenerationFilesystem {
    type Directory = OwnedDirectory;
    type Payload = dyn PayloadSource;
    type Error = ExecError;

    fn refused(intent: &GenerationIntent, reason: &str) -> ExecError {
        refused(intent, reason)
    }

    fn namespace(
        &self,
        intent: &GenerationIntent,
        create: bool,
    ) -> Result<Option<OwnedDirectory>, ExecError> {
        let root = intent
            .materialization
            .directory()
            .parent()
            .and_then(|path| path.parent())
            .and_then(|path| path.parent())
            .ok_or_else(|| refused(intent, "materialization has no installation namespace"))?;
        let _state_boundary = self.state.payload_directory(&to_host_path(&root)?)?;
        match namespace(intent, create) {
            Ok(directory) => Ok(Some(directory)),
            Err(error) if !create && missing(&error) => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn open_child(
        &self,
        parent: &OwnedDirectory,
        name: &str,
        access: GenerationAccess,
    ) -> Result<Option<OwnedDirectory>, ExecError> {
        if parent.kind_or_absent(name)?.is_none() {
            return Ok(None);
        }
        let directory = parent.open_child(name)?;
        let mode = directory.stat_self()?.mode;
        if (access == GenerationAccess::Private && mode != 0o700) || !matches!(mode, 0o700 | 0o755)
        {
            return Err(ExecError::Executor(
                "generation directory access policy changed".into(),
            ));
        }
        directory.verify_descendants(Path::new("."), rustix::process::geteuid().as_raw())?;
        Ok(Some(directory))
    }

    fn create_private_child(
        &self,
        parent: &OwnedDirectory,
        name: &str,
    ) -> Result<OwnedDirectory, ExecError> {
        let directory = parent.create_exclusive_directory(name, Mode::from_bits_truncate(0o700))?;
        parent.sync()?;
        Ok(directory)
    }

    fn object(directory: &OwnedDirectory) -> Result<NativeObjectIdentity, ExecError> {
        object(directory)
    }

    fn populate_private(
        &self,
        directory: &OwnedDirectory,
        intent: &GenerationIntent,
        payload: &dyn PayloadSource,
    ) -> Result<(), ExecError> {
        for file in intent.materialization.descriptor().files() {
            let source = intent
                .sources
                .get(&file.path)
                .ok_or_else(|| refused(intent, "generation member has no authenticated source"))?;
            let mut components: Vec<_> = file.path.as_str().split('/').collect();
            let leaf = components
                .pop()
                .ok_or_else(|| refused(intent, "generation member path is empty"))?;
            let mut parent = directory.duplicate()?;
            for component in components {
                parent = match self.open_child(&parent, component, GenerationAccess::Private)? {
                    Some(directory) => directory,
                    None => self.create_private_child(&parent, component)?,
                };
            }
            if parent.kind_or_absent(leaf)?.is_some() {
                verify_file(&parent, leaf, file, intent)?;
                continue;
            }
            let mut reader = payload
                .open(&source.source_relative, &file.sha256, file.size)
                .map_err(|source_error| ExecError::Payload {
                    path: source.source_relative.to_string(),
                    source: source_error,
                })?;
            let (size, digest) = parent.stage_stream(
                leaf,
                &mut reader,
                file.size,
                Mode::from_bits_truncate(file_mode(file)),
            )?;
            if size != file.size || digest != file.sha256 {
                return Err(refused(
                    intent,
                    "materialized member does not match authenticated content",
                ));
            }
            parent.sync()?;
        }
        Ok(())
    }

    fn verify_tree(
        &self,
        directory: &OwnedDirectory,
        intent: &GenerationIntent,
        access: GenerationAccess,
    ) -> Result<MaterializationTree, ExecError> {
        verify_tree(directory, intent, access)
    }

    fn publish_access(
        &self,
        root: OwnedDirectory,
        intent: &GenerationIntent,
        tree: &MaterializationTree,
    ) -> Result<OwnedDirectory, ExecError> {
        tree.validate(intent.materialization.descriptor())
            .map_err(|error| refused(intent, &error))?;
        if object(&root)? != tree.root {
            return Err(refused(
                intent,
                "generation root changed before access publication",
            ));
        }
        for (path, expected) in tree.directories.iter().rev() {
            let mut parents = vec![root.duplicate()?];
            let mut prefix = String::new();
            for component in path.as_str().split('/') {
                if !prefix.is_empty() {
                    prefix.push('/');
                }
                prefix.push_str(component);
                let parent = parents.last().expect("root capability");
                let directory = parent.open_child(component)?;
                let key =
                    zup_core::RelativePath::new(&prefix).map_err(|error| refused(intent, error))?;
                if tree.directories.get(&key) != Some(&object(&directory)?) {
                    return Err(refused(
                        intent,
                        "generation parent changed during access publication",
                    ));
                }
                parents.push(directory);
            }
            let directory = parents.last().expect("generation member");
            if object(directory)? != *expected {
                return Err(refused(
                    intent,
                    "generation directory changed before access publication",
                ));
            }
            directory.chmod_self(published_directory_mode(intent))?;
            directory.sync()?;
            zup_transaction::fault_injection::boundary(
                zup_transaction::fault_injection::Boundary::MaterializationMemberExposed,
                intent
                    .operation_id(zup_plan::GenerationOperation::Seal)
                    .as_str(),
            );
        }
        if object(&root)? != tree.root {
            return Err(refused(
                intent,
                "generation root changed during access publication",
            ));
        }
        root.chmod_self(published_directory_mode(intent))?;
        root.sync()?;
        Ok(root)
    }

    fn rename_noreplace(
        &self,
        parent: &OwnedDirectory,
        source: &str,
        destination: &str,
        expected: NativeObjectIdentity,
    ) -> Result<(), ExecError> {
        let directory = parent.open_child(source)?;
        if object(&directory)? != expected {
            return Err(ExecError::Executor(
                "staging directory identity changed before publication".into(),
            ));
        }
        parent.rename_noreplace(source, destination)?;
        if object(&parent.open_child(destination)?)? != expected {
            return Err(ExecError::Executor(
                "published directory identity changed".into(),
            ));
        }
        Ok(())
    }

    fn sync(directory: &OwnedDirectory) -> Result<(), ExecError> {
        Ok(directory.sync()?)
    }

    fn read_record(
        &self,
        intent: &GenerationIntent,
    ) -> Result<Option<MaterializationRecord>, ExecError> {
        let (directory, name) = match self.metadata(intent, false) {
            Ok(metadata) => metadata,
            Err(error) if missing(&error) => return Ok(None),
            Err(error) => return Err(error),
        };
        if directory.kind_or_absent(&name)?.is_none() {
            return Ok(None);
        }
        let file = directory.open_regular_read(&name)?;
        let state = rustix::fs::fstat(&file).map_err(|error| refused(intent, error))?;
        if state.st_uid != rustix::process::geteuid().as_raw() || state.st_mode & 0o7777 != 0o600 {
            return Err(refused(
                intent,
                "materialization metadata has unsafe ownership or permissions",
            ));
        }
        Ok(Some(
            serde_json::from_reader(file).map_err(|error| refused(intent, error))?,
        ))
    }

    fn write_record(
        &self,
        _intent: &GenerationIntent,
        record: &MaterializationRecord,
        create_new: bool,
    ) -> Result<(), ExecError> {
        let root = OwnedDirectory::open(&self.state.root)?;
        root.verify_descendants(Path::new("."), rustix::process::geteuid().as_raw())?;
        let directory = child(
            &root,
            "materializations",
            Mode::from_bits_truncate(0o700),
            create_new,
        )?;
        let directory = child(
            &directory,
            &record.specification.installation().to_string(),
            Mode::from_bits_truncate(0o700),
            create_new,
        )?;
        let name = format!("{}.json", record.specification.id());
        let bytes = serde_json::to_vec(record)?;
        if create_new {
            directory.create_durable_exclusive(&name, &bytes, Mode::from_bits_truncate(0o600))?;
        } else {
            directory.write_durable(&name, &bytes, Mode::from_bits_truncate(0o600))?;
        }
        Ok(())
    }

    fn origin(&self, transaction: TransactionId) -> Result<TransactionRecord, ExecError> {
        self.state.source(transaction)
    }
}

fn namespace(intent: &GenerationIntent, create: bool) -> Result<OwnedDirectory, ExecError> {
    let directory = intent.materialization.directory();
    let root = directory
        .parent()
        .and_then(|path| path.parent())
        .and_then(|path| path.parent())
        .ok_or_else(|| refused(intent, "materialization has no installation namespace"))?;
    let expected = root
        .join(format!(
            "{}/{}",
            intent.publication.namespace(),
            intent.materialization.id()
        ))
        .map_err(|error| refused(intent, error))?;
    if !directory.equivalent(&expected) {
        return Err(refused(
            intent,
            "materialization is outside its installation namespace",
        ));
    }
    let host = to_host_path(&root)?;
    let mut parent = if create {
        OwnedDirectory::create(&host, published_directory_mode(intent))?
    } else {
        OwnedDirectory::open(&host)?
    };
    parent.verify_descendants(Path::new("."), rustix::process::geteuid().as_raw())?;
    for component in intent.publication.namespace().split('/') {
        parent = child(&parent, component, Mode::from_bits_truncate(0o700), create)?;
    }
    Ok(parent)
}

fn missing(error: &ExecError) -> bool {
    match error {
        ExecError::Path(crate::error::PathError::Io { source, .. }) => {
            source.kind() == std::io::ErrorKind::NotFound
        }
        ExecError::Path(crate::error::PathError::Missing { .. }) => true,
        _ => false,
    }
}

fn child(
    parent: &OwnedDirectory,
    name: &str,
    mode: Mode,
    create: bool,
) -> Result<OwnedDirectory, ExecError> {
    let directory = if create && parent.kind_or_absent(name)?.is_none() {
        parent.create_exclusive_directory(name, mode)?
    } else {
        parent.open_child(name)?
    };
    directory.verify_descendants(Path::new("."), rustix::process::geteuid().as_raw())?;
    Ok(directory)
}

fn verify_tree(
    root: &OwnedDirectory,
    intent: &GenerationIntent,
    access: GenerationAccess,
) -> Result<MaterializationTree, ExecError> {
    let expected: BTreeMap<_, _> = intent
        .materialization
        .descriptor()
        .files()
        .iter()
        .map(|file| (file.path.as_str().to_owned(), file))
        .collect();
    let mut directories = BTreeSet::new();
    for path in expected.keys() {
        for (index, _) in path.match_indices('/') {
            directories.insert(path[..index].to_owned());
        }
    }
    type Parent = Option<(std::sync::Arc<OwnedDirectory>, String, NativeObjectIdentity)>;
    enum Walk {
        Scan(String, std::sync::Arc<OwnedDirectory>, Parent),
        Recheck(std::sync::Arc<OwnedDirectory>, Parent),
    }
    let mut pending = vec![Walk::Scan(
        String::new(),
        std::sync::Arc::new(root.duplicate()?),
        None,
    )];
    let mut tree = MaterializationTree {
        root: object(root)?,
        files: BTreeMap::new(),
        directories: BTreeMap::new(),
    };
    let device = root.stat_self()?.dev;
    while let Some(walk) = pending.pop() {
        let (prefix, directory, parent) = match walk {
            Walk::Scan(prefix, directory, parent) => (prefix, directory, parent),
            Walk::Recheck(directory, parent) => {
                directory.sync()?;
                if let Some((parent, name, expected)) = parent
                    && parent.stat_child(&name)?.object() != expected
                {
                    return Err(refused(
                        intent,
                        "generation directory changed during verification",
                    ));
                }
                continue;
            }
        };
        let mode = directory.stat_self()?.mode;
        let public_mode = published_directory_mode(intent).bits();
        let valid = match access {
            GenerationAccess::Private => mode == 0o700,
            GenerationAccess::Publishing => mode == 0o700 || mode == public_mode,
            GenerationAccess::Public => mode == public_mode,
        };
        if !valid {
            return Err(refused(
                intent,
                "generation directory access policy changed",
            ));
        }
        pending.push(Walk::Recheck(std::sync::Arc::clone(&directory), parent));
        for name in directory.child_names()? {
            let path = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            let state = directory.stat_child(&name)?;
            if state.dev != device {
                return Err(refused(intent, "generation crosses a filesystem boundary"));
            }
            match state.kind {
                EntryKind::Directory if directories.contains(&path) => {
                    let child = directory.open_child(&name)?;
                    child
                        .verify_descendants(Path::new("."), rustix::process::geteuid().as_raw())?;
                    let identity = object(&child)?;
                    if identity != state.object() {
                        return Err(refused(
                            intent,
                            "generation directory changed while opening",
                        ));
                    }
                    tree.directories.insert(
                        zup_core::RelativePath::new(&path)
                            .map_err(|error| refused(intent, error))?,
                        identity,
                    );
                    pending.push(Walk::Scan(
                        path,
                        std::sync::Arc::new(child),
                        Some((std::sync::Arc::clone(&directory), name, identity)),
                    ));
                }
                EntryKind::Regular if expected.contains_key(&path) => {
                    let identity = verify_file(&directory, &name, expected[&path], intent)?;
                    if identity != state.object() {
                        return Err(refused(intent, "generation member changed while opening"));
                    }
                    tree.files.insert(expected[&path].path.clone(), identity);
                }
                _ => {
                    return Err(refused(
                        intent,
                        "generation contains an undeclared or unsafe entry",
                    ));
                }
            }
        }
    }
    tree.validate(intent.materialization.descriptor())
        .map_err(|error| refused(intent, error))?;
    Ok(tree)
}

fn verify_file(
    parent: &OwnedDirectory,
    name: &str,
    expected: &GenerationFile,
    intent: &GenerationIntent,
) -> Result<NativeObjectIdentity, ExecError> {
    let observed = parent.stream_regular(name)?;
    if observed.size != expected.size
        || observed.sha256 != expected.sha256
        || observed.mode != file_mode(expected)
        || observed.uid != rustix::process::geteuid().as_raw()
    {
        return Err(refused(
            intent,
            "generation member content, mode or ownership changed",
        ));
    }
    observed
        .file
        .sync_all()
        .map_err(|error| refused(intent, error))?;
    let object = observed.object();
    if parent.stat_child(name)?.object() != object {
        return Err(refused(
            intent,
            "generation member changed during verification",
        ));
    }
    Ok(object)
}

fn object(directory: &OwnedDirectory) -> Result<NativeObjectIdentity, ExecError> {
    let state = directory.stat_self()?;
    Ok(NativeObjectIdentity::Linux {
        device: state.dev,
        inode: state.ino,
    })
}

fn published_directory_mode(intent: &GenerationIntent) -> Mode {
    Mode::from_bits_truncate(if intent.privilege == Privilege::System {
        0o755
    } else {
        0o700
    })
}

fn file_mode(file: &GenerationFile) -> u32 {
    if file.executable { 0o555 } else { 0o444 }
}

fn refused(intent: &GenerationIntent, reason: impl std::fmt::Display) -> ExecError {
    ExecError::refused(
        intent.materialization.directory().as_str(),
        reason.to_string(),
    )
}
