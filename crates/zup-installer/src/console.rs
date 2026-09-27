//! The console frontend: a person, a terminal, and a few questions.
//!
//! The console build is the only one that talks. It asks for a scope the
//! application allows but does not fix, for the components the application offers
//! but does not require, for a directory the application permits, and finally
//! whether to go ahead. Every one of those has a non-interactive answer, and a
//! caller who supplied one never sees a question.

use std::path::{Path, PathBuf};

use zup_core::{ComponentId, Frontend, InstallScope, Installer, SelectedScope};
use zup_exec::LifecycleAction;
use zup_runtime::{ExecutionPolicy, InstallOutcome};
use zup_windows::EmbeddedBundle;

use crate::cli::{LifecycleArgs, ScopeArg};
use crate::context::RuntimeContext;
use crate::execute;
use crate::lifecycle::{self, PreparedRuntime, Request};
use crate::package;
use crate::state;

/// The console theme, so a zup-driven install reads as one product rather than
/// as a library's defaults inside somebody else's window.
struct SetupTheme;

impl cliclack::Theme for SetupTheme {
    fn bar_color(&self, state: &cliclack::ThemeState) -> console::Style {
        match state {
            cliclack::ThemeState::Active => console::Style::new().cyan(),
            cliclack::ThemeState::Cancel | cliclack::ThemeState::Error(_) => {
                console::Style::new().red()
            }
            _ => console::Style::new().bright().black(),
        }
    }

    fn state_symbol_color(&self, state: &cliclack::ThemeState) -> console::Style {
        match state {
            cliclack::ThemeState::Submit => console::Style::new().green(),
            _ => self.bar_color(state),
        }
    }
}

/// Ask the questions, then apply the package.
pub fn run_apply(context: RuntimeContext, args: LifecycleArgs) -> miette::Result<()> {
    ask(context, Request::Apply, args)
}

/// Ask the questions, then run one named lifecycle.
pub fn run(
    context: RuntimeContext,
    verb: lifecycle::Verb,
    args: LifecycleArgs,
) -> miette::Result<()> {
    ask(context, Request::Named(verb.action()), args)
}

/// The interactive console, launched with no operation named.
///
/// A double-click in a terminal means "do the obvious thing", and the obvious
/// thing is the same decision `install` makes: read the machine's record, and do
/// whatever makes it match this package.
pub fn direct_launch(executable: &Path, bundle: &EmbeddedBundle) -> miette::Result<()> {
    cliclack::set_theme(SetupTheme);
    let build = package::target_plan(bundle)?;
    let installer = &build.installer;
    let installed = find_installation(installer, None, ScopeArg::Either)?;
    let action = state::resolve_applied_action(
        installed.as_ref().map(|(_, ledger)| &ledger.version),
        &installer.app.version.to_string(),
    )?;
    let scope = choose_scope(
        installer,
        ScopeArg::User,
        installed.as_ref().map(|(s, _)| *s),
    )?;
    let components = choose_components(
        installer,
        &[],
        &[],
        installed.as_ref().map(|(_, ledger)| ledger),
    )?;
    let install_directory = choose_install_directory(
        installer,
        None,
        installed.as_ref().map(|(_, ledger)| ledger),
    )?;
    let location = install_directory
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "the configured install directory".into());
    if !confirm(&format!("{} to {location}?", state::action_name(action)))? {
        return Err(cancelled());
    }
    let _ = cliclack::outro("Ready to install");
    let _ = executable;
    let args = LifecycleArgs {
        scope: scope_arg(scope),
        enable: components.iter().map(ToString::to_string).collect(),
        disable: installer
            .components
            .iter()
            .filter(|component| !components.contains(&component.id))
            .map(|component| component.id.to_string())
            .collect(),
        install_directory,
        non_interactive: true,
        yes: true,
        ui: false,
        ..LifecycleArgs::default()
    };
    lifecycle::transition(
        RuntimeContext::new(Frontend::Console).with_output(zup_presentation::OutputFormat::Human),
        Request::Named(action),
        &args,
        ExecutionPolicy::Interactive,
    )
}

/// Turn a set of answers into a run.
fn ask(context: RuntimeContext, request: Request, mut args: LifecycleArgs) -> miette::Result<()> {
    cliclack::set_theme(SetupTheme);
    let executable = package::current_executable()?;
    let bundle = package::open_bundle(&executable)?;
    let build = package::target_plan(&bundle)?;
    let installer = &build.installer;
    let installed = find_installation(installer, args.state_root.as_deref(), args.scope)?;
    let action = match request {
        Request::Named(action) => action,
        Request::Apply => state::resolve_applied_action(
            installed.as_ref().map(|(_, ledger)| &ledger.version),
            &installer.app.version.to_string(),
        )?,
    };
    let scope = choose_scope(installer, args.scope, installed.as_ref().map(|(s, _)| *s))?;
    let repair = matches!(action, LifecycleAction::Repair { .. });
    if repair && (!args.enable.is_empty() || !args.disable.is_empty()) {
        return Err(miette::miette!(
            "repair uses the committed component selection"
        ));
    }
    let components = if repair {
        Vec::new()
    } else {
        choose_components(
            installer,
            &args.enable,
            &args.disable,
            installed.as_ref().map(|(_, ledger)| ledger),
        )?
    };
    let install_directory = if repair {
        if args.install_directory.is_some() {
            return Err(miette::miette!(
                "repair uses the committed install location"
            ));
        }
        None
    } else {
        choose_install_directory(
            installer,
            args.install_directory.as_deref(),
            installed.as_ref().map(|(_, ledger)| ledger),
        )?
    };
    let location = install_directory
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "the configured install directory".into());
    if !confirm(&format!("{} to {location}?", state::action_name(action)))? {
        return Err(cancelled());
    }
    let _ = cliclack::outro("Ready to install");
    args.scope = scope_arg(scope);
    if !repair {
        args.enable = components.iter().map(ToString::to_string).collect();
        args.disable = installer
            .components
            .iter()
            .filter(|component| !components.contains(&component.id))
            .map(|component| component.id.to_string())
            .collect();
    } else {
        args.enable.clear();
        args.disable.clear();
    }
    args.install_directory = install_directory;
    // The answers above *were* the interaction. Telling the engine the same thing
    // twice would show a second, redundant confirmation.
    args.non_interactive = true;
    args.yes = true;
    let _ = context;
    lifecycle::transition(
        context,
        Request::Named(action),
        &args,
        ExecutionPolicy::Interactive,
    )
}

fn confirm(question: &str) -> miette::Result<bool> {
    cliclack::confirm(question)
        .initial_value(true)
        .interact()
        .map_err(|error| miette::miette!("prompt: {error}"))
}

/// Confirm an uninstall, which is the one question with a safe default.
///
/// Uninstalling is the only operation here that cannot be undone by running it
/// again, so the default answer is no and a stray Enter does not remove an
/// application.
pub fn confirm_uninstall() -> miette::Result<bool> {
    cliclack::set_theme(SetupTheme);
    cliclack::confirm("Uninstall this application?")
        .initial_value(false)
        .interact()
        .map_err(|error| miette::miette!("prompt: {error}"))
}

/// Confirm an update before it is installed.
pub fn confirm_update() -> miette::Result<bool> {
    cliclack::set_theme(SetupTheme);
    cliclack::confirm("Install this update?")
        .initial_value(true)
        .interact()
        .map_err(|error| miette::miette!("prompt: {error}"))
}

/// The failure a cancelled question produces.
pub fn cancelled() -> miette::Report {
    let _ = cliclack::outro_cancel("Cancelled");
    miette::miette!("cancelled")
}

fn scope_arg(scope: SelectedScope) -> ScopeArg {
    match scope {
        SelectedScope::User => ScopeArg::User,
        SelectedScope::Machine => ScopeArg::Machine,
    }
}

/// The one installation this application has, in whichever scope allows it.
fn find_installation(
    installer: &Installer,
    state_root: Option<&Path>,
    requested: ScopeArg,
) -> miette::Result<Option<(SelectedScope, zup_exec::InstallLedger)>> {
    let scopes = match installer.install.scope {
        InstallScope::User => vec![SelectedScope::User],
        InstallScope::Machine => vec![SelectedScope::Machine],
        InstallScope::Either => vec![SelectedScope::User, SelectedScope::Machine],
    };
    let mut found = Vec::new();
    for scope in scopes {
        if matches!(requested, ScopeArg::User) && scope != SelectedScope::User
            || matches!(requested, ScopeArg::Machine) && scope != SelectedScope::Machine
        {
            continue;
        }
        let root = state::resolve_state_root(state_root.map(Path::to_path_buf), scope)?;
        let ledger = zup_windows::InstallLedgerStore::new(&root)
            .load(&installer.app.id, scope)
            .map_err(|error| miette::miette!("ledger: {error}"))?;
        if let Some(ledger) = ledger {
            found.push((scope, ledger));
        }
    }
    if found.len() > 1 {
        return Err(miette::miette!(
            "this application is installed in both scopes; choose --scope"
        ));
    }
    Ok(found.pop())
}

/// The scope to install into.
///
/// An application that fixes its scope gets no question. An application that
/// allows either, on a machine that already has one, keeps it: moving an
/// installation between scopes is a different decision from reinstalling it.
fn choose_scope(
    installer: &Installer,
    requested: ScopeArg,
    installed: Option<SelectedScope>,
) -> miette::Result<SelectedScope> {
    match installer.install.scope {
        InstallScope::Machine => return Ok(SelectedScope::Machine),
        InstallScope::User => return Ok(SelectedScope::User),
        InstallScope::Either => {}
    }
    if let Some(scope) = installed {
        return Ok(scope);
    }
    let initial = match requested {
        ScopeArg::Machine => SelectedScope::Machine,
        ScopeArg::User | ScopeArg::Either => SelectedScope::User,
    };
    cliclack::select("Install for")
        .item(SelectedScope::User, "Current user", "")
        .item(SelectedScope::Machine, "All users", "")
        .initial_value(initial)
        .interact()
        .map_err(|error| miette::miette!("prompt: {error}"))
}

/// The components to install, defaulting to what the application and the machine
/// already agree on.
fn choose_components(
    installer: &Installer,
    requested_enable: &[String],
    requested_disable: &[String],
    installed: Option<&zup_exec::InstallLedger>,
) -> miette::Result<Vec<ComponentId>> {
    if installer.components.is_empty() {
        return Ok(Vec::new());
    }
    let mut selected = installer
        .components
        .iter()
        .filter(|component| {
            component.required
                || component.default
                || installed
                    .is_some_and(|ledger| ledger.selected_components.contains(&component.id))
                || requested_enable
                    .iter()
                    .any(|value| value == component.id.as_str())
        })
        .map(|component| component.id.clone())
        .collect::<Vec<_>>();
    for value in requested_disable {
        if let Ok(id) = ComponentId::new(value) {
            selected.retain(|component| component != &id);
        }
    }
    // Every component required means there is nothing to decide.
    if installer
        .components
        .iter()
        .all(|component| component.required)
        && selected.len() == installer.components.len()
    {
        return Ok(selected);
    }
    let items = installer
        .components
        .iter()
        .map(|component| {
            (
                component.id.clone(),
                component.name.to_string(),
                component.description.clone().unwrap_or_default(),
            )
        })
        .collect::<Vec<_>>();
    let mut prompt = cliclack::multiselect("Components").required(false);
    for (id, name, description) in &items {
        prompt = prompt.item(id.clone(), name, description);
    }
    let mut chosen = prompt
        .initial_values(selected)
        .interact()
        .map_err(|error| miette::miette!("prompt: {error}"))?;
    for component in &installer.components {
        if component.required && !chosen.contains(&component.id) {
            chosen.push(component.id.clone());
        }
    }
    Ok(chosen)
}

/// The directory to install into, where the application allows a choice.
fn choose_install_directory(
    installer: &Installer,
    requested: Option<&Path>,
    installed: Option<&zup_exec::InstallLedger>,
) -> miette::Result<Option<PathBuf>> {
    if !installer.install.allow_directory_override {
        return Ok(requested.map(Path::to_path_buf));
    }
    if let Some(path) = requested {
        return Ok(Some(path.to_path_buf()));
    }
    let default = installed
        .and_then(|ledger| ledger.install_directory.as_ref())
        .map(ToString::to_string)
        .unwrap_or_default();
    let value: String = cliclack::input("Install location")
        .default_input(&default)
        .interact()
        .map_err(|error| miette::miette!("prompt: {error}"))?;
    Ok((!value.trim().is_empty()).then(|| PathBuf::from(value)))
}

/// Run a prepared lifecycle with a progress bar, retrying a blocked install.
///
/// A blocked installation is the common case for a desktop application: the user
/// has it open. Throwing away a staged payload and starting over would be the
/// slowest possible response to the easiest question, so the overlay is retained
/// and only the transaction is retried.
pub fn execute(prepared: PreparedRuntime, action: LifecycleAction) -> miette::Result<()> {
    let mut pending = Some(prepared);
    while let Some(prepared) = pending.take() {
        let retry = prepared.clone();
        let outcome = match execute_once(prepared, action) {
            Ok(outcome) => outcome,
            Err(error) => {
                retry.backend.cleanup_overlay(&retry.request);
                return Err(error);
            }
        };
        match outcome {
            InstallOutcome::Committed => {
                println!("done");
                return Ok(());
            }
            InstallOutcome::Cancelled => return Err(miette::miette!("cancelled")),
            InstallOutcome::Failed(message) if message == BLOCKED => {
                let choice = cliclack::select("Installation blocked")
                    .item("retry", "Retry", "after closing the listed applications")
                    .item("cancel", "Cancel", "stop without changing the installation")
                    .initial_value("retry")
                    .interact();
                match choice {
                    Ok("retry") => pending = Some(retry),
                    Ok(_) => {
                        retry.backend.cleanup_overlay(&retry.request);
                        return Err(cancelled());
                    }
                    Err(error) => {
                        retry.backend.cleanup_overlay(&retry.request);
                        return Err(miette::miette!("prompt: {error}"));
                    }
                }
            }
            other => return Err(miette::miette!("transaction: {other:?}")),
        }
    }
    Err(miette::miette!("cancelled"))
}

/// The engine's message for a payload held open by a running application.
const BLOCKED: &str = "blocked by running applications";

fn execute_once(
    prepared: PreparedRuntime,
    action: LifecycleAction,
) -> miette::Result<InstallOutcome> {
    let request = prepared.request.clone();
    println!("{} {}", state::action_name(action), request.app_id);
    let total = request.transaction_plan.total_work();
    let progress = indicatif::ProgressBar::new(total);
    progress.set_style(
        indicatif::ProgressStyle::with_template("  {msg:.<28} {bar:30.cyan/blue} {percent:>3}%")
            .unwrap_or_else(|_| indicatif::ProgressStyle::default_bar()),
    );
    let (events, _) = tokio::sync::broadcast::channel(256);
    let mut receiver = events.subscribe();
    let pump_progress = progress.clone();
    let pump = std::thread::spawn(move || {
        loop {
            let event = match receiver.blocking_recv() {
                Ok(event) => event,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            };
            let terminal = execute::is_terminal(&event);
            match &event {
                zup_runtime::RuntimeEvent::Progress {
                    completed,
                    total,
                    action,
                } => {
                    if *total > 0 {
                        pump_progress.set_length(*total);
                    }
                    pump_progress.set_position((*completed).min((*total).max(1)));
                    pump_progress.set_message(action.clone());
                }
                zup_runtime::RuntimeEvent::ResourceBlocked { detail, .. } => {
                    eprintln!("installation blocked by running applications\n{detail}");
                }
                zup_runtime::RuntimeEvent::Failed { message, .. } => {
                    eprintln!("{message}");
                }
                zup_runtime::RuntimeEvent::StateChanged { state }
                    if *state == zup_runtime::RuntimeState::Cancelled =>
                {
                    println!("Cancelling safely…");
                }
                _ => {}
            }
            if terminal {
                break;
            }
        }
    });
    let cancel = zup_runtime::CancellationHandle::new();
    let prepared = prepared.retaining_overlay();
    let result = execute::execute_with_control_signal(
        prepared,
        cancel,
        events,
        ExecutionPolicy::Interactive,
    );
    progress.finish_and_clear();
    let _ = pump.join();
    result
}
