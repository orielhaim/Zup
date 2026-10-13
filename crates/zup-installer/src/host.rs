pub use preset::{Composed, PresetError, Source, materialize};

pub use zup_preset_host::{
    ActionRefusal, HostDecision, HostState, Selection, capabilities, component, components,
    diagnostic, engine_scope, install_options, maintenance_state, plan, product, scope, scopes,
};

pub mod preset {
    use std::collections::BTreeMap;
    use std::io;
    use std::path::{Path, PathBuf};

    use zup_core::PresetRuntime;
    use zup_preset_protocol::Capabilities;

    #[derive(Debug, thiserror::Error)]
    pub enum PresetError {
        #[error("this installation has no window to show: {0}")]
        Missing(String),
        #[error("this host cannot present the preset: {0}")]
        Incompatible(String),
        #[error("the installed UI content cannot be used: {0}")]
        Integrity(#[from] zup_bundle::PresetContentError),
        #[error("the preset could not be prepared for launch: {0}")]
        Prepare(#[source] io::Error),
    }

    pub enum Source<'a> {
        Composed {
            executable: &'a Path,
            preset: &'a PresetRuntime,
            bundle: &'a zup_windows::EmbeddedBundle,
        },
        Installed {
            directory: &'a Path,
            runtime: &'a zup_core::InstalledPreset,
            target: &'a zup_core::Target,
        },
    }

    #[derive(Debug)]
    pub struct Composed {
        pub executable: PathBuf,
        pub configuration: zup_preset_protocol::Configuration,
    }

    pub fn materialize(
        source: &Source<'_>,
        capabilities: &Capabilities,
    ) -> Result<Composed, PresetError> {
        let preset = match source {
            Source::Composed { preset, .. } => *preset,
            Source::Installed { runtime, .. } => &runtime.preset,
        };
        zup_preset_host::process::check_presentable(preset, capabilities)
            .map_err(PresetError::Incompatible)?;

        let (executable, assets) = match source {
            Source::Installed {
                directory,
                runtime,
                target,
            } => {
                let resolved = zup_bundle::resolve_preset_content(
                    directory,
                    runtime,
                    target.executable_suffix(),
                    zup_windows::plain_path_text,
                )?;
                (resolved.executable, resolved.assets)
            }
            Source::Composed {
                executable, bundle, ..
            } => {
                let mut assets = BTreeMap::new();
                for asset in &preset.assets {
                    let (_, bytes) = bundle.ui_asset(asset.name.as_str()).map_err(|error| {
                        PresetError::Missing(format!("the asset `{}`: {error}", asset.name))
                    })?;
                    let path = staging_directory(executable).join(asset.name.as_str());
                    if let Some(parent) = path.parent() {
                        std::fs::create_dir_all(parent)
                            .map_err(|error| PresetError::Prepare(io::Error::other(error)))?;
                    }
                    zup_windows::write_durable(&path, &bytes).map_err(|error| {
                        PresetError::Prepare(io::Error::other(error.to_string()))
                    })?;
                    assets.insert(asset.name.to_string(), zup_windows::plain_path_text(&path));
                }
                (locate(executable)?, assets)
            }
        };

        Ok(Composed {
            executable,
            configuration: zup_preset_protocol::Configuration {
                settings: preset.settings.clone(),
                assets,
            },
        })
    }

    fn staging_directory(executable: &Path) -> PathBuf {
        executable
            .parent()
            .unwrap_or(Path::new("."))
            .join("preset-assets")
    }

    fn locate(executable: &Path) -> Result<PathBuf, PresetError> {
        let composed = executable.parent().unwrap_or(Path::new(".")).join(
            zup_windows::preset_executable_name(std::env::consts::EXE_SUFFIX),
        );
        if composed.is_file() {
            return Ok(composed);
        }
        let bundle = crate::package::open_bundle(executable)
            .map_err(|error| PresetError::Prepare(io::Error::other(error.to_string())))?;
        let bytes = bundle.preset().ok_or(PresetError::Missing(
            "this installer was composed without a preset".into(),
        ))?;
        zup_windows::write_durable(&composed, bytes)
            .map_err(|error| PresetError::Prepare(io::Error::other(error.to_string())))?;
        Ok(composed)
    }

    pub use zup_preset_host::process::{PresetProcess, PresetReader, SessionError, launch};
}

use std::path::{Path, PathBuf};
use std::sync::Arc;

use zup_core::{Frontend, SelectedScope};
use zup_exec::InstallLedger;
use zup_session::{SessionDiscovery, SessionHost, SessionLaunch};

use crate::maintenance as state;
use crate::package;

#[derive(Clone)]
pub struct WindowsSessionHost {
    executable: PathBuf,
    bundle: Option<zup_windows::EmbeddedBundle>,
    app_id: Option<zup_core::AppId>,
    scope: SelectedScope,
    frontend: Frontend,
    state_root: PathBuf,
}

impl WindowsSessionHost {
    fn operation_host(&self) -> Result<zup_windows::WindowsOperationHost<'_>, String> {
        let bundle = self
            .bundle
            .as_ref()
            .ok_or_else(|| "this file carries no installer package".to_owned())?;
        Ok(zup_windows::WindowsOperationHost {
            app_id: self
                .app_id
                .clone()
                .ok_or_else(|| "the session host was not discovered".to_owned())?,
            scope: self.scope,
            frontend: self.frontend,
            state_root: self.state_root.clone(),
            content: zup_windows::WindowsContent::Embedded(bundle),
            acquire_prerequisites: true,
        })
    }
}

impl zup_runtime::OperationHost for WindowsSessionHost {
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

    fn observe(
        &self,
        target: Option<&zup_platform::TargetPlan>,
        ledger: Option<&InstallLedger>,
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
        ledger: Option<&InstallLedger>,
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

    fn prepare_bootstrap(
        &self,
        install: &zup_plan::InstallPlan,
    ) -> Result<Option<zup_runtime::BootstrapRequest>, String> {
        self.operation_host()?.prepare_bootstrap(install)
    }
}

impl SessionHost for WindowsSessionHost {
    fn discover(
        &mut self,
        executable: &Path,
        launch: &SessionLaunch,
    ) -> Result<SessionDiscovery, String> {
        let bundle = package::open_bundle_if_present(executable)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| {
                "this file carries no installer package; run the installer you downloaded"
                    .to_owned()
            })?;
        let build = package::target_plan(&bundle).map_err(|error| error.to_string())?;
        let installer = build.installer.clone();
        let mut priors = Vec::new();
        for scope in zup_preset_host::surface::scopes(&installer)
            .into_iter()
            .map(zup_preset_host::convert::engine_scope)
        {
            let root = state::resolve_state_root(launch.state_root.clone(), scope)
                .map_err(|error| error.to_string())?;
            let ledger = zup_windows::InstallLedgerStore::new(root)
                .load(&installer.app.id, scope)
                .map_err(|error| error.to_string())?;
            if let Some(ledger) = ledger {
                priors.push((scope, ledger));
            }
        }
        if launch.maintenance && priors.is_empty() {
            return Err("the installed application was not found".into());
        }
        let scope = priors
            .first()
            .map(|(scope, _)| *scope)
            .or(launch.scope)
            .unwrap_or_else(|| zup_preset_host::surface::default_scope(&installer));
        let state_root = state::resolve_state_root(launch.state_root.clone(), scope)
            .map_err(|error| error.to_string())?;
        self.executable = executable.to_path_buf();
        self.bundle = Some(bundle);
        self.app_id = Some(installer.app.id.clone());
        self.scope = scope;
        self.frontend = installer.frontend;
        self.state_root = state_root.clone();
        Ok(SessionDiscovery {
            build,
            priors,
            scope,
            state_root,
        })
    }

    fn backend(
        &self,
        prepared: &zup_runtime::PreparedOperation,
    ) -> Result<Arc<dyn zup_runtime::RuntimeBackend>, String> {
        let install = prepared
            .install_plan
            .as_ref()
            .ok_or_else(|| "the operation planned no installation to execute".to_owned())?;
        let backend = if prepared.action == zup_exec::LifecycleAction::Uninstall {
            zup_windows::WindowsRuntimeBackend::from_path(self.executable.clone(), None)
                .map_err(|error| error.to_string())?
        } else {
            zup_windows::WindowsRuntimeBackend::from_path_with_generated_files(
                self.executable.clone(),
                &self.state_root,
                self.scope,
                install,
                &prepared.generated_files,
                &prepared.transaction_plan,
            )
            .map_err(|error| error.to_string())?
        };
        Ok(Arc::new(backend))
    }

    fn materialize_preset(
        &self,
        discovery: &SessionDiscovery,
    ) -> Result<zup_session::PresetLaunch, String> {
        let installer = discovery.installer();
        let bundle = self
            .bundle
            .as_ref()
            .ok_or_else(|| "this file carries no installer package".to_owned())?;
        let installed = discovery.prior().is_some();
        let capabilities = zup_preset_host::surface::capabilities(installer, installed);
        let installed: Option<(zup_core::InstalledPreset, PathBuf)> = discovery
            .prior()
            .map(|ledger| {
                let runtime = ledger.preset().cloned().ok_or_else(|| {
                    format!(
                        "{} is installed with no recorded window, and its maintenance runtime \
                         cannot present one.\n\nReinstall it to restore the window it was \
                         installed with.",
                        installer.app.name
                    )
                })?;
                let directory = zup_transaction::maintenance_directory(
                    &discovery.state_root,
                    &installer.app.id,
                    discovery.scope,
                    &ledger.version,
                );
                Ok::<_, String>((runtime, directory))
            })
            .transpose()?;
        let source = match &installed {
            Some((runtime, directory)) => Source::Installed {
                directory,
                runtime,
                target: &installer.target,
            },
            None => {
                let preset = installer
                    .preset
                    .as_ref()
                    .ok_or_else(|| "this installer was composed without a preset".to_owned())?;
                Source::Composed {
                    executable: &self.executable,
                    preset,
                    bundle,
                }
            }
        };
        let composed =
            preset::materialize(&source, &capabilities).map_err(|error| error.to_string())?;
        Ok(zup_session::PresetLaunch {
            executable: composed.executable,
            configuration: composed.configuration,
            capabilities,
            product: zup_preset_host::surface::product(installer),
            environment: zup_preset_protocol::environment::current(installer.target.as_str()),
        })
    }

    fn update(
        &self,
        scope: SelectedScope,
        status: &mut dyn FnMut(zup_preset_protocol::UpdateState),
    ) -> Result<(), String> {
        state::from_maintenance_surface(&self.executable, scope, status)
    }

    fn copy_diagnostics(&self, text: &str) {
        let _ = std::process::Command::new("clip").arg(text).output();
    }

    fn after_commit(
        &self,
        discovery: &SessionDiscovery,
        _prepared: &zup_runtime::PreparedOperation,
    ) -> Result<(), String> {
        state::remove_lock(
            &discovery.state_root,
            &discovery.build.installer.app.id,
            discovery.scope,
        )
        .map_err(|error| error.to_string())?;
        zup_windows::cleanup_app_payload_overlays(
            &discovery.state_root,
            &discovery.build.installer.app.id,
            discovery.scope,
        )
        .map_err(|error| error.to_string())
    }
}

pub struct Launch {
    pub maintenance: bool,
    pub auto_uninstall: bool,
    pub scope: Option<SelectedScope>,
    pub state_root: Option<PathBuf>,
    pub preselected: Option<Vec<zup_core::ComponentId>>,
}

impl Launch {
    pub fn from_path(executable: &Path) -> Self {
        Self {
            maintenance: zup_transaction::is_maintenance_path(executable),
            auto_uninstall: false,
            scope: None,
            state_root: None,
            preselected: None,
        }
    }

    pub fn from_arguments(args: &crate::cli::LifecycleArgs) -> Self {
        Self {
            maintenance: false,
            auto_uninstall: false,
            scope: Some(SelectedScope::from(args.scope)),
            state_root: args.state_root.clone(),
            preselected: (!args.enable.is_empty()).then(|| {
                args.enable
                    .iter()
                    .filter_map(|raw| zup_core::ComponentId::new(raw).ok())
                    .collect()
            }),
        }
    }

    pub fn uninstall_confirmation(args: &crate::maintenance::UninstallArgs) -> Self {
        Self {
            maintenance: true,
            auto_uninstall: true,
            scope: args.scope.map(SelectedScope::from),
            state_root: args.state_root.clone(),
            preselected: None,
        }
    }

    fn session_launch(&self) -> SessionLaunch {
        SessionLaunch {
            scope: self.scope,
            state_root: self.state_root.clone(),
            preselected: self.preselected.clone(),
            maintenance: self.maintenance,
            auto_uninstall: self.auto_uninstall,
        }
    }
}

pub fn surface(executable: &Path) -> miette::Result<()> {
    block_on(zup_session::run(
        WindowsSessionHost::default(),
        executable,
        Launch::from_path(executable).session_launch(),
    ))
}

pub fn surface_from_arguments(
    context: crate::run::RuntimeContext,
    args: crate::cli::LifecycleArgs,
) -> miette::Result<()> {
    if context.frontend != Frontend::Gui {
        return Err(miette::miette!("this build has no graphical surface"));
    }
    let executable = package::current_executable()?;
    block_on(zup_session::run(
        WindowsSessionHost::default(),
        &executable,
        Launch::from_arguments(&args).session_launch(),
    ))
}

pub fn uninstall_confirmation(
    executable: &Path,
    args: &crate::maintenance::UninstallArgs,
) -> miette::Result<()> {
    block_on(zup_session::run(
        WindowsSessionHost::default(),
        executable,
        Launch::uninstall_confirmation(args).session_launch(),
    ))
}

fn block_on(future: impl std::future::Future<Output = miette::Result<()>>) -> miette::Result<()> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| miette::miette!("start the session runtime: {error}"))?
        .block_on(future)
}

impl Default for WindowsSessionHost {
    fn default() -> Self {
        Self {
            executable: PathBuf::new(),
            bundle: None,
            app_id: None,
            scope: SelectedScope::User,
            frontend: Frontend::Gui,
            state_root: PathBuf::new(),
        }
    }
}
