use std::path::{Path, PathBuf};
use std::sync::Arc;

use zup_acquire::CachePolicy;
use zup_core::SelectedScope;
use zup_plan::LifecycleAction;
use zup_presentation::OutputFormat;
use zup_runtime::{
    ExecutionPolicy, Operation, OperationInput, OperationSelection, RuntimeRequest,
    prepare_operation, resolve_operation, runtime_request,
};
use zup_windows::{WindowsContent, WindowsOperationHost, WindowsRuntimeBackend};

use crate::acquire;
use crate::cli::LifecycleArgs;
use crate::execute;
use crate::frontend;
use crate::maintenance as state;
use crate::package;
use crate::run::RuntimeContext;

#[derive(Clone)]
pub struct PreparedRuntime {
    pub(crate) request: RuntimeRequest,
    pub(crate) backend: Arc<WindowsRuntimeBackend>,
    pub(crate) action: LifecycleAction,
}

pub fn apply(context: RuntimeContext, args: LifecycleArgs) -> miette::Result<()> {
    if args.ui {
        return frontend::graphical_for(context, args);
    }
    transition(context, Operation::Apply, &args, policy_for(&args))
}

pub fn run(context: RuntimeContext, verb: Verb, args: LifecycleArgs) -> miette::Result<()> {
    if args.ui && context.frontend == zup_core::Frontend::Gui {
        return frontend::graphical_for(context, args);
    }
    if args.ui {
        return Err(miette::miette!(
            "this build has no graphical surface; omit --ui"
        ));
    }
    transition(context, verb.action(), &args, policy_for(&args))
}

#[derive(Debug, Clone, Copy)]
pub enum Verb {
    Cleanup {
        keep: u16,
    },
    Modify,
    Repair {
        force: bool,
    },
    Upgrade,
    Downgrade,
    Rollback {
        revision: zup_core::ActivationRevision,
    },
}

impl Verb {
    pub fn action(self) -> Operation {
        match self {
            Self::Cleanup { keep } => Operation::Cleanup { keep },
            Self::Modify => Operation::Modify,
            Self::Repair { force } => Operation::Repair { force },
            Self::Upgrade => Operation::Upgrade,
            Self::Downgrade => Operation::Downgrade,
            Self::Rollback { revision } => Operation::Rollback { revision },
        }
    }
}

fn policy_for(args: &LifecycleArgs) -> ExecutionPolicy {
    if args.non_interactive || args.yes {
        ExecutionPolicy::NonInteractive
    } else {
        ExecutionPolicy::Interactive
    }
}

pub fn direct_launch(context: RuntimeContext) -> miette::Result<()> {
    let executable = package::current_executable()?;
    let bundle = package::open_bundle_if_present(&executable)?.ok_or_else(|| {
        miette::miette!("this file carries no installer package; run the installer you downloaded")
    })?;
    match context.frontend {
        zup_core::Frontend::Gui => frontend::graphical(context, &executable, &bundle),
        zup_core::Frontend::Console => crate::host::terminal(context.frontend),
        zup_core::Frontend::Headless => Err(miette::miette!(
            "this runtime needs an operation.\n\n  Usage: {} <install|modify|repair|update|uninstall> [OPTIONS]",
            crate::cli::invoked_name()
        )),
    }
}

pub fn transition(
    context: RuntimeContext,
    request: Operation,
    args: &LifecycleArgs,
    policy: ExecutionPolicy,
) -> miette::Result<()> {
    let executable = package::current_executable()?;

    if let Some(handoff_path) = &args.handoff {
        return run_handoff(context, &executable, request, args, policy, handoff_path);
    }

    match package::open_bundle_if_present(&executable)? {
        Some(bundle)
            if bundle.package().is_plan_only()
                && !matches!(
                    request,
                    Operation::Rollback { .. } | Operation::Cleanup { .. }
                ) =>
        {
            run_graph_transition(request, args, &bundle, policy)
        }
        Some(bundle) => {
            let build = package::target_plan(&bundle)?;
            let scope = build
                .installer
                .install
                .scope
                .select(args.scope.map(Into::into))
                .map_err(|error| {
                    miette::miette!("{error}; specify --scope user or --scope machine")
                })?;
            run_embedded(
                request,
                context,
                scope,
                args,
                policy,
                OutputFormat::from(args.output),
            )
        }
        None => Err(miette::miette!(
            "this file carries no installer package; run the installer you downloaded"
        )),
    }
}

pub fn run_embedded(
    request: Operation,
    _context: RuntimeContext,
    scope: SelectedScope,
    args: &LifecycleArgs,
    policy: ExecutionPolicy,
    output: OutputFormat,
) -> miette::Result<()> {
    let prepared = prepare_embedded_transition(
        request,
        scope,
        args.state_root.clone(),
        args.enable.clone(),
        args.disable.clone(),
        args.install_directory.clone(),
    )?;
    if output == OutputFormat::Human {
        execute::execute_with_policy(prepared, policy).map_err(Into::into)
    } else {
        let action = prepared.action;
        execute::execute_frontend(prepared, output, action).map_err(Into::into)
    }
}

fn run_handoff(
    context: RuntimeContext,
    executable: &Path,
    request: Operation,
    args: &LifecycleArgs,
    policy: ExecutionPolicy,
    handoff_path: &Path,
) -> miette::Result<()> {
    let cache_root = args.acquired.clone().ok_or_else(|| {
        miette::miette!(
            "a handoff was given without a verified content cache, so there is no content"
        )
    })?;
    let accepted = crate::handoff::accept(
        executable,
        &cache_root,
        handoff_path,
        args.handoff_digest.as_deref(),
        args.state_root.as_deref(),
    )
    .map_err(|error| miette::miette!("{error}"))?;
    if args
        .scope
        .map(SelectedScope::from)
        .is_some_and(|scope| scope != accepted.scope)
    {
        return Err(miette::miette!(
            "requested scope differs from the runtime handoff; restart with the same scope"
        ));
    }
    let resume = accepted.session.resume_line();
    if args.output == crate::terminal::Output::Human && !resume.is_empty() {
        eprintln!("{resume}");
    }
    let expected = match accepted.mode {
        zup_acquire::HandoffMode::Cleanup { keep } => LifecycleAction::Cleanup { keep },
        zup_acquire::HandoffMode::Install => LifecycleAction::Install,
        zup_acquire::HandoffMode::Upgrade => LifecycleAction::Upgrade,
        zup_acquire::HandoffMode::Downgrade => LifecycleAction::Downgrade,
        zup_acquire::HandoffMode::Rollback { revision } => LifecycleAction::Rollback { revision },
        zup_acquire::HandoffMode::Modify => LifecycleAction::Modify,
        zup_acquire::HandoffMode::Uninstall => LifecycleAction::Uninstall,
        zup_acquire::HandoffMode::Repair => LifecycleAction::Repair {
            force: matches!(request, Operation::Repair { force: true }),
        },
    };
    if let Some(action) = named_action(request)
        && expected != action
    {
        return Err(miette::miette!(
            "the launcher handed over a `{}` handoff but `{}` was requested",
            accepted.mode.verb(),
            state::action_name(action)
        ));
    }
    if context.frontend != zup_core::Frontend::Gui {
        return crate::host::terminal_handoff(context, expected, args, accepted);
    }
    run_acquired_transition(
        acquire::Request {
            state_root: args.state_root.clone(),
            action: Some(expected),
            enable: args.enable.clone(),
            disable: args.disable.clone(),
            install_directory: args.install_directory.clone(),
            scope: Some(accepted.scope),
        },
        &accepted.acquired,
        args.output(),
        policy,
    )
}

pub(crate) fn named_action(operation: Operation) -> Option<LifecycleAction> {
    match operation {
        Operation::Cleanup { keep } => Some(LifecycleAction::Cleanup { keep }),
        Operation::Install => Some(LifecycleAction::Install),
        Operation::Upgrade => Some(LifecycleAction::Upgrade),
        Operation::Downgrade => Some(LifecycleAction::Downgrade),
        Operation::Rollback { revision } => Some(LifecycleAction::Rollback { revision }),
        Operation::Modify => Some(LifecycleAction::Modify),
        Operation::Repair { force } => Some(LifecycleAction::Repair { force }),
        Operation::Uninstall => Some(LifecycleAction::Uninstall),
        Operation::Apply => None,
    }
}

pub(crate) fn resolve_request_operation(
    operation: Operation,
    installed: Option<&semver::Version>,
    package: &str,
) -> miette::Result<LifecycleAction> {
    let package = semver::Version::parse(package).map_err(|error| {
        miette::miette!("this package's version `{package}` is not a version: {error}")
    })?;
    resolve_operation(operation, installed, &package).map_err(|error| miette::miette!("{error}"))
}

fn run_graph_transition(
    request: Operation,
    args: &LifecycleArgs,
    bundle: &zup_windows::EmbeddedBundle,
    policy: ExecutionPolicy,
) -> miette::Result<()> {
    let build = package::target_plan(bundle)?;
    let installer = &build.installer;
    let scope = installer
        .install
        .scope
        .select(args.scope.map(Into::into))
        .map_err(|error| miette::miette!("{error}; specify --scope user or --scope machine"))?;
    let state_root = state::resolve_state_root(&installer.app.id, args.state_root.clone(), scope)?;
    let ledger = zup_windows::InstallationRecordStore::new(&state_root)
        .observe(&installer.app.id, scope)
        .map_err(|error| miette::miette!("installation ledger: {error}"))?;

    let content_root = if scope == SelectedScope::Machine && args.state_root.is_none() {
        state::peer_user_state_root(&installer.app.id)?
    } else {
        state_root.clone()
    };
    let context = acquire::source_context(installer, ledger.as_ref(), request, content_root)
        .map_err(|error| miette::miette!("source selection: {error}"))?;
    let seeds: Vec<PathBuf> = args.source.iter().cloned().collect();
    let (sink, receiver) = zup_acquire::ProgressSink::channel(256);
    let emitter = match args.output {
        crate::terminal::Output::Jsonl => Some(zup_presentation::acquisition_thread(receiver)),
        _ => {
            drop(receiver);
            None
        }
    };
    let tokio = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|error| miette::miette!("acquisition runtime: {error}"))?;

    let acquired = tokio.block_on(acquire::acquire(
        context,
        zup_update::ComponentSelection::All,
        None,
        &seeds,
        sink,
    ));
    drop(emitter);
    let acquired = acquired.map_err(|error| miette::miette!("release acquisition: {error}"))?;

    acquire::validate_selected_source(&acquired, bundle.package().plan(), ledger.as_ref(), request)
        .map_err(|error| miette::miette!("source selection: {error}"))?;

    if let Operation::Repair { force } = request {
        let owned = ledger
            .as_ref()
            .map(zup_plan::owned_content_digests)
            .ok_or_else(|| miette::miette!("installation not found in the {scope} scope"))?;
        let manifest = &acquired.manifest;
        let plan = acquire::repair_closure(&acquired.resolved, manifest, &owned)
            .map_err(|error| miette::miette!("repair closure: {error}"))?;
        let narrowed = acquire::Acquired { plan, ..acquired };
        return run_acquired_transition(
            acquire::Request {
                state_root: Some(state_root.clone()),
                action: Some(LifecycleAction::Repair { force }),
                scope: Some(scope),
                ..acquire::Request::default()
            },
            &narrowed,
            args.output(),
            policy,
        );
    }

    let action = match request {
        Operation::Apply => resolve_request_operation(
            Operation::Apply,
            ledger
                .as_ref()
                .filter(|ledger| ledger.is_installed())
                .map(|ledger| &ledger.current.version),
            &acquired.resolved.descriptor.version,
        )?,
        named => named_action(named).expect("named operations resolve directly"),
    };

    run_acquired_transition(
        acquire::Request {
            state_root: Some(state_root),
            scope: Some(scope),
            ..acquire::Request::new(action).components(&args.enable, &args.disable)
        },
        &acquired,
        args.output(),
        policy,
    )
}

pub fn run_acquired_transition(
    request: acquire::Request,
    acquired: &acquire::Acquired,
    output: OutputFormat,
    policy: ExecutionPolicy,
) -> miette::Result<()> {
    let acquire::Request {
        state_root,
        action,
        enable,
        disable,
        install_directory,
        scope,
    } = request;
    let action =
        action.ok_or_else(|| miette::miette!("a graph transition needs a lifecycle verb"))?;
    let build =
        acquire::target_plan(&acquired.manifest).map_err(|error| miette::miette!("{error}"))?;
    let installer = &build.installer;

    let scope = installer
        .install
        .scope
        .select(scope)
        .map_err(|error| miette::miette!("{error}; specify --scope user or --scope machine"))?;

    let state_root = state::resolve_state_root(&installer.app.id, state_root, scope)?;
    let prior = match zup_windows::InstallationRecordStore::new(&state_root)
        .observe(&installer.app.id, scope)
    {
        Ok(prior) => prior,
        Err(error) => return Err(miette::miette!("installation ledger: {error}")),
    };

    if matches!(action, LifecycleAction::Install) && prior.is_some() {
        return Err(miette::miette!(
            "{} is already installed; run `update` to install a newer release",
            installer.app.name
        ));
    }
    if !matches!(action, LifecycleAction::Install) && prior.is_none() {
        return Err(miette::miette!("installation not found in selected scope"));
    }
    if let Some(installed) = prior
        .as_ref()
        .and_then(zup_plan::InstallationRecord::release_identity)
        && installed.variant != acquired.resolved.variant.id
    {
        return Err(miette::miette!(
            "this installation runs variant `{}` and the release offers `{}`",
            installed.variant,
            acquired.resolved.variant.id
        ));
    }

    let identity = acquired.identity();
    let payload = acquire::payload_source(acquired)
        .map_err(|error| miette::miette!("content source: {error}"))?;
    let source = Arc::new(
        zup_windows::AcquiredRuntime::stage(acquired)
            .map_err(|error| miette::miette!("release runtime: {error}"))?,
    );
    let executable = source.path();
    let host = WindowsOperationHost {
        app_id: installer.app.id.clone(),
        scope,
        frontend: installer.frontend,
        state_root: state_root.clone(),
        content: WindowsContent::Acquired(&acquired.manifest, &payload),
        acquire_prerequisites: true,
    };
    let input = OperationInput {
        build: &build,
        installer: executable,
        scope,
        operation: Operation::from(action),
        selection: OperationSelection {
            deployment: None,
            enable,
            disable,
            install_directory,
        },
        prior: prior.as_ref(),
        state_root: &state_root,
        cancellation: &zup_plan::NeverCancelled,
    };
    let prepared = prepare_operation(&input, &host).map_err(|error| miette::miette!("{error}"))?;
    let request = runtime_request(
        &prepared,
        state_root,
        zup_runtime::RequestOptions {
            bootstrap: prepared.bootstrap().cloned(),
            release: Some(identity.clone()),
        },
    );
    let backend = zup_windows::WindowsRuntimeBackend::from_acquired(
        Arc::new(payload),
        acquired.cache.root().to_path_buf(),
        acquired
            .handoff(scope, handoff_mode(action))
            .map_err(|error| miette::miette!("{error}"))?,
        source,
    )
    .with_generated_files(&prepared, &request.state_root)
    .map_err(|error| miette::miette!("plugin content: {error}"))?;
    let prepared = PreparedRuntime {
        request,
        backend: Arc::new(backend),
        action,
    };

    if output == OutputFormat::Human {
        let summary = acquired.summary();
        if summary.cached_bytes > 0 {
            eprintln!("  {summary}");
        }
    }
    let result = if output == OutputFormat::Human {
        execute::execute_with_policy(prepared, policy).map_err(Into::into)
    } else {
        execute::execute_frontend(prepared, output, action).map_err(Into::into)
    };
    if result.is_ok() {
        let _ = acquired.maintain_cache(CachePolicy::Auto);
    }
    result
}

pub struct EmbeddedPreparationMode<'a> {
    pub cancellation: &'a dyn zup_plan::CancellationQuery,
    pub acquire_prerequisites: bool,
}

pub fn prepare_embedded_transition(
    request: Operation,
    scope: SelectedScope,
    state: Option<PathBuf>,
    enable: Vec<String>,
    disable: Vec<String>,
    install_directory: Option<PathBuf>,
) -> miette::Result<PreparedRuntime> {
    prepare_embedded_transition_with_cancellation(
        request,
        scope,
        state,
        enable,
        disable,
        install_directory,
        EmbeddedPreparationMode {
            cancellation: &zup_plan::NeverCancelled,
            acquire_prerequisites: true,
        },
    )
}

pub fn prepare_embedded_transition_with_cancellation(
    request: Operation,
    scope: SelectedScope,
    state: Option<PathBuf>,
    enable: Vec<String>,
    disable: Vec<String>,
    install_directory: Option<PathBuf>,
    mode: EmbeddedPreparationMode<'_>,
) -> miette::Result<PreparedRuntime> {
    let executable = package::current_executable()?;
    let bundle = package::open_bundle(&executable)?;
    let build = package::target_plan(&bundle)?;
    let app_id = build.installer.app.id.clone();
    let state_root = state::resolve_state_root(&app_id, state, scope)?;
    let prior = zup_windows::InstallationRecordStore::new(&state_root)
        .observe(&app_id, scope)
        .map_err(|error| miette::miette!("ledger: {error}"))?;
    let host = WindowsOperationHost {
        app_id: app_id.clone(),
        scope,
        frontend: build.installer.frontend,
        state_root: state_root.clone(),
        content: WindowsContent::Embedded(&bundle),
        acquire_prerequisites: mode.acquire_prerequisites,
    };
    let input = OperationInput {
        build: &build,
        installer: &executable,
        scope,
        operation: request,
        selection: OperationSelection {
            deployment: None,
            enable,
            disable,
            install_directory,
        },
        prior: prior.as_ref(),
        state_root: &state_root,
        cancellation: mode.cancellation,
    };
    let prepared = prepare_operation(&input, &host).map_err(|error| miette::miette!("{error}"))?;
    let request = runtime_request(
        &prepared,
        state_root.clone(),
        zup_runtime::RequestOptions {
            bootstrap: prepared.bootstrap().cloned(),
            release: None,
        },
    );
    let backend = WindowsRuntimeBackend::from_path(executable, None)
        .map_err(|error| miette::miette!("payload source: {error}"))?
        .with_generated_files(&prepared, &state_root)
        .map_err(|error| miette::miette!("plugin payload overlay: {error}"))?;
    Ok(PreparedRuntime {
        request,
        backend: Arc::new(backend),
        action: prepared.action,
    })
}

pub(crate) fn handoff_mode(action: LifecycleAction) -> zup_acquire::HandoffMode {
    match action {
        LifecycleAction::Cleanup { keep } => zup_acquire::HandoffMode::Cleanup { keep },
        LifecycleAction::Install => zup_acquire::HandoffMode::Install,
        LifecycleAction::Upgrade => zup_acquire::HandoffMode::Upgrade,
        LifecycleAction::Downgrade => zup_acquire::HandoffMode::Downgrade,
        LifecycleAction::Rollback { revision } => zup_acquire::HandoffMode::Rollback { revision },
        LifecycleAction::Modify => zup_acquire::HandoffMode::Modify,
        LifecycleAction::Repair { .. } => zup_acquire::HandoffMode::Repair,
        LifecycleAction::Uninstall => zup_acquire::HandoffMode::Uninstall,
    }
}
