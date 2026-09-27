//! Applying a package to a machine.
//!
//! One question, four answers. A person who double-clicks a newer
//! `Acme-Setup.exe`, a script that runs `install --yes`, the Apps & Features
//! entry, and a framework updater all mean the same thing: make this machine
//! match this package. Which lifecycle that is depends on what the machine
//! already has, and the machine already knows — so the runtime reads its own
//! ledger and resolves the verb, rather than asking the caller to get it right.
//!
//! The three sources of a package, in the order they are preferred:
//!
//! 1. **A handoff.** A dispatcher resolved an authenticated release, filled a
//!    verified cache, and started this runtime. The handoff is checked against
//!    the cache and against this image's own digest before it is believed.
//! 2. **This image's embedded package.** A self-contained installer carries its
//!    content with it.
//! 3. **A plan the image was compiled with and a release graph to fill.** A thin
//!    runtime carries the plan and none of the content, because the content came
//!    from — and comes again from — a release graph.
//!
//! There is no fourth source. A runtime does not read `zup.toml`, does not walk
//! a source tree, and does not compile a manifest. If this image carries no
//! package, that is a refusal and not an invitation to go and find one.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use zup_acquire::CachePolicy;
use zup_core::{AppId, ComponentId, RelativePath, ResourceKey, SelectedScope, hash_reader};
use zup_exec::LifecycleAction;
use zup_plan::{CancellationQuery, PluginExecutor, TargetBuildPlan};
use zup_presentation::OutputFormat;
use zup_runtime::{ExecutionPolicy, RuntimeRequest};
use zup_windows::WindowsRuntimeBackend;

use crate::acquire;
use crate::cli::LifecycleArgs;
use crate::context::RuntimeContext;
use crate::execute;
use crate::frontend;
use crate::package;
use crate::state;

/// What the caller asked for: a verb, or "make this machine match this package".
#[derive(Debug, Clone, Copy)]
pub enum Request {
    /// The caller named the lifecycle.
    Named(LifecycleAction),
    /// The caller named no lifecycle and expects the machine's own record to
    /// supply one.
    Apply,
}

impl Request {
    /// The verb this request names, given what the machine already has.
    fn resolve(
        self,
        installed: Option<&semver::Version>,
        package_version: &str,
    ) -> miette::Result<LifecycleAction> {
        match self {
            Self::Named(action) => Ok(action),
            Self::Apply => state::resolve_applied_action(installed, package_version),
        }
    }
}

/// A plan and the backend that can satisfy it.
#[derive(Clone)]
pub struct PreparedRuntime {
    pub(crate) request: RuntimeRequest,
    pub(crate) backend: Arc<WindowsRuntimeBackend>,
    /// The lifecycle this plan performs, after the machine's own record has
    /// resolved an "apply this package" request into a verb.
    pub(crate) action: LifecycleAction,
}

impl PreparedRuntime {
    /// The same plan, executed with a payload overlay that survives a refusal.
    ///
    /// The console frontend retries a blocked installation rather than starting
    /// over, and re-downloading or re-staging the payload to do it would be the
    /// slowest part of the retry.
    #[cfg(feature = "console")]
    pub(crate) fn retaining_overlay(&self) -> Self {
        let backend = self
            .backend
            .as_ref()
            .clone()
            .with_overlay_policy(zup_windows::OverlayPolicy::RetainOnBlocked);
        Self {
            request: self.request.clone(),
            backend: Arc::new(backend),
            action: self.action,
        }
    }
}

/// Apply this package: install, upgrade, or modify, resolved from the machine.
pub fn apply(context: RuntimeContext, args: LifecycleArgs) -> miette::Result<()> {
    if args.ui {
        return frontend::graphical_for(context, args);
    }
    if args.may_prompt(context) {
        return frontend::console_apply(context, args);
    }
    transition(context, Request::Apply, &args, policy_for(&args))
}

/// Run one named lifecycle.
pub fn run(context: RuntimeContext, verb: Verb, args: LifecycleArgs) -> miette::Result<()> {
    if args.ui && context.frontend == zup_core::Frontend::Gui {
        return frontend::graphical_for(context, args);
    }
    if args.ui {
        return Err(miette::miette!(
            "this build has no graphical surface; omit --ui"
        ));
    }
    if args.may_prompt(context) {
        return frontend::console_verb(context, verb, args);
    }
    transition(
        context,
        Request::Named(verb.action()),
        &args,
        policy_for(&args),
    )
}

/// The lifecycle verbs a caller may name.
#[derive(Debug, Clone, Copy)]
pub enum Verb {
    Modify,
    Repair { force_files: bool },
    Upgrade,
}

impl Verb {
    /// The engine action this verb names.
    pub fn action(self) -> LifecycleAction {
        match self {
            Self::Modify => LifecycleAction::Modify,
            Self::Repair { force_files } => LifecycleAction::Repair { force_files },
            Self::Upgrade => LifecycleAction::Upgrade,
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

/// The GUI install or maintenance window, with no arguments.
///
/// This is what a double-click means. A fresh machine gets the installer; a
/// machine that already has the application gets its maintenance surface; a
/// package newer than what is installed offers the upgrade, and a package older
/// than what is installed is refused by the same policy a command-line apply uses.
pub fn direct_launch(context: RuntimeContext) -> miette::Result<()> {
    let executable = package::current_executable()?;
    let bundle = package::open_bundle_if_present(&executable)?.ok_or_else(|| {
        miette::miette!("this file carries no installer package; run the installer you downloaded")
    })?;
    match context.frontend {
        zup_core::Frontend::Gui => frontend::graphical(context, &executable, &bundle),
        zup_core::Frontend::Console => frontend::console_direct(&executable, &bundle),
        zup_core::Frontend::Headless => Err(miette::miette!(
            "this runtime needs an operation.\n\n  Usage: {} <install|modify|repair|update|uninstall> [OPTIONS]",
            crate::cli::invoked_name()
        )),
    }
}

/// Run a lifecycle against whichever package this process can act on.
pub fn transition(
    context: RuntimeContext,
    request: Request,
    args: &LifecycleArgs,
    policy: ExecutionPolicy,
) -> miette::Result<()> {
    let executable = package::current_executable()?;

    // A handoff arrives before anything else, because it is what says which
    // release this process is installing. A handoff that does not verify is a
    // refusal here rather than a surprise three phases later.
    if let Some(handoff_path) = &args.handoff {
        return run_handoff(&executable, request, args, policy, handoff_path);
    }

    match package::open_bundle_if_present(&executable)? {
        Some(bundle) if bundle.package().is_plan_only() => {
            run_graph_transition(request, args, &bundle, policy)
        }
        Some(bundle) => {
            let build = package::target_plan(&bundle)?;
            let scope = if build.installer.install.scope == zup_core::InstallScope::Machine {
                SelectedScope::Machine
            } else {
                SelectedScope::from(args.scope)
            };
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

/// Run a lifecycle the caller named, from an embedded package.
pub fn run_embedded(
    request: Request,
    context: RuntimeContext,
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
        if context.frontend == zup_core::Frontend::Console {
            let action = prepared.action;
            return frontend::execute_console(prepared, action);
        }
        execute::execute_with_policy(prepared, policy).map_err(Into::into)
    } else {
        let action = prepared.action;
        execute::execute_frontend(prepared, output, action).map_err(Into::into)
    }
}

/// Check a handoff and run the lifecycle it names.
fn run_handoff(
    executable: &Path,
    request: Request,
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
    )
    .map_err(|error| {
        // The guarantee is stated rather than implied: every one of these checks
        // runs before the transaction engine is asked for a plan.
        debug_assert!(error.left_machine_unchanged());
        miette::miette!("{error}")
    })?;
    // The dispatcher's progress is the runtime's starting point, not a new one:
    // the bytes it already fetched are in this cache and are still here.
    let resume = accepted.session.resume_line();
    if args.output == crate::cli::OutputArg::Human && !resume.is_empty() {
        eprintln!("{resume}");
    }
    // The handoff says which lifecycle this is, and it agrees with the verb the
    // user typed. A disagreement means one of the two processes is not the one
    // that thinks it is, which is worth a refusal.
    let expected = match accepted.mode {
        zup_acquire::HandoffMode::Install => LifecycleAction::Install,
        zup_acquire::HandoffMode::Upgrade => LifecycleAction::Upgrade,
        zup_acquire::HandoffMode::Modify => LifecycleAction::Modify,
        zup_acquire::HandoffMode::Repair => LifecycleAction::Repair { force_files: false },
    };
    if let Request::Named(action) = request
        && expected != action
    {
        return Err(miette::miette!(
            "the launcher handed over a `{}` handoff but `{}` was requested",
            accepted.mode.verb(),
            state::action_name(action)
        ));
    }
    // A handed-over graph is a graph a launcher resolved online.
    run_acquired_transition(
        acquire::Request {
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

/// Run a lifecycle for a thin runtime, against the release graph.
///
/// The embedded package is the runtime's own copy of the plan, and it is the only
/// thing a runtime-only process knows: there is no manifest beside it and no
/// source directory. The update configuration embedded in that plan is where the
/// repository, the channel, and the trusted root come from — which is the point of
/// inlining them: a machine that lost the installer file can still repair itself,
/// because the identity of what it installed travels with it.
///
/// A repair narrows the closure to the resources the ledger says drifted, so a
/// one-file repair costs one file. Every other lifecycle takes the whole
/// selection. Both are the same code; only the closure differs.
fn run_graph_transition(
    request: Request,
    args: &LifecycleArgs,
    bundle: &zup_windows::EmbeddedBundle,
    policy: ExecutionPolicy,
) -> miette::Result<()> {
    let build = package::target_plan(bundle)?;
    let installer = &build.installer;
    let config = installer.updates.as_ref().ok_or_else(|| {
        miette::miette!(
            "this runtime carries a plan but no `[updates]` configuration, so there is no \
             release graph to acquire from"
        )
    })?;
    let scope = if installer.install.scope == zup_core::InstallScope::Machine {
        SelectedScope::Machine
    } else {
        SelectedScope::from(args.scope)
    };
    let state_root = state::resolve_state_root(args.state_root.clone(), scope)?;
    let ledger = zup_windows::InstallLedgerStore::new(&state_root)
        .load(&installer.app.id, scope)
        .map_err(|error| miette::miette!("installation ledger: {error}"))?;

    // A repair is only meaningful against a committed installation, and a
    // committed installation from a graph records which graph. An installation
    // with no identity was made from a manifest, and repairing it through the
    // graph would fetch a different release's bytes.
    if let Some(identity) = ledger
        .as_ref()
        .and_then(zup_exec::InstallLedger::release_identity)
    {
        if identity.app_id != installer.app.id {
            return Err(miette::miette!(
                "the installation in {scope} is `{}` and this runtime installs `{}`",
                identity.app_id,
                installer.app.id
            ));
        }
    } else if matches!(request, Request::Named(LifecycleAction::Repair { .. })) {
        return Err(miette::miette!(
            "this installation has no release identity, so there is no authenticated source \
             to repair it from; re-run it from its installer"
        ));
    }

    // The cache lives beside the state, and the state a machine-scope install uses
    // for content is the per-user one: a shared cache would need an authority the
    // acquisition engine has no business holding.
    let content_root = if scope == SelectedScope::Machine && args.state_root.is_none() {
        state::peer_user_state_root()?
    } else {
        state_root.clone()
    };
    let context = zup_update::TrustContext::from_update_config(
        config,
        installer.app.id.clone(),
        content_root,
    );
    let seeds: Vec<PathBuf> = args.source.iter().cloned().collect();
    let (sink, receiver) = zup_acquire::ProgressSink::channel(256);
    let emitter = match args.output {
        crate::cli::OutputArg::Jsonl => Some(zup_presentation::acquisition_thread(receiver)),
        _ => None,
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
    let acquired = acquired.map_err(|error| {
        debug_assert!(error.left_machine_unchanged());
        miette::miette!("release acquisition: {error}")
    })?;

    // A repair restores what the machine owns. The ledger holds each owned
    // resource's digest, so the closure is the intersection of what drifted and
    // what the release carries — which is also the ownership check, because a
    // digest in neither cannot be asked for.
    if let Request::Named(LifecycleAction::Repair { force_files }) = request {
        let drifted = drifted_digests(&state_root, &build.installer, scope, force_files)?;
        if drifted.is_empty() {
            return Ok(());
        }
        let manifest = &acquired.manifest;
        let plan = acquire::repair_closure(&acquired.resolved, manifest, &drifted)
            .map_err(|error| miette::miette!("repair closure: {error}"))?;
        let narrowed = acquire::Acquired { plan, ..acquired };
        return run_acquired_transition(
            acquire::Request {
                action: Some(LifecycleAction::Repair { force_files }),
                scope: Some(scope),
                ..acquire::Request::default()
            },
            &narrowed,
            args.output(),
            policy,
        );
    }

    let action = match request {
        Request::Named(action) => action,
        // A plan-only runtime that already has the application resolves the same
        // way an embedded one does: an older installed release means the graph's
        // release is the upgrade, and the same version means a modify.
        Request::Apply => {
            let action = state::resolve_applied_action(
                ledger.as_ref().map(|ledger| &ledger.version),
                &acquired.resolved.descriptor.version,
            )?;
            if action == LifecycleAction::Install {
                LifecycleAction::Upgrade
            } else {
                action
            }
        }
    };

    if let Some(installed) = ledger
        .as_ref()
        .and_then(zup_exec::InstallLedger::release_identity)
        && installed.same_release(&acquired.identity())
        && action != LifecycleAction::Uninstall
    {
        return match args.output {
            crate::cli::OutputArg::Jsonl => {
                println!(
                    "{}",
                    serde_json::to_string(&zup_presentation::AutomationEvent::Completed {
                        outcome: zup_presentation::ProcessOutcome::Success,
                    })
                    .map_err(|error| miette::miette!("output: {error}"))?
                );
                Ok(())
            }
            _ => {
                println!("up to date ({})", acquired.resolved.descriptor.version);
                Ok(())
            }
        };
    }

    run_acquired_transition(
        acquire::Request {
            scope: Some(scope),
            ..acquire::Request::new(action).components(&args.enable, &args.disable)
        },
        &acquired,
        args.output(),
        policy,
    )
}

/// The digests behind owned resources the machine no longer has correct.
///
/// An owned resource with no file on disk, or one whose file hashes to something
/// else, is drift. The answer is a set of digests, not a decision: whether a
/// drifted file is restored or the operation is refused is the executor's call,
/// and this only says what the machine is missing.
fn drifted_digests(
    state_root: &Path,
    installer: &zup_core::Installer,
    scope: SelectedScope,
    _force_files: bool,
) -> miette::Result<BTreeSet<zup_core::Sha256Digest>> {
    let ledger = zup_windows::InstallLedgerStore::new(state_root)
        .load(&installer.app.id, scope)
        .map_err(|error| miette::miette!("installation ledger: {error}"))?
        .ok_or_else(|| miette::miette!("installation not found in the {scope} scope"))?;
    Ok(zup_exec::owned_content_digests(&ledger))
}

/// Plan and run one lifecycle against an acquired graph.
///
/// This is the only place a resolved graph turns into a machine change, and it
/// hands the transaction engine the one thing it needs: a plan whose payload the
/// executor reads out of the verified cache. Everything above this line is
/// content and identity; everything below is the existing lifecycle, unchanged.
pub fn run_acquired_transition(
    request: acquire::Request,
    acquired: &acquire::Acquired,
    output: OutputFormat,
    policy: ExecutionPolicy,
) -> miette::Result<()> {
    let acquire::Request {
        action,
        enable,
        disable,
        install_directory,
        scope,
    } = request;
    let action =
        action.ok_or_else(|| miette::miette!("a graph transition needs a lifecycle verb"))?;
    let build = zup_bundle::Package::from_bytes(
        zup_bundle::BundleWriter::encode_plan_only_plan(
            &acquired.manifest.plan,
            &acquired.manifest.plan.plugins,
        )
        .map_err(|error| miette::miette!("graph plan: {error}"))?,
    )
    .and_then(|package| package.build_plan())
    .map_err(|error| miette::miette!("graph plan: {error}"))?;
    let build = build
        .targets
        .into_iter()
        .next()
        .ok_or_else(|| miette::miette!("the release plan names no target"))?;
    let installer = &build.installer;

    // A handoff states the scope, and it is stated rather than derived because
    // the dispatcher resolved the same plan and reached the same answer. With no
    // handoff, a plan that allows either scope goes to the user's profile on a
    // fresh install, because that needs no elevation.
    let scope = match scope {
        Some(scope) => scope,
        None if installer.install.scope == zup_core::InstallScope::Machine => {
            SelectedScope::Machine
        }
        None if installer.install.scope == zup_core::InstallScope::User => SelectedScope::User,
        None => {
            let committed = zup_windows::InstallLedgerStore::new(acquired.cache.root())
                .load(&installer.app.id, SelectedScope::Machine)
                .ok()
                .flatten();
            if committed.is_some() {
                SelectedScope::Machine
            } else {
                SelectedScope::User
            }
        }
    };
    if installer.install.scope == zup_core::InstallScope::Machine && scope != SelectedScope::Machine
    {
        return Err(miette::miette!(
            "this application installs machine-wide, not into the {} scope",
            scope
        ));
    }

    let state_root = state::resolve_state_root(Some(acquired.cache.root().to_path_buf()), scope)?;
    let prior =
        match zup_windows::InstallLedgerStore::new(&state_root).load(&installer.app.id, scope) {
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
        .and_then(zup_exec::InstallLedger::release_identity)
        && installed.variant != acquired.resolved.variant.id
    {
        // The installed machine runs a different variant than the graph offers.
        // Installing it anyway would replace a working installation with another
        // architecture's payload under the same identity.
        return Err(miette::miette!(
            "this installation runs variant `{}` and the release offers `{}`",
            installed.variant,
            acquired.resolved.variant.id
        ));
    }

    let install = plan_from_graph(
        &build,
        action,
        &enable,
        &disable,
        install_directory.as_deref(),
        &prior,
        scope,
    )?;
    let identity = acquired.identity();
    let payload = acquire::payload_source(acquired).map_err(|error| {
        debug_assert!(error.left_machine_unchanged());
        miette::miette!("content source: {error}")
    })?;
    let work_root = state_root.join("work");
    let target =
        zup_windows::resolve_target(&install, &zup_windows::WindowsTargetContext::new(scope))
            .map_err(|error| miette::miette!("target: {error}"))?;
    let execution = zup_windows::plan_target_lifecycle_with_frontend(
        action,
        &installer.app.id,
        scope,
        Some(&target),
        &state_root,
        installer.frontend,
    )
    .map_err(|error| miette::miette!("lifecycle plan: {error}"))?;
    let bootstrap = crate::bootstrap::prepare(&install, &state_root, scope, None)?;
    let request = RuntimeRequest {
        target: target.target.clone(),
        app_id: installer.app.id.clone(),
        app_version: target.app.version.clone(),
        scope,
        transaction_plan: execution,
        state_root,
        work_root,
        recovery_id: None,
        bootstrap,
        release: Some(identity.clone()),
    };
    let backend = zup_windows::WindowsRuntimeBackend::from_acquired(
        Arc::new(payload),
        acquired.cache.root().to_path_buf(),
    )
    .map_err(|error| miette::miette!("content source: {error}"))?;
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
    // The retention record is written only after a commit, so a failed install
    // never leaves a machine believing it has content it does not.
    if result.is_ok() {
        let _ = acquired.record_retention(CachePolicy::Auto);
    }
    result
}

/// Plan an install from a graph, honouring the component rules a lifecycle
/// already had.
fn plan_from_graph(
    build: &TargetBuildPlan,
    action: LifecycleAction,
    enable: &[String],
    disable: &[String],
    install_directory: Option<&Path>,
    prior: &Option<zup_exec::InstallLedger>,
    scope: SelectedScope,
) -> miette::Result<zup_plan::InstallPlan> {
    let installer = &build.installer;
    let allow_directory_override = installer.install.allow_directory_override;
    if matches!(action, LifecycleAction::Repair { .. })
        && (install_directory.is_some() || !enable.is_empty() || !disable.is_empty())
    {
        return Err(miette::miette!(
            "repair uses the committed component selection and install location"
        ));
    }
    let mut request = zup_plan::PlanRequest::new(installer.target.clone(), scope);
    request.install_directory = if matches!(action, LifecycleAction::Repair { .. }) {
        allow_directory_override
            .then(|| state::persisted_install_directory(prior.as_ref()))
            .flatten()
    } else {
        state::choose_install_directory(
            install_directory,
            prior.as_ref(),
            allow_directory_override,
        )?
    };
    if matches!(
        action,
        LifecycleAction::Upgrade | LifecycleAction::Modify | LifecycleAction::Repair { .. }
    ) {
        let previous = prior
            .as_ref()
            .ok_or_else(|| miette::miette!("installation not found"))?;
        for component in &installer.components {
            if previous.selected_components.contains(&component.id) {
                request.components.enable.insert(component.id.clone());
            } else if !component.required {
                request.components.disable.insert(component.id.clone());
            }
        }
    }
    for raw in enable {
        let id =
            ComponentId::new(raw).map_err(|error| miette::miette!("component {raw}: {error}"))?;
        request.components.disable.remove(&id);
        request.components.enable.insert(id);
    }
    for raw in disable {
        let id =
            ComponentId::new(raw).map_err(|error| miette::miette!("component {raw}: {error}"))?;
        request.components.enable.remove(&id);
        request.components.disable.insert(id);
    }
    zup_plan::plan(
        &zup_plan::BuildPlan {
            targets: vec![build.clone()],
        },
        &request,
    )
    .map_err(|error| miette::miette!("plan: {error}"))
}

/// How an embedded package is read, for callers that prepare more than one plan.
pub struct EmbeddedPreparationMode<'a> {
    /// Whether a cancelled window cancels the planning as well as the execution.
    pub cancellation: &'a dyn zup_plan::CancellationQuery,
    /// Whether unsatisfied prerequisites are acquired now or left to the plan.
    ///
    /// A preview acquires nothing: it answers "what would this do" and a download
    /// is not an answer to that.
    pub acquire_prerequisites: bool,
}

/// The inputs to one embedded-package lifecycle.
pub struct EmbeddedPreparation<'a> {
    pub request: Request,
    pub scope: SelectedScope,
    pub build: &'a TargetBuildPlan,
    pub prior: Option<zup_exec::InstallLedger>,
    pub state_root: PathBuf,
    pub payload_root: PathBuf,
    pub enable: Vec<String>,
    pub disable: Vec<String>,
    pub install_directory: Option<PathBuf>,
}

/// Read this image's package and prepare a lifecycle from it.
pub fn prepare_embedded_transition(
    request: Request,
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

/// The same, with a window's cancellation query and a preview's cheaper mode.
pub fn prepare_embedded_transition_with_cancellation(
    request: Request,
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
    let plugin_target = build.installer.target.clone();
    let state_root = state::resolve_state_root(state, scope)?;
    let prior = zup_windows::InstallLedgerStore::new(&state_root)
        .load(&app_id, scope)
        .map_err(|error| miette::miette!("ledger: {error}"))?;
    prepare_embedded_request(
        EmbeddedPreparation {
            request,
            scope,
            build: &build,
            prior,
            state_root,
            payload_root: executable,
            enable,
            disable,
            install_directory,
        },
        Some(&bundle),
        mode.acquire_prerequisites,
        mode.cancellation,
        || {
            zup_plugin_runtime::WasmtimePluginExecutor::load(
                bundle.package().clone(),
                &plugin_target,
            )
            .map_err(|error| miette::miette!("plugin runtime: {error}"))
        },
    )
}

/// Turn an embedded package and a machine's record into an executable plan.
///
/// The plugin executor is a parameter rather than a construction so that a
/// lifecycle which never plans a plugin — an uninstall, which removes files by
/// ownership rather than by re-planning — never loads the component engine at
/// all. That is a startup-time saving on the one path a user waits for least, and
/// it is also a smaller attack surface on the one path that runs while another
/// process is holding the application's files open.
pub fn prepare_embedded_request<E>(
    preparation: EmbeddedPreparation<'_>,
    embedded_bundle: Option<&zup_windows::EmbeddedBundle>,
    acquire_prerequisites: bool,
    cancellation: &dyn CancellationQuery,
    load_executor: impl FnOnce() -> Result<E, miette::Report>,
) -> miette::Result<PreparedRuntime>
where
    E: PluginExecutor,
{
    let EmbeddedPreparation {
        request,
        scope,
        build,
        prior,
        state_root,
        payload_root,
        enable,
        disable,
        install_directory,
    } = preparation;
    let app_id = build.installer.app.id.clone();
    let repair = matches!(request, Request::Named(LifecycleAction::Repair { .. }));
    if repair {
        if !enable.is_empty() || !disable.is_empty() {
            return Err(miette::miette!(
                "repair uses the committed component selection"
            ));
        }
        if install_directory.is_some() {
            return Err(miette::miette!(
                "repair uses the committed install location"
            ));
        }
    }
    let action = request.resolve(
        prior.as_ref().map(|ledger| &ledger.version),
        &build.installer.app.version.to_string(),
    )?;
    let selected_install_directory = if action == LifecycleAction::Uninstall {
        None
    } else if repair {
        build
            .installer
            .install
            .allow_directory_override
            .then(|| state::persisted_install_directory(prior.as_ref()))
            .flatten()
    } else {
        state::choose_install_directory(
            install_directory.as_deref(),
            prior.as_ref(),
            build.installer.install.allow_directory_override,
        )?
    };
    if action == LifecycleAction::Uninstall {
        let ledger = prior.ok_or_else(|| miette::miette!("installation not found"))?;
        if ledger.target != build.installer.target {
            return Err(miette::miette!(
                "ledger target `{}` does not match this package's target `{}`",
                ledger.target,
                build.installer.target
            ));
        }
        let execution = zup_windows::plan_target_lifecycle_with_frontend(
            action,
            &app_id,
            scope,
            None,
            &state_root,
            build.installer.frontend,
        )
        .map_err(|error| miette::miette!("lifecycle plan: {error}"))?;
        let request = RuntimeRequest {
            target: build.installer.target.clone(),
            app_id,
            app_version: ledger.version,
            scope,
            transaction_plan: execution,
            work_root: state_root.join("work"),
            state_root,
            recovery_id: None,
            release: None,
            bootstrap: None,
        };
        let backend = WindowsRuntimeBackend::from_path(payload_root, None)
            .map_err(|error| miette::miette!("payload source: {error}"))?;
        return Ok(PreparedRuntime {
            request,
            backend: Arc::new(backend),
            action,
        });
    }

    let mut plan_request = zup_plan::PlanRequest::new(build.installer.target.clone(), scope);
    plan_request.install_directory = selected_install_directory;
    if matches!(
        action,
        LifecycleAction::Upgrade | LifecycleAction::Modify | LifecycleAction::Repair { .. }
    ) {
        let previous = prior
            .as_ref()
            .ok_or_else(|| miette::miette!("installation not found"))?;
        for component in &build.installer.components {
            if previous.selected_components.contains(&component.id) {
                plan_request.components.enable.insert(component.id.clone());
            } else if !component.required {
                plan_request.components.disable.insert(component.id.clone());
            }
        }
    }
    for raw in enable {
        let id =
            ComponentId::new(&raw).map_err(|error| miette::miette!("component {raw}: {error}"))?;
        plan_request.components.disable.remove(&id);
        plan_request.components.enable.insert(id);
    }
    for raw in disable {
        let id =
            ComponentId::new(&raw).map_err(|error| miette::miette!("component {raw}: {error}"))?;
        plan_request.components.enable.remove(&id);
        plan_request.components.disable.insert(id);
    }

    let mut executor = load_executor()?;
    let planning = zup_plan::BuildPlan {
        targets: vec![build.clone()],
    };
    let planned =
        zup_plan::plan_with_plugins(&planning, &plan_request, &mut executor, cancellation)
            .map_err(|error| miette::miette!("plan: {error}"))?;
    let install = &planned.plan;
    let mut target =
        zup_windows::resolve_target(install, &zup_windows::WindowsTargetContext::new(scope))
            .map_err(|error| miette::miette!("target: {error}"))?;
    attach_maintenance_copy(&mut target, &state_root, &app_id, scope, &payload_root)?;
    let execution = zup_windows::plan_target_lifecycle_with_frontend(
        action,
        &app_id,
        scope,
        Some(&target),
        &state_root,
        build.installer.frontend,
    )
    .map_err(|error| miette::miette!("lifecycle plan: {error}"))?;
    let bootstrap = if acquire_prerequisites {
        crate::bootstrap::prepare(install, &state_root, scope, embedded_bundle)?
    } else {
        None
    };
    let backend = WindowsRuntimeBackend::from_path_with_generated_files(
        payload_root.clone(),
        &state_root,
        scope,
        install,
        &planned.generated_files,
        &execution,
    )
    .map_err(|error| miette::miette!("plugin payload overlay: {error}"))?;
    let work_root = state_root.join("work");
    let request = RuntimeRequest {
        target: target.target.clone(),
        app_id,
        app_version: target.app.version.clone(),
        scope,
        transaction_plan: execution,
        state_root,
        work_root,
        recovery_id: None,
        release: None,
        bootstrap,
    };
    Ok(PreparedRuntime {
        request,
        backend: Arc::new(backend),
        action,
    })
}

/// Add this image to the plan as the installation's maintenance runtime.
///
/// This is what makes a machine that has lost `Acme-Setup.exe` still repairable:
/// the runtime that performed the install is persisted beside the application,
/// as an owned resource with its own digest, and it is the file Apps & Features
/// and the recovery path both address.
fn attach_maintenance_copy(
    target: &mut zup_platform::TargetPlan,
    state_root: &Path,
    app_id: &AppId,
    scope: SelectedScope,
    payload_root: &Path,
) -> miette::Result<()> {
    let (size, sha256) = hash_reader(
        std::fs::File::open(payload_root)
            .map_err(|error| miette::miette!("installer executable: {error}"))?,
    )
    .map_err(|error| miette::miette!("installer executable: {error}"))?;
    let destination = package::maintenance_destination(
        state_root,
        app_id,
        scope,
        &target.app.version.to_string(),
        &target.target,
    )?;
    target.files.push(zup_platform::TargetFile {
        key: ResourceKey::Maintenance {
            app_id: app_id.to_string(),
            version: target.app.version.to_string(),
            destination: destination.to_string(),
        },
        source_relative: RelativePath::new("__zup_maintenance__.exe").expect("valid relative path"),
        destination,
        size,
        sha256,
        // The maintenance executable lives in the scope's own state root, so it
        // needs that scope's authority and no more.
        privilege: scope.authorization(),
    });
    target.summary.file_count += 1;
    target.summary.install_bytes = target.summary.install_bytes.saturating_add(size);
    target.summary.resource_count += 1;
    Ok(())
}

/// A cancellation query backed by the engine's own handle.
#[cfg(any(feature = "gui", test))]
pub struct RuntimeCancellationQuery<'a>(pub &'a zup_runtime::CancellationHandle);

#[cfg(any(feature = "gui", test))]
impl CancellationQuery for RuntimeCancellationQuery<'_> {
    fn is_cancelled(&self) -> bool {
        self.0.is_cancelled()
    }
}

#[cfg(test)]
mod tests;
