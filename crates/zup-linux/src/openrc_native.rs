use crate::{
    error::ExecError,
    fs::OwnedDirectory,
    service_ops::{ServicePayload, ServiceReceipt},
};
use std::{
    collections::BTreeMap,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use zup_core::{Privilege, ServiceStart};
use zup_plan::{ObservedServiceState, OwnedResource, ServiceRuntimeState, ServiceState};
use zup_transaction::{
    NativeServiceManager, ServiceControlOperation, ServiceObservation, ServiceTerminalState,
};

fn refused(name: &str, reason: impl Into<String>) -> ExecError {
    ExecError::refused(name, reason)
}

pub fn active() -> bool {
    std::fs::read_to_string("/proc/1/comm")
        .is_ok_and(|name| matches!(name.trim(), "init" | "openrc-init"))
        && !Path::new("/run/systemd/system").exists()
        && std::fs::read_to_string("/run/openrc/softlevel")
            .is_ok_and(|level| !level.trim().is_empty())
        && Path::new("/sbin/openrc-run").exists()
}

pub struct Openrc {
    pub roots: crate::locations::MachineRoots,
}

impl Openrc {
    pub fn snapshot(
        &self,
        target: &zup_plan::DesiredState<zup_plan::TargetPath, zup_plan::TargetPath>,
    ) -> Result<Vec<zup_plan::ObservedService>, String> {
        target
            .services
            .iter()
            .map(|service| {
                let name = crate::openrc::service_name(&service.id);
                if service.name.as_str() != name {
                    return Err("resolved service manager differs from active OpenRC".into());
                }
                Ok(zup_plan::ObservedService {
                    key: service.key.clone(),
                    id: service.id.clone(),
                    state: self.observe_registration(&name)?,
                })
            })
            .collect()
    }

    pub fn compile(
        &mut self,
        execution: &zup_plan::ExecutionPlan,
        target: Option<&zup_plan::DesiredState<zup_plan::TargetPath, zup_plan::TargetPath>>,
        ledger: Option<&zup_plan::InstallLedger>,
    ) -> Result<zup_transaction::TransactionInput, String> {
        if execution
            .validate_desired(target)
            .map_err(|error| error.to_string())?
            .scope
            != zup_core::SelectedScope::Machine
        {
            return Err("OpenRC requires machine authority".into());
        }
        let mut files = execution.clone();
        files.services.clear();
        files
            .removals
            .retain(|removal| !matches!(removal.owned, OwnedResource::Service { .. }));
        let mut input =
            crate::input::compile_files_only(&files, target).map_err(|error| error.to_string())?;
        let target_files: BTreeMap<_, _> = target
            .into_iter()
            .flat_map(|target| &target.files)
            .map(|file| (file.destination.to_string(), file.executable))
            .collect();
        let affected = execution.affected_services(ledger);
        for service in &execution.services {
            let name = crate::openrc::resource_name(&service.key)?;
            if service.name != name || service.privilege != Privilege::System {
                return Err("OpenRC service identity or scope mismatch".into());
            }
            crate::service_ops::validate_executable(
                &service.command,
                &target_files,
                &self.roots,
                0,
                false,
            )
            .map_err(|error| error.to_string())?;
            let owned = ledger.and_then(|ledger| ledger.resources.get(&service.key));
            if affected.contains(&service.key)
                && (owned.is_some()
                    || service.previous == ObservedServiceState::Absent
                        && service.lifecycle.install == zup_core::InitialServiceState::Running)
            {
                let generation = match service.previous.configuration() {
                    Some(ServiceState::Registration {
                        ref command,
                        failure,
                        ..
                    }) => {
                        self.runtime(
                            &name,
                            command,
                            failure != zup_core::ServiceFailurePolicy::None,
                        )?
                        .1
                    }
                    Some(ServiceState::Absent) => "stopped".into(),
                    None => return Err("unrecognized OpenRC service cannot be controlled".into()),
                };
                input
                    .service_controls
                    .extend(zup_transaction::plan_service_controls(
                        service,
                        owned,
                        true,
                        name.clone(),
                        self.generation()?,
                        generation,
                    )?);
            }
            if service.kind == zup_plan::IntegrationChangeKind::NoOp {
                continue;
            }
            if matches!(
                service.kind,
                zup_plan::IntegrationChangeKind::Conflict | zup_plan::IntegrationChangeKind::Drift
            ) {
                return Err("OpenRC service has an ownership conflict or unapproved drift".into());
            }
            let state = ServiceState::Registration {
                execution: service.execution.clone(),
                failure: service.failure,
                dependencies: service.dependency_names.clone(),
                display_name: service.display_name.clone(),
                command: service.command.clone(),
                start: service.start,
                readiness: service.readiness,
            };
            let bytes = crate::openrc::render(&name, &state)?;
            let previous_source = self.definition(&name)?;
            if service.kind == zup_plan::IntegrationChangeKind::Create
                && (previous_source.is_some() || self.policy(&name)? != "disabled")
            {
                return Err("foreign OpenRC service or runlevel entry already exists".into());
            }
            if service.previous.configuration().as_ref()
                != self.observe_registration(&name)?.configuration().as_ref()
            {
                return Err("OpenRC registration changed after observation".into());
            }
            input.backend_operations.push(
                crate::service_exec::apply_operation(
                    service,
                    &name,
                    &bytes,
                    true,
                    self.policy(&name)?,
                    previous_source.as_deref(),
                )
                .map_err(|error| error.to_string())?,
            );
        }
        for removal in &execution.removals {
            let OwnedResource::Service {
                installed,
                name,
                privilege,
                ..
            } = &removal.owned
            else {
                continue;
            };
            if *privilege != Privilege::System
                || crate::openrc::resource_name(&removal.key)? != *name
                || self.observe_registration(name)?.configuration().as_ref() != Some(installed)
            {
                return Err("OpenRC removal ownership changed".into());
            }
            let ServiceState::Registration {
                command, failure, ..
            } = installed.as_ref()
            else {
                return Err("OpenRC removal lacks installed configuration".into());
            };
            let (runtime, generation) = self.runtime(
                name,
                command,
                *failure != zup_core::ServiceFailurePolicy::None,
            )?;
            match runtime {
                ServiceRuntimeState::Running => {
                    input.service_controls.push(ServiceControlOperation {
                        key: removal.key.clone(),
                        native_name: name.clone(),
                        privilege: *privilege,
                        stage: zup_transaction::ServiceControlStage::Quiesce,
                        before_configuration: installed.as_ref().clone(),
                        configuration: installed.as_ref().clone(),
                        owned_configuration: installed.as_ref().clone(),
                        before: ServiceTerminalState::Running,
                        after: ServiceTerminalState::Stopped,
                        manager_generation: self.generation()?,
                        before_execution_generation: generation,
                        timeout_seconds: 90,
                        rollback_timeout_seconds: 90,
                        dependencies: Vec::new(),
                    })
                }
                ServiceRuntimeState::Stopped => {}
                _ => {
                    return Err(
                        "cannot remove an OpenRC service with ambiguous runtime state".into(),
                    );
                }
            }
            let operation = crate::service_exec::remove_operation(
                &removal.key,
                name,
                &removal.owned,
                removal.privilege,
            )
            .map_err(|error| error.to_string())?;
            input.retired_keys.push(operation.key.clone());
            input.backend_operations.push(operation);
        }
        zup_transaction::bind_service_dependencies(&mut input.service_controls, execution, ledger)?;
        Ok(input)
    }

    pub fn connect(roots: crate::locations::MachineRoots) -> Result<Self, String> {
        if !active() {
            return Err("an active OpenRC init environment is required".into());
        }
        for executable in [
            "/sbin/rc-service",
            "/sbin/rc-update",
            "/sbin/openrc-run",
            "/sbin/start-stop-daemon",
            "/sbin/supervise-daemon",
        ] {
            crate::prerequisites::trusted_executable(executable)
                .map_err(|error| error.to_string())?;
        }
        trusted_directory(Path::new("/etc/init.d"))?;
        trusted_directory(Path::new("/etc/runlevels"))?;
        trusted_directory(Path::new("/run/openrc"))?;
        Ok(Self { roots })
    }

    fn generation(&self) -> Result<String, String> {
        if !active() {
            return Err("OpenRC init environment disappeared".into());
        }
        let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .map_err(|error| error.to_string())?;
        let init = process_identity(1)?.ok_or("init process is unavailable")?;
        Ok(format!("openrc:{}:{init}", boot.trim()))
    }

    fn source(name: &str) -> Result<PathBuf, String> {
        if !crate::openrc::valid_name(name) {
            return Err("invalid OpenRC service name".into());
        }
        Ok(Path::new("/etc/init.d").join(name))
    }

    fn definition(&self, name: &str) -> Result<Option<Vec<u8>>, String> {
        trusted_directory(Path::new("/etc/init.d"))?;
        let path = Self::source(name)?;
        for override_path in [
            Path::new("/etc/conf.d").join(name),
            Path::new("/etc/init.d").join(format!("{name}.options")),
        ] {
            match std::fs::symlink_metadata(&override_path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                _ => {
                    return Err(format!(
                        "foreign OpenRC configuration override at {}",
                        override_path.display()
                    ));
                }
            }
        }
        let source =
            crate::service_ops::read_canonical_source(&path).map_err(|error| error.to_string())?;
        if source.is_some() {
            let metadata = std::fs::symlink_metadata(path).map_err(|error| error.to_string())?;
            if metadata.uid() != 0 || metadata.mode() & 0o022 != 0 || metadata.mode() & 0o111 == 0 {
                return Err(
                    "OpenRC definition requires root ownership and safe executable permissions"
                        .into(),
                );
            }
        }
        Ok(source)
    }

    fn policy(&self, name: &str) -> Result<&'static str, String> {
        let mut automatic = false;
        for entry in std::fs::read_dir("/etc/runlevels").map_err(|error| error.to_string())? {
            let entry = entry.map_err(|error| error.to_string())?;
            let directory = entry.path();
            trusted_directory(&directory)?;
            match std::fs::symlink_metadata(directory.join(name)) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.to_string()),
                Ok(metadata) => {
                    if entry.file_name() != "default" || !metadata.file_type().is_symlink() {
                        return Err(format!("foreign OpenRC runlevel entry for {name}"));
                    }
                    let target = OwnedDirectory::open(&directory)
                        .map_err(|error| error.to_string())?
                        .read_link_target(name)
                        .map_err(|error| error.to_string())?;
                    if target != Self::source(name)?
                        && target != Path::new("../../init.d").join(name)
                    {
                        return Err(format!(
                            "OpenRC runlevel entry for {name} targets another definition"
                        ));
                    }
                    automatic = true;
                }
            }
        }
        Ok(if automatic { "enabled" } else { "disabled" })
    }

    pub fn observe_registration(&self, name: &str) -> Result<ObservedServiceState, String> {
        let Some(bytes) = self.definition(name)? else {
            if self.policy(name)? != "disabled" {
                return Err("orphaned OpenRC runlevel entry".into());
            }
            return Ok(ObservedServiceState::Absent);
        };
        let state = match crate::openrc::parse(name, &bytes) {
            Ok(state) => state,
            Err(_) => return Ok(ObservedServiceState::InvalidService),
        };
        let ServiceState::Registration {
            execution,
            failure,
            dependencies,
            display_name,
            command,
            start,
            readiness,
        } = state
        else {
            unreachable!()
        };
        let expected = if start == ServiceStart::Automatic {
            "enabled"
        } else {
            "disabled"
        };
        if self.policy(name)? != expected {
            return Ok(ObservedServiceState::InvalidService);
        }
        let runtime_state = Some(
            self.runtime(
                name,
                &command,
                failure != zup_core::ServiceFailurePolicy::None,
            )?
            .0,
        );
        Ok(ObservedServiceState::Service {
            execution,
            failure,
            dependencies,
            display_name,
            command,
            start,
            readiness,
            runtime_state,
        })
    }

    fn runtime(
        &self,
        name: &str,
        command: &zup_plan::CommandSpec,
        supervised: bool,
    ) -> Result<(ServiceRuntimeState, String), String> {
        for (directory, state) in [
            ("starting", ServiceRuntimeState::StartPending),
            ("stopping", ServiceRuntimeState::StopPending),
        ] {
            if std::fs::symlink_metadata(Path::new("/run/openrc").join(directory).join(name))
                .is_ok()
            {
                return Ok((state, "pending".into()));
            }
        }
        let pidfile = Path::new("/run").join(format!("{name}.pid"));
        let options = Path::new("/run/openrc/options").join(name);
        let orphaned = || -> Result<bool, String> {
            if !options.exists() {
                return Ok(false);
            }
            trusted_directory(&options)?;
            for record in ["child_pid", "zup_pid"] {
                if let Some(pid) = read_pid(&options.join(record))?
                    && process_identity(pid)?.is_some()
                {
                    return Ok(true);
                }
            }
            Ok(false)
        };
        let Some(supervisor_pid) = read_pid(&pidfile)? else {
            if orphaned()? {
                return Err("OpenRC has a live runtime without its authoritative PID file".into());
            }
            return Ok((ServiceRuntimeState::Stopped, "stopped".into()));
        };
        let Some(supervisor_generation) = process_identity(supervisor_pid)? else {
            if orphaned()? {
                return Err(
                    "OpenRC supervisor exited while an associated process remains alive".into(),
                );
            }
            return Ok((ServiceRuntimeState::Stopped, "stopped".into()));
        };
        let options = Path::new("/run/openrc/options").join(name);
        trusted_directory(&options)?;
        if trusted_value(&options.join("zup_resource"))?.trim() != name
            || read_pid(&options.join("zup_pid"))? != Some(supervisor_pid)
            || trusted_value(&options.join("zup_start_ticks"))?.trim() != supervisor_generation
        {
            return Err("OpenRC native execution evidence is stale or was replaced".into());
        }
        if supervised {
            let supervisor = std::fs::metadata(format!("/proc/{supervisor_pid}/exe"))
                .map_err(|error| error.to_string())?;
            let expected = std::fs::metadata(
                crate::prerequisites::trusted_executable("/sbin/supervise-daemon")
                    .map_err(|error| error.to_string())?,
            )
            .map_err(|error| error.to_string())?;
            if (supervisor.dev(), supervisor.ino()) != (expected.dev(), expected.ino()) {
                return Err("OpenRC supervisor executable identity changed".into());
            }
        }
        let pid = if supervised {
            let Some(pid) = read_pid(
                &Path::new("/run/openrc/options")
                    .join(name)
                    .join("child_pid"),
            )?
            else {
                return Ok((ServiceRuntimeState::StartPending, "pending".into()));
            };
            pid
        } else {
            supervisor_pid
        };
        let Some(generation) = process_identity(pid)? else {
            return Ok((
                if supervised {
                    ServiceRuntimeState::StartPending
                } else {
                    ServiceRuntimeState::Stopped
                },
                "pending".into(),
            ));
        };
        if supervised {
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .map_err(|error| error.to_string())?;
            if stat
                .rsplit_once(") ")
                .and_then(|(_, fields)| fields.split_ascii_whitespace().nth(1))
                .and_then(|parent| parent.parse::<u32>().ok())
                != Some(supervisor_pid)
            {
                return Err("OpenRC child no longer belongs to its supervisor".into());
            }
        }
        let executable = crate::lowering::to_host_path(&command.executable)
            .map_err(|error| error.to_string())?;
        let live =
            std::fs::metadata(format!("/proc/{pid}/exe")).map_err(|error| error.to_string())?;
        let expected = std::fs::symlink_metadata(&executable).map_err(|error| error.to_string())?;
        if (live.dev(), live.ino()) != (expected.dev(), expected.ino()) {
            return Err("OpenRC process identity differs from the authorized executable".into());
        }
        let source = self
            .definition(name)?
            .ok_or("OpenRC definition disappeared")?;
        let ServiceState::Registration { execution, .. } = crate::openrc::parse(name, &source)?
        else {
            unreachable!()
        };
        let (uid, gid) = execution_ids(&execution)?;
        let status = std::fs::read_to_string(format!("/proc/{pid}/status"))
            .map_err(|error| error.to_string())?;
        for (field, expected) in [("Uid:", uid), ("Gid:", gid)] {
            let ids = status
                .lines()
                .find_map(|line| line.strip_prefix(field))
                .ok_or("native process lacks execution identity")?;
            if ids.split_ascii_whitespace().count() != 4
                || ids
                    .split_ascii_whitespace()
                    .any(|id| id.parse::<u32>() != Ok(expected))
            {
                return Err(
                    "OpenRC runtime execution identity differs from authorized user and group"
                        .into(),
                );
            }
        }
        if !status.lines().any(|line| {
            line.strip_prefix("NoNewPrivs:")
                .is_some_and(|value| value.trim() == "1")
        }) {
            return Err("OpenRC process lacks NoNewPrivileges".into());
        }
        if process_identity(pid)?.as_ref() != Some(&generation) {
            return Err("OpenRC process changed during observation".into());
        }
        let started =
            std::fs::symlink_metadata(Path::new("/run/openrc/started").join(name)).is_ok();
        Ok((
            if started {
                ServiceRuntimeState::Running
            } else {
                ServiceRuntimeState::Unknown
            },
            format!("{supervisor_pid}:{supervisor_generation}:{pid}:{generation}"),
        ))
    }

    fn command(
        &self,
        executable: &str,
        arguments: &[&str],
        timeout: Duration,
    ) -> Result<String, String> {
        let executable = crate::prerequisites::trusted_executable(executable)
            .map_err(|error| error.to_string())?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| error.to_string())?;
        let mut log = tempfile::tempfile().map_err(|error| error.to_string())?;
        let mut error_log = tempfile::tempfile().map_err(|error| error.to_string())?;
        let status = runtime.block_on(async {
            let mut command = tokio::process::Command::new(executable);
            command.args(arguments).env_clear().env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin").env("LC_ALL", "C")
                .stdin(Stdio::null()).stdout(log.try_clone().map_err(|error| error.to_string())?).stderr(error_log.try_clone().map_err(|error| error.to_string())?).kill_on_drop(true);
            let mut child = command.spawn().map_err(|error| error.to_string())?;
            tokio::time::timeout(timeout, child.wait()).await.map_err(|_| "OpenRC command deadline expired; native state must be reconciled before further mutation".to_owned())?.map_err(|error| error.to_string())
        })?;
        use std::io::{Read, Seek};
        log.rewind().map_err(|error| error.to_string())?;
        let mut bytes = Vec::new();
        log.take(1024 * 1024)
            .read_to_end(&mut bytes)
            .map_err(|error| error.to_string())?;
        let diagnostic = String::from_utf8_lossy(&bytes).into_owned();
        if !status.success() {
            error_log.rewind().map_err(|error| error.to_string())?;
            let mut error = String::new();
            error_log
                .take(65536)
                .read_to_string(&mut error)
                .map_err(|error| error.to_string())?;
            return Err(format!(
                "OpenRC command exited with {status}: {diagnostic}{error}"
            ));
        }
        Ok(diagnostic)
    }

    fn set_policy(&self, name: &str, desired: &str) -> Result<(), String> {
        let previous = self.policy(name)?;
        if previous != desired {
            let action = match desired {
                "enabled" => "add",
                "disabled" => "del",
                _ => return Err("unsupported OpenRC startup policy".into()),
            };
            self.command(
                "/sbin/rc-update",
                &[action, name, "default"],
                Duration::from_secs(30),
            )?;
        }
        if self.policy(name)? != desired {
            return Err("OpenRC runlevel update failed verification".into());
        }
        Ok(())
    }

    fn ensure_stopped(&self, name: &str, state: &ServiceState) -> Result<(), String> {
        if let ServiceState::Registration {
            command, failure, ..
        } = state
            && self
                .runtime(
                    name,
                    command,
                    *failure != zup_core::ServiceFailurePolicy::None,
                )?
                .0
                != ServiceRuntimeState::Stopped
        {
            return Err("OpenRC registration mutation requires verified shutdown".into());
        }
        Ok(())
    }

    pub fn prepare(&self, payload: &ServicePayload) -> Result<(), ExecError> {
        let (name, state) = payload_state(payload)?;
        if crate::openrc::render(name, &state).is_err() {
            return Err(refused(name, "unsupported OpenRC registration"));
        }
        if let ServiceState::Registration { execution, .. } = &state {
            execution_ids(execution).map_err(|reason| refused(name, reason))?;
        }
        self.definition(name)
            .map_err(|reason| refused(name, reason))?;
        self.policy(name).map_err(|reason| refused(name, reason))?;
        self.generation().map_err(|reason| refused(name, reason))?;
        Ok(())
    }

    pub fn apply(&mut self, payload: &ServicePayload) -> Result<ServiceReceipt, ExecError> {
        self.prepare(payload)?;
        let (name, state) = payload_state(payload)?;
        let previous_source = self
            .definition(name)
            .map_err(|reason| refused(name, reason))?;
        let previous_policy = self
            .policy(name)
            .map_err(|reason| refused(name, reason))?
            .to_owned();
        let installed_policy;
        let installed_source;
        match payload {
            ServicePayload::Apply {
                service,
                unit_bytes,
                previous_source: expected_source,
                previous_policy: expected_policy,
                ..
            } => {
                if previous_source != *expected_source || previous_policy != *expected_policy {
                    return Err(refused(name, "OpenRC definition changed after planning"));
                }
                if crate::openrc::render(name, &state).map_err(|reason| refused(name, reason))?
                    != *unit_bytes
                {
                    return Err(refused(
                        name,
                        "OpenRC payload differs from authorized configuration",
                    ));
                }
                self.ensure_stopped(name, &state)
                    .map_err(|reason| refused(name, reason))?;
                crate::service_ops::validate_executable_live(&service.command, &self.roots, 0)?;
                let directory = OwnedDirectory::open(Path::new("/etc/init.d"))?;
                if previous_source.is_none() {
                    directory.create_durable_exclusive(
                        name,
                        unit_bytes,
                        rustix::fs::Mode::from_bits_truncate(0o755),
                    )?;
                } else {
                    directory.write_durable(
                        name,
                        unit_bytes,
                        rustix::fs::Mode::from_bits_truncate(0o755),
                    )?;
                }
                installed_policy = if service.start == ServiceStart::Automatic {
                    "enabled"
                } else {
                    "disabled"
                }
                .to_owned();
                self.set_policy(name, &installed_policy)
                    .map_err(|reason| refused(name, reason))?;
                installed_source = Some(zup_core::hash_bytes(unit_bytes).to_hex());
            }
            ServicePayload::Remove { owned, .. } => {
                let OwnedResource::Service { installed, .. } = owned.as_ref() else {
                    return Err(refused(name, "owned OpenRC registration required"));
                };
                if self
                    .observe_registration(name)
                    .map_err(|reason| refused(name, reason))?
                    .configuration()
                    .as_ref()
                    != Some(installed)
                {
                    return Err(refused(name, "OpenRC registration differs from its owner"));
                }
                self.ensure_stopped(name, installed)
                    .map_err(|reason| refused(name, reason))?;
                self.set_policy(name, "disabled")
                    .map_err(|reason| refused(name, reason))?;
                let directory = OwnedDirectory::open(Path::new("/etc/init.d"))?;
                directory.remove_file(name)?;
                directory.sync()?;
                installed_policy = "disabled".into();
                installed_source = None;
            }
        }
        Ok(ServiceReceipt {
            unit: name.into(),
            previous_source_sha256: previous_source
                .as_ref()
                .map(|bytes| zup_core::hash_bytes(bytes).to_hex()),
            installed_source_sha256: installed_source,
            previous_policy,
            installed_policy,
            reloaded: false,
            changes: Vec::new(),
        })
    }

    pub fn verify(&self, receipt: &ServiceReceipt) -> Result<(), ExecError> {
        if self
            .definition(&receipt.unit)
            .map_err(|reason| refused(&receipt.unit, reason))?
            .as_ref()
            .map(|bytes| zup_core::hash_bytes(bytes).to_hex())
            != receipt.installed_source_sha256
            || self
                .policy(&receipt.unit)
                .map_err(|reason| refused(&receipt.unit, reason))?
                != receipt.installed_policy
        {
            return Err(refused(
                &receipt.unit,
                "OpenRC registration changed after execution",
            ));
        }
        Ok(())
    }

    pub fn reconcile(
        &self,
        payload: &ServicePayload,
        receipt: Option<&ServiceReceipt>,
    ) -> Result<zup_transaction::ReconcileResult, ExecError> {
        use zup_transaction::ReconcileResult;
        if let Some(receipt) = receipt {
            return Ok(if self.verify(receipt).is_ok() {
                ReconcileResult::Applied
            } else {
                ReconcileResult::Ambiguous
            });
        }
        let (name, state) = payload_state(payload)?;
        let current = self
            .definition(name)
            .map_err(|reason| refused(name, reason))?;
        let policy = self.policy(name).map_err(|reason| refused(name, reason))?;
        match payload {
            ServicePayload::Apply {
                previous_source,
                previous_policy,
                unit_bytes,
                ..
            } => {
                if current == *previous_source && policy == previous_policy {
                    return Ok(ReconcileResult::NotApplied);
                }
                if previous_source.is_some()
                    && current.as_deref() == Some(unit_bytes)
                    && policy
                        == if matches!(
                            state,
                            ServiceState::Registration {
                                start: ServiceStart::Automatic,
                                ..
                            }
                        ) {
                            "enabled"
                        } else {
                            "disabled"
                        }
                {
                    let receipt = ServiceReceipt {
                        unit: name.into(),
                        previous_source_sha256: previous_source
                            .as_ref()
                            .map(|bytes| zup_core::hash_bytes(bytes).to_hex()),
                        installed_source_sha256: Some(zup_core::hash_bytes(unit_bytes).to_hex()),
                        previous_policy: previous_policy.clone(),
                        installed_policy: policy.into(),
                        reloaded: false,
                        changes: Vec::new(),
                    };
                    return Ok(ReconcileResult::AppliedWithReceipt(
                        zup_transaction::OperationReceipt::Backend {
                            key: crate::service_ops::backend_key_for_unit(name),
                            payload: serde_json::to_vec(&receipt)
                                .map_err(|error| refused(name, error.to_string()))?,
                        },
                    ));
                }
            }
            ServicePayload::Remove { .. }
                if current.as_ref()
                    == Some(
                        &crate::openrc::render(name, &state)
                            .map_err(|reason| refused(name, reason))?,
                    ) =>
            {
                return Ok(ReconcileResult::NotApplied);
            }
            _ => {}
        }
        Ok(ReconcileResult::Ambiguous)
    }

    pub fn rollback(
        &mut self,
        payload: &ServicePayload,
        receipt: &ServiceReceipt,
    ) -> Result<(), ExecError> {
        self.verify(receipt)?;
        let (name, state) = payload_state(payload)?;
        if name != receipt.unit {
            return Err(refused(name, "OpenRC rollback receipt identity mismatch"));
        }
        self.ensure_stopped(name, &state)
            .map_err(|reason| refused(name, reason))?;
        let previous = match payload {
            ServicePayload::Apply {
                previous_source, ..
            } => previous_source.clone(),
            ServicePayload::Remove { .. } => {
                Some(crate::openrc::render(name, &state).map_err(|reason| refused(name, reason))?)
            }
        };
        if previous
            .as_ref()
            .map(|bytes| zup_core::hash_bytes(bytes).to_hex())
            != receipt.previous_source_sha256
        {
            return Err(refused(
                name,
                "OpenRC rollback source does not match durable receipt",
            ));
        }
        let directory = OwnedDirectory::open(Path::new("/etc/init.d"))?;
        match previous {
            Some(bytes) => {
                directory.write_durable(
                    name,
                    &bytes,
                    rustix::fs::Mode::from_bits_truncate(0o755),
                )?;
                self.set_policy(name, &receipt.previous_policy)
                    .map_err(|reason| refused(name, reason))?;
            }
            None => {
                self.set_policy(name, "disabled")
                    .map_err(|reason| refused(name, reason))?;
                directory.remove_file(name)?;
                directory.sync()?;
            }
        }
        if self
            .definition(name)
            .map_err(|reason| refused(name, reason))?
            .as_ref()
            .map(|bytes| zup_core::hash_bytes(bytes).to_hex())
            != receipt.previous_source_sha256
            || self.policy(name).map_err(|reason| refused(name, reason))? != receipt.previous_policy
        {
            return Err(refused(name, "OpenRC rollback failed verification"));
        }
        Ok(())
    }
}

impl NativeServiceManager for Openrc {
    fn stopped_runtime_is_quiescent(
        &mut self,
        operation: &ServiceControlOperation,
    ) -> Result<bool, String> {
        let current = self.observe(operation)?;
        Ok(current.configuration == operation.configuration
            && current.manager_generation == operation.manager_generation
            && current.runtime == ServiceRuntimeState::Stopped)
    }
    fn stopped_start_can_be_compensated(
        &mut self,
        operation: &ServiceControlOperation,
    ) -> Result<bool, String> {
        let current = self.observe(operation)?;
        Ok(current.configuration == operation.configuration
            && current.manager_generation == operation.manager_generation
            && current.runtime == ServiceRuntimeState::Stopped)
    }

    fn observe(
        &mut self,
        operation: &ServiceControlOperation,
    ) -> Result<ServiceObservation, String> {
        operation.validate()?;
        if operation.privilege != Privilege::System
            || crate::openrc::resource_name(&operation.key)? != operation.native_name
        {
            return Err("OpenRC service authority mismatch".into());
        }
        let configuration = self
            .observe_registration(&operation.native_name)?
            .configuration()
            .ok_or("unrecognized OpenRC registration")?;
        let ServiceState::Registration {
            command, failure, ..
        } = &configuration
        else {
            return Err("OpenRC registration is absent".into());
        };
        let (runtime, execution_generation) = self.runtime(
            &operation.native_name,
            command,
            *failure != zup_core::ServiceFailurePolicy::None,
        )?;
        Ok(ServiceObservation {
            configuration,
            runtime,
            execution_generation,
            manager_generation: self.generation()?,
        })
    }

    fn transition(
        &mut self,
        operation: &ServiceControlOperation,
        state: ServiceTerminalState,
        expected: &ServiceObservation,
    ) -> Result<(), String> {
        if &self.observe(operation)? != expected
            || expected.configuration != operation.configuration
            || expected.manager_generation != operation.manager_generation
        {
            return Err("OpenRC service changed before native control".into());
        }
        let ServiceState::Registration { command, .. } = &operation.configuration else {
            return Err("OpenRC registration required".into());
        };
        if state == ServiceTerminalState::Running {
            crate::service_ops::validate_executable_live(command, &self.roots, 0)
                .map_err(|error| error.to_string())?;
        }
        if state == ServiceTerminalState::Stopped {
            let tool = ["/usr/libexec/rc/bin/rc-depend", "/lib/rc/bin/rc-depend"]
                .into_iter()
                .find(|path| Path::new(path).exists())
                .ok_or("OpenRC dependency inspection is unavailable")?;
            let dependents = self.command(
                tool,
                &[
                    "--quiet",
                    "--nocolor",
                    "--type",
                    "needsme",
                    "--stopping",
                    &operation.native_name,
                ],
                Duration::from_secs(30),
            )?;
            for dependent in dependents
                .split_ascii_whitespace()
                .filter(|name| *name != operation.native_name)
            {
                if !dependent
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
                {
                    return Err("OpenRC returned an ambiguous dependent service identity".into());
                }
                if std::fs::symlink_metadata(Path::new("/run/openrc/started").join(dependent))
                    .is_ok()
                {
                    return Err(format!(
                        "running dependent service {dependent} must be explicitly managed and quiesced before {}",
                        operation.native_name
                    ));
                }
            }
        } else if let ServiceState::Registration { dependencies, .. } = &operation.configuration {
            for dependency in dependencies {
                if !matches!(
                    self.observe_registration(dependency)?,
                    ObservedServiceState::Service {
                        runtime_state: Some(ServiceRuntimeState::Running),
                        ..
                    }
                ) {
                    return Err(format!(
                        "required managed OpenRC service {dependency} is not running"
                    ));
                }
            }
        }
        let action = match state {
            ServiceTerminalState::Running => "start",
            ServiceTerminalState::Stopped => "stop",
        };
        let deadline = std::time::Instant::now() + operation.timeout_for(state);
        self.command(
            "/sbin/rc-service",
            &["--nodeps", &operation.native_name, action],
            operation.timeout_for(state),
        )?;
        zup_transaction::fault_injection::boundary(
            zup_transaction::fault_injection::Boundary::NativeRequestSubmitted,
            &operation.native_name,
        );
        let mut interval = Duration::from_millis(20);
        loop {
            let current = self.observe(operation)?;
            if current.configuration != expected.configuration
                || current.manager_generation != expected.manager_generation
            {
                return Err("OpenRC registration or manager changed during control".into());
            }
            if state.matches(current.runtime) {
                return Ok(());
            }
            let now = std::time::Instant::now();
            if now >= deadline
                || !matches!(
                    current.runtime,
                    ServiceRuntimeState::StartPending | ServiceRuntimeState::StopPending
                )
            {
                return Err("OpenRC request did not reach its authorized terminal state".into());
            }
            std::thread::park_timeout(interval.min(deadline - now));
            interval = (interval * 2).min(Duration::from_millis(200));
        }
    }
}

fn trusted_directory(path: &Path) -> Result<(), String> {
    crate::fs::anchor_reachable(path).map_err(|error| error.to_string())?;
    for ancestor in path.ancestors() {
        let metadata = std::fs::symlink_metadata(ancestor).map_err(|error| error.to_string())?;
        let native_runtime_group = ancestor.starts_with("/run/openrc") && metadata.gid() == 0;
        if !metadata.is_dir()
            || metadata.uid() != 0
            || metadata.mode() & 0o002 != 0
            || metadata.mode() & 0o020 != 0 && !native_runtime_group
        {
            return Err(format!("untrusted OpenRC directory {}", ancestor.display()));
        }
    }
    Ok(())
}

fn read_pid(path: &Path) -> Result<Option<u32>, String> {
    let directory = OwnedDirectory::open(path.parent().ok_or("PID state lacks a parent")?)
        .map_err(|error| error.to_string())?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("invalid PID state name")?;
    if directory
        .kind_or_absent(name)
        .map_err(|error| error.to_string())?
        .is_none()
    {
        return Ok(None);
    }
    let pid = trusted_value(path)?
        .trim()
        .parse::<u32>()
        .map_err(|error| error.to_string())?;
    if pid <= 1 {
        return Err("OpenRC process identity cannot name init".into());
    }
    Ok(Some(pid))
}

fn trusted_value(path: &Path) -> Result<String, String> {
    let directory = OwnedDirectory::open(path.parent().ok_or("native evidence lacks a parent")?)
        .map_err(|error| error.to_string())?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("invalid native evidence name")?;
    let mut file = directory
        .open_regular_read(name)
        .map_err(|error| error.to_string())?;
    let metadata = file.metadata().map_err(|error| error.to_string())?;
    if metadata.uid() != 0 || metadata.mode() & 0o022 != 0 || metadata.len() > 256 {
        return Err("untrusted OpenRC native execution evidence".into());
    }
    use std::io::Read;
    let mut bytes = String::new();
    file.by_ref()
        .take(257)
        .read_to_string(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() > 256 {
        return Err("oversized native execution evidence".into());
    }
    Ok(bytes)
}

fn process_identity(pid: u32) -> Result<Option<String>, String> {
    let text = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    let (_, fields) = text
        .rsplit_once(") ")
        .ok_or("invalid native process status")?;
    let fields: Vec<_> = fields.split_ascii_whitespace().collect();
    if matches!(fields.first(), Some(&"Z" | &"X")) {
        return Ok(None);
    }
    Ok(Some(
        fields
            .get(19)
            .ok_or("native process lacks start generation")?
            .to_string(),
    ))
}

fn execution_ids(execution: &zup_core::ServiceExecution) -> Result<(u32, u32), String> {
    let zup_core::ServiceExecution::OpenrcUser { user, group } = execution else {
        return Err("OpenRC execution identity required".into());
    };
    let account = uzers::get_user_by_name(user)
        .ok_or_else(|| format!("OpenRC service account {user} does not exist"))?;
    let gid = match group {
        Some(group) => uzers::get_group_by_name(group)
            .ok_or_else(|| format!("OpenRC service group {group} does not exist"))?
            .gid(),
        None => account.primary_group_id(),
    };
    if account.uid() == 0 && user != "root"
        || gid == 0 && user != "root" && group.as_deref() != Some("root")
    {
        return Err("root execution authority must be requested explicitly".into());
    }
    Ok((account.uid(), gid))
}

fn payload_state(payload: &ServicePayload) -> Result<(&str, ServiceState), ExecError> {
    match payload {
        ServicePayload::Apply { service, unit, .. } => {
            if crate::openrc::resource_name(&service.key).map_err(|reason| refused(unit, reason))?
                != *unit
                || service.name != *unit
                || service.privilege != Privilege::System
            {
                return Err(refused(unit, "OpenRC payload authority mismatch"));
            }
            Ok((
                unit,
                ServiceState::Registration {
                    execution: service.execution.clone(),
                    failure: service.failure,
                    dependencies: service.dependency_names.clone(),
                    display_name: service.display_name.clone(),
                    command: service.command.clone(),
                    start: service.start,
                    readiness: service.readiness,
                },
            ))
        }
        ServicePayload::Remove { key, unit, owned } => {
            let OwnedResource::Service {
                installed,
                privilege: Privilege::System,
                name,
                ..
            } = owned.as_ref()
            else {
                return Err(refused(
                    unit,
                    "OpenRC removal requires machine service ownership",
                ));
            };
            if crate::openrc::resource_name(key).as_ref() != Ok(unit) || name != unit {
                return Err(refused(unit, "OpenRC removal identity mismatch"));
            }
            Ok((unit, installed.as_ref().clone()))
        }
    }
}
