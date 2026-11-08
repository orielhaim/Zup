use crate::error::IpcError;
use std::time::Duration;

pub const DBUS_TIMEOUT: Duration = Duration::from_secs(30);

fn unknown_property(error: &zbus_systemd::zbus::Error) -> bool {
    match error {
        zbus_systemd::zbus::Error::MethodError(name, _, _) => {
            name.as_str() == "org.freedesktop.DBus.Error.UnknownProperty"
        }
        zbus_systemd::zbus::Error::FDO(error) => matches!(
            error.as_ref(),
            zbus_systemd::zbus::fdo::Error::UnknownProperty(_)
        ),
        _ => false,
    }
}

async fn verify_running_credentials(
    service: &zbus_systemd::systemd1::ServiceProxy<'_>,
    unit: &str,
    uid: u32,
    gid: u32,
) -> Result<(), IpcError> {
    let refused = |reason| IpcError::SystemdRefused {
        unit: unit.into(),
        reason,
    };
    let pid = service
        .main_pid()
        .await
        .map_err(|error| refused(error.to_string()))?;
    let status = std::fs::read_to_string(format!("/proc/{pid}/status"))
        .map_err(|error| refused(format!("process credentials are unavailable: {error}")))?;
    for (field, expected) in [("Uid:", uid), ("Gid:", gid)] {
        if !status
            .lines()
            .find_map(|line| line.strip_prefix(field))
            .is_some_and(|ids| {
                let ids = ids.split_ascii_whitespace().collect::<Vec<_>>();
                ids.len() == 4 && ids.iter().all(|id| id.parse::<u32>() == Ok(expected))
            })
        {
            return Err(refused(
                "process credentials differ from the native execution identity".into(),
            ));
        }
    }
    if status
        .lines()
        .find_map(|line| line.strip_prefix("NoNewPrivs:"))
        .map(str::trim)
        != Some("1")
    {
        return Err(refused("running process lacks NoNewPrivileges".into()));
    }
    if service
        .main_pid()
        .await
        .map_err(|error| refused(error.to_string()))?
        != pid
    {
        return Err(refused(
            "main process changed during credential verification".into(),
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UnitChange {
    pub kind: String,

    pub source: String,

    pub destination: String,
}

impl UnitChange {
    pub fn bound(kind: &str, source: &str, destination: &str) -> Result<Self, IpcError> {
        for value in [kind, source, destination] {
            if value.len() > 1024 {
                return Err(IpcError::SystemdAmbiguous {
                    unit: source.to_owned(),
                    reason: "a systemd change description exceeds its bound".into(),
                });
            }
            if value.bytes().any(|b| b == 0 || b == b'\n' || b == b'\r') {
                return Err(IpcError::SystemdAmbiguous {
                    unit: source.to_owned(),
                    reason: "a systemd change description holds control bytes".into(),
                });
            }
        }
        Ok(Self {
            kind: kind.to_owned(),
            source: source.to_owned(),
            destination: destination.to_owned(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitInfo {
    pub drop_in_paths: Vec<String>,
    pub need_daemon_reload: bool,
    pub load_state: String,
    pub fragment_path: String,
    pub unit_file_state: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitRuntime {
    pub argv: Option<Vec<String>>,
    pub dependencies: Vec<String>,
    pub native_timeouts: Option<zup_core::NativeServiceTimeouts>,
    pub failure: zup_core::ServiceFailurePolicy,
    pub execution: Option<zup_core::ServiceExecution>,
    pub service_type: String,
    pub pending_job: bool,
    pub state: zup_plan::ServiceRuntimeState,
    pub sub_state: String,
    pub result: String,
    pub manager_generation: String,
    pub execution_generation: String,
}

pub trait SystemdManager {
    fn process_scope_is_empty(&mut self, unit: &str) -> Result<bool, IpcError>;
    fn retain_unit(&mut self, _unit: &str) -> Result<(), IpcError> {
        Ok(())
    }
    fn generation(&mut self) -> Result<String, IpcError> {
        Err(IpcError::SystemdUnavailable(
            "manager generation is unavailable".into(),
        ))
    }
    fn runtime(&mut self, unit: &str) -> Result<UnitRuntime, IpcError> {
        Err(IpcError::SystemdRefused {
            unit: unit.into(),
            reason: "runtime observation is unavailable".into(),
        })
    }

    fn control(
        &mut self,
        unit: &str,
        state: zup_transaction::ServiceTerminalState,
        timeout: Duration,
        controlled_units: &[String],
    ) -> Result<(), IpcError> {
        let _ = (state, timeout, controlled_units);
        Err(IpcError::SystemdRefused {
            unit: unit.into(),
            reason: "runtime control is unavailable".into(),
        })
    }
    fn reload(&mut self) -> Result<(), IpcError>;

    fn unit_file_state(&mut self, unit: &str) -> Result<String, IpcError>;

    fn load_unit(&mut self, unit: &str) -> Result<UnitInfo, IpcError>;

    fn version(&mut self) -> Result<String, IpcError>;

    fn enable(&mut self, unit: &str) -> Result<Vec<UnitChange>, IpcError>;

    fn remove_owned_enablement(
        &mut self,
        unit: &str,
        canonical_source: &str,
    ) -> Result<Vec<UnitChange>, IpcError>;

    fn mask(&mut self, unit: &str) -> Result<Vec<UnitChange>, IpcError>;

    fn unmask(&mut self, unit: &str) -> Result<Vec<UnitChange>, IpcError>;
}

pub fn probe_systemd() -> Result<(), IpcError> {
    RealSystemd::connect()?.probe()
}

pub struct RealSystemd {
    runtime: tokio::runtime::Runtime,
    connection: zbus_systemd::zbus::Connection,
    subscribed: bool,
}

impl RealSystemd {
    pub fn connect() -> Result<Self, IpcError> {
        crate::service_host::validate_init_view().map_err(IpcError::SystemdUnavailable)?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| IpcError::SystemdUnavailable(format!("tokio runtime: {error}")))?;
        let connection = runtime
            .block_on(async {
                tokio::time::timeout(DBUS_TIMEOUT, async {
                    let stream = tokio::net::UnixStream::connect("/run/dbus/system_bus_socket")
                        .await
                        .map_err(|error| error.to_string())?;
                    let peer = stream.peer_cred().map_err(|error| error.to_string())?;
                    let pid = peer
                        .pid()
                        .filter(|pid| *pid > 0)
                        .ok_or("system bus peer is outside the local PID namespace")?;
                    crate::service_host::same_object(
                        "/proc/self/ns/pid",
                        &format!("/proc/{pid}/ns/pid"),
                    )
                    .map_err(|error| error.to_string())?;
                    zbus_systemd::zbus::connection::Builder::unix_stream(
                        stream.into_std().map_err(|error| error.to_string())?,
                    )
                    .build()
                    .await
                    .map_err(|error| format!("system bus: {error}"))
                })
                .await
                .map_err(|_| "connecting to the system bus timed out".to_owned())?
            })
            .map_err(IpcError::SystemdUnavailable)?;
        let mut manager = Self {
            runtime,
            connection,
            subscribed: false,
        };
        manager.generation()?;
        Ok(manager)
    }

    fn manager(&self) -> Result<zbus_systemd::systemd1::ManagerProxy<'_>, IpcError> {
        self.runtime
            .block_on(async {
                tokio::time::timeout(
                    DBUS_TIMEOUT,
                    zbus_systemd::systemd1::ManagerProxy::new(&self.connection),
                )
                .await
                .map_err(|_| IpcError::SystemdTimeout("manager proxy".into()))?
                .map_err(|error| IpcError::SystemdUnavailable(format!("systemd manager: {error}")))
            })
            .map_err(|error| match error {
                IpcError::SystemdTimeout(what) => IpcError::SystemdTimeout(what),
                other => other,
            })
    }

    fn call<R>(
        &self,
        what: &str,
        work: impl std::future::Future<Output = Result<R, IpcError>>,
    ) -> Result<R, IpcError> {
        self.runtime.block_on(async {
            tokio::time::timeout(DBUS_TIMEOUT, work)
                .await
                .map_err(|_| IpcError::SystemdTimeout(what.to_owned()))?
        })
    }

    fn probe(&mut self) -> Result<(), IpcError> {
        let proxy = self.manager()?;
        self.call("probe", async {
            proxy
                .version()
                .await
                .map(|_| ())
                .map_err(|error| IpcError::SystemdUnavailable(format!("systemd version: {error}")))
        })
    }

    fn owned_wants_dir() -> std::path::PathBuf {
        std::path::PathBuf::from("/etc/systemd/system/multi-user.target.wants")
    }

    fn changes(
        raw: Vec<(String, String, String)>,
        unit: &str,
    ) -> Result<Vec<UnitChange>, IpcError> {
        if raw.len() > 64 {
            return Err(IpcError::SystemdAmbiguous {
                unit: unit.to_owned(),
                reason: "systemd reports more changes than one unit owns".into(),
            });
        }
        raw.into_iter()
            .map(|(kind, source, destination)| UnitChange::bound(&kind, &source, &destination))
            .collect()
    }
}

impl SystemdManager for RealSystemd {
    fn process_scope_is_empty(&mut self, unit: &str) -> Result<bool, IpcError> {
        let manager = self.manager()?;
        self.call("service process scope", async {
            match manager.get_unit_processes(unit.into()).await {
                Ok(processes) => Ok(processes.is_empty()),
                // systemd retains units with populated cgroups, including inactive masked units.
                Err(zbus_systemd::zbus::Error::MethodError(name, _, _))
                    if name.as_str() == "org.freedesktop.systemd1.NoSuchUnit" =>
                {
                    Ok(true)
                }
                Err(error) => Err(IpcError::SystemdRefused {
                    unit: unit.into(),
                    reason: error.to_string(),
                }),
            }
        })
    }
    fn retain_unit(&mut self, unit: &str) -> Result<(), IpcError> {
        let manager = self.manager()?;
        self.call("retain unit", async {
            manager
                .ref_unit(unit.into())
                .await
                .map_err(|error| IpcError::SystemdRefused {
                    unit: unit.into(),
                    reason: error.to_string(),
                })
        })
    }
    fn generation(&mut self) -> Result<String, IpcError> {
        crate::service_host::validate_init_view().map_err(IpcError::SystemdUnavailable)?;
        self.call("manager generation", async {
            let bus = zbus_systemd::zbus::fdo::DBusProxy::new(&self.connection)
                .await
                .map_err(|e| IpcError::SystemdUnavailable(e.to_string()))?;
            let owner = bus
                .get_name_owner(
                    "org.freedesktop.systemd1"
                        .try_into()
                        .expect("constant bus name"),
                )
                .await
                .map_err(|e| IpcError::SystemdUnavailable(e.to_string()))?;
            let pid = bus
                .get_connection_unix_process_id(owner.clone().into())
                .await
                .map_err(|e| IpcError::SystemdUnavailable(e.to_string()))?;
            if pid != 1 {
                return Err(IpcError::SystemdUnavailable(
                    "the systemd bus owner is not the local system manager".into(),
                ));
            }
            let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
                .map_err(|e| IpcError::SystemdUnavailable(e.to_string()))?;
            let policy =
                crate::systemd_policy::generation().map_err(IpcError::SystemdUnavailable)?;
            Ok(format!("{}:{owner}:{policy}", boot.trim()))
        })
    }
    fn runtime(&mut self, unit: &str) -> Result<UnitRuntime, IpcError> {
        let manager = self.manager()?;
        self.call("runtime observation", async {
            let failed = |error: zbus_systemd::zbus::Error| IpcError::SystemdRefused {
                unit: unit.into(),
                reason: error.to_string(),
            };
            let path = manager.load_unit(unit.into()).await.map_err(failed)?;
            let proxy = zbus_systemd::systemd1::UnitProxy::new(&self.connection, path.clone())
                .await
                .map_err(failed)?;
            let service = zbus_systemd::systemd1::ServiceProxy::new(&self.connection, path)
                .await
                .map_err(failed)?;
            if !service.exec_condition().await.map_err(failed)?.is_empty()
                || !service.exec_start_pre().await.map_err(failed)?.is_empty()
                || !service.exec_start_post().await.map_err(failed)?.is_empty()
                || !service.exec_reload().await.map_err(failed)?.is_empty()
                || !service.exec_stop().await.map_err(failed)?.is_empty()
                || !service.exec_stop_post().await.map_err(failed)?.is_empty()
            {
                return Err(IpcError::SystemdRefused {
                    unit: unit.into(),
                    reason: "unmanaged executable lifecycle hooks".into(),
                });
            }
            match service.exec_reload_post().await {
                Ok(commands) if commands.is_empty() => {}
                Err(error) if unknown_property(&error) => {}
                Err(error) => return Err(failed(error)),
                Ok(_) => return Err(IpcError::SystemdRefused {
                    unit: unit.into(),
                    reason: "unmanaged executable reload hooks".into(),
                }),
            }
            let commands = service.exec_start_ex().await.map_err(failed)?;
            let argv = match commands.as_slice() {
                [] => None,
                [(path, argv, flags, ..)]
                    if argv.first() == Some(path) && flags == &["no-env-expand"] => Some(argv.clone()),
                _ => return Err(IpcError::SystemdRefused {
                    unit: unit.into(),
                    reason: "unmanaged executable command or execution flags".into(),
                }),
            };
            let active = proxy.active_state().await.map_err(failed)?;
            let sub_state = proxy.sub_state().await.map_err(failed)?;
            let result = service.result().await.map_err(failed)?;
            let service_type = service.type_property().await.map_err(failed)?;
            let dependencies = proxy.requires().await.map_err(failed)?
                .into_iter()
                .filter(|name| !matches!(name.as_str(), "sysinit.target" | "system.slice"))
                .collect();
            let execution = if proxy.load_state().await.map_err(failed)? == "loaded" {
                let stop_failure_mode = crate::systemd_policy::stop_failure_mode(
                    &proxy.drop_in_paths().await.map_err(failed)?,
                ).map_err(|reason| IpcError::SystemdRefused { unit: unit.into(), reason })?;
                match service.restart_randomized_delay_u_sec().await {
                    Ok(0) => {},
                    Err(error) if unknown_property(&error) => {},
                    Err(error) => return Err(failed(error)),
                    Ok(_) => return Err(IpcError::SystemdRefused {
                        unit: unit.into(),
                        reason: "unmanaged randomized restart delay".into(),
                    }),
                }
                if !proxy.wants().await.map_err(failed)?.is_empty()
                    || !proxy.requisite().await.map_err(failed)?.is_empty()
                    || !proxy.binds_to().await.map_err(failed)?.is_empty()
                    || !proxy.upholds().await.map_err(failed)?.is_empty()
                    || !proxy.part_of().await.map_err(failed)?.is_empty()
                    || !proxy.on_failure().await.map_err(failed)?.is_empty()
                    || !proxy.on_success().await.map_err(failed)?.is_empty()
                    || proxy.conflicts().await.map_err(failed)?.iter().any(|name| name != "shutdown.target")
                    || proxy.failure_action().await.map_err(failed)? != "none"
                    || proxy.success_action().await.map_err(failed)? != "none"
                    || proxy.start_limit_action().await.map_err(failed)? != "none"
                    || proxy.stop_when_unneeded().await.map_err(failed)?
                    || proxy.refuse_manual_start().await.map_err(failed)?
                    || proxy.refuse_manual_stop().await.map_err(failed)?
                    || proxy.job_timeout_u_sec().await.map_err(failed)? != u64::MAX
                    || proxy.job_running_timeout_u_sec().await.map_err(failed)? != u64::MAX
                    || proxy.job_timeout_action().await.map_err(failed)? != "none"
                {
                    return Err(IpcError::SystemdRefused {
                        unit: unit.into(),
                        reason: "unmanaged effective unit dependencies or actions".into(),
                    });
                }
                if service.kill_mode().await.map_err(failed)? != "control-group"
                    || !service.send_sigkill().await.map_err(failed)?
                    || service.kill_signal().await.map_err(failed)? != rustix::process::Signal::TERM.as_raw()
                    || service.final_kill_signal().await.map_err(failed)? != rustix::process::Signal::KILL.as_raw()
                    || service.send_sighup().await.map_err(failed)?
                {
                    return Err(IpcError::SystemdRefused {
                        unit: unit.into(),
                        reason: "effective shutdown policy does not terminate the complete service cgroup".into(),
                    });
                }
                if !service.no_new_privileges().await.map_err(failed)? {
                    return Err(IpcError::SystemdRefused {
                        unit: unit.into(),
                        reason: "effective service security lacks NoNewPrivileges".into(),
                    });
                }
                if service.delegate().await.map_err(failed)? {
                    return Err(IpcError::SystemdRefused { unit: unit.into(), reason: "service cgroup delegation is not authorized".into() });
                }
                if service.ambient_capabilities().await.map_err(failed)? != 0
                    || !service.supplementary_groups().await.map_err(failed)?.is_empty()
                    || !service.pam_name().await.map_err(failed)?.is_empty()
                    || !service.environment().await.map_err(failed)?.is_empty()
                    || !service.environment_files().await.map_err(failed)?.is_empty()
                    || !service.pass_environment().await.map_err(failed)?.is_empty()
                    || !service.unset_environment().await.map_err(failed)?.is_empty()
                    || !service.root_directory().await.map_err(failed)?.is_empty()
                    || !service.root_image().await.map_err(failed)?.is_empty()
                    || !matches!(service.working_directory().await.map_err(failed)?.as_str(), "" | "/")
                    || !service.bind_paths().await.map_err(failed)?.is_empty()
                    || !service.bind_read_only_paths().await.map_err(failed)?.is_empty()
                    || !service.extension_images().await.map_err(failed)?.is_empty()
                    || !service.extension_directories().await.map_err(failed)?.is_empty()
                    || !service.mount_images().await.map_err(failed)?.is_empty()
                    || !service.open_file().await.map_err(failed)?.is_empty()
                    || !service.set_credential().await.map_err(failed)?.is_empty()
                    || !service.set_credential_encrypted().await.map_err(failed)?.is_empty()
                    || !service.load_credential().await.map_err(failed)?.is_empty()
                    || !service.load_credential_encrypted().await.map_err(failed)?.is_empty()
                    || !service.import_credential().await.map_err(failed)?.is_empty()
                {
                    return Err(IpcError::SystemdRefused {
                        unit: unit.into(),
                        reason: "unmanaged effective execution environment or privileges".into(),
                    });
                }
                if service.exit_type().await.map_err(failed)? != "main"
                    || service.notify_access().await.map_err(failed)? != if service_type == "notify" { "main" } else { "none" }
                    || service.remain_after_exit().await.map_err(failed)?
                    || !service.pid_file().await.map_err(failed)?.is_empty()
                    || !matches!(service.watchdog_u_sec().await.map_err(failed)?, 0 | u64::MAX)
                    || service.runtime_max_u_sec().await.map_err(failed)? != u64::MAX
                    || service.runtime_randomized_extra_u_sec().await.map_err(failed)? != 0
                    || service.timeout_start_failure_mode().await.map_err(failed)? != "terminate"
                    || service.timeout_stop_failure_mode().await.map_err(failed)? != stop_failure_mode
                    || service.restart_mode().await.map_err(failed)? != "normal"
                    || service.restart_steps().await.map_err(failed)? != 0
                    || service.restart_max_delay_u_sec().await.map_err(failed)? != u64::MAX
                {
                    return Err(IpcError::SystemdRefused {
                        unit: unit.into(),
                        reason: "unmanaged effective runtime lifecycle policy".into(),
                    });
                }
                for (statuses, signals) in [
                    service.success_exit_status().await.map_err(failed)?,
                    service.restart_prevent_exit_status().await.map_err(failed)?,
                    service.restart_force_exit_status().await.map_err(failed)?,
                ] {
                    if !statuses.is_empty() || !signals.is_empty() {
                        return Err(IpcError::SystemdRefused {
                            unit: unit.into(),
                            reason: "unmanaged effective exit classification".into(),
                        });
                    }
                }
                Some(if service.dynamic_user().await.map_err(failed)? {
                    let name = crate::services::dynamic_user_name(unit);
                    if service.user().await.map_err(failed)? != name
                        || service.group().await.map_err(failed)? != name
                    {
                        return Err(IpcError::SystemdRefused {
                            unit: unit.into(),
                            reason: "dynamic service account names differ from the owned registration".into(),
                        });
                    }
                    let uid = match manager.lookup_dynamic_user_by_name(name.clone()).await {
                        Ok(uid) => {
                            if manager.lookup_dynamic_user_by_uid(uid).await.map_err(failed)? != name {
                                return Err(IpcError::SystemdRefused { unit: unit.into(), reason: "dynamic account allocation does not round-trip through the manager".into() });
                            }
                            Some(uid)
                        },
                        Err(zbus_systemd::zbus::Error::MethodError(error, _, _))
                            if error.as_str() == "org.freedesktop.systemd1.NoSuchDynamicUser" && active != "active" => None,
                        Err(error) => return Err(failed(error)),
                    };
                    let user = uzers::get_user_by_name(&name);
                    let group = uzers::get_group_by_name(&name);
                    if user.as_ref().is_some_and(|user| Some(user.uid()) != uid)
                        || group.as_ref().is_some_and(|group| Some(group.gid()) != uid)
                    {
                        return Err(IpcError::SystemdRefused { unit: unit.into(), reason: "dynamic account name collides with another account".into() });
                    }
                    if active == "active" {
                        let uid = uid.ok_or_else(|| IpcError::SystemdRefused { unit: unit.into(), reason: "running service lacks a dynamic account allocation".into() })?;
                        verify_running_credentials(&service, unit, uid, uid).await?;
                        if manager.lookup_dynamic_user_by_uid(uid).await.map_err(failed)? != name {
                            return Err(IpcError::SystemdRefused { unit: unit.into(), reason: "dynamic execution changed during credential verification".into() });
                        }
                    }
                    zup_core::ServiceExecution::SystemdDynamicUser
                } else {
                    let user = service.user().await.map_err(failed)?;
                    let group = service.group().await.map_err(failed)?;
                    if active == "active" {
                        let account = uzers::get_user_by_name(&user).ok_or_else(|| IpcError::SystemdRefused { unit: unit.into(), reason: "service account is not resolvable".into() })?;
                        let gid = if group.is_empty() { account.primary_group_id() } else {
                            uzers::get_group_by_name(&group).ok_or_else(|| IpcError::SystemdRefused { unit: unit.into(), reason: "service group is not resolvable".into() })?.gid()
                        };
                        verify_running_credentials(&service, unit, account.uid(), gid).await?;
                    }
                    zup_core::ServiceExecution::SystemdUser {
                        user,
                        group: (!group.is_empty()).then_some(group),
                    }
                })
            } else {
                None
            };
            let seconds = |micros: u64| {
                if micros.is_multiple_of(1_000_000) {
                    u32::try_from(micros / 1_000_000).ok()
                } else {
                    None
                }
            };
            let restart = service.restart().await.map_err(failed)?;
            let native_timeouts = if execution.is_some() {
                let timeout = |value| {
                    seconds(value).filter(|value| *value > 0).ok_or_else(|| {
                        IpcError::SystemdRefused {
                            unit: unit.into(),
                            reason: "native timeouts must use bounded whole seconds".into(),
                        }
                    })
                };
                Some(zup_core::NativeServiceTimeouts::Systemd {
                    start_timeout_seconds: timeout(
                        service.timeout_start_u_sec().await.map_err(failed)?,
                    )?,
                    stop_grace_seconds: timeout(
                        service.timeout_stop_u_sec().await.map_err(failed)?,
                    )?,
                    abort_timeout_seconds: timeout(
                        service.timeout_abort_u_sec().await.map_err(failed)?,
                    )?,
                })
            } else {
                None
            };
            let failure = match restart.as_str() {
                "no" => {
                    if execution.is_some() && (service.restart_u_sec().await.map_err(failed)? != 0
                        || proxy.start_limit_interval_u_sec().await.map_err(failed)? != 0
                        || proxy.start_limit_burst().await.map_err(failed)? != 0)
                    {
                        return Err(IpcError::SystemdRefused {
                            unit: unit.into(),
                            reason: "unmanaged effective disabled restart policy".into(),
                        });
                    }
                    zup_core::ServiceFailurePolicy::None
                },
                "on-failure" | "on-abnormal" => {
                    let Some(delay_seconds) =
                        seconds(service.restart_u_sec().await.map_err(failed)?)
                    else {
                        return Err(IpcError::SystemdRefused {
                            unit: unit.into(),
                            reason: "restart delay must use whole seconds".into(),
                        });
                    };
                    let Some(start_limit_interval_seconds) =
                        seconds(proxy.start_limit_interval_u_sec().await.map_err(failed)?)
                    else {
                        return Err(IpcError::SystemdRefused {
                            unit: unit.into(),
                            reason: "start limit interval must use whole seconds".into(),
                        });
                    };
                    zup_core::ServiceFailurePolicy::Systemd {
                        condition: if restart == "on-failure" {
                            zup_core::SystemdRestartCondition::OnFailure
                        } else {
                            zup_core::SystemdRestartCondition::OnAbnormal
                        },
                        delay_seconds,
                        start_limit_interval_seconds,
                        start_limit_burst: proxy.start_limit_burst().await.map_err(failed)?,
                    }
                }
                _ => {
                    return Err(IpcError::SystemdRefused {
                        unit: unit.into(),
                        reason: "unmanaged runtime failure policy".into(),
                    });
                }
            };
            let invocation = proxy.invocation_id().await.map_err(failed)?;
            let pending_job = proxy.job().await.map_err(failed)?.0 != 0;
            let bus = zbus_systemd::zbus::fdo::DBusProxy::new(&self.connection)
                .await
                .map_err(failed)?;
            let owner = bus
                .get_name_owner(
                    "org.freedesktop.systemd1"
                        .try_into()
                        .expect("constant bus name"),
                )
                .await
                .map_err(|e| IpcError::SystemdUnavailable(e.to_string()))?;
            let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
                .map_err(|e| IpcError::SystemdUnavailable(e.to_string()))?;
            use zup_plan::ServiceRuntimeState as Runtime;
            let state = match (active.as_str(), sub_state.as_str()) {
                ("active", "running") => Runtime::Running,
                ("inactive", "dead") | ("failed", "failed") => Runtime::Stopped,
                ("activating", _) => Runtime::StartPending,
                ("deactivating", _) => Runtime::StopPending,
                _ => Runtime::Unknown,
            };
            Ok(UnitRuntime {
                argv,
                dependencies,
                native_timeouts,
                failure,
                execution,
                service_type,
                pending_job,
                state,
                sub_state,
                result,
                manager_generation: format!(
                    "{}:{owner}:{}",
                    boot.trim(),
                    crate::systemd_policy::generation().map_err(IpcError::SystemdUnavailable)?
                ),
                execution_generation: if state == Runtime::Stopped {
                    "stopped".into()
                } else {
                    invocation
                        .iter()
                        .map(|byte| format!("{byte:02x}"))
                        .collect()
                },
            })
        })
    }

    fn control(
        &mut self,
        unit: &str,
        state: zup_transaction::ServiceTerminalState,
        timeout: Duration,
        controlled_units: &[String],
    ) -> Result<(), IpcError> {
        use futures_util::StreamExt;
        let deadline = tokio::time::Instant::now() + timeout;
        if !self.subscribed {
            let manager = self.manager()?;
            self.runtime.block_on(async {
                tokio::time::timeout_at(deadline, manager.subscribe())
                    .await
                    .map_err(|_| IpcError::SystemdTimeout("subscribe to jobs".into()))?
                    .map_err(|error| IpcError::SystemdUnavailable(error.to_string()))
            })?;
            self.subscribed = true;
        }
        let manager = self.manager()?;
        self.runtime.block_on(async {
            tokio::time::timeout_at(deadline, async {
                let failed = |error: zbus_systemd::zbus::Error| IpcError::SystemdRefused {
                    unit: unit.into(),
                    reason: error.to_string(),
                };
                let mut jobs = manager.receive_job_removed().await.map_err(failed)?;
                // Keep inactive units loaded through ResetFailedUnit and job completion.
                // The manager releases this client's references when the bus disconnects.
                if state == zup_transaction::ServiceTerminalState::Running
                    || !matches!(manager.get_unit_file_state(unit.into()).await.map_err(failed)?.as_str(), "masked" | "masked-runtime")
                {
                    manager.ref_unit(unit.into()).await.map_err(failed)?;
                }
                if state == zup_transaction::ServiceTerminalState::Stopped {
                    let path = manager.get_unit(unit.into()).await.map_err(failed)?;
                    let proxy = zbus_systemd::systemd1::UnitProxy::new(&self.connection, path).await.map_err(failed)?;
                    for dependents in [proxy.required_by().await, proxy.bound_by().await, proxy.consists_of().await, proxy.propagates_stop_to().await] {
                        for dependent in dependents.map_err(failed)? {
                            if !controlled_units.contains(&dependent) {
                                return Err(IpcError::SystemdRefused { unit: unit.into(), reason: format!("stopping this unit could control undeclared dependent {dependent}") });
                            }
                            let path = manager.get_unit(dependent.clone()).await.map_err(failed)?;
                            let dependent = zbus_systemd::systemd1::UnitProxy::new(&self.connection, path).await.map_err(failed)?;
                            if !matches!(dependent.active_state().await.map_err(failed)?.as_str(), "inactive" | "failed") || dependent.job().await.map_err(failed)?.0 != 0 {
                                return Err(IpcError::SystemdRefused { unit: unit.into(), reason: "managed dependents must finish shutdown before their dependency".into() });
                            }
                        }
                    }
                }
                let within_deadline = || {
                    if tokio::time::Instant::now() >= deadline {
                        Err(IpcError::SystemdTimeout(format!("service {unit}")))
                    } else {
                        Ok(())
                    }
                };
                within_deadline()?;
                let job = match state {
                    zup_transaction::ServiceTerminalState::Running => {
                        // A deployment start begins a new native retry budget, including
                        // compensation after a failed replacement exhausted the old one.
                        manager.reset_failed_unit(unit.into()).await.map_err(failed)?;
                        within_deadline()?;
                        manager.start_unit(unit.into(), "fail".into()).await
                    }
                    zup_transaction::ServiceTerminalState::Stopped => {
                        manager.stop_unit(unit.into(), "fail".into()).await
                    }
                }
                .map_err(failed)?;
                zup_transaction::fault_injection::boundary(zup_transaction::fault_injection::Boundary::NativeRequestSubmitted, unit);
                while let Some(signal) = jobs.next().await {
                    let args = signal.args().map_err(failed)?;
                    if args.job == job && args.unit == unit {
                        if args.result == "done" {
                            let path = manager.get_unit(unit.into()).await.map_err(failed)?;
                            let observed = zbus_systemd::systemd1::UnitProxy::new(&self.connection, path).await.map_err(failed)?;
                            let mut interval = std::time::Duration::from_millis(20);
                            loop {
                                let active = observed.active_state().await.map_err(failed)?;
                                if observed.job().await.map_err(failed)?.0 == 0
                                    && matches!(active.as_str(), "active" | "inactive" | "failed")
                                {
                                    within_deadline()?;
                                    return Ok(());
                                }
                                tokio::time::sleep(interval).await;
                                interval = (interval * 2).min(std::time::Duration::from_millis(250));
                            }
                        }
                        return Err(IpcError::SystemdRefused {
                            unit: unit.into(),
                            reason: format!("job result: {}", args.result),
                        });
                    }
                }
                Err(IpcError::SystemdUnavailable(
                    "job signal stream disconnected".into(),
                ))
            })
            .await
            .map_err(|_| IpcError::SystemdTimeout(format!("service {unit}")))?
        })
    }
    fn reload(&mut self) -> Result<(), IpcError> {
        let proxy = self.manager()?;
        self.call("reload", async {
            proxy
                .reload()
                .await
                .map_err(|error| IpcError::SystemdUnavailable(format!("reload: {error}")))
        })
    }

    fn unit_file_state(&mut self, unit: &str) -> Result<String, IpcError> {
        let proxy = self.manager()?;
        let unit = unit.to_owned();
        self.call("unit-file-state", async {
            match proxy.get_unit_file_state(unit.clone()).await {
                Ok(state) => Ok(state),
                Err(zbus_systemd::zbus::Error::FDO(error))
                    if matches!(
                        error.as_ref(),
                        zbus_systemd::zbus::fdo::Error::FileNotFound(_)
                    ) =>
                {
                    Ok("not-found".into())
                }
                Err(error) => Err(IpcError::SystemdRefused {
                    unit: unit.clone(),
                    reason: format!("unit-file state: {error}"),
                }),
            }
        })
    }

    fn load_unit(&mut self, unit: &str) -> Result<UnitInfo, IpcError> {
        let proxy = self.manager()?;
        let unit = unit.to_owned();
        let path = self.call("load-unit", async {
            proxy
                .load_unit(unit.clone())
                .await
                .map_err(|error| IpcError::SystemdRefused {
                    unit: unit.clone(),
                    reason: format!("load unit: {error}"),
                })
        })?;
        let info = self.call("unit-properties", async {
            let unit_proxy = zbus_systemd::systemd1::UnitProxy::new(&self.connection, path.clone())
                .await
                .map_err(|error| IpcError::SystemdRefused {
                    unit: unit.clone(),
                    reason: format!("unit proxy: {error}"),
                })?;
            let (load_state, fragment_path, unit_file_state) = tokio::join!(
                unit_proxy.load_state(),
                unit_proxy.fragment_path(),
                unit_proxy.unit_file_state(),
            );
            Ok::<_, IpcError>(UnitInfo {
                drop_in_paths: unit_proxy.drop_in_paths().await.map_err(|error| {
                    IpcError::SystemdRefused {
                        unit: unit.clone(),
                        reason: error.to_string(),
                    }
                })?,
                need_daemon_reload: unit_proxy.need_daemon_reload().await.map_err(|error| {
                    IpcError::SystemdRefused {
                        unit: unit.clone(),
                        reason: error.to_string(),
                    }
                })?,
                load_state: load_state.map_err(|error| IpcError::SystemdRefused {
                    unit: unit.clone(),
                    reason: format!("load state: {error}"),
                })?,
                fragment_path: fragment_path.map_err(|error| IpcError::SystemdRefused {
                    unit: unit.clone(),
                    reason: format!("fragment path: {error}"),
                })?,
                unit_file_state: unit_file_state.map_err(|error| IpcError::SystemdRefused {
                    unit: unit.clone(),
                    reason: format!("unit-file state: {error}"),
                })?,
            })
        })?;
        Ok(info)
    }

    fn enable(&mut self, unit: &str) -> Result<Vec<UnitChange>, IpcError> {
        let proxy = self.manager()?;
        let unit = unit.to_owned();
        self.call("enable", async {
            let (carries_install_info, changes) = proxy
                .enable_unit_files(vec![unit.clone()], false, false)
                .await
                .map_err(|error| IpcError::SystemdRefused {
                    unit: unit.clone(),
                    reason: format!("enable: {error}"),
                })?;
            let _ = carries_install_info;
            Self::changes(changes, &unit)
        })
    }

    fn remove_owned_enablement(
        &mut self,
        unit: &str,
        canonical_source: &str,
    ) -> Result<Vec<UnitChange>, IpcError> {
        remove_exact_enablement_link(&Self::owned_wants_dir(), unit, canonical_source)
    }

    fn version(&mut self) -> Result<String, IpcError> {
        let proxy = self.manager()?;
        self.call("version", async {
            proxy
                .version()
                .await
                .map_err(|error| IpcError::SystemdUnavailable(format!("systemd version: {error}")))
        })
    }

    fn mask(&mut self, unit: &str) -> Result<Vec<UnitChange>, IpcError> {
        let proxy = self.manager()?;
        let unit = unit.to_owned();
        self.call("mask", async {
            let changes = proxy
                .mask_unit_files(vec![unit.clone()], false, false)
                .await
                .map_err(|error| IpcError::SystemdRefused {
                    unit: unit.clone(),
                    reason: format!("mask: {error}"),
                })?;
            Self::changes(changes, &unit)
        })
    }

    fn unmask(&mut self, unit: &str) -> Result<Vec<UnitChange>, IpcError> {
        let proxy = self.manager()?;
        let unit = unit.to_owned();
        self.call("unmask", async {
            let changes = proxy
                .unmask_unit_files(vec![unit.clone()], false)
                .await
                .map_err(|error| IpcError::SystemdRefused {
                    unit: unit.clone(),
                    reason: format!("unmask: {error}"),
                })?;
            Self::changes(changes, &unit)
        })
    }
}

fn remove_exact_enablement_link(
    wants_dir: &std::path::Path,
    unit: &str,
    canonical_source: &str,
) -> Result<Vec<UnitChange>, IpcError> {
    let unit = unit.to_owned();
    let link = wants_dir.join(&unit);
    let directory = match crate::fs::OwnedDirectory::open(wants_dir) {
        Ok(directory) => directory,
        Err(error) => {
            let missing = std::fs::symlink_metadata(wants_dir)
                .err()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound);
            if missing {
                return Ok(Vec::new());
            }
            return Err(IpcError::SystemdRefused {
                unit,
                reason: format!("owned enablement is not inspectable: {error}"),
            });
        }
    };
    let target = match directory.read_link_target(&unit) {
        Ok(target) => target,
        Err(crate::error::PathError::Missing { .. }) => return Ok(Vec::new()),
        Err(error) => {
            return Err(IpcError::SystemdRefused {
                unit,
                reason: format!("owned enablement is not inspectable: {error}"),
            });
        }
    };
    if target.to_string_lossy() != canonical_source {
        return Err(IpcError::SystemdAmbiguous {
            unit,
            reason: format!(
                "the owned enablement link points at `{}`, not the Zup source",
                target.display()
            ),
        });
    }
    directory
        .remove_file(&unit)
        .map_err(|error| IpcError::SystemdRefused {
            unit: unit.clone(),
            reason: format!("the owned enablement link cannot be removed: {error}"),
        })?;
    directory.sync().map_err(|error| IpcError::SystemdRefused {
        unit: unit.clone(),
        reason: format!("the wants directory does not flush: {error}"),
    })?;
    let link = link.to_string_lossy().into_owned();
    Ok(vec![UnitChange::bound("unlink", &link, "").map_err(
        |error| IpcError::SystemdAmbiguous {
            unit: unit.clone(),
            reason: error.to_string(),
        },
    )?])
}

#[cfg(all(test, feature = "test-support"))]
mod fake_tests {
    use super::*;
    use crate::test_support::FakeSystemd;

    #[test]
    fn fake_models_the_start_policy_states() {
        let mut fake = FakeSystemd::default();
        fake.seed(
            "zup-a.service",
            "disabled",
            "/usr/local/lib/systemd/system/zup-a.service",
        );
        assert_eq!(fake.unit_file_state("zup-a.service").unwrap(), "disabled");
        fake.enable("zup-a.service").unwrap();
        assert_eq!(fake.unit_file_state("zup-a.service").unwrap(), "enabled");
        fake.mask("zup-a.service").unwrap();
        assert_eq!(fake.unit_file_state("zup-a.service").unwrap(), "masked");
        fake.unmask("zup-a.service").unwrap();
        assert_eq!(fake.unit_file_state("zup-a.service").unwrap(), "disabled");
    }

    #[test]
    fn fake_refuses_owned_link_removal_with_unrelated_links() {
        let mut fake = FakeSystemd::default();
        fake.seed(
            "zup-a.service",
            "enabled",
            "/usr/local/lib/systemd/system/zup-a.service",
        );
        fake.extra_links.insert(
            "zup-a.service".into(),
            vec!["/etc/systemd/system/graphical.target.wants/zup-a.service".into()],
        );
        assert!(
            fake.remove_owned_enablement(
                "zup-a.service",
                "/usr/local/lib/systemd/system/zup-a.service"
            )
            .is_err()
        );
        assert_eq!(fake.unit_file_state("zup-a.service").unwrap(), "enabled");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changes_are_bounded() {
        assert!(UnitChange::bound("symlink", "a", &"x".repeat(2048)).is_err());
        assert!(UnitChange::bound("symlink", "a\nb", "c").is_err());
    }

    #[test]
    fn exact_link_removal_touches_only_the_proven_name() {
        let base = tempfile::tempdir().expect("an isolated tree");
        let wants = base.path().join("multi-user.target.wants");
        std::fs::create_dir_all(&wants).expect("a wants directory");
        let unit = "zup-owned.service";
        let canonical = "/usr/local/lib/systemd/system/zup-owned.service";
        let link = wants.join(unit);
        std::os::unix::fs::symlink(canonical, &link).expect("the owned link");
        let neighbor = wants.join("unrelated.service");
        std::fs::write(&neighbor, b"[Unit]\n").expect("a neighbor");

        let changes =
            remove_exact_enablement_link(&wants, unit, canonical).expect("owned link removes");
        assert_eq!(changes.len(), 1);
        assert!(!link.exists() && std::fs::symlink_metadata(&link).is_err());
        assert!(neighbor.is_file(), "neighbors survive");

        assert!(
            remove_exact_enablement_link(&wants, unit, canonical)
                .expect("absent link is no change")
                .is_empty()
        );
    }

    #[test]
    fn exact_link_removal_refuses_a_repointed_link() {
        let base = tempfile::tempdir().expect("an isolated tree");
        let wants = base.path().join("multi-user.target.wants");
        std::fs::create_dir_all(&wants).expect("a wants directory");
        let unit = "zup-owned.service";
        let link = wants.join(unit);
        std::os::unix::fs::symlink("/usr/lib/systemd/system/zup-owned.service", &link)
            .expect("a repointed link");
        assert!(
            remove_exact_enablement_link(
                &wants,
                unit,
                "/usr/local/lib/systemd/system/zup-owned.service"
            )
            .is_err()
        );
        assert!(
            std::fs::symlink_metadata(&link).is_ok(),
            "a repointed link is preserved, never removed"
        );
    }

    #[test]
    fn exact_link_removal_refuses_a_non_link() {
        let base = tempfile::tempdir().expect("an isolated tree");
        let wants = base.path().join("multi-user.target.wants");
        std::fs::create_dir_all(&wants).expect("a wants directory");
        let unit = "zup-owned.service";
        std::fs::write(wants.join(unit), b"[Unit]\n").expect("a regular file");
        assert!(
            remove_exact_enablement_link(
                &wants,
                unit,
                "/usr/local/lib/systemd/system/zup-owned.service"
            )
            .is_err()
        );
        assert!(wants.join(unit).is_file(), "a regular file is preserved");
    }

    #[test]
    fn exact_link_removal_tolerates_a_missing_tree() {
        let base = tempfile::tempdir().expect("an isolated tree");
        assert!(
            remove_exact_enablement_link(
                &base.path().join("no-such-wants"),
                "zup-owned.service",
                "/usr/local/lib/systemd/system/zup-owned.service",
            )
            .expect("a missing tree is no change")
            .is_empty()
        );
    }
}
