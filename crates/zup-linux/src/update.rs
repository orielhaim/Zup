use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use zup_acquire::{AcquisitionItem, CachePolicy, ContentReason, HostProfile, ProgressSink, Verify};
use zup_core::{SelectedScope, TargetBuildPlan};
use zup_session::{OperationContext, SessionHost};
use zup_update::{ReleaseResolver, ResolvedRelease, TrustContext, content};

struct UpdateSession {
    resolver: ReleaseResolver,
    release: ResolvedRelease,
    available: bool,
}

struct UpdateInstaller {
    host: crate::LinuxSessionHost,
    build: Arc<TargetBuildPlan>,
    stage: tempfile::TempDir,
}

impl UpdateSession {
    async fn resolve(
        host: &crate::LinuxSessionHost,
        build: &TargetBuildPlan,
        scope: SelectedScope,
        state_root: Option<&Path>,
        source: Option<&Path>,
        progress: ProgressSink,
    ) -> Result<Self, String> {
        let installer = &build.installer;
        let context = host.observe(installer, scope, state_root)?;
        let ledger = context
            .ledger
            .as_ref()
            .ok_or("this application is not installed in the selected scope")?;
        let config = installer
            .updates
            .as_ref()
            .ok_or("this application is not configured for updates")?;
        let cache_root = if scope == SelectedScope::Machine {
            crate::paths::user_state_root().map_err(|error| error.to_string())?
        } else {
            context.state_root.clone()
        };
        let trust = TrustContext::from_update_config(config, installer.app.id.clone(), cache_root);
        let mut profile = HostProfile::native();
        profile.frontends = vec![installer.frontend.as_str().to_owned()];
        let mut resolver = ReleaseResolver::new(trust, profile, CachePolicy::Auto, progress)
            .map_err(|error| error.to_string())?;
        if let Some(source) = source {
            resolver = resolver.with_seed("local source", source);
        }
        let release = resolver
            .resolve()
            .await
            .map_err(|error| error.to_string())?;
        let manifest = content::parse_manifest(&release).map_err(|error| error.to_string())?;
        if manifest.plan.installer.app.id != installer.app.id
            || manifest.plan.installer.app.version.to_string() != release.descriptor.version
            || manifest.target != ledger.target
            || !manifest
                .plan
                .installer
                .install
                .scope
                .scopes()
                .contains(&scope)
        {
            return Err("the authenticated update does not match the installed application, target, version, or scope".into());
        }
        release
            .require_runtime()
            .map_err(|error| error.to_string())?;
        content::declared_content(&manifest).map_err(|error| error.to_string())?;
        let planned = content::target_plan(&manifest).map_err(|error| error.to_string())?;
        let errors = crate::capabilities::operation_capability_errors(&planned, scope);
        if !errors.is_empty() {
            return Err(errors.join("\n"));
        }
        let version = semver::Version::parse(&release.descriptor.version)
            .map_err(|error| error.to_string())?;
        let available = version > ledger.version;
        Ok(Self {
            resolver,
            release,
            available,
        })
    }

    async fn acquire(
        self,
        cancellation: Arc<dyn zup_acquire::Cancellation>,
    ) -> Result<UpdateInstaller, String> {
        let manifest = content::parse_manifest(&self.release).map_err(|error| error.to_string())?;
        let mut plan = content::component_closure(
            &self.release,
            &manifest,
            zup_update::ComponentSelection::All,
        )
        .map_err(|error| error.to_string())?;
        let runtime = self
            .release
            .require_runtime()
            .map_err(|error| error.to_string())?;
        plan.extend(vec![AcquisitionItem::new(runtime, ContentReason::Runtime)])
            .map_err(|error| error.to_string())?;
        let acquired = content::barrier_with_cancellation(
            &self.resolver,
            self.release,
            manifest,
            plan,
            cancellation.clone(),
        )
        .await
        .map_err(|error| error.to_string())?;
        tokio::task::spawn_blocking(move || materialize(acquired, runtime, cancellation))
            .await
            .map_err(|error| error.to_string())?
    }
}

fn materialize(
    acquired: content::Acquired,
    runtime: zup_acquire::ContentDescriptor,
    cancellation: Arc<dyn zup_acquire::Cancellation>,
) -> Result<UpdateInstaller, String> {
    if cancellation.is_cancelled() {
        return Err("update cancelled before installation".into());
    }
    let stage = tempfile::Builder::new()
        .prefix("zup-update-")
        .tempdir()
        .map_err(|error| error.to_string())?;
    let read = |descriptor: &zup_acquire::ContentDescriptor| -> Result<Vec<u8>, String> {
        acquired
            .cache
            .get(descriptor, Verify::Full)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| format!("verified update content {} is missing", descriptor.digest))?
            .read_to_end()
            .map_err(|error| error.to_string())
    };
    let mut compressed = BTreeMap::new();
    for digest in acquired.manifest.content_digests() {
        if cancellation.is_cancelled() {
            return Err("update cancelled before installation".into());
        }
        let entry = acquired
            .resolved
            .catalog
            .entry(&digest)
            .ok_or_else(|| format!("update content {digest} is not catalogued"))?;
        let bytes = read(&entry.descriptor(zup_acquire::ContentKind::Payload))?;
        compressed.insert(
            digest,
            zstd::stream::encode_all(bytes.as_slice(), 3).map_err(|error| error.to_string())?,
        );
    }
    let package = stage.path().join("package.zup");
    zup_bundle::BundleWriter::write_plan(
        &acquired.manifest.plan,
        &compressed,
        Some(&acquired.identity()),
        &package,
    )
    .map_err(|error| error.to_string())?;
    let runtime_path = stage.path().join("runtime");
    std::fs::write(&runtime_path, read(&runtime)?).map_err(|error| error.to_string())?;
    let image = zup_binary::Executable::read(&runtime_path).map_err(|error| error.to_string())?;
    image
        .refuse_target(&acquired.manifest.target)
        .map_err(|error| error.to_string())?;
    image
        .refuse_frontend(acquired.manifest.frontend)
        .map_err(|error| error.to_string())?;
    let preset = acquired
        .manifest
        .preset
        .map(|preset| {
            let entry = acquired
                .resolved
                .catalog
                .entry(&preset.digest)
                .ok_or("the update preset is not catalogued")?;
            read(&entry.descriptor(zup_acquire::ContentKind::Payload))
        })
        .transpose()?;
    let executable = stage.path().join("setup");
    zup_compose::compose(
        &acquired.manifest.target,
        &runtime_path,
        &executable,
        &std::fs::read(&package).map_err(|error| error.to_string())?,
        preset.as_deref(),
    )
    .map_err(|error| error.to_string())?;
    let mut host = crate::LinuxSessionHost::open(executable.clone());
    let build = host.describe(&executable)?;
    Ok(UpdateInstaller { host, build, stage })
}

struct UpdateCancellation(pub zup_runtime::CancellationHandle);
impl zup_acquire::Cancellation for UpdateCancellation {
    fn is_cancelled(&self) -> bool {
        self.0.is_cancelled()
    }
}

pub(crate) fn prepare(
    host: &mut crate::LinuxSessionHost,
    build: &Arc<TargetBuildPlan>,
    context: &OperationContext,
    source: Option<&Path>,
    request: &zup_runtime::UpdateRequest,
    cancellation: zup_runtime::CancellationHandle,
    events: &mut (dyn FnMut(zup_presentation::InstallerEvent) + Send),
) -> Result<zup_session::UpdatePreparation, String> {
    let (progress, mut receiver) = ProgressSink::channel(64);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    runtime.block_on(async {
        let work = async {
            let session = UpdateSession::resolve(host, build, context.scope, Some(&context.state_root), source, progress).await?;
            let version = session.release.descriptor.version.clone();
            if !session.available { return Ok((zup_session::UpdatePreparation::UpToDate { version }, None)); }
            if matches!(request, zup_runtime::UpdateRequest::Check) { return Ok((zup_session::UpdatePreparation::Available { version }, None)); }
            if let zup_runtime::UpdateRequest::Install { version: Some(expected) } = request && expected != &version {
                return Err("The available update changed. Check for updates again before installing.".into());
            }
            let updated = session.acquire(Arc::new(UpdateCancellation(cancellation.clone()))).await?;
            Ok((zup_session::UpdatePreparation::Ready { build: updated.build.clone() }, Some(updated)))
        };
        tokio::pin!(work);
        let signal = tokio::signal::ctrl_c();
        tokio::pin!(signal);
        let mut receiving = true;
        let result: Result<_, String> = loop {
            tokio::select! {
                result = &mut work => break result,
                event = receiver.recv(), if receiving => if let Some(event) = event {
                    for event in zup_presentation::automation_events(&event) {
                        if !matches!(event, zup_presentation::InstallerEvent::Completed { .. } | zup_presentation::InstallerEvent::Failed { .. }) { events(event); }
                    }
                } else { receiving = false; },
                signal = &mut signal => {
                    signal.map_err(|error| error.to_string())?;
                    cancellation.cancel();
                    return Err("update cancelled before installation".into());
                },
                _ = cancellation.cancelled() => return Err("update cancelled before installation".into()),
            }
        };
        while let Ok(event) = receiver.try_recv() {
            for event in zup_presentation::automation_events(&event) {
                if !matches!(event, zup_presentation::InstallerEvent::Completed { .. } | zup_presentation::InstallerEvent::Failed { .. }) { events(event); }
            }
        }
        result
    }).map(|(preparation, updated)| {
        if let Some(updated) = updated {
            host.installer = updated.host.installer;
            host.update_stage = Some(Arc::new(updated.stage));
        }
        preparation
    })
}

pub(crate) fn run(
    mut host: crate::LinuxSessionHost,
    context: &OperationContext,
    request: &zup_runtime::UpdateRequest,
    control: &zup_runtime::OperationControl,
    status: &mut (dyn FnMut(zup_preset_protocol::UpdateState) + Send),
) -> Result<zup_runtime::InstallOutcome, String> {
    use zup_preset_protocol::UpdateState;
    let executable = host.installer.clone();
    let build = host.describe(&executable)?;
    let context = host.observe(&build.installer, context.scope, Some(&context.state_root))?;
    let scope = context.scope;
    status(UpdateState::Checking {
        detail: "Checking the authenticated update channel".into(),
    });
    let updated = prepare(
        &mut host,
        &build,
        &context,
        None,
        request,
        control.cancellation(),
        &mut |event| {
            if let zup_presentation::InstallerEvent::Progress { label, .. } = event {
                status(UpdateState::Installing { detail: label });
            }
        },
    );
    let updated = match updated {
        Ok(updated) => updated,
        Err(_) if control.cancellation().is_cancelled() => {
            return Ok(zup_runtime::InstallOutcome::Cancelled);
        }
        Err(error) => return Err(error),
    };
    let build = match updated {
        zup_session::UpdatePreparation::Ready { build } => build,
        zup_session::UpdatePreparation::Available { version } => {
            status(UpdateState::Available {
                current: context
                    .ledger
                    .as_ref()
                    .expect("installed update")
                    .version
                    .to_string(),
                available: version,
            });
            return Ok(zup_runtime::InstallOutcome::Committed);
        }
        zup_session::UpdatePreparation::UpToDate { version } => {
            status(UpdateState::UpToDate { current: version });
            return Ok(zup_runtime::InstallOutcome::Committed);
        }
    };
    status(UpdateState::Installing {
        detail: format!("Installing {}", build.installer.app.version),
    });
    let bound = host.bind(&context)?;
    let prepared = zup_runtime::prepare_operation(
        &zup_runtime::OperationInput {
            build: &build,
            installer: &host.installer,
            scope,
            operation: zup_runtime::Operation::Upgrade,
            selection: Default::default(),
            prior: context.ledger.as_ref(),
            state_root: &context.state_root,
            cancellation: &|| control.cancellation().is_cancelled(),
        },
        &bound,
    )
    .map_err(|error| error.to_string())?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    let outcome = runtime.block_on(zup_session::run_operation(
        &host, &context, &build, &prepared, control,
    ));
    let (outcome, _) = zup_session::reconcile_operation(
        &host,
        &context,
        &prepared,
        zup_runtime::outcome_of(outcome),
    );
    Ok(outcome)
}
