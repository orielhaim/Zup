//! Bringing an installed application up to a newer release.
//!
//! An update is a graph lifecycle, not a second installer. The runtime resolves
//! the channel this application was configured with, checks the release against
//! the identity its own ledger recorded, computes the closure, and moves only
//! what the verified cache does not already hold. The offline installer is still
//! published, because an enterprise install wants one file; it is a claim in the
//! release graph rather than a second package representation, and this path never
//! needs it.
//!
//! Everything this module does is available to every frontend, and none of it is
//! conditional on which build is running it. An earlier version exposed the
//! update path only when the build happened to include the authoring plane,
//! because the code that resolved a graph was sitting in the developer CLI. The
//! boundary has moved; the capability has not changed shape.

#[cfg(feature = "gui")]
use std::path::Path;
use std::path::PathBuf;

use clap::{Args, Subcommand, ValueHint};
use std::io::IsTerminal as _;
use zup_core::SelectedScope;
use zup_exec::LifecycleAction;
use zup_presentation::{AutomationEvent, AutomationResult, OutputFormat, ProcessOutcome};
use zup_runtime::ExecutionPolicy;

use zup_acquire::CachePolicy;
use zup_update::{ComponentSelection, TrustContext};

use crate::acquire;
use crate::cli::{OutputArg, ScopeArg};
use crate::context::RuntimeContext;
use crate::lifecycle;
use crate::package;
use crate::state;

/// Resolve a verified release and install what changed.
#[derive(Debug, Args)]
pub struct UpdateArgs {
    #[command(subcommand)]
    pub command: Option<UpdateCommand>,
    /// Which installation to update.
    #[arg(long, value_enum)]
    pub scope: Option<ScopeArg>,
    /// A local release tree to read before the network.
    #[arg(long, value_hint = ValueHint::DirPath)]
    pub source: Option<PathBuf>,
    /// Answer with a machine-readable result instead of prose.
    #[arg(long, value_enum, default_value = "human")]
    pub output: OutputArg,
    /// Never ask a question.
    #[arg(long)]
    pub non_interactive: bool,
    /// Proceed without asking for confirmation.
    #[arg(long)]
    pub yes: bool,

    /// The state root the installation record lives in. Derived when absent.
    #[arg(long, hide = true, value_hint = ValueHint::DirPath)]
    pub state_root: Option<PathBuf>,
}

/// What an update invocation should do.
#[derive(Debug, Subcommand)]
pub enum UpdateCommand {
    /// Report whether a newer release exists, without acquiring it.
    Check,
}

impl UpdateArgs {
    /// The output format the caller asked for.
    pub fn output(&self) -> OutputFormat {
        self.output.into()
    }
}

/// Bring the installed application up to a newer release.
pub fn run(context: RuntimeContext, args: UpdateArgs) -> miette::Result<()> {
    let _ = context;
    let executable = package::current_executable()?;
    let bundle = package::open_bundle(&executable).map_err(|error| {
        miette::miette!("an update needs this application's installer package: {error}")
    })?;
    let build = package::target_plan(&bundle)?;
    let installer = &build.installer;
    let config = installer
        .updates
        .as_ref()
        .ok_or_else(|| miette::miette!("this application is not configured for updates"))?;
    let output = args.output();
    let check_only = args.command.is_some();
    let machine_run = output != OutputFormat::Human && !check_only;

    let (scope, state_root, ledger) = committed_installation(installer, &args)?;
    // A machine-scope install keeps its content cache in the user's profile: a
    // per-machine cache would need an authority the acquisition engine has no
    // business holding, and the content is identical either way.
    let update_root = if scope == SelectedScope::Machine && args.state_root.is_none() {
        state::peer_user_state_root()?
    } else {
        state_root.clone()
    };

    if output == OutputFormat::Jsonl && !machine_run {
        println!(
            "{}",
            serde_json::to_string(&AutomationEvent::started(
                installer.app.id.as_str(),
                ledger.version.to_string(),
                "update",
            ))
            .map_err(|error| miette::miette!("output: {error}"))?
        );
        println!(
            "{}",
            serde_json::to_string(&AutomationEvent::Phase {
                state: "resolving".into(),
            })
            .map_err(|error| miette::miette!("output: {error}"))?
        );
    } else if output == OutputFormat::Human && std::io::stderr().is_terminal() {
        eprintln!("Checking for updates…");
    }

    let installed = ledger.release_identity().cloned();
    let (sink, _receiver) = zup_acquire::ProgressSink::channel(256);
    let context =
        TrustContext::from_update_config(config, installer.app.id.clone(), update_root.clone());
    let mut resolver = zup_update::ReleaseResolver::new(
        context,
        zup_acquire::HostProfile::native(),
        CachePolicy::Auto,
        sink.clone(),
    )
    .map_err(|error| miette::miette!("update resolver: {error}"))?;
    if let Some(source) = &args.source {
        resolver = resolver.with_seed("local source", source.clone());
    }
    let tokio = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|error| miette::miette!("update runtime: {error}"))?;
    let resolved = tokio
        .block_on(resolver.resolve())
        .map_err(|error| miette::miette!("update check: {error}"))?;

    // Up to date is decided on digests where both sides have one, and on the
    // version otherwise. A rebuild of the same version is not an update, because
    // the lifecycle would refuse it as a same-version upgrade anyway.
    let same_release = installed.as_ref().is_some_and(|installed| {
        installed.release == resolved.descriptor.release_digest
            && installed.catalog == resolved.descriptor.catalog.digest
    });
    let newer = match &installed {
        Some(installed) => acquire::is_newer(installed, &resolved.descriptor.version),
        // An installation with no release identity came from a package with no
        // graph. Any release is newer than "nothing this runtime can name".
        None => true,
    };
    if same_release || !newer {
        return up_to_date(
            output,
            installer.app.id.as_str(),
            ledger.version.to_string(),
            scope,
            machine_run,
        );
    }
    if check_only {
        return update_available(
            output,
            installer.app.id.as_str(),
            ledger.version.to_string(),
            resolved.descriptor.version.clone(),
            scope,
        );
    }

    confirm(&args, output)?;
    if output == OutputFormat::Human {
        println!(
            "update available: {} → {}",
            ledger.version, resolved.descriptor.version
        );
        if std::io::stderr().is_terminal() {
            eprintln!("Acquiring the update…");
        }
    }
    let acquired = tokio
        .block_on(acquire::acquire_closure(
            &resolver,
            resolved,
            ComponentSelection::All,
        ))
        .map_err(|error| {
            debug_assert!(error.left_machine_unchanged());
            miette::miette!("update acquisition: {error}")
        })?;
    if output == OutputFormat::Human {
        eprintln!("  {}", acquired.estimate());
    }
    let policy = if args.non_interactive || output != OutputFormat::Human {
        ExecutionPolicy::NonInteractive
    } else {
        ExecutionPolicy::Interactive
    };
    lifecycle::run_acquired_transition(
        acquire::Request {
            scope: Some(scope),
            ..acquire::Request::new(LifecycleAction::Upgrade)
        },
        &acquired,
        output,
        policy,
    )
}

/// The maintenance surface's update button.
///
#[cfg(feature = "gui")]
/// The same graph path as the command, with a progress line instead of a stream.
/// A window and a terminal install the same bytes, so a window does not cost the
/// machine a second copy of the application.
pub fn from_maintenance_surface(
    executable: &Path,
    scope: SelectedScope,
    mut status: impl FnMut(&str),
) -> Result<Option<(String, String)>, String> {
    let bundle = package::open_bundle(executable).map_err(|error| error.to_string())?;
    let build = package::target_plan(&bundle).map_err(|error| error.to_string())?;
    let installer = &build.installer;
    let config = installer
        .updates
        .as_ref()
        .ok_or_else(|| "this application is not configured for updates".to_owned())?;
    let root = state::resolve_state_root(None, scope).map_err(|error| error.to_string())?;
    let ledger = zup_windows::InstallLedgerStore::new(&root)
        .load(&installer.app.id, scope)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "the installed application was not found".to_owned())?;
    let update_root = if scope == SelectedScope::Machine {
        state::peer_user_state_root().map_err(|error| error.to_string())?
    } else {
        root.clone()
    };
    let context = TrustContext::from_update_config(config, installer.app.id.clone(), update_root);
    let (sink, receiver) = zup_acquire::ProgressSink::channel(64);
    let emitter = zup_presentation::acquisition_thread(receiver);
    let tokio = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;

    status("Checking for updates…");
    let acquired = tokio
        .block_on(acquire::acquire(
            context,
            ComponentSelection::All,
            None,
            &[],
            sink,
        ))
        .map_err(|error| error.to_string())?;
    let available = acquired.resolved.descriptor.version.clone();
    let same_release = ledger
        .release_identity()
        .is_some_and(|installed| installed.release == acquired.resolved.descriptor.release_digest);
    if same_release {
        drop(emitter);
        status("Up to date");
        return Ok(None);
    }
    status(&format!("Downloading {}", acquired.estimate()));
    lifecycle::run_acquired_transition(
        acquire::Request {
            scope: Some(scope),
            ..acquire::Request::new(LifecycleAction::Upgrade)
        },
        &acquired,
        OutputFormat::Human,
        ExecutionPolicy::Interactive,
    )
    .map_err(|error| error.to_string())?;
    let _ = emitter;
    Ok(Some((ledger.version.to_string(), available)))
}

/// The installation an update applies to: the one the ledger actually records.
fn committed_installation(
    installer: &zup_core::Installer,
    args: &UpdateArgs,
) -> miette::Result<(SelectedScope, PathBuf, zup_exec::InstallLedger)> {
    let mut scope = if installer.install.scope == zup_core::InstallScope::Machine {
        SelectedScope::Machine
    } else {
        args.scope
            .map(SelectedScope::from)
            .unwrap_or(SelectedScope::User)
    };
    let mut state_root = state::resolve_state_root(args.state_root.clone(), scope)?;
    let store = zup_windows::InstallLedgerStore::new(&state_root);
    let mut ledger = store
        .load(&installer.app.id, scope)
        .map_err(|error| miette::miette!("installation ledger: {error}"))?;
    // An application that installs for the current user but was installed
    // machine-wide is common enough that refusing would be unhelpful, and looking
    // is cheap. Only when the caller said nothing at all.
    if ledger.is_none()
        && args.scope.is_none()
        && args.state_root.is_none()
        && scope == SelectedScope::User
    {
        scope = SelectedScope::Machine;
        state_root = state::resolve_state_root(None, scope)?;
        ledger = store
            .load(&installer.app.id, scope)
            .map_err(|error| miette::miette!("installation ledger: {error}"))?;
    }
    let ledger = ledger.ok_or_else(|| miette::miette!("this application is not installed"))?;
    Ok((scope, state_root, ledger))
}

/// Whether to ask before installing.
///
/// The console frontend is the only one that can ask, and only a person in front
/// of a terminal is asked. Every other frontend either answered already or is
/// answering a script, and a script is not asked questions.
fn confirm(args: &UpdateArgs, output: OutputFormat) -> miette::Result<()> {
    if args.yes || args.non_interactive || output != OutputFormat::Human {
        return Ok(());
    }
    if crate::context::console_is_a_terminal() && !crate::frontend::confirm_update()? {
        return Err(crate::frontend::cancelled());
    }
    Ok(())
}

fn up_to_date(
    output: OutputFormat,
    application: &str,
    version: String,
    scope: SelectedScope,
    machine_run: bool,
) -> miette::Result<()> {
    match output {
        OutputFormat::Human => println!("up to date ({version})"),
        OutputFormat::Json => {
            let mut result = AutomationResult::new(ProcessOutcome::Success, application, version);
            result.scope = Some(scope);
            println!(
                "{}",
                result
                    .to_json()
                    .map_err(|error| miette::miette!("output: {error}"))?
            );
        }
        OutputFormat::Jsonl => {
            if machine_run {
                println!(
                    "{}",
                    serde_json::to_string(&AutomationEvent::started(
                        application,
                        version,
                        "update"
                    ))
                    .map_err(|error| miette::miette!("output: {error}"))?
                );
            }
            println!(
                "{}",
                serde_json::to_string(&AutomationEvent::Completed {
                    outcome: ProcessOutcome::Success,
                })
                .map_err(|error| miette::miette!("output: {error}"))?
            );
        }
    }
    Ok(())
}

fn update_available(
    output: OutputFormat,
    application: &str,
    current: String,
    available: String,
    scope: SelectedScope,
) -> miette::Result<()> {
    match output {
        OutputFormat::Human => println!("update available: {current} → {available}"),
        OutputFormat::Json => {
            let mut result =
                AutomationResult::new(ProcessOutcome::Success, application, available.clone());
            result.scope = Some(scope);
            result.message = Some(format!("update available from {current}"));
            println!(
                "{}",
                result
                    .to_json()
                    .map_err(|error| miette::miette!("output: {error}"))?
            );
        }
        OutputFormat::Jsonl => {
            for state in ["available", "completed"] {
                let event = if state == "available" {
                    AutomationEvent::Phase {
                        state: state.into(),
                    }
                } else {
                    AutomationEvent::Completed {
                        outcome: ProcessOutcome::Success,
                    }
                };
                println!(
                    "{}",
                    serde_json::to_string(&event)
                        .map_err(|error| miette::miette!("output: {error}"))?
                );
            }
        }
    }
    Ok(())
}
