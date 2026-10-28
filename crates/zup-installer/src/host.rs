pub use preset::{Composed, PresetError, Source, materialize};

pub use zup_preset_host::{
    ActionRefusal, HostDecision, HostState, Selection, capabilities, component, components,
    diagnostic, engine_scope, install_options, maintenance_state, plan, product, scope, scopes,
};

pub mod preset {
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
        pub stage: Option<PathBuf>,
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

        let (executable, assets, stage) = match source {
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
                (resolved.executable, resolved.assets, None)
            }
            Source::Composed { bundle, .. } => {
                let bytes = bundle.preset().ok_or(PresetError::Missing(
                    "this installer was composed without a preset".into(),
                ))?;
                let executable_name =
                    zup_windows::preset_executable_name(std::env::consts::EXE_SUFFIX);
                let mut files = vec![(executable_name.as_str(), bytes)];
                let mut owned = Vec::new();
                for asset in &preset.assets {
                    let (_, contents) = bundle.ui_asset(asset.name.as_str()).map_err(|error| {
                        PresetError::Missing(format!("the asset `{}`: {error}", asset.name))
                    })?;
                    owned.push((asset.name.to_string(), contents));
                }
                for (name, contents) in &owned {
                    files.push((name.as_str(), contents.as_slice()));
                }
                let staged = zup_windows::stage_session(&executable_name, &files)
                    .map_err(|error| PresetError::Prepare(io::Error::other(error)))?;
                (staged.executable, staged.assets, Some(staged.root))
            }
        };

        Ok(Composed {
            executable,
            configuration: zup_preset_protocol::Configuration {
                settings: preset.settings.clone(),
                assets,
            },
            stage,
        })
    }

    pub use zup_preset_host::process::{PresetProcess, PresetReader, SessionError, launch};
}

use std::path::{Path, PathBuf};
use std::sync::Arc;

use zup_core::{Frontend, SelectedScope};
use zup_session::{SessionDiscovery, SessionHost, SessionLaunch};

use crate::maintenance as state;
use crate::package;

#[derive(Clone)]
pub struct WindowsSessionHost {
    executable: PathBuf,
    bundle: Option<zup_windows::EmbeddedBundle>,
    app_id: Option<zup_core::AppId>,
    frontend: Frontend,
    acquired: Option<Arc<crate::acquire::Acquired>>,
    payload: Option<Arc<zup_bundle::AcquiredPayloadSource>>,
}

impl WindowsSessionHost {
    fn operation_host<'a>(
        &'a self,
        context: &'a zup_session::OperationContext,
    ) -> Result<zup_windows::WindowsOperationHost<'a>, String> {
        let bundle = self
            .bundle
            .as_ref()
            .ok_or_else(|| "this file carries no installer package".to_owned())?;
        Ok(zup_windows::WindowsOperationHost {
            app_id: self
                .app_id
                .clone()
                .ok_or_else(|| "the session host was not discovered".to_owned())?,
            scope: context.scope,
            frontend: self.frontend,
            state_root: context.state_root.clone(),
            content: match (&self.acquired, &self.payload) {
                (Some(acquired), Some(payload)) => {
                    zup_windows::WindowsContent::Acquired(&acquired.manifest, payload)
                }
                _ => zup_windows::WindowsContent::Embedded(bundle),
            },
            acquire_prerequisites: true,
        })
    }
}

impl SessionHost for WindowsSessionHost {
    type Bound<'a> = zup_windows::WindowsOperationHost<'a>;

    fn acquire_content(
        &mut self,
        discovery: &mut SessionDiscovery,
        context: &zup_session::OperationContext,
        operation: zup_runtime::Operation,
        selection: &zup_runtime::OperationSelection,
        events: &mut (dyn FnMut(zup_presentation::InstallerEvent) + Send),
    ) -> Result<(), String> {
        if !self
            .bundle
            .as_ref()
            .is_some_and(|bundle| bundle.package().is_plan_only())
            || operation == zup_runtime::Operation::Uninstall
        {
            return Ok(());
        }
        let installer = discovery.installer();
        let config = installer.updates.as_ref().ok_or_else(|| {
            "this thin installer has no authenticated update repository".to_owned()
        })?;
        if matches!(operation, zup_runtime::Operation::Repair { .. })
            && context
                .ledger
                .as_ref()
                .and_then(zup_exec::InstallLedger::release_identity)
                .is_none()
        {
            return Err(
                "this installation has no authenticated release identity to repair from".into(),
            );
        }
        let action = zup_runtime::resolve_operation(
            operation,
            context.ledger.as_ref().map(|ledger| &ledger.version),
            &installer.app.version,
        )
        .map_err(|error| error.to_string())?;
        let request = zup_runtime::plan_request(
            installer,
            context.scope,
            context.ledger.as_ref(),
            selection,
            action,
            None,
        )
        .map_err(|error| error.to_string())?;
        let selected = zup_plan::select_components(&installer.components, &request.components)
            .map_err(|error| error.to_string())?;
        let selected_names = selected.iter().map(|id| id.as_str()).collect::<Vec<_>>();
        let components = zup_update::ComponentSelection::Only(&selected_names);
        let content_root = if context.scope == SelectedScope::Machine {
            state::peer_user_state_root().map_err(|error| error.to_string())?
        } else {
            context.state_root.clone()
        };
        let trust = zup_update::TrustContext::from_update_config(
            config,
            installer.app.id.clone(),
            content_root,
        );
        let (sink, mut receiver) = zup_acquire::ProgressSink::channel(128);
        let acquired = std::thread::scope(|scope| {
            scope.spawn(move || {
                while let Some(event) = receiver.blocking_recv() {
                    for event in zup_presentation::automation_events(&event) {
                        events(event);
                    }
                }
            });
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .map_err(|error| error.to_string())?
                .block_on(crate::acquire::acquire(trust, components, None, &[], sink))
                .map_err(|error| error.to_string())
        })?;
        if acquired.resolved.variant.target != installer.target
            || acquired.resolved.descriptor.app_id != installer.app.id
        {
            return Err(
                "the authenticated release does not match this installer's application and target"
                    .into(),
            );
        }
        if let Some(prior) = context
            .ledger
            .as_ref()
            .and_then(zup_exec::InstallLedger::release_identity)
            && prior.variant != acquired.resolved.variant.id
        {
            return Err("the authenticated release does not match the installed variant".into());
        }
        let payload =
            crate::acquire::payload_source(&acquired).map_err(|error| error.to_string())?;
        let package = zup_bundle::BundleWriter::encode_plan_only_plan(
            &acquired.manifest.plan,
            &acquired.manifest.plan.plugins,
        )
        .map_err(|error| error.to_string())?;
        discovery.build = zup_bundle::Package::from_bytes(package)
            .and_then(|package| package.build_plan())
            .map_err(|error| error.to_string())?
            .targets
            .into_iter()
            .next()
            .ok_or_else(|| "the release contains no target plan".to_owned())?;
        self.payload = Some(Arc::new(payload));
        self.acquired = Some(Arc::new(acquired));
        Ok(())
    }

    fn release_identity(&self) -> Option<zup_core::ReleaseIdentity> {
        self.acquired.as_ref().map(|acquired| acquired.identity())
    }

    fn bind<'a>(
        &'a self,
        context: &'a zup_session::OperationContext,
    ) -> Result<Self::Bound<'a>, String> {
        self.operation_host(context)
    }

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
        let mut installations = Vec::new();
        for scope in zup_preset_host::surface::scopes(&installer)
            .into_iter()
            .map(zup_preset_host::engine_scope)
        {
            let state_root = state::resolve_state_root(launch.state_root.clone(), scope)
                .map_err(|error| error.to_string())?;
            let ledger = zup_windows::InstallLedgerStore::new(&state_root)
                .load(&installer.app.id, scope)
                .map_err(|error| error.to_string())?;
            installations.push(zup_session::ScopedInstallation {
                scope,
                state_root,
                ledger,
            });
        }
        if launch.maintenance
            && installations
                .iter()
                .all(|installation| installation.ledger.is_none())
        {
            return Err("the installed application was not found".into());
        }
        self.executable = executable.to_path_buf();
        self.bundle = Some(bundle);
        self.app_id = Some(installer.app.id.clone());
        self.frontend = installer.frontend;
        Ok(SessionDiscovery {
            build,
            installations,
        })
    }

    fn backend(
        &self,
        context: &zup_session::OperationContext,
        prepared: &zup_runtime::PreparedOperation,
    ) -> Result<Arc<dyn zup_runtime::RuntimeBackend>, String> {
        let backend = if let Some(payload) = &self.payload {
            zup_windows::WindowsRuntimeBackend::from_acquired(
                Arc::clone(payload),
                self.acquired
                    .as_ref()
                    .ok_or_else(|| "acquired content has no authenticated identity".to_owned())?
                    .cache
                    .root()
                    .to_path_buf(),
            )
            .map_err(|error| error.to_string())?
        } else if prepared.action == zup_exec::LifecycleAction::Uninstall {
            zup_windows::WindowsRuntimeBackend::from_path(self.executable.clone(), None)
                .map_err(|error| error.to_string())?
        } else {
            let install = prepared
                .install_plan()
                .ok_or_else(|| "the operation planned no installation to execute".to_owned())?;
            zup_windows::WindowsRuntimeBackend::from_path_with_generated_files(
                self.executable.clone(),
                &context.state_root,
                context.scope,
                install,
                prepared.generated_files(),
                &prepared.transaction_plan,
            )
            .map_err(|error| error.to_string())?
        };
        Ok(Arc::new(backend))
    }

    fn materialize_preset(
        &self,
        discovery: &SessionDiscovery,
        context: &zup_session::OperationContext,
    ) -> Result<zup_session::PresetLaunch, String> {
        let installer = discovery.installer();
        let bundle = self
            .bundle
            .as_ref()
            .ok_or_else(|| "this file carries no installer package".to_owned())?;
        let installed = context.ledger.is_some();
        let capabilities = zup_preset_host::surface::capabilities(installer, installed);
        let installed: Option<(zup_core::InstalledPreset, PathBuf)> = context
            .ledger
            .as_ref()
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
                    &context.state_root,
                    &installer.app.id,
                    context.scope,
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
            stage: composed.stage.map(zup_preset_host::PresetStage::acquire),
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
        context: &zup_session::OperationContext,
        _prepared: &zup_runtime::PreparedOperation,
    ) -> Result<(), String> {
        let app_id = self
            .app_id
            .as_ref()
            .ok_or_else(|| "the session host was not discovered".to_owned())?;
        zup_windows::cleanup_app_payload_overlays(&context.state_root, app_id, context.scope)
            .map_err(|error| error.to_string())?;
        if let Some(acquired) = &self.acquired {
            acquired
                .record_retention(zup_acquire::CachePolicy::Auto)
                .map_err(|error| error.to_string())?;
        }
        Ok(())
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
            interactive: true,
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
            frontend: Frontend::Gui,
            acquired: None,
            payload: None,
        }
    }
}
