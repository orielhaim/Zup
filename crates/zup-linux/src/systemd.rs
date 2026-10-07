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
/// enablement/mask transitions. No start, no stop, no restart, and no
/// broad disable: enablement is retired by removing exactly the owned
/// link, never by asking systemd to delete every symlink it knows about.
pub trait SystemdManager {
    /// Reload systemd's unit configuration after source changes.
    fn reload(&mut self) -> Result<(), SystemdError>;
    /// Persistent unit-file state (`enabled`, `disabled`, `masked`, ...).
    fn unit_file_state(&mut self, unit: &str) -> Result<String, SystemdError>;
    /// Load state plus which fragment systemd resolves for the unit.
    fn load_unit(&mut self, unit: &str) -> Result<UnitInfo, SystemdError>;
    /// The manager's own version string (`Manager.Version`, e.g. `259`).
    fn version(&mut self) -> Result<String, SystemdError>;
    /// Persistently enable (never runtime-only, never forced).
    fn enable(&mut self, unit: &str) -> Result<Vec<UnitChange>, SystemdError>;
    /// Remove exactly the owned enablement link for `unit` — the single
    /// `<wants-dir>/<unit>` symlink pointing at `canonical_source` — and
    /// nothing else. Refuses (leaving everything in place) when the link
    /// is absent it reports no change; when it points anywhere else it is
    /// a conflict. Aliases, `.requires/` links, and runtime links are
    /// never touched; callers prove their absence up front instead.
    fn remove_owned_enablement(
        &mut self,
        unit: &str,
        canonical_source: &str,
    ) -> Result<Vec<UnitChange>, SystemdError>;
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

    /// The directory holding the single enablement link this backend owns.
    fn owned_wants_dir() -> std::path::PathBuf {
        std::path::PathBuf::from("/etc/systemd/system/multi-user.target.wants")
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

    /// Remove exactly the owned enablement link and nothing else.
    ///
    /// Directory-relative throughout: the wants directory is opened once
    /// with `O_NOFOLLOW`, the entry must be a symlink, its target must
    /// name the canonical source byte for byte, and only then is that one
    /// name unlinked. A broad `DisableUnitFiles` would also delete
    /// aliases, `.requires/` links, and anything else systemd knows
    /// about; this deletes one proven name or nothing at all.
    fn remove_owned_enablement(
        &mut self,
        unit: &str,
        canonical_source: &str,
    ) -> Result<Vec<UnitChange>, SystemdError> {
        remove_exact_enablement_link(&Self::owned_wants_dir(), unit, canonical_source)
    }

    fn version(&mut self) -> Result<String, SystemdError> {
        let proxy = self.manager()?;
        self.call("version", async {
            proxy
                .version()
                .await
                .map_err(|error| SystemdError::Unavailable(format!("systemd version: {error}")))
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

/// Remove exactly one proven enablement link from a wants directory.
///
/// The directory is opened once with `O_NOFOLLOW`; the entry must be a
/// symlink; its target must name the canonical source byte for byte; only
/// then is that one name unlinked and the directory flushed. Anything
/// else — absent link (already retired), a link pointing elsewhere, a
/// regular file, an uninspectable directory — refuses or reports no
/// change without touching anything. This is the mechanism behind the
/// manager's owned-link removal, extracted so tests prove it against
/// isolated trees instead of the host's `/etc`.
fn remove_exact_enablement_link(
    wants_dir: &std::path::Path,
    unit: &str,
    canonical_source: &str,
) -> Result<Vec<UnitChange>, SystemdError> {
    let unit = unit.to_owned();
    let link = wants_dir.join(&unit);
    let directory = match crate::fs::OwnedDirectory::open(wants_dir) {
        Ok(directory) => directory,
        Err(error) => {
            // No wants directory means no enablement link: already
            // retired, with nothing removed.
            let missing = std::fs::symlink_metadata(wants_dir)
                .err()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound);
            if missing {
                return Ok(Vec::new());
            }
            return Err(SystemdError::Refused {
                unit,
                reason: format!("owned enablement is not inspectable: {error}"),
            });
        }
    };
    let target = match directory.read_link_target(&unit) {
        Ok(target) => target,
        Err(crate::fs::FileSystemError::Missing { .. }) => return Ok(Vec::new()),
        Err(error) => {
            return Err(SystemdError::Refused {
                unit,
                reason: format!("owned enablement is not inspectable: {error}"),
            });
        }
    };
    if target.to_string_lossy() != canonical_source {
        return Err(SystemdError::Ambiguous {
            unit,
            reason: format!(
                "the owned enablement link points at `{}`, not the Zup source",
                target.display()
            ),
        });
    }
    directory
        .remove_file(&unit)
        .map_err(|error| SystemdError::Refused {
            unit: unit.clone(),
            reason: format!("the owned enablement link cannot be removed: {error}"),
        })?;
    directory.sync().map_err(|error| SystemdError::Refused {
        unit: unit.clone(),
        reason: format!("the wants directory does not flush: {error}"),
    })?;
    let link = link.to_string_lossy().into_owned();
    Ok(vec![UnitChange::bound("unlink", &link, "").map_err(
        |error| SystemdError::Ambiguous {
            unit: unit.clone(),
            reason: error.to_string(),
        },
    )?])
}

/// Deterministic in-memory manager for tests and failure injection.
///
/// Models persistent unit-file state per unit (`enabled`, `disabled`,
/// `masked`, ...) plus load state and fragment path, with scriptable
/// failures: each operation can be failed once or always, and replies can
/// be "lost" (applied but reported as an error) to prove reconciliation.
#[derive(Debug, Clone)]
pub struct FakeSystemd {
    units: BTreeMap<String, FakeUnit>,
    /// The `Manager.Version` string this fake reports.
    pub version: String,
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

impl Default for FakeSystemd {
    fn default() -> Self {
        Self {
            units: BTreeMap::new(),
            // Newer than the `Type=exec` baseline, so existing tests
            // exercise the supported path unless they say otherwise.
            version: "259".into(),
            fail_next: BTreeMap::new(),
            fail_always: BTreeMap::new(),
            lose_reply: BTreeMap::new(),
            extra_links: BTreeMap::new(),
            reloads: 0,
        }
    }
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

    /// Test-only helper simulating an external administrator edit: sets
    /// persistent state unconditionally, bypassing ownership. Production
    /// never calls a broad disable; it removes exactly the owned link.
    pub fn disable(&mut self, unit: &str) -> Result<Vec<UnitChange>, SystemdError> {
        if let Some(error) = self.fail("disable", unit) {
            return Err(error);
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

    fn version(&mut self) -> Result<String, SystemdError> {
        if let Some(error) = self.fail("version", "") {
            return Err(error);
        }
        Ok(self.version.clone())
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

    fn remove_owned_enablement(
        &mut self,
        unit: &str,
        _canonical_source: &str,
    ) -> Result<Vec<UnitChange>, SystemdError> {
        if let Some(error) = self.fail("remove_owned_enablement", unit) {
            return Err(error);
        }
        if let Some(extra) = self.extra_links.get(unit)
            && !extra.is_empty()
        {
            return Err(SystemdError::Ambiguous {
                unit: unit.to_owned(),
                reason: format!(
                    "refusing owned-link removal: unrelated enablement exists: {}",
                    extra.join(", ")
                ),
            });
        }
        let entry = self.entry(unit);
        let removed = entry.state == "enabled" || entry.state == "enabled-runtime";
        entry.state = "disabled".into();
        let changes = removed
            .then(|| {
                UnitChange::bound(
                    "unlink",
                    &format!("/etc/systemd/system/multi-user.target.wants/{unit}"),
                    "",
                )
                .expect("static change bounds")
            })
            .into_iter()
            .collect();
        if self.lost("remove_owned_enablement") {
            return Err(SystemdError::Unavailable(
                "owned-link removal reply lost".into(),
            ));
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

    fn version(&mut self) -> Result<String, SystemdError> {
        self.0.borrow_mut().version()
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

    fn remove_owned_enablement(
        &mut self,
        unit: &str,
        canonical_source: &str,
    ) -> Result<Vec<UnitChange>, SystemdError> {
        self.0
            .borrow_mut()
            .remove_owned_enablement(unit, canonical_source)
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

    #[test]
    fn fake_reports_its_manager_version() {
        let mut fake = FakeSystemd::default();
        assert_eq!(fake.version().unwrap(), "259");
        fake.version = "239".into();
        assert_eq!(fake.version().unwrap(), "239");
    }

    #[test]
    fn changes_are_bounded() {
        assert!(UnitChange::bound("symlink", "a", &"x".repeat(2048)).is_err());
        assert!(UnitChange::bound("symlink", "a\nb", "c").is_err());
    }

    /// The exact-link removal only ever removes the proven owned name:
    /// neighbors, foreign targets, and non-links are preserved, and every
    /// refusal leaves the tree exactly as found.
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

        // Already retired reports no change.
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
