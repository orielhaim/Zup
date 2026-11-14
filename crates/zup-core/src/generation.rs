use crate::{InstallationId, MaterializationId};
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{
    PayloadPlacement, PayloadRequirement, RelativePath, Sha256Digest, Target,
    TargetOperatingSystem, TargetPath, TargetPathError,
};

const GENERATION_SCHEMA: u32 = 1;
pub const DEPLOYMENTS_DIRECTORY: &str = ".zup/deployments";
pub const MAINTENANCE_DEPLOYMENTS_DIRECTORY: &str = ".zup/maintenance";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaterializationTree {
    pub root: crate::NativeObjectIdentity,
    pub files: BTreeMap<RelativePath, crate::NativeObjectIdentity>,
    pub directories: BTreeMap<RelativePath, crate::NativeObjectIdentity>,
}

impl MaterializationTree {
    pub fn parent_chain(
        &self,
        member: &RelativePath,
    ) -> Result<Vec<crate::NativeObjectIdentity>, String> {
        if !self.files.contains_key(member) && !self.directories.contains_key(member) {
            return Err("native tree does not own this member".into());
        }
        let mut objects = vec![self.root];
        for (index, _) in member.as_str().match_indices('/') {
            let parent = RelativePath::new(&member.as_str()[..index])
                .expect("member prefixes are relative paths");
            objects.push(
                *self
                    .directories
                    .get(&parent)
                    .ok_or("native tree has no parent identity")?,
            );
        }
        Ok(objects)
    }

    pub fn validate(&self, descriptor: &GenerationDescriptor) -> Result<(), String> {
        let files: std::collections::BTreeSet<_> =
            descriptor.files().iter().map(|file| &file.path).collect();
        let mut directories = std::collections::BTreeSet::new();
        for file in descriptor.files() {
            for (index, _) in file.path.as_str().match_indices('/') {
                directories.insert(
                    RelativePath::new(&file.path.as_str()[..index])
                        .expect("declared member prefixes are relative paths"),
                );
            }
        }
        if self.files.keys().collect::<std::collections::BTreeSet<_>>() != files
            || self
                .directories
                .keys()
                .cloned()
                .collect::<std::collections::BTreeSet<_>>()
                != directories
            || self.root.operating_system() != descriptor.target().operating_system()
        {
            return Err(
                "native materialization tree differs from its declared members or target".into(),
            );
        }
        let mut objects = std::collections::HashSet::from([self.root]);
        for object in self.files.values().chain(self.directories.values()) {
            let same_filesystem = match (self.root, *object) {
                (
                    crate::NativeObjectIdentity::Windows { volume: root, .. },
                    crate::NativeObjectIdentity::Windows { volume, .. },
                ) => root == volume,
                (
                    crate::NativeObjectIdentity::Linux { device: root, .. },
                    crate::NativeObjectIdentity::Linux { device, .. },
                ) => root == device,
                _ => false,
            };
            if !same_filesystem || !objects.insert(*object) {
                return Err(
                    "native materialization tree crosses or aliases filesystem objects".into(),
                );
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PayloadTreeId(Sha256Digest);

impl PayloadTreeId {
    pub const fn digest(self) -> Sha256Digest {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DeploymentContractId(Sha256Digest);

impl DeploymentContractId {
    pub const fn from_digest(digest: Sha256Digest) -> Self {
        Self(digest)
    }
    pub const fn digest(self) -> Sha256Digest {
        self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GenerationFile {
    pub path: RelativePath,
    pub size: u64,
    pub sha256: Sha256Digest,
    pub executable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "GenerationWire", into = "GenerationWire")]
pub struct GenerationDescriptor {
    target: Target,
    files: Vec<GenerationFile>,
    size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GenerationWire {
    schema: u32,
    target: Target,
    files: Vec<GenerationFile>,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum GenerationError {
    #[error("materialization directory must be a non-root path on the descriptor's target")]
    MaterializationDirectory,
    #[error("unsupported generation schema {0}")]
    Schema(u32),
    #[error("generation target `{0}` is unsupported")]
    Target(Target),
    #[error(transparent)]
    Path(#[from] TargetPathError),
    #[error("generation paths `{first}` and `{second}` conflict")]
    Conflict {
        first: RelativePath,
        second: RelativePath,
    },
    #[error("generation size exceeds the supported range")]
    SizeOverflow,
}

impl GenerationDescriptor {
    pub fn from_payloads<P>(
        target: Target,
        payloads: &[PayloadRequirement<P>],
    ) -> Result<Self, GenerationError> {
        let files = payloads
            .iter()
            .filter_map(|payload| match &payload.placement {
                PayloadPlacement::Generation { path } => Some(GenerationFile {
                    path: path.clone(),
                    size: payload.size,
                    sha256: payload.sha256,
                    executable: payload.executable,
                }),
                PayloadPlacement::Fixed { .. } => None,
            })
            .collect();
        Self::new(target, files)
    }

    pub fn new(target: Target, mut files: Vec<GenerationFile>) -> Result<Self, GenerationError> {
        let root = match target.operating_system() {
            TargetOperatingSystem::Windows => "C:\\generation",
            TargetOperatingSystem::Linux => "/generation",
            _ => return Err(GenerationError::Target(target)),
        };
        let root = TargetPath::new(target, root)?;
        let mut paths = BTreeMap::new();
        let mut size = 0u64;
        for file in &files {
            let path = root.join(file.path.as_str())?;
            if let Some(first) = paths.insert(path, file.path.clone()) {
                return Err(GenerationError::Conflict {
                    first,
                    second: file.path.clone(),
                });
            }
            size = size
                .checked_add(file.size)
                .ok_or(GenerationError::SizeOverflow)?;
        }
        for (path, name) in &paths {
            let mut parent = path.parent();
            while let Some(directory) = parent {
                if let Some(first) = paths.get(&directory) {
                    return Err(GenerationError::Conflict {
                        first: first.clone(),
                        second: name.clone(),
                    });
                }
                parent = directory.parent();
            }
        }
        files.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(Self {
            target,
            files,
            size,
        })
    }

    pub fn tree_id(&self) -> PayloadTreeId {
        let mut hash = Sha256::new();
        hash.update(b"zup:generation\0");
        hash.update(GENERATION_SCHEMA.to_le_bytes());
        hash_field(&mut hash, self.target.as_str().as_bytes());
        hash.update((self.files.len() as u64).to_le_bytes());
        for file in &self.files {
            hash_field(&mut hash, file.path.as_str().as_bytes());
            hash.update(file.size.to_le_bytes());
            hash.update(file.sha256.as_bytes());
            hash.update([u8::from(file.executable)]);
        }
        PayloadTreeId(Sha256Digest::from_hasher(hash))
    }

    pub const fn target(&self) -> Target {
        self.target
    }

    pub fn files(&self) -> &[GenerationFile] {
        &self.files
    }

    pub const fn size(&self) -> u64 {
        self.size
    }
}

fn hash_field(hash: &mut Sha256, bytes: &[u8]) {
    hash.update((bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaterializationRef {
    pub id: MaterializationId,
    pub tree: PayloadTreeId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "MaterializationWire", into = "MaterializationWire")]
pub struct MaterializationSpec {
    installation: InstallationId,
    id: MaterializationId,
    directory: TargetPath,
    descriptor: GenerationDescriptor,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MaterializationWire {
    installation: InstallationId,
    id: MaterializationId,
    directory: TargetPath,
    descriptor: GenerationDescriptor,
}

impl MaterializationSpec {
    pub fn new(
        installation: InstallationId,
        id: MaterializationId,
        directory: TargetPath,
        descriptor: GenerationDescriptor,
    ) -> Result<Self, GenerationError> {
        if directory.target() != &descriptor.target() || directory.parent().is_none() {
            return Err(GenerationError::MaterializationDirectory);
        }
        Ok(Self {
            installation,
            id,
            directory,
            descriptor,
        })
    }

    pub const fn installation(&self) -> InstallationId {
        self.installation
    }
    pub const fn id(&self) -> MaterializationId {
        self.id
    }
    pub fn directory(&self) -> &TargetPath {
        &self.directory
    }
    pub fn descriptor(&self) -> &GenerationDescriptor {
        &self.descriptor
    }
    pub fn reference(&self) -> MaterializationRef {
        MaterializationRef {
            id: self.id,
            tree: self.descriptor.tree_id(),
        }
    }
}

impl TryFrom<MaterializationWire> for MaterializationSpec {
    type Error = GenerationError;
    fn try_from(wire: MaterializationWire) -> Result<Self, Self::Error> {
        Self::new(wire.installation, wire.id, wire.directory, wire.descriptor)
    }
}

impl From<MaterializationSpec> for MaterializationWire {
    fn from(spec: MaterializationSpec) -> Self {
        Self {
            installation: spec.installation,
            id: spec.id,
            directory: spec.directory,
            descriptor: spec.descriptor,
        }
    }
}

impl TryFrom<GenerationWire> for GenerationDescriptor {
    type Error = GenerationError;

    fn try_from(wire: GenerationWire) -> Result<Self, Self::Error> {
        if wire.schema != GENERATION_SCHEMA {
            return Err(GenerationError::Schema(wire.schema));
        }
        Self::new(wire.target, wire.files)
    }
}

impl From<GenerationDescriptor> for GenerationWire {
    fn from(descriptor: GenerationDescriptor) -> Self {
        Self {
            schema: GENERATION_SCHEMA,
            target: descriptor.target,
            files: descriptor.files,
        }
    }
}
