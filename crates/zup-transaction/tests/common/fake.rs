//! Comprehensive fake executor for failure-injection tests.
//!
//! Records the order of every executor call so ordering guarantees - prepare
//! before mutation, verify before commit - are asserted, not assumed.

#![allow(dead_code)]

use zup_core::Sha256Digest;
use zup_transaction::{
    FileDelta, NodeKind, OperationExecutor, OperationId, OperationReceipt, ReconcileResult,
    TransactionNode,
};

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

    fn record(&mut self, verb: &'static str, operation: &TransactionNode) {
        let id = operation.id.as_str().to_owned();
        self.calls.push(Call::new(verb, &id));
    }
}

/// The receipt a node would land with, for building durable fixtures.
pub fn receipt_for(operation: &TransactionNode) -> OperationReceipt {
    receipt_for_node(operation)
}

fn receipt_for_node(operation: &TransactionNode) -> OperationReceipt {
    let digest = Sha256Digest::from_bytes([0; 32]).to_hex();
    match &operation.kind {
        NodeKind::StageFile { .. } => OperationReceipt::StageFile {
            staged_path: "staged".into(),
            size: 1,
            sha256: digest,
        },
        NodeKind::FileMutation { delta, .. } => match delta {
            FileDelta::Create => OperationReceipt::CreateFile {
                destination: "destination".into(),
                installed_sha256: digest,
                installed_size: 1,
                executable: operation.meta.executable.unwrap_or(false),
                created_directories: Vec::new(),
            },
            _ => OperationReceipt::ReplaceFile {
                destination: "destination".into(),
                previous_sha256: digest.clone(),
                previous_size: 1,
                backup_path: "backup".into(),
                new_sha256: digest,
                new_size: 1,
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
                destination: removal.destination.clone(),
                backup_path: removal.destination.clone(),
                sha256: Sha256Digest::from_bytes([0; 32]),
                size: 1,
            }
        }
        NodeKind::BackendOperation { key, .. } | NodeKind::BackendRemoval { key } => {
            OperationReceipt::Backend {
                key: key.clone(),
                payload: b"opaque".to_vec(),
            }
        }
        NodeKind::Barrier => OperationReceipt::Control,
    }
}

impl OperationExecutor for FakeExecutor {
    type Error = String;

    fn prepare(&mut self, operation: &TransactionNode) -> Result<(), Self::Error> {
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

    fn apply(&mut self, operation: &TransactionNode) -> Result<OperationReceipt, Self::Error> {
        self.record("apply", operation);
        if self.fail_apply_at == Some(self.applied.len()) {
            return Err(format!(
                "injected apply failure at {}",
                operation.id.as_str()
            ));
        }
        self.applied.push(operation.id.as_str().to_owned());
        Ok(receipt_for_node(operation))
    }

    fn verify(
        &mut self,
        operation: &TransactionNode,
        _receipt: &OperationReceipt,
    ) -> Result<(), Self::Error> {
        self.record("verify", operation);
        if self.fail_verify_at == Some(self.verified.len()) {
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
        operation: &TransactionNode,
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
        operation: &TransactionNode,
        _receipt: Option<&OperationReceipt>,
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
