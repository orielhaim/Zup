use std::collections::BTreeMap;

use zup_core::{ResourceKey, SelectedScope, Template};
use zup_exec::{HostSnapshot, InstallLedger, LifecycleAction};
use zup_plan::InstallPlan;
use zup_platform::TargetPlan;
use zup_runtime::{ContentAccess, OperationHost, OperationInput, attach_maintenance_copy};
use zup_transaction::TransactionInput;

pub struct MachineHost {
    pub roots: crate::locations::MachineRoots,
    pub systemd: crate::machine::SystemdRoots,
}

pub struct LinuxOperationHost {
    pub machine: Option<MachineHost>,
}

impl OperationHost for LinuxOperationHost {
    fn check_capabilities(&self, input: &OperationInput) -> Result<(), String> {
        if input.scope == SelectedScope::User {
            crate::run::refuse_redirected_hierarchy(input.state_root)
                .map_err(|error| error.to_string())?;
        }
        let errors = crate::capabilities::operation_capability_errors(input.build, input.scope);
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("\n"))
        }
    }

    fn resolve_target(&self, install: &InstallPlan) -> Result<TargetPlan, String> {
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

    fn resolve_target_for_uninstall(&self) -> bool {
        true
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
            let target = prior.map(|ledger| ledger.target).unwrap_or_else(|| {
                zup_core::Target::parse(crate::capabilities::SUPPORTED_LINUX_TARGET)
                    .expect("a supported target")
            });
            return crate::machine::host_to_install_template(directory, &machine.roots, &target)
                .map(Some)
                .map_err(|error| error.to_string());
        }
        zup_runtime::choose_install_directory(explicit, prior, allowed, repair)
            .map_err(|error| error.to_string())
    }

    fn observe(
        &self,
        target: Option<&TargetPlan>,
        ledger: Option<&InstallLedger>,
    ) -> Result<(Option<HostSnapshot>, BTreeMap<ResourceKey, bool>), String> {
        match target {
            Some(target) => {
                let mut snapshot = crate::executor::snapshot_target(target);
                if crate::input::requires_service_manager(target, ledger) {
                    let systemd = self
                        .machine
                        .as_ref()
                        .map(|machine| &machine.systemd)
                        .ok_or_else(|| "services need machine scope".to_owned())?;
                    let mut manager = crate::systemd::RealSystemd::connect()
                        .map_err(|error| format!("systemd is unavailable: {error}"))?;
                    snapshot.services =
                        crate::input::snapshot_services(target, &mut manager, systemd)
                            .map_err(|error| format!("service snapshot: {error}"))?;
                }
                Ok((
                    Some(snapshot),
                    crate::run::inspect_owned_matches_for(ledger),
                ))
            }
            None => Ok((None, crate::run::inspect_owned_matches_for(ledger))),
        }
    }

    fn compile(
        &self,
        action: LifecycleAction,
        execution: &zup_exec::ExecutionPlan,
        target: Option<&TargetPlan>,
        _ledger: Option<&InstallLedger>,
    ) -> Result<TransactionInput, String> {
        let target =
            target.ok_or_else(|| "a Linux transaction compiles against its target".to_owned())?;
        match &self.machine {
            Some(machine) => {
                let mut manager = crate::systemd::RealSystemd::connect()
                    .map_err(|error| format!("systemd is unavailable: {error}"))?;
                crate::input::compile_machine_execution_plan(
                    execution,
                    target,
                    crate::input::ServiceCompilation {
                        roots: &machine.roots,
                        systemd: &machine.systemd,
                        manager: &mut manager,
                        force_services: matches!(
                            action,
                            LifecycleAction::Repair { force_files: true }
                        ),
                    },
                )
            }
            None => crate::input::compile_execution_plan(execution, target),
        }
        .map_err(|error| error.to_string())
    }

    fn attach_content(
        &self,
        target: &mut TargetPlan,
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

fn attach_ui_runtime(
    target: &mut TargetPlan,
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
        let destination = zup_platform::TargetPath::new(target.target, path.to_string_lossy())
            .map_err(|error| format!("preset runtime destination: {error}"))?;
        let text = destination.to_string();
        target.files.push(zup_platform::TargetFile {
            key: zup_core::ResourceKey::File { destination: text },
            source_relative: source,
            destination,
            size,
            sha256: digest,
            privilege: scope.authorization(),
            executable,
        });
        target.summary.file_count += 1;
        target.summary.resource_count += 1;
        target.summary.install_bytes = target.summary.install_bytes.saturating_add(size);
    }
    target.preset = Some(zup_core::InstalledPreset {
        preset: preset.clone(),
        executable: executable_digest,
    });
    Ok(())
}
