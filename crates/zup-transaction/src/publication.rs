use serde::{Deserialize, Serialize};
use zup_core::Sha256Digest;
use zup_plan::InstallationRecord;

use crate::TransactionRecord;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum PublicationState {
    NotRequested,
    Requested { record_hash: Sha256Digest },
    Published { record_hash: Sha256Digest },
}

pub trait InstallationPublisher {
    type Error: std::fmt::Display;

    fn prepare(&self, transaction: &TransactionRecord) -> Result<InstallationRecord, Self::Error>;

    /// Publish durably and read back through the protected installation authority.
    /// Repeating a request must verify and durably republish the same record.
    fn publish(&self, transaction: &TransactionRecord) -> Result<InstallationRecord, Self::Error>;
}

pub fn verify_published_installation(
    transaction: &TransactionRecord,
    installation: &InstallationRecord,
) -> Result<Sha256Digest, String> {
    let mut data = std::collections::BTreeMap::new();
    for node in &transaction.plan.nodes {
        if let zup_plan::NodeKind::PersistentDirectory { operation } = &node.kind
            && operation.stage == zup_plan::PersistentDirectoryStage::Access
        {
            let Some(crate::OperationReceipt::PersistentDirectory(receipt)) =
                transaction.receipt(&node.id)
            else {
                return Err("persistent publication is missing its verified access receipt".into());
            };
            let owned = &receipt.owned;
            if receipt.stage != operation.stage
                || owned.installation != transaction.plan.installation
                || owned.directory != operation.intent.directory
                || data
                    .insert(owned.directory.id.clone(), owned.clone())
                    .is_some()
                || installation.data.get(&owned.directory.id) != Some(owned)
            {
                return Err(
                    "persistent publication differs from authenticated access intent".into(),
                );
            }
        }
    }

    let expected = match transaction.plan.observed {
        Some(observed) if transaction.plan.transition.is_maintain() => observed.revision,
        Some(observed) => observed
            .revision
            .next()
            .map_err(|error| error.to_string())?,
        None => zup_core::ActivationRevision::FIRST,
    };
    let materialization = transaction
        .plan
        .nodes
        .iter()
        .find_map(|node| match &node.kind {
            zup_plan::NodeKind::Generation {
                operation: zup_plan::GenerationOperation::Activate,
                intent,
            } => Some(&intent.materialization),
            _ => None,
        });
    let maintenance = transaction
        .plan
        .nodes
        .iter()
        .find_map(|node| match &node.kind {
            zup_plan::NodeKind::Generation { intent, .. }
                if intent.publication == zup_plan::GenerationPublication::Maintenance =>
            {
                Some(&intent.materialization)
            }
            _ => None,
        });
    if transaction.plan.transition.is_maintain() {
        if installation.id != transaction.plan.installation
            || installation.app_id != transaction.app_id
            || installation.scope != transaction.scope
            || installation.target != transaction.target
            || installation.commit.transaction != transaction.transaction_id
            || installation.commit.plan_hash != transaction.plan_hash
            || installation.current.requirements != transaction.plan.requirements
            || Some(installation.current.selection()) != transaction.plan.observed
            || installation.current.version != transaction.app_version
            || installation.current.release != transaction.release
            || installation.current.selected_components != transaction.plan.selected_components
            || installation.current.preset != transaction.plan.preset
            || transaction.plan.install_directory.as_ref() != Some(&installation.directory)
        {
            return Err("metadata publication changed the active deployment selection".into());
        }
        let bytes = installation.encode().map_err(|error| error.to_string())?;
        return Ok(zup_core::hash_bytes(&bytes));
    }
    if installation.id != transaction.plan.installation
        || installation.app_id != transaction.app_id
        || installation.scope != transaction.scope
        || installation.target != transaction.target
        || installation.commit.transaction != transaction.transaction_id
        || installation.commit.plan_hash != transaction.plan_hash
        || installation.current.transaction != transaction.transaction_id
        || installation.current.plan_hash != transaction.plan_hash
        || installation.current.requirements != transaction.plan.requirements
        || installation.current.revision != expected
        || installation.current.version != transaction.app_version
        || installation.current.release != transaction.release
        || installation.current.materialization.as_ref() != materialization
        || installation.current.maintenance.as_ref() != maintenance
        || installation.current.selected_components != transaction.plan.selected_components
        || installation.current.preset != transaction.plan.preset
        || transaction.plan.install_directory.as_ref() != Some(&installation.directory)
        || (installation.state == zup_plan::InstallationState::Removed)
            != transaction.plan.transition.is_remove()
    {
        return Err(
            "published installation does not match authenticated transaction intent".into(),
        );
    }
    let bytes = installation.encode().map_err(|error| error.to_string())?;
    Ok(zup_core::hash_bytes(&bytes))
}

pub fn verify_installation_evidence(
    installation: &InstallationRecord,
    journal: &TransactionRecord,
) -> Result<(), String> {
    journal.validate().map_err(|error| error.to_string())?;
    let digest = verify_published_installation(journal, installation)?;
    let expected = match journal.publication {
        PublicationState::Requested { record_hash }
        | PublicationState::Published { record_hash } => record_hash,
        PublicationState::NotRequested => {
            return Err("installation has no durable publication request".into());
        }
    };
    if digest != expected {
        return Err(
            "installation ownership differs from its protected publication evidence".into(),
        );
    }
    Ok(())
}
