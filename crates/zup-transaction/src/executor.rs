use serde::{Deserialize, Serialize};
use zup_core::TargetPath;
use zup_core::{ResourceKey, Sha256Digest};

use zup_plan::DeploymentNode;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ReconcileResult {
    #[default]
    NotApplied,
    /// Safe to resume, but effects remain and cannot be skipped during rollback.
    Incomplete,
    Applied,
    AppliedWithReceipt(OperationReceipt),
    Ambiguous,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RollbackReconcileResult {
    Restored,
    Retry,
    Ambiguous,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum NativeCacheCompletion {
    Completed {
        tool: TargetPath,
        object: zup_core::NativeObjectIdentity,
        sha256: Sha256Digest,
    },
    CompensationOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CleanupDeferral {
    UntrackedReaders,
    InUse,
    UnsupportedFilesystem,
    FenceUnavailable,
    ReferencedMetadata,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum CleanupCompletion {
    Removed,
    AlreadyAbsent,
    Deferred { reason: CleanupDeferral },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum OperationReceipt {
    Cleanup {
        intent: Sha256Digest,
        completion: CleanupCompletion,
    },
    PersistentDirectory(Box<crate::PersistentDirectoryReceipt>),
    NativeCache {
        intent: Sha256Digest,
        completion: NativeCacheCompletion,
    },
    Generation(Box<crate::GenerationReceipt>),
    ServiceControl(Box<crate::ServiceControlReceipt>),
    NativeService(Box<crate::NativeServiceReceipt>),
    NativeServiceCompensation {
        intent: Sha256Digest,
    },
    Control,
    RetainedFile {
        object: zup_core::NativeObjectIdentity,
        destination: TargetPath,
        size: u64,
        sha256: Sha256Digest,
    },
    StageFile {
        object: zup_core::NativeObjectIdentity,
        staged_path: String,
        size: u64,
        sha256: Sha256Digest,
    },
    CreateFile {
        object: zup_core::NativeObjectIdentity,
        destination: String,
        installed_sha256: Sha256Digest,
        installed_size: u64,
        executable: bool,
        created_directories: Vec<String>,
    },
    ReplaceFile {
        previous_object: zup_core::NativeObjectIdentity,
        object: zup_core::NativeObjectIdentity,
        destination: String,
        previous_sha256: Sha256Digest,
        previous_size: u64,
        previous_mode: u32,
        backup_path: String,
        new_sha256: Sha256Digest,
        new_size: u64,
        executable: bool,
    },
    RemoveFile {
        object: zup_core::NativeObjectIdentity,
        destination: TargetPath,
        backup_path: TargetPath,
        sha256: Sha256Digest,
        size: u64,
        mode: u32,
    },
    Backend {
        key: ResourceKey,
        payload: Vec<u8>,
    },
}

impl OperationReceipt {
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::PersistentDirectory(receipt) => {
                if let zup_plan::PersistentOwnership::Filesystem { binding, object } =
                    &receipt.owned.ownership
                    && binding.path.target().operating_system() != object.operating_system()
                {
                    return Err("persistent receipt native identity differs from its target".into());
                }
            }
            Self::NativeCache { completion, .. } => {
                if let NativeCacheCompletion::Completed { tool, object, .. } = completion
                    && (tool.target().operating_system() != zup_core::TargetOperatingSystem::Linux
                        || !matches!(object, zup_core::NativeObjectIdentity::Linux { .. }))
                {
                    return Err("native cache tool evidence is not Linux-native".into());
                }
            }
            Self::Generation(receipt) => receipt.validate()?,
            Self::ServiceControl(receipt) => {
                if let crate::ServiceControlAuthority::ReobservedQuiescent { manager_generation } =
                    &receipt.authority
                    && (manager_generation.is_empty()
                        || manager_generation.len() > 256
                        || manager_generation.chars().any(char::is_control)
                        || receipt.operation.intent != zup_plan::ServiceControlIntent::Transition
                        || receipt.execution_generation != "stopped"
                        || match receipt.operation.stage {
                            zup_plan::ServiceControlStage::Quiesce => {
                                zup_plan::disabled_startup(&receipt.operation.configuration)
                                    .as_ref()
                                    != Some(&receipt.operation.configuration)
                            }
                            zup_plan::ServiceControlStage::Restore => {
                                receipt.completion
                                    != crate::ServiceControlCompletion::CompensationOnly
                                    || zup_plan::disabled_startup(
                                        &receipt.operation.before_configuration,
                                    )
                                    .as_ref()
                                        != Some(&receipt.operation.before_configuration)
                            }
                        })
                {
                    return Err("invalid reobserved service compensation authority".into());
                }
                receipt.operation.validate()?;
                receipt
                    .process_scope
                    .validate(receipt.operation.registration)?;
                if let zup_plan::ServiceScopeObservation::WindowsJob {
                    state:
                        zup_plan::WindowsJobState::Member {
                            execution_generation,
                        },
                    ..
                } = &receipt.process_scope
                    && execution_generation != &receipt.execution_generation
                {
                    return Err("service scope receipt refers to a different execution".into());
                }
                if matches!(
                    receipt.process_scope,
                    zup_plan::ServiceScopeObservation::WindowsJob {
                        state: zup_plan::WindowsJobState::Empty
                            | zup_plan::WindowsJobState::Occupied { .. },
                        ..
                    }
                ) && receipt.execution_generation != "stopped"
                {
                    return Err("non-running service scope has a running execution receipt".into());
                }

                if receipt.completion == crate::ServiceControlCompletion::CompensationOnly
                    && receipt.operation.stage != zup_plan::ServiceControlStage::Restore
                {
                    return Err("only an incomplete startup can carry compensation evidence".into());
                }
                if receipt.execution_generation.is_empty()
                    || receipt.execution_generation.len() > 256
                    || receipt.execution_generation.chars().any(char::is_control)
                {
                    return Err("invalid service execution generation".into());
                }
            }
            Self::Backend { key, payload } => {
                if !matches!(key, ResourceKey::Backend { .. }) {
                    return Err("backend receipt key is not a backend resource".into());
                }
                if payload.len() > zup_plan::MAX_BACKEND_PAYLOAD_BYTES {
                    return Err("backend receipt payload exceeds the transaction limit".into());
                }
            }
            Self::StageFile { .. } | Self::RetainedFile { .. } | Self::CreateFile { .. } => {}
            Self::ReplaceFile { previous_mode, .. } => {
                if *previous_mode & !0o7777 != 0 {
                    return Err("invalid replaced file mode".to_string());
                }
            }
            Self::RemoveFile { mode, .. } => {
                if *mode & !0o7777 != 0 {
                    return Err("invalid removed file mode".to_string());
                }
            }
            Self::Cleanup { .. }
            | Self::NativeService(_)
            | Self::NativeServiceCompensation { .. }
            | Self::Control => {}
        }
        Ok(())
    }
}

pub trait OperationExecutor {
    type Error;

    fn prepare(&mut self, operation: &DeploymentNode) -> Result<(), Self::Error>;

    fn apply(
        &mut self,
        operation: &DeploymentNode,
        transaction: &crate::TransactionRecord,
    ) -> Result<OperationReceipt, Self::Error>;

    fn verify(
        &mut self,
        operation: &DeploymentNode,
        receipt: &OperationReceipt,
    ) -> Result<(), Self::Error>;

    fn rollback(
        &mut self,
        operation: &DeploymentNode,
        receipt: &OperationReceipt,
    ) -> Result<(), Self::Error>;

    fn prepare_compensation(
        &mut self,
        _operation: &DeploymentNode,
        receipt: &OperationReceipt,
    ) -> Result<OperationReceipt, Self::Error> {
        Ok(receipt.clone())
    }

    fn reconcile(
        &mut self,
        operation: &DeploymentNode,
        receipt: Option<&OperationReceipt>,
        transaction: &crate::TransactionRecord,
    ) -> Result<ReconcileResult, Self::Error>;

    fn reconcile_rollback(
        &mut self,
        _operation: &DeploymentNode,
        _receipt: &OperationReceipt,
    ) -> Result<RollbackReconcileResult, Self::Error> {
        Ok(RollbackReconcileResult::Ambiguous)
    }
}

pub trait CancellationProbe {
    fn is_cancelled(&self) -> bool;
}
