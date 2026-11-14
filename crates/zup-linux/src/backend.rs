use std::collections::BTreeMap;
use std::path::PathBuf;

use zup_core::SelectedScope;
use zup_runtime::{
    EventingExecutor, InstallOutcome, Operation, OperationSelection, RuntimeBackend,
    RuntimeControl, RuntimeEvent, RuntimeFuture, RuntimeOperation, RuntimeRequest, SessionError,
};
use zup_transaction::{
    FilesystemTransactionStore, ResourceLock, TransactionCoordinator, TransactionOutcome,
    TransactionStore,
};

use crate::carrier::Carrier;
use crate::error::ExecError;
use crate::executor::LinuxFileExecutor;
use crate::ledger::LinuxInstallationStore;
use crate::lifecycle::LinuxOutcome;

#[derive(Clone)]
pub struct LinuxRuntimeBackend {
    pub installer: PathBuf,
    pub operation: Operation,
    pub selection: OperationSelection,
    pub generated: BTreeMap<String, Vec<u8>>,
}

pub struct ExecutionReport {
    pub outcome: InstallOutcome,
    pub recovery_transaction: Option<String>,
}

impl From<LinuxOutcome> for ExecutionReport {
    fn from(outcome: LinuxOutcome) -> Self {
        match outcome {
            LinuxOutcome::Assessed(assessment) => Self {
                outcome: InstallOutcome::RecoveryAssessed { assessment },
                recovery_transaction: None,
            },
            LinuxOutcome::Committed { .. } => Self {
                outcome: InstallOutcome::Committed,
                recovery_transaction: None,
            },
            LinuxOutcome::RolledBack => Self {
                outcome: InstallOutcome::RolledBack,
                recovery_transaction: None,
            },
            LinuxOutcome::RecoveryRequired {
                transaction,
                detail,
            } => Self {
                outcome: InstallOutcome::RecoveryRequired {
                    transaction,
                    detail,
                },
                recovery_transaction: transaction.map(|id| id.to_string()),
            },
            LinuxOutcome::CommittedPendingReconciliation {
                transaction,
                detail,
            } => Self {
                outcome: InstallOutcome::CommittedPendingReconciliation {
                    transaction: zup_core::TransactionId::from_uuid(transaction),
                    detail,
                },
                recovery_transaction: Some(transaction.to_string()),
            },
            LinuxOutcome::Busy => Self {
                outcome: InstallOutcome::Busy {
                    operation: "linux install",
                },
                recovery_transaction: None,
            },
        }
    }
}

impl LinuxRuntimeBackend {
    pub fn execute_blocking(
        &self,
        request: &RuntimeRequest,
        control: &RuntimeControl,
    ) -> Result<ExecutionReport, SessionError> {
        if control.cancellation.is_cancelled() {
            return Ok(ExecutionReport {
                outcome: InstallOutcome::Cancelled,
                recovery_transaction: None,
            });
        }
        if request.scope == SelectedScope::Machine
            && rustix::process::geteuid().as_raw() != 0
            && !control.policy.allows_authorization()
        {
            return Err(SessionError::AuthorizationRequired);
        }
        if matches!(request.operation, RuntimeOperation::InspectRecovery) {
            if request.scope == SelectedScope::Machine && rustix::process::geteuid().as_raw() != 0 {
                return Err(SessionError::AuthorizationRequired);
            }
            crate::run::refuse_redirected_hierarchy(&request.state_root)
                .map_err(|error| SessionError::Transaction(error.to_string()))?;
            let key = ResourceLock::lock_key(request.app_id.as_str(), scope_token(request.scope));
            let _lock = ResourceLock::try_acquire(&request.state_root, &key)
                .map_err(|error| SessionError::Transaction(error.to_string()))?
                .ok_or(SessionError::InstallationBusy)?;
            let target = FilesystemTransactionStore::private_at(&request.state_root)
                .pending_recovery(&request.app_id, request.scope)
                .map_err(|error| SessionError::Transaction(error.to_string()))?
                .map(|target| target.summary());
            if target
                .as_ref()
                .is_some_and(|target| target.target != request.target)
            {
                return Err(SessionError::Protocol(
                    "recovery inspection target mismatch".into(),
                ));
            }
            return Ok(ExecutionReport {
                outcome: InstallOutcome::RecoveryInspected {
                    target,
                    active: LinuxInstallationStore::new(&request.state_root)
                        .observe(&request.app_id, request.scope)
                        .ok()
                        .flatten()
                        .and_then(|installation| {
                            FilesystemTransactionStore::private_at(&request.state_root)
                                .active_launchers(&installation)
                                .ok()
                                .flatten()
                        })
                        .map(Box::new),
                },
                recovery_transaction: None,
            });
        }
        if let Some(id) = request.operation.recovery_transaction() {
            return self.execute_recovery(request, control, &id);
        }
        if let Some(bootstrap) = request
            .operation
            .deployment()
            .and_then(|deployment| deployment.bootstrap.as_ref())
        {
            validate_bootstrap_binding(&self.installer, request, bootstrap)
                .map_err(SessionError::Protocol)?;
            crate::prerequisites::execute(bootstrap, control)
                .map_err(SessionError::Prerequisite)?;
            if control.cancellation.is_cancelled() {
                return Ok(ExecutionReport {
                    outcome: InstallOutcome::Cancelled,
                    recovery_transaction: None,
                });
            }
        }
        match request.scope {
            SelectedScope::User => self.execute_user(request, control),
            SelectedScope::Machine => self.execute_machine(request, control),
        }
    }

    fn execute_recovery(
        &self,
        request: &RuntimeRequest,
        control: &RuntimeControl,
        id: &zup_core::TransactionId,
    ) -> Result<ExecutionReport, SessionError> {
        if request.scope == SelectedScope::Machine {
            let carrier = Carrier::open(&self.installer)
                .map_err(|error| SessionError::Transaction(error.to_string()))?;
            let app = &carrier.package().plan().installer.app;
            if app.id != request.app_id
                || app.version != request.app_version
                || *carrier.target() != request.target
            {
                return Err(SessionError::Protocol(
                    "recovery package identity differs from the runtime request".into(),
                ));
            }
            return crate::elevate::recover_machine(
                &self.installer,
                &request.state_root,
                id.as_uuid(),
                request.operation.recovery_request(),
                control,
            )
            .map(Into::into)
            .map_err(execution_error);
        }
        let events = &control.events;
        crate::run::refuse_redirected_hierarchy(&request.state_root)
            .map_err(|error| SessionError::Transaction(error.to_string()))?;
        let lock_key = ResourceLock::lock_key(request.app_id.as_str(), scope_token(request.scope));
        let _lock = match ResourceLock::try_acquire(&request.state_root, &lock_key) {
            Ok(Some(lock)) => lock,
            Ok(None) => {
                return Ok(ExecutionReport {
                    outcome: InstallOutcome::Busy {
                        operation: "linux install",
                    },
                    recovery_transaction: None,
                });
            }
            Err(error) => return Err(SessionError::Transaction(error.to_string())),
        };
        let store = FilesystemTransactionStore::private_at(&request.state_root);
        let record = store
            .load(id)
            .map_err(|error| SessionError::Transaction(error.to_string()))?;
        if record.app_id != request.app_id
            || record.scope != request.scope
            || record.app_version != request.app_version
            || record.target != request.target
        {
            return Err(SessionError::Transaction(
                "recovery identity mismatch".into(),
            ));
        }
        let ledger_store = LinuxInstallationStore::new(&request.state_root);
        let _ = events.send(RuntimeEvent::PlanPrepared {
            plan: Box::new(record.plan.clone()),
        });
        match record.phase {
            zup_transaction::TransactionPhase::Committed if record.cleanup_complete() => {
                return match ledger_store.reconcile_installation(&record, request.scope) {
                    Ok(_) => {
                        observe_reap(&request.state_root, &request.app_id, request.scope, &_lock);
                        Ok(ExecutionReport {
                            outcome: InstallOutcome::Committed,
                            recovery_transaction: None,
                        })
                    }
                    Err(error) => Ok(ExecutionReport {
                        outcome: InstallOutcome::CommittedPendingReconciliation {
                            transaction: record.transaction_id,
                            detail: error.to_string(),
                        },
                        recovery_transaction: Some(record.transaction_id.to_string()),
                    }),
                };
            }
            zup_transaction::TransactionPhase::RolledBack => {
                observe_reap(&request.state_root, &request.app_id, request.scope, &_lock);
                return Ok(ExecutionReport {
                    outcome: InstallOutcome::RolledBack,
                    recovery_transaction: None,
                });
            }
            _ => {}
        }
        let generated = if crate::integration::phase_keeps_generated(record.phase) {
            crate::integration::generated_for_plan(
                &request.state_root,
                &record.app_id,
                record.scope,
                &_lock,
                &record.plan,
            )
            .map_err(|error| SessionError::Transaction(error.to_string()))?
        } else {
            std::collections::BTreeMap::new()
        };
        let carrier = Carrier::open(&self.installer)
            .map_err(|error| SessionError::Transaction(error.to_string()))?;
        let payload = crate::run::RunnerPayload::open(&carrier, carrier.executable(), generated)
            .map_err(|error| SessionError::Transaction(error.to_string()))?;
        let mut executor = LinuxFileExecutor::new().with_payload(payload);
        executor
            .register_plan(&record.plan)
            .map_err(|error| SessionError::Transaction(error.to_string()))?;
        executor.set_state_authority(
            request.state_root.clone(),
            request.app_id.clone(),
            request.scope,
        );
        executor.set_transaction_id(record.transaction_id);
        let total_work = record.plan.total_work();
        let mut executor = EventingExecutor::new(
            executor,
            total_work,
            &control.cancellation,
            |event| {
                let _ = events.send(event);
            },
            || ExecError::Cancelled,
        );
        match zup_transaction::recover_requested(
            record,
            &store,
            &mut executor,
            &ledger_store,
            request.operation.recovery_request(),
        ) {
            Ok((_, TransactionOutcome::Assessed(assessment))) => Ok(ExecutionReport {
                outcome: InstallOutcome::RecoveryAssessed { assessment },
                recovery_transaction: Some(id.to_string()),
            }),
            Ok((record, TransactionOutcome::Committed)) => {
                match ledger_store.reconcile_installation(&record, request.scope) {
                    Ok(_) => {
                        observe_reap(&request.state_root, &request.app_id, request.scope, &_lock);
                        Ok(ExecutionReport {
                            outcome: InstallOutcome::Committed,
                            recovery_transaction: None,
                        })
                    }
                    Err(error) => Ok(ExecutionReport {
                        outcome: InstallOutcome::CommittedPendingReconciliation {
                            transaction: record.transaction_id,
                            detail: error.to_string(),
                        },
                        recovery_transaction: Some(record.transaction_id.to_string()),
                    }),
                }
            }
            Ok((_record, TransactionOutcome::RolledBack)) => {
                observe_reap(&request.state_root, &request.app_id, request.scope, &_lock);
                Ok(ExecutionReport {
                    outcome: InstallOutcome::RolledBack,
                    recovery_transaction: None,
                })
            }
            Ok((record, TransactionOutcome::RecoveryRequired)) => Ok(ExecutionReport {
                outcome: InstallOutcome::recovery(&record),
                recovery_transaction: Some(record.transaction_id.to_string()),
            }),
            Err(error) => Ok(ExecutionReport {
                outcome: InstallOutcome::recovery_failure(&store, *id, error.to_string()),
                recovery_transaction: Some(id.to_string()),
            }),
        }
    }

    fn execute_user(
        &self,
        request: &RuntimeRequest,
        control: &RuntimeControl,
    ) -> Result<ExecutionReport, SessionError> {
        let RuntimeOperation::Deployment(deployment) = &request.operation else {
            return Err(SessionError::Protocol(
                "deployment runner requires a deployment request".into(),
            ));
        };
        let plan = &deployment.plan;
        let events = &control.events;
        let _ = events.send(RuntimeEvent::StateChanged {
            state: zup_runtime::RuntimeState::Executing,
        });
        crate::run::refuse_redirected_hierarchy(&request.state_root)
            .map_err(|error| SessionError::Transaction(error.to_string()))?;
        let carrier = Carrier::open(&self.installer)
            .map_err(|error| SessionError::Transaction(error.to_string()))?;
        let lock_key = ResourceLock::lock_key(request.app_id.as_str(), scope_token(request.scope));
        let _lock = match ResourceLock::try_acquire(&request.state_root, &lock_key) {
            Ok(Some(lock)) => lock,
            Ok(None) => {
                return Ok(ExecutionReport {
                    outcome: InstallOutcome::Busy {
                        operation: "linux install",
                    },
                    recovery_transaction: None,
                });
            }
            Err(error) => return Err(SessionError::Transaction(error.to_string())),
        };
        if control.cancellation.is_cancelled() {
            return Ok(ExecutionReport {
                outcome: InstallOutcome::Cancelled,
                recovery_transaction: None,
            });
        }
        let ledger_store = LinuxInstallationStore::new(&request.state_root);
        match ledger_store.repair_committed(&request.app_id, request.scope) {
            Ok(()) => {}
            Err(ExecError::RecoveryRequired {
                transaction,
                detail,
            }) => {
                return Ok(ExecutionReport {
                    outcome: InstallOutcome::RecoveryRequired {
                        transaction: transaction.map(|id| id.as_uuid()),
                        detail,
                    },
                    recovery_transaction: transaction.map(|id| id.to_string()),
                });
            }
            Err(error) => return Err(SessionError::Transaction(error.to_string())),
        }
        let ledger = ledger_store
            .load(&request.app_id, request.scope)
            .map_err(|error| SessionError::Transaction(error.to_string()))?;
        zup_runtime::resolve_operation(
            self.operation,
            ledger.as_ref().map(|ledger| &ledger.current.version),
            &request.app_version,
        )
        .map_err(|error| SessionError::Transaction(error.to_string()))?;
        ledger_store
            .validate_plan(&request.app_id, request.scope, plan)
            .map_err(|error| SessionError::Transaction(error.to_string()))?;
        let generated = generated_inputs(plan, &self.generated)?;
        crate::integration::reap_generated(
            &request.state_root,
            &request.app_id,
            request.scope,
            &_lock,
        )
        .map_err(|error| SessionError::Transaction(error.to_string()))?;
        crate::integration::stage_generated(
            &request.state_root,
            &request.app_id,
            request.scope,
            &_lock,
            &generated,
        )
        .map_err(|error| SessionError::Transaction(error.to_string()))?;
        let payload = crate::run::RunnerPayload::open(&carrier, &self.installer, generated)
            .map_err(|error| SessionError::Transaction(error.to_string()))?;
        let mut executor = LinuxFileExecutor::new().with_payload(payload);
        executor
            .register_plan(plan)
            .map_err(|error| SessionError::Transaction(error.to_string()))?;
        let store = FilesystemTransactionStore::private_at(&request.state_root);
        let open = TransactionCoordinator::new(store)
            .begin(
                request.app_id.clone(),
                request.scope,
                request.app_version.clone(),
                plan.clone(),
            )
            .map_err(|error| SessionError::Transaction(error.to_string()))?;
        let open = open
            .with_release(deployment.release.clone())
            .map_err(|error| SessionError::Transaction(error.to_string()))?;
        executor.set_state_authority(
            request.state_root.clone(),
            request.app_id.clone(),
            request.scope,
        );
        executor.set_transaction_id(open.record().transaction_id);
        if control.cancellation.is_cancelled() {
            return release_cancelled(open, &mut executor);
        }
        let total_work = plan.total_work();
        let mut executor = EventingExecutor::new(
            executor,
            total_work,
            &control.cancellation,
            |event| {
                let _ = events.send(event);
            },
            || ExecError::Cancelled,
        );
        match open.execute(&mut executor, &ledger_store) {
            Ok(zup_transaction::SettledTransaction {
                outcome: TransactionOutcome::Assessed(assessment),
                ..
            }) => Ok(ExecutionReport {
                outcome: InstallOutcome::RecoveryAssessed { assessment },
                recovery_transaction: None,
            }),
            Ok(zup_transaction::SettledTransaction {
                record,
                outcome: TransactionOutcome::Committed,
            }) => match ledger_store.reconcile_installation(&record, request.scope) {
                Ok(_) => {
                    observe_reap(&request.state_root, &request.app_id, request.scope, &_lock);
                    Ok(ExecutionReport {
                        outcome: InstallOutcome::Committed,
                        recovery_transaction: None,
                    })
                }
                Err(error) => Ok(ExecutionReport {
                    outcome: InstallOutcome::CommittedPendingReconciliation {
                        transaction: record.transaction_id,
                        detail: error.to_string(),
                    },
                    recovery_transaction: Some(record.transaction_id.to_string()),
                }),
            },
            Ok(zup_transaction::SettledTransaction {
                record: _,
                outcome: TransactionOutcome::RolledBack,
            }) => {
                observe_reap(&request.state_root, &request.app_id, request.scope, &_lock);
                if control.cancellation.is_cancelled() {
                    return Ok(ExecutionReport {
                        outcome: InstallOutcome::Cancelled,
                        recovery_transaction: None,
                    });
                }
                Ok(ExecutionReport {
                    outcome: InstallOutcome::RolledBack,
                    recovery_transaction: None,
                })
            }
            Ok(zup_transaction::SettledTransaction {
                record,
                outcome: TransactionOutcome::RecoveryRequired,
            }) => Ok(ExecutionReport {
                outcome: InstallOutcome::recovery(&record),
                recovery_transaction: Some(record.transaction_id.to_string()),
            }),
            Err(error) => Ok(ExecutionReport {
                outcome: InstallOutcome::RecoveryRequired {
                    transaction: None,
                    detail: error.to_string(),
                },
                recovery_transaction: None,
            }),
        }
    }

    fn execute_machine(
        &self,
        request: &RuntimeRequest,
        control: &RuntimeControl,
    ) -> Result<ExecutionReport, SessionError> {
        let RuntimeOperation::Deployment(deployment) = &request.operation else {
            return Err(SessionError::Protocol(
                "deployment runner requires a deployment request".into(),
            ));
        };
        let plan = &deployment.plan;
        if control.cancellation.is_cancelled() {
            return Ok(ExecutionReport {
                outcome: InstallOutcome::Cancelled,
                recovery_transaction: None,
            });
        }
        if request.scope != SelectedScope::Machine {
            return Err(SessionError::Transaction(
                "the machine runner serves machine scope only".into(),
            ));
        }
        crate::machine::resolve_machine_state_root(
            Some(request.state_root.as_path()),
            &crate::locations::MachineRoots::production(),
            &request.app_id,
        )
        .map_err(|error| SessionError::Transaction(error.to_string()))?;
        if rustix::process::geteuid().as_raw() == 0 {
            let outcome = crate::elevate::run_machine_loopback(
                &crate::run::LinuxRunRequest {
                    installer: self.installer.clone(),
                    scope: SelectedScope::Machine,
                    state_root: Some(request.state_root.clone()),
                    action: self.operation,
                    selection: self.selection.clone(),
                },
                &crate::locations::MachineRoots::production(),
                control,
            )
            .map_err(execution_error)?;
            return Ok(outcome.into());
        }

        let launcher = crate::pkexec::SystemPkexec::resolve()
            .map_err(|error| SessionError::Transaction(error.to_string()))?;
        let intent = zup_protocol::PrepareOperation {
            operation: zup_protocol::PrivilegedRequest::Deployment {
                intent: request
                    .operation
                    .deployment()
                    .map(|deployment| Box::new(deployment.intent.clone()))
                    .ok_or_else(|| {
                        SessionError::PlanInvalid(
                            "privileged deployment requires authenticated intent".into(),
                        )
                    })?,
                expected_requirements: Some(Box::new(plan.requirements.clone())),
            },
            app_id: request.app_id.to_string(),
            app_version: request.app_version.to_string(),
            scope: "machine".to_owned(),
            target: request.target,
            state_root: request.state_root.display().to_string(),
        };
        let _ = control
            .events
            .send(zup_runtime::RuntimeEvent::StateChanged {
                state: zup_runtime::RuntimeState::Executing,
            });
        let outcome = match crate::elevate::drive_elevated(
            &self.installer,
            &intent,
            &request.target,
            &launcher,
            Some(&control.events),
            Some(&control.cancellation),
        ) {
            Err(ExecError::StalePlan) => {
                let roots = crate::locations::MachineRoots::production();
                let state_root = crate::machine::resolve_machine_state_root(
                    Some(request.state_root.as_path()),
                    &roots,
                    &request.app_id,
                )
                .map_err(|error| SessionError::Transaction(error.to_string()))?;
                let systemd = crate::machine::SystemdRoots::production();
                let retry = crate::run::LinuxRunRequest {
                    installer: self.installer.clone(),
                    scope: SelectedScope::Machine,
                    state_root: Some(state_root.clone()),
                    action: self.operation,
                    selection: self.selection.clone(),
                };
                let (intent, expected) =
                    crate::elevate::plan_expected(&retry, &state_root, &roots, &systemd, 0)
                        .map_err(|error| SessionError::Transaction(error.to_string()))?;
                match crate::elevate::drive_elevated(
                    &self.installer,
                    &intent,
                    &expected.target,
                    &launcher,
                    Some(&control.events),
                    Some(&control.cancellation),
                ) {
                    Ok(outcome) => outcome,
                    Err(ExecError::Cancelled) => {
                        return Ok(ExecutionReport {
                            outcome: InstallOutcome::Cancelled,
                            recovery_transaction: None,
                        });
                    }
                    Err(error) => return Err(execution_error(error)),
                }
            }
            Err(ExecError::Cancelled) => {
                return Ok(ExecutionReport {
                    outcome: InstallOutcome::Cancelled,
                    recovery_transaction: None,
                });
            }
            outcome => outcome.map_err(execution_error)?,
        };
        Ok(outcome.into())
    }
}

fn validate_bootstrap_binding(
    installer: &std::path::Path,
    request: &RuntimeRequest,
    bootstrap: &zup_runtime::BootstrapRequest,
) -> Result<(), String> {
    let deployment = request
        .operation
        .deployment()
        .ok_or_else(|| "bootstrap requires a deployment operation".to_owned())?;
    let carrier = Carrier::open(installer).map_err(|error| error.to_string())?;
    let app = &carrier.package().plan().installer.app;
    if app.id != request.app_id
        || app.version != request.app_version
        || *carrier.target() != request.target
    {
        return Err("bootstrap identity differs from the authenticated installer".into());
    }
    let build = carrier
        .package()
        .build_plan()
        .map_err(|error| error.to_string())?
        .by_target(&request.target)
        .cloned()
        .ok_or_else(|| "installer does not contain the requested target".to_owned())?;
    build
        .installer
        .install
        .scope
        .select(Some(request.scope))
        .map_err(|error| error.to_string())?;
    if !matches!(
        deployment.intent.action,
        zup_plan::LifecycleAction::Install
            | zup_plan::LifecycleAction::Upgrade
            | zup_plan::LifecycleAction::Downgrade
            | zup_plan::LifecycleAction::Modify
            | zup_plan::LifecycleAction::Repair { .. }
    ) {
        return Err("this operation does not authorize prerequisite bootstrap".into());
    }
    let previous = crate::ledger::LinuxInstallationStore::new(&request.state_root)
        .load(&request.app_id, request.scope)
        .map_err(|error| error.to_string())?;
    let operations = crate::prerequisites::active_for_operation(
        &build.installer,
        deployment.intent.action,
        &deployment.intent.enable,
        &deployment.intent.disable,
        previous
            .as_ref()
            .map(|ledger| ledger.current.selected_components.as_slice()),
    )?
    .into_iter()
    .map(|prerequisite| zup_bootstrap::BootstrapOperation {
        id: prerequisite.id,
        name: prerequisite.name.to_string(),
        target: prerequisite.target,
        requirement: prerequisite.requirement,
        package: prerequisite.package,
        installer: prerequisite.installer,
    })
    .collect::<Vec<_>>();
    let plan = zup_bootstrap::BootstrapPlan::new(
        zup_bootstrap::BootstrapKey {
            app_id: request.app_id.clone(),
            app_version: request.app_version.clone(),
            target: request.target,
            scope: request.scope,
        },
        operations,
    )
    .map_err(|error| error.to_string())?;
    let expected = zup_bootstrap::BoundBootstrapPlan::new(plan, BTreeMap::new())
        .map_err(|error| error.to_string())?;
    if expected.plan.operations.is_empty() {
        return Err("a bootstrap request has no active prerequisites".into());
    }
    for operation in &expected.plan.operations {
        crate::prerequisites::validate(&operation.requirement, &operation.package)
            .map_err(|error| error.to_string())?;
        if operation.installer != zup_core::PrerequisiteInstaller::default() {
            return Err(
                "native package prerequisites do not accept artifact installer options".into(),
            );
        }
    }
    if bootstrap.plan != expected {
        return Err("bootstrap plan differs from authenticated deployment requirements".into());
    }
    let expected_state =
        crate::paths::user_state_root(&request.app_id).map_err(|error| error.to_string())?;
    if bootstrap.state_root != expected_state
        || bootstrap.quarantine_root != expected_state.join("bootstrap")
    {
        return Err("bootstrap storage paths differ from the application state root".into());
    }
    Ok(())
}

fn scope_token(scope: SelectedScope) -> &'static str {
    match scope {
        SelectedScope::User => "user",
        SelectedScope::Machine => "machine",
    }
}

impl RuntimeBackend for LinuxRuntimeBackend {
    fn execute<'a>(
        &'a self,
        request: RuntimeRequest,
        control: RuntimeControl,
    ) -> RuntimeFuture<'a, Result<InstallOutcome, SessionError>> {
        let backend = self.clone();
        Box::pin(async move {
            match tokio::task::spawn_blocking(move || backend.execute_blocking(&request, &control))
                .await
            {
                Ok(result) => result.map(|report| report.outcome),
                Err(error) => Err(SessionError::Transaction(error.to_string())),
            }
        })
    }
}

fn generated_inputs(
    plan: &zup_plan::DeploymentPlan,
    generated: &BTreeMap<String, Vec<u8>>,
) -> Result<BTreeMap<String, Vec<u8>>, SessionError> {
    let mut bound = BTreeMap::new();
    for node in &plan.nodes {
        let Some(source) = node.meta.source_relative.as_ref() else {
            continue;
        };
        if !crate::integration::is_generated_content(source.as_str()) {
            continue;
        }
        let bytes = generated.get(source.as_str()).ok_or_else(|| {
            SessionError::Transaction(format!("generated content missing for {source}"))
        })?;
        let (size, digest) = zup_core::hash_reader(bytes.as_slice())
            .map_err(|error| SessionError::Transaction(error.to_string()))?;
        if Some(digest) != node.meta.expected_sha256 || Some(size) != node.meta.expected_size {
            return Err(SessionError::Transaction(
                "generated content does not match the plan".into(),
            ));
        }
        bound.insert(source.to_string(), bytes.clone());
    }
    Ok(bound)
}

fn release_cancelled(
    open: zup_transaction::OpenTransaction<FilesystemTransactionStore>,
    executor: &mut LinuxFileExecutor,
) -> Result<ExecutionReport, SessionError> {
    match open.release(executor) {
        Ok(zup_transaction::SettledTransaction {
            outcome: TransactionOutcome::Assessed(assessment),
            ..
        }) => Ok(ExecutionReport {
            outcome: InstallOutcome::RecoveryAssessed { assessment },
            recovery_transaction: None,
        }),
        Ok(zup_transaction::SettledTransaction {
            outcome: TransactionOutcome::RolledBack,
            ..
        }) => Ok(ExecutionReport {
            outcome: InstallOutcome::Cancelled,
            recovery_transaction: None,
        }),
        Ok(zup_transaction::SettledTransaction {
            record,
            outcome: TransactionOutcome::RecoveryRequired,
        }) => Ok(ExecutionReport {
            outcome: InstallOutcome::recovery(&record),
            recovery_transaction: Some(record.transaction_id.to_string()),
        }),
        Ok(zup_transaction::SettledTransaction {
            outcome: TransactionOutcome::Committed,
            ..
        }) => Err(SessionError::Transaction(
            "a cancelled transaction cannot commit".into(),
        )),
        Err(error) => Ok(ExecutionReport {
            outcome: InstallOutcome::RecoveryRequired {
                transaction: None,
                detail: error.to_string(),
            },
            recovery_transaction: None,
        }),
    }
}

fn observe_reap(
    state_root: &std::path::Path,
    app_id: &zup_core::AppId,
    scope: SelectedScope,
    lock: &ResourceLock,
) {
    if let Err(error) = crate::integration::reap_generated(state_root, app_id, scope, lock) {
        tracing::warn!(
            %error,
            "generated content cleanup will be retried under the installation lock"
        );
    }
}

pub async fn recover_installation(
    installer: &std::path::Path,
    context: &zup_session::OperationContext,
    transaction: zup_core::TransactionId,
    recovery_request: zup_transaction::RecoveryRequest,
    control: &zup_runtime::OperationControl,
) -> Result<InstallOutcome, SessionError> {
    let id = transaction;
    let installer = installer.to_path_buf();
    let context = context.clone();
    let (backend, request) = tokio::task::spawn_blocking(move || {
        let carrier = Carrier::open(&installer)
            .map_err(|error| SessionError::Transaction(error.to_string()))?;
        let app = &carrier.package().plan().installer.app;
        let (app_id, app_version, target) = if context.scope == SelectedScope::Machine {
            (app.id.clone(), app.version.clone(), *carrier.target())
        } else {
            let record = FilesystemTransactionStore::private_at(&context.state_root)
                .load(&id)
                .map_err(|error| SessionError::Transaction(error.to_string()))?;
            if record.app_id != app.id
                || record.scope != context.scope
                || record.target != *carrier.target()
            {
                return Err(SessionError::Transaction(
                    "recovery identity mismatch".into(),
                ));
            }
            (record.app_id, record.app_version, record.target)
        };
        let request = RuntimeRequest {
            operation: zup_runtime::RuntimeOperation::Recovery {
                transaction: id,
                request: recovery_request,
            },
            target,
            app_id,
            app_version,
            scope: context.scope,
            state_root: context.state_root.clone(),
        };
        let backend = LinuxRuntimeBackend {
            installer,
            operation: Operation::Apply,
            selection: Default::default(),
            generated: Default::default(),
        };
        Ok::<_, SessionError>((backend, request))
    })
    .await
    .map_err(|error| SessionError::Transaction(error.to_string()))??;
    zup_runtime::execute(&backend, request, control).await
}

fn execution_error(error: ExecError) -> SessionError {
    match error {
        ExecError::Cancelled => SessionError::Cancelled,
        ExecError::RecoveryRequired {
            transaction,
            detail,
        } => SessionError::RecoveryRequired {
            transaction: transaction.map(|id| id.as_uuid()),
            detail,
        },
        ExecError::Elevation(crate::error::IpcError::PkexecCancelled) => {
            SessionError::AuthorizationCancelled
        }
        ExecError::Elevation(crate::error::IpcError::AuthorizationFailed(_)) => {
            SessionError::AuthorizationRequired
        }
        other => SessionError::Transaction(other.to_string()),
    }
}
