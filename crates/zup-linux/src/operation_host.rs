use std::collections::BTreeMap;

use zup_core::{ResourceKey, SelectedScope, Template};
use zup_plan::{DesiredState, HostSnapshot, InstallationRecord};

use zup_plan::DeploymentEffects;
use zup_runtime::{ContentAccess, OperationHost, OperationInput, attach_maintenance_copy};

pub struct MachineHost {
    pub roots: crate::locations::MachineRoots,
    pub systemd: crate::machine::SystemdRoots,
}

pub struct LinuxOperationHost {
    pub installer: std::path::PathBuf,
    pub machine: Option<MachineHost>,
}

impl OperationHost for LinuxOperationHost {
    fn collected_deployments(
        &self,
        input: &zup_runtime::OperationInput,
        record: &InstallationRecord,
    ) -> Result<Vec<zup_plan::RetiredDeploymentSelection>, String> {
        let remaining = self.cleanup_candidates(input, record, None)?;
        if input.scope == SelectedScope::Machine && rustix::process::geteuid().as_raw() != 0 {
            return Ok(Vec::new());
        }
        Ok(zup_transaction::collected_deployments(record, &remaining))
    }

    fn cleanup_candidates(
        &self,
        input: &OperationInput,
        record: &InstallationRecord,
        retiring: Option<&zup_plan::RetentionChange>,
    ) -> Result<Vec<zup_plan::CleanupOperation>, String> {
        if input.scope == SelectedScope::Machine && rustix::process::geteuid().as_raw() != 0 {
            return Ok(Vec::new());
        }
        let store = zup_transaction::FilesystemTransactionStore::private_at(input.state_root);
        let mut operations = zup_transaction::cleanup_candidates(&store, record, retiring)?;
        let state = crate::state::StateAuthority {
            root: input.state_root.to_owned(),
            app: record.app_id.clone(),
            scope: record.scope,
        };
        operations.extend(
            zup_transaction::HistoryStorage(crate::history::LinuxHistoryFilesystem(state))
                .candidates(&store, record, retiring)?
                .into_iter()
                .map(|history| zup_plan::CleanupOperation {
                    candidate: zup_plan::CleanupCandidate::History(history),
                    privilege: record.scope.authorization(),
                }),
        );
        Ok(operations)
    }

    fn data_bindings(
        &self,
        target: &zup_plan::BoundDeployment,
    ) -> Result<BTreeMap<zup_core::ResourceId, zup_plan::PersistentBinding>, String> {
        crate::persistent::resolve_persistent_bindings(target)
    }

    fn persistent_claims(
        &self,
        input: &OperationInput,
    ) -> Result<Option<zup_plan::PersistentClaimSelection>, zup_runtime::OperationError> {
        if input.scope == SelectedScope::Machine && rustix::process::geteuid().as_raw() != 0 {
            return Ok(None);
        }
        crate::ledger::LinuxInstallationStore::new(input.state_root)
            .persistent_claims(
                &input.build.installer.app.id,
                input.scope,
                input.build.installer.target,
            )
            .map_err(|error| zup_runtime::OperationError::refused("persistent data", error))
    }

    fn prepare_bootstrap(
        &self,
        install: &DesiredState,
    ) -> Result<Option<zup_runtime::BootstrapRequest>, String> {
        crate::prerequisites::prepare(install)
    }
    fn plan_build(
        &self,
        build: &zup_core::TargetBuildPlan,
        request: &zup_plan::PlanRequest,
        cancellation: &dyn zup_plan::CancellationQuery,
    ) -> Result<zup_plan::PlannedInstallation, zup_runtime::OperationError> {
        let planning = zup_core::BuildPlan {
            targets: vec![build.clone()],
        };
        if build.installer.plugins.is_empty() {
            return zup_plan::plan(&planning, request, None, cancellation)
                .map_err(zup_runtime::OperationError::from);
        }
        let carrier = crate::carrier::Carrier::open(&self.installer)
            .map_err(|error| zup_runtime::OperationError::refused("plugin", error))?;
        let executor = zup_plugin_runtime::WasmtimePluginExecutor::load(
            carrier.into_package(),
            &build.installer.target,
        )
        .map_err(|error| zup_runtime::OperationError::refused("plugin", error))?;
        let mut executor = zup_plugin_runtime::NativePluginExecutor {
            executor,
            locations: self
                .machine
                .as_ref()
                .map(|machine| {
                    crate::locations::LinuxInstallLocationResolver::with_machine_roots(
                        machine.roots.clone(),
                    )
                })
                .unwrap_or_default(),
        };
        zup_plan::plan(&planning, request, Some(&mut executor), cancellation)
            .map_err(zup_runtime::OperationError::from)
    }

    fn validate_context(&self, input: &OperationInput) -> Result<(), zup_runtime::OperationError> {
        if input.scope == SelectedScope::User {
            crate::run::refuse_redirected_hierarchy(input.state_root)
                .map_err(|error| zup_runtime::OperationError::refused("context", error))?;
        }
        if input.scope == SelectedScope::User || rustix::process::geteuid().as_raw() == 0 {
            let store = zup_transaction::FilesystemTransactionStore::private_at(input.state_root);
            let settled = zup_transaction::settled_records(
                &store,
                &input.build.installer.app.id,
                input.scope,
                input.prior,
            )
            .map_err(|error| match error {
                zup_transaction::ReconciliationError::RecoveryRequired(transaction) => {
                    zup_runtime::OperationError::RecoveryRequired(transaction)
                }
                error => zup_runtime::OperationError::refused("journal", error),
            })?;
            if let Some(record) = settled.into_iter().find(|record| {
                record.phase == zup_transaction::TransactionPhase::Committed
                    && record.reconciliation != zup_transaction::ReconciliationState::Complete
            }) {
                return Err(zup_runtime::OperationError::RecoveryRequired(
                    record.transaction_id,
                ));
            }
        }
        Ok(())
    }

    fn resolve_target(&self, install: &DesiredState) -> Result<zup_plan::BoundDeployment, String> {
        match &self.machine {
            Some(machine) => crate::resolve::resolve_target(
                install,
                &crate::locations::LinuxInstallLocationResolver::with_machine_roots(
                    machine.roots.clone(),
                ),
            ),
            None => crate::resolve::resolve_target(
                install,
                &crate::locations::LinuxInstallLocationResolver::default(),
            ),
        }
        .map_err(|error| error.to_string())
    }

    fn install_directory(
        &self,
        explicit: Option<&std::path::Path>,
        prior: Option<&InstallationRecord>,
        allowed: bool,
        repair: bool,
    ) -> Result<Option<Template>, String> {
        if let (Some(machine), Some(directory)) = (&self.machine, explicit) {
            crate::machine::authorize_machine_install_directory(directory, &machine.roots)
                .map_err(|error| error.to_string())?;
            let target = match prior {
                Some(ledger) => ledger.target,
                None => zup_core::Target::host().map_err(|error| error.to_string())?,
            };
            return crate::machine::host_to_install_template(directory, &machine.roots, &target)
                .map(Some)
                .map_err(|error| error.to_string());
        }
        zup_runtime::choose_install_directory(explicit, prior, allowed, repair)
            .map_err(|error| error.to_string())
    }

    fn observe(
        &self,
        target: Option<&zup_plan::BoundDeployment>,
        ledger: Option<&InstallationRecord>,
    ) -> Result<(Option<HostSnapshot>, BTreeMap<ResourceKey, bool>), String> {
        let mut snapshot = target.map(crate::executor::snapshot_target);
        let mut owned_matches = crate::run::inspect_owned_matches_for(ledger);
        if crate::input::requires_service_manager(target, ledger) {
            let systemd = self
                .machine
                .as_ref()
                .map(|machine| &machine.systemd)
                .ok_or_else(|| "services need machine scope".to_owned())?;
            if crate::openrc_native::active() {
                let machine = self.machine.as_ref().ok_or("services need machine scope")?;
                let manager = crate::openrc_native::Openrc::connect(machine.roots.clone())?;
                if let (Some(target), Some(snapshot)) = (target, snapshot.as_mut()) {
                    snapshot.services = manager.snapshot(target)?;
                }
                if let Some(ledger) = ledger {
                    for (key, owned) in &ledger.current.resources {
                        if let zup_plan::OwnedResource::Service(zup_plan::OwnedService {
                            name,
                            installed,
                            ..
                        }) = owned
                        {
                            owned_matches.insert(
                                key.clone(),
                                manager.observe_registration(name)?.configuration().as_ref()
                                    == Some(installed),
                            );
                        }
                    }
                }
                return Ok((snapshot, owned_matches));
            }
            let mut manager = crate::systemd::RealSystemd::connect()
                .map_err(|error| format!("systemd is unavailable: {error}"))?;
            if let (Some(target), Some(snapshot)) = (target, snapshot.as_mut()) {
                snapshot.services = crate::input::snapshot_services(target, &mut manager, systemd)
                    .map_err(|error| format!("service snapshot: {error}"))?;
            }
            if let Some(ledger) = ledger {
                for (key, owned) in &ledger.current.resources {
                    if let zup_plan::OwnedResource::Service(zup_plan::OwnedService {
                        name,
                        installed,
                        ..
                    }) = owned
                    {
                        let path = crate::machine::authorize_systemd_unit(name, systemd)
                            .map_err(|error| error.to_string())?;
                        let observed = crate::service_ops::observe_unit(
                            name,
                            &path,
                            ledger.target,
                            &mut manager,
                        )
                        .map_err(|error| error.to_string())?;
                        owned_matches.insert(
                            key.clone(),
                            observed.configuration().as_ref() == Some(installed),
                        );
                    }
                }
            }
        }
        Ok((snapshot, owned_matches))
    }

    fn compile(
        &self,
        execution: &zup_plan::ExecutionPlan,
        target: Option<&zup_plan::BoundDeployment>,
        _ledger: Option<&InstallationRecord>,
    ) -> Result<DeploymentEffects, String> {
        execution
            .validate_generation(_ledger)
            .map_err(|error| error.to_string())?;
        if crate::openrc_native::active()
            && (!execution.services.is_empty()
                || execution
                    .removals
                    .iter()
                    .any(|removal| matches!(removal.owned, zup_plan::OwnedResource::Service(_))))
        {
            let machine = self.machine.as_ref().ok_or("services need machine scope")?;
            let mut manager = crate::openrc_native::Openrc::connect(machine.roots.clone())?;
            return manager.compile(execution, target, _ledger);
        }
        match &self.machine {
            Some(machine)
                if !execution.services.is_empty()
                    || execution.removals.iter().any(|removal| {
                        matches!(removal.owned, zup_plan::OwnedResource::Service(_))
                    }) =>
            {
                let mut manager = crate::systemd::RealSystemd::connect()
                    .map_err(|error| format!("systemd is unavailable: {error}"))?;
                crate::input::compile_machine_execution_plan(
                    execution,
                    target,
                    crate::input::ServiceCompilation {
                        ledger: _ledger,
                        roots: &machine.roots,
                        systemd: &machine.systemd,
                        manager: &mut manager,
                    },
                )
            }
            _ => crate::input::compile_execution_plan(execution, target),
        }
        .map_err(|error| error.to_string())
    }

    fn attach_content(
        &self,
        target: &mut zup_plan::BoundDeployment,
        content: &ContentAccess,
        preset: Option<&zup_core::PresetRuntime>,
    ) -> Result<(), String> {
        let preparation = attach_maintenance_copy(target, content, crate::run::MAINTENANCE_SOURCE)
            .map_err(|error| error.to_string())?;
        if preparation == zup_plan::GenerationPreparation::ReuseVerified {
            if target.preset.as_ref().map(|installed| &installed.preset) != preset {
                return Err("repair requires the installed preset declaration".into());
            }
            return Ok(());
        }
        if let Ok(host) = crate::lowering::to_host_path(&target.install_directory) {
            crate::fs::anchor_reachable(&host).map_err(|error| match error {
                crate::error::PathError::UnexpectedKind { path, expected } => {
                    format!(
                        "{expected}; the install destination must not pass through a link: {path}"
                    )
                }
                other => other.to_string(),
            })?;
        }
        if let Some(preset) = preset {
            let carrier = crate::carrier::Carrier::open(content.installer)
                .map_err(|error| error.to_string())?;
            zup_runtime::attach_preset_content(target, preset, &|name| {
                if name == zup_bundle::PRESET_SOURCE {
                    carrier
                        .preset()
                        .map(<[u8]>::to_vec)
                        .ok_or_else(|| "this installer was composed without a preset".to_owned())
                } else {
                    let asset = name
                        .strip_prefix("__zup_preset_asset__/")
                        .ok_or_else(|| "invalid preset source".to_owned())?;
                    carrier
                        .package()
                        .ui_asset(asset)
                        .map(|(_, bytes)| bytes)
                        .map_err(|error| error.to_string())
                }
            })
            .map_err(|error| error.to_string())?;
        }
        Ok(())
    }
}

pub(crate) fn generated_content(
    prepared: &zup_runtime::PreparedOperation,
    roots: Option<&crate::locations::MachineRoots>,
) -> Result<BTreeMap<String, Vec<u8>>, String> {
    let resolver = roots
        .cloned()
        .map(crate::locations::LinuxInstallLocationResolver::with_machine_roots)
        .unwrap_or_default();
    let mut generated = prepared
        .install_plan()
        .map(|plan| crate::integration::generated_map(plan, &resolver))
        .transpose()
        .map_err(|error| error.to_string())?
        .unwrap_or_default();
    generated.extend(
        prepared
            .generated_files()
            .iter()
            .map(|file| (file.source_relative.to_string(), file.bytes.clone())),
    );
    Ok(generated)
}
