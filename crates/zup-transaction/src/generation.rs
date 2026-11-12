use serde::{Deserialize, Serialize};
use zup_core::{
    InstallationId, MaterializationRef, MaterializationTree, NativeObjectIdentity, Sha256Digest,
    TargetPath,
};
use zup_plan::{ActivationSelection, GenerationIntent, GenerationOperation};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum MaterializationState {
    Reserved {
        directory: zup_core::ResourceId,
    },
    Preparing {
        directory: zup_core::ResourceId,
    },
    Prepared {
        directory: zup_core::ResourceId,
        tree: MaterializationTree,
    },
    Publishing {
        directory: zup_core::ResourceId,
        tree: MaterializationTree,
    },
    Sealed {
        tree: MaterializationTree,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GenerationAccess {
    Private,
    Publishing,
    Public,
}

impl MaterializationState {
    pub fn directory(&self) -> Option<&zup_core::ResourceId> {
        match self {
            Self::Reserved { directory }
            | Self::Preparing { directory }
            | Self::Prepared { directory, .. }
            | Self::Publishing { directory, .. } => Some(directory),
            Self::Sealed { .. } => None,
        }
    }

    pub fn tree(&self) -> Option<&MaterializationTree> {
        match self {
            Self::Reserved { .. } | Self::Preparing { .. } => None,
            Self::Prepared { tree, .. } | Self::Publishing { tree, .. } | Self::Sealed { tree } => {
                Some(tree)
            }
        }
    }

    pub fn access(&self) -> GenerationAccess {
        match self {
            Self::Reserved { .. } | Self::Preparing { .. } | Self::Prepared { .. } => {
                GenerationAccess::Private
            }
            Self::Publishing { .. } => GenerationAccess::Publishing,
            Self::Sealed { .. } => GenerationAccess::Public,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaterializationRecord {
    pub schema: u32,
    pub transaction: zup_core::TransactionId,
    pub specification: zup_core::MaterializationSpec,
    pub namespace: NativeObjectIdentity,
    /// In `Reserved`, identifies the parent namespace. Otherwise identifies the content root.
    pub object: NativeObjectIdentity,
    pub state: MaterializationState,
    pub retired_preparations: Vec<RetiredPreparation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetiredPreparation {
    pub directory: zup_core::ResourceId,
    pub object: NativeObjectIdentity,
    pub collection: PreparationCollection,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum PreparationCollection {
    Retired,
    Deleting { tree: PreparationTree },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreparationTree {
    pub directories: std::collections::BTreeMap<zup_core::RelativePath, NativeObjectIdentity>,
    pub files: std::collections::BTreeMap<zup_core::RelativePath, zup_plan::MetadataFile>,
}

#[derive(Serialize)]
struct MaterializationSeal<'a> {
    schema: u32,
    transaction: zup_core::TransactionId,
    specification: &'a zup_core::MaterializationSpec,
    namespace: NativeObjectIdentity,
    object: NativeObjectIdentity,
    tree: &'a MaterializationTree,
}

impl MaterializationRecord {
    pub fn observation(&self) -> MaterializationObservation {
        MaterializationObservation {
            installation: self.specification.installation(),
            materialization: self.specification.reference(),
            directory: self.specification.directory().clone(),
            object: self.object,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.schema != 2 {
            return Err("unsupported materialization record schema".into());
        }
        if let Some(tree) = self.state.tree() {
            tree.validate(self.specification.descriptor())?;
            if tree.root != self.object {
                return Err("sealed member tree differs from its materialization root".into());
            }
        }
        let mut directories = std::collections::BTreeSet::new();
        if let Some(directory) = self.state.directory() {
            validate_preparation_name(directory)?;
            directories.insert(directory);
        }
        if !same_filesystem(self.object, self.namespace)
            || matches!(self.state, MaterializationState::Reserved { .. })
                != (self.object == self.namespace)
        {
            return Err("materialization root conflicts with its namespace reservation".into());
        }
        let mut objects = std::collections::HashSet::from([self.object, self.namespace]);
        for preparation in &self.retired_preparations {
            validate_preparation_name(&preparation.directory)?;
            if !directories.insert(&preparation.directory)
                || !objects.insert(preparation.object)
                || !same_filesystem(preparation.object, self.namespace)
            {
                return Err(
                    "retired preparation identity conflicts with the materialization".into(),
                );
            }
            if let PreparationCollection::Deleting { tree } = &preparation.collection {
                tree.validate(preparation.object)?;
            }
        }
        self.observation().validate()
    }

    pub fn fingerprint(&self) -> Result<Sha256Digest, String> {
        use sha2::{Digest, Sha256};
        self.validate()?;
        let MaterializationState::Sealed { tree } = &self.state else {
            return Err("only sealed content has a materialization fingerprint".into());
        };
        let bytes = serde_json::to_vec(&MaterializationSeal {
            schema: 2,
            transaction: self.transaction,
            specification: &self.specification,
            namespace: self.namespace,
            object: self.object,
            tree,
        })
        .map_err(|error| error.to_string())?;
        let mut hash = Sha256::new();
        hash.update(b"zup:materialization-seal:2\0");
        hash.update(bytes);
        Ok(Sha256Digest::from_hasher(hash))
    }
}

impl PreparationTree {
    pub fn validate(&self, root: NativeObjectIdentity) -> Result<(), String> {
        let mut objects = std::collections::HashSet::from([root]);
        for (path, object) in self
            .directories
            .iter()
            .map(|(p, o)| (p, *o))
            .chain(self.files.iter().map(|(p, f)| (p, f.object)))
        {
            if !same_filesystem(root, object)
                || !objects.insert(object)
                || (self.files.contains_key(path) && self.directories.contains_key(path))
            {
                return Err("preparation inventory crosses or aliases native objects".into());
            }
            for (index, _) in path.as_str().match_indices('/') {
                let parent = zup_core::RelativePath::new(&path.as_str()[..index])
                    .map_err(|e| e.to_string())?;
                if !self.directories.contains_key(&parent) {
                    return Err("preparation inventory is missing a parent".into());
                }
            }
        }
        Ok(())
    }
}

fn same_filesystem(a: NativeObjectIdentity, b: NativeObjectIdentity) -> bool {
    match (a, b) {
        (
            NativeObjectIdentity::Windows { volume: a, .. },
            NativeObjectIdentity::Windows { volume: b, .. },
        ) => a == b,
        (
            NativeObjectIdentity::Linux { device: a, .. },
            NativeObjectIdentity::Linux { device: b, .. },
        ) => a == b,
        _ => false,
    }
}

fn validate_preparation_name(directory: &zup_core::ResourceId) -> Result<(), String> {
    if directory
        .as_str()
        .strip_prefix(".preparing-")
        .is_some_and(|id| id.parse::<zup_core::MaterializationId>().is_ok())
    {
        Ok(())
    } else {
        Err("invalid materialization staging directory".into())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaterializationObservation {
    pub installation: InstallationId,
    pub materialization: MaterializationRef,
    pub directory: TargetPath,
    pub object: NativeObjectIdentity,
}

impl MaterializationObservation {
    pub fn validate(&self) -> Result<(), String> {
        if self.directory.target().operating_system() != self.object.operating_system()
            || self.directory.parent().is_none()
        {
            return Err("generation directory does not match its native object identity".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "step", deny_unknown_fields)]
pub enum GenerationReceipt {
    Materialized {
        observed: MaterializationObservation,
    },
    Sealed {
        observed: MaterializationObservation,
        seal: Sha256Digest,
        tree: MaterializationTree,
    },
    Verified {
        observed: MaterializationObservation,
        seal: Sha256Digest,
    },
    Activated {
        observed: MaterializationObservation,
        selection: ActivationSelection,
        seal: Sha256Digest,
    },
}

impl GenerationReceipt {
    pub fn observed(&self) -> &MaterializationObservation {
        match self {
            Self::Materialized { observed }
            | Self::Sealed { observed, .. }
            | Self::Verified { observed, .. }
            | Self::Activated { observed, .. } => observed,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        let observed = self.observed();
        observed.validate()?;
        if let Self::Sealed { tree, .. } = self
            && tree.root != observed.object
        {
            return Err("sealed receipt differs from its native tree root".into());
        }
        if let Self::Activated { selection, .. } = self
            && selection.materialization != Some(observed.materialization)
        {
            return Err("activation receipt selects a different materialization".into());
        }
        Ok(())
    }

    pub fn matches(&self, operation: GenerationOperation, intent: &GenerationIntent) -> bool {
        let observed = self.observed();
        if observed.installation != intent.materialization.installation()
            || observed.materialization != intent.materialization.reference()
            || &observed.directory != intent.materialization.directory()
        {
            return false;
        }
        if let Self::Sealed { tree, .. } = self
            && tree.validate(intent.materialization.descriptor()).is_err()
        {
            return false;
        }
        match (operation, self) {
            (GenerationOperation::Materialize, Self::Materialized { .. })
            | (GenerationOperation::Seal, Self::Sealed { .. })
            | (GenerationOperation::Verify, Self::Verified { .. }) => true,
            (GenerationOperation::Activate, Self::Activated { selection, .. }) => {
                Some(*selection)
                    == intent
                        .publication
                        .selection(intent.materialization.reference())
            }
            _ => false,
        }
    }
}
