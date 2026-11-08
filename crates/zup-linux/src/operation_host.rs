use std::collections::BTreeMap;

use zup_core::{ResourceKey, SelectedScope, Template};
use zup_plan::{DesiredState, HostSnapshot, InstallLedger};

use zup_runtime::{ContentAccess, OperationHost, OperationInput, attach_maintenance_copy};
use zup_transaction::TransactionInput;

pub struct MachineHost {
    pub roots: crate::locations::MachineRoots,
    pub systemd: crate::machine::SystemdRoots,
}

pub struct LinuxOperationHost {
    pub installer: std::path::PathBuf,
    pub machine: Option<MachineHost>,
}

impl OperationHost for LinuxOperationHost {
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
        let mut executor = LinuxPluginExecutor {
            inner: executor,
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

    fn resolve_target(
        &self,
        install: &DesiredState,
    ) -> Result<DesiredState<zup_plan::TargetPath, zup_plan::TargetPath>, String> {
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
        prior: Option<&InstallLedger>,
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
        target: Option<&DesiredState<zup_plan::TargetPath, zup_plan::TargetPath>>,
        ledger: Option<&InstallLedger>,
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
                    for (key, owned) in &ledger.resources {
                        if let zup_plan::OwnedResource::Service {
                            name, installed, ..
                        } = owned
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
                for (key, owned) in &ledger.resources {
                    if let zup_plan::OwnedResource::Service {
                        name, installed, ..
                    } = owned
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
        target: Option<&DesiredState<zup_plan::TargetPath, zup_plan::TargetPath>>,
        _ledger: Option<&InstallLedger>,
    ) -> Result<TransactionInput, String> {
        execution
            .validate_generation(_ledger)
            .map_err(|error| error.to_string())?;
        if crate::openrc_native::active()
            && (!execution.services.is_empty()
                || execution.removals.iter().any(|removal| {
                    matches!(removal.owned, zup_plan::OwnedResource::Service { .. })
                }))
        {
            let machine = self.machine.as_ref().ok_or("services need machine scope")?;
            let mut manager = crate::openrc_native::Openrc::connect(machine.roots.clone())?;
            return manager.compile(execution, target, _ledger);
        }
        match &self.machine {
            Some(machine)
                if !execution.services.is_empty()
                    || execution.removals.iter().any(|removal| {
                        matches!(removal.owned, zup_plan::OwnedResource::Service { .. })
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
        target: &mut DesiredState<zup_plan::TargetPath, zup_plan::TargetPath>,
        content: &ContentAccess,
        preset: Option<&zup_core::PresetRuntime>,
    ) -> Result<(), String> {
        attach_maintenance_copy(
            target,
            content.installer,
            content.state_root,
            content.scope,
            crate::run::MAINTENANCE_SOURCE,
        )
        .map_err(|error| error.to_string())?;
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
            let executable = carrier
                .preset()
                .ok_or_else(|| "this installer was composed without a preset".to_owned())?
                .to_vec();
            let assets = preset
                .assets
                .iter()
                .map(|asset| {
                    let (_, bytes) = carrier
                        .package()
                        .ui_asset(asset.name.as_str())
                        .map_err(|error| format!("the asset `{}`: {error}", asset.name))?;
                    if bytes.len() as u64 != asset.size
                        || zup_core::hash_bytes(&bytes) != asset.sha256
                    {
                        return Err(format!(
                            "the asset `{}` is not the content this application configured",
                            asset.name
                        ));
                    }
                    Ok(bytes)
                })
                .collect::<Result<Vec<_>, String>>()?;
            let runtime_directory = zup_transaction::maintenance_directory(
                content.state_root,
                &target.app.id,
                content.scope,
                &target.app.version,
            );
            let executable_digest = zup_core::hash_bytes(&executable);
            let mut files = vec![(
                zup_bundle::preset_path(
                    &runtime_directory,
                    &executable_digest,
                    target.target.executable_suffix(),
                ),
                executable_digest,
                executable.len() as u64,
                zup_core::RelativePath::new(zup_bundle::PRESET_SOURCE)
                    .expect("a reserved source name is always relative"),
                true,
            )];
            for (asset, _) in preset.assets.iter().zip(&assets) {
                files.push((
                    zup_bundle::asset_path(&runtime_directory, asset.name.as_str(), &asset.sha256),
                    asset.sha256,
                    asset.size,
                    zup_bundle::asset_source_name(asset.name.as_str()),
                    false,
                ));
            }
            attach_ui_runtime(target, content.scope, preset, executable_digest, files)?;
        }
        Ok(())
    }
}

struct LinuxPluginExecutor {
    inner: zup_plugin_runtime::WasmtimePluginExecutor,
    locations: crate::locations::LinuxInstallLocationResolver,
}

impl zup_plan::PluginExecutor for LinuxPluginExecutor {
    fn target(&self) -> &zup_core::Target {
        self.inner.target()
    }

    fn plan(
        &mut self,
        binding: &zup_core::PluginBinding,
        context: &zup_plan::PluginPlanningContext,
        cancellation: &dyn zup_plan::CancellationQuery,
    ) -> Result<zup_plan::PluginResourceProposal, zup_plan::PluginFailure> {
        let root = zup_plan::resolve_template_path(
            &context.install_directory,
            &context.target,
            &self.locations,
            context.scope,
        )
        .map_err(|error| zup_plan::PluginFailure::internal(error.to_string()))?;
        let mut context = context.clone();
        context.install_directory = Template::parse(root.as_str())
            .map_err(|error| zup_plan::PluginFailure::internal(error.to_string()))?;
        self.inner.plan(binding, &context, cancellation)
    }
}

fn attach_ui_runtime(
    target: &mut DesiredState<zup_plan::TargetPath, zup_plan::TargetPath>,
    scope: SelectedScope,
    preset: &zup_core::PresetRuntime,
    executable_digest: zup_core::Sha256Digest,
    files: Vec<(
        std::path::PathBuf,
        zup_core::Sha256Digest,
        u64,
        zup_core::RelativePath,
        bool,
    )>,
) -> Result<(), String> {
    for (path, digest, size, source, executable) in files {
        let destination = zup_plan::TargetPath::new(target.target, path.to_string_lossy())
            .map_err(|error| format!("preset runtime destination: {error}"))?;
        let text = destination.to_string();
        target.files.push(zup_plan::DesiredFile {
            key: zup_core::ResourceKey::File { destination: text },
            source_relative: source,
            destination,
            size,
            sha256: digest,
            privilege: scope.authorization(),
            executable,
        });
    }
    target.preset = Some(zup_core::InstalledPreset {
        preset: preset.clone(),
        executable: executable_digest,
    });
    Ok(())
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
