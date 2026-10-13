use std::path::{Path, PathBuf};
use std::sync::Arc;

use zup_core::SelectedScope;
use zup_runtime::{PreparedOperation, RuntimeBackend};
use zup_session::{SessionDiscovery, SessionHost, SessionLaunch};

use crate::operation_host::LinuxOperationHost;

#[derive(Clone, Default)]
pub struct LinuxSessionHost {
    installer: PathBuf,
    scope: Option<SelectedScope>,
    state_root: Option<PathBuf>,
}

impl LinuxSessionHost {
    pub fn open(installer: PathBuf) -> Self {
        Self {
            installer,
            scope: None,
            state_root: None,
        }
    }

    fn operation_host(&self) -> Result<LinuxOperationHost, String> {
        let machine = match self.scope {
            Some(SelectedScope::Machine) => Some(crate::operation_host::MachineHost {
                roots: crate::locations::MachineRoots::production(),
                systemd: crate::machine::SystemdRoots::production(),
            }),
            _ => None,
        };
        Ok(LinuxOperationHost { machine })
    }
}

impl zup_runtime::OperationHost for LinuxSessionHost {
    fn check_capabilities(&self, input: &zup_runtime::OperationInput) -> Result<(), String> {
        self.operation_host()?.check_capabilities(input)
    }

    fn plan_build(
        &self,
        build: &zup_core::TargetBuildPlan,
        request: &zup_plan::PlanRequest,
        cancellation: &dyn zup_plan::CancellationQuery,
    ) -> Result<zup_plan::PlannedInstallation, String> {
        self.operation_host()?
            .plan_build(build, request, cancellation)
    }

    fn resolve_target(
        &self,
        install: &zup_plan::InstallPlan,
    ) -> Result<zup_platform::TargetPlan, String> {
        self.operation_host()?.resolve_target(install)
    }

    fn resolve_target_for_uninstall(&self) -> bool {
        true
    }

    fn install_directory(
        &self,
        explicit: Option<&std::path::Path>,
        prior: Option<&zup_exec::InstallLedger>,
        allowed: bool,
        repair: bool,
    ) -> Result<Option<zup_core::Template>, String> {
        self.operation_host()?
            .install_directory(explicit, prior, allowed, repair)
    }

    fn observe(
        &self,
        target: Option<&zup_platform::TargetPlan>,
        ledger: Option<&zup_exec::InstallLedger>,
    ) -> Result<
        (
            Option<zup_exec::HostSnapshot>,
            std::collections::BTreeMap<zup_core::ResourceKey, bool>,
        ),
        String,
    > {
        self.operation_host()?.observe(target, ledger)
    }

    fn compile(
        &self,
        action: zup_exec::LifecycleAction,
        execution: &zup_exec::ExecutionPlan,
        target: Option<&zup_platform::TargetPlan>,
        ledger: Option<&zup_exec::InstallLedger>,
    ) -> Result<zup_transaction::TransactionInput, String> {
        self.operation_host()?
            .compile(action, execution, target, ledger)
    }

    fn attach_content(
        &self,
        target: &mut zup_platform::TargetPlan,
        content: &zup_runtime::ContentAccess,
        preset: Option<&zup_core::PresetRuntime>,
    ) -> Result<(), String> {
        self.operation_host()?
            .attach_content(target, content, preset)
    }
}

impl SessionHost for LinuxSessionHost {
    fn discover(
        &mut self,
        executable: &Path,
        launch: &SessionLaunch,
    ) -> Result<SessionDiscovery, String> {
        self.installer = executable.to_path_buf();
        let carrier =
            crate::carrier::Carrier::open(executable).map_err(|error| error.to_string())?;
        let build = carrier
            .package()
            .build_plan()
            .map_err(|error| error.to_string())?
            .targets
            .into_iter()
            .next()
            .ok_or_else(|| "an installer package holds exactly one target".to_owned())?;
        let scope = zup_runtime::resolve_scope(build.installer.install.scope, launch.scope)
            .map_err(|error| error.to_string())?;
        let state_root = match &launch.state_root {
            Some(root) => root.clone(),
            None => crate::paths::state_root(scope).map_err(|error| error.to_string())?,
        };
        let mut priors = Vec::new();
        for candidate in [SelectedScope::User, SelectedScope::Machine] {
            let root = if candidate == scope || launch.state_root.is_some() {
                state_root.clone()
            } else {
                crate::paths::state_root(candidate).map_err(|error| error.to_string())?
            };
            let ledger = crate::ledger::LinuxLedgerStore::new(&root)
                .load(&build.installer.app.id, candidate)
                .map_err(|error| error.to_string())?;
            if let Some(ledger) = ledger {
                priors.push((candidate, ledger));
            }
        }
        self.scope = Some(scope);
        self.state_root = Some(state_root.clone());
        Ok(SessionDiscovery {
            build,
            priors,
            scope,
            state_root,
        })
    }

    fn backend(&self, prepared: &PreparedOperation) -> Result<Arc<dyn RuntimeBackend>, String> {
        let state_root = self
            .state_root
            .clone()
            .ok_or_else(|| "the session host was not discovered".to_owned())?;
        let target = prepared
            .target_plan
            .as_ref()
            .ok_or_else(|| "a Linux transaction resolves its target".to_owned())?;
        let generated = match prepared.install_plan.as_ref() {
            Some(install) => {
                let generated = crate::integration::generated_map(install)
                    .map_err(|error| error.to_string())?;
                crate::integration::save_generated(
                    &state_root,
                    &install.app.id,
                    target.scope,
                    &generated,
                )
                .map_err(|error| error.to_string())?;
                generated
            }
            None => crate::integration::load_generated(&state_root, &target.app.id, target.scope)
                .map_err(|error| error.to_string())?,
        };
        Ok(Arc::new(crate::backend::LinuxRuntimeBackend {
            installer: self.installer.clone(),
            operation: prepared.operation,
            selection: prepared.selection.clone(),
            install_dir_override: prepared.selection.install_directory.clone(),
            generated,
        }))
    }

    fn materialize_preset(
        &self,
        discovery: &SessionDiscovery,
    ) -> Result<zup_session::PresetLaunch, String> {
        let installer = discovery.installer();
        let installed = discovery.prior().is_some();
        let capabilities = zup_preset_host::surface::capabilities(installer, installed);
        let runtime = match discovery.prior() {
            Some(ledger) => ledger
                .preset()
                .map(|recorded| recorded.preset.clone())
                .ok_or_else(|| {
                    format!(
                        "{} is installed with no recorded window, and its maintenance runtime \
                         cannot present one.\n\nReinstall it to restore the window it was \
                         installed with.",
                        installer.app.name
                    )
                })?,
            None => installer
                .preset
                .clone()
                .ok_or_else(|| "this installer was composed without a preset".to_owned())?,
        };
        zup_preset_host::process::check_presentable(&runtime, &capabilities)
            .map_err(|error| error.to_string())?;
        let (executable, assets) = match discovery.prior() {
            Some(ledger) => {
                let recorded = ledger.preset().ok_or_else(|| {
                    format!(
                        "{} is installed with no recorded window.",
                        installer.app.name
                    )
                })?;
                let directory = zup_transaction::maintenance_directory(
                    &discovery.state_root,
                    &installer.app.id,
                    discovery.scope,
                    &ledger.version,
                );
                let resolved = zup_bundle::resolve_preset_content(
                    &directory,
                    recorded,
                    installer.target.executable_suffix(),
                    |path| path.to_string_lossy().into_owned(),
                )
                .map_err(|error| error.to_string())?;
                (resolved.executable, resolved.assets)
            }
            None => {
                let carrier = crate::carrier::Carrier::open(&self.installer)
                    .map_err(|error| error.to_string())?;
                let staged = self
                    .installer
                    .parent()
                    .unwrap_or(Path::new("."))
                    .join(format!("preset{}", installer.target.executable_suffix()));
                write_staged(
                    &staged,
                    carrier
                        .preset()
                        .ok_or_else(|| "this installer was composed without a preset".to_owned())?,
                )?;
                let assets = self
                    .installer
                    .parent()
                    .unwrap_or(Path::new("."))
                    .join("preset-assets");
                let mut staged_assets = std::collections::BTreeMap::new();
                for asset in &runtime.assets {
                    let (_, bytes) = carrier
                        .package()
                        .ui_asset(asset.name.as_str())
                        .map_err(|error| format!("the asset `{}`: {error}", asset.name))?;
                    let path = assets.join(asset.name.as_str());
                    write_staged(&path, &bytes)?;
                    staged_assets
                        .insert(asset.name.to_string(), path.to_string_lossy().into_owned());
                }
                (staged, staged_assets)
            }
        };
        Ok(zup_session::PresetLaunch {
            executable,
            configuration: zup_preset_protocol::Configuration {
                settings: runtime.settings.clone(),
                assets,
            },
            capabilities,
            product: zup_preset_host::surface::product(installer),
            environment: zup_preset_protocol::environment::current(installer.target.as_str()),
        })
    }

    fn copy_diagnostics(&self, text: &str) {
        for (program, args) in [
            ("wl-copy", vec![] as Vec<&str>),
            ("xclip", vec!["-selection", "clipboard"]),
            ("xsel", vec!["--clipboard", "--input"]),
        ] {
            if std::process::Command::new(program)
                .args(&args)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .and_then(|mut child| {
                    use std::io::Write as _;
                    child
                        .stdin
                        .as_mut()
                        .ok_or_else(|| std::io::Error::other("no stdin"))?
                        .write_all(text.as_bytes())?;
                    child.wait()
                })
                .is_ok()
            {
                return;
            }
        }
    }
}

fn write_staged(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let temporary = path.with_extension("zup-tmp");
    std::fs::write(&temporary, bytes).map_err(|error| error.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let _ = std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o755));
    }
    std::fs::rename(&temporary, path).map_err(|error| {
        let _ = std::fs::remove_file(&temporary);
        error.to_string()
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}
