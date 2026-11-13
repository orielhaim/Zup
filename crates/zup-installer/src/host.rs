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
    acquired: Option<AcquiredContent>,
    handoff: Option<Arc<crate::handoff::Accepted>>,
    seed: Option<PathBuf>,
}

#[derive(Clone)]
struct AcquiredContent {
    image: Arc<zup_windows::AcquiredRuntime>,
    release: Arc<crate::acquire::Acquired>,
    payload: Arc<zup_bundle::AcquiredPayloadSource>,
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
            content: match &self.acquired {
                Some(content) => zup_windows::WindowsContent::Acquired(
                    &content.release.manifest,
                    &content.payload,
                ),
                None => zup_windows::WindowsContent::Embedded(bundle),
            },
            acquire_prerequisites: true,
        })
    }
}

impl SessionHost for WindowsSessionHost {
    type Bound<'a> = zup_windows::WindowsOperationHost<'a>;

    fn acquire_content(
        &mut self,
        build: &mut Arc<zup_core::TargetBuildPlan>,
        context: &zup_session::OperationContext,
        operation: zup_runtime::Operation,
        selection: &zup_runtime::OperationSelection,
        events: &mut (dyn FnMut(zup_presentation::InstallerEvent) + Send),
    ) -> Result<(), String> {
        if matches!(
            operation,
            zup_runtime::Operation::Rollback { .. } | zup_runtime::Operation::Cleanup { .. }
        ) {
            return Ok(());
        }
        if !self
            .bundle
            .as_ref()
            .is_some_and(|bundle| bundle.package().is_plan_only())
            && self.handoff.is_none()
            && !matches!(operation, zup_runtime::Operation::Rollback { revision } if context.ledger.as_ref().is_some_and(|record| record.retained.iter().any(|snapshot| snapshot.revision == revision && snapshot.release.is_some())))
            || operation == zup_runtime::Operation::Uninstall
        {
            return Ok(());
        }
        let installer = &build.installer;
        let action = zup_runtime::resolve_operation(
            operation,
            context
                .ledger
                .as_ref()
                .filter(|ledger| ledger.is_installed())
                .map(|ledger| &ledger.current.version),
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
        let selected = if let zup_runtime::Operation::Rollback { revision } = operation {
            context
                .ledger
                .as_ref()
                .and_then(|record| {
                    record
                        .retained
                        .iter()
                        .find(|snapshot| snapshot.revision == revision)
                })
                .ok_or("requested deployment is not retained")?
                .selected_components
                .clone()
        } else {
            zup_plan::select_components(&installer.components, &request.components)
                .map_err(|error| error.to_string())?
        };
        let selected_names = selected.iter().map(|id| id.as_str()).collect::<Vec<_>>();
        let components = zup_update::ComponentSelection::Only(&selected_names);
        let content_root = if context.scope == SelectedScope::Machine {
            state::peer_user_state_root(&build.installer.app.id)
                .map_err(|error| error.to_string())?
        } else {
            context.state_root.clone()
        };
        let trust = crate::acquire::source_context(
            installer,
            context.ledger.as_ref(),
            operation,
            content_root,
        )?;
        let (sink, mut receiver) = zup_acquire::ProgressSink::channel(128);
        let acquired = std::thread::scope(|scope| {
            scope.spawn(move || {
                while let Some(event) = receiver.blocking_recv() {
                    for event in zup_presentation::automation_events(&event) {
                        events(event);
                    }
                }
            });
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .map_err(|error| error.to_string())?;
            if let Some(handoff) = &self.handoff
                && !matches!(
                    operation,
                    zup_runtime::Operation::Rollback { .. }
                        | zup_runtime::Operation::Cleanup { .. }
                )
            {
                let mut resolver = zup_update::ReleaseResolver::new(
                    trust,
                    zup_acquire::HostProfile::native(),
                    zup_acquire::CachePolicy::Auto,
                    sink,
                )
                .map_err(|error| error.to_string())?;
                if let Some(seed) = &self.seed {
                    resolver = resolver.with_seed("local source", seed.clone());
                }
                runtime
                    .block_on(crate::acquire::acquire_closure(
                        &resolver,
                        handoff.acquired.resolved.clone(),
                        components,
                    ))
                    .map_err(|error| error.to_string())
            } else {
                let seeds = self.seed.iter().cloned().collect::<Vec<_>>();
                runtime
                    .block_on(crate::acquire::acquire(
                        trust, components, None, &seeds, sink,
                    ))
                    .map_err(|error| error.to_string())
            }
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
            .and_then(zup_plan::InstallationRecord::release_identity)
            && prior.variant != acquired.resolved.variant.id
        {
            return Err("the authenticated release does not match the installed variant".into());
        }
        crate::acquire::validate_selected_source(
            &acquired,
            self.bundle
                .as_ref()
                .ok_or("the session has no source package")?
                .plan(),
            context.ledger.as_ref(),
            operation,
        )?;
        let payload =
            crate::acquire::payload_source(&acquired).map_err(|error| error.to_string())?;
        *build = Arc::new(
            crate::acquire::target_plan(&acquired.manifest).map_err(|error| error.to_string())?,
        );
        let image = Arc::new(
            zup_windows::AcquiredRuntime::stage(&acquired).map_err(|error| error.to_string())?,
        );
        self.acquired = Some(AcquiredContent {
            image,
            release: Arc::new(acquired),
            payload: Arc::new(payload),
        });
        Ok(())
    }

    fn release_identity(&self) -> Option<zup_core::ReleaseIdentity> {
        self.acquired
            .as_ref()
            .map(|content| content.release.identity())
            .or_else(|| {
                self.handoff
                    .as_ref()
                    .map(|handoff| handoff.acquired.identity())
            })
            .or_else(|| {
                self.bundle
                    .as_ref()
                    .and_then(|bundle| bundle.package().release_identity().cloned())
            })
    }

    fn bind<'a>(
        &'a self,
        context: &'a zup_session::OperationContext,
    ) -> Result<Self::Bound<'a>, String> {
        self.operation_host(context)
    }

    fn content_executable(&self, _executable: &Path) -> PathBuf {
        self.acquired.as_ref().map_or_else(
            || self.executable.clone(),
            |content| content.image.path().to_path_buf(),
        )
    }

    fn refresh_installation(
        &mut self,
        context: &zup_session::OperationContext,
    ) -> Result<Option<Arc<zup_core::TargetBuildPlan>>, String> {
        let Some(record) = context
            .ledger
            .as_ref()
            .filter(|record| record.is_installed())
        else {
            return Ok(None);
        };
        let maintenance = record
            .current
            .maintenance
            .as_ref()
            .ok_or("installation has no verified maintenance generation")?;
        let runtime = maintenance
            .descriptor()
            .files()
            .iter()
            .find(|file| {
                file.executable
                    && file.path.as_str()
                        == format!("maintenance{}", record.target.executable_suffix())
            })
            .ok_or("maintenance generation does not contain its runtime executable")?;
        let path = maintenance
            .directory()
            .join(runtime.path.as_str())
            .map_err(|error| error.to_string())?;
        self.acquired = None;
        self.handoff = None;
        self.describe(Path::new(path.as_str())).map(Some)
    }

    fn describe(&mut self, executable: &Path) -> Result<Arc<zup_core::TargetBuildPlan>, String> {
        let bundle = package::open_bundle_if_present(executable)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| {
                "this file carries no installer package; run the installer you downloaded"
                    .to_owned()
            })?;
        let build = match &self.handoff {
            Some(handoff) => crate::acquire::target_plan(&handoff.acquired.manifest)
                .map_err(|error| error.to_string())?,
            None => package::target_plan(&bundle).map_err(|error| error.to_string())?,
        };
        self.executable = executable.to_path_buf();
        self.bundle = Some(bundle);
        self.app_id = Some(build.installer.app.id.clone());
        self.frontend = build.installer.frontend;
        Ok(Arc::new(build))
    }

    fn observe(
        &self,
        installer: &zup_core::Installer,
        scope: SelectedScope,
        state_root: Option<&Path>,
    ) -> Result<zup_session::OperationContext, String> {
        let state_root =
            state::resolve_state_root(&installer.app.id, state_root.map(Path::to_path_buf), scope)
                .map_err(|error| error.to_string())?;
        let ledger = zup_windows::InstallationRecordStore::new(&state_root)
            .observe(&installer.app.id, scope)
            .map_err(|error| error.to_string())?;
        Ok(zup_session::OperationContext {
            scope,
            state_root,
            ledger,
        })
    }

    fn backend(
        &self,
        context: &zup_session::OperationContext,
        prepared: &zup_runtime::PreparedOperation,
    ) -> Result<Arc<dyn zup_runtime::RuntimeBackend>, String> {
        let backend = if matches!(
            prepared.action,
            zup_plan::LifecycleAction::Rollback { .. } | zup_plan::LifecycleAction::Cleanup { .. }
        ) {
            zup_windows::WindowsRuntimeBackend::from_path(self.executable.clone(), None)
                .map_err(|error| error.to_string())?
        } else if let Some(content) = &self.acquired {
            zup_windows::WindowsRuntimeBackend::from_acquired(
                Arc::clone(&content.payload),
                content.release.cache.root().to_path_buf(),
                content
                    .release
                    .handoff(
                        context.scope,
                        crate::lifecycle::handoff_mode(prepared.action),
                    )
                    .map_err(|error| error.to_string())?,
                Arc::clone(&content.image),
            )
        } else {
            zup_windows::WindowsRuntimeBackend::from_path(self.executable.clone(), None)
                .map_err(|error| error.to_string())?
        }
        .with_generated_files(prepared, &context.state_root)?;
        Ok(Arc::new(backend))
    }

    fn recover<'a>(
        &'a self,
        context: &'a zup_session::OperationContext,
        transaction: zup_core::TransactionId,
        request: zup_transaction::RecoveryRequest,
        control: &'a zup_runtime::OperationControl,
    ) -> zup_runtime::RuntimeFuture<
        'a,
        Result<zup_runtime::InstallOutcome, zup_runtime::SessionError>,
    > {
        Box::pin(async move {
            use zup_transaction::TransactionStore as _;
            let build = match &self.acquired {
                Some(content) => crate::acquire::target_plan(&content.release.manifest)
                    .map_err(|error| zup_runtime::SessionError::Protocol(error.to_string()))?,
                None => package::target_plan(self.bundle.as_ref().ok_or_else(|| {
                    zup_runtime::SessionError::Protocol(
                        "recovery requires an authenticated application package".into(),
                    )
                })?)
                .map_err(|error| zup_runtime::SessionError::Protocol(error.to_string()))?,
            };
            let app = &build.installer.app;
            let version = if context.scope == SelectedScope::User {
                let record =
                    zup_transaction::FilesystemTransactionStore::private_at(&context.state_root)
                        .load(&transaction)
                        .map_err(|error| zup_runtime::SessionError::Protocol(error.to_string()))?;
                if record.app_id != app.id
                    || record.scope != context.scope
                    || record.target != build.installer.target
                {
                    return Err(zup_runtime::SessionError::Protocol(
                        "recovery transaction belongs to a different application, scope, or target"
                            .into(),
                    ));
                }
                record.app_version
            } else {
                app.version.clone()
            };
            let request = zup_runtime::RuntimeRequest {
                operation: zup_runtime::RuntimeOperation::Recovery {
                    transaction,
                    request,
                },
                target: build.installer.target,
                app_id: app.id.clone(),
                app_version: version,
                scope: context.scope,
                state_root: context.state_root.clone(),
            };
            let backend = zup_windows::WindowsRuntimeBackend::for_recovery();
            zup_runtime::execute(&backend, request, control).await
        })
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
                let directory = PathBuf::from(
                    ledger
                        .current
                        .maintenance
                        .as_ref()
                        .ok_or("the installed preset has no verified maintenance generation")?
                        .directory()
                        .as_str(),
                );
                Ok::<_, String>((runtime, directory))
            })
            .transpose()?;
        let source = match &installed {
            Some((runtime, directory)) => preset::Source::Installed {
                directory,
                runtime,
                target: &installer.target,
            },
            None => {
                let preset = installer
                    .preset
                    .as_ref()
                    .ok_or_else(|| "this installer was composed without a preset".to_owned())?;
                preset::Source::Composed {
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

    #[cfg(feature = "gui")]
    fn update(
        &mut self,
        context: &zup_session::OperationContext,
        request: &zup_runtime::UpdateRequest,
        control: &zup_runtime::OperationControl,
        status: &mut (dyn FnMut(zup_preset_protocol::UpdateState) + Send),
    ) -> Result<zup_session::SessionOutcome, String> {
        use zup_acquire::CachePolicy;
        use zup_preset_protocol::UpdateState;
        use zup_update::{ComponentSelection, TrustContext};
        let scope = context.scope;
        let state_root = &context.state_root;
        let bundle = package::open_bundle(&self.executable).map_err(|error| error.to_string())?;
        let build = package::target_plan(&bundle).map_err(|error| error.to_string())?;
        let installer = &build.installer;
        let config = installer
            .updates
            .as_ref()
            .ok_or_else(|| "this application is not configured for updates".to_owned())?;
        let root =
            state::resolve_state_root(&installer.app.id, Some(state_root.to_path_buf()), scope)
                .map_err(|error| error.to_string())?;
        let ledger = zup_windows::InstallationRecordStore::new(&root)
            .observe(&installer.app.id, scope)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "the installed application was not found".to_owned())?;
        let current = ledger.current.version.to_string();
        let update_root = if scope == SelectedScope::Machine {
            state::peer_user_state_root(&installer.app.id).map_err(|error| error.to_string())?
        } else {
            root.clone()
        };
        let context = TrustContext::from_update_config(
            config,
            installer.app.id.clone(),
            installer.install.scope,
            update_root,
        );
        let (sink, mut receiver) = zup_acquire::ProgressSink::channel(64);
        let tokio = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(|error| error.to_string())?;

        status(UpdateState::Checking {
            detail: "Resolving the update channel…".into(),
        });
        let resolver = zup_update::ReleaseResolver::new(
            context,
            zup_acquire::HostProfile::native(),
            CachePolicy::Auto,
            sink,
        )
        .map_err(|error| error.to_string())?;
        let Some(resolved) = tokio.block_on(update_acquisition(
            resolver.resolve(),
            &mut receiver,
            control.cancellation(),
            request,
            status,
        ))?
        else {
            return Ok(zup_session::SessionOutcome::Deployment(
                zup_runtime::InstallOutcome::Cancelled,
            ));
        };
        let available = resolved.descriptor.version.clone();
        let candidate = zup_core::ReleaseCandidate {
            version: available
                .parse()
                .map_err(|error: semver::Error| error.to_string())?,
            release: resolved.descriptor.release_digest,
        };
        if let zup_runtime::UpdateRequest::Install {
            expected: Some(expected),
        } = request
            && expected != &candidate
        {
            return Err(
                "The available update changed. Check for updates again before installing.".into(),
            );
        }
        if !candidate.is_update_for(&ledger.current.version, ledger.release_identity()) {
            status(UpdateState::UpToDate { current });
            return Ok(zup_session::SessionOutcome::UpdateChecked);
        }
        if matches!(request, zup_runtime::UpdateRequest::Check) {
            status(UpdateState::Available { current, candidate });
            return Ok(zup_session::SessionOutcome::UpdateChecked);
        }
        let manifest =
            crate::acquire::parse_manifest(&resolved).map_err(|error| error.to_string())?;
        let selected_names = ledger
            .current
            .selected_components
            .iter()
            .map(|id| id.as_str())
            .collect::<Vec<_>>();
        let closure = crate::acquire::component_closure(
            &resolved,
            &manifest,
            ComponentSelection::Only(&selected_names),
        )
        .map_err(|error| error.to_string())?;
        let cancellation = control.cancellation();
        let Some(acquired) = tokio.block_on(update_acquisition(
            crate::acquire::barrier_with_cancellation(
                &resolver,
                resolved,
                manifest,
                closure,
                Arc::new(move || cancellation.is_cancelled()),
            ),
            &mut receiver,
            control.cancellation(),
            request,
            status,
        ))?
        else {
            return Ok(zup_session::SessionOutcome::Deployment(
                zup_runtime::InstallOutcome::Cancelled,
            ));
        };
        status(UpdateState::Installing {
            detail: acquired.estimate().to_string(),
        });
        let updated_build = Arc::new(
            crate::acquire::target_plan(&acquired.manifest).map_err(|error| error.to_string())?,
        );
        if updated_build.installer.app.id != installer.app.id
            || updated_build.installer.target != installer.target
            || updated_build.installer.app.version != candidate.version
            || updated_build.installer.frontend != installer.frontend
            || !updated_build
                .installer
                .install
                .scope
                .scopes()
                .contains(&scope)
        {
            return Err(
                "the authenticated update belongs to a different application or target".into(),
            );
        }
        if ledger
            .release_identity()
            .is_some_and(|identity| identity.variant != acquired.resolved.variant.id)
        {
            return Err(
                "the authenticated update differs from the installed release variant".into(),
            );
        }
        let payload =
            Arc::new(crate::acquire::payload_source(&acquired).map_err(|error| error.to_string())?);
        let image = Arc::new(
            zup_windows::AcquiredRuntime::stage(&acquired).map_err(|error| error.to_string())?,
        );
        self.acquired = Some(AcquiredContent {
            image,
            release: Arc::new(acquired),
            payload,
        });
        let host = &*self;
        let context = zup_session::OperationContext {
            scope,
            state_root: root,
            ledger: Some(ledger),
        };
        let bound = host.bind(&context)?;
        let cancellation = control.cancellation();
        let prepared = zup_runtime::prepare_operation(
            &zup_runtime::OperationInput {
                build: &updated_build,
                installer: &host.content_executable(&host.executable),
                scope,
                operation: zup_runtime::Operation::Upgrade,
                selection: Default::default(),
                prior: context.ledger.as_ref(),
                state_root: &context.state_root,
                cancellation: &|| cancellation.is_cancelled(),
            },
            &bound,
        );
        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err(error @ zup_runtime::OperationError::RecoveryRequired(transaction)) => {
                return Ok(zup_session::SessionOutcome::Deployment(
                    zup_runtime::InstallOutcome::RecoveryRequired {
                        transaction: Some(transaction.0),
                        detail: error.to_string(),
                    },
                ));
            }
            Err(zup_runtime::OperationError::Cancelled) => {
                return Ok(zup_session::SessionOutcome::Deployment(
                    zup_runtime::InstallOutcome::Cancelled,
                ));
            }
            Err(error) => return Err(error.to_string()),
        };
        let result = tokio.block_on(zup_session::run_operation(
            host,
            &context,
            &updated_build,
            &prepared,
            control,
        ));
        let (outcome, _) = zup_session::reconcile_operation(
            host,
            &context,
            &prepared,
            zup_runtime::outcome_of(result),
        );
        if matches!(outcome, zup_runtime::InstallOutcome::Committed) {
            status(UpdateState::UpToDate { current: available });
        }
        Ok(zup_session::SessionOutcome::Deployment(outcome))
    }

    fn copy_diagnostics(&self, text: &str) {
        let _ = std::process::Command::new("clip").arg(text).output();
    }

    fn after_commit(
        &self,
        context: &zup_session::OperationContext,
        prepared: &zup_runtime::PreparedOperation,
    ) -> Result<(), String> {
        let app_id = self
            .app_id
            .as_ref()
            .ok_or_else(|| "the session host was not discovered".to_owned())?;
        if prepared.action == zup_plan::LifecycleAction::Uninstall {
            zup_windows::cleanup_app_payload_overlays(&context.state_root, app_id, context.scope)
                .map_err(|error| error.to_string())?;
        }
        if let Some(acquired) = &self.acquired {
            acquired
                .release
                .maintain_cache(zup_acquire::CachePolicy::Auto)
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
            scope: args.scope.map(Into::into),
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
            scope: args.scope.map(Into::into),
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
            handoff: None,
            seed: None,
        }
    }
}

pub(crate) fn command() -> clap::Command {
    crate::terminal::command()
        .subcommand(
            <crate::maintenance::RecoveryArgs as clap::Args>::augment_args(
                clap::Command::new("recover")
                    .about("Reconcile a protected interrupted transaction"),
            ),
        )
        .subcommand(
            clap::Command::new("update")
                .about("Check for and install authenticated updates")
                .subcommand(clap::Command::new("check"))
                .arg(
                    clap::Arg::new("source")
                        .long("source")
                        .value_name("PATH")
                        .value_parser(clap::value_parser!(PathBuf))
                        .value_hint(clap::ValueHint::DirPath),
                ),
        )
}

pub(crate) fn terminal(frontend: Frontend) -> miette::Result<()> {
    use clap::FromArgMatches;
    let mut matches = crate::terminal::parse_matches(command());
    if matches.subcommand_name() == Some("update") {
        let (_, update) = matches
            .remove_subcommand()
            .expect("selected native operation");
        let cli =
            crate::terminal::Cli::from_arg_matches(&matches).expect("validated terminal arguments");
        let output = cli.output.into();
        let args = crate::maintenance::UpdateArgs {
            command: update
                .subcommand_name()
                .map(|_| crate::maintenance::UpdateCommand::Check),
            scope: cli.scope,
            output: cli.output,
            non_interactive: cli.non_interactive,
            yes: cli.yes,
            plain: cli.plain,
            state_root: cli.state_root,
            source: update.get_one::<PathBuf>("source").cloned(),
        };
        let result = crate::maintenance::run_update(
            crate::run::RuntimeContext::new(frontend).with_output(output),
            args,
        );
        if let Err(error) = &result
            && !crate::execute::already_reported(error)
            && error
                .downcast_ref::<crate::terminal::ReportedFailure>()
                .is_none()
        {
            crate::emit_failure(output, error);
        }
        return result;
    }
    let cli =
        crate::terminal::Cli::from_arg_matches(&matches).expect("validated terminal arguments");
    crate::terminal::run_cli(WindowsSessionHost::default(), frontend, cli)
}

pub(crate) fn terminal_handoff(
    context: crate::run::RuntimeContext,
    action: zup_plan::LifecycleAction,
    args: &crate::cli::LifecycleArgs,
    accepted: crate::handoff::Accepted,
) -> miette::Result<()> {
    let operation = match action {
        zup_plan::LifecycleAction::Cleanup { keep } => crate::terminal::Command::Cleanup { keep },
        zup_plan::LifecycleAction::Install => {
            crate::terminal::Command::Install(crate::terminal::Components {
                enable: args.enable.clone(),
                disable: args.disable.clone(),
            })
        }
        zup_plan::LifecycleAction::Upgrade => {
            crate::terminal::Command::Upgrade(crate::terminal::Components {
                enable: args.enable.clone(),
                disable: args.disable.clone(),
            })
        }
        zup_plan::LifecycleAction::Downgrade => {
            crate::terminal::Command::Downgrade(crate::terminal::Components {
                enable: args.enable.clone(),
                disable: args.disable.clone(),
            })
        }
        zup_plan::LifecycleAction::Rollback { revision } => {
            crate::terminal::Command::Rollback { revision }
        }
        zup_plan::LifecycleAction::Modify => {
            crate::terminal::Command::Modify(crate::terminal::Components {
                enable: args.enable.clone(),
                disable: args.disable.clone(),
            })
        }
        zup_plan::LifecycleAction::Repair { force } => crate::terminal::Command::Repair { force },
        zup_plan::LifecycleAction::Uninstall => crate::terminal::Command::Uninstall,
    };
    let cli = crate::terminal::Cli {
        operation: Some(operation),
        scope: Some(match accepted.scope {
            SelectedScope::User => crate::terminal::Scope::User,
            SelectedScope::Machine => crate::terminal::Scope::Machine,
        }),
        state_root: args.state_root.clone(),
        install_directory: args.install_directory.clone(),
        non_interactive: args.non_interactive,
        yes: args.yes,
        plain: args.plain,
        output: args.output,
    };
    let host = WindowsSessionHost {
        handoff: Some(Arc::new(accepted)),
        seed: args.source.clone(),
        ..Default::default()
    };
    crate::terminal::run_cli(host, context.frontend, cli)
}

#[cfg(feature = "gui")]
async fn update_acquisition<T, E: std::fmt::Display>(
    work: impl std::future::Future<Output = Result<T, E>>,
    receiver: &mut zup_acquire::ProgressReceiver,
    cancellation: zup_runtime::CancellationHandle,
    request: &zup_runtime::UpdateRequest,
    status: &mut (dyn FnMut(zup_preset_protocol::UpdateState) + Send),
) -> Result<Option<T>, String> {
    tokio::pin!(work);
    let mut receiving = true;
    loop {
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Ok(None),
            result = &mut work => return result.map(Some).map_err(|error| error.to_string()),
            event = receiver.recv(), if receiving => {
                if let Some(event) = event {
                    for event in zup_presentation::automation_events(&event) {
                        if let zup_presentation::InstallerEvent::Progress { label, .. } = event {
                            status(if matches!(request, zup_runtime::UpdateRequest::Check) {
                                zup_preset_protocol::UpdateState::Checking { detail: label }
                            } else {
                                zup_preset_protocol::UpdateState::Installing { detail: label }
                            });
                        }
                    }
                } else { receiving = false; }
            }
        }
    }
}
