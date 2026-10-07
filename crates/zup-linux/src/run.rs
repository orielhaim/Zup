//! Running a Linux lifecycle, in this process.
//!
//! One function per verb, one path through all of them: open the carrier (which
//! verifies everything before anything is mutated), resolve the plan, prove the
//! capabilities, take the installation lock, snapshot the machine, plan the
//! lifecycle against the ledger, compile the transaction, run it through the
//! real coordinator, and publish the ledger. There is no worker, no privilege
//! escalation, and no second process: user scope means this process can own
//! every directory it touches.
//!
//! Maintenance is a file in the transaction, not a copy made afterwards. The
//! installer image running right now is appended to the plan as a
//! maintenance-keyed payload whose destination is the versioned maintenance
//! path, so repair, uninstall, and upgrade handle it through receipts like
//! every other file - and a machine that has lost the original download is
//! still repairable from the copy it owns.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use zup_bundle::{PackagePayloadSource, PayloadError, PayloadReader, PayloadSource};
use zup_core::{AppId, RelativePath, ResourceKey, SelectedScope, Sha256Digest};
use zup_exec::{InstallLedger, LifecycleAction, OwnedResource, plan_lifecycle};
use zup_platform::{TargetFile, TargetPlan};
use zup_transaction::{
    FilesystemTransactionStore, InstallationLock, TransactionCoordinator, TransactionOutcome,
    TransactionStore,
};

use crate::capabilities::validate_target_plan;
use crate::carrier::{Carrier, CarrierError};
use crate::executor::{LinuxFileExecutor, LinuxFileExecutorError};
use crate::input::{LinuxInputError, compile_execution_plan};
use crate::integration::{load_generated, save_generated};
use crate::ledger::{LinuxLedgerError, LinuxLedgerStore};
use crate::resolve::{LinuxResolveError, resolve_target};
use crate::snapshot::snapshot_target;
use crate::state::state_root;

/// The reserved payload name of the maintenance copy: this image itself.
///
/// Extensionless, because Linux executables are. It is a name only the runner
/// serves - no package carries it - so a payload that claimed it would be a
/// collision the package reader refuses before this is ever consulted.
const MAINTENANCE_SOURCE: &str = "__zup_maintenance__";

/// Which lifecycle to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinuxAction {
    Install,
    Upgrade,
    /// Repair owned files: missing ones always come back; damaged ones only
    /// with `force_files`, because a present-but-different file may be a user
    /// edit rather than damage, and overwriting it silently would be the
    /// installer deciding the user's bytes are wrong.
    Repair {
        force_files: bool,
    },
    Uninstall,
    /// Resolve from the ledger: absent means install, a older record means
    /// upgrade, the same version means repair without force.
    Apply,
}

/// What one run did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinuxOutcome {
    Committed {
        version: semver::Version,
    },
    RolledBack,
    RecoveryRequired {
        transaction: String,
    },
    /// Another process holds this installation's lock.
    Busy,
}

/// Everything one run needs.
#[derive(Debug, Clone)]
pub struct LinuxRunRequest {
    /// The installer image to open: the carrier whose package installs.
    ///
    /// Normally the executable running right now, which is also what makes it
    /// the maintenance source. A test may point it at any composed installer.
    pub installer: PathBuf,
    pub scope: SelectedScope,
    /// An explicit state root, for isolated environments. `None` resolves the
    /// scope's own root, which is the only correct answer outside a test.
    pub state_root: Option<PathBuf>,
    pub action: LinuxAction,
}

/// Why a Linux lifecycle could not run.
#[derive(Debug, thiserror::Error)]
pub enum LinuxRunError {
    #[error("machine scope is not supported on Linux in this phase")]
    MachineScope,

    #[error("installer package: {0}")]
    Carrier(#[from] CarrierError),

    #[error("package: {0}")]
    Package(#[from] zup_bundle::PackageError),

    #[error("plan: {0}")]
    Plan(#[from] zup_plan::PlanError),

    #[error("target resolution: {0}")]
    Resolve(#[from] LinuxResolveError),

    #[error("capabilities: {0}")]
    Capabilities(#[from] crate::capabilities::LinuxCapabilityError),

    #[error("ledger: {0}")]
    Ledger(#[from] LinuxLedgerError),

    #[error("lock: {0}")]
    Lock(#[from] zup_transaction::LockError),

    #[error("lifecycle: {0}")]
    Lifecycle(#[from] zup_exec::LifecycleError),

    #[error("transaction input: {0}")]
    Input(#[from] LinuxInputError),

    #[error("integration: {0}")]
    Integration(#[from] crate::integration::IntegrationError),

    #[error("transaction plan: {0}")]
    Compile(#[from] zup_transaction::TransactionPlanError),

    #[error("transaction store: {0}")]
    Store(#[from] zup_transaction::StoreError),

    #[error("transaction executor: {0}")]
    Executor(String),

    #[error("transaction coordination: {0}")]
    Coordinator(#[from] zup_transaction::TransactionError),

    #[error("downgrade from {installed} to {requested} is refused")]
    Downgrade {
        installed: semver::Version,
        requested: semver::Version,
    },

    #[error("an installer package holds exactly one target; this one holds {count}")]
    MultipleTargets { count: usize },

    #[error("refused path `{path}`: {reason}")]
    RefusedPath { path: String, reason: String },

    #[error("i/o at `{path}`: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

impl From<LinuxFileExecutorError> for LinuxRunError {
    fn from(error: LinuxFileExecutorError) -> Self {
        Self::Executor(error.to_string())
    }
}

/// Run one Linux lifecycle to a stable outcome.
pub fn run(request: &LinuxRunRequest) -> Result<LinuxOutcome, LinuxRunError> {
    if request.scope != SelectedScope::User {
        return Err(LinuxRunError::MachineScope);
    }
    // The carrier verifies everything - footer, digest, package, target -
    // before any path below could be acted on.
    let carrier = Carrier::open(&request.installer)?;
    let mut targets = carrier.package().build_plan()?.targets;
    if targets.len() != 1 {
        return Err(LinuxRunError::MultipleTargets {
            count: targets.len(),
        });
    }
    let build = targets.remove(0);

    let state_root = match &request.state_root {
        Some(root) => root.clone(),
        None => state_root(request.scope).map_err(|error| LinuxRunError::Io {
            path: "<state root>".into(),
            source: std::io::Error::other(error.to_string()),
        })?,
    };
    let ledger_store = LinuxLedgerStore::new(&state_root);
    // The state hierarchy is zup's own bookkeeping. If any existing part of
    // it is reached through a symbolic link, the journal, the ledger, and the
    // lock would all live wherever the link points - so the hierarchy is
    // refused before anything reads or writes through it. Ancestors are
    // checked by path; the root's own entries are checked by listing, because
    // a redirect planted *inside* the root (transactions/, maintenance/) is a
    // child, not an ancestor, and a prefix walk never sees it.
    refuse_redirected_hierarchy(&state_root)?;
    // A machine that crashed between commit and publish has a journal that
    // says more than its ledger does. Closing that gap - or refusing when an
    // unfinished transaction needs recovery first - happens before any new
    // planning, because planning against a stale ledger plans the wrong
    // transition.
    ledger_store.repair_committed(&build.installer.app.id, request.scope)?;

    let plan_request = zup_plan::PlanRequest::new(build.installer.target.clone(), request.scope);
    let install = zup_plan::plan(
        &zup_plan::BuildPlan {
            targets: vec![build],
        },
        &plan_request,
    )?;
    let mut target = resolve_target(&install)?;
    attach_maintenance_copy(&mut target, &request.installer, &state_root, request.scope)?;
    validate_target_plan(&target)?;
    // Same rule for where the payload goes: an install directory reached
    // through a link would land the application wherever the link points.
    // The executor still enforces its own per-operation refusals below; this
    // is the up-front statement that the destination tree is what it claims.
    if let Ok(host) = crate::lowering::to_host_path(&target.install_directory) {
        crate::fs::refuse_symlink_ancestors(&host).map_err(|error| match error {
            crate::fs::FileSystemError::UnexpectedKind { path, expected } => {
                LinuxRunError::RefusedPath {
                    path,
                    reason: format!(
                        "{expected}; the install destination must not pass through a link"
                    ),
                }
            }
            other => LinuxRunError::Executor(other.to_string()),
        })?;
    }

    let ledger = ledger_store.load(&target.app.id, request.scope)?;
    let action = resolve_action(request.action, ledger.as_ref(), &target.app.version)?;

    // One installation, one writer. The identity is the portable
    // (application, scope) pair, not a PID file and not a Linux-only key: two
    // backends disagreeing about the lock would mean two installers writing
    // one installation at once.
    let lock_key = InstallationLock::lock_key(target.app.id.as_str(), scope_token(request.scope));
    let _lock = match InstallationLock::try_acquire(&state_root, &lock_key)? {
        Some(lock) => lock,
        None => return Ok(LinuxOutcome::Busy),
    };

    let snapshot = snapshot_target(&target);
    let owned_matches = inspect_owned_matches(ledger.as_ref());
    let execution = plan_lifecycle(
        action,
        (action != LifecycleAction::Uninstall).then_some(&target),
        Some(&snapshot),
        ledger.as_ref(),
        &owned_matches,
    )?;
    let input = compile_execution_plan(&execution, &target)?;
    let plan = zup_transaction::compile_transaction(&input)?;
    ledger_store.validate_plan(&target.app.id, request.scope, &target.app.version, &plan)?;
    // Required tools resolve before anything mutates: installing the files
    // and then discovering the database cannot be regenerated is exactly the
    // half-installation the capability boundary exists to prevent.
    preflight_refresh(&plan)?;
    // Generated integration bytes are rendered from the manifest, not carried
    // by the package, so they are persisted beside the state a recovery run
    // can always read. The render is deterministic, so what recovery serves
    // is what this run planned.
    let generated = crate::integration::generated_map(&install)?;
    save_generated(&state_root, &target.app.id, request.scope, &generated)?;
    let generated = load_generated(&state_root, &target.app.id, request.scope)?;

    let store = FilesystemTransactionStore::new(&state_root);
    let coordinator = TransactionCoordinator::new(store);
    let record = coordinator.begin(
        target.app.id.clone(),
        request.scope,
        target.app.version.clone(),
        plan.clone(),
    )?;
    let payload = RunnerPayload::open(&carrier, &request.installer, generated)?;
    let mut executor = LinuxFileExecutor::new().with_payload(payload);
    executor.register_plan(&plan)?;
    let (record, outcome) = coordinator.execute(record, &mut executor)?;

    match outcome {
        TransactionOutcome::Committed => {
            let ledger = ledger_store.publish_committed(&record, request.scope)?;
            if action == LifecycleAction::Upgrade {
                retire_old_generations(
                    &state_root,
                    &target.app.id,
                    request.scope,
                    &ledger.version,
                )?;
            }
            if action == LifecycleAction::Uninstall {
                // The installation is gone, so its lock marker goes with it -
                // but only while nobody holds it, so a concurrent run cannot
                // lose its exclusion under it.
                let _ = InstallationLock::remove_if_unheld(&state_root, &lock_key);
            }
            Ok(LinuxOutcome::Committed {
                version: ledger.version,
            })
        }
        TransactionOutcome::RolledBack => {
            // File rollbacks restore the authoritative sources; the derived
            // databases are regenerated from the restored state here, because
            // the transaction graph rolls backend nodes back before the files
            // they derive from.
            sweep_refresh(&record)?;
            Ok(LinuxOutcome::RolledBack)
        }
        TransactionOutcome::RecoveryRequired => Ok(LinuxOutcome::RecoveryRequired {
            transaction: record.transaction_id.to_string(),
        }),
    }
}

/// Recover one interrupted transaction from the journal alone.
///
/// The payload comes from the maintenance copy's embedded package - the copy
/// the installation owns - never from the original download, which may be
/// long gone. A recovery that needed the download would make the maintenance
/// copy pointless.
pub fn recover_transaction(
    state_root: &Path,
    app_id: &AppId,
    scope: SelectedScope,
    transaction: &zup_transaction::TransactionId,
) -> Result<LinuxOutcome, LinuxRunError> {
    if scope != SelectedScope::User {
        return Err(LinuxRunError::MachineScope);
    }
    // Recovery reads the journal and replays it; a redirected hierarchy
    // would have it read and replay somebody else's record.
    refuse_redirected_hierarchy(state_root)?;
    let store = FilesystemTransactionStore::new(state_root);
    let record = store.load(transaction)?;
    if record.app_id != *app_id || record.scope != scope {
        return Err(LinuxRunError::Ledger(LinuxLedgerError::Ownership(
            "transaction identity".into(),
        )));
    }
    let maintenance = zup_transaction::maintenance_runtime_path(
        state_root,
        app_id,
        scope,
        &record.app_version,
        record.target.executable_suffix(),
    );
    let carrier = Carrier::open(&maintenance)?;
    let generated = load_generated(state_root, app_id, scope)?;
    let payload = RunnerPayload::open(&carrier, carrier.executable(), generated)?;
    let lock_key = InstallationLock::lock_key(app_id.as_str(), scope_token(scope));
    let _lock = match InstallationLock::try_acquire(state_root, &lock_key)? {
        Some(lock) => lock,
        None => return Ok(LinuxOutcome::Busy),
    };
    let mut executor = LinuxFileExecutor::new().with_payload(payload);
    executor.register_plan(&record.plan)?;
    preflight_refresh(&record.plan)?;
    let (record, outcome) = zup_transaction::recover(record, &store, &mut executor)?;
    match outcome {
        TransactionOutcome::Committed => {
            LinuxLedgerStore::new(state_root).publish_committed(&record, scope)?;
            Ok(LinuxOutcome::Committed {
                version: record.app_version.clone(),
            })
        }
        TransactionOutcome::RolledBack => {
            sweep_refresh(&record)?;
            Ok(LinuxOutcome::RolledBack)
        }
        TransactionOutcome::RecoveryRequired => Ok(LinuxOutcome::RecoveryRequired {
            transaction: record.transaction_id.to_string(),
        }),
    }
}

/// Refuse a state hierarchy that passes through a symbolic link.
///
/// Two halves: the root's ancestors by prefix walk, and the root's own
/// entries by listing. Zup never stores a symlink directly under its state
/// root - journals, ledgers, generations, and lock files are all real files
/// and directories - so a link there is either planted or corrupt, and either
/// way it is not followed.
fn refuse_redirected_hierarchy(state_root: &Path) -> Result<(), LinuxRunError> {
    let refused = |path: PathBuf, what: &str| LinuxRunError::RefusedPath {
        path: path.display().to_string(),
        reason: format!("{what}; the state hierarchy must not pass through a link"),
    };
    crate::fs::refuse_symlink_ancestors(state_root).map_err(|error| match error {
        crate::fs::FileSystemError::UnexpectedKind { path, expected } => {
            LinuxRunError::RefusedPath {
                path,
                reason: format!("{expected}; the state hierarchy must not pass through a link"),
            }
        }
        other => LinuxRunError::Executor(other.to_string()),
    })?;
    let entries = match std::fs::read_dir(state_root) {
        Ok(entries) => entries,
        // Absent is the fresh-machine case, not a redirect.
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(LinuxRunError::Io {
                path: state_root.display().to_string(),
                source,
            });
        }
    };
    for entry in entries {
        let entry = entry.map_err(|source| LinuxRunError::Io {
            path: state_root.display().to_string(),
            source,
        })?;
        let file_type = entry.file_type().map_err(|source| LinuxRunError::Io {
            path: entry.path().display().to_string(),
            source,
        })?;
        if file_type.is_symlink() {
            return Err(refused(entry.path(), "a state entry is a symbolic link"));
        }
    }
    Ok(())
}

/// Regenerate every derived database a transaction's refresh nodes name.
///
/// Runs after rollback, when the authoritative sources are restored but the
/// transaction graph has already rolled its refresh nodes back. Idempotent by
/// nature: regenerating from current sources can only converge.
///
/// The sweep regenerates from the resulting world even when that world holds
/// no source: a rollback that removed the final package source changed the
/// authoritative state to the empty one, and the derived cache must follow it
/// there rather than keep the removed entries. Only an absent database
/// directory means there is nowhere stale to converge, and only then is a
/// refresh skipped.
fn sweep_refresh(record: &zup_transaction::TransactionRecord) -> Result<(), LinuxRunError> {
    for node in &record.plan.nodes {
        let zup_transaction::NodeKind::BackendOperation { .. } = &node.kind else {
            continue;
        };
        let Some(backend) = &node.meta.backend else {
            continue;
        };
        let request =
            crate::refresh::RefreshRequest::decode(&backend.payload).map_err(|error| {
                LinuxRunError::Executor(format!("invalid refresh payload: {error}"))
            })?;
        if !crate::refresh::database_present(&request) {
            continue;
        }
        crate::refresh::ensure_refreshable(&request).map_err(LinuxFileExecutorError::from)?;
        crate::refresh::run_refresh(&request).map_err(LinuxFileExecutorError::from)?;
    }
    Ok(())
}

/// Preflight every refresh a plan holds, before anything mutates.
///
/// The coordinator only prepares barriers, so backend preflight cannot live
/// in the executor: a missing tool must fail the run here, with the payload
/// and integration sources still untouched, rather than halfway through.
fn preflight_refresh(plan: &zup_transaction::TransactionPlan) -> Result<(), LinuxRunError> {
    for node in &plan.nodes {
        let zup_transaction::NodeKind::BackendOperation { .. } = &node.kind else {
            continue;
        };
        let Some(backend) = &node.meta.backend else {
            return Err(LinuxRunError::Executor(format!(
                "a refresh node without its request: {}",
                node.id
            )));
        };
        let request =
            crate::refresh::RefreshRequest::decode(&backend.payload).map_err(|error| {
                LinuxRunError::Executor(format!("invalid refresh payload: {error}"))
            })?;
        crate::refresh::preflight(&request).map_err(LinuxFileExecutorError::from)?;
    }
    Ok(())
}

/// The scope token the portable lock identity is keyed on.
fn scope_token(scope: SelectedScope) -> &'static str {
    match scope {
        SelectedScope::User => "user",
        SelectedScope::Machine => "machine",
    }
}

/// Resolve an `Apply` against the machine's own record.
///
/// Absent means install. An older record means upgrade. The same version
/// means repair: running the same installer twice must converge rather than
/// fail, and repair is what convergence is called. Anything newer is a
/// downgrade, refused rather than installed over.
fn resolve_action(
    action: LinuxAction,
    ledger: Option<&InstallLedger>,
    version: &semver::Version,
) -> Result<LifecycleAction, LinuxRunError> {
    let explicit = match action {
        LinuxAction::Install => LifecycleAction::Install,
        LinuxAction::Upgrade => LifecycleAction::Upgrade,
        LinuxAction::Repair { force_files } => LifecycleAction::Repair { force_files },
        LinuxAction::Uninstall => LifecycleAction::Uninstall,
        LinuxAction::Apply => {
            let Some(ledger) = ledger else {
                return Ok(LifecycleAction::Install);
            };
            match ledger.version.cmp(version) {
                std::cmp::Ordering::Less => LifecycleAction::Upgrade,
                std::cmp::Ordering::Equal => LifecycleAction::Repair { force_files: false },
                std::cmp::Ordering::Greater => {
                    return Err(LinuxRunError::Downgrade {
                        installed: ledger.version.clone(),
                        requested: version.clone(),
                    });
                }
            }
        }
    };
    Ok(explicit)
}

/// Append this running image to the plan as the installation's maintenance copy.
///
/// The bytes are the installer's own executable, read once here and served to
/// the transaction from memory: beside it is what a previous run left behind,
/// and planning out of that would be planning an install from the output of an
/// install that may have been rolled back. The file carries executable intent,
/// because a maintenance copy nothing can run maintains nothing.
fn attach_maintenance_copy(
    target: &mut TargetPlan,
    installer: &Path,
    state_root: &Path,
    scope: SelectedScope,
) -> Result<(), LinuxRunError> {
    let bytes = std::fs::read(installer).map_err(|source| LinuxRunError::Io {
        path: installer.display().to_string(),
        source,
    })?;
    let (size, sha256) =
        zup_core::hash_reader(bytes.as_slice()).map_err(|source| LinuxRunError::Io {
            path: installer.display().to_string(),
            source,
        })?;
    let destination = zup_platform::TargetPath::new(
        target.target.clone(),
        zup_transaction::maintenance_runtime_path(
            state_root,
            &target.app.id,
            scope,
            &target.app.version,
            target.target.executable_suffix(),
        )
        .to_string_lossy(),
    )
    .map_err(|error| {
        LinuxRunError::Resolve(LinuxResolveError::InvalidPath {
            kind: "maintenance destination",
            path: state_root.display().to_string(),
            reason: error.to_string(),
        })
    })?;
    target.files.push(TargetFile {
        key: ResourceKey::Maintenance {
            app_id: target.app.id.to_string(),
            version: target.app.version.to_string(),
            destination: destination.to_string(),
        },
        source_relative: RelativePath::new(MAINTENANCE_SOURCE).expect("a reserved source name"),
        destination,
        size,
        sha256,
        privilege: scope.authorization(),
        executable: true,
    });
    target.summary.file_count += 1;
    Ok(())
}

/// Whether each owned file still holds what the ledger says it owns.
///
/// A read-only observation: the transaction repeats every ownership check
/// immediately before mutation, so this is planning input, not a verdict.
fn inspect_owned_matches(ledger: Option<&InstallLedger>) -> BTreeMap<ResourceKey, bool> {
    let mut matches = BTreeMap::new();
    let Some(ledger) = ledger else {
        return matches;
    };
    for (key, owned) in &ledger.resources {
        let found = match owned {
            OwnedResource::File {
                destination,
                sha256,
                size,
                ..
            } => {
                let Ok(host) = crate::lowering::to_host_path(destination) else {
                    matches.insert(key.clone(), false);
                    continue;
                };
                let Some(parent) = host
                    .parent()
                    .filter(|parent| !parent.as_os_str().is_empty())
                else {
                    matches.insert(key.clone(), false);
                    continue;
                };
                match crate::fs::OwnedDirectory::open(parent) {
                    Ok(directory) => {
                        let name = host
                            .file_name()
                            .map(|name| name.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        match directory.kind_or_absent(&name) {
                            Ok(Some(crate::fs::EntryKind::Regular)) => directory
                                .read_regular(&name)
                                .map(|bytes| {
                                    zup_core::hash_reader(bytes.as_slice()).is_ok_and(
                                        |(found_size, found)| {
                                            found_size == *size && found == *sha256
                                        },
                                    )
                                })
                                .unwrap_or(false),
                            _ => false,
                        }
                    }
                    Err(_) => false,
                }
            }
            // No other owned kind exists on a Linux installation: the
            // capability gate plans none, so the ledger holds none.
            _ => false,
        };
        matches.insert(key.clone(), found);
    }
    matches
}

/// Remove maintenance generations the upgrade replaced.
///
/// Only generations holding nothing the ledger still owns: each one is a
/// directory zup wrote end to end, containing the runtime copy the ledger has
/// since replaced. A generation that has acquired anything else is left alone.
fn retire_old_generations(
    state_root: &Path,
    app_id: &AppId,
    scope: SelectedScope,
    current: &semver::Version,
) -> Result<(), LinuxRunError> {
    let root = zup_transaction::maintenance_root(state_root, app_id, scope);
    let entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(LinuxRunError::Io {
                path: root.display().to_string(),
                source,
            });
        }
    };
    for entry in entries {
        let entry = entry.map_err(|source| LinuxRunError::Io {
            path: root.display().to_string(),
            source,
        })?;
        let keep = entry
            .file_name()
            .to_str()
            .is_some_and(|name| name == current.to_string());
        if keep {
            continue;
        }
        let path = entry.path();
        let is_dir = std::fs::symlink_metadata(&path)
            .map(|metadata| metadata.is_dir())
            .unwrap_or(false);
        if !is_dir {
            continue;
        }
        // End to end zup-owned: the generation directory holds only the
        // runtime copy this backend wrote. Anything else in it means someone
        // put it there, and it stays.
        let owned = std::fs::read_dir(&path)
            .map(|entries| {
                entries.filter_map(|entry| entry.ok()).all(|entry| {
                    entry.file_name().to_str().is_some_and(|name| {
                        name == zup_transaction::MAINTENANCE_RUNTIME_DIRECTORY
                            || name == zup_transaction::MAINTENANCE_PACKAGE_NAME
                            || name == zup_transaction::MAINTENANCE_INDEX_NAME
                    })
                })
            })
            .unwrap_or(false);
        if !owned {
            continue;
        }
        std::fs::remove_dir_all(&path).map_err(|source| LinuxRunError::Io {
            path: path.display().to_string(),
            source,
        })?;
    }
    Ok(())
}

/// The transaction's payload: package content plus this image's own bytes.
///
/// The maintenance copy's source name is served from memory - the bytes read
/// to plan it - and everything else delegates to the package with its
/// verification intact. Two sources, one trait, no special case at the call
/// site.
struct RunnerPayload {
    package: PackagePayloadSource,
    maintenance: Vec<u8>,
    maintenance_sha256: Sha256Digest,
    maintenance_size: u64,
    generated: std::collections::BTreeMap<String, Vec<u8>>,
}

impl RunnerPayload {
    fn open(
        carrier: &Carrier,
        installer: &Path,
        generated: std::collections::BTreeMap<String, Vec<u8>>,
    ) -> Result<Self, LinuxRunError> {
        let maintenance = std::fs::read(installer).map_err(|source| LinuxRunError::Io {
            path: installer.display().to_string(),
            source,
        })?;
        let (size, sha256) =
            zup_core::hash_reader(maintenance.as_slice()).map_err(|source| LinuxRunError::Io {
                path: installer.display().to_string(),
                source,
            })?;
        Ok(Self {
            package: carrier.package().payload_source(),
            maintenance,
            maintenance_sha256: sha256,
            maintenance_size: size,
            generated,
        })
    }
}

impl PayloadSource for RunnerPayload {
    fn open(
        &self,
        path: &zup_core::RelativePath,
        expected_sha256: &Sha256Digest,
        expected_size: u64,
    ) -> Result<PayloadReader, PayloadError> {
        if path.as_str() == MAINTENANCE_SOURCE {
            if *expected_sha256 != self.maintenance_sha256 || expected_size != self.maintenance_size
            {
                return Err(PayloadError::DigestMismatch {
                    path: path.to_string(),
                });
            }
            return Ok(Box::new(std::io::Cursor::new(self.maintenance.clone())));
        }
        if let Some(bytes) = self.generated.get(path.as_str()) {
            let (size, sha256) = zup_core::hash_reader(bytes.as_slice()).map_err(|_| {
                PayloadError::DigestMismatch {
                    path: path.to_string(),
                }
            })?;
            if sha256 != *expected_sha256 || size != expected_size {
                return Err(PayloadError::DigestMismatch {
                    path: path.to_string(),
                });
            }
            return Ok(Box::new(std::io::Cursor::new(bytes.clone())));
        }
        self.package.open(path, expected_sha256, expected_size)
    }
}
