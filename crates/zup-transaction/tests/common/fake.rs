//! Comprehensive fake executor for failure-injection tests.
//!
//! Records the order of every executor call so ordering guarantees - prepare
//! before mutation, verify before commit - are asserted, not assumed.

#![allow(dead_code)]

use zup_core::Sha256Digest;
use zup_plan::FileOperationKind;
use zup_plan::{DeploymentNode, NodeKind, OperationId};
use zup_transaction::{OperationExecutor, OperationReceipt, ReconcileResult};

/// One recorded executor call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Call {
    pub verb: &'static str,
    pub id: String,
}

impl Call {
    pub fn new(verb: &'static str, id: &str) -> Self {
        Self {
            verb,
            id: id.to_owned(),
        }
    }

    pub fn label(&self) -> String {
        format!("{}:{}", self.verb, self.id)
    }
}

/// Records call order and supports configurable failures.
#[derive(Debug, Default)]
pub struct FakeExecutor {
    pub calls: Vec<Call>,
    pub prepared: Vec<String>,
    pub applied: Vec<String>,
    pub verified: Vec<String>,
    pub rolled_back: Vec<String>,
    pub reconcile_calls: Vec<String>,
    pub fail_prepare_at: Option<usize>,
    pub fail_verify_at: Option<usize>,
    pub fail_verify_operation: Option<OperationId>,
    pub fail_apply_at: Option<usize>,
    pub fail_rollback_at: Option<usize>,
    pub reconcile_result: ReconcileResult,
}

impl FakeExecutor {
    pub fn new() -> Self {
        Self {
            reconcile_result: ReconcileResult::NotApplied,
            ..Default::default()
        }
    }

    pub fn fail_on_prepare(mut self, index: usize) -> Self {
        self.fail_prepare_at = Some(index);
        self
    }

    pub fn fail_on_verify(mut self, index: usize) -> Self {
        self.fail_verify_at = Some(index);
        self
    }

    pub fn fail_on_apply(mut self, index: usize) -> Self {
        self.fail_apply_at = Some(index);
        self
    }

    pub fn fail_on_rollback(mut self, index: usize) -> Self {
        self.fail_rollback_at = Some(index);
        self
    }

    pub fn reconcile_with(mut self, result: ReconcileResult) -> Self {
        self.reconcile_result = result;
        self
    }

    /// Ordered call labels, e.g. `prepare:ctrl:begin`.
    pub fn labels(&self) -> Vec<String> {
        self.calls.iter().map(Call::label).collect()
    }

    /// Position of the first call with this verb and id, if any.
    pub fn position(&self, verb: &str, id: &OperationId) -> Option<usize> {
        self.calls
            .iter()
            .position(|call| call.verb == verb && call.id == id.as_str())
    }

    /// Every call with this verb, in order.
    pub fn verb(&self, verb: &str) -> Vec<String> {
        self.calls
            .iter()
            .filter(|call| call.verb == verb)
            .map(|call| call.id.clone())
            .collect()
    }

    fn record(&mut self, verb: &'static str, operation: &DeploymentNode) {
        let id = operation.id.as_str().to_owned();
        self.calls.push(Call::new(verb, &id));
    }
}

/// The receipt a node would land with, for building durable fixtures.
pub fn receipt_for(operation: &DeploymentNode) -> OperationReceipt {
    receipt_for_node(operation)
}

fn receipt_for_node(operation: &DeploymentNode) -> OperationReceipt {
    let digest = operation
        .meta
        .expected_sha256
        .unwrap_or_else(|| Sha256Digest::from_bytes([0; 32]));
    let size = operation.meta.expected_size.unwrap_or(1);
    let destination = operation
        .file_destination()
        .map(ToString::to_string)
        .unwrap_or_default();
    let object = if operation.file_destination().is_some_and(|path| {
        path.target().operating_system() == zup_core::TargetOperatingSystem::Linux
    }) {
        zup_core::NativeObjectIdentity::Linux {
            device: 1,
            inode: 2,
        }
    } else {
        zup_core::NativeObjectIdentity::Windows {
            volume: 1,
            file: [2; 16],
        }
    };
    match &operation.kind {
        NodeKind::Cleanup { operation } => OperationReceipt::Cleanup {
            intent: operation.fingerprint(),
            completion: zup_transaction::CleanupCompletion::AlreadyAbsent,
        },
        NodeKind::PersistentDirectory { .. } => {
            panic!("persistent directories require a native filesystem fixture")
        }
        NodeKind::NativeService { operation } => {
            let configuration = operation.configurations().1;
            let registration = if configuration == zup_plan::ServiceState::Absent {
                zup_core::ObservedServiceRegistration::Unclaimed
            } else {
                match operation.registration() {
                    zup_core::NativeServiceRegistration::SourceFile => {
                        zup_core::ObservedServiceRegistration::SourceFile
                    }
                    registration @ zup_core::NativeServiceRegistration::Scm { .. } => {
                        let zup_core::ResourceKey::Service { id } = operation.key() else {
                            panic!("service fixture requires a service key");
                        };
                        zup_core::ObservedServiceRegistration::Scm {
                            claim: registration.claim(id.clone()).unwrap(),
                        }
                    }
                }
            };
            OperationReceipt::NativeService(Box::new(zup_transaction::NativeServiceReceipt {
                intent: operation.fingerprint(),
                configuration,
                registration,
            }))
        }
        NodeKind::NativeCache { operation } => OperationReceipt::NativeCache {
            intent: operation.fingerprint(),
            completion: zup_transaction::NativeCacheCompletion::Completed {
                tool: zup_core::TargetPath::new(
                    *operation.directory.target(),
                    "/usr/bin/update-mime-database",
                )
                .unwrap(),
                object: zup_core::NativeObjectIdentity::Linux {
                    device: 1,
                    inode: 2,
                },
                sha256: digest,
            },
        },
        NodeKind::RetainFile { .. } => {
            let Some(zup_plan::FilePrecondition::Exact {
                object,
                size,
                sha256,
            }) = operation.meta.file_precondition
            else {
                panic!("retained fixture requires an exact instance")
            };
            OperationReceipt::RetainedFile {
                object,
                destination: operation.meta.destination.clone().unwrap(),
                size,
                sha256,
            }
        }
        NodeKind::StageFile { .. } => OperationReceipt::StageFile {
            object,
            staged_path: "staged".into(),
            size,
            sha256: digest,
        },
        NodeKind::FileMutation { delta, .. } => match delta {
            FileOperationKind::Create | FileOperationKind::RestoreOwned => {
                OperationReceipt::CreateFile {
                    object,
                    destination,
                    installed_sha256: digest,
                    installed_size: size,
                    executable: operation.meta.executable.unwrap_or(false),
                    created_directories: Vec::new(),
                }
            }
            _ => OperationReceipt::ReplaceFile {
                object,
                previous_object: match operation.meta.file_precondition {
                    Some(zup_plan::FilePrecondition::Exact { object, .. }) => object,
                    _ => super::owned_instance().object,
                },
                destination,
                previous_sha256: match operation.meta.file_precondition {
                    Some(zup_plan::FilePrecondition::Exact { sha256, .. }) => sha256,
                    _ => digest,
                },
                previous_size: match operation.meta.file_precondition {
                    Some(zup_plan::FilePrecondition::Exact { size, .. }) => size,
                    _ => 1,
                },
                previous_mode: 0o644,
                backup_path: "backup".into(),
                new_sha256: digest,
                new_size: size,
                executable: operation.meta.executable.unwrap_or(false),
            },
        },
        NodeKind::FileRemoval { .. } => {
            let removal = operation
                .meta
                .removal
                .as_ref()
                .expect("file removal metadata");
            OperationReceipt::RemoveFile {
                object: removal.instance.object,
                destination: removal.destination.clone(),
                backup_path: removal.destination.clone(),
                sha256: removal.sha256,
                size: removal.size,
                mode: 0o644,
            }
        }
        NodeKind::BackendOperation { key, .. } | NodeKind::BackendRemoval { key } => {
            OperationReceipt::Backend {
                key: key.clone(),
                payload: b"opaque".to_vec(),
            }
        }
        NodeKind::ServiceControl { operation } => {
            OperationReceipt::ServiceControl(Box::new(zup_transaction::ServiceControlReceipt {
                operation: operation.as_ref().clone(),
                completion: zup_transaction::ServiceControlCompletion::Reached,
                authority: zup_transaction::ServiceControlAuthority::Planned,
                execution_generation: "fake-execution".into(),
                process_scope: operation.process_scope.clone(),
            }))
        }
        NodeKind::Retention { .. }
        | NodeKind::ReserveInstallation
        | NodeKind::PublishInstallation
        | NodeKind::Barrier => OperationReceipt::Control,
        NodeKind::Generation { .. } => {
            panic!("generation fixtures require a materialization executor")
        }
    }
}

impl OperationExecutor for FakeExecutor {
    type Error = String;

    fn prepare(&mut self, operation: &DeploymentNode) -> Result<(), Self::Error> {
        self.record("prepare", operation);
        if self.fail_prepare_at == Some(self.prepared.len()) {
            return Err(format!(
                "injected prepare failure at {}",
                operation.id.as_str()
            ));
        }
        self.prepared.push(operation.id.as_str().to_owned());
        Ok(())
    }

    fn apply(
        &mut self,
        operation: &DeploymentNode,
        _transaction: &zup_transaction::TransactionRecord,
    ) -> Result<OperationReceipt, Self::Error> {
        self.record("apply", operation);
        if self.fail_apply_at == Some(self.applied.len()) {
            return Err(format!(
                "injected apply failure at {}",
                operation.id.as_str()
            ));
        }
        self.applied.push(operation.id.as_str().to_owned());
        if let NodeKind::PersistentDirectory { operation } = &operation.kind {
            let ownership = operation
                .intent
                .previous
                .as_ref()
                .map(|owned| owned.ownership.clone())
                .unwrap_or_else(|| match &operation.intent.binding {
                    zup_plan::PersistentBinding::Filesystem(binding) => {
                        zup_plan::PersistentOwnership::Filesystem {
                            binding: binding.clone(),
                            object: zup_core::NativeObjectIdentity::Windows {
                                volume: 1,
                                file: [3; 16],
                            },
                        }
                    }
                    zup_plan::PersistentBinding::SystemdState(binding) => {
                        zup_plan::PersistentOwnership::SystemdState {
                            binding: binding.clone(),
                        }
                    }
                });
            return Ok(OperationReceipt::PersistentDirectory(Box::new(
                zup_transaction::PersistentDirectoryReceipt {
                    stage: operation.stage,
                    owned: zup_plan::OwnedPersistentDirectory {
                        installation: operation.intent.installation,
                        directory: operation.intent.directory.clone(),
                        ownership,
                        origin: operation
                            .intent
                            .previous
                            .as_ref()
                            .map_or(_transaction.transaction_id, |owned| owned.origin),
                    },
                },
            )));
        }
        Ok(receipt_for_node(operation))
    }

    fn verify(
        &mut self,
        operation: &DeploymentNode,
        _receipt: &OperationReceipt,
    ) -> Result<(), Self::Error> {
        self.record("verify", operation);
        if self.fail_verify_at == Some(self.verified.len())
            || self.fail_verify_operation.as_ref() == Some(&operation.id)
        {
            return Err(format!(
                "injected verify failure at {}",
                operation.id.as_str()
            ));
        }
        self.verified.push(operation.id.as_str().to_owned());
        Ok(())
    }

    fn rollback(
        &mut self,
        operation: &DeploymentNode,
        _receipt: &OperationReceipt,
    ) -> Result<(), Self::Error> {
        self.record("rollback", operation);
        if self.fail_rollback_at == Some(self.rolled_back.len()) {
            return Err(format!(
                "injected rollback failure at {}",
                operation.id.as_str()
            ));
        }
        self.rolled_back.push(operation.id.as_str().to_owned());
        Ok(())
    }

    fn reconcile(
        &mut self,
        operation: &DeploymentNode,
        _receipt: Option<&OperationReceipt>,
        _transaction: &zup_transaction::TransactionRecord,
    ) -> Result<ReconcileResult, Self::Error> {
        self.record("reconcile", operation);
        self.reconcile_calls.push(operation.id.as_str().to_owned());
        Ok(match self.reconcile_result.clone() {
            ReconcileResult::Applied => {
                ReconcileResult::AppliedWithReceipt(receipt_for_node(operation))
            }
            result => result,
        })
    }
}

pub fn op_id(id: &OperationId) -> String {
    id.to_string()
}
