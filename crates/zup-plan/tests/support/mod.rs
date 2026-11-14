use zup_core::{ActivationRevision, TransactionId, hash_bytes};
use zup_plan::{DeploymentSnapshot, InstallationRecord, InstallationState};

pub fn installed(desired: &zup_plan::BoundDeployment) -> InstallationRecord {
    {
        let fixture_commit = zup_plan::InstallationCommit {
            transaction: TransactionId::new_v7(),
            plan_hash: hash_bytes(b"planner fixture"),
        };
        InstallationRecord {
            commit: fixture_commit,
            retired_deployments: Vec::new(),
            data: Default::default(),
            id: desired.installation,
            state: InstallationState::Installed,
            app_id: desired.app.id.clone(),
            scope: desired.scope,
            target: desired.target,
            directory: desired.install_directory.clone(),
            current: DeploymentSnapshot {
                maintenance: None,
                requirements: desired.requirements.clone(),
                revision: ActivationRevision::FIRST,
                transaction: fixture_commit.transaction,
                plan_hash: fixture_commit.plan_hash,
                version: desired.app.version.clone(),
                selected_components: desired.selected_components.clone(),
                release: None,
                preset: desired.preset.clone(),
                materialization: None,
                resources: Default::default(),
            },
            retained: Vec::new(),
        }
    }
}
