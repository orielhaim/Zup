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

use crate::carrier::Carrier;
use crate::error::{ExecError, PathError};
use crate::executor::{LinuxFileExecutor, snapshot_target};
use crate::input::compile_execution_plan;
use crate::integration::{load_generated, save_generated};
use crate::ledger::LinuxLedgerStore;
use crate::paths::state_root;
use crate::resolve::resolve_target;

const MAINTENANCE_SOURCE: &str = "__zup_maintenance__";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinuxAction {
    Install,
    Upgrade,

    Repair { force_files: bool },
    Uninstall,

    Apply,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinuxOutcome {
    Committed { version: semver::Version },
    RolledBack,
    RecoveryRequired { transaction: String },

    Busy,
}

#[derive(Debug, Clone)]
pub struct LinuxRunRequest {
    pub installer: PathBuf,
    pub scope: SelectedScope,

    pub state_root: Option<PathBuf>,
    pub action: LinuxAction,

    pub install_dir_override: Option<PathBuf>,
}

pub fn run(request: &LinuxRunRequest) -> Result<LinuxOutcome, ExecError> {
    match request.scope {
        SelectedScope::User => run_user(request),
        SelectedScope::Machine => crate::elevate::run_machine(request),
    }
}

fn run_user(request: &LinuxRunRequest) -> Result<LinuxOutcome, ExecError> {
    if request.scope != SelectedScope::User {
        return Err(ExecError::MachineScope);
    }

    let carrier = Carrier::open(&request.installer)?;
    let mut targets = carrier.package().build_plan()?.targets;
    if targets.len() != 1 {
        return Err(ExecError::MultipleTargets {
            count: targets.len(),
        });
    }
    let build = targets.remove(0);

    let state_root = match &request.state_root {
        Some(root) => root.clone(),
        None => state_root(request.scope).map_err(|error| PathError::Io {
            path: "<state root>".into(),
            source: std::io::Error::other(error.to_string()),
        })?,
    };
    let ledger_store = LinuxLedgerStore::new(&state_root);

    refuse_redirected_hierarchy(&state_root)?;

    ledger_store.repair_committed(&build.installer.app.id, request.scope)?;

    let plan_request = zup_plan::PlanRequest::new(build.installer.target.clone(), request.scope);
    let install = zup_plan::plan(
        &zup_plan::BuildPlan {
            targets: vec![build],
        },
        &plan_request,
    )?;
    let mut target = resolve_target(
        &install,
        &crate::locations::LinuxInstallLocationResolver::default(),
    )?;
    attach_maintenance_copy_for(&mut target, &request.installer, &state_root, request.scope)?;

    if let Ok(host) = crate::lowering::to_host_path(&target.install_directory) {
        crate::fs::refuse_symlink_ancestors(&host).map_err(|error| match error {
            crate::error::PathError::UnexpectedKind { path, expected } => {
                ExecError::from(PathError::Refused {
                    path,
                    reason: format!(
                        "{expected}; the install destination must not pass through a link"
                    ),
                })
            }
            other => ExecError::Executor(other.to_string()),
        })?;
    }

    let ledger = ledger_store.load(&target.app.id, request.scope)?;
    let action = resolve_action(request.action, ledger.as_ref(), &target.app.version)?;

    let lock_key = InstallationLock::lock_key(target.app.id.as_str(), scope_token(request.scope));
    let _lock = match InstallationLock::try_acquire(&state_root, &lock_key)? {
        Some(lock) => lock,
        None => return Ok(LinuxOutcome::Busy),
    };

    let snapshot = snapshot_target(&target);
    let owned_matches = inspect_owned_matches_for(ledger.as_ref());
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

    preflight_refresh(&plan)?;

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
                retire_old_generations_for(
                    &state_root,
                    &target.app.id,
                    request.scope,
                    &ledger.version,
                )?;
            }
            if action == LifecycleAction::Uninstall {
                let _ = InstallationLock::remove_if_unheld(&state_root, &lock_key);
            }
            Ok(LinuxOutcome::Committed {
                version: ledger.version,
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

pub fn recover_transaction(
    state_root: &Path,
    app_id: &AppId,
    scope: SelectedScope,
    transaction: &zup_transaction::TransactionId,
) -> Result<LinuxOutcome, ExecError> {
    if scope != SelectedScope::User {
        return Err(ExecError::MachineScope);
    }

    refuse_redirected_hierarchy(state_root)?;
    let store = FilesystemTransactionStore::new(state_root);
    let record = store.load(transaction)?;
    if record.app_id != *app_id || record.scope != scope {
        return Err(ExecError::Ownership("transaction identity".into()));
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

fn refuse_redirected_hierarchy(state_root: &Path) -> Result<(), ExecError> {
    let refused = |path: PathBuf, what: &str| PathError::Refused {
        path: path.display().to_string(),
        reason: format!("{what}; the state hierarchy must not pass through a link"),
    };
    crate::fs::refuse_symlink_ancestors(state_root).map_err(|error| match error {
        crate::error::PathError::UnexpectedKind { path, expected } => {
            ExecError::from(PathError::Refused {
                path,
                reason: format!("{expected}; the state hierarchy must not pass through a link"),
            })
        }
        other => ExecError::Executor(other.to_string()),
    })?;
    let entries = match std::fs::read_dir(state_root) {
        Ok(entries) => entries,

        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(PathError::Io {
                path: state_root.display().to_string(),
                source,
            }
            .into());
        }
    };
    for entry in entries {
        let entry = entry.map_err(|source| PathError::Io {
            path: state_root.display().to_string(),
            source,
        })?;
        let file_type = entry.file_type().map_err(|source| PathError::Io {
            path: entry.path().display().to_string(),
            source,
        })?;
        if file_type.is_symlink() {
            return Err(refused(entry.path(), "a state entry is a symbolic link").into());
        }
    }
    Ok(())
}

fn sweep_refresh(record: &zup_transaction::TransactionRecord) -> Result<(), ExecError> {
    for node in &record.plan.nodes {
        let zup_transaction::NodeKind::BackendOperation { .. } = &node.kind else {
            continue;
        };
        let Some(backend) = &node.meta.backend else {
            continue;
        };
        let request = crate::refresh::RefreshRequest::decode(&backend.payload)
            .map_err(|error| ExecError::Executor(format!("invalid refresh payload: {error}")))?;
        if !crate::refresh::database_present(&request) {
            continue;
        }
        crate::refresh::ensure_refreshable(&request)?;
        crate::refresh::run_refresh(&request)?;
    }
    Ok(())
}

fn preflight_refresh(plan: &zup_transaction::TransactionPlan) -> Result<(), ExecError> {
    for node in &plan.nodes {
        let zup_transaction::NodeKind::BackendOperation { .. } = &node.kind else {
            continue;
        };
        let Some(backend) = &node.meta.backend else {
            return Err(ExecError::Executor(format!(
                "a refresh node without its request: {}",
                node.id
            )));
        };
        let request = crate::refresh::RefreshRequest::decode(&backend.payload)
            .map_err(|error| ExecError::Executor(format!("invalid refresh payload: {error}")))?;
        crate::refresh::preflight(&request)?;
    }
    Ok(())
}

fn scope_token(scope: SelectedScope) -> &'static str {
    match scope {
        SelectedScope::User => "user",
        SelectedScope::Machine => "machine",
    }
}

pub(crate) fn resolve_action(
    action: LinuxAction,
    ledger: Option<&InstallLedger>,
    version: &semver::Version,
) -> Result<LifecycleAction, ExecError> {
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
                    return Err(ExecError::Downgrade {
                        installed: ledger.version.clone(),
                        requested: version.clone(),
                    });
                }
            }
        }
    };
    Ok(explicit)
}

pub(crate) fn attach_maintenance_copy_for(
    target: &mut TargetPlan,
    installer: &Path,
    state_root: &Path,
    scope: SelectedScope,
) -> Result<(), ExecError> {
    let bytes = std::fs::read(installer).map_err(|source| PathError::Io {
        path: installer.display().to_string(),
        source,
    })?;
    let (size, sha256) =
        zup_core::hash_reader(bytes.as_slice()).map_err(|source| PathError::Io {
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
        ExecError::PlanFailure(
            PathError::Invalid {
                kind: "maintenance destination",
                path: state_root.display().to_string(),
                reason: error.to_string(),
            }
            .into(),
        )
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

pub(crate) fn inspect_owned_matches_for(
    ledger: Option<&InstallLedger>,
) -> BTreeMap<ResourceKey, bool> {
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

            _ => false,
        };
        matches.insert(key.clone(), found);
    }
    matches
}

pub(crate) fn retire_old_generations_for(
    state_root: &Path,
    app_id: &AppId,
    scope: SelectedScope,
    current: &semver::Version,
) -> Result<(), ExecError> {
    let root = zup_transaction::maintenance_root(state_root, app_id, scope);
    let entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(PathError::Io {
                path: root.display().to_string(),
                source,
            }
            .into());
        }
    };
    for entry in entries {
        let entry = entry.map_err(|source| PathError::Io {
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
        std::fs::remove_dir_all(&path).map_err(|source| PathError::Io {
            path: path.display().to_string(),
            source,
        })?;
    }
    Ok(())
}

pub(crate) struct RunnerPayload {
    package: PackagePayloadSource,
    maintenance: Vec<u8>,
    maintenance_sha256: Sha256Digest,
    maintenance_size: u64,
    generated: std::collections::BTreeMap<String, Vec<u8>>,
}

impl RunnerPayload {
    pub(crate) fn from_prepared(
        package: PackagePayloadSource,
        maintenance: Vec<u8>,
        maintenance_sha256: Sha256Digest,
        maintenance_size: u64,
        generated: std::collections::BTreeMap<String, Vec<u8>>,
    ) -> Self {
        Self {
            package,
            maintenance,
            maintenance_sha256,
            maintenance_size,
            generated,
        }
    }

    fn open(
        carrier: &Carrier,
        installer: &Path,
        generated: std::collections::BTreeMap<String, Vec<u8>>,
    ) -> Result<Self, ExecError> {
        let maintenance = std::fs::read(installer).map_err(|source| PathError::Io {
            path: installer.display().to_string(),
            source,
        })?;
        let (size, sha256) =
            zup_core::hash_reader(maintenance.as_slice()).map_err(|source| PathError::Io {
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
