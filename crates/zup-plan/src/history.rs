use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use zup_core::{
    InstallationId, MaterializationId, NativeObjectIdentity, RelativePath, ResourceId,
    Sha256Digest, Target, TransactionId,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetadataFile {
    pub object: NativeObjectIdentity,
    pub size: u64,
    pub sha256: Sha256Digest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JournalMember {
    Record,
    Lock,
    Head,
    EventsA,
    EventsB,
    CheckpointA,
    CheckpointB,
}

impl JournalMember {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Record => "transaction.json",
            Self::Lock => "transaction.lock",
            Self::Head => "head.json",
            Self::EventsA => "events-a.log",
            Self::EventsB => "events-b.log",
            Self::CheckpointA => "checkpoint-a.json",
            Self::CheckpointB => "checkpoint-b.json",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum HistoryFileTarget {
    Materialization { id: MaterializationId },
    Persistent { resource: ResourceId },
    Collection { intent: Sha256Digest },
}

impl HistoryFileTarget {
    pub fn path(&self, installation: InstallationId, source: TransactionId) -> RelativePath {
        RelativePath::new(match self {
            Self::Materialization { id } => format!("materializations/{installation}/{id}.json"),
            Self::Persistent { resource } => format!(
                "data/{source}-{}.json",
                zup_core::hash_bytes(resource.as_str().as_bytes())
            ),
            Self::Collection { intent } => format!("collections/{source}-{intent}.json"),
        })
        .expect("metadata paths contain only generated identifiers")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum HistoryObject {
    Journal {
        plan_hash: Sha256Digest,
        object: NativeObjectIdentity,
        members: BTreeMap<JournalMember, MetadataFile>,
        sidecars: Vec<HistoryFileTarget>,
    },
    File {
        target: HistoryFileTarget,
        observed: MetadataFile,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryCleanup {
    pub installation: InstallationId,
    pub source: TransactionId,
    pub materializations: Vec<MaterializationId>,
    pub object: HistoryObject,
}

impl HistoryCleanup {
    pub fn validate(&self, target: Target) -> Result<(), String> {
        if !self
            .materializations
            .windows(2)
            .all(|pair| pair[0] < pair[1])
        {
            return Err("history materialization references are noncanonical".into());
        }
        let matches =
            |file: &MetadataFile| file.object.operating_system() == target.operating_system();
        match &self.object {
            HistoryObject::Journal {
                object,
                members,
                sidecars,
                ..
            } => {
                if object.operating_system() != target.operating_system()
                    || !members.contains_key(&JournalMember::Record)
                    || !members.values().all(matches)
                    || !sidecars.windows(2).all(|pair| pair[0] < pair[1])
                {
                    return Err("history journal evidence is incomplete or noncanonical".into());
                }
            }
            HistoryObject::File { observed, .. } if !matches(observed) => {
                return Err("history file identity differs from its native target".into());
            }
            HistoryObject::File { .. } => {}
        }
        Ok(())
    }

    pub fn validate_owned(&self, record: &crate::InstallationRecord) -> Result<(), String> {
        self.validate(record.target)?;
        if self.installation != record.id || record.references_transaction(self.source) {
            return Err("transaction history is still required by the installation".into());
        }
        let rooted = |id| {
            record
                .retained
                .iter()
                .chain(std::iter::once(&record.current))
                .chain(
                    record
                        .retired_deployments
                        .iter()
                        .map(|retired| &retired.snapshot),
                )
                .any(|snapshot| {
                    snapshot
                        .materialization
                        .iter()
                        .chain(&snapshot.maintenance)
                        .any(|spec| spec.id() == id)
                })
        };
        let live = self.materializations.iter().copied().any(rooted);
        if live {
            return Err("history retains evidence for a rooted materialization".into());
        }
        Ok(())
    }
}
