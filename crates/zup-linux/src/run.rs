use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use zup_bundle::{PackagePayloadSource, PayloadError, PayloadReader, PayloadSource};
use zup_core::{AppId, ResourceKey, SelectedScope, Sha256Digest};
use zup_exec::{InstallLedger, OwnedResource};

use crate::carrier::Carrier;
use crate::error::{ExecError, PathError};
use crate::lifecycle::LinuxOutcome;
use zup_session::SessionHost;

pub(crate) const MAINTENANCE_SOURCE: &str = "__zup_maintenance__";

#[derive(Debug, Clone)]
pub struct LinuxRunRequest {
    pub installer: PathBuf,
    pub scope: SelectedScope,

    pub state_root: Option<PathBuf>,
    pub action: zup_runtime::Operation,

    pub install_dir_override: Option<PathBuf>,

    pub selection: zup_runtime::OperationSelection,
}

pub fn run(request: &LinuxRunRequest) -> Result<LinuxOutcome, ExecError> {
    crate::carrier::Carrier::open(&request.installer)?;
    let mut host = crate::session_host::LinuxSessionHost::open(request.installer.clone());
    let discovery = host
        .discover(
            &request.installer,
            &zup_session::SessionLaunch {
                scope: Some(request.scope),
                state_root: request.state_root.clone(),
                ..Default::default()
            },
        )
        .map_err(ExecError::Ownership)?;
    if request.scope == SelectedScope::Machine {
        crate::machine::resolve_machine_state_root(
            request.state_root.as_deref(),
            &crate::locations::MachineRoots::production(),
        )?;
    } else {
        refuse_redirected_hierarchy(&discovery.state_root)?;
    }
    let mut selection = request.selection.clone();
    if selection.install_directory.is_none() {
        selection.install_directory = request.install_dir_override.clone();
    }
    let input = zup_runtime::OperationInput {
        build: &discovery.build,
        installer: &request.installer,
        scope: discovery.scope,
        operation: request.action,
        selection,
        prior: discovery.prior(),
        state_root: &discovery.state_root,
        cancellation: &zup_plan::NeverCancelled,
    };
    let prepared = zup_runtime::prepare_operation(&input, &host).map_err(|error| {
        let message = error.to_string();
        if message.contains("pass through a link") || message.contains("symbolic link") {
            ExecError::Path(PathError::Refused {
                path: discovery.state_root.display().to_string(),
                reason: message,
            })
        } else {
            ExecError::from(error)
        }
    })?;
    let (events, _) = tokio::sync::broadcast::channel(16);
    let control = zup_runtime::RuntimeControl::new(
        zup_runtime::CancellationHandle::new(),
        events,
        zup_runtime::ExecutionPolicy::Interactive,
    );
    let outcome = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| ExecError::Executor(error.to_string()))?
        .block_on(zup_session::run_operation(
            &host, &discovery, &prepared, control,
        ))
        .map_err(|error| ExecError::Executor(error.to_string()))?;
    let app_version = prepared
        .target_plan
        .as_ref()
        .map(|target| target.app.version.clone())
        .unwrap_or_else(|| discovery.build.installer.app.version.clone());
    match outcome {
        zup_runtime::InstallOutcome::Committed => Ok(LinuxOutcome::Committed {
            version: app_version,
        }),
        zup_runtime::InstallOutcome::RolledBack => Ok(LinuxOutcome::RolledBack),
        zup_runtime::InstallOutcome::RecoveryRequired => Ok(LinuxOutcome::RecoveryRequired {
            transaction: zup_runtime::discover_recovery(&discovery.state_root)
                .into_iter()
                .find_map(|status| match status {
                    zup_runtime::RecoveryStatus::RecoveryRequired { transaction_id } => {
                        Some(transaction_id)
                    }
                    _ => None,
                })
                .ok_or_else(|| ExecError::Ownership("recovery transaction unknown".into()))?,
        }),
        zup_runtime::InstallOutcome::Busy { .. } => Ok(LinuxOutcome::Busy),
        zup_runtime::InstallOutcome::Cancelled => Err(ExecError::Ownership("cancelled".into())),
        zup_runtime::InstallOutcome::RebootRequired { .. }
        | zup_runtime::InstallOutcome::Failed(_) => Err(ExecError::Ownership(
            "the operation did not complete".into(),
        )),
    }
}

pub(crate) fn refuse_redirected_hierarchy(state_root: &Path) -> Result<(), ExecError> {
    let refused = |path: PathBuf, what: &str| PathError::Refused {
        path: path.display().to_string(),
        reason: format!("{what}; the state hierarchy must not pass through a link"),
    };
    crate::fs::anchor_reachable(state_root).map_err(|error| match error {
        crate::error::PathError::UnexpectedKind { path, expected } => {
            ExecError::from(PathError::Refused {
                path,
                reason: format!("{expected}; the state hierarchy must not pass through a link"),
            })
        }
        other => ExecError::Executor(other.to_string()),
    })?;
    let owned = match crate::fs::OwnedDirectory::open(state_root) {
        Ok(owned) => owned,
        Err(crate::error::PathError::Io { source, .. })
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            return Ok(());
        }
        Err(error) => return Err(ExecError::Executor(error.to_string())),
    };
    for name in owned
        .child_names()
        .map_err(|error| ExecError::Executor(error.to_string()))?
    {
        if owned
            .kind_or_absent(&name)
            .map_err(|error| ExecError::Executor(error.to_string()))?
            .is_some_and(|kind| kind == crate::fs::EntryKind::Symlink)
        {
            return Err(refused(state_root.join(&name), "a state entry is a symbolic link").into());
        }
    }
    Ok(())
}

pub(crate) fn sweep_refresh(record: &zup_transaction::TransactionRecord) -> Result<(), ExecError> {
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

pub(crate) fn preflight_refresh(plan: &zup_transaction::TransactionPlan) -> Result<(), ExecError> {
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
                                .stream_regular(&name)
                                .map(|current| current.size == *size && current.sha256 == *sha256)
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
    installer: PathBuf,
    maintenance_sha256: Sha256Digest,
    maintenance_size: u64,
    generated: std::collections::BTreeMap<String, Vec<u8>>,
}

impl RunnerPayload {
    pub(crate) fn from_prepared(
        package: PackagePayloadSource,
        installer: PathBuf,
        maintenance_sha256: Sha256Digest,
        maintenance_size: u64,
        generated: std::collections::BTreeMap<String, Vec<u8>>,
    ) -> Self {
        Self {
            package,
            installer,
            maintenance_sha256,
            maintenance_size,
            generated,
        }
    }

    pub(crate) fn open(
        carrier: &Carrier,
        installer: &Path,
        generated: std::collections::BTreeMap<String, Vec<u8>>,
    ) -> Result<Self, ExecError> {
        let (size, sha256) = hash_carrier(installer)?;
        Ok(Self {
            package: carrier.package().payload_source(),
            installer: installer.to_path_buf(),
            maintenance_sha256: sha256,
            maintenance_size: size,
            generated,
        })
    }

    fn open_maintenance(&self, path: &str) -> Result<PayloadReader, PayloadError> {
        let mut file =
            std::fs::File::open(&self.installer).map_err(|source| PayloadError::Read {
                path: path.to_string(),
                source,
            })?;
        let (size, sha256) =
            zup_core::hash_reader(&mut file).map_err(|source| PayloadError::Read {
                path: path.to_string(),
                source,
            })?;
        if sha256 != self.maintenance_sha256 || size != self.maintenance_size {
            return Err(PayloadError::DigestMismatch {
                path: path.to_string(),
            });
        }
        use std::io::Seek as _;
        file.seek(std::io::SeekFrom::Start(0))
            .map_err(|source| PayloadError::Read {
                path: path.to_string(),
                source,
            })?;
        Ok(Box::new(file))
    }
}

pub(crate) fn hash_carrier(installer: &Path) -> Result<(u64, Sha256Digest), ExecError> {
    let file = std::fs::File::open(installer).map_err(|source| PathError::Io {
        path: installer.display().to_string(),
        source,
    })?;
    zup_core::hash_reader(file)
        .map_err(|source| PathError::Io {
            path: installer.display().to_string(),
            source,
        })
        .map_err(ExecError::from)
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
            return self.open_maintenance(&path.to_string());
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
