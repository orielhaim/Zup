use crate::error::IpcError;
use std::time::Duration;

pub const DBUS_TIMEOUT: Duration = Duration::from_secs(30);

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
    pub service_type: String,
    pub pending_job: bool,
    pub state: zup_plan::ServiceRuntimeState,
    pub sub_state: String,
    pub result: String,
    pub manager_generation: String,
    pub execution_generation: String,
}

pub trait SystemdManager {
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
    ) -> Result<(), IpcError> {
        let _ = (state, timeout);
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
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| IpcError::SystemdUnavailable(format!("tokio runtime: {error}")))?;
        let connection = runtime
            .block_on(async {
                tokio::time::timeout(DBUS_TIMEOUT, zbus_systemd::zbus::Connection::system())
                    .await
                    .map_err(|_| "connecting to the system bus timed out".to_owned())?
                    .map_err(|error| format!("system bus: {error}"))
            })
            .map_err(IpcError::SystemdUnavailable)?;
        Ok(Self {
            runtime,
            connection,
            subscribed: false,
        })
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
    fn generation(&mut self) -> Result<String, IpcError> {
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
            let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
                .map_err(|e| IpcError::SystemdUnavailable(e.to_string()))?;
            Ok(format!("{}:{owner}", boot.trim()))
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
            let active = proxy.active_state().await.map_err(failed)?;
            let sub_state = proxy.sub_state().await.map_err(failed)?;
            let result = service.result().await.map_err(failed)?;
            let service_type = service.type_property().await.map_err(failed)?;
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
                service_type,
                pending_job,
                state,
                sub_state,
                result,
                manager_generation: format!("{}:{owner}", boot.trim()),
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
    ) -> Result<(), IpcError> {
        use futures_util::StreamExt;
        if !self.subscribed {
            let manager = self.manager()?;
            self.call("subscribe to jobs", async {
                manager
                    .subscribe()
                    .await
                    .map_err(|error| IpcError::SystemdUnavailable(error.to_string()))
            })?;
            self.subscribed = true;
        }
        let manager = self.manager()?;
        self.runtime.block_on(async {
            tokio::time::timeout(timeout, async {
                let failed = |error: zbus_systemd::zbus::Error| IpcError::SystemdRefused {
                    unit: unit.into(),
                    reason: error.to_string(),
                };
                let mut jobs = manager.receive_job_removed().await.map_err(failed)?;
                if state == zup_transaction::ServiceTerminalState::Stopped {
                    let path = manager.get_unit(unit.into()).await.map_err(failed)?;
                    let proxy = zbus_systemd::systemd1::UnitProxy::new(&self.connection, path).await.map_err(failed)?;
                    for dependents in [proxy.required_by().await, proxy.bound_by().await, proxy.consists_of().await, proxy.propagates_stop_to().await] {
                        if !dependents.map_err(failed)?.is_empty() {
                            return Err(IpcError::SystemdRefused { unit: unit.into(), reason: "stopping this unit could stop another unit; dependent lifecycle authorization is required".into() });
                        }
                    }
                }
                let job = match state {
                    zup_transaction::ServiceTerminalState::Running => {
                        manager.start_unit(unit.into(), "fail".into()).await
                    }
                    zup_transaction::ServiceTerminalState::Stopped => {
                        manager.stop_unit(unit.into(), "fail".into()).await
                    }
                }
                .map_err(failed)?;
                while let Some(signal) = jobs.next().await {
                    let args = signal.args().map_err(failed)?;
                    if args.job == job && args.unit == unit {
                        if args.result == "done" {
                            return Ok(());
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
