use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use zup_core::SelectedScope;
use zup_runtime::{
    EventingExecutor, InstallOutcome, Operation, OperationSelection, RuntimeBackend,
    RuntimeControl, RuntimeEvent, RuntimeFuture, RuntimePayloadSource, RuntimeRequest,
    SessionError,
};
use zup_transaction::{
    FilesystemTransactionStore, InstallationLock, TransactionCoordinator, TransactionOutcome,
    TransactionStore,
};

use crate::carrier::Carrier;
use crate::error::ExecError;
use crate::executor::LinuxFileExecutor;
use crate::ledger::LinuxLedgerStore;
use crate::lifecycle::LinuxOutcome;

#[derive(Clone)]
pub struct LinuxRuntimeBackend {
    pub installer: PathBuf,
    pub operation: Operation,
    pub selection: OperationSelection,
    pub install_dir_override: Option<PathBuf>,
    pub generated: BTreeMap<String, Vec<u8>>,
}

pub struct ExecutionReport {
    pub outcome: InstallOutcome,
    pub recovery_transaction: Option<String>,
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
        if let Some(id) = request.recovery_id {
            return self.execute_recovery(request, control, &id);
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
        id: &zup_transaction::TransactionId,
    ) -> Result<ExecutionReport, SessionError> {
        let events = &control.events;
        crate::run::refuse_redirected_hierarchy(&request.state_root)
            .map_err(|error| SessionError::Transaction(error.to_string()))?;
        let store = FilesystemTransactionStore::new(&request.state_root);
        let record = store
            .load(id)
            .map_err(|error| SessionError::Transaction(error.to_string()))?;
        if record.app_id != request.app_id
            || record.scope != request.scope
            || record.app_version != request.app_version
            || record.target != request.target
            || record.plan != request.transaction_plan
        {
            return Err(SessionError::Transaction(
                "recovery identity mismatch".into(),
            ));
        }
        let maintenance = zup_transaction::maintenance_runtime_path(
            &request.state_root,
            &request.app_id,
            request.scope,
            &record.app_version,
            record.target.executable_suffix(),
        );
        let carrier = Carrier::open(&maintenance)
            .map_err(|error| SessionError::Transaction(error.to_string()))?;
        let generated =
            crate::integration::load_generated(&request.state_root, &request.app_id, request.scope)
                .map_err(|error| SessionError::Transaction(error.to_string()))?;
        let payload = crate::run::RunnerPayload::open(&carrier, carrier.executable(), generated)
            .map_err(|error| SessionError::Transaction(error.to_string()))?;
        let lock_key =
            InstallationLock::lock_key(request.app_id.as_str(), scope_token(request.scope));
        let _lock = match InstallationLock::try_acquire(&request.state_root, &lock_key) {
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
        let mut executor = LinuxFileExecutor::new().with_payload(payload);
        executor
            .register_plan(&record.plan)
            .map_err(|error| SessionError::Transaction(error.to_string()))?;
        executor.set_transaction_id(record.transaction_id);
        crate::run::preflight_refresh(&record.plan)
            .map_err(|error| SessionError::Transaction(error.to_string()))?;
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
        let ledger_store = LinuxLedgerStore::new(&request.state_root);
        match zup_transaction::recover(record, &store, &mut executor) {
            Ok((record, TransactionOutcome::Committed)) => {
                match ledger_store.publish_committed(&record, request.scope) {
                    Ok(_) => Ok(ExecutionReport {
                        outcome: InstallOutcome::Committed,
                        recovery_transaction: None,
                    }),
                    Err(error) => Ok(ExecutionReport {
                        outcome: InstallOutcome::CommittedPendingReconciliation {
                            detail: error.to_string(),
                        },
                        recovery_transaction: Some(record.transaction_id.to_string()),
                    }),
                }
            }
            Ok((record, TransactionOutcome::RolledBack)) => {
                let _ = crate::run::sweep_refresh(&record);
                Ok(ExecutionReport {
                    outcome: InstallOutcome::RolledBack,
                    recovery_transaction: None,
                })
            }
            Ok((record, TransactionOutcome::RecoveryRequired)) => Ok(ExecutionReport {
                outcome: InstallOutcome::RecoveryRequired,
                recovery_transaction: Some(record.transaction_id.to_string()),
            }),
            Err(error) => Err(SessionError::Transaction(error.to_string())),
        }
    }

    fn execute_user(
        &self,
        request: &RuntimeRequest,
        control: &RuntimeControl,
    ) -> Result<ExecutionReport, SessionError> {
        let events = &control.events;
        let _ = events.send(RuntimeEvent::StateChanged {
            state: zup_runtime::RuntimeState::Executing,
        });
        crate::run::refuse_redirected_hierarchy(&request.state_root)
            .map_err(|error| SessionError::Transaction(error.to_string()))?;
        let carrier = Carrier::open(&self.installer)
            .map_err(|error| SessionError::Transaction(error.to_string()))?;
        let lock_key =
            InstallationLock::lock_key(request.app_id.as_str(), scope_token(request.scope));
        let _lock = match InstallationLock::try_acquire(&request.state_root, &lock_key) {
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
        let ledger_store = LinuxLedgerStore::new(&request.state_root);
        match ledger_store.repair_committed(&request.app_id, request.scope) {
            Ok(()) => {}
            Err(ExecError::RecoveryRequired(transaction)) => {
                return Ok(ExecutionReport {
                    outcome: InstallOutcome::RecoveryRequired,
                    recovery_transaction: Some(transaction),
                });
            }
            Err(error) => return Err(SessionError::Transaction(error.to_string())),
        }
        let ledger = ledger_store
            .load(&request.app_id, request.scope)
            .map_err(|error| SessionError::Transaction(error.to_string()))?;
        let action = zup_runtime::resolve_operation(
            self.operation,
            ledger.as_ref().map(|ledger| &ledger.version),
            &request.app_version,
        )
        .map_err(|error| SessionError::Transaction(error.to_string()))?;
        ledger_store
            .validate_plan(
                &request.app_id,
                request.scope,
                &request.app_version,
                &request.transaction_plan,
            )
            .map_err(|error| SessionError::Transaction(error.to_string()))?;
        crate::run::preflight_refresh(&request.transaction_plan)
            .map_err(|error| SessionError::Transaction(error.to_string()))?;
        let payload =
            crate::run::RunnerPayload::open(&carrier, &self.installer, self.generated.clone())
                .map_err(|error| SessionError::Transaction(error.to_string()))?;
        let mut executor = LinuxFileExecutor::new().with_payload(payload);
        executor
            .register_plan(&request.transaction_plan)
            .map_err(|error| SessionError::Transaction(error.to_string()))?;
        let store = FilesystemTransactionStore::new(&request.state_root);
        let open = TransactionCoordinator::new(store)
            .begin(
                request.app_id.clone(),
                request.scope,
                request.app_version.clone(),
                request.transaction_plan.clone(),
            )
            .map_err(|error| SessionError::Transaction(error.to_string()))?;
        executor.set_transaction_id(open.record().transaction_id);
        if control.cancellation.is_cancelled() {
            return release_cancelled(open, &mut executor);
        }
        let total_work = request.transaction_plan.total_work();
        let mut executor = EventingExecutor::new(
            executor,
            total_work,
            &control.cancellation,
            |event| {
                let _ = events.send(event);
            },
            || ExecError::Cancelled,
        );
        match open.execute(&mut executor) {
            Ok(zup_transaction::SettledTransaction {
                record,
                outcome: TransactionOutcome::Committed,
            }) => match ledger_store.publish_committed(&record, request.scope) {
                Ok(ledger) => {
                    if action == zup_exec::LifecycleAction::Uninstall {
                        let _ = InstallationLock::remove_if_unheld(&request.state_root, &lock_key);
                    } else {
                        if action == zup_exec::LifecycleAction::Upgrade
                            && let Err(error) = crate::run::retire_old_generations_for(
                                &request.state_root,
                                &request.app_id,
                                request.scope,
                                &ledger.version,
                            )
                        {
                            return Ok(ExecutionReport {
                                outcome: InstallOutcome::CommittedPendingReconciliation {
                                    detail: error.to_string(),
                                },
                                recovery_transaction: Some(record.transaction_id.to_string()),
                            });
                        }
                        let _ = ledger;
                    }
                    Ok(ExecutionReport {
                        outcome: InstallOutcome::Committed,
                        recovery_transaction: None,
                    })
                }
                Err(error) => Ok(ExecutionReport {
                    outcome: InstallOutcome::CommittedPendingReconciliation {
                        detail: error.to_string(),
                    },
                    recovery_transaction: Some(record.transaction_id.to_string()),
                }),
            },
            Ok(zup_transaction::SettledTransaction {
                record,
                outcome: TransactionOutcome::RolledBack,
            }) => {
                let _ = crate::run::sweep_refresh(&record);
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
                outcome: InstallOutcome::RecoveryRequired,
                recovery_transaction: Some(record.transaction_id.to_string()),
            }),
            Err(_) => Ok(ExecutionReport {
                outcome: InstallOutcome::RecoveryRequired,
                recovery_transaction: None,
            }),
        }
    }

    fn execute_machine(
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
        if request.scope != SelectedScope::Machine {
            return Err(SessionError::Transaction(
                "the machine runner serves machine scope only".into(),
            ));
        }
        crate::machine::resolve_machine_state_root(
            Some(request.state_root.as_path()),
            &crate::locations::MachineRoots::production(),
        )
        .map_err(|error| SessionError::Transaction(error.to_string()))?;
        if rustix::process::geteuid().as_raw() == 0 {
            let outcome = crate::elevate::run_machine_loopback(
                &crate::run::LinuxRunRequest {
                    installer: self.installer.clone(),
                    scope: SelectedScope::Machine,
                    state_root: Some(request.state_root.clone()),
                    action: self.operation,
                    install_dir_override: self.selection.install_directory.clone(),
                    selection: self.selection.clone(),
                },
                &crate::locations::MachineRoots::production(),
            )
            .map_err(|error| SessionError::Transaction(error.to_string()))?;
            return Ok(match outcome {
                LinuxOutcome::Committed { .. } => ExecutionReport {
                    outcome: InstallOutcome::Committed,
                    recovery_transaction: None,
                },
                LinuxOutcome::RolledBack => ExecutionReport {
                    outcome: InstallOutcome::RolledBack,
                    recovery_transaction: None,
                },
                LinuxOutcome::CommittedPendingReconciliation { transaction } => ExecutionReport {
                    outcome: InstallOutcome::CommittedPendingReconciliation {
                        detail:
                            "the transaction committed and ledger reconciliation is still required"
                                .into(),
                    },
                    recovery_transaction: Some(transaction),
                },
                LinuxOutcome::RecoveryRequired { transaction } => ExecutionReport {
                    outcome: InstallOutcome::RecoveryRequired,
                    recovery_transaction: Some(transaction),
                },
                LinuxOutcome::Busy => ExecutionReport {
                    outcome: InstallOutcome::Busy {
                        operation: "linux install",
                    },
                    recovery_transaction: None,
                },
            });
        }
        let launcher = crate::pkexec::SystemPkexec::resolve()
            .map_err(|error| SessionError::Transaction(error.to_string()))?;
        let intent = zup_protocol::PrepareOperation {
            operation: self.operation.name().to_owned(),
            force_files: matches!(
                self.operation,
                Operation::Repair { force_files } if force_files
            ),
            install_dir_override: self
                .selection
                .install_directory
                .as_ref()
                .map(|directory| directory.display().to_string()),
            selected_components: self.selection.enable.clone(),
            disabled_components: self.selection.disable.clone(),
            expected_plan_digest: Some(request.transaction_plan.fingerprint().to_hex()),
            app_id: request.app_id.to_string(),
            app_version: request.app_version.to_string(),
            scope: "machine".to_owned(),
            target: request.target,
            state_root: request.state_root.display().to_string(),
        };
        let digest = request.transaction_plan.fingerprint().to_hex();
        let _ = control
            .events
            .send(zup_runtime::RuntimeEvent::StateChanged {
                state: zup_runtime::RuntimeState::Executing,
            });
        let outcome = match crate::elevate::drive_elevated(
            &intent,
            &digest,
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
                )
                .map_err(|error| SessionError::Transaction(error.to_string()))?;
                let systemd = crate::machine::SystemdRoots::production();
                let retry = crate::run::LinuxRunRequest {
                    installer: self.installer.clone(),
                    scope: SelectedScope::Machine,
                    state_root: Some(state_root.clone()),
                    action: self.operation,
                    install_dir_override: self.selection.install_directory.clone(),
                    selection: self.selection.clone(),
                };
                let (intent, expected) =
                    crate::elevate::plan_expected(&retry, &state_root, &roots, &systemd, 0)
                        .map_err(|error| SessionError::Transaction(error.to_string()))?;
                match crate::elevate::drive_elevated(
                    &intent,
                    &expected.digest,
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
                    Err(ExecError::RecoveryRequired(_)) => {
                        return Err(SessionError::RecoveryRequired);
                    }
                    Err(error) => return Err(SessionError::Transaction(error.to_string())),
                }
            }
            Err(ExecError::Cancelled) => {
                return Ok(ExecutionReport {
                    outcome: InstallOutcome::Cancelled,
                    recovery_transaction: None,
                });
            }
            outcome => outcome.map_err(|error| match error {
                ExecError::RecoveryRequired(_) => SessionError::RecoveryRequired,
                ExecError::Cancelled => SessionError::Transaction("cancelled".into()),
                _ => SessionError::Transaction(error.to_string()),
            })?,
        };
        match outcome {
            LinuxOutcome::Committed { .. } => Ok(ExecutionReport {
                outcome: InstallOutcome::Committed,
                recovery_transaction: None,
            }),
            LinuxOutcome::RolledBack => Ok(ExecutionReport {
                outcome: InstallOutcome::RolledBack,
                recovery_transaction: None,
            }),
            LinuxOutcome::RecoveryRequired { transaction } => Ok(ExecutionReport {
                outcome: InstallOutcome::RecoveryRequired,
                recovery_transaction: Some(transaction),
            }),
            LinuxOutcome::CommittedPendingReconciliation { transaction } => Ok(ExecutionReport {
                outcome: InstallOutcome::CommittedPendingReconciliation {
                    detail: "the transaction committed and ledger reconciliation is still required"
                        .into(),
                },
                recovery_transaction: Some(transaction),
            }),
            LinuxOutcome::Busy => Ok(ExecutionReport {
                outcome: InstallOutcome::Busy {
                    operation: "linux install",
                },
                recovery_transaction: None,
            }),
        }
    }
}

fn scope_token(scope: SelectedScope) -> &'static str {
    match scope {
        SelectedScope::User => "user",
        SelectedScope::Machine => "machine",
    }
}

struct UnavailablePayload;

impl zup_bundle::PayloadSource for UnavailablePayload {
    fn open(
        &self,
        path: &zup_core::RelativePath,
        _expected_sha256: &zup_core::Sha256Digest,
        _expected_size: u64,
    ) -> Result<zup_bundle::PayloadReader, zup_bundle::PayloadError> {
        Err(zup_bundle::PayloadError::NotFound {
            path: path.to_string(),
        })
    }
}

impl RuntimeBackend for LinuxRuntimeBackend {
    fn payload_source(&self, _request: &RuntimeRequest) -> RuntimePayloadSource {
        match Carrier::open(&self.installer) {
            Ok(carrier) => Arc::new(carrier.package().payload_source()),
            Err(_) => Arc::new(UnavailablePayload),
        }
    }

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

fn release_cancelled(
    open: zup_transaction::OpenTransaction<FilesystemTransactionStore>,
    executor: &mut LinuxFileExecutor,
) -> Result<ExecutionReport, SessionError> {
    match open.release(executor) {
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
            outcome: InstallOutcome::RecoveryRequired,
            recovery_transaction: Some(record.transaction_id.to_string()),
        }),
        Ok(zup_transaction::SettledTransaction {
            outcome: TransactionOutcome::Committed,
            ..
        }) => Err(SessionError::Transaction(
            "a cancelled transaction cannot commit".into(),
        )),
        Err(_) => Ok(ExecutionReport {
            outcome: InstallOutcome::RecoveryRequired,
            recovery_transaction: None,
        }),
    }
}
