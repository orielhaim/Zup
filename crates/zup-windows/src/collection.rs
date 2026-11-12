use crate::filesystem::authority::{DirectoryAccess, ExclusiveFile, FileAuthority, FileParent};
use crate::generation::{GenerationDirectory, WindowsGenerationFilesystem};
use private_fs::{Durability, PrivateDir, Publication};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::path::Path;
use zup_core::MaterializationTree;
use zup_core::{NativeObjectIdentity, RelativePath, SelectedScope, Sha256Digest, TransactionId};
use zup_plan::GenerationCleanup;
use zup_transaction::{
    CollectionFence, CollectionState, GenerationCollection, GenerationCollector,
    GenerationFilesystem,
};

pub(crate) struct WindowsCollector {
    pub state: crate::machine_state::StateAuthority,
}
pub(crate) struct Fences(Vec<(RelativePath, FileParent, ExclusiveFile)>);
fn display(error: impl std::fmt::Display) -> String {
    error.to_string()
}

impl WindowsCollector {
    fn machine(&self) -> Result<crate::machine_authority::MachineRoot, String> {
        self.state.machine().map_err(display)
    }
    fn metadata(&self, create: bool) -> Result<Option<GenerationDirectory>, String> {
        match self.state.scope {
            SelectedScope::User => {
                let root = PrivateDir::open(&self.state.root).map_err(display)?;
                match if create {
                    root.create_dir("collections")
                } else {
                    root.open_dir("collections")
                } {
                    Ok(directory) => Ok(Some(GenerationDirectory::User(directory))),
                    Err(private_fs::Error::Io(error))
                        if create && error.kind() == std::io::ErrorKind::AlreadyExists =>
                    {
                        root.open_dir("collections")
                            .map(GenerationDirectory::User)
                            .map(Some)
                            .map_err(display)
                    }
                    Err(private_fs::Error::Io(error))
                        if !create && error.kind() == std::io::ErrorKind::NotFound =>
                    {
                        Ok(None)
                    }
                    Err(error) => Err(display(error)),
                }
            }
            SelectedScope::Machine => match self
                .machine()?
                .state_directory(zup_transaction::StateNamespace::Collections, create)
            {
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
    fn origin(&self, generation: &GenerationCleanup) -> Result<zup_plan::GenerationIntent, String> {
        let record = self.state.source(generation.creator)?;
        record
            .plan
            .nodes
            .into_iter()
            .find_map(|node| match node.kind {
                zup_plan::NodeKind::Generation {
                    operation: zup_plan::GenerationOperation::Materialize,
                    intent,
                } if intent.materialization == generation.materialization => Some(*intent),
                _ => None,
            })
            .ok_or("generation creation intent is missing".into())
    }
    fn generation_root(
        &self,
        generation: &GenerationCleanup,
    ) -> Result<Option<GenerationDirectory>, String> {
        let intent = self.origin(generation)?;
        let fs = WindowsGenerationFilesystem::<zup_bundle::DirectoryPayloadSource>::new(
            self.state.clone(),
        );
        let record = fs
            .read_record(&intent)?
            .ok_or("generation metadata is missing")?;
        if record.transaction != generation.creator
            || record.object != generation.tree.root
            || record.specification != generation.materialization
            || !matches!(&record.state, zup_transaction::MaterializationState::Sealed { tree } if tree == &generation.tree)
            || record.fingerprint()? != generation.seal
        {
            return Err("generation seal differs from protected collection intent".into());
        }
        let parent = fs
            .namespace(&intent, false)?
            .ok_or("generation namespace is missing")?;
        if parent.identity()? != record.namespace {
            return Err("generation namespace identity changed".into());
        }
        let root = fs.open_child(
            &parent,
            &generation.materialization.id().to_string(),
            zup_transaction::GenerationAccess::Public,
        )?;
        if let Some(root) = &root
            && root.identity()? != generation.tree.root
        {
            return Err("generation root native identity changed".into());
        }
        Ok(root)
    }
    fn parent(&self, path: &Path) -> Result<FileParent, String> {
        let authority = match self.state.scope {
            SelectedScope::User => {
                FileAuthority::User(crate::transport::UserSid::current().map_err(display)?)
            }
            SelectedScope::Machine => FileAuthority::Machine,
        };
        FileParent::open(
            path.parent().ok_or("collection member has no parent")?,
            authority,
            DirectoryAccess::CreateFiles,
        )
        .map_err(display)
    }
}
impl GenerationCollector for WindowsCollector {
    type Coordinator = std::fs::File;
    type Fence = Fences;
    fn coordinate(&self, generation: &GenerationCleanup) -> Result<Self::Coordinator, String> {
        let intent = self.origin(generation)?;
        WindowsGenerationFilesystem::<zup_bundle::DirectoryPayloadSource>::new(self.state.clone())
            .lock(&intent)
    }
    fn load(
        &self,
        transaction: TransactionId,
        intent: Sha256Digest,
    ) -> Result<Option<GenerationCollection>, String> {
        let Some(directory) = self.metadata(false)? else {
            return Ok(None);
        };
        let name = format!("{transaction}-{intent}.json");
        if !directory.names()?.contains(&name) {
            return Ok(None);
        }
        let bytes = match directory {
            GenerationDirectory::User(directory) => {
                let mut bytes = Vec::new();
                directory
                    .open_file(&name)
                    .map_err(display)?
                    .take(16 * 1024 * 1024 + 1)
                    .read_to_end(&mut bytes)
                    .map_err(display)?;
                if bytes.len() > 16 * 1024 * 1024 {
                    return Err("collection metadata exceeds its limit".into());
                }
                bytes
            }
            GenerationDirectory::Machine(directory) => directory
                .read_bounded(
                    &name,
                    crate::filesystem::PolicyKind::Administrators,
                    16 * 1024 * 1024,
                )
                .map_err(display)?
                .ok_or("collection evidence disappeared")?,
        };
        serde_json::from_slice(&bytes).map(Some).map_err(display)
    }
    fn save(&self, record: &GenerationCollection, create: bool) -> Result<(), String> {
        let directory = self
            .metadata(true)?
            .ok_or("collection namespace unavailable")?;
        let name = format!("{}-{}.json", record.transaction, record.intent);
        let bytes = serde_json::to_vec(record).map_err(display)?;
        match directory {
            GenerationDirectory::User(directory) => {
                let mut output = directory
                    .atomic_with_policy(
                        &name,
                        Durability::Full,
                        if create {
                            Publication::CreateNew
                        } else {
                            Publication::Replace
                        },
                    )
                    .map_err(display)?;
                output.write_all(&bytes).map_err(display)?;
                output.commit().map_err(display)?;
            }
            GenerationDirectory::Machine(directory) => directory
                .publish(
                    &name,
                    &bytes,
                    create,
                    crate::filesystem::PolicyKind::Administrators,
                )
                .map_err(display)?,
        }
        if self.load(record.transaction, record.intent)?.as_ref() != Some(record) {
            return Err("collection evidence readback differs".into());
        }
        Ok(())
    }
    fn inspect(
        &self,
        generation: &GenerationCleanup,
        previous: Option<&GenerationCollection>,
    ) -> Result<Option<MaterializationTree>, String> {
        let Some(root) = self.generation_root(generation)? else {
            return Ok(None);
        };
        let expected = generation
            .materialization
            .descriptor()
            .files()
            .iter()
            .map(|file| (file.path.as_str(), file))
            .collect::<BTreeMap<_, _>>();
        let mut directories = BTreeSet::new();
        for path in expected.keys() {
            for (index, _) in path.match_indices('/') {
                directories.insert(&path[..index]);
            }
        }
        let partial = previous.is_some_and(|record| record.state == CollectionState::Deleting);
        let mut tree = MaterializationTree {
            root: root.identity()?,
            files: BTreeMap::new(),
            directories: BTreeMap::new(),
        };
        let mut pending = vec![(String::new(), root)];
        while let Some((prefix, directory)) = pending.pop() {
            for name in directory.names()? {
                let path = if prefix.is_empty() {
                    name.clone()
                } else {
                    format!("{prefix}/{name}")
                };
                let relative = RelativePath::new(&path).map_err(display)?;
                let object = if directories.contains(path.as_str()) {
                    let child =
                        directory.child(&name, false, zup_transaction::GenerationAccess::Public)?;
                    let object = child.identity()?;
                    if let Some(previous) = previous
                        && previous.tree.directories.get(&relative) != Some(&object)
                    {
                        return Err("generation directory was substituted during collection".into());
                    }
                    tree.directories.insert(relative, object);
                    pending.push((path, child));
                    object
                } else if let Some(expected) = expected.get(path.as_str()) {
                    let object = directory.verify_file(
                        &name,
                        expected,
                        zup_transaction::GenerationAccess::Public,
                    )?;
                    if let Some(previous) = previous
                        && previous.tree.files.get(&relative) != Some(&object)
                    {
                        return Err("generation member was substituted during collection".into());
                    }
                    tree.files.insert(relative, object);
                    object
                } else {
                    return Err("generation contains undeclared collection entries".into());
                };
                if !matches!((generation.tree.root, object), (NativeObjectIdentity::Windows { volume: root, .. }, NativeObjectIdentity::Windows { volume, .. }) if root == volume)
                {
                    return Err("generation collection crosses volume boundaries".into());
                }
            }
        }
        if !partial
            && (tree.files.len() != expected.len() || tree.directories.len() != directories.len())
        {
            return Err("generation is incomplete before collection intent".into());
        }
        Ok(Some(tree))
    }
    fn fence(
        &self,
        generation: &GenerationCleanup,
        tree: &MaterializationTree,
    ) -> Result<CollectionFence<Fences>, String> {
        let root = crate::lowering::host_path(generation.materialization.directory());
        let mut fences = Vec::new();
        for file in generation
            .materialization
            .descriptor()
            .files()
            .iter()
            .filter(|file| tree.files.contains_key(&file.path))
        {
            let path = root.join(file.path.as_str());
            let parent = self.parent(&path)?;
            parent
                .verify_ancestry(&tree.parent_chain(&file.path)?)
                .map_err(display)?;
            if !parent.ntfs().map_err(display)? {
                return Ok(CollectionFence::Deferred(
                    zup_transaction::CleanupDeferral::UnsupportedFilesystem,
                ));
            }
            let fence = match parent.fence_owned_file(
                path.file_name().ok_or("collection member has no name")?,
                tree.files[&file.path],
                file.size,
                file.sha256,
            ) {
                Ok(fence) => fence,
                Err(error) if error.sharing_violation() => {
                    return Ok(CollectionFence::Deferred(
                        zup_transaction::CleanupDeferral::InUse,
                    ));
                }
                Err(error) => return Err(display(error)),
            };
            if file.executable && !fence.native_image().map_err(display)? {
                return Err(
                    "native-executable reader contract includes a script or unsupported executable"
                        .into(),
                );
            }
            fences.push((file.path.clone(), parent, fence));
        }
        Ok(CollectionFence::Ready(Fences(fences)))
    }
    fn remove(
        &self,
        generation: &GenerationCleanup,
        tree: &MaterializationTree,
        mut fences: Fences,
    ) -> Result<(), String> {
        let root = crate::lowering::host_path(generation.materialization.directory());
        fences.0.sort_by_key(|(path, ..)| {
            !generation
                .materialization
                .descriptor()
                .files()
                .iter()
                .any(|file| &file.path == path && file.executable)
        });
        for (_, parent, file) in fences.0 {
            parent.discard_fenced_file(file).map_err(display)?;
            zup_transaction::fault_injection::boundary(
                zup_transaction::fault_injection::Boundary::CollectionMemberRemoved,
                &format!("collect:generation:{}", generation.materialization.id()),
            );
        }
        let mut directories = tree.directories.keys().collect::<Vec<_>>();
        directories.sort_by_key(|path| std::cmp::Reverse(path.as_str().matches('/').count()));
        for relative in directories {
            let path = root.join(relative.as_str());
            let parent = self.parent(&path)?;
            parent
                .verify_ancestry(&tree.parent_chain(relative)?)
                .map_err(display)?;
            parent
                .remove_empty_directory(
                    path.file_name().ok_or("collection directory has no name")?,
                    tree.directories[relative],
                )
                .map_err(display)?;
        }
        self.parent(&root)?
            .remove_empty_directory(
                root.file_name().ok_or("generation root has no name")?,
                tree.root,
            )
            .map_err(display)?;
        Ok(())
    }
}
