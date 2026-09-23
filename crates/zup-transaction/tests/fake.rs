//! Comprehensive fake executor for failure-injection tests.

#![allow(dead_code)]

use std::collections::BTreeMap;

use zup_transaction::{
    OperationExecutor, OperationId, OperationReceipt, ReconcileResult, TransactionNode,
};

/// Records apply/rollback order and supports configurable failures.
#[derive(Debug, Default)]
pub struct FakeExecutor {
    pub applied: Vec<String>,
    pub rolled_back: Vec<String>,
    pub reconcile_calls: Vec<String>,
    pub fail_apply_at: Option<usize>,
    pub fail_rollback_at: Option<usize>,
    pub reconcile_result: ReconcileResult,
    pub crash_after_applies: Option<usize>,
    pub irreversible: BTreeMap<String, bool>,
}

impl FakeExecutor {
    pub fn new() -> Self {
        Self {
            reconcile_result: ReconcileResult::NotApplied,
            ..Default::default()
        }
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
}

impl OperationExecutor for FakeExecutor {
    type Error = String;

    fn apply(&mut self, operation: &TransactionNode) -> Result<OperationReceipt, Self::Error> {
        let id = operation.id.as_str().to_owned();
        if self.fail_apply_at == Some(self.applied.len()) {
            return Err(format!("injected apply failure at {id}"));
        }
        if let Some(n) = self.crash_after_applies
            && self.applied.len() >= n
        {
            return Err(format!("simulated crash before apply {id}"));
        }
        self.applied.push(id);
        Ok(OperationReceipt::Control)
    }

    fn rollback(
        &mut self,
        operation: &TransactionNode,
        _receipt: &OperationReceipt,
    ) -> Result<(), Self::Error> {
        let id = operation.id.as_str().to_owned();
        if self.fail_rollback_at == Some(self.rolled_back.len()) {
            return Err(format!("injected rollback failure at {id}"));
        }
        self.rolled_back.push(id);
        Ok(())
    }

    fn reconcile(
        &mut self,
        operation: &TransactionNode,
        _receipt: Option<&OperationReceipt>,
    ) -> Result<ReconcileResult, Self::Error> {
        self.reconcile_calls.push(operation.id.as_str().to_owned());
        Ok(self.reconcile_result.clone())
    }
}

pub fn op_id(id: &OperationId) -> String {
    id.to_string()
}
