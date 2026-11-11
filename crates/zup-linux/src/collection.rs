use crate::fs::{EntryKind, OwnedDirectory};
use rustix::fs::{Mode, OFlags};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use zup_core::MaterializationTree;
use zup_core::{NativeObjectIdentity, RelativePath, Sha256Digest, TransactionId};
use zup_plan::GenerationCleanup;
use zup_transaction::{
    CollectionFence, CollectionState, GenerationCollection, GenerationCollector,
    GenerationFilesystem,
};

pub(crate) struct LinuxCollector(pub crate::state::StateAuthority);
pub(crate) struct Fences(Vec<rustix::fd::OwnedFd>);

fn display(error: impl std::fmt::Display) -> String {
    error.to_string()
}
fn identity(directory: &OwnedDirectory) -> Result<NativeObjectIdentity, String> {
    let stat = directory.stat_self().map_err(display)?;
    Ok(NativeObjectIdentity::Linux {
        device: stat.dev,
        inode: stat.ino,
    })
}
fn member(
    root: &OwnedDirectory,
    path: &str,
    tree: &MaterializationTree,
) -> Result<(OwnedDirectory, String), String> {
    let mut parts = path.split('/').collect::<Vec<_>>();
    let name = parts.pop().ok_or("empty generation member")?.to_owned();
    let mut parent = root.duplicate().map_err(display)?;
    if identity(&parent)? != tree.root {
        return Err("generation root changed before member access".into());
    }
    let mut prefix = String::new();
    for part in parts {
        if !prefix.is_empty() {
            prefix.push('/');
        }
        prefix.push_str(part);
        parent = parent.open_child(part).map_err(display)?;
        let relative = RelativePath::new(&prefix).map_err(display)?;
        if tree.directories.get(&relative) != Some(&identity(&parent)?) {
            return Err("generation directory changed before member access".into());
        }
    }
    Ok((parent, name))
}
impl LinuxCollector {
    fn metadata(&self, create: bool) -> Result<Option<OwnedDirectory>, String> {
        let root = OwnedDirectory::open(&self.0.root).map_err(display)?;
        root.verify_descendants(Path::new("."), rustix::process::geteuid().as_raw())
            .map_err(display)?;
        if root
            .kind_or_absent("collections")
            .map_err(display)?
            .is_none()
        {
            return if create {
                root.create_exclusive_directory("collections", Mode::from_bits_truncate(0o700))
                    .map(Some)
                    .map_err(display)
            } else {
                Ok(None)
            };
        }
        let directory = root.open_child("collections").map_err(display)?;
        let stat = directory.stat_self().map_err(display)?;
        if stat.uid != rustix::process::geteuid().as_raw() || stat.mode != 0o700 {
            return Err("collection metadata permissions changed".into());
        }
        Ok(Some(directory))
    }
    fn root(&self, generation: &GenerationCleanup) -> Result<Option<OwnedDirectory>, String> {
        let path = crate::lowering::to_host_path(generation.materialization.directory())
            .map_err(display)?;
        crate::fs::verify_ancestor_trust(
            path.parent().ok_or("generation has no namespace")?,
            rustix::process::geteuid().as_raw(),
        )
        .map_err(display)?;
        let (parent, name) = crate::fs::open_parent(&path).map_err(display)?;
        if parent.kind_or_absent(&name).map_err(display)?.is_none() {
            return Ok(None);
        }
        let directory = parent.open_child(&name).map_err(display)?;
        if identity(&directory)? != generation.tree.root {
            return Err("generation root native identity changed".into());
        }
        directory
            .verify_descendants(Path::new("."), rustix::process::geteuid().as_raw())
            .map_err(display)?;
        Ok(Some(directory))
    }
    fn verify_origin(&self, generation: &GenerationCleanup) -> Result<(), String> {
        let journal = self.0.source(generation.creator).map_err(display)?;
        let intent = journal
            .plan
            .nodes
            .iter()
            .find_map(|node| match &node.kind {
                zup_plan::NodeKind::Generation {
                    operation: zup_plan::GenerationOperation::Materialize,
                    intent,
                } if intent.materialization == generation.materialization => Some(intent.as_ref()),
                _ => None,
            })
            .ok_or("generation creation intent is missing")?;
        let record = crate::generation::LinuxGenerationFilesystem::new(self.0.clone())
            .read_record(intent)
            .map_err(display)?
            .ok_or("generation metadata is missing")?;
        if record.transaction != generation.creator
            || record.object != generation.tree.root
            || record.specification != generation.materialization
            || !matches!(&record.state, zup_transaction::MaterializationState::Sealed { tree } if tree == &generation.tree)
            || record.fingerprint()? != generation.seal
        {
            return Err("generation seal differs from protected collection intent".into());
        }
        Ok(())
    }
}
impl GenerationCollector for LinuxCollector {
    type Fence = Fences;
    fn load(
        &self,
        transaction: TransactionId,
        intent: Sha256Digest,
    ) -> Result<Option<GenerationCollection>, String> {
        let Some(directory) = self.metadata(false)? else {
            return Ok(None);
        };
        let name = format!("{transaction}-{intent}.json");
        if directory.kind_or_absent(&name).map_err(display)?.is_none() {
            return Ok(None);
        }
        let mut file = directory.open_regular_read(&name).map_err(display)?;
        let stat = rustix::fs::fstat(&file).map_err(display)?;
        if stat.st_uid != rustix::process::geteuid().as_raw()
            || stat.st_mode & 0o7777 != 0o600
            || stat.st_nlink != 1
        {
            return Err("collection metadata has unsafe authority".into());
        }
        let mut bytes = Vec::new();
        (&mut file)
            .take(16 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map_err(display)?;
        if bytes.len() > 16 * 1024 * 1024 {
            return Err("collection metadata exceeds its limit".into());
        }
        serde_json::from_slice(&bytes).map(Some).map_err(display)
    }
    fn save(&self, record: &GenerationCollection, create: bool) -> Result<(), String> {
        let directory = self
            .metadata(true)?
            .ok_or("collection metadata namespace unavailable")?;
        let bytes = serde_json::to_vec(record).map_err(display)?;
        let name = format!("{}-{}.json", record.transaction, record.intent);
        if create {
            directory
                .create_durable_exclusive(&name, &bytes, Mode::from_bits_truncate(0o600))
                .map_err(display)?;
        } else {
            directory
                .write_durable(&name, &bytes, Mode::from_bits_truncate(0o600))
                .map_err(display)?;
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
        self.verify_origin(generation)?;
        let Some(root) = self.root(generation)? else {
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
            root: identity(&root)?,
            files: BTreeMap::new(),
            directories: BTreeMap::new(),
        };
        let mut pending = vec![(String::new(), root)];
        while let Some((prefix, directory)) = pending.pop() {
            for name in directory.child_names().map_err(display)? {
                let path = if prefix.is_empty() {
                    name.clone()
                } else {
                    format!("{prefix}/{name}")
                };
                let relative = RelativePath::new(&path).map_err(display)?;
                let stat = directory.stat_child(&name).map_err(display)?;
                match stat.kind {
                    EntryKind::Directory if directories.contains(path.as_str()) => {
                        let child = directory.open_child(&name).map_err(display)?;
                        child
                            .verify_descendants(Path::new("."), rustix::process::geteuid().as_raw())
                            .map_err(display)?;
                        let object = identity(&child)?;
                        if let Some(previous) = previous
                            && previous.tree.directories.get(&relative) != Some(&object)
                        {
                            return Err(
                                "generation directory was substituted during collection".into()
                            );
                        }
                        tree.directories.insert(relative, object);
                        pending.push((path, child));
                    }
                    EntryKind::Regular if expected.contains_key(path.as_str()) => {
                        let expected = expected[path.as_str()];
                        let observed = directory.stream_regular(&name).map_err(display)?;
                        let mode = if expected.executable { 0o555 } else { 0o444 };
                        if observed.size != expected.size
                            || observed.sha256 != expected.sha256
                            || observed.uid != rustix::process::geteuid().as_raw()
                            || observed.mode != mode
                                && !(previous.is_some()
                                    && expected.executable
                                    && observed.mode == mode | 0o200)
                        {
                            return Err("generation member content, permissions or ownership changed during collection".into());
                        }
                        let object = observed.object();
                        if let Some(previous) = previous
                            && previous.tree.files.get(&relative) != Some(&object)
                        {
                            return Err(
                                "generation member was substituted during collection".into()
                            );
                        }
                        tree.files.insert(relative, object);
                    }
                    _ => {
                        return Err(
                            "generation contains undeclared or unsafe collection entries".into(),
                        );
                    }
                }
                if stat.dev
                    != match generation.tree.root {
                        NativeObjectIdentity::Linux { device, .. } => device,
                        _ => return Err("non-Linux generation identity".into()),
                    }
                {
                    return Err("generation collection crosses filesystem boundaries".into());
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
        let root = self
            .root(generation)?
            .ok_or("generation disappeared before collection fencing")?;
        let filesystem = rustix::fs::fstatfs(root.as_fd()).map_err(display)?;
        if !matches!(
            filesystem.f_type as u64,
            0xef53 | 0x9123683e | 0x58465342 | 0x01021994
        ) {
            return Ok(CollectionFence::Deferred(
                zup_transaction::CleanupDeferral::UnsupportedFilesystem,
            ));
        }
        let mut fences = Vec::new();
        for expected in generation
            .materialization
            .descriptor()
            .files()
            .iter()
            .filter(|file| file.executable && tree.files.contains_key(&file.path))
        {
            let (parent, name) = member(&root, expected.path.as_str(), tree)?;
            let mut file = parent.open_regular_read(&name).map_err(display)?;
            let stat = rustix::fs::fstat(&file).map_err(display)?;
            if tree.files[&expected.path]
                != (NativeObjectIdentity::Linux {
                    device: stat.st_dev,
                    inode: stat.st_ino,
                })
            {
                return Err("generation image changed before fencing".into());
            }
            let mut header = [0u8; 20];
            file.read_exact(&mut header).map_err(display)?;
            if &header[..4] != b"\x7fELF"
                || header[5] != 1
                || !matches!(u16::from_le_bytes([header[16], header[17]]), 2 | 3)
            {
                return Err(
                    "native-executable reader contract includes a script or unsupported executable"
                        .into(),
                );
            }
            rustix::fs::fchmod(&file, Mode::from_bits_truncate(0o755)).map_err(display)?;
            zup_transaction::fault_injection::boundary(
                zup_transaction::fault_injection::Boundary::CollectionImageWritable,
                &format!("collect:generation:{}", generation.materialization.id()),
            );
            let writer = rustix::fs::openat(
                parent.as_fd(),
                &name,
                OFlags::WRONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
                Mode::empty(),
            );
            rustix::fs::fchmod(&file, Mode::from_bits_truncate(0o555)).map_err(display)?;
            file.sync_all().map_err(display)?;
            let writer = match writer {
                Ok(writer) => writer,
                Err(rustix::io::Errno::TXTBSY) => {
                    return Ok(CollectionFence::Deferred(
                        zup_transaction::CleanupDeferral::InUse,
                    ));
                }
                Err(error) => return Err(display(error)),
            };
            let after = rustix::fs::fstat(&writer).map_err(display)?;
            if (after.st_dev, after.st_ino) != (stat.st_dev, stat.st_ino) {
                return Err("generation image native identity changed while fencing".into());
            }
            file.seek(SeekFrom::Start(0)).map_err(display)?;
            if zup_core::hash_reader(file).map_err(display)? != (expected.size, expected.sha256) {
                return Err("generation image content changed while fencing".into());
            }
            fences.push(writer);
        }
        Ok(CollectionFence::Ready(Fences(fences)))
    }
    fn remove(
        &self,
        generation: &GenerationCleanup,
        tree: &MaterializationTree,
        fences: Fences,
    ) -> Result<(), String> {
        let root = self
            .root(generation)?
            .ok_or("generation disappeared before collection")?;
        for executable in [true, false] {
            for file in generation
                .materialization
                .descriptor()
                .files()
                .iter()
                .filter(|file| file.executable == executable && tree.files.contains_key(&file.path))
            {
                let (parent, name) = member(&root, file.path.as_str(), tree)?;
                let observed = parent.stream_regular(&name).map_err(display)?;
                if observed.object() != tree.files[&file.path]
                    || observed.size != file.size
                    || observed.sha256 != file.sha256
                {
                    return Err("generation file changed before deletion".into());
                }
                parent.remove_file(&name).map_err(display)?;
                parent.sync().map_err(display)?;
                zup_transaction::fault_injection::boundary(
                    zup_transaction::fault_injection::Boundary::CollectionMemberRemoved,
                    &format!("collect:generation:{}", generation.materialization.id()),
                );
            }
        }
        let mut directories = tree.directories.keys().collect::<Vec<_>>();
        directories.sort_by_key(|path| std::cmp::Reverse(path.as_str().matches('/').count()));
        for path in directories {
            let (parent, name) = member(&root, path.as_str(), tree)?;
            if identity(&parent.open_child(&name).map_err(display)?)? != tree.directories[path] {
                return Err("generation directory changed before deletion".into());
            }
            parent.remove_empty_directory(&name).map_err(display)?;
            parent.sync().map_err(display)?;
        }
        let host = crate::lowering::to_host_path(generation.materialization.directory())
            .map_err(display)?;
        let (parent, name) = crate::fs::open_parent(&host).map_err(display)?;
        if identity(&parent.open_child(&name).map_err(display)?)? != tree.root {
            return Err("generation root changed before deletion".into());
        }
        parent.remove_empty_directory(&name).map_err(display)?;
        parent.sync().map_err(display)?;
        drop(fences.0);
        Ok(())
    }
}
