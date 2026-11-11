use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::marker::PhantomData;

use private_fs::{Durability, PrivateDir, Publication};
use zup_bundle::PayloadSource;
use zup_core::{
    GenerationFile, MaterializationTree, NativeObjectIdentity, Privilege, TransactionId,
};
use zup_plan::GenerationIntent;
use zup_transaction::{
    GenerationAccess, GenerationFilesystem, MaterializationRecord, TransactionRecord,
};

use crate::filesystem::{MachineDir, PolicyKind};
use crate::machine_authority::MachineRoot;

pub(crate) enum GenerationDirectory {
    User(PrivateDir),
    Machine(MachineDir),
}

impl GenerationDirectory {
    pub(crate) fn child(
        &self,
        name: &str,
        create: bool,
        access: GenerationAccess,
    ) -> Result<Self, String> {
        match self {
            Self::User(directory) => {
                let result = if create {
                    directory.create_dir(name)
                } else {
                    directory.open_dir(name)
                };
                result.map(Self::User).map_err(display)
            }
            Self::Machine(directory) => {
                let result = directory.generation_child(name, create, access);
                result.map(Self::Machine).map_err(display)
            }
        }
    }

    pub(crate) fn names(&self) -> Result<Vec<String>, String> {
        match self {
            Self::User(directory) => directory
                .entries()
                .map_err(display)?
                .into_iter()
                .map(|name| {
                    name.into_string()
                        .map_err(|_| "non-Unicode generation entry".into())
                })
                .collect(),
            Self::Machine(directory) => directory.list().map_err(display),
        }
    }

    pub(crate) fn identity(&self) -> Result<NativeObjectIdentity, String> {
        let (volume, file) = match self {
            Self::User(directory) => {
                crate::filesystem::private_directory_identity(directory).map_err(display)?
            }
            Self::Machine(directory) => directory.object_identity().map_err(display)?,
        };
        Ok(NativeObjectIdentity::Windows { volume, file })
    }

    fn sync(&self) -> Result<(), String> {
        match self {
            Self::User(directory) => directory.sync_all().map_err(display),
            Self::Machine(directory) => directory.sync().map_err(display),
        }
    }

    pub(crate) fn verify_file(
        &self,
        name: &str,
        expected: &GenerationFile,
        access: GenerationAccess,
    ) -> Result<NativeObjectIdentity, String> {
        let (observed, object) = match self {
            Self::User(directory) => {
                let mut file = directory.open_file(name).map_err(display)?;
                crate::filesystem::verify_payload_file(&file).map_err(display)?;
                let observed =
                    zup_core::hash_reader((&mut file).take(expected.size.saturating_add(1)))
                        .map_err(display)?;
                file.verify().map_err(display)?;
                let (volume, id) = crate::filesystem::identity(&file).map_err(display)?;
                let object = NativeObjectIdentity::Windows { volume, file: id };
                if self.file_identity(name, access)? != object {
                    return Err("generation member changed during verification".into());
                }
                (observed, object)
            }
            Self::Machine(directory) => {
                let mut file = directory
                    .open_generation_file(name, access)
                    .map_err(display)?;
                crate::filesystem::verify_payload_file(&file).map_err(display)?;
                let observed =
                    zup_core::hash_reader((&mut file).take(expected.size.saturating_add(1)))
                        .map_err(display)?;
                let (volume, id) = crate::filesystem::identity(&file).map_err(display)?;
                let object = NativeObjectIdentity::Windows { volume, file: id };
                if self.file_identity(name, access)? != object {
                    return Err("generation member changed during verification".into());
                }
                (observed, object)
            }
        };
        if observed != (expected.size, expected.sha256) {
            return Err("generation member differs from authenticated content".into());
        }
        Ok(object)
    }

    pub(crate) fn file_identity(
        &self,
        name: &str,
        access: GenerationAccess,
    ) -> Result<NativeObjectIdentity, String> {
        let (volume, file) = match self {
            Self::User(directory) => {
                crate::filesystem::identity(&directory.open_file(name).map_err(display)?)
                    .map_err(display)?
            }
            Self::Machine(directory) => crate::filesystem::identity(
                &directory
                    .open_generation_file(name, access)
                    .map_err(display)?,
            )
            .map_err(display)?,
        };
        Ok(NativeObjectIdentity::Windows { volume, file })
    }

    fn write_file(
        &self,
        name: &str,
        reader: &mut dyn Read,
        expected: &GenerationFile,
    ) -> Result<(), String> {
        match self {
            Self::User(directory) => {
                let mut output = directory
                    .atomic_with_policy(name, Durability::Full, Publication::CreateNew)
                    .map_err(display)?;
                let mut hash = sha2::Sha256::default();
                use sha2::Digest;
                let mut size = 0u64;
                let mut buffer = [0u8; 64 * 1024];
                let mut bounded = reader.take(expected.size.saturating_add(1));
                loop {
                    let count = bounded.read(&mut buffer).map_err(display)?;
                    if count == 0 {
                        break;
                    }
                    output.write_all(&buffer[..count]).map_err(display)?;
                    hash.update(&buffer[..count]);
                    size += count as u64;
                }
                if size != expected.size
                    || zup_core::Sha256Digest::from_hasher(hash) != expected.sha256
                {
                    return Err("payload differs from authenticated content".into());
                }
                output.commit().map_err(display)?;
            }
            Self::Machine(directory) => directory
                .stage_generation_file(name, reader, expected.size, expected.sha256)
                .map_err(display)?,
        }
        self.verify_file(name, expected, GenerationAccess::Private)
            .map(|_| ())
    }
}

pub(crate) struct WindowsGenerationFilesystem<P> {
    state: crate::machine_state::StateAuthority,
    payload: PhantomData<fn() -> P>,
}

impl<P> WindowsGenerationFilesystem<P> {
    pub fn new(state: crate::machine_state::StateAuthority) -> Self {
        Self {
            state,
            payload: PhantomData,
        }
    }

    fn metadata(
        &self,
        intent: &GenerationIntent,
        create: bool,
    ) -> Result<Option<GenerationDirectory>, String> {
        if intent.privilege
            != match self.state.scope {
                zup_core::SelectedScope::User => Privilege::User,
                zup_core::SelectedScope::Machine => Privilege::System,
            }
        {
            return Err("generation scope differs from the state authority".into());
        }
        match intent.privilege {
            Privilege::User => {
                let root = PrivateDir::open(&self.state.root).map_err(display)?;
                let directory = if create {
                    root.ensure_dir_all(format!(
                        "materializations/{}",
                        intent.materialization.installation()
                    ))
                } else {
                    root.open_dir(format!(
                        "materializations/{}",
                        intent.materialization.installation()
                    ))
                };
                match directory {
                    Ok(directory) => Ok(Some(GenerationDirectory::User(directory))),
                    Err(private_fs::Error::Io(error))
                        if !create && error.kind() == std::io::ErrorKind::NotFound =>
                    {
                        Ok(None)
                    }
                    Err(error) => Err(display(error)),
                }
            }
            Privilege::System => match self.machine()?.state_directory(
                zup_transaction::StateNamespace::Materializations(
                    intent.materialization.installation(),
                ),
                create,
            ) {
                Ok(directory) => Ok(Some(GenerationDirectory::Machine(directory))),
                Err(crate::filesystem::FilesystemError::Io { source, .. })
                    if !create && source.kind() == std::io::ErrorKind::NotFound =>
                {
                    Ok(None)
                }
                Err(error) => Err(display(error)),
            },
        }
    }

    fn machine(&self) -> Result<MachineRoot, String> {
        self.state.machine().map_err(display)
    }
}

impl<P: PayloadSource> GenerationFilesystem for WindowsGenerationFilesystem<P> {
    type Directory = GenerationDirectory;
    type Payload = P;
    type Error = String;

    fn refused(intent: &GenerationIntent, reason: &str) -> String {
        format!(
            "generation `{}`: {reason}",
            intent.materialization.directory()
        )
    }

    fn namespace(
        &self,
        intent: &GenerationIntent,
        create: bool,
    ) -> Result<Option<GenerationDirectory>, String> {
        let directory = intent.materialization.directory();
        let root = directory
            .parent()
            .and_then(|path| path.parent())
            .and_then(|path| path.parent())
            .ok_or("materialization has no installation namespace")?;
        let expected = root
            .join(format!(
                "{}/{}",
                intent.publication.namespace(),
                intent.materialization.id()
            ))
            .map_err(display)?;
        if !directory.equivalent(&expected) {
            return Err("materialization is outside its installation namespace".into());
        }
        let path = crate::lowering::host_path(&root);
        let _state_boundary = self.state.payload_directory(&path).map_err(display)?;
        // Retained ancestors deny delete sharing while the private namespace is opened.
        let _parent_authority = if intent.privilege == Privilege::User {
            use crate::filesystem::authority::{DirectoryAccess, FileAuthority, FileParent};
            let user = crate::transport::UserSid::current().map_err(display)?;
            match FileParent::open(
                &path,
                FileAuthority::User(user),
                if create {
                    DirectoryAccess::PrepareFiles
                } else {
                    DirectoryAccess::Read
                },
            ) {
                Ok(authority) => Some(authority),
                Err(crate::filesystem::FilesystemError::Io { source, .. })
                    if !create && source.kind() == std::io::ErrorKind::NotFound =>
                {
                    return Ok(None);
                }
                Err(error) => return Err(display(error)),
            }
        } else {
            None
        };
        let mut parent = match intent.privilege {
            Privilege::User => match if create {
                PrivateDir::ensure(path.join(".zup"))
            } else {
                PrivateDir::open(path.join(".zup"))
            } {
                Ok(directory) => GenerationDirectory::User(directory),
                Err(private_fs::Error::Io(error))
                    if !create && error.kind() == std::io::ErrorKind::NotFound =>
                {
                    return Ok(None);
                }
                Err(error) => return Err(display(error)),
            },
            Privilege::System => match MachineDir::payload_root(&path, create) {
                Ok(directory) => GenerationDirectory::Machine(directory),
                Err(crate::filesystem::FilesystemError::Io { source, .. })
                    if !create && source.kind() == std::io::ErrorKind::NotFound =>
                {
                    return Ok(None);
                }
                Err(error) => return Err(display(error)),
            },
        };
        for component in intent.publication.namespace().split('/') {
            if intent.privilege == Privilege::User && component == ".zup" {
                continue;
            }
            parent = match self.open_child(&parent, component, GenerationAccess::Public)? {
                Some(directory) => directory,
                None if create => parent.child(component, true, GenerationAccess::Public)?,
                None => return Ok(None),
            };
        }
        Ok(Some(parent))
    }

    fn open_child(
        &self,
        parent: &GenerationDirectory,
        name: &str,
        access: GenerationAccess,
    ) -> Result<Option<GenerationDirectory>, String> {
        if !parent.names()?.iter().any(|entry| entry == name) {
            return Ok(None);
        }
        parent.child(name, false, access).map(Some)
    }

    fn create_child(
        &self,
        parent: &GenerationDirectory,
        name: &str,
        _intent: &GenerationIntent,
    ) -> Result<GenerationDirectory, String> {
        let directory = parent.child(name, true, GenerationAccess::Private)?;
        parent.sync()?;
        Ok(directory)
    }

    fn object(directory: &GenerationDirectory) -> Result<NativeObjectIdentity, String> {
        directory.identity()
    }

    fn populate(
        &self,
        root: &GenerationDirectory,
        intent: &GenerationIntent,
        payload: &P,
    ) -> Result<(), String> {
        for file in intent.materialization.descriptor().files() {
            let source = intent
                .sources
                .get(&file.path)
                .ok_or("generation member has no authenticated source")?;
            let mut components = file.path.as_str().split('/').collect::<Vec<_>>();
            let name = components.pop().ok_or("empty generation member")?;
            let mut directories = Vec::new();
            for component in components {
                let parent = directories.last().unwrap_or(root);
                let directory =
                    match self.open_child(parent, component, GenerationAccess::Private)? {
                        Some(directory) => directory,
                        None => self.create_child(parent, component, intent)?,
                    };
                directories.push(directory);
            }
            let parent = directories.last().unwrap_or(root);
            if parent.names()?.iter().any(|entry| entry == name) {
                parent.verify_file(name, file, GenerationAccess::Private)?;
                continue;
            }
            let mut reader = payload
                .open(&source.source_relative, &file.sha256, file.size)
                .map_err(display)?;
            parent.write_file(name, &mut reader, file)?;
        }
        Ok(())
    }

    fn verify_tree(
        &self,
        root: &GenerationDirectory,
        intent: &GenerationIntent,
        access: GenerationAccess,
    ) -> Result<MaterializationTree, String> {
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
        fn walk(
            directory: &GenerationDirectory,
            prefix: &str,
            expected: &BTreeMap<String, &GenerationFile>,
            directories: &BTreeSet<String>,
            volume: u64,
            tree: &mut MaterializationTree,
            access: GenerationAccess,
        ) -> Result<(), String> {
            let NativeObjectIdentity::Windows {
                volume: current, ..
            } = directory.identity()?
            else {
                return Err("non-Windows generation object".into());
            };
            if current != volume {
                return Err("generation crosses a volume boundary".into());
            }
            for name in directory.names()? {
                let path = if prefix.is_empty() {
                    name.clone()
                } else {
                    format!("{prefix}/{name}")
                };
                if directories.contains(&path) {
                    let child = directory.child(&name, false, access)?;
                    let object = child.identity()?;
                    tree.directories
                        .insert(zup_core::RelativePath::new(&path).map_err(display)?, object);
                    walk(&child, &path, expected, directories, volume, tree, access)?;
                    if directory.child(&name, false, access)?.identity()? != object {
                        return Err("generation directory changed during verification".into());
                    }
                } else if let Some(file) = expected.get(&path) {
                    let object = directory.verify_file(&name, file, access)?;
                    tree.files.insert(file.path.clone(), object);
                } else {
                    return Err("generation contains an undeclared or unsafe entry".into());
                }
            }
            directory.sync()
        }
        let NativeObjectIdentity::Windows { volume, .. } = root.identity()? else {
            return Err("non-Windows generation object".into());
        };
        let mut tree = MaterializationTree {
            root: root.identity()?,
            files: BTreeMap::new(),
            directories: BTreeMap::new(),
        };
        walk(root, "", &expected, &directories, volume, &mut tree, access)?;
        tree.validate(intent.materialization.descriptor())?;
        Ok(tree)
    }

    fn publish_access(
        &self,
        root: GenerationDirectory,
        intent: &GenerationIntent,
        tree: &MaterializationTree,
    ) -> Result<GenerationDirectory, String> {
        tree.validate(intent.materialization.descriptor())?;
        if root.identity()? != tree.root {
            return Err("generation root identity changed before access publication".into());
        }
        fn parents(
            root: &GenerationDirectory,
            path: &str,
            tree: &MaterializationTree,
        ) -> Result<(Vec<GenerationDirectory>, String), String> {
            let mut parts = path.split('/').collect::<Vec<_>>();
            let leaf = parts.pop().ok_or("empty generation member")?.to_owned();
            let mut retained = Vec::new();
            let mut prefix = String::new();
            for component in parts {
                if !prefix.is_empty() {
                    prefix.push('/');
                }
                prefix.push_str(component);
                let parent = retained.last().unwrap_or(root);
                let directory = parent.child(component, false, GenerationAccess::Publishing)?;
                let key = zup_core::RelativePath::new(&prefix).map_err(display)?;
                if tree.directories.get(&key) != Some(&directory.identity()?) {
                    return Err("generation parent changed during access publication".into());
                }
                retained.push(directory);
            }
            Ok((retained, leaf))
        }
        for (path, expected) in &tree.files {
            let (retained, name) = parents(&root, path.as_str(), tree)?;
            let parent = retained.last().unwrap_or(&root);
            if let GenerationDirectory::Machine(directory) = parent {
                let NativeObjectIdentity::Windows { volume, file } = expected else {
                    return Err("non-Windows generation file identity".into());
                };
                directory
                    .expose_generation_file(&name, (*volume, *file))
                    .map_err(display)?;
                zup_transaction::fault_injection::boundary(
                    zup_transaction::fault_injection::Boundary::MaterializationMemberExposed,
                    intent
                        .operation_id(zup_plan::GenerationOperation::Seal)
                        .as_str(),
                );
            }
        }
        for (path, expected) in tree.directories.iter().rev() {
            let (retained, name) = parents(&root, path.as_str(), tree)?;
            let parent = retained.last().unwrap_or(&root);
            let directory = parent.child(&name, false, GenerationAccess::Publishing)?;
            if directory.identity()? != *expected {
                return Err("generation directory changed during access publication".into());
            }
            if let GenerationDirectory::Machine(directory) = directory {
                let NativeObjectIdentity::Windows { volume, file } = expected else {
                    return Err("non-Windows generation directory identity".into());
                };
                directory
                    .expose_generation_directory((*volume, *file))
                    .map_err(display)?;
            }
        }
        match root {
            GenerationDirectory::User(directory) => Ok(GenerationDirectory::User(directory)),
            GenerationDirectory::Machine(directory) => {
                let NativeObjectIdentity::Windows { volume, file } = tree.root else {
                    return Err("non-Windows generation root identity".into());
                };
                directory
                    .expose_generation_directory((volume, file))
                    .map(GenerationDirectory::Machine)
                    .map_err(display)
            }
        }
    }

    fn rename_noreplace(
        &self,
        parent: &GenerationDirectory,
        source: &str,
        destination: &str,
        expected: NativeObjectIdentity,
    ) -> Result<(), String> {
        let NativeObjectIdentity::Windows { volume, file } = expected else {
            return Err("non-Windows materialization identity".into());
        };
        match parent {
            GenerationDirectory::User(directory) => crate::filesystem::rename_private_directory(
                directory,
                source,
                directory,
                destination,
                (volume, file),
            )
            .map_err(display),
            GenerationDirectory::Machine(directory) => directory
                .rename_directory(source, directory, destination, (volume, file))
                .map_err(display),
        }
    }

    fn sync(directory: &GenerationDirectory) -> Result<(), String> {
        directory.sync()
    }

    fn read_record(
        &self,
        intent: &GenerationIntent,
    ) -> Result<Option<MaterializationRecord>, String> {
        let Some(directory) = self.metadata(intent, false)? else {
            return Ok(None);
        };
        let name = format!("{}.json", intent.materialization.id());
        if !directory.names()?.iter().any(|entry| entry == &name) {
            return Ok(None);
        }
        let record = match directory {
            GenerationDirectory::User(directory) => {
                serde_json::from_reader(directory.open_file(&name).map_err(display)?)
                    .map_err(display)?
            }
            GenerationDirectory::Machine(directory) => serde_json::from_slice(
                &directory
                    .read(&name, PolicyKind::Administrators)
                    .map_err(display)?
                    .ok_or("materialization metadata disappeared")?,
            )
            .map_err(display)?,
        };
        Ok(Some(record))
    }

    fn write_record(
        &self,
        intent: &GenerationIntent,
        record: &MaterializationRecord,
        create_new: bool,
    ) -> Result<(), String> {
        let directory = self
            .metadata(intent, create_new)?
            .ok_or("materialization metadata namespace is missing")?;
        let name = format!("{}.json", record.specification.id());
        let bytes = serde_json::to_vec(record).map_err(display)?;
        match directory {
            GenerationDirectory::User(directory) => {
                let mut output = directory
                    .atomic_with_policy(
                        &name,
                        Durability::Full,
                        if create_new {
                            Publication::CreateNew
                        } else {
                            Publication::Replace
                        },
                    )
                    .map_err(display)?;
                output.write_all(&bytes).map_err(display)?;
                output.commit().map_err(display)?;
                Ok(())
            }
            GenerationDirectory::Machine(directory) => directory
                .publish(&name, &bytes, create_new, PolicyKind::Administrators)
                .map_err(display),
        }
    }

    fn origin(&self, transaction: TransactionId) -> Result<TransactionRecord, String> {
        self.state.source(transaction)
    }
}

fn display(error: impl std::fmt::Display) -> String {
    error.to_string()
}
