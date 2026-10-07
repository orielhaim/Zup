//! The narrow systemd manager surface Phase 6 needs.
//!
//! Production talks to the systemd system manager over D-Bus through
//! `zbus_systemd` (only the generated `systemd1` interface, only the Tokio
//! integration). Tests talk to [`FakeSystemd`]. Both implement
//! [`SystemdManager`], which exposes exactly the operations Zup performs -
//! reload, unit-file-state inspection, enable/disable/mask/unmask with
//! `force=false` and persistent (non-runtime) semantics - and deliberately
//! omits `StartUnit`/`StopUnit`/`RestartUnit`: Zup owns unit source and
//! persistent start policy, never running process state.
//!
//! One connection per logical privileged operation where practical: the real
//! manager connects once per [`RealSystemd`] value and is dropped when the
//! worker exits. No background client survives the worker.

use std::collections::BTreeMap;
use std::time::Duration;

/// Bounded wait for control-plane D-Bus operations.
pub const DBUS_TIMEOUT: Duration = Duration::from_secs(30);

/// Why systemd could not be driven.
#[derive(Debug, thiserror::Error)]
pub enum SystemdError {
    #[error("systemd is unavailable: {0}")]
    Unavailable(String),
    #[error("systemd refused `{unit}`: {reason}")]
    Refused { unit: String, reason: String },
    #[error("systemd state for `{unit}` is ambiguous: {reason}")]
    Ambiguous { unit: String, reason: String },
    #[error("systemd operation timed out: {0}")]
    Timeout(String),
}

/// One unit-file change systemd reports, bounded before it is persisted.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UnitChange {
    /// Change kind systemd reports (`symlink`, `unlink`, ...).
    pub kind: String,
    /// The unit or link the change concerns.
    pub source: String,
    /// Where the change points, when systemd names one.
    pub destination: String,
}

impl UnitChange {
    /// Bound one reported change: small strings only, so a hostile manager
    /// cannot grow the journal without limit.
    pub fn bound(kind: &str, source: &str, destination: &str) -> Result<Self, SystemdError> {
        for value in [kind, source, destination] {
            if value.len() > 1024 {
                return Err(SystemdError::Ambiguous {
                    unit: source.to_owned(),
                    reason: "a systemd change description exceeds its bound".into(),
                });
            }
            if value.bytes().any(|b| b == 0 || b == b'\n' || b == b'\r') {
                return Err(SystemdError::Ambiguous {
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

/// What the manager knows about one loaded unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitInfo {
    pub load_state: String,
    pub fragment_path: String,
    pub unit_file_state: String,
}

/// The narrow control surface: reload, inspection, persistent
/// enablement/mask transitions. No start, no stop, no restart.
pub trait SystemdManager {
    /// Reload systemd's unit configuration after source changes.
    fn reload(&mut self) -> Result<(), SystemdError>;
    /// Persistent unit-file state (`enabled`, `disabled`, `masked`, ...).
    fn unit_file_state(&mut self, unit: &str) -> Result<String, SystemdError>;
    /// Load state plus which fragment systemd resolves for the unit.
    fn load_unit(&mut self, unit: &str) -> Result<UnitInfo, SystemdError>;
    /// Persistently enable (never runtime-only, never forced).
    fn enable(&mut self, unit: &str) -> Result<Vec<UnitChange>, SystemdError>;
    /// Remove persistent enablement Zup owns (never forced). Implementations
    /// must refuse rather than delete unrelated administrator links; see the
    /// service executor's ownership check.
    fn disable(&mut self, unit: &str) -> Result<Vec<UnitChange>, SystemdError>;
    /// Persistently mask (never forced).
    fn mask(&mut self, unit: &str) -> Result<Vec<UnitChange>, SystemdError>;
    /// Remove the persistent mask (never forced).
    fn unmask(&mut self, unit: &str) -> Result<Vec<UnitChange>, SystemdError>;
}

/// Whether systemd's system manager is reachable without mutating anything.
///
/// Read-only probe: connects, names `org.freedesktop.systemd1`, reads one
/// manager property. Used by `doctor` and by the runtime capability
/// preflight before any filesystem mutation.
pub fn probe_systemd() -> Result<(), SystemdError> {
    RealSystemd::connect()?.probe()
}

/// Production manager: the systemd system bus via `zbus_systemd`.
pub struct RealSystemd {
    runtime: tokio::runtime::Runtime,
    connection: zbus_systemd::zbus::Connection,
}

impl RealSystemd {
    /// Connect to the system bus with a bounded timeout.
    pub fn connect() -> Result<Self, SystemdError> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| SystemdError::Unavailable(format!("tokio runtime: {error}")))?;
        let connection = runtime
            .block_on(async {
                tokio::time::timeout(DBUS_TIMEOUT, zbus_systemd::zbus::Connection::system())
                    .await
                    .map_err(|_| "connecting to the system bus timed out".to_owned())?
                    .map_err(|error| format!("system bus: {error}"))
            })
            .map_err(SystemdError::Unavailable)?;
        Ok(Self {
            runtime,
            connection,
        })
    }

    fn manager(&self) -> Result<zbus_systemd::systemd1::ManagerProxy<'_>, SystemdError> {
        self.runtime
            .block_on(async {
                tokio::time::timeout(
                    DBUS_TIMEOUT,
                    zbus_systemd::systemd1::ManagerProxy::new(&self.connection),
                )
                .await
                .map_err(|_| SystemdError::Timeout("manager proxy".into()))?
                .map_err(|error| SystemdError::Unavailable(format!("systemd manager: {error}")))
            })
            .map_err(|error| match error {
                SystemdError::Timeout(what) => SystemdError::Timeout(what),
                other => other,
            })
    }

    fn call<R>(
        &self,
        what: &str,
        work: impl std::future::Future<Output = Result<R, SystemdError>>,
    ) -> Result<R, SystemdError> {
        self.runtime.block_on(async {
            tokio::time::timeout(DBUS_TIMEOUT, work)
                .await
                .map_err(|_| SystemdError::Timeout(what.to_owned()))?
        })
    }

    fn probe(&mut self) -> Result<(), SystemdError> {
        let proxy = self.manager()?;
        self.call("probe", async {
            proxy
                .version()
                .await
                .map(|_| ())
                .map_err(|error| SystemdError::Unavailable(format!("systemd version: {error}")))
        })
    }

    fn changes(
        raw: Vec<(String, String, String)>,
        unit: &str,
    ) -> Result<Vec<UnitChange>, SystemdError> {
        if raw.len() > 64 {
            return Err(SystemdError::Ambiguous {
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
    fn reload(&mut self) -> Result<(), SystemdError> {
        let proxy = self.manager()?;
        self.call("reload", async {
            proxy
                .reload()
                .await
                .map_err(|error| SystemdError::Unavailable(format!("reload: {error}")))
        })
    }

    fn unit_file_state(&mut self, unit: &str) -> Result<String, SystemdError> {
        let proxy = self.manager()?;
        let unit = unit.to_owned();
        self.call("unit-file-state", async {
            proxy
                .get_unit_file_state(unit.clone())
                .await
                .map_err(|error| SystemdError::Refused {
                    unit: unit.clone(),
                    reason: format!("unit-file state: {error}"),
                })
        })
    }

    fn load_unit(&mut self, unit: &str) -> Result<UnitInfo, SystemdError> {
        let proxy = self.manager()?;
        let unit = unit.to_owned();
        let path = self.call("load-unit", async {
            proxy
                .load_unit(unit.clone())
                .await
                .map_err(|error| SystemdError::Refused {
                    unit: unit.clone(),
                    reason: format!("load unit: {error}"),
                })
        })?;
        let info = self.call("unit-properties", async {
            let unit_proxy = zbus_systemd::systemd1::UnitProxy::new(&self.connection, path.clone())
                .await
                .map_err(|error| SystemdError::Refused {
                    unit: unit.clone(),
                    reason: format!("unit proxy: {error}"),
                })?;
            let (load_state, fragment_path, unit_file_state) = tokio::join!(
                unit_proxy.load_state(),
                unit_proxy.fragment_path(),
                unit_proxy.unit_file_state(),
            );
            Ok::<_, SystemdError>(UnitInfo {
                load_state: load_state.map_err(|error| SystemdError::Refused {
                    unit: unit.clone(),
                    reason: format!("load state: {error}"),
                })?,
                fragment_path: fragment_path.map_err(|error| SystemdError::Refused {
                    unit: unit.clone(),
                    reason: format!("fragment path: {error}"),
                })?,
                unit_file_state: unit_file_state.map_err(|error| SystemdError::Refused {
                    unit: unit.clone(),
                    reason: format!("unit-file state: {error}"),
                })?,
            })
        })?;
        Ok(info)
    }

    fn enable(&mut self, unit: &str) -> Result<Vec<UnitChange>, SystemdError> {
        let proxy = self.manager()?;
        let unit = unit.to_owned();
        self.call("enable", async {
            let (carries_install_info, changes) = proxy
                .enable_unit_files(vec![unit.clone()], false, false)
                .await
                .map_err(|error| SystemdError::Refused {
                    unit: unit.clone(),
                    reason: format!("enable: {error}"),
                })?;
            let _ = carries_install_info;
            Self::changes(changes, &unit)
        })
    }

    fn disable(&mut self, unit: &str) -> Result<Vec<UnitChange>, SystemdError> {
        let proxy = self.manager()?;
        let unit = unit.to_owned();
        self.call("disable", async {
            let changes = proxy
                .disable_unit_files(vec![unit.clone()], false)
                .await
                .map_err(|error| SystemdError::Refused {
                    unit: unit.clone(),
                    reason: format!("disable: {error}"),
                })?;
            Self::changes(changes, &unit)
        })
    }

    fn mask(&mut self, unit: &str) -> Result<Vec<UnitChange>, SystemdError> {
        let proxy = self.manager()?;
        let unit = unit.to_owned();
        self.call("mask", async {
            let changes = proxy
                .mask_unit_files(vec![unit.clone()], false, false)
                .await
                .map_err(|error| SystemdError::Refused {
                    unit: unit.clone(),
                    reason: format!("mask: {error}"),
                })?;
            Self::changes(changes, &unit)
        })
    }

    fn unmask(&mut self, unit: &str) -> Result<Vec<UnitChange>, SystemdError> {
        let proxy = self.manager()?;
        let unit = unit.to_owned();
        self.call("unmask", async {
            let changes = proxy
                .unmask_unit_files(vec![unit.clone()], false)
                .await
                .map_err(|error| SystemdError::Refused {
                    unit: unit.clone(),
                    reason: format!("unmask: {error}"),
                })?;
            Self::changes(changes, &unit)
        })
    }
}

/// Deterministic in-memory manager for tests and failure injection.
///
/// Models persistent unit-file state per unit (`enabled`, `disabled`,
/// `masked`, ...) plus load state and fragment path, with scriptable
/// failures: each operation can be failed once or always, and replies can
/// be "lost" (applied but reported as an error) to prove reconciliation.
#[derive(Debug, Default, Clone)]
pub struct FakeSystemd {
    units: BTreeMap<String, FakeUnit>,
    /// Fail the next call to `operation` with `message`.
    pub fail_next: BTreeMap<String, String>,
    /// Fail every call to `operation` with `message`.
    pub fail_always: BTreeMap<String, String>,
    /// Apply the mutation but report failure, for lost-reply tests.
    pub lose_reply: BTreeMap<String, String>,
    /// Unexpected extra enablement links, for ownership tests.
    pub extra_links: BTreeMap<String, Vec<String>>,
    pub reloads: usize,
}

#[derive(Debug, Clone)]
struct FakeUnit {
    state: String,
    load_state: String,
    fragment_path: String,
}

impl FakeSystemd {
    /// Seed one unit's persistent state.
    pub fn seed(&mut self, unit: &str, state: &str, fragment_path: &str) {
        self.units.insert(
            unit.to_owned(),
            FakeUnit {
                state: state.to_owned(),
                load_state: if state == "masked" {
                    "masked".into()
                } else {
                    "loaded".into()
                },
                fragment_path: fragment_path.to_owned(),
            },
        );
    }

    /// Seed one unit's fragment path without touching its persistent
    /// state: repeated snapshots across versions must not reset policy.
    pub fn seed_fragment(&mut self, unit: &str, fragment_path: &str) {
        let entry = self.units.entry(unit.to_owned()).or_insert(FakeUnit {
            state: "disabled".into(),
            load_state: "loaded".into(),
            fragment_path: String::new(),
        });
        entry.fragment_path = fragment_path.to_owned();
    }

    fn fail(&mut self, operation: &str, unit: &str) -> Option<SystemdError> {
        if let Some(message) = self.fail_next.remove(operation) {
            return Some(SystemdError::Unavailable(format!(
                "{operation} {unit}: {message}"
            )));
        }
        if let Some(message) = self.fail_always.get(operation) {
            return Some(SystemdError::Unavailable(format!(
                "{operation} {unit}: {message}"
            )));
        }
        None
    }

    fn lost(&mut self, operation: &str) -> bool {
        self.lose_reply.remove(operation).is_some()
    }

    fn entry(&mut self, unit: &str) -> &mut FakeUnit {
        self.units.entry(unit.to_owned()).or_insert(FakeUnit {
            state: "disabled".into(),
            load_state: "loaded".into(),
            fragment_path: String::new(),
        })
    }
}

impl SystemdManager for FakeSystemd {
    fn reload(&mut self) -> Result<(), SystemdError> {
        if let Some(error) = self.fail("reload", "") {
            return Err(error);
        }
        self.reloads += 1;
        if self.lost("reload") {
            return Err(SystemdError::Unavailable("reload reply lost".into()));
        }
        Ok(())
    }

    fn unit_file_state(&mut self, unit: &str) -> Result<String, SystemdError> {
        if let Some(error) = self.fail("unit_file_state", unit) {
            return Err(error);
        }
        Ok(self
            .units
            .get(unit)
            .map(|entry| entry.state.clone())
            .unwrap_or_else(|| "disabled".to_owned()))
    }

    fn load_unit(&mut self, unit: &str) -> Result<UnitInfo, SystemdError> {
        if let Some(error) = self.fail("load_unit", unit) {
            return Err(error);
        }
        let entry = self.units.get(unit);
        Ok(UnitInfo {
            load_state: entry
                .map(|entry| entry.load_state.clone())
                .unwrap_or_else(|| "not-found".to_owned()),
            fragment_path: entry
                .map(|entry| entry.fragment_path.clone())
                .unwrap_or_default(),
            unit_file_state: entry
                .map(|entry| entry.state.clone())
                .unwrap_or_else(|| "disabled".to_owned()),
        })
    }

    fn enable(&mut self, unit: &str) -> Result<Vec<UnitChange>, SystemdError> {
        if let Some(error) = self.fail("enable", unit) {
            return Err(error);
        }
        let entry = self.entry(unit);
        entry.state = "enabled".into();
        entry.load_state = "loaded".into();
        let changes = vec![
            UnitChange::bound(
                "symlink",
                &format!("/etc/systemd/system/multi-user.target.wants/{unit}"),
                &format!("/usr/local/lib/systemd/system/{unit}"),
            )
            .expect("static change bounds"),
        ];
        if self.lost("enable") {
            return Err(SystemdError::Unavailable("enable reply lost".into()));
        }
        Ok(changes)
    }

    fn disable(&mut self, unit: &str) -> Result<Vec<UnitChange>, SystemdError> {
        if let Some(error) = self.fail("disable", unit) {
            return Err(error);
        }
        if let Some(extra) = self.extra_links.get(unit)
            && !extra.is_empty()
        {
            return Err(SystemdError::Ambiguous {
                unit: unit.to_owned(),
                reason: format!(
                    "refusing broad disable: unrelated enablement exists: {}",
                    extra.join(", ")
                ),
            });
        }
        let entry = self.entry(unit);
        entry.state = "disabled".into();
        let changes = vec![
            UnitChange::bound(
                "unlink",
                &format!("/etc/systemd/system/multi-user.target.wants/{unit}"),
                "",
            )
            .expect("static change bounds"),
        ];
        if self.lost("disable") {
            return Err(SystemdError::Unavailable("disable reply lost".into()));
        }
        Ok(changes)
    }

    fn mask(&mut self, unit: &str) -> Result<Vec<UnitChange>, SystemdError> {
        if let Some(error) = self.fail("mask", unit) {
            return Err(error);
        }
        let entry = self.entry(unit);
        entry.state = "masked".into();
        entry.load_state = "masked".into();
        let changes = vec![
            UnitChange::bound(
                "symlink",
                &format!("/etc/systemd/system/{unit}"),
                "/dev/null",
            )
            .expect("static change bounds"),
        ];
        if self.lost("mask") {
            return Err(SystemdError::Unavailable("mask reply lost".into()));
        }
        Ok(changes)
    }

    fn unmask(&mut self, unit: &str) -> Result<Vec<UnitChange>, SystemdError> {
        if let Some(error) = self.fail("unmask", unit) {
            return Err(error);
        }
        let entry = self.entry(unit);
        if entry.state == "masked" {
            entry.state = "disabled".into();
            entry.load_state = "loaded".into();
        }
        let changes = vec![
            UnitChange::bound("unlink", &format!("/etc/systemd/system/{unit}"), "")
                .expect("static change bounds"),
        ];
        if self.lost("unmask") {
            return Err(SystemdError::Unavailable("unmask reply lost".into()));
        }
        Ok(changes)
    }
}

/// A shareable fake manager for multi-step tests: every executor borrows
/// the same underlying table, so install, upgrade, repair, and uninstall
/// steps observe one continuous systemd state.
#[derive(Debug, Clone, Default)]
pub struct SharedFakeSystemd(std::rc::Rc<std::cell::RefCell<FakeSystemd>>);

impl SharedFakeSystemd {
    /// Observe the current table.
    pub fn borrow(&self) -> std::cell::Ref<'_, FakeSystemd> {
        self.0.borrow()
    }

    /// Mutate the current table (seeding, failure injection).
    pub fn borrow_mut(&self) -> std::cell::RefMut<'_, FakeSystemd> {
        self.0.borrow_mut()
    }
}

impl SystemdManager for SharedFakeSystemd {
    fn reload(&mut self) -> Result<(), SystemdError> {
        self.0.borrow_mut().reload()
    }

    fn unit_file_state(&mut self, unit: &str) -> Result<String, SystemdError> {
        self.0.borrow_mut().unit_file_state(unit)
    }

    fn load_unit(&mut self, unit: &str) -> Result<UnitInfo, SystemdError> {
        self.0.borrow_mut().load_unit(unit)
    }

    fn enable(&mut self, unit: &str) -> Result<Vec<UnitChange>, SystemdError> {
        self.0.borrow_mut().enable(unit)
    }

    fn disable(&mut self, unit: &str) -> Result<Vec<UnitChange>, SystemdError> {
        self.0.borrow_mut().disable(unit)
    }

    fn mask(&mut self, unit: &str) -> Result<Vec<UnitChange>, SystemdError> {
        self.0.borrow_mut().mask(unit)
    }

    fn unmask(&mut self, unit: &str) -> Result<Vec<UnitChange>, SystemdError> {
        self.0.borrow_mut().unmask(unit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn fake_refuses_broad_disable_with_unrelated_links() {
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
        assert!(fake.disable("zup-a.service").is_err());
        assert_eq!(fake.unit_file_state("zup-a.service").unwrap(), "enabled");
    }

    #[test]
    fn changes_are_bounded() {
        assert!(UnitChange::bound("symlink", "a", &"x".repeat(2048)).is_err());
        assert!(UnitChange::bound("symlink", "a\nb", "c").is_err());
    }
}
