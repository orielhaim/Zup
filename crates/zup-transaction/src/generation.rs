use serde::{Deserialize, Serialize};
use zup_core::{
    InstallationId, MaterializationRef, MaterializationTree, NativeObjectIdentity, Sha256Digest,
    TargetPath,
};
use zup_plan::{ActivationSelection, GenerationIntent, GenerationOperation};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum MaterializationState {
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
            Self::Preparing { directory }
            | Self::Prepared { directory, .. }
            | Self::Publishing { directory, .. } => Some(directory),
            Self::Sealed { .. } => None,
        }
    }

    pub fn tree(&self) -> Option<&MaterializationTree> {
        match self {
            Self::Preparing { .. } => None,
            Self::Prepared { tree, .. } | Self::Publishing { tree, .. } | Self::Sealed { tree } => {
                Some(tree)
            }
        }
    }

    pub fn access(&self) -> GenerationAccess {
        match self {
            Self::Preparing { .. } | Self::Prepared { .. } => GenerationAccess::Private,
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
    pub object: NativeObjectIdentity,
    pub state: MaterializationState,
    pub retired_preparations: Vec<RetiredPreparation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetiredPreparation {
    pub directory: zup_core::ResourceId,
    pub object: NativeObjectIdentity,
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
        if self.schema != 1 {
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
        let mut objects = std::collections::HashSet::from([self.object]);
        for preparation in &self.retired_preparations {
            validate_preparation_name(&preparation.directory)?;
            if !directories.insert(&preparation.directory)
                || !objects.insert(preparation.object)
                || preparation.object.operating_system() != self.object.operating_system()
            {
                return Err(
                    "retired preparation identity conflicts with the materialization".into(),
                );
            }
        }
        self.observation().validate()
    }

    pub fn fingerprint(&self) -> Result<Sha256Digest, String> {
        use sha2::{Digest, Sha256};
        self.validate()?;
        let bytes = serde_json::to_vec(self).map_err(|error| error.to_string())?;
        let mut hash = Sha256::new();
        hash.update(b"zup:materialization-record:1\0");
        hash.update(bytes);
        Ok(Sha256Digest::from_hasher(hash))
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
