use serde::{Deserialize, Serialize};
use zup_core::{ActivationRevision, Sha256Digest, TransactionId};

use crate::{DeploymentSnapshot, InstallationRecord};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetiredDeploymentSelection {
    pub revision: ActivationRevision,
    pub transaction: TransactionId,
    pub plan_hash: Sha256Digest,
}

impl From<&DeploymentSnapshot> for RetiredDeploymentSelection {
    fn from(snapshot: &DeploymentSnapshot) -> Self {
        Self {
            revision: snapshot.revision,
            transaction: snapshot.transaction,
            plan_hash: snapshot.plan_hash,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetentionChange {
    pub retire: Vec<RetiredDeploymentSelection>,
    pub forget: Vec<RetiredDeploymentSelection>,
}

impl RetentionChange {
    pub fn keeping_previous(record: &InstallationRecord, keep: u16) -> Option<Self> {
        let retire: Vec<_> = record
            .retained
            .iter()
            .take(record.retained.len().saturating_sub(usize::from(keep)))
            .map(Into::into)
            .collect();
        (!retire.is_empty()).then_some(Self {
            retire,
            forget: Vec::new(),
        })
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.retire.is_empty() && self.forget.is_empty()
            || [&self.retire, &self.forget].iter().any(|selections| {
                selections
                    .windows(2)
                    .any(|pair| pair[0].revision >= pair[1].revision)
            })
            || self.retire.iter().any(|selected| {
                self.forget
                    .iter()
                    .any(|other| selected.revision == other.revision)
            })
        {
            return Err("retention requires distinct revisions in increasing order".into());
        }
        Ok(())
    }

    pub fn validate_owned(&self, record: &InstallationRecord) -> Result<(), String> {
        self.validate()?;
        for selection in &self.retire {
            if !record
                .retained
                .iter()
                .any(|snapshot| RetiredDeploymentSelection::from(snapshot) == *selection)
            {
                return Err(
                    "retirement selection differs from protected retained ownership".into(),
                );
            }
        }
        for selection in &self.forget {
            if !record
                .retired_deployments
                .iter()
                .any(|retired| RetiredDeploymentSelection::from(&retired.snapshot) == *selection)
            {
                return Err("collected selection differs from protected retired ownership".into());
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetiredDeployment {
    pub snapshot: DeploymentSnapshot,
    pub retired_by: TransactionId,
}
