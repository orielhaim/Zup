use zup_transaction::{
    CancellationProbe, NodeKind, OperationExecutor, OperationReceipt, ReconcileResult,
};

use crate::{CancellationHandle, RuntimeEvent, TokenProbe};

pub fn operation_work(node: &zup_transaction::TransactionNode) -> u64 {
    if matches!(
        node.kind,
        NodeKind::StageFile { .. } | NodeKind::FileMutation { .. }
    ) {
        node.meta.expected_size.unwrap_or(1).max(1)
    } else {
        1
    }
}

pub fn operation_action(node: &zup_transaction::TransactionNode) -> &'static str {
    match &node.kind {
        NodeKind::StageFile { .. } | NodeKind::FileMutation { .. } => "Installing files…",
        NodeKind::BackendOperation { .. } | NodeKind::BackendRemoval { .. } => {
            "Updating application settings…"
        }
        NodeKind::FileRemoval { .. } => "Removing files…",
        NodeKind::Barrier => "Finishing…",
    }
}

pub struct EventingExecutor<E, F, G> {
    inner: E,
    emit: F,
    cancelled: G,
    cancel: TokenProbe,
    completed_work: u64,
    total_work: u64,
}

impl<E, F, G> EventingExecutor<E, F, G> {
    pub fn new(
        inner: E,
        total_work: u64,
        cancel: &CancellationHandle,
        emit: F,
        cancelled: G,
    ) -> Self {
        Self {
            inner,
            emit,
            cancelled,
            cancel: cancel.probe(),
            completed_work: 0,
            total_work,
        }
    }

    pub fn into_inner(self) -> E {
        self.inner
    }
}

impl<E, F, G> OperationExecutor for EventingExecutor<E, F, G>
where
    E: OperationExecutor,
    F: Fn(RuntimeEvent) + Send,
    G: Fn() -> E::Error + Send,
{
    type Error = E::Error;

    fn prepare(&mut self, operation: &zup_transaction::TransactionNode) -> Result<(), Self::Error> {
        if self.cancel.is_cancelled() {
            return Err((self.cancelled)());
        }
        self.inner.prepare(operation)
    }

    fn apply(
        &mut self,
        operation: &zup_transaction::TransactionNode,
    ) -> Result<OperationReceipt, Self::Error> {
        if self.cancel.is_cancelled() {
            return Err((self.cancelled)());
        }
        (self.emit)(RuntimeEvent::OperationStarted {
            id: operation.id.to_string(),
        });
        let receipt = self.inner.apply(operation)?;
        self.completed_work = self
            .completed_work
            .saturating_add(operation_work(operation));
        (self.emit)(RuntimeEvent::Progress {
            completed: self.completed_work.min(self.total_work),
            total: self.total_work,
            action: operation_action(operation).into(),
        });
        Ok(receipt)
    }

    fn verify(
        &mut self,
        operation: &zup_transaction::TransactionNode,
        receipt: &OperationReceipt,
    ) -> Result<(), Self::Error> {
        self.inner.verify(operation, receipt)
    }

    fn rollback(
        &mut self,
        operation: &zup_transaction::TransactionNode,
        receipt: &OperationReceipt,
    ) -> Result<(), Self::Error> {
        self.inner.rollback(operation, receipt)
    }

    fn reconcile(
        &mut self,
        operation: &zup_transaction::TransactionNode,
        receipt: Option<&OperationReceipt>,
    ) -> Result<ReconcileResult, Self::Error> {
        self.inner.reconcile(operation, receipt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct RecordingExecutor {
        events: Vec<String>,
    }

    struct Refused(String);

    impl std::fmt::Display for Refused {
        fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(out, "{}", self.0)
        }
    }

    impl std::fmt::Debug for Refused {
        fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(out, "Refused({})", self.0)
        }
    }

    impl OperationExecutor for RecordingExecutor {
        type Error = Refused;

        fn prepare(
            &mut self,
            _operation: &zup_transaction::TransactionNode,
        ) -> Result<(), Self::Error> {
            Ok(())
        }

        fn apply(
            &mut self,
            operation: &zup_transaction::TransactionNode,
        ) -> Result<OperationReceipt, Self::Error> {
            self.events.push(operation.id.to_string());
            Ok(OperationReceipt::Control)
        }

        fn verify(
            &mut self,
            _operation: &zup_transaction::TransactionNode,
            _receipt: &OperationReceipt,
        ) -> Result<(), Self::Error> {
            Ok(())
        }

        fn rollback(
            &mut self,
            _operation: &zup_transaction::TransactionNode,
            _receipt: &OperationReceipt,
        ) -> Result<(), Self::Error> {
            Ok(())
        }

        fn reconcile(
            &mut self,
            _operation: &zup_transaction::TransactionNode,
            _receipt: Option<&OperationReceipt>,
        ) -> Result<ReconcileResult, Self::Error> {
            Ok(ReconcileResult::Applied)
        }
    }

    fn node(id: &str) -> zup_transaction::TransactionNode {
        zup_transaction::TransactionNode {
            id: zup_transaction::OperationId::new(id),
            phase: zup_transaction::Phase::FileMutation,
            kind: NodeKind::Barrier,
            declaration_order: 0,
            meta: Default::default(),
        }
    }

    #[test]
    fn applied_operations_report_progress_in_order() {
        let cancel = CancellationHandle::new();
        let reported = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = reported.clone();
        let mut executor = EventingExecutor::new(
            RecordingExecutor { events: Vec::new() },
            2,
            &cancel,
            move |event| {
                sink.lock().expect("sink").push(event);
            },
            || Refused("cancelled".into()),
        );
        executor.apply(&node("a")).expect("applies");
        executor.apply(&node("b")).expect("applies");
        let reported = reported.lock().expect("sink");
        let progress: Vec<_> = reported
            .iter()
            .filter_map(|event| match event {
                RuntimeEvent::Progress {
                    completed, total, ..
                } => Some((*completed, *total)),
                _ => None,
            })
            .collect();
        assert_eq!(progress, vec![(1, 2), (2, 2)]);
        assert_eq!(
            reported
                .iter()
                .filter(|event| matches!(event, RuntimeEvent::OperationStarted { .. }))
                .count(),
            2
        );
    }

    #[test]
    fn a_cancelled_executor_refuses_before_delegating() {
        let cancel = CancellationHandle::new();
        cancel.cancel();
        let mut executor = EventingExecutor::new(
            RecordingExecutor { events: Vec::new() },
            1,
            &cancel,
            |_| {},
            || Refused("cancelled".into()),
        );
        let error = executor.apply(&node("a")).expect_err("cancelled");
        assert_eq!(error.to_string(), "cancelled");
        assert!(executor.into_inner().events.is_empty());
    }
}
