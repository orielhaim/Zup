use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeploymentStorageEstimate {
    pub new_generation_bytes: u64,
    pub fixed_staging_bytes: u64,
    pub verified_reuse_bytes: u64,
    pub retained_fixed_bytes: u64,
    pub cleanup_candidate_bytes: u64,
}

impl DeploymentStorageEstimate {
    pub const fn additional_payload_bytes(self) -> u64 {
        self.new_generation_bytes
            .saturating_add(self.fixed_staging_bytes)
    }
}
