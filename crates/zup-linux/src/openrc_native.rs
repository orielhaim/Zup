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

fn state_marked(name: &str, state: &str) -> Result<bool, String> {
    match std::fs::symlink_metadata(Path::new("/run/openrc").join(state).join(name)) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.to_string()),
    }
}

fn live_runtime_records(name: &str) -> Result<bool, String> {
    if let Some(controller) = ControllerIdentity::read(name)?
        && process_identity(controller.pid)?.is_some()
    {
        return Ok(true);
    }
    if let Some(pid) = read_pid(&Path::new("/run").join(format!("{name}.pid")))?
        && process_identity(pid)?.is_some()
    {
        return Ok(true);
    }
    let options = Path::new("/run/openrc/options").join(name);
    match std::fs::symlink_metadata(&options) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.to_string()),
        Ok(_) => trusted_directory(&options)?,
    }
    for record in ["child_pid", "zup_pid"] {
        if let Some(pid) = read_pid(&options.join(record))?
            && process_identity(pid)?.is_some()
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn ensure_absent_runtime(name: &str) -> Result<(), String> {
    if ProcessScope::observe(name)?.populated || live_runtime_records(name)? {
        return Err("OpenRC registration is absent while native runtime evidence is live".into());
    }
    for state in ["starting", "started", "stopping"] {
        if state_marked(name, state)? {
            return Err(
                "OpenRC registration is absent while native bookkeeping remains active".into(),
            );
        }
    }
    Ok(())
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
    controller_binary: (u64, u64),
}

#[derive(Debug, PartialEq, Eq)]
struct ControllerIdentity {
    instance: String,
    pid: u32,
    ticks: u64,
}

impl ControllerIdentity {
    fn read(name: &str) -> Result<Option<Self>, String> {
        let path = Path::new("/run/openrc").join(format!("supervise-{name}.identity"));
        match std::fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.to_string()),
            Ok(_) => {}
        }
        let record = trusted_value(&path)?;
        let fields = record.split_ascii_whitespace().collect::<Vec<_>>();
        let [instance, pid, ticks] = fields.as_slice() else {
            return Err("invalid OpenRC controller identity".into());
        };
        if instance.len() != 32 || !instance.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err("invalid OpenRC controller instance".into());
        }
        let pid = pid.parse::<u32>().map_err(|error| error.to_string())?;
        let ticks = ticks.parse::<u64>().map_err(|error| error.to_string())?;
        if pid <= 1 || ticks == 0 {
            return Err("invalid OpenRC controller process generation".into());
        }
        Ok(Some(Self {
            instance: instance.to_string(),
            pid,
            ticks,
        }))
    }
}

#[derive(Debug, PartialEq, Eq)]
enum ChildPhase {
    Pending,
    Running,
    Reaped,
}

#[derive(Debug, PartialEq, Eq)]
struct ControllerChild {
    pid: u32,
    ticks: u64,
    sequence: u64,
    phase: ChildPhase,
}

impl ControllerChild {
    fn read(name: &str, controller: &ControllerIdentity) -> Result<Self, String> {
        let record =
            trusted_value(&Path::new("/run/openrc").join(format!("supervise-{name}.child")))?;
        let fields = record.split_ascii_whitespace().collect::<Vec<_>>();
        let [instance, pid, ticks, sequence, phase] = fields.as_slice() else {
            return Err("invalid OpenRC child identity".into());
        };
        if *instance != controller.instance {
            return Err("OpenRC child belongs to another controller instance".into());
        }
        let pid = pid.parse::<u32>().map_err(|error| error.to_string())?;
        let ticks = ticks.parse::<u64>().map_err(|error| error.to_string())?;
        let sequence = sequence.parse::<u64>().map_err(|error| error.to_string())?;
        if pid <= 1 || ticks == 0 || sequence == 0 {
            return Err("invalid OpenRC child process generation".into());
        }
        let phase = match *phase {
            "pending" => ChildPhase::Pending,
            "running" => ChildPhase::Running,
            "reaped" => ChildPhase::Reaped,
            _ => return Err("invalid OpenRC child execution phase".into()),
        };
        Ok(Self {
            pid,
            ticks,
            sequence,
            phase,
        })
    }
}

enum RuntimeSnapshotError {
    Changed,
    Refused(String),
}

impl From<String> for RuntimeSnapshotError {
    fn from(reason: String) -> Self {
        Self::Refused(reason)
    }
}

impl From<&str> for RuntimeSnapshotError {
    fn from(reason: &str) -> Self {
        Self::Refused(reason.into())
    }
}

fn process_snapshot<T>(
    read: impl FnOnce() -> std::io::Result<T>,
) -> Result<T, RuntimeSnapshotError> {
    read().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            RuntimeSnapshotError::Changed
        } else {
            RuntimeSnapshotError::Refused(error.to_string())
        }
    })
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
            if service.previous == ObservedServiceState::Absent {
                ensure_absent_runtime(&name)?;
            } else {
                ProcessScope::observe(&name)?;
            }
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
            if (affected.contains(&service.key) || service.preserves_runtime())
                && (owned.is_some() || service.previous == ObservedServiceState::Absent)
            {
                let generation = match service.previous.configuration() {
                    Some(ServiceState::Registration { ref command, .. }) => {
                        self.runtime(&name, command)?.1
                    }
                    Some(ServiceState::Absent) => "stopped".into(),
                    None => return Err("unrecognized OpenRC service cannot be controlled".into()),
                };
                input
                    .service_controls
                    .extend(zup_transaction::plan_service_controls(
                        service,
                        owned,
                        affected.contains(&service.key),
                        name.clone(),
                        self.generation()?,
                        generation,
                    )?);
            }
            if service.kind == zup_plan::IntegrationChangeKind::NoOp
                && !affected.contains(&service.key)
            {
                continue;
            }
            if matches!(
                service.kind,
                zup_plan::IntegrationChangeKind::Conflict | zup_plan::IntegrationChangeKind::Drift
            ) {
                return Err("OpenRC service has an ownership conflict or unapproved drift".into());
            }
            let state = service.desired_configuration();
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
                timeouts,
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
            let ServiceState::Registration { command, .. } = installed.as_ref() else {
                return Err("OpenRC removal lacks installed configuration".into());
            };
            let (runtime, generation) = self.runtime(name, command)?;
            match runtime {
                ServiceRuntimeState::Running | ServiceRuntimeState::Stopped => {
                    input.service_controls.push(ServiceControlOperation {
                        intent: zup_transaction::ServiceControlIntent::Transition,
                        key: removal.key.clone(),
                        native_name: name.clone(),
                        privilege: *privilege,
                        stage: zup_transaction::ServiceControlStage::Quiesce,
                        before_configuration: installed.as_ref().clone(),
                        configuration: zup_transaction::disabled_startup(installed)
                            .expect("registration"),
                        owned_configuration: installed.as_ref().clone(),
                        before: if runtime == ServiceRuntimeState::Running {
                            ServiceTerminalState::Running
                        } else {
                            ServiceTerminalState::Stopped
                        },
                        after: ServiceTerminalState::Stopped,
                        manager_generation: self.generation()?,
                        before_execution_generation: generation,
                        timeouts: *timeouts,
                        dependencies: Vec::new(),
                    })
                }
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
        crate::service_ops::bind_startup_inhibition(&mut input)
            .map_err(|error| error.to_string())?;
        Ok(input)
    }

    pub fn connect(roots: crate::locations::MachineRoots) -> Result<Self, String> {
        crate::service_host::validate_init_view()?;
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
        let binary =
            std::fs::metadata("/sbin/supervise-daemon").map_err(|error| error.to_string())?;
        let manager = Self {
            roots,
            controller_binary: (binary.dev(), binary.ino()),
        };
        if manager
            .command(
                "/sbin/supervise-daemon",
                &["--control-features"],
                Duration::from_secs(5),
            )?
            .trim()
            != "pidfd-control-v1"
        {
            return Err("OpenRC requires the pinned pidfd-control-v1 native controller".into());
        }
        manager.generation()?;
        Ok(manager)
    }

    fn generation(&self) -> Result<String, String> {
        crate::service_host::validate_init_view()?;
        if !active() {
            return Err("OpenRC init environment disappeared".into());
        }
        let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .map_err(|error| error.to_string())?;
        let init = process_identity(1)?.ok_or("init process is unavailable")?;
        let binary = std::fs::metadata(
            crate::prerequisites::trusted_executable("/sbin/supervise-daemon")
                .map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?;
        if (binary.dev(), binary.ino()) != self.controller_binary {
            return Err("OpenRC native controller changed after capability negotiation".into());
        }
        Ok(format!(
            "openrc:{}:{init}:{}:{}:pidfd-control-v1",
            boot.trim(),
            binary.dev(),
            binary.ino()
        ))
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
            ensure_absent_runtime(name)?;
            return Ok(ObservedServiceState::Absent);
        };
        let state = match crate::openrc::parse(name, &bytes) {
            Ok(state) => state,
            Err(_) => return Ok(ObservedServiceState::InvalidService),
        };
        let ServiceState::Registration {
            native_timeouts,
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
        let runtime_state = Some(self.runtime(name, &command)?.0);
        Ok(ObservedServiceState::Service {
            native_timeouts,
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
    ) -> Result<(ServiceRuntimeState, String), String> {
        for _ in 0..3 {
            match self.runtime_snapshot(name, command) {
                Ok(snapshot) => return Ok(snapshot),
                Err(RuntimeSnapshotError::Changed) => continue,
                Err(RuntimeSnapshotError::Refused(reason)) => return Err(reason),
            }
        }
        Err("OpenRC process did not stabilize during identity observation".into())
    }

    fn runtime_snapshot(
        &self,
        name: &str,
        command: &zup_plan::CommandSpec,
    ) -> Result<(ServiceRuntimeState, String), RuntimeSnapshotError> {
        for (directory, state) in [
            ("starting", ServiceRuntimeState::StartPending),
            ("stopping", ServiceRuntimeState::StopPending),
        ] {
            if state_marked(name, directory)? {
                return Ok((state, "pending".into()));
            }
        }
        let scope = ProcessScope::observe(name)?;
        let Some(controller) = ControllerIdentity::read(name)? else {
            if scope.populated || live_runtime_records(name)? {
                return Err("OpenRC runtime lacks its private controller identity".into());
            }
            return Ok((ServiceRuntimeState::Stopped, "stopped".into()));
        };
        let supervisor_pid = controller.pid;
        if process_identity(supervisor_pid)?.as_deref() != Some(&controller.ticks.to_string()) {
            if scope.populated || live_runtime_records(name)? {
                return Err("OpenRC controller exited while its process scope remains live".into());
            }
            return Ok((ServiceRuntimeState::Stopped, "stopped".into()));
        }
        let supervisor =
            process_snapshot(|| std::fs::metadata(format!("/proc/{supervisor_pid}/exe")))?;
        let expected = std::fs::metadata(
            crate::prerequisites::trusted_executable("/sbin/supervise-daemon")
                .map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?;
        if (supervisor.dev(), supervisor.ino()) != (expected.dev(), expected.ino()) {
            return Err("OpenRC controller executable identity changed".into());
        }
        scope.verify_member(supervisor_pid)?;
        let child = ControllerChild::read(name, &controller)?;
        if child.phase != ChildPhase::Running {
            return Ok((ServiceRuntimeState::StartPending, "pending".into()));
        }
        let pid = child.pid;
        let Some(generation) = process_identity(pid)? else {
            return Err(RuntimeSnapshotError::Changed);
        };
        if generation != child.ticks.to_string() {
            return Err("OpenRC child process generation was replaced".into());
        }
        let stat = process_snapshot(|| std::fs::read_to_string(format!("/proc/{pid}/stat")))?;
        if stat
            .rsplit_once(") ")
            .and_then(|(_, fields)| fields.split_ascii_whitespace().nth(1))
            .and_then(|parent| parent.parse::<u32>().ok())
            != Some(supervisor_pid)
        {
            return Err("OpenRC child no longer belongs to its controller".into());
        }
        let executable = crate::lowering::to_host_path(&command.executable)
            .map_err(|error| error.to_string())?;
        let live = process_snapshot(|| std::fs::metadata(format!("/proc/{pid}/exe")))?;
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
        let status = process_snapshot(|| std::fs::read_to_string(format!("/proc/{pid}/status")))?;
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
        scope.verify_member(supervisor_pid)?;
        scope.verify_member(pid)?;
        if ProcessScope::observe(name)? != scope {
            return Err(RuntimeSnapshotError::Changed);
        }
        let final_image = process_snapshot(|| std::fs::metadata(format!("/proc/{pid}/exe")))?;
        if process_identity(pid)?.as_ref() != Some(&generation)
            || ControllerIdentity::read(name)?.as_ref() != Some(&controller)
            || ControllerChild::read(name, &controller)? != child
            || process_identity(supervisor_pid)?.as_deref() != Some(&controller.ticks.to_string())
        {
            return Err(RuntimeSnapshotError::Changed);
        }
        if (final_image.dev(), final_image.ino()) != (expected.dev(), expected.ino()) {
            return Err("OpenRC process executable changed during observation".into());
        }
        let started = state_marked(name, "started")?;
        Ok((
            if started {
                ServiceRuntimeState::Running
            } else {
                ServiceRuntimeState::Unknown
            },
            format!(
                "{}:{}:{supervisor_pid}:{}:{pid}:{generation}:{}",
                scope
                    .inode
                    .ok_or("OpenRC runtime lacks its native process scope")?,
                controller.instance,
                controller.ticks,
                child.sequence
            ),
        ))
    }

    fn command(
        &self,
        executable: &str,
        arguments: &[&str],
        timeout: Duration,
    ) -> Result<String, String> {
        let controller_command = executable == "/sbin/supervise-daemon";
        let executable = crate::prerequisites::trusted_executable(executable)
            .map_err(|error| error.to_string())?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| error.to_string())?;
        let mut log = tempfile::tempfile().map_err(|error| error.to_string())?;
        let mut error_log = tempfile::tempfile().map_err(|error| error.to_string())?;
        let status = runtime.block_on(async {
            let mut command = tokio::process::Command::new(&executable);
            command.args(arguments).env_clear().env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin").env("LC_ALL", "C")
                .stdin(Stdio::null()).stdout(log.try_clone().map_err(|error| error.to_string())?).stderr(error_log.try_clone().map_err(|error| error.to_string())?).kill_on_drop(true);
            if controller_command
                && let Some(name) = arguments.first().filter(|name| crate::openrc::valid_name(name))
            {
                command.env("RC_SVCNAME", name);
            }
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
        if let ServiceState::Registration { command, .. } = state
            && self.runtime(name, command)?.0 != ServiceRuntimeState::Stopped
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
        if self
            .definition(name)
            .map_err(|reason| refused(name, reason))?
            .is_none()
        {
            ensure_absent_runtime(name).map_err(|reason| refused(name, reason))?;
        } else {
            ProcessScope::observe(name).map_err(|reason| refused(name, reason))?;
        }
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
                crate::service_ops::validate_executable_live(&service.command, &self.roots, 0)?;
                let desired_policy = if service.start == ServiceStart::Automatic {
                    "enabled"
                } else {
                    "disabled"
                };
                if previous_source.as_deref() == Some(unit_bytes)
                    && previous_policy == desired_policy
                {
                    let digest = zup_core::hash_bytes(unit_bytes).to_hex();
                    return Ok(ServiceReceipt {
                        unit: name.into(),
                        previous_source_sha256: Some(digest.clone()),
                        installed_source_sha256: Some(digest),
                        previous_policy,
                        installed_policy: desired_policy.into(),
                        reloaded: false,
                        changes: Vec::new(),
                    });
                }
                if !service.preserves_runtime() {
                    self.ensure_stopped(name, &state)
                        .map_err(|reason| refused(name, reason))?;
                }
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
        let preserve_runtime = matches!(payload,
            ServicePayload::Apply { service, .. } if service.preserves_runtime());
        if !preserve_runtime {
            self.ensure_stopped(name, &state)
                .map_err(|reason| refused(name, reason))?;
        }
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
            Err("OpenRC activation inhibition did not establish quiescence".into())
        }
    }
    fn stopped_runtime_is_quiescent(
        &mut self,
        operation: &ServiceControlOperation,
        configuration: &zup_plan::ServiceState,
    ) -> Result<bool, String> {
        let current = self.observe(operation)?;
        Ok(&current.configuration == configuration
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
        let ServiceState::Registration { command, .. } = &configuration else {
            return Err("OpenRC registration is absent".into());
        };
        let (runtime, execution_generation) = self.runtime(&operation.native_name, command)?;
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
        deadline: std::time::Instant,
    ) -> Result<ServiceObservation, String> {
        let remaining = || {
            deadline
                .checked_duration_since(std::time::Instant::now())
                .filter(|duration| !duration.is_zero())
                .ok_or_else(|| "OpenRC lifecycle deadline expired".to_owned())
        };
        if &self.observe(operation)? != expected
            || expected.configuration != operation.before_configuration
            || expected.manager_generation != operation.manager_generation
        {
            return Err("OpenRC service changed before native control".into());
        }
        if operation.before_configuration != operation.configuration {
            remaining()?;
            let bytes = crate::openrc::render(&operation.native_name, &operation.configuration)?;
            let directory = OwnedDirectory::open(Path::new("/etc/init.d"))
                .map_err(|error| error.to_string())?;
            directory
                .write_durable(
                    &operation.native_name,
                    &bytes,
                    rustix::fs::Mode::from_bits_truncate(0o755),
                )
                .map_err(|error| error.to_string())?;
            let ServiceState::Registration { start, .. } = operation.configuration else {
                return Err("OpenRC registration required".into());
            };
            self.set_policy(
                &operation.native_name,
                if start == ServiceStart::Automatic {
                    "enabled"
                } else {
                    "disabled"
                },
            )?;
            zup_transaction::fault_injection::boundary(
                zup_transaction::fault_injection::Boundary::NativeConfigurationChanged,
                &operation.native_name,
            );
            remaining()?;
        }
        let ServiceState::Registration { command, .. } = &operation.configuration else {
            return Err("OpenRC registration required".into());
        };
        if state == ServiceTerminalState::Running {
            let path = crate::service_ops::validate_executable_live(command, &self.roots, 0)
                .map_err(|error| error.to_string())?;
            let (directory, name) =
                crate::fs::open_parent(&path).map_err(|error| error.to_string())?;
            crate::openrc::validate_executable(
                &directory
                    .read_regular(&name)
                    .map_err(|error| error.to_string())?,
                command.executable.target(),
            )?;
        }
        let current = self.observe(operation)?;
        if current.configuration != operation.configuration
            || current.manager_generation != expected.manager_generation
            || current.runtime != expected.runtime
            || current.execution_generation != expected.execution_generation
        {
            return Err("OpenRC authority or execution changed after activation update".into());
        }
        if current.runtime == ServiceRuntimeState::Stopped
            && state_marked(&operation.native_name, "started")?
        {
            self.command(
                "/sbin/rc-service",
                &["--nodeps", &operation.native_name, "zap"],
                remaining()?,
            )?;
            if self.observe(operation)? != current {
                return Err(
                    "OpenRC runtime or registration changed while resetting stopped bookkeeping"
                        .into(),
                );
            }
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
                remaining()?,
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
                if !matches!(
                    self.observe_registration(dependent)?,
                    ObservedServiceState::Service {
                        runtime_state: Some(ServiceRuntimeState::Stopped),
                        ..
                    }
                ) {
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
        let dispatch = self.observe(operation)?;
        if dispatch != current {
            return Err("OpenRC authority or execution changed before native dispatch".into());
        }
        if !state.matches(dispatch.runtime) {
            match state {
                ServiceTerminalState::Running => {
                    self.command(
                        "/sbin/rc-service",
                        &["--nodeps", &operation.native_name, "start"],
                        remaining()?,
                    )?;
                }
                ServiceTerminalState::Stopped => {
                    let controller = ControllerIdentity::read(&operation.native_name)?
                        .ok_or("OpenRC shutdown lacks its controller identity")?;
                    let child = ControllerChild::read(&operation.native_name, &controller)?;
                    if self.observe(operation)? != dispatch {
                        return Err("OpenRC execution changed before guarded shutdown".into());
                    }
                    let child_identity =
                        format!("{}:{}:{}", child.pid, child.ticks, child.sequence);
                    let timeout = remaining()?.as_secs_f64().ceil().to_string();
                    self.command(
                        "/sbin/supervise-daemon",
                        &[
                            &operation.native_name,
                            "--shutdown",
                            "--shutdown-timeout",
                            &timeout,
                            "--expected-instance",
                            &controller.instance,
                            "--expected-child",
                            &child_identity,
                        ],
                        remaining()?,
                    )?;
                }
            }
        }
        zup_transaction::fault_injection::boundary(
            zup_transaction::fault_injection::Boundary::NativeRequestSubmitted,
            &operation.native_name,
        );
        let mut interval = Duration::from_millis(20);
        loop {
            remaining()?;
            let current = self.observe(operation)?;
            if current.configuration != operation.configuration
                || current.manager_generation != expected.manager_generation
            {
                return Err("OpenRC registration or manager changed during control".into());
            }
            if state.matches(current.runtime) {
                remaining()?;
                return Ok(current);
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

fn trusted_scope_control(path: &Path) -> Result<(), String> {
    let metadata = std::fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    if !metadata.is_file() || metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
        return Err("OpenRC process scope is delegated or has untrusted controls".into());
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
struct ProcessScope {
    name: String,
    inode: Option<u64>,
    populated: bool,
}

impl ProcessScope {
    fn observe(name: &str) -> Result<Self, String> {
        if !crate::openrc::valid_name(name) {
            return Err("invalid OpenRC process scope name".into());
        }
        let root = Path::new("/sys/fs/cgroup");
        trusted_directory(root)?;
        if rustix::fs::statfs(root)
            .map_err(|error| error.to_string())?
            .f_type
            != 0x6367_7270
        {
            return Err("OpenRC services require cgroup v2 mounted at /sys/fs/cgroup".into());
        }
        for control in ["cgroup.procs", "cgroup.threads"] {
            trusted_scope_control(&root.join(control))?;
        }
        let path = root.join(format!("openrc.{name}"));
        let inode = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => {
                trusted_directory(&path)?;
                for control in ["cgroup.procs", "cgroup.threads"] {
                    trusted_scope_control(&path.join(control))?;
                }
                Some(metadata.ino())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.to_string()),
        };
        let populated = if inode.is_some() {
            let events = trusted_value(&path.join("cgroup.events"))?;
            match events
                .lines()
                .find_map(|line| line.strip_prefix("populated "))
            {
                Some("0") => false,
                Some("1") => true,
                _ => {
                    return Err(
                        "OpenRC process scope lacks authoritative population evidence".into(),
                    );
                }
            }
        } else {
            false
        };
        Ok(Self {
            name: name.to_owned(),
            inode,
            populated,
        })
    }

    fn verify_member(&self, pid: u32) -> Result<(), RuntimeSnapshotError> {
        let membership =
            process_snapshot(|| std::fs::read_to_string(format!("/proc/{pid}/cgroup")))?;
        if self.inode.is_none()
            || !self.populated
            || membership.trim() != format!("0::/openrc.{}", self.name)
        {
            return Err("OpenRC process is outside its protected native cgroup".into());
        }
        Ok(())
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
            Ok((unit, service.desired_configuration()))
        }
        ServicePayload::Remove {
            key, unit, owned, ..
        } => {
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
