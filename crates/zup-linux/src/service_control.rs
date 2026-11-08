use zup_transaction::{
    NativeServiceManager, ServiceControlOperation, ServiceObservation, ServiceTerminalState,
};

pub fn validate_binding(
    plan: &zup_transaction::TransactionPlan,
    operation: &ServiceControlOperation,
    ledger: Option<&zup_plan::InstallLedger>,
) -> Result<(), String> {
    let mut protected_files = match ledger.and_then(|ledger| ledger.resources.get(&operation.key)) {
        Some(zup_plan::OwnedResource::Service { requires_files, .. }) => requires_files.clone(),
        _ => Vec::new(),
    };
    for node in &plan.nodes {
        if let Some(backend) = &node.meta.backend
            && let Ok(crate::service_ops::ServicePayload::Apply { service, .. }) =
                crate::service_ops::decode_payload(&backend.payload)
            && service.key == operation.key
        {
            protected_files.extend(service.requires_files);
        }
    }
    zup_transaction::validate_runtime_preservation(plan, operation, &protected_files)?;
    let zup_core::ResourceKey::Service { id } = &operation.key else {
        return Err("service resource required".into());
    };
    if operation.privilege != zup_core::Privilege::System
        || (if matches!(
            operation.configuration,
            zup_plan::ServiceState::Registration {
                execution: zup_core::ServiceExecution::OpenrcUser { .. },
                ..
            }
        ) {
            crate::openrc::service_name(id)
        } else {
            crate::services::unit_name(id).map_err(|e| e.to_string())?
        }) != operation.native_name
    {
        return Err("service authority mismatch".into());
    }
    if let Some(zup_plan::OwnedResource::Service {
        installed,
        timeouts,
        ..
    }) = ledger.and_then(|ledger| ledger.resources.get(&operation.key))
        && installed.as_ref() == &operation.owned_configuration
        && *timeouts == operation.timeouts
        && (installed.as_ref() == &operation.configuration
            || operation.stage == zup_transaction::ServiceControlStage::Quiesce
                && operation.before_configuration == **installed
                && zup_transaction::disabled_startup(installed).as_ref()
                    == Some(&operation.configuration))
    {
        return Ok(());
    }
    for node in &plan.nodes {
        let Some(backend) = &node.meta.backend else {
            continue;
        };
        let Ok(crate::service_ops::ServicePayload::Apply { service, .. }) =
            crate::service_ops::decode_payload(&backend.payload)
        else {
            continue;
        };
        let installed = service.desired_configuration();
        let repairing_live_configuration = operation.intent
            == zup_transaction::ServiceControlIntent::Preserve
            && operation.stage == zup_transaction::ServiceControlStage::Quiesce
            && service.kind == zup_plan::IntegrationChangeKind::RepairOwned
            && service.previous.configuration().as_ref() == Some(&operation.configuration)
            && installed == operation.owned_configuration;
        if service.key == operation.key
            && operation.timeouts
                == if operation.stage == zup_transaction::ServiceControlStage::Quiesce {
                    match ledger.and_then(|ledger| ledger.resources.get(&service.key)) {
                        Some(zup_plan::OwnedResource::Service { timeouts, .. }) => *timeouts,
                        _ => service.lifecycle.timeouts_for(&service.execution),
                    }
                } else {
                    service.lifecycle.timeouts_for(&service.execution)
                }
            && (installed == operation.configuration || repairing_live_configuration)
            && (operation.owned_configuration != zup_plan::ServiceState::Absent
                || service.previous == zup_plan::ObservedServiceState::Absent
                    && service.kind == zup_plan::IntegrationChangeKind::Create)
        {
            return Ok(());
        }
    }
    Err("service control is not bound to an owned registration transition".into())
}

impl NativeServiceManager for crate::service_exec::ServiceContext<'_> {
    fn finish_startup_inhibition(
        &mut self,
        operation: &ServiceControlOperation,
    ) -> Result<(), String> {
        if operation.before_configuration == operation.configuration {
            return Ok(());
        }
        if self.stopped_runtime_is_quiescent(operation, &operation.configuration)? {
            Ok(())
        } else {
            Err("systemd activation inhibition did not establish quiescence".into())
        }
    }
    fn stopped_runtime_is_quiescent(
        &mut self,
        operation: &ServiceControlOperation,
        configuration: &zup_plan::ServiceState,
    ) -> Result<bool, String> {
        let current = self.observe(operation)?;
        let runtime = self
            .manager
            .runtime(&operation.native_name)
            .map_err(|error| error.to_string())?;
        Ok(&current.configuration == configuration
            && current.manager_generation == operation.manager_generation
            && runtime.manager_generation == operation.manager_generation
            && current.runtime == zup_plan::ServiceRuntimeState::Stopped
            && runtime.state == zup_plan::ServiceRuntimeState::Stopped
            && !runtime.pending_job
            && self
                .manager
                .process_scope_is_empty(&operation.native_name)
                .map_err(|error| error.to_string())?
            && self
                .manager
                .generation()
                .map_err(|error| error.to_string())?
                == operation.manager_generation)
    }
    fn stopped_start_can_be_compensated(
        &mut self,
        operation: &ServiceControlOperation,
    ) -> Result<bool, String> {
        let current = self.observe(operation)?;
        let runtime = self
            .manager
            .runtime(&operation.native_name)
            .map_err(|e| e.to_string())?;
        Ok(current.configuration == operation.configuration
            && current.manager_generation == operation.manager_generation
            && runtime.manager_generation == operation.manager_generation
            && current.runtime == zup_plan::ServiceRuntimeState::Stopped
            && runtime.state == zup_plan::ServiceRuntimeState::Stopped
            && !runtime.pending_job
            && self
                .manager
                .process_scope_is_empty(&operation.native_name)
                .map_err(|error| error.to_string())?
            && matches!(
                runtime.result.as_str(),
                "exit-code"
                    | "signal"
                    | "core-dump"
                    | "timeout"
                    | "watchdog"
                    | "protocol"
                    | "resources"
                    | "start-limit-hit"
                    | "oom-kill"
            )
            && self
                .manager
                .generation()
                .map_err(|error| error.to_string())?
                == operation.manager_generation)
    }
    fn observe(
        &mut self,
        operation: &ServiceControlOperation,
    ) -> Result<ServiceObservation, String> {
        operation.validate()?;
        let zup_core::ResourceKey::Service { id } = &operation.key else {
            return Err("service resource required".into());
        };
        let unit = crate::services::unit_name(id).map_err(|e| e.to_string())?;
        if unit != operation.native_name {
            return Err("service native identity mismatch".into());
        }
        let canonical = crate::machine::authorize_systemd_unit(&unit, self.systemd)
            .map_err(|e| e.to_string())?;
        crate::service_exec::refuse_foreign_integration(&unit, &canonical)
            .map_err(|e| e.to_string())?;
        let target = match &operation.configuration {
            zup_plan::ServiceState::Registration { command, .. } => *command.executable.target(),
            _ => return Err("service registration required".into()),
        };
        let zup_plan::ServiceState::Registration {
            native_timeouts,
            failure,
            execution,
            readiness,
            ..
        } = &operation.configuration
        else {
            unreachable!()
        };
        let expected = crate::services::render_registration(&unit, &operation.configuration)?;
        if operation.intent == zup_transaction::ServiceControlIntent::Transition
            && operation.timeouts.native() != *native_timeouts
        {
            return Err("service timing policy differs from its registration".into());
        }
        if crate::service_ops::read_canonical_source(&canonical)
            .map_err(|e| e.to_string())?
            .as_deref()
            != Some(expected.as_slice())
        {
            return Err("service definition changed before runtime control".into());
        }
        let configuration =
            crate::service_ops::observe_unit(&unit, &canonical, target, self.manager)
                .map_err(|e| e.to_string())?
                .configuration()
                .ok_or("unrecognized service registration")?;
        let info = self.manager.load_unit(&unit).map_err(|e| e.to_string())?;
        let disabled = matches!(
            configuration,
            zup_plan::ServiceState::Registration {
                start: zup_core::ServiceStart::Disabled,
                ..
            }
        );
        if (!disabled
            && (info.load_state != "loaded"
                || std::path::Path::new(&info.fragment_path) != canonical))
            || crate::systemd_policy::validate(&info.drop_in_paths).is_err()
            || info.need_daemon_reload
        {
            return Err("service fragment changed".into());
        }
        let runtime = self.manager.runtime(&unit).map_err(|e| e.to_string())?;
        if runtime.pending_job
            && !matches!(
                runtime.state,
                zup_plan::ServiceRuntimeState::StartPending
                    | zup_plan::ServiceRuntimeState::StopPending
            )
        {
            return Err("service has a concurrent native manager job".into());
        }
        let expected_type = match readiness {
            zup_core::ServiceReadiness::Manager => "exec",
            zup_core::ServiceReadiness::Notify => "notify",
        };
        if !disabled
            && (runtime.service_type != expected_type
                || runtime.execution.as_ref() != Some(execution)
                || runtime.failure != *failure
                || runtime.native_timeouts != Some(*native_timeouts))
        {
            return Err(
                "native service readiness or execution identity differs from the authorized configuration".into(),
            );
        }
        Ok(ServiceObservation {
            configuration,
            runtime: runtime.state,
            manager_generation: runtime.manager_generation,
            execution_generation: runtime.execution_generation,
        })
    }

    fn transition(
        &mut self,
        operation: &ServiceControlOperation,
        state: ServiceTerminalState,
        expected: &ServiceObservation,
        deadline: std::time::Instant,
    ) -> Result<ServiceObservation, String> {
        let current = self.observe(operation)?;
        if &current != expected
            || current.configuration != operation.before_configuration
            || current.manager_generation != operation.manager_generation
        {
            return Err("service changed before native control".into());
        }
        if state == ServiceTerminalState::Stopped {
            if current.runtime == zup_plan::ServiceRuntimeState::Running
                || operation.stage == zup_transaction::ServiceControlStage::Quiesce
                    && zup_transaction::requires_startup_inhibition(&operation.before_configuration)
            {
                self.manager
                    .retain_unit(&operation.native_name)
                    .map_err(|error| error.to_string())?;
            }
            if !state.matches(current.runtime) {
                // Reloading a mask discards native stop settings, so stop under the installed policy first.
                self.manager
                    .control(
                        &operation.native_name,
                        state,
                        deadline
                            .checked_duration_since(std::time::Instant::now())
                            .filter(|remaining| !remaining.is_zero())
                            .ok_or("service lifecycle deadline expired before native control")?,
                        self.controlled_units,
                    )
                    .map_err(|error| error.to_string())?;
            }
        }
        if operation.before_configuration != operation.configuration {
            let zup_plan::ServiceState::Registration { start, .. } = operation.configuration else {
                return Err("service registration required".into());
            };
            if std::time::Instant::now() >= deadline {
                return Err("service lifecycle deadline expired before activation change".into());
            }
            let canonical =
                crate::machine::authorize_systemd_unit(&operation.native_name, self.systemd)
                    .map_err(|error| error.to_string())?;
            let mut changes = Vec::new();
            crate::service_exec::apply_policy(
                &operation.native_name,
                &canonical,
                start,
                self.manager,
                &mut changes,
            )
            .map_err(|error| error.to_string())?;
            for change in changes {
                crate::service_ops::validate_changes(&operation.native_name, &[change])
                    .map_err(|error| error.to_string())?;
            }
            self.manager.reload().map_err(|error| error.to_string())?;
            zup_transaction::fault_injection::boundary(
                zup_transaction::fault_injection::Boundary::NativeConfigurationChanged,
                &operation.native_name,
            );
        }
        if state == ServiceTerminalState::Stopped || state.matches(current.runtime) {
            return self.observe(operation);
        }
        if state == ServiceTerminalState::Running
            && let zup_plan::ServiceState::Registration { command, .. } = &operation.configuration
        {
            crate::service_ops::validate_executable_live(command, self.roots, self.expected_uid)
                .map_err(|e| e.to_string())?;
        }
        self.manager
            .control(
                &operation.native_name,
                state,
                deadline
                    .checked_duration_since(std::time::Instant::now())
                    .filter(|remaining| !remaining.is_zero())
                    .ok_or("service lifecycle deadline expired before native control")?,
                self.controlled_units,
            )
            .map_err(|e| e.to_string())?;
        self.observe(operation)
    }
}
