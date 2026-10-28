use zup_transaction::{
    NodeKind, OperationExecutor, OperationReceipt, ReconcileResult, StoreError, TransactionId,
    TransactionNode, TransactionPhase, TransactionRecord, TransactionStore,
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Boundary {
    BeforeMutation,
    AfterMutation,
    BeforeJournal,
    AfterJournal,
    BeforeCommit,
    AfterCommit,
    DuringRollback,
    BeforeLedger,
    AfterLedger,
}

impl Boundary {
    pub fn reach(self, selected: Self) {
        if self == selected {
            std::process::exit(77);
        }
    }
}

pub struct InterruptedStore<S> {
    pub inner: S,
    pub boundary: Boundary,
}

impl<S: TransactionStore> TransactionStore for InterruptedStore<S> {
    fn create(&self, record: &TransactionRecord) -> Result<(), StoreError> {
        self.inner.create(record)
    }

    fn load(&self, id: &TransactionId) -> Result<TransactionRecord, StoreError> {
        self.inner.load(id)
    }

    fn compare_and_swap(
        &self,
        revision: u64,
        record: &TransactionRecord,
    ) -> Result<(), StoreError> {
        use Boundary::*;
        let committing = record.phase == TransactionPhase::Committed;
        self.boundary.reach(if committing {
            BeforeCommit
        } else {
            BeforeJournal
        });
        self.inner.compare_and_swap(revision, record)?;
        self.boundary.reach(if committing {
            AfterCommit
        } else {
            AfterJournal
        });
        Ok(())
    }
}

pub struct InterruptedExecutor<E> {
    pub inner: E,
    pub boundary: Boundary,
}

impl<E: OperationExecutor> OperationExecutor for InterruptedExecutor<E>
where
    E::Error: std::fmt::Display,
{
    type Error = String;

    fn prepare(&mut self, node: &TransactionNode) -> Result<(), String> {
        self.inner.prepare(node).map_err(|error| error.to_string())
    }

    fn apply(&mut self, node: &TransactionNode) -> Result<OperationReceipt, String> {
        let mutating = matches!(
            node.kind,
            NodeKind::FileMutation { .. } | NodeKind::FileRemoval { .. }
        );
        if mutating {
            self.boundary.reach(Boundary::BeforeMutation);
        }
        let receipt = self.inner.apply(node).map_err(|error| error.to_string())?;
        if mutating {
            self.boundary.reach(Boundary::AfterMutation);
        }
        Ok(receipt)
    }

    fn verify(&mut self, node: &TransactionNode, receipt: &OperationReceipt) -> Result<(), String> {
        if self.boundary == Boundary::DuringRollback
            && matches!(node.kind, NodeKind::FileMutation { .. })
        {
            return Err("injected verification failure".into());
        }
        self.inner
            .verify(node, receipt)
            .map_err(|error| error.to_string())
    }

    fn rollback(
        &mut self,
        node: &TransactionNode,
        receipt: &OperationReceipt,
    ) -> Result<(), String> {
        self.inner
            .rollback(node, receipt)
            .map_err(|error| error.to_string())?;
        self.boundary.reach(Boundary::DuringRollback);
        Ok(())
    }

    fn reconcile(
        &mut self,
        node: &TransactionNode,
        receipt: Option<&OperationReceipt>,
    ) -> Result<ReconcileResult, String> {
        self.inner
            .reconcile(node, receipt)
            .map_err(|error| error.to_string())
    }
}
