use zup_plan::NodeKind;
use zup_transaction::{CancellationProbe, OperationExecutor, OperationReceipt, ReconcileResult};

use crate::{CancellationHandle, RuntimeEvent, TokenProbe};

pub fn operation_phase(node: &zup_plan::DeploymentNode) -> zup_core::OperationPhase {
    use zup_core::OperationPhase;
    match node.kind {
        NodeKind::Cleanup { .. } | NodeKind::Retention { .. } => OperationPhase::Cleanup,
        NodeKind::PersistentDirectory { .. } => OperationPhase::Files,
        NodeKind::Generation { operation, .. } => match operation {
            zup_plan::GenerationOperation::Materialize => OperationPhase::Stage,
            zup_plan::GenerationOperation::Seal | zup_plan::GenerationOperation::Verify => {
                OperationPhase::Verify
            }
            zup_plan::GenerationOperation::Activate => OperationPhase::Activate,
        },
        NodeKind::StageFile { .. }
        | NodeKind::FileMutation { .. }
        | NodeKind::FileRemoval { .. } => OperationPhase::Files,
        NodeKind::RetainFile { .. } => OperationPhase::Verify,
        NodeKind::ServiceControl { .. }
        | NodeKind::NativeService { .. }
        | NodeKind::NativeCache { .. }
        | NodeKind::BackendOperation { .. }
        | NodeKind::BackendRemoval { .. } => OperationPhase::System,
        NodeKind::ReserveInstallation => OperationPhase::Stage,
        NodeKind::Barrier | NodeKind::PublishInstallation => OperationPhase::Finish,
    }
}

pub fn operation_action(node: &zup_plan::DeploymentNode) -> &'static str {
    match &node.kind {
        NodeKind::Cleanup { operation }
            if matches!(operation.candidate, zup_plan::CleanupCandidate::History(_)) =>
        {
            "Cleaning unreferenced deployment history..."
        }
        NodeKind::Cleanup { .. } => "Cleaning retired payload...",
        NodeKind::Retention { .. } => "Retiring rollback references...",
        NodeKind::PersistentDirectory { operation } => {
            if operation.intent.previous.is_some() {
                "Preserving application data..."
            } else {
                "Preparing application data..."
            }
        }
        NodeKind::NativeCache { .. } => "Updating native integration cache...",
        NodeKind::RetainFile { .. } => "Verifying retained file...",
        NodeKind::Generation { operation, .. } => match operation {
            zup_plan::GenerationOperation::Materialize => "Preparing application...",
            zup_plan::GenerationOperation::Seal | zup_plan::GenerationOperation::Verify => {
                "Verifying application..."
            }
            zup_plan::GenerationOperation::Activate => "Activating application...",
        },
        NodeKind::ServiceControl { operation } => match operation.stage {
            zup_plan::ServiceControlStage::Quiesce => "Stopping service…",
            zup_plan::ServiceControlStage::Restore => "Starting service…",
        },
        NodeKind::NativeService { operation } => match operation.as_ref() {
            zup_plan::NativeServiceOperation::Configure { service, .. }
                if service.kind == zup_plan::IntegrationChangeKind::NoOp =>
            {
                "Verifying service registration..."
            }
            zup_plan::NativeServiceOperation::Configure { .. } => "Configuring native service...",
            zup_plan::NativeServiceOperation::Remove { .. } => "Removing native service...",
        },
        NodeKind::StageFile { .. } | NodeKind::FileMutation { .. } => "Installing files…",
        NodeKind::BackendOperation { .. } | NodeKind::BackendRemoval { .. } => {
            "Updating application settings…"
        }
        NodeKind::FileRemoval { .. } => "Removing files…",
        NodeKind::ReserveInstallation => "Reserving installation ownership...",
        NodeKind::PublishInstallation => "Publishing installation record...",
        NodeKind::Barrier => "Finishing…",
    }
}

pub fn operation_result_action(
    node: &zup_plan::DeploymentNode,
    receipt: &OperationReceipt,
) -> &'static str {
    match receipt {
        OperationReceipt::Cleanup {
            completion: zup_transaction::CleanupCompletion::Removed,
            ..
        } if matches!(&node.kind, NodeKind::Cleanup { operation } if matches!(operation.candidate, zup_plan::CleanupCandidate::History(_))) => {
            "Collected unreferenced deployment history"
        }
        OperationReceipt::Cleanup {
            completion: zup_transaction::CleanupCompletion::AlreadyAbsent,
            ..
        } if matches!(&node.kind, NodeKind::Cleanup { operation } if matches!(operation.candidate, zup_plan::CleanupCandidate::History(_))) => {
            "Deployment history was already absent"
        }
        OperationReceipt::Cleanup {
            completion: zup_transaction::CleanupCompletion::Removed,
            ..
        } => "Collected unused owned payload",
        OperationReceipt::Cleanup {
            completion: zup_transaction::CleanupCompletion::AlreadyAbsent,
            ..
        } => "Retired payload was already absent",
        OperationReceipt::Cleanup {
            completion: zup_transaction::CleanupCompletion::Deferred { reason },
            ..
        } => match reason {
            zup_transaction::CleanupDeferral::InUse => {
                "Retained payload because a native consumer is using it"
            }
            zup_transaction::CleanupDeferral::UntrackedReaders => {
                "Retained generation because its reader lifetime is not declared"
            }
            zup_transaction::CleanupDeferral::UnsupportedFilesystem => {
                "Retained payload because this filesystem cannot prove safe collection"
            }
            zup_transaction::CleanupDeferral::FenceUnavailable => {
                "Retained payload because a native consumer fence is unavailable"
            }
            zup_transaction::CleanupDeferral::ReferencedMetadata => {
                "Retained transaction history until its metadata collection completes"
            }
        },
        _ => operation_action(node),
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

    fn prepare_compensation(
        &mut self,
        operation: &zup_plan::DeploymentNode,
        receipt: &OperationReceipt,
    ) -> Result<OperationReceipt, Self::Error> {
        self.inner.prepare_compensation(operation, receipt)
    }

    fn prepare(&mut self, operation: &zup_plan::DeploymentNode) -> Result<(), Self::Error> {
        if self.cancel.is_cancelled() {
            return Err((self.cancelled)());
        }
        self.inner.prepare(operation)
    }

    fn apply(
        &mut self,
        operation: &zup_plan::DeploymentNode,
        transaction: &zup_transaction::TransactionRecord,
    ) -> Result<OperationReceipt, Self::Error> {
        if self.cancel.is_cancelled() {
            return Err((self.cancelled)());
        }
        (self.emit)(RuntimeEvent::OperationStarted {
            id: operation.id.to_string(),
        });
        let receipt = self.inner.apply(operation, transaction)?;
        self.completed_work = self.completed_work.saturating_add(operation.work());
        (self.emit)(RuntimeEvent::Progress {
            phase: operation_phase(operation),
            completed: self.completed_work.min(self.total_work),
            total: self.total_work,
            action: operation_result_action(operation, &receipt).into(),
        });
        Ok(receipt)
    }

    fn verify(
        &mut self,
        operation: &zup_plan::DeploymentNode,
        receipt: &OperationReceipt,
    ) -> Result<(), Self::Error> {
        self.inner.verify(operation, receipt)
    }

    fn rollback(
        &mut self,
        operation: &zup_plan::DeploymentNode,
        receipt: &OperationReceipt,
    ) -> Result<(), Self::Error> {
        self.inner.rollback(operation, receipt)
    }

    fn reconcile(
        &mut self,
        operation: &zup_plan::DeploymentNode,
        receipt: Option<&OperationReceipt>,
        transaction: &zup_transaction::TransactionRecord,
    ) -> Result<ReconcileResult, Self::Error> {
        self.inner.reconcile(operation, receipt, transaction)
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

        fn prepare(&mut self, _operation: &zup_plan::DeploymentNode) -> Result<(), Self::Error> {
            Ok(())
        }

        fn apply(
            &mut self,
            operation: &zup_plan::DeploymentNode,
            _transaction: &zup_transaction::TransactionRecord,
        ) -> Result<OperationReceipt, Self::Error> {
            self.events.push(operation.id.to_string());
            Ok(OperationReceipt::Control)
        }

        fn verify(
            &mut self,
            _operation: &zup_plan::DeploymentNode,
            _receipt: &OperationReceipt,
        ) -> Result<(), Self::Error> {
            Ok(())
        }

        fn rollback(
            &mut self,
            _operation: &zup_plan::DeploymentNode,
            _receipt: &OperationReceipt,
        ) -> Result<(), Self::Error> {
            Ok(())
        }

        fn reconcile(
            &mut self,
            _operation: &zup_plan::DeploymentNode,
            _receipt: Option<&OperationReceipt>,
            _transaction: &zup_transaction::TransactionRecord,
        ) -> Result<ReconcileResult, Self::Error> {
            Ok(ReconcileResult::Applied)
        }
    }

    fn transaction() -> zup_transaction::TransactionRecord {
        let mut effects = zup_plan::DeploymentEffects::new(
            zup_core::InstallationId::new_v7(),
            crate::requirements_fixture::requirements(
                zup_core::Target::LINUX_X64,
                zup_core::SelectedScope::User,
                "example.application",
                "1.0.0",
            ),
        );
        effects.install_directory =
            Some(zup_core::TargetPath::new(zup_core::Target::LINUX_X64, "/opt/test").unwrap());
        zup_transaction::TransactionRecord::new(
            zup_core::TransactionId::new_v7(),
            zup_core::AppId::new("example.events").unwrap(),
            zup_core::SelectedScope::User,
            "1.0.0".parse().unwrap(),
            zup_plan::compile_deployment(&effects).unwrap(),
        )
    }

    fn node(id: &str) -> zup_plan::DeploymentNode {
        zup_plan::DeploymentNode {
            id: zup_plan::OperationId::new(id),
            phase: zup_plan::Phase::FileMutation,
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
        executor.apply(&node("a"), &transaction()).expect("applies");
        executor.apply(&node("b"), &transaction()).expect("applies");
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
        let error = executor
            .apply(&node("a"), &transaction())
            .expect_err("cancelled");
        assert_eq!(error.to_string(), "cancelled");
        assert!(executor.into_inner().events.is_empty());
    }
}
