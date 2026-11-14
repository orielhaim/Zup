use crate::openrc::Admission;
use crate::{error::ExecError, fs::OwnedDirectory};
use std::{
    collections::BTreeMap,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use zup_core::{Privilege, ServiceStart};
use zup_plan::{ObservedServiceState, OwnedResource, ServiceRuntimeState, ServiceState};
use zup_plan::{ServiceControlOperation, ServiceTerminalState};
use zup_transaction::{NativeServiceManager, ServiceObservation};

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
    if let Some(pid) = read_pid(&options.join("child_pid"))?
        && process_identity(pid)?.is_some()
    {
        return Ok(true);
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunlevelPolicy {
    Manual,
    Automatic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeProcess {
    pid: u32,
    ticks: u64,
}

impl NativeProcess {
    fn observe(pid: u32) -> Result<Option<Self>, String> {
        process_identity(pid)?
            .map(|ticks| {
                ticks
                    .parse()
                    .map(|ticks| Self { pid, ticks })
                    .map_err(|error: std::num::ParseIntError| error.to_string())
            })
            .transpose()
    }

    fn unchanged(self) -> Result<bool, String> {
        Ok(Self::observe(self.pid)? == Some(self))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ScopeIdentity {
    device: u64,
    inode: u64,
}

#[derive(Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeExecution {
    scope: ScopeIdentity,
    supervisor: Option<NativeProcess>,
    process: NativeProcess,
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
    fn check_inhibited(&self, operation: &ServiceControlOperation) -> Result<(), String> {
        let source = self
            .definition(&operation.native_name)?
            .ok_or("OpenRC definition disappeared")?;
        if self.generation()? != operation.manager_generation
            || self.configuration(&operation.native_name, &source)? != operation.configuration
            || !matches!(
                operation.configuration,
                ServiceState::Registration {
                    start: ServiceStart::Disabled,
                    ..
                }
            )
        {
            return Err("OpenRC stop requires the authenticated disabled registration".into());
        }
        Ok(())
    }

    fn drain_admission(
        &self,
        operation: &ServiceControlOperation,
        deadline: std::time::Instant,
    ) -> Result<(), String> {
        self.check_inhibited(operation)?;
        let path = Path::new("/run/openrc/exclusive").join(&operation.native_name);
        let parent =
            OwnedDirectory::open(path.parent().unwrap()).map_err(|error| error.to_string())?;
        let lease = match rustix::fs::openat(
            parent.as_fd(),
            operation.native_name.as_str(),
            rustix::fs::OFlags::WRONLY | rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        ) {
            Ok(lease) => lease,
            Err(rustix::io::Errno::NOENT) => return self.check_inhibited(operation),
            Err(error) => return Err(error.to_string()),
        };
        let stat = rustix::fs::fstat(&lease).map_err(|error| error.to_string())?;
        if rustix::fs::FileType::from_raw_mode(stat.st_mode) != rustix::fs::FileType::RegularFile
            || stat.st_uid != 0
            || stat.st_mode & 0o022 != 0
        {
            return Err("OpenRC native admission lease is untrusted".into());
        }
        loop {
            match rustix::fs::flock(&lease, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
                Ok(()) => break,
                Err(rustix::io::Errno::WOULDBLOCK) => {
                    if std::time::Instant::now() >= deadline {
                        return Err(
                            "OpenRC start admission did not drain before the stop deadline".into(),
                        );
                    }
                    std::thread::park_timeout(Duration::from_millis(20));
                }
                Err(error) => return Err(error.to_string()),
            }
        }
        // OpenRC unlinks its lease after the request. The protected admission record gates later starts.
        drop(lease);
        self.check_inhibited(operation)
    }

    fn stop_native(
        &self,
        operation: &ServiceControlOperation,
        deadline: std::time::Instant,
    ) -> Result<(), String> {
        self.drain_admission(operation, deadline)?;
        let name = &operation.native_name;
        let scope = ProcessScope::observe(name)?;
        let evidence =
            serde_json::from_str::<NativeExecution>(&operation.before_execution_generation);
        if !scope.populated && !live_runtime_records(name)? && evidence.is_err() {
            return self.check_inhibited(operation);
        }
        let evidence =
            evidence.map_err(|_| "OpenRC stop lacks authenticated native execution evidence")?;
        if scope.identity() != Some(evidence.scope) {
            return Err("OpenRC process scope differs from the journaled execution".into());
        }
        let directory =
            OwnedDirectory::open(&Path::new("/sys/fs/cgroup").join(format!("openrc.{name}")))
                .map_err(|error| error.to_string())?;
        let held = directory.stat_self().map_err(|error| error.to_string())?;
        if (ScopeIdentity {
            device: held.dev,
            inode: held.ino,
        }) != evidence.scope
        {
            return Err("OpenRC process scope changed while being retained".into());
        }
        let native = evidence.supervisor.unwrap_or(evidence.process);
        if let Some(current) = NativeProcess::observe(native.pid)? {
            if current != native {
                return Err("OpenRC native process identity was reused".into());
            }
            let pid =
                rustix::process::Pid::from_raw(native.pid as i32).ok_or("invalid native PID")?;
            let process = rustix::process::pidfd_open(pid, rustix::process::PidfdFlags::NONBLOCK)
                .map_err(|error| error.to_string())?;
            if !native.unchanged()? {
                return Err("OpenRC native process changed while opening its pidfd".into());
            }
            scope
                .verify_member(native.pid)
                .map_err(|_| "OpenRC native process left its authenticated scope")?;
            self.check_inhibited(operation)?;
            rustix::process::pidfd_send_signal(&process, rustix::process::Signal::TERM)
                .or_else(|error| {
                    if error == rustix::io::Errno::SRCH {
                        Ok(())
                    } else {
                        Err(error)
                    }
                })
                .map_err(|error| error.to_string())?;
            let ServiceState::Registration {
                native_timeouts:
                    zup_core::NativeServiceTimeouts::Openrc {
                        stop_grace_seconds, ..
                    },
                ..
            } = operation.configuration
            else {
                return Err("OpenRC stop requires native retry settings".into());
            };
            let grace = (std::time::Instant::now()
                + Duration::from_secs(stop_grace_seconds.into()))
            .min(deadline);
            loop {
                let mut waiting = [rustix::event::PollFd::new(
                    &process,
                    rustix::event::PollFlags::IN,
                )];
                if rustix::event::poll(
                    &mut waiting,
                    Some(&rustix::event::Timespec {
                        tv_sec: 0,
                        tv_nsec: 0,
                    }),
                )
                .map_err(|error| error.to_string())?
                    != 0
                    || std::time::Instant::now() >= grace
                {
                    break;
                }
                std::thread::park_timeout(Duration::from_millis(20));
            }
        }
        self.check_inhibited(operation)?;
        if ProcessScope::observe(name)?.identity() != Some(evidence.scope) {
            return Err("OpenRC scope changed before descendant termination".into());
        }
        let control = rustix::fs::openat(
            directory.as_fd(),
            "cgroup.kill",
            rustix::fs::OFlags::WRONLY | rustix::fs::OFlags::CLOEXEC | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::empty(),
        )
        .map_err(|error| error.to_string())?;
        let stat = rustix::fs::fstat(&control).map_err(|error| error.to_string())?;
        if stat.st_uid != 0 || stat.st_mode & 0o022 != 0 {
            return Err("OpenRC cgroup termination control is delegated".into());
        }
        rustix::io::write(&control, b"1").map_err(|error| error.to_string())?;
        loop {
            let current = ProcessScope::observe(name)?;
            if current.identity() != Some(evidence.scope) {
                return Err("OpenRC scope changed during descendant termination".into());
            }
            if !current.populated && !live_runtime_records(name)? {
                break;
            }
            if std::time::Instant::now() >= deadline {
                return Err(
                    "OpenRC native scope did not become empty before the stop deadline".into(),
                );
            }
            std::thread::park_timeout(Duration::from_millis(20));
        }
        self.check_inhibited(operation)?;
        let run = OwnedDirectory::open(Path::new("/run")).map_err(|error| error.to_string())?;
        let pid_name = format!("{name}.pid");
        if run
            .kind_or_absent(&pid_name)
            .map_err(|error| error.to_string())?
            .is_some()
        {
            let held = run
                .stream_regular(&pid_name)
                .map_err(|error| error.to_string())?;
            if held.uid != 0
                || held.mode & 0o022 != 0
                || held.size > 256
                || read_pid(&Path::new("/run").join(&pid_name))? != Some(native.pid)
                || NativeProcess::observe(native.pid)?.is_some()
                || run
                    .stat_child(&pid_name)
                    .map_err(|error| error.to_string())?
                    .object()
                    != held.object()
            {
                return Err(
                    "OpenRC stopped pidfile no longer identifies its terminated native process"
                        .into(),
                );
            }
            run.remove_file(&pid_name)
                .map_err(|error| error.to_string())?;
            run.sync().map_err(|error| error.to_string())?;
        }
        self.check_inhibited(operation)?;
        self.command(
            "/sbin/rc-service",
            &["--nodeps", name, "zap"],
            deadline
                .checked_duration_since(std::time::Instant::now())
                .ok_or("OpenRC stop deadline expired")?,
        )?;
        self.check_inhibited(operation)
    }

    pub fn snapshot(
        &self,
        target: &zup_plan::BoundDeployment,
    ) -> Result<Vec<zup_plan::ObservedService>, String> {
        target
            .services
            .iter()
            .map(|service| {
                let name = service.name.to_string();
                let state = self.observe_registration(&name)?;
                Ok(zup_plan::ObservedService {
                    registration: if state == ObservedServiceState::Absent {
                        zup_core::ObservedServiceRegistration::Unclaimed
                    } else {
                        zup_core::ObservedServiceRegistration::SourceFile
                    },
                    key: service.key.clone(),
                    id: service.id.clone(),
                    state,
                })
            })
            .collect()
    }

    pub fn compile(
        &mut self,
        execution: &zup_plan::ExecutionPlan,
        target: Option<&zup_plan::BoundDeployment>,
        ledger: Option<&zup_plan::InstallationRecord>,
    ) -> Result<zup_plan::DeploymentEffects, String> {
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
            .retain(|removal| !matches!(removal.owned, OwnedResource::Service(_)));
        let mut input =
            crate::input::compile_files_only(&files, target).map_err(|error| error.to_string())?;
        let target_files: BTreeMap<_, _> = target
            .into_iter()
            .flat_map(|target| &target.files)
            .map(|file| (file.destination().to_string(), file.executable))
            .collect();
        let affected = execution.affected_services(ledger);
        for service in &execution.services {
            let name = service.name.clone();
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
            let owned = ledger.and_then(|ledger| ledger.current.resources.get(&service.key));
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
                    .extend(zup_plan::plan_service_controls(
                        service,
                        owned,
                        affected.contains(&service.key),
                        name.clone(),
                        self.generation()?,
                        generation,
                        zup_plan::ServiceScopeObservation::NativeManager,
                    )?);
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
                && (previous_source.is_some()
                    || self.runlevel_policy(&name)? != RunlevelPolicy::Manual
                    || self.admission(&name)?.is_some())
            {
                return Err("foreign OpenRC service or runlevel entry already exists".into());
            }
            if service.previous.configuration().as_ref()
                != self.observe_registration(&name)?.configuration().as_ref()
            {
                return Err("OpenRC registration changed after observation".into());
            }
            let source_key = zup_core::ResourceKey::File {
                id: zup_core::FileResourceId::Native {
                    id: zup_core::NativeFileId::ServiceSource {
                        id: zup_core::ServiceId::new(&service.id)
                            .map_err(|error| error.to_string())?,
                    },
                },
            };
            let source = input
                .files
                .iter()
                .find(|file| file.key == source_key)
                .ok_or("OpenRC source is missing its owned file effect")?;
            if source.destination.as_str() != Self::source(&name)?.to_string_lossy()
                || source.expected_sha256 != zup_core::hash_bytes(&bytes)
                || source.expected_size != bytes.len() as u64
                || !source.executable
            {
                return Err("OpenRC source file differs from its native declaration".into());
            }
            let mut service = service.clone();
            service.lifecycle = service.lifecycle.resolved(&service.execution);
            input
                .native_services
                .push(zup_plan::NativeServiceOperation::Configure {
                    service: Box::new(service),
                    previous: match owned {
                        Some(OwnedResource::Service(service)) => Some(Box::new(service.clone())),
                        None => None,
                        _ => return Err("OpenRC service ownership type mismatch".into()),
                    },
                    activation: zup_plan::ServiceActivation::Declared,
                });
        }
        for removal in &execution.removals {
            let OwnedResource::Service(
                owned_service @ zup_plan::OwnedService {
                    timeouts,
                    installed,
                    name,
                    privilege,
                    ..
                },
            ) = &removal.owned
            else {
                continue;
            };
            if *privilege != Privilege::System
                || !crate::openrc::valid_name(name)
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
                        requires_files: owned_service.requires_files.clone(),
                        registration: zup_core::NativeServiceRegistration::SourceFile,
                        intent: zup_plan::ServiceControlIntent::Transition,
                        key: removal.key.clone(),
                        native_name: name.clone(),
                        privilege: *privilege,
                        stage: zup_plan::ServiceControlStage::Quiesce,
                        before_configuration: installed.as_ref().clone(),
                        configuration: zup_plan::disabled_startup(installed).expect("registration"),
                        owned_configuration: installed.as_ref().clone(),
                        before: if runtime == ServiceRuntimeState::Running {
                            ServiceTerminalState::Running
                        } else {
                            ServiceTerminalState::Stopped
                        },
                        after: ServiceTerminalState::Stopped,
                        manager_generation: self.generation()?,
                        before_execution_generation: generation,
                        process_scope: zup_plan::ServiceScopeObservation::NativeManager,
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
            input.retired_keys.push(removal.key.clone());
            input
                .native_services
                .push(zup_plan::NativeServiceOperation::Remove {
                    key: removal.key.clone(),
                    service: Box::new(owned_service.clone()),
                    activation: zup_plan::ServiceActivation::Declared,
                });
        }
        zup_plan::bind_service_dependencies(&mut input.service_controls, execution, ledger)?;
        crate::service_ops::bind_startup_inhibition(&mut input);
        input
            .bind_generations(target)
            .map_err(|error| error.to_string())?;
        Ok(input)
    }

    pub fn connect(roots: crate::locations::MachineRoots) -> Result<Self, String> {
        crate::service_host::validate_init_view()?;
        let _process = rustix::process::pidfd_open(
            rustix::process::getpid(),
            rustix::process::PidfdFlags::NONBLOCK,
        )
        .map_err(|error| format!("OpenRC deployment requires kernel pidfds: {error}"))?;
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
            return Err("OpenRC native supervisor changed after validation".into());
        }
        Ok(format!(
            "openrc:{}:{init}:{}:{}",
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
        let override_path = Path::new("/etc/init.d").join(format!("{name}.options"));
        match std::fs::symlink_metadata(&override_path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            _ => {
                return Err(format!(
                    "foreign OpenRC configuration override at {}",
                    override_path.display()
                ));
            }
        }
        self.check_configuration_files(name)?;
        let directory =
            OwnedDirectory::open(path.parent().unwrap()).map_err(|error| error.to_string())?;
        let source = read_native_file(&directory, name, crate::service_ops::MAX_UNIT_BYTES)?;
        if let Some((_, mode)) = &source
            && mode & 0o111 == 0
        {
            return Err(
                "OpenRC definition requires root ownership and safe executable permissions".into(),
            );
        }
        Ok(source.map(|(bytes, _)| bytes))
    }

    fn check_configuration_files(&self, name: &str) -> Result<(), String> {
        trusted_directory(Path::new("/etc/conf.d"))?;
        let directory =
            OwnedDirectory::open(Path::new("/etc/conf.d")).map_err(|error| error.to_string())?;
        let base = name.split('.').next().expect("service name");
        let levels = OwnedDirectory::open(Path::new("/etc/runlevels"))
            .map_err(|error| error.to_string())?
            .child_names()
            .map_err(|error| error.to_string())?;
        for candidate in directory.child_names().map_err(|error| error.to_string())? {
            if candidate == name
                || candidate.starts_with(&format!("{name}."))
                || base != name
                    && (candidate == base
                        || levels
                            .iter()
                            .any(|level| candidate == format!("{base}.{level}")))
            {
                let Some((bytes, _)) =
                    read_native_file(&directory, &candidate, crate::service_ops::MAX_UNIT_BYTES)?
                else {
                    return Err("OpenRC configuration disappeared during inspection".into());
                };
                let text = std::str::from_utf8(&bytes).map_err(|error| error.to_string())?;
                if text
                    .lines()
                    .any(|line| !line.trim().is_empty() && !line.trim_start().starts_with('#'))
                {
                    return Err(format!(
                        "OpenRC configuration {candidate} overrides the owned service definition"
                    ));
                }
            }
        }
        Ok(())
    }

    fn admission_directory(&self, create: bool) -> Result<Option<OwnedDirectory>, String> {
        trusted_directory(Path::new("/etc"))?;
        let mut directory =
            OwnedDirectory::open(Path::new("/etc")).map_err(|error| error.to_string())?;
        for name in Path::new(crate::openrc::ADMISSION_DIRECTORY)
            .strip_prefix("/etc")
            .expect("admission state belongs under /etc")
            .iter()
            .map(|name| name.to_str().expect("constant admission path"))
        {
            directory = match directory
                .kind_or_absent(name)
                .map_err(|error| error.to_string())?
            {
                None if !create => return Ok(None),
                None => directory
                    .create_exclusive_directory(name, crate::fs::STATE_DIRECTORY_MODE)
                    .map_err(|error| error.to_string())?,
                Some(_) => directory
                    .open_child(name)
                    .map_err(|error| error.to_string())?,
            };
            let state = directory.stat_self().map_err(|error| error.to_string())?;
            if state.uid != 0 || state.mode != 0o700 {
                return Err("OpenRC admission directory is not private installer authority".into());
            }
        }
        Ok(Some(directory))
    }

    fn admission(&self, name: &str) -> Result<Option<Admission>, String> {
        Self::source(name)?;
        let Some(directory) = self.admission_directory(false)? else {
            return Ok(None);
        };
        let Some((bytes, mode)) = read_native_file(&directory, name, 32)? else {
            return Ok(None);
        };
        if mode != 0o600 {
            return Err("OpenRC admission record is not private".into());
        }
        match bytes.as_slice() {
            b"allowed\n" => Ok(Some(Admission::Allowed)),
            b"inhibited\n" => Ok(Some(Admission::Inhibited)),
            _ => Err("OpenRC admission record is not recognized".into()),
        }
    }

    fn set_admission(&self, name: &str, admission: Admission) -> Result<(), String> {
        let previous = self.admission(name)?;
        let directory = self
            .admission_directory(true)?
            .ok_or("OpenRC admission directory disappeared")?;
        match previous {
            Some(before) if before == admission => return Ok(()),
            Some(_) => directory.write_durable(name, admission.bytes(), crate::fs::STATE_FILE_MODE),
            None => directory.create_durable_exclusive(
                name,
                admission.bytes(),
                crate::fs::STATE_FILE_MODE,
            ),
        }
        .map_err(|error| error.to_string())
    }

    fn configuration(&self, name: &str, source: &[u8]) -> Result<ServiceState, String> {
        let mut state = crate::openrc::parse(name, source)?;
        if let ServiceState::Registration { start, .. } = &mut state {
            let policy = self.runlevel_policy(name)?;
            *start = match self
                .admission(name)?
                .ok_or("OpenRC registration lacks protected admission evidence")?
            {
                Admission::Inhibited => ServiceStart::Disabled,
                Admission::Allowed if policy == RunlevelPolicy::Automatic => {
                    ServiceStart::Automatic
                }
                Admission::Allowed => ServiceStart::Manual,
            };
        }
        Ok(state)
    }

    fn set_startup(&self, name: &str, configuration: &ServiceState) -> Result<(), String> {
        let start = match configuration {
            ServiceState::Registration { start, .. } => *start,
            ServiceState::Absent => ServiceStart::Manual,
        };
        self.admission(name)?;
        if start == ServiceStart::Disabled {
            self.set_admission(name, Admission::Inhibited)?;
            zup_transaction::fault_injection::boundary(
                zup_transaction::fault_injection::Boundary::NativeAdmissionInhibited,
                name,
            );
        }
        self.set_runlevel_policy(
            name,
            if start == ServiceStart::Automatic {
                RunlevelPolicy::Automatic
            } else {
                RunlevelPolicy::Manual
            },
        )?;
        if configuration == &ServiceState::Absent {
            if self.admission(name)?.is_some()
                && let Some(directory) = self.admission_directory(false)?
            {
                directory
                    .remove_file(name)
                    .map_err(|error| error.to_string())?;
                directory.sync().map_err(|error| error.to_string())?;
            }
        } else if start != ServiceStart::Disabled {
            self.set_admission(name, Admission::Allowed)?;
        }
        Ok(())
    }

    fn runlevel_policy(&self, name: &str) -> Result<RunlevelPolicy, String> {
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
        Ok(if automatic {
            RunlevelPolicy::Automatic
        } else {
            RunlevelPolicy::Manual
        })
    }

    pub fn observe_registration(&self, name: &str) -> Result<ObservedServiceState, String> {
        let Some(bytes) = self.definition(name)? else {
            if self.runlevel_policy(name)? != RunlevelPolicy::Manual
                || self.admission(name)?.is_some()
            {
                return Err("orphaned OpenRC activation resource".into());
            }
            ensure_absent_runtime(name)?;
            return Ok(ObservedServiceState::Absent);
        };
        let state = match self.configuration(name, &bytes) {
            Ok(state) => state,
            Err(_) => return Ok(ObservedServiceState::InvalidService),
        };
        let ServiceState::Registration {
            state_directories,
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
        let runtime_state = Some(self.runtime(name, &command)?.0);
        Ok(ObservedServiceState::Service {
            state_directories,
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
        command: &zup_core::CommandSpec,
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
        command: &zup_core::CommandSpec,
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
        let source = self
            .definition(name)?
            .ok_or("OpenRC definition disappeared")?;
        let ServiceState::Registration {
            execution, failure, ..
        } = crate::openrc::parse(name, &source)?
        else {
            unreachable!()
        };
        let pidfile = Path::new("/run").join(format!("{name}.pid"));
        let Some(native_pid) = read_pid(&pidfile)? else {
            if scope.populated || live_runtime_records(name)? {
                return Err("OpenRC process scope lacks native launch evidence".into());
            }
            return Ok((ServiceRuntimeState::Stopped, "stopped".into()));
        };
        let Some(native) = NativeProcess::observe(native_pid)? else {
            if scope.populated || live_runtime_records(name)? {
                return Err(
                    "OpenRC native launcher exited while its process scope remains live".into(),
                );
            }
            return Ok((ServiceRuntimeState::Stopped, "stopped".into()));
        };
        let supervisor = match failure {
            zup_core::ServiceFailurePolicy::None => None,
            zup_core::ServiceFailurePolicy::Openrc { .. } => Some(native),
            _ => return Err("OpenRC native launch policy is invalid".into()),
        };
        let process = if let Some(supervisor) = supervisor {
            let live =
                process_snapshot(|| std::fs::metadata(format!("/proc/{}/exe", supervisor.pid)))?;
            let expected = std::fs::metadata(
                crate::prerequisites::trusted_executable("/sbin/supervise-daemon")
                    .map_err(|error| error.to_string())?,
            )
            .map_err(|error| error.to_string())?;
            if (live.dev(), live.ino()) != (expected.dev(), expected.ino()) {
                return Err("OpenRC supervisor executable identity changed".into());
            }
            scope.verify_member(supervisor.pid)?;
            let Some(pid) = read_pid(
                &Path::new("/run/openrc/options")
                    .join(name)
                    .join("child_pid"),
            )?
            else {
                return Ok((ServiceRuntimeState::StartPending, "pending".into()));
            };
            let Some(child) = NativeProcess::observe(pid)? else {
                return Ok((ServiceRuntimeState::StartPending, "pending".into()));
            };
            let stat = process_snapshot(|| std::fs::read_to_string(format!("/proc/{pid}/stat")))?;
            if stat
                .rsplit_once(") ")
                .and_then(|(_, fields)| fields.split_ascii_whitespace().nth(1))
                .and_then(|parent| parent.parse::<u32>().ok())
                != Some(supervisor.pid)
            {
                return Err("OpenRC child no longer belongs to its native supervisor".into());
            }
            child
        } else {
            native
        };
        let pid = process.pid;
        let executable = crate::lowering::to_host_path(&command.executable)
            .map_err(|error| error.to_string())?;
        let live = process_snapshot(|| std::fs::metadata(format!("/proc/{pid}/exe")))?;
        let expected = std::fs::symlink_metadata(&executable).map_err(|error| error.to_string())?;
        if (live.dev(), live.ino()) != (expected.dev(), expected.ino()) {
            return Err("OpenRC process identity differs from the authorized executable".into());
        }
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
        if let Some(supervisor) = supervisor {
            scope.verify_member(supervisor.pid)?;
        }
        scope.verify_member(pid)?;
        if ProcessScope::observe(name)? != scope {
            return Err(RuntimeSnapshotError::Changed);
        }
        let final_image = process_snapshot(|| std::fs::metadata(format!("/proc/{pid}/exe")))?;
        if !process.unchanged()?
            || !native.unchanged()?
            || read_pid(&pidfile)? != Some(native_pid)
            || supervisor.is_some()
                && read_pid(
                    &Path::new("/run/openrc/options")
                        .join(name)
                        .join("child_pid"),
                )? != Some(pid)
            || self.definition(name)?.as_deref() != Some(source.as_slice())
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
            serde_json::to_string(&NativeExecution {
                scope: scope
                    .identity()
                    .ok_or("OpenRC runtime lacks its native process scope")?,
                supervisor,
                process,
            })
            .map_err(|error| error.to_string())?,
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

    fn set_runlevel_policy(&self, name: &str, desired: RunlevelPolicy) -> Result<(), String> {
        let path = Path::new("/etc/runlevels/default");
        trusted_directory(path)?;
        let directory = OwnedDirectory::open(path).map_err(|error| error.to_string())?;
        let previous = self.runlevel_policy(name)?;
        if previous != desired {
            let action = match desired {
                RunlevelPolicy::Automatic => "add",
                RunlevelPolicy::Manual => "del",
            };
            self.command(
                "/sbin/rc-update",
                &[action, name, "default"],
                Duration::from_secs(30),
            )?;
        }
        if self.runlevel_policy(name)? != desired {
            return Err("OpenRC runlevel update failed verification".into());
        }
        if OwnedDirectory::open(path)
            .and_then(|current| current.stat_self())
            .map_err(|error| error.to_string())?
            .object()
            != directory
                .stat_self()
                .map_err(|error| error.to_string())?
                .object()
        {
            return Err("OpenRC runlevel directory changed during publication".into());
        }
        directory.sync().map_err(|error| error.to_string())?;
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

    pub fn prepare_registration(
        &self,
        operation: &zup_plan::NativeServiceOperation,
    ) -> Result<(), ExecError> {
        let name = operation.native_name().to_owned();
        if !crate::openrc::valid_name(&name) || operation.privilege() != Privilege::System {
            return Err(refused(
                &name,
                "OpenRC native registration authority mismatch",
            ));
        }
        for state in operation
            .configurations()
            .0
            .iter()
            .chain(std::iter::once(&operation.configurations().1))
        {
            if let ServiceState::Registration { execution, .. } = state
                && !matches!(execution, zup_core::ServiceExecution::OpenrcUser { .. })
            {
                return Err(refused(&name, "OpenRC execution identity required"));
            }
        }
        self.definition(&name)
            .map_err(|reason| refused(&name, reason))?;
        self.generation().map_err(|reason| refused(&name, reason))?;
        Ok(())
    }

    fn registration_source(&self, name: &str, state: &ServiceState) -> Result<(), String> {
        let expected = match state {
            ServiceState::Absent => None,
            _ => Some(crate::openrc::render(name, state)?),
        };
        if self.definition(name)? != expected {
            return Err("OpenRC source differs from the owned file effect".into());
        }
        Ok(())
    }

    fn configure_registration(
        &self,
        operation: &zup_plan::NativeServiceOperation,
        desired: &ServiceState,
    ) -> Result<(), String> {
        let name = operation.native_name().to_owned();
        self.registration_source(&name, desired)?;
        let generation = self.generation()?;
        let preserving = operation.configurations().0.as_ref().is_some_and(|before| {
            *desired != ServiceState::Absent && desired.update_from(before).preserves_runtime()
        });
        let runtime = if let ServiceState::Registration { command, .. } = desired {
            crate::service_ops::validate_executable_live(command, &self.roots, 0)
                .map_err(|error| error.to_string())?;
            let path = crate::lowering::to_host_path(&command.executable)
                .map_err(|error| error.to_string())?;
            let (directory, child) =
                crate::fs::open_parent(&path).map_err(|error| error.to_string())?;
            crate::openrc::validate_executable(
                &directory
                    .read_regular(&child)
                    .map_err(|error| error.to_string())?,
                command.executable.target(),
            )?;
            let current = self.runtime(&name, command)?;
            if !preserving && current.0 != ServiceRuntimeState::Stopped {
                return Err(
                    "OpenRC registration changes require a stopped native process scope".into(),
                );
            }
            Some(current)
        } else {
            ensure_absent_runtime(&name)?;
            None
        };
        self.set_startup(&name, desired)?;
        if self.generation()? != generation
            || self.observe_registration(&name)?.configuration().as_ref() != Some(desired)
        {
            return Err("OpenRC registration failed native readback".into());
        }
        if let Some(expected) = runtime
            && let ServiceState::Registration { command, .. } = desired
            && self.runtime(&name, command)? != expected
        {
            return Err("OpenRC runtime changed during native registration".into());
        }
        Ok(())
    }

    fn registration_receipt(
        operation: &zup_plan::NativeServiceOperation,
    ) -> zup_transaction::OperationReceipt {
        let configuration = operation.configurations().1;
        zup_transaction::OperationReceipt::NativeService(Box::new(
            zup_transaction::NativeServiceReceipt {
                intent: operation.fingerprint(),
                registration: if configuration == ServiceState::Absent {
                    zup_core::ObservedServiceRegistration::Unclaimed
                } else {
                    zup_core::ObservedServiceRegistration::SourceFile
                },
                configuration,
            },
        ))
    }

    pub fn apply_registration(
        &self,
        operation: &zup_plan::NativeServiceOperation,
    ) -> Result<zup_transaction::OperationReceipt, ExecError> {
        let name = operation.native_name().to_owned();
        self.configure_registration(operation, &operation.configurations().1)
            .map_err(|reason| refused(&name, reason))?;
        Ok(Self::registration_receipt(operation))
    }

    pub fn verify_registration(
        &self,
        operation: &zup_plan::NativeServiceOperation,
        receipt: &zup_transaction::OperationReceipt,
    ) -> Result<(), ExecError> {
        let name = operation.native_name().to_owned();
        if !matches!(receipt, zup_transaction::OperationReceipt::NativeService(receipt) if receipt.matches(operation))
            || self
                .observe_registration(&name)
                .map_err(|reason| refused(&name, reason))?
                .configuration()
                .as_ref()
                != Some(&operation.configurations().1)
        {
            return Err(refused(
                &name,
                "OpenRC native receipt differs from observed registration",
            ));
        }
        Ok(())
    }

    pub fn rollback_registration(
        &self,
        operation: &zup_plan::NativeServiceOperation,
        receipt: &zup_transaction::OperationReceipt,
    ) -> Result<(), ExecError> {
        let name = operation.native_name().to_owned();
        if !matches!(receipt, zup_transaction::OperationReceipt::NativeService(receipt) if receipt.matches(operation))
            && !matches!(receipt, zup_transaction::OperationReceipt::NativeServiceCompensation { intent } if *intent == operation.fingerprint())
        {
            return Err(refused(
                &name,
                "OpenRC compensation receipt differs from intent",
            ));
        }
        self.configure_registration(
            operation,
            &operation.configurations().0.unwrap_or(ServiceState::Absent),
        )
        .map_err(|reason| refused(&name, reason))
    }

    pub fn reconcile_registration(
        &self,
        operation: &zup_plan::NativeServiceOperation,
    ) -> Result<zup_transaction::ReconcileResult, ExecError> {
        let receipt = Self::registration_receipt(operation);
        if self.verify_registration(operation, &receipt).is_ok() {
            return Ok(zup_transaction::ReconcileResult::AppliedWithReceipt(
                receipt,
            ));
        }
        let name = operation.native_name().to_owned();
        let desired = operation.configurations().1;
        if self.registration_source(&name, &desired).is_ok()
            && (if desired == ServiceState::Absent {
                ensure_absent_runtime(&name)
            } else {
                self.ensure_stopped(&name, &desired)
            })
            .is_ok()
        {
            Ok(zup_transaction::ReconcileResult::Incomplete)
        } else {
            Ok(zup_transaction::ReconcileResult::Ambiguous)
        }
    }
}

impl NativeServiceManager for Openrc {
    fn prepare_recovery(
        &mut self,
        operation: &ServiceControlOperation,
        deadline: std::time::Instant,
    ) -> Result<(), String> {
        operation.validate()?;
        if operation.intent != zup_plan::ServiceControlIntent::Transition {
            return Ok(());
        }
        if self.generation()? != operation.manager_generation {
            return Ok(());
        }
        let source = self
            .definition(&operation.native_name)?
            .ok_or("OpenRC definition disappeared")?;
        if source != crate::openrc::render(&operation.native_name, &operation.configuration)? {
            return Err("OpenRC recovery registration or manager changed".into());
        }
        if operation.stage == zup_plan::ServiceControlStage::Restore {
            let Some(inhibited) = zup_plan::disabled_startup(&operation.configuration) else {
                return Err("OpenRC activation recovery requires a registration".into());
            };
            if operation.before != ServiceTerminalState::Stopped
                || operation.before_configuration != inhibited
            {
                return Err(
                    "OpenRC activation recovery lacks its closed admission boundary".into(),
                );
            }
            self.set_startup(&operation.native_name, &inhibited)?;
            let mut stop = operation.clone();
            stop.stage = zup_plan::ServiceControlStage::Quiesce;
            stop.before_configuration = inhibited.clone();
            stop.configuration = inhibited;
            stop.after = ServiceTerminalState::Stopped;
            self.drain_admission(&stop, deadline)?;
            let ServiceState::Registration { command, .. } = &stop.configuration else {
                unreachable!()
            };
            let (runtime, generation) = self.runtime(&operation.native_name, command)?;
            match runtime {
                ServiceRuntimeState::Stopped => return Ok(()),
                ServiceRuntimeState::Running => {
                    stop.before = ServiceTerminalState::Running;
                    stop.before_execution_generation = generation;
                    return self.stop_native(&stop, deadline);
                }
                _ => {
                    return Err(
                        "OpenRC activation request did not settle under closed admission".into(),
                    );
                }
            }
        }
        if self.admission(&operation.native_name)? != Some(Admission::Inhibited) {
            let actual = self.configuration(&operation.native_name, &source)?;
            if actual == operation.before_configuration && actual != operation.configuration {
                return Ok(());
            }
            return Err("OpenRC recovery lacks its persisted activation inhibition".into());
        }
        self.set_runlevel_policy(&operation.native_name, RunlevelPolicy::Manual)?;
        self.stop_native(operation, deadline)
    }

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
            || !crate::openrc::valid_name(&operation.native_name)
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
            registration: zup_core::ObservedServiceRegistration::SourceFile,
            configuration,
            runtime,
            execution_generation,
            process_scope: zup_plan::ServiceScopeObservation::NativeManager,
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
            self.set_startup(&operation.native_name, &operation.configuration)?;
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
        if !state.matches(dispatch.runtime) || state == ServiceTerminalState::Stopped {
            match state {
                ServiceTerminalState::Running => {
                    self.command(
                        "/sbin/rc-service",
                        &["--nodeps", &operation.native_name, "start"],
                        remaining()?,
                    )?;
                }
                ServiceTerminalState::Stopped => {
                    self.stop_native(operation, deadline)?;
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

fn read_native_file(
    directory: &OwnedDirectory,
    name: &str,
    limit: usize,
) -> Result<Option<(Vec<u8>, u32)>, String> {
    match directory
        .kind_or_absent(name)
        .map_err(|error| error.to_string())?
    {
        None => return Ok(None),
        Some(crate::fs::EntryKind::Regular) => {}
        Some(_) => {
            return Err(format!(
                "OpenRC native file {name} is not a regular owned file"
            ));
        }
    }
    let mut file = directory
        .open_regular_read(name)
        .map_err(|error| error.to_string())?;
    let before = file.metadata().map_err(|error| error.to_string())?;
    if before.uid() != 0
        || before.mode() & 0o022 != 0
        || before.nlink() != 1
        || before.len() > limit as u64
    {
        return Err(format!(
            "OpenRC native file {name} has unsafe ownership, access or size"
        ));
    }
    use std::io::Read;
    let mut bytes = Vec::new();
    file.by_ref()
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    let current = directory
        .stat_child(name)
        .map_err(|error| error.to_string())?;
    let identity = |metadata: &std::fs::Metadata| {
        (
            metadata.dev(),
            metadata.ino(),
            metadata.uid(),
            metadata.gid(),
            metadata.mode(),
            metadata.nlink(),
            metadata.len(),
            metadata.mtime(),
            metadata.mtime_nsec(),
            metadata.ctime(),
            metadata.ctime_nsec(),
        )
    };
    if bytes.len() as u64 != before.len()
        || identity(&before) != identity(&file.metadata().map_err(|error| error.to_string())?)
        || (current.dev, current.ino, current.uid, current.mode)
            != (
                before.dev(),
                before.ino(),
                before.uid(),
                before.mode() & 0o7777,
            )
    {
        return Err(format!(
            "OpenRC native file {name} changed during inspection"
        ));
    }
    Ok(Some((bytes, before.mode() & 0o7777)))
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
    device: u64,
    populated: bool,
}

impl ProcessScope {
    fn identity(&self) -> Option<ScopeIdentity> {
        self.inode.map(|inode| ScopeIdentity {
            device: self.device,
            inode,
        })
    }

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
        let mut device = 0;
        let inode = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => {
                trusted_directory(&path)?;
                for control in ["cgroup.procs", "cgroup.threads"] {
                    trusted_scope_control(&path.join(control))?;
                }
                trusted_scope_control(&path.join("cgroup.kill"))?;
                device = metadata.dev();
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
            device,
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
    match std::fs::symlink_metadata(path.parent().ok_or("PID state lacks a parent")?) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.to_string()),
        Ok(_) => {}
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    include!("openrc_native/admission_tests.rs");
    use std::os::unix::fs::PermissionsExt;
    use zup_core::{
        CommandSpec, ResourceKey, ServiceExecution, ServiceFailurePolicy, ServiceId,
        ServiceTimeouts, Target, TargetPath,
    };
    use zup_plan::{
        NativeServiceOperation, ServiceActivation, ServiceControlIntent, ServiceControlStage,
    };

    fn control(
        manager: &Openrc,
        key: &ResourceKey,
        name: &str,
        before: &ServiceState,
        after: &ServiceState,
        running: bool,
        start: bool,
    ) -> ServiceControlOperation {
        let ServiceState::Registration { command, .. } = before else {
            unreachable!()
        };
        ServiceControlOperation {
            requires_files: Vec::new(),
            registration: zup_core::NativeServiceRegistration::SourceFile,
            intent: ServiceControlIntent::Transition,
            key: key.clone(),
            native_name: name.into(),
            privilege: Privilege::System,
            stage: if start {
                ServiceControlStage::Restore
            } else {
                ServiceControlStage::Quiesce
            },
            before_configuration: before.clone(),
            configuration: after.clone(),
            owned_configuration: before.clone(),
            before: if running {
                ServiceTerminalState::Running
            } else {
                ServiceTerminalState::Stopped
            },
            after: if start {
                ServiceTerminalState::Running
            } else {
                ServiceTerminalState::Stopped
            },
            manager_generation: manager.generation().unwrap(),
            before_execution_generation: manager.runtime(name, command).unwrap().1,
            process_scope: zup_plan::ServiceScopeObservation::NativeManager,
            timeouts: ServiceTimeouts::Openrc {
                start_timeout_seconds: 10,
                stop_grace_seconds: 1,
                kill_wait_seconds: 2,
                settle_seconds: 2,
            },
            dependencies: vec![],
        }
    }

    fn declaration(
        key: &ResourceKey,
        name: &str,
        state: &ServiceState,
        previous: Option<zup_plan::OwnedService>,
    ) -> NativeServiceOperation {
        let ResourceKey::Service { id } = key else {
            unreachable!()
        };
        let ServiceState::Registration {
            execution,
            failure,
            command,
            start,
            readiness,
            display_name,
            ..
        } = state
        else {
            unreachable!()
        };
        NativeServiceOperation::Configure {
            service: Box::new(zup_plan::ServiceOperation {
                state_directories: Vec::new(),
                registration: zup_core::NativeServiceRegistration::SourceFile,
                execution: execution.clone(),
                failure: *failure,
                key: key.clone(),
                kind: if previous.is_some() {
                    zup_plan::IntegrationChangeKind::UpdateOwned
                } else {
                    zup_plan::IntegrationChangeKind::Create
                },
                id: id.to_string(),
                name: name.into(),
                display_name: display_name.clone(),
                command: command.as_ref().clone(),
                start: *start,
                lifecycle: zup_core::ServiceLifecycle {
                    timeouts: Some(ServiceTimeouts::Openrc {
                        start_timeout_seconds: 10,
                        stop_grace_seconds: 1,
                        kill_wait_seconds: 2,
                        settle_seconds: 2,
                    }),
                    ..Default::default()
                },
                readiness: *readiness,
                requires_files: vec![],
                requires_services: vec![],
                dependency_names: vec![],
                privilege: Privilege::System,
                previous: ObservedServiceState::Absent,
                conflict: None,
            }),
            previous: previous.map(Box::new),
            activation: ServiceActivation::Declared,
        }
    }

    #[rstest::rstest]
    #[case::direct(false)]
    #[case::supervised(true)]
    #[ignore = "requires a disposable stock OpenRC cgroup-v2 VM as root"]
    fn stock_native_activation_and_interrupted_shutdown(#[case] supervised: bool) {
        assert!(std::env::var_os("ZUP_TEST_STOCK_OPENRC").is_some());
        assert_eq!(rustix::process::geteuid().as_raw(), 0);
        let tag = uuid::Uuid::now_v7().simple().to_string();
        let id = ServiceId::new(format!("probe-{tag}")).unwrap();
        let key = ResourceKey::Service { id: id.clone() };
        let name = format!("zup-openrc-fixture-{tag}");
        let root = Path::new("/opt").join(format!("zup-openrc-{tag}"));
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        for generation in ["a", "b"] {
            let directory = root.join(generation);
            std::fs::create_dir(&directory).unwrap();
            std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o755)).unwrap();
            std::fs::copy("/bin/busybox", directory.join("sleep")).unwrap();
            std::fs::set_permissions(
                directory.join("sleep"),
                std::fs::Permissions::from_mode(0o755),
            )
            .unwrap();
        }
        let target = Target::host().unwrap();
        let state = |generation: &str| ServiceState::Registration {
            state_directories: Vec::new(),
            native_timeouts: zup_core::NativeServiceTimeouts::Openrc {
                stop_grace_seconds: 1,
                kill_wait_seconds: 2,
            },
            execution: ServiceExecution::OpenrcUser {
                user: "nobody".into(),
                group: Some("nobody".into()),
            },
            failure: if supervised {
                ServiceFailurePolicy::Openrc {
                    delay_seconds: 1,
                    max_respawns: 3,
                    respawn_period_seconds: 60,
                }
            } else {
                ServiceFailurePolicy::None
            },
            dependencies: vec![],
            display_name: "Stock native activation".into(),
            command: Box::new(CommandSpec::new(
                TargetPath::new(
                    target,
                    root.join(generation).join("sleep").to_string_lossy(),
                )
                .unwrap(),
                vec!["600".into()],
            )),
            start: ServiceStart::Manual,
            readiness: zup_core::ServiceReadiness::Manager,
        };
        let mut manager = Openrc::connect(crate::locations::MachineRoots::production()).unwrap();
        let source = OwnedDirectory::open(Path::new("/etc/init.d")).unwrap();
        let a = state("a");
        source
            .create_durable_exclusive(
                &name,
                &crate::openrc::render(&name, &a).unwrap(),
                rustix::fs::Mode::from_bits_truncate(0o755),
            )
            .unwrap();
        let initial = declaration(&key, &name, &a, None);
        let receipt = manager.apply_registration(&initial).unwrap();
        manager.verify_registration(&initial, &receipt).unwrap();
        let start = control(&manager, &key, &name, &a, &a, false, true);
        zup_transaction::apply_service_control(&mut manager, &start).unwrap();
        let disabled_a = zup_plan::disabled_startup(&a).unwrap();
        let overrides = OwnedDirectory::open(Path::new("/etc/conf.d")).unwrap();
        let override_name = format!("{name}.default");
        overrides
            .create_durable_exclusive(
                &override_name,
                b"",
                rustix::fs::Mode::from_bits_truncate(0o644),
            )
            .unwrap();
        let stop = control(&manager, &key, &name, &a, &disabled_a, true, false);
        if supervised {
            manager.set_startup(&name, &disabled_a).unwrap();
            let evidence: NativeExecution =
                serde_json::from_str(&stop.before_execution_generation).unwrap();
            let native = evidence.supervisor.unwrap();
            let pid = rustix::process::Pid::from_raw(native.pid as i32).unwrap();
            let held =
                rustix::process::pidfd_open(pid, rustix::process::PidfdFlags::empty()).unwrap();
            assert!(native.unchanged().unwrap());
            rustix::process::pidfd_send_signal(&held, rustix::process::Signal::KILL).unwrap();
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while native.unchanged().unwrap() && std::time::Instant::now() < deadline {
                std::thread::park_timeout(Duration::from_millis(10));
            }
            assert!(ProcessScope::observe(&name).unwrap().populated);
            assert!(matches!(
                zup_transaction::reconcile_service_control(&mut manager, &stop, None).unwrap(),
                zup_transaction::ReconcileResult::AppliedWithReceipt(_)
            ));
        } else {
            zup_transaction::apply_service_control(&mut manager, &stop).unwrap();
        }
        assert!(!ProcessScope::observe(&name).unwrap().populated);
        assert!(
            manager
                .command(
                    "/sbin/rc-service",
                    &["--nodeps", &name, "start"],
                    Duration::from_secs(5)
                )
                .is_err()
        );
        let b = state("b");
        source
            .write_durable(
                &name,
                &crate::openrc::render(&name, &b).unwrap(),
                rustix::fs::Mode::from_bits_truncate(0o755),
            )
            .unwrap();
        let mut next = declaration(&key, &name, &b, Some(initial.contract()));
        let NativeServiceOperation::Configure { activation, .. } = &mut next else {
            unreachable!()
        };
        *activation = ServiceActivation::Inhibited;
        manager.apply_registration(&next).unwrap();
        let disabled_b = zup_plan::disabled_startup(&b).unwrap();
        let restore = control(&manager, &key, &name, &disabled_b, &b, false, true);
        zup_transaction::apply_service_control(&mut manager, &restore).unwrap();
        assert!(matches!(
            zup_transaction::reconcile_service_control(&mut manager, &restore, None).unwrap(),
            zup_transaction::ReconcileResult::AppliedWithReceipt(zup_transaction::OperationReceipt::ServiceControl(receipt))
                if receipt.completion == zup_transaction::ServiceControlCompletion::CompensationOnly
        ));
        assert!(!ProcessScope::observe(&name).unwrap().populated);
        zup_transaction::apply_service_control(&mut manager, &restore).unwrap();
        manager
            .command(
                "/sbin/rc-service",
                &[&name, "stop"],
                Duration::from_secs(10),
            )
            .unwrap();
        assert!(!ProcessScope::observe(&name).unwrap().populated);
        let restart = control(&manager, &key, &name, &b, &b, false, true);
        zup_transaction::apply_service_control(&mut manager, &restart).unwrap();
        let stop = control(&manager, &key, &name, &b, &disabled_b, true, false);
        zup_transaction::apply_service_control(&mut manager, &stop).unwrap();
        source.remove_file(&name).unwrap();
        source.sync().unwrap();
        let remove = NativeServiceOperation::Remove {
            key,
            service: Box::new(next.contract()),
            activation: ServiceActivation::Inhibited,
        };
        manager.apply_registration(&remove).unwrap();
        assert_eq!(
            manager.observe_registration(&name).unwrap(),
            ObservedServiceState::Absent
        );
        std::fs::remove_dir_all(&root).unwrap();
        overrides.remove_file(&override_name).unwrap();
    }
}
