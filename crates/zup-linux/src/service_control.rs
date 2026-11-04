use zup_transaction::{
    NativeServiceManager, ServiceControlOperation, ServiceObservation, ServiceTerminalState,
};

pub fn validate_binding(
    plan: &zup_transaction::TransactionPlan,
    operation: &ServiceControlOperation,
    ledger: Option<&zup_plan::InstallLedger>,
) -> Result<(), String> {
    let zup_core::ResourceKey::Service { id } = &operation.key else {
        return Err("service resource required".into());
    };
    if operation.privilege != zup_core::Privilege::System
        || crate::services::unit_name(id).map_err(|e| e.to_string())? != operation.native_name
    {
        return Err("service authority mismatch".into());
    }
    if let Some(zup_plan::OwnedResource::Service { installed, .. }) =
        ledger.and_then(|ledger| ledger.resources.get(&operation.key))
        && installed == &operation.configuration
        && installed == &operation.owned_configuration
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
        let installed = zup_plan::ServiceState::Registration {
            display_name: service.display_name.clone(),
            command: service.command.clone(),
            start: service.start,
            readiness: service.readiness,
        };
        if service.key == operation.key
            && installed == operation.configuration
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
            ))
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
            display_name,
            command,
            start,
            readiness,
        } = &operation.configuration
        else {
            unreachable!()
        };
        let service = zup_plan::DesiredService {
            key: operation.key.clone(),
            id: id.clone(),
            name: zup_core::NonEmptyString::new(&operation.native_name)
                .map_err(|e| e.to_string())?,
            display_name: None,
            command: command.clone(),
            start: *start,
            privilege: operation.privilege,
            lifecycle: Default::default(),
            readiness: *readiness,
        };
        let expected =
            crate::services::render_unit(&service, display_name).map_err(|e| e.to_string())?;
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
        if info.load_state != "loaded"
            || std::path::Path::new(&info.fragment_path) != canonical
            || !info.drop_in_paths.is_empty()
            || info.need_daemon_reload
        {
            return Err("service fragment changed".into());
        }
        let runtime = self.manager.runtime(&unit).map_err(|e| e.to_string())?;
        let expected_type = match readiness {
            zup_core::ServiceReadiness::Manager => "exec",
            zup_core::ServiceReadiness::Notify => "notify",
        };
        if runtime.service_type != expected_type {
            return Err(
                "native service readiness type differs from the authorized configuration".into(),
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
    ) -> Result<(), String> {
        let current = self.observe(operation)?;
        if current.configuration != operation.configuration
            || current.manager_generation != operation.manager_generation
        {
            return Err("service changed before native control".into());
        }
        if state == ServiceTerminalState::Running
            && let zup_plan::ServiceState::Registration { command, .. } = &operation.configuration
        {
            crate::service_ops::validate_executable_live(command, self.roots, self.expected_uid)
                .map_err(|e| e.to_string())?;
        }
        self.manager
            .control(&operation.native_name, state, operation.timeout_for(state))
            .map_err(|e| e.to_string())
    }
}
