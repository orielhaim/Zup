use crate::{
    ComponentId, Condition, Installer, PluginId, PrerequisiteId, RelativePath, Sha256Digest, Target,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginArtifact {
    pub plugin_id: PluginId,
    pub source_size: u64,
    pub source_sha256: Sha256Digest,
    pub target: Target,
    pub wasmtime_version: String,
    pub component_format_version: u32,
    pub plugin_api_version: String,
    pub wit_digest: Sha256Digest,
    pub engine_fingerprint: Sha256Digest,
    pub component_size: u64,
    pub component_sha256: Sha256Digest,
    pub blob: Sha256Digest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackagePlan {
    pub installer: Installer,
    pub entries: Vec<PayloadEntry>,
    #[serde(default)]
    pub prerequisite_artifacts: Vec<PrerequisiteArtifact>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ui_assets: Vec<crate::PresetAsset>,
    pub plugins: Vec<PluginArtifact>,
    pub total_size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PayloadEntry {
    pub id: crate::PayloadId,
    pub path: RelativePath,
    pub placement: crate::PayloadPlacement,
    pub size: u64,
    pub sha256: Sha256Digest,
    pub blob: Sha256Digest,
    pub component: Option<ComponentId>,
    pub condition: Option<Condition>,
    pub executable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrerequisiteArtifact {
    pub prerequisite_id: PrerequisiteId,
    pub path: RelativePath,
    pub size: u64,
    pub sha256: Sha256Digest,
    pub blob: Sha256Digest,
}
