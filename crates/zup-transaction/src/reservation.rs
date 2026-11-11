use serde::{Deserialize, Serialize};
use zup_core::{AppId, InstallationId, SelectedScope, Target, TargetPath, TransactionId};

use crate::TransactionRecord;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstallationReservation {
    pub schema: u32,
    pub installation: InstallationId,
    pub app_id: AppId,
    pub scope: SelectedScope,
    pub target: Target,
    pub directory: TargetPath,
    pub origin: TransactionId,
}

impl InstallationReservation {
    pub fn for_transaction(transaction: &TransactionRecord) -> Result<Self, String> {
        Ok(Self {
            schema: 1,
            installation: transaction.plan.installation,
            app_id: transaction.app_id.clone(),
            scope: transaction.scope,
            target: transaction.target,
            directory: transaction
                .plan
                .install_directory
                .clone()
                .ok_or("installation reservation requires a physical installation directory")?,
            origin: transaction.transaction_id,
        })
    }

    pub fn matches(&self, transaction: &TransactionRecord) -> bool {
        self.schema == 1
            && self.installation == transaction.plan.installation
            && self.app_id == transaction.app_id
            && self.scope == transaction.scope
            && self.target == transaction.target
            && self.directory.target() == &self.target
            && transaction.plan.install_directory.as_ref() == Some(&self.directory)
    }
}
