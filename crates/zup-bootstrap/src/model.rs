use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;
use zup_core::{
    AppId, Prerequisite, PrerequisiteArchitecture, PrerequisiteId, PrerequisiteInstaller,
    PrerequisitePackage, PrerequisiteRequirement, RelativePath, SelectedScope, Sha256Digest,
    TargetTriple,
};

pub const BOOTSTRAP_PLAN_SCHEMA: u32 = 1;
pub const BOOTSTRAP_STATE_SCHEMA: u32 = 1;
pub const MAX_BOOTSTRAP_OPERATIONS: usize = 256;
pub const MAX_BOOTSTRAP_STATE_BYTES: u64 = 8 * 1024 * 1024;
pub const MAX_BOOTSTRAP_PLAN_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BootstrapId(pub Uuid);

impl BootstrapId {
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }

    pub const fn from_uuid(value: Uuid) -> Self {
        Self(value)
    }

    pub fn for_plan(plan: &BootstrapPlan) -> Self {
        Self::for_hash(plan.fingerprint())
    }

    pub fn for_hash(plan_hash: Sha256Digest) -> Self {
        let mut bytes = [0u8; 16];
        bytes.copy_from_slice(&plan_hash.as_bytes()[..16]);
        bytes[6] = (bytes[6] & 0x0f) | 0x50;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        Self(Uuid::from_bytes(bytes))
    }

    pub const fn as_uuid(&self) -> Uuid {
        self.0
    }
}

impl Default for BootstrapId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for BootstrapId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BootstrapKey {
    pub app_id: AppId,
    pub app_version: Version,
    pub scope: SelectedScope,
    pub target: TargetTriple,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BootstrapOperation {
    pub id: PrerequisiteId,
    pub name: String,
    pub target: PrerequisiteArchitecture,
    pub requirement: PrerequisiteRequirement,
    pub package: PrerequisitePackage,
    pub installer: PrerequisiteInstaller,
}

impl BootstrapOperation {
    pub fn from_prerequisite(prerequisite: &Prerequisite) -> Self {
        Self {
            id: prerequisite.id.clone(),
            name: prerequisite.name.to_string(),
            target: prerequisite.target,
            requirement: prerequisite.requirement.clone(),
            package: prerequisite.package.clone(),
            installer: prerequisite.installer.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BootstrapPlan {
    pub schema: u32,
    pub key: BootstrapKey,
    pub operations: Vec<BootstrapOperation>,
}

impl BootstrapPlan {
    pub fn new(
        key: BootstrapKey,
        operations: Vec<BootstrapOperation>,
    ) -> Result<Self, BootstrapError> {
        if operations.len() > MAX_BOOTSTRAP_OPERATIONS {
            return Err(BootstrapError::Limit("too many prerequisite operations"));
        }
        let mut ids = BTreeSet::new();
        for operation in &operations {
            if !ids.insert(operation.id.as_str().to_ascii_lowercase()) {
                return Err(BootstrapError::DuplicateOperation(operation.id.to_string()));
            }
            validate_operation(operation)?;
        }
        let plan = Self {
            schema: BOOTSTRAP_PLAN_SCHEMA,
            key,
            operations,
        };
        if serde_json::to_vec(&plan)
            .map_err(|_| BootstrapError::Limit("bootstrap plan is not serializable"))?
            .len()
            > MAX_BOOTSTRAP_PLAN_BYTES
        {
            return Err(BootstrapError::Limit(
                "bootstrap plan exceeds the size limit",
            ));
        }
        Ok(plan)
    }

    pub fn fingerprint(&self) -> Sha256Digest {
        let bytes = serde_json::to_vec(self).expect("bootstrap plan serializes");
        Sha256Digest::from_bytes(Sha256::digest(bytes).into())
    }

    pub fn target(&self) -> &TargetTriple {
        &self.key.target
    }

    pub fn operation(&self, id: &PrerequisiteId) -> Option<&BootstrapOperation> {
        self.operations.iter().find(|operation| &operation.id == id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuarantinedArtifact {
    pub relative_path: RelativePath,
    pub size: u64,
    pub sha256: Sha256Digest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageIdentity {
    pub prerequisite_id: PrerequisiteId,
    pub filename: String,
    pub size: Option<u64>,
    pub sha256: Sha256Digest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoundBootstrapOperation {
    pub operation: BootstrapOperation,
    pub artifact: QuarantinedArtifact,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoundBootstrapPlan {
    pub id: BootstrapId,
    pub plan: BootstrapPlan,
    pub plan_hash: Sha256Digest,
    pub artifacts: BTreeMap<PrerequisiteId, QuarantinedArtifact>,
}

impl BoundBootstrapPlan {
    pub fn new(
        plan: BootstrapPlan,
        artifacts: BTreeMap<PrerequisiteId, QuarantinedArtifact>,
    ) -> Result<Self, BootstrapError> {
        let id = BootstrapId::for_plan(&plan);
        Self::with_id(id, plan, artifacts)
    }

    pub fn with_id(
        id: BootstrapId,
        plan: BootstrapPlan,
        artifacts: BTreeMap<PrerequisiteId, QuarantinedArtifact>,
    ) -> Result<Self, BootstrapError> {
        if plan.schema != BOOTSTRAP_PLAN_SCHEMA {
            return Err(BootstrapError::InvalidState(
                "unsupported bootstrap plan schema",
            ));
        }
        let plan = BootstrapPlan::new(plan.key.clone(), plan.operations.clone())?;
        if id != BootstrapId::for_plan(&plan) {
            return Err(BootstrapError::InvalidState(
                "bootstrap identity is not deterministic",
            ));
        }
        let plan_hash = plan.fingerprint();
        let operation_ids = plan
            .operations
            .iter()
            .map(|operation| operation.id.clone())
            .collect::<BTreeSet<_>>();
        if artifacts.keys().any(|id| !operation_ids.contains(id)) {
            return Err(BootstrapError::InvalidState(
                "artifact references unknown operation",
            ));
        }
        for operation in &plan.operations {
            if let Some(artifact) = artifacts.get(&operation.id) {
                let expected_path = RelativePath::from_components([
                    operation.id.as_str(),
                    operation.package.filename(),
                ])
                .map_err(|_| BootstrapError::InvalidState("invalid artifact path"))?;
                if artifact.relative_path != expected_path
                    || artifact.sha256 != operation.package.digest()
                    || artifact.size > zup_core::MAX_PREREQUISITE_PACKAGE_BYTES
                    || operation
                        .package
                        .size()
                        .is_some_and(|size| artifact.size != size)
                {
                    return Err(BootstrapError::ArtifactMismatch(operation.id.to_string()));
                }
            }
        }
        Ok(Self {
            id,
            plan,
            plan_hash,
            artifacts,
        })
    }

    pub fn operation(&self, id: &PrerequisiteId) -> Option<BoundBootstrapOperation> {
        self.artifacts.get(id).and_then(|artifact| {
            self.plan
                .operation(id)
                .map(|operation| BoundBootstrapOperation {
                    operation: operation.clone(),
                    artifact: artifact.clone(),
                })
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BootstrapPhase {
    Planned,
    Preparing,
    Executing,
    Ready,
    RebootRequired,
    RecoveryRequired,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum BootstrapOperationState {
    Pending,
    Downloading,
    Ready {
        artifact: QuarantinedArtifact,
    },
    Running,
    Satisfied {
        version: Option<Version>,
        evidence: String,
    },
    RebootRequired {
        exit_code: i32,
    },
    Failed {
        code: String,
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BootstrapState {
    pub schema: u32,
    pub id: BootstrapId,
    pub key: BootstrapKey,
    pub plan_hash: Sha256Digest,
    pub phase: BootstrapPhase,
    pub revision: u64,
    pub completed: Vec<PrerequisiteId>,
    pub remaining: Vec<PrerequisiteId>,
    pub operations: Vec<BootstrapOperationStateRecord>,
    pub packages: Vec<PackageIdentity>,
    pub reboot_required: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BootstrapOperationStateRecord {
    pub id: PrerequisiteId,
    pub state: BootstrapOperationState,
}

impl BootstrapState {
    pub fn new(plan: &BootstrapPlan) -> Self {
        let operations = plan
            .operations
            .iter()
            .map(|operation| BootstrapOperationStateRecord {
                id: operation.id.clone(),
                state: BootstrapOperationState::Pending,
            })
            .collect::<Vec<_>>();
        let ids = operations
            .iter()
            .map(|operation| operation.id.clone())
            .collect();
        Self {
            schema: BOOTSTRAP_STATE_SCHEMA,
            id: BootstrapId::for_plan(plan),
            key: plan.key.clone(),
            plan_hash: plan.fingerprint(),
            phase: BootstrapPhase::Planned,
            revision: 0,
            completed: Vec::new(),
            remaining: ids,
            operations,
            packages: plan
                .operations
                .iter()
                .map(|operation| PackageIdentity {
                    prerequisite_id: operation.id.clone(),
                    filename: operation.package.filename().to_owned(),
                    size: operation.package.size(),
                    sha256: operation.package.digest(),
                })
                .collect(),
            reboot_required: false,
        }
    }

    pub fn validate(&self, plan: &BootstrapPlan) -> Result<(), BootstrapError> {
        if self.key.target != plan.key.target {
            return Err(BootstrapError::TargetMismatch);
        }
        if self.schema != BOOTSTRAP_STATE_SCHEMA
            || self.id != BootstrapId::for_plan(plan)
            || self.key != plan.key
            || self.plan_hash != plan.fingerprint()
            || self.operations.len() != plan.operations.len()
            || self.operations.len() > MAX_BOOTSTRAP_OPERATIONS
            || self.packages.len() != plan.operations.len()
            || self.packages.len() > MAX_BOOTSTRAP_OPERATIONS
        {
            return Err(BootstrapError::InvalidState("state identity or bounds"));
        }
        let plan_ids = plan
            .operations
            .iter()
            .map(|operation| &operation.id)
            .collect::<BTreeSet<_>>();
        let state_ids = self
            .operations
            .iter()
            .map(|operation| &operation.id)
            .collect::<BTreeSet<_>>();
        let completed = self.completed.iter().collect::<BTreeSet<_>>();
        let remaining = self.remaining.iter().collect::<BTreeSet<_>>();
        if state_ids != plan_ids
            || completed.len() != self.completed.len()
            || remaining.len() != self.remaining.len()
            || !completed.is_disjoint(&remaining)
            || completed
                .union(&remaining)
                .copied()
                .collect::<BTreeSet<_>>()
                != plan_ids
        {
            return Err(BootstrapError::InvalidState(
                "operation identity or partition",
            ));
        }
        let package_ids = self
            .packages
            .iter()
            .map(|package| &package.prerequisite_id)
            .collect::<BTreeSet<_>>();
        if package_ids != plan_ids || package_ids.len() != self.packages.len() {
            return Err(BootstrapError::InvalidState(
                "package identity or duplicate state",
            ));
        }
        for package in &self.packages {
            let Some(operation) = plan.operation(&package.prerequisite_id) else {
                return Err(BootstrapError::InvalidState(
                    "package references unknown operation",
                ));
            };
            if package.sha256 != operation.package.digest()
                || package.size != operation.package.size()
                || package.filename != operation.package.filename()
            {
                return Err(BootstrapError::InvalidState("package identity changed"));
            }
        }
        Ok(())
    }

    pub fn operation_mut(&mut self, id: &PrerequisiteId) -> Option<&mut BootstrapOperationState> {
        self.operations
            .iter_mut()
            .find(|operation| &operation.id == id)
            .map(|operation| &mut operation.state)
    }

    pub fn mark(
        &mut self,
        id: &PrerequisiteId,
        state: BootstrapOperationState,
    ) -> Result<(), BootstrapError> {
        let Some(operation) = self
            .operations
            .iter_mut()
            .find(|operation| &operation.id == id)
        else {
            return Err(BootstrapError::UnknownOperation(id.to_string()));
        };
        operation.state = state;
        Ok(())
    }

    pub fn recompute(&mut self) {
        let satisfied = self
            .operations
            .iter()
            .filter(|operation| {
                matches!(operation.state, BootstrapOperationState::Satisfied { .. })
            })
            .count();
        let reboot = self.operations.iter().any(|operation| {
            matches!(
                operation.state,
                BootstrapOperationState::RebootRequired { .. }
            )
        });
        let failed = self
            .operations
            .iter()
            .any(|operation| matches!(operation.state, BootstrapOperationState::Failed { .. }));
        self.reboot_required = reboot;
        self.completed = self
            .operations
            .iter()
            .filter(|operation| {
                matches!(operation.state, BootstrapOperationState::Satisfied { .. })
            })
            .map(|operation| operation.id.clone())
            .collect();
        self.remaining = self
            .operations
            .iter()
            .filter(|operation| {
                !matches!(operation.state, BootstrapOperationState::Satisfied { .. })
            })
            .map(|operation| operation.id.clone())
            .collect();
        self.phase = if failed && self.operations.iter().any(|operation| {
            matches!(
                &operation.state,
                BootstrapOperationState::Failed { code, .. } if code == "ambiguous_external_process"
            )
        }) {
            BootstrapPhase::RecoveryRequired
        } else if failed {
            BootstrapPhase::Failed
        } else if reboot {
            BootstrapPhase::RebootRequired
        } else if satisfied == self.operations.len() {
            BootstrapPhase::Ready
        } else if self
            .operations
            .iter()
            .any(|operation| matches!(operation.state, BootstrapOperationState::Running))
        {
            BootstrapPhase::Executing
        } else {
            BootstrapPhase::Preparing
        };
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DetectionResult {
    Satisfied {
        version: Option<Version>,
        evidence: String,
    },
    Missing,
    Incompatible {
        found: Version,
        required: VersionReq,
    },
}

pub trait PrerequisiteSatisfier: Send + Sync {
    fn satisfy(&self, operation: &BootstrapOperation) -> Result<DetectionResult, BootstrapError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderRequest {
    pub prerequisite_id: PrerequisiteId,
    pub requirement: PrerequisiteRequirement,
    pub executable: PathBuf,
    pub arguments: Vec<String>,
    pub expected_digest: Sha256Digest,
    pub expected_size: Option<u64>,
    pub success_exit_codes: Vec<i32>,
    pub reboot_exit_codes: Vec<i32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderOutcome {
    Succeeded,
    AlreadySatisfied,
    RebootRequired { exit_code: i32 },
}

pub trait PrerequisiteProvider: Send + Sync {
    fn execute(&self, request: &ProviderRequest) -> Result<ProviderOutcome, BootstrapError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BootstrapOutcome {
    Ready,
    RebootRequired {
        exit_code: i32,
        prerequisite_id: PrerequisiteId,
    },
    RecoveryRequired,
}

#[derive(Debug, Error)]
pub enum BootstrapError {
    #[error("bootstrap operation `{0}` is not in the plan")]
    UnknownOperation(String),
    #[error("duplicate bootstrap operation `{0}`")]
    DuplicateOperation(String),
    #[error("bootstrap artifact for `{0}` is missing")]
    MissingArtifact(String),
    #[error("bootstrap artifact for `{0}` does not match its package identity")]
    ArtifactMismatch(String),
    #[error("bootstrap target mismatch")]
    TargetMismatch,
    #[error("bootstrap state is invalid: {0}")]
    InvalidState(&'static str),
    #[error("bootstrap recovery is required: {0}")]
    RecoveryRequired(String),
    #[error("bootstrap operation failed: {0}")]
    Provider(String),
    #[error("bootstrap provider preflight failed: {0}")]
    ProviderPreflight(String),
    #[error("bootstrap requirement check failed: {0}")]
    Requirement(String),
    #[error("prerequisite process unexpectedly initiated a reboot")]
    UnexpectedReboot,
    #[error("prerequisite returned success but the requirement is still unsatisfied")]
    RequirementStillUnsatisfied,
    #[error("bootstrap limit exceeded: {0}")]
    Limit(&'static str),
    #[error("bootstrap I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("bootstrap JSON failed: {0}")]
    Json(#[from] serde_json::Error),
}

fn validate_operation(operation: &BootstrapOperation) -> Result<(), BootstrapError> {
    if operation.name.trim().is_empty() || operation.name.len() > 1024 {
        return Err(BootstrapError::Limit("invalid prerequisite name"));
    }
    if operation.installer.arguments.len() > zup_core::MAX_PREREQUISITE_ARGUMENTS
        || operation.installer.arguments.iter().any(|argument| {
            argument.len() > zup_core::MAX_PREREQUISITE_ARGUMENT_BYTES
                || argument.contains('\0')
                || argument.contains("${")
        })
    {
        return Err(BootstrapError::Limit("invalid prerequisite arguments"));
    }
    let success = operation
        .installer
        .success_exit_codes
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    let reboot = operation
        .installer
        .reboot_exit_codes
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    if success.is_empty() || reboot.is_empty() || success.intersection(&reboot).next().is_some() {
        return Err(BootstrapError::InvalidState(
            "invalid prerequisite exit-code sets",
        ));
    }
    if operation
        .package
        .size()
        .is_some_and(|size| size > zup_core::MAX_PREREQUISITE_PACKAGE_BYTES)
    {
        return Err(BootstrapError::Limit(
            "prerequisite package exceeds the size limit",
        ));
    }
    if let PrerequisitePackage::Remote {
        url: package_url,
        size,
        filename,
        ..
    } = &operation.package
    {
        let parsed = url::Url::parse(package_url)
            .map_err(|_| BootstrapError::InvalidState("invalid remote prerequisite URL"))?;
        if parsed.scheme() != "https"
            || parsed.host_str().is_none()
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.fragment().is_some()
            || size.is_some_and(|size| size > zup_core::MAX_PREREQUISITE_PACKAGE_BYTES)
            || filename.is_empty()
            || filename.contains(['/', '\\', ':', '\0'])
        {
            return Err(BootstrapError::InvalidState("invalid remote prerequisite"));
        }
    }
    Ok(())
}
