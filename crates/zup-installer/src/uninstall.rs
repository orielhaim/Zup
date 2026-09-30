//! Removing an application, and getting out of its own way to do it.
//!
//! An uninstall has one problem the other lifecycles do not: the file doing the
//! work is a file it has to delete. The runtime therefore hands off to a copy of
//! itself in the temporary directory, tells that copy to wait for this process to
//! exit, and gets out of the way. The copy then runs the same lifecycle the
//! direct invocation would, because it is the same code with the same ledger.

use std::path::{Path, PathBuf};

use clap::{Args, ValueHint};
use zup_core::{AppId, Frontend, InstallScope, SelectedScope};
use zup_exec::LifecycleAction;
use zup_presentation::OutputFormat;
use zup_runtime::{ExecutionPolicy, RuntimeRequest};

use crate::cli::{OutputArg, ScopeArg};
use crate::context::RuntimeContext;
use crate::lifecycle::{self, PreparedRuntime, Request};
use crate::package;
use crate::state;

/// Remove the application and the resources it owns.
#[derive(Debug, Clone, Args)]
pub struct UninstallArgs {
    /// Which installation to remove.
    #[arg(long, value_enum)]
    pub scope: Option<ScopeArg>,
    /// Answer with a machine-readable result instead of prose.
    #[arg(long, value_enum, default_value = "human")]
    pub output: OutputArg,
    /// Never ask a question.
    #[arg(long)]
    pub non_interactive: bool,
    /// Proceed without asking for confirmation.
    #[arg(long)]
    pub yes: bool,

    /// The application to remove, when the caller is not the application's own
    /// runtime. A product-specific maintenance executable already knows.
    #[arg(long, hide = true)]
    pub app_id: Option<String>,
    /// The state root to read and write. Derived from the scope when absent.
    #[arg(long, hide = true, value_hint = ValueHint::DirPath)]
    pub state_root: Option<PathBuf>,
    /// The scratch directory for the transaction. Derived when absent.
    #[arg(long, hide = true, value_hint = ValueHint::DirPath)]
    pub work_root: Option<PathBuf>,
    /// The graphical surface, where this build has one.
    #[arg(long, hide = true)]
    pub ui: bool,
}

impl UninstallArgs {
    /// The output format the caller asked for.
    pub fn output(&self) -> OutputFormat {
        self.output.into()
    }
}

/// Remove the application.
pub fn run(context: RuntimeContext, args: UninstallArgs) -> miette::Result<()> {
    let executable = package::current_executable()?;

    // A maintenance copy cannot delete itself, and neither can the installer a
    // user is running from a Downloads folder Windows has open. Both hand off to a
    // copy in the temporary directory, and that copy waits for this process to
    // exit before it touches anything.
    //
    // This process therefore does *not* wait for the runner, whatever the output
    // format. Waiting would be a deadlock: the runner waits for this process, and
    // a script that ran `maintenance.exe uninstall --output json` and blocked
    // would wait for a result that can only be produced after the script's own
    // child is gone. The runner inherits this process's exit intent through the
    // verb and the arguments it was given; its own outcome is its own to report.
    if zup_windows::is_maintenance_executable(&executable) {
        launch_runner(&executable, &args, false)?;
        return Ok(());
    }

    if args.ui {
        return Err(miette::miette!(
            "this build has no graphical surface; omit --ui or add --yes"
        ));
    }

    if context.is_live_console()
        && !args.non_interactive
        && !args.yes
        && args.output == OutputArg::Human
        && !crate::frontend::confirm_uninstall()?
    {
        return Err(crate::frontend::cancelled());
    }

    let bundle = package::open_bundle_if_present(&executable)?;
    let build = bundle.as_ref().map(package::target_plan).transpose()?;
    let app_id = match args.app_id.as_deref() {
        Some(id) => AppId::new(id).map_err(|error| miette::miette!("application ID: {error}"))?,
        None => build
            .as_ref()
            .ok_or_else(|| miette::miette!("this file carries no installer package"))?
            .installer
            .app
            .id
            .clone(),
    };
    let (scope, state_root) = resolve_scope(
        &app_id,
        build.as_ref().map(|build| &build.installer),
        args.scope.unwrap_or(ScopeArg::Either),
        args.state_root.as_deref(),
    )?;
    let output = args.output();
    let policy = if args.non_interactive
        || args.yes
        || args.output != OutputArg::Human
        || !context.is_live_console()
    {
        ExecutionPolicy::NonInteractive
    } else {
        ExecutionPolicy::Interactive
    };

    let prepared = match &build {
        Some(_) => prepare_from_package(scope, state_root.clone())?,
        None => prepare_from_ledger(
            &app_id,
            scope,
            state_root.clone(),
            args.work_root.clone(),
            build.as_ref().map(|build| build.installer.frontend),
        )?,
    };
    let result = if output == OutputFormat::Human {
        crate::execute::execute_with_policy(prepared, policy).map_err(Into::into)
    } else {
        crate::execute::execute_frontend(prepared, output, LifecycleAction::Uninstall)
            .map_err(Into::into)
    };
    if result.is_ok() {
        remove_lock(&state_root, &app_id, scope)?;
        zup_windows::cleanup_app_payload_overlays(&state_root, &app_id, scope)
            .map_err(|error| miette::miette!("clean up payload overlays: {error}"))?;
    }
    result
}

/// Plan the uninstall from the package this image carries.
fn prepare_from_package(
    scope: SelectedScope,
    state_root: PathBuf,
) -> miette::Result<PreparedRuntime> {
    lifecycle::prepare_embedded_transition_with_cancellation(
        Request::Named(LifecycleAction::Uninstall),
        scope,
        Some(state_root),
        Vec::new(),
        Vec::new(),
        None,
        lifecycle::EmbeddedPreparationMode {
            cancellation: &zup_plan::NeverCancelled,
            acquire_prerequisites: false,
        },
    )
}

/// Plan the uninstall from the committed ledger alone.
///
/// This is the path a runtime with no package takes: the ledger says what was
/// installed, what it owns, and for which target, and an uninstall is a removal
/// by ownership rather than a re-plan. There is nothing to read from a source
/// tree because there was never one.
fn prepare_from_ledger(
    app_id: &AppId,
    scope: SelectedScope,
    state_root: PathBuf,
    work_root: Option<PathBuf>,
    frontend: Option<Frontend>,
) -> miette::Result<PreparedRuntime> {
    let ledger = zup_windows::InstallLedgerStore::new(&state_root)
        .load(app_id, scope)
        .map_err(|error| miette::miette!("ledger: {error}"))?
        .ok_or_else(|| miette::miette!("installation not found"))?;
    let execution = zup_windows::plan_target_lifecycle_with_frontend(
        LifecycleAction::Uninstall,
        app_id,
        scope,
        None,
        &state_root,
        frontend.unwrap_or(Frontend::Gui),
    )
    .map_err(|error| miette::miette!("uninstall plan: {error}"))?;
    let work_root = work_root.unwrap_or_else(|| state_root.join("work"));
    // The payload root is only used to satisfy the backend's shape; an uninstall
    // never reads a payload file, because every file it removes is named by the
    // ledger and verified against it.
    let payload_root =
        std::env::current_dir().map_err(|error| miette::miette!("working directory: {error}"))?;
    let request = RuntimeRequest {
        target: ledger.target.clone(),
        app_id: app_id.clone(),
        app_version: ledger.version,
        scope,
        transaction_plan: execution,
        state_root: state_root.clone(),
        work_root,
        recovery_id: None,
        release: None,
        bootstrap: None,
    };
    let backend = zup_windows::WindowsRuntimeBackend::from_path(payload_root, None)
        .map_err(|error| miette::miette!("payload source: {error}"))?;
    Ok(PreparedRuntime {
        request,
        backend: std::sync::Arc::new(backend),
        action: LifecycleAction::Uninstall,
    })
}

/// The scope the installation this application has is actually in.
///
/// An application that installs machine-wide is only ever in that scope. Anything
/// else looks, because an application that allows either scope can legitimately
/// be installed in one of them, and uninstalling the wrong one is not a mistake
/// the user can easily undo.
fn resolve_scope(
    app_id: &AppId,
    installer: Option<&zup_core::Installer>,
    requested: ScopeArg,
    state_root: Option<&Path>,
) -> miette::Result<(SelectedScope, PathBuf)> {
    let declared = installer.map(|installer| installer.install.scope);
    let allowed: &[SelectedScope] = match declared {
        Some(InstallScope::User) => &[SelectedScope::User],
        Some(InstallScope::Machine) => &[SelectedScope::Machine],
        Some(InstallScope::Either) | None => &[SelectedScope::User, SelectedScope::Machine],
    };
    if declared == Some(InstallScope::Machine) {
        return Ok((
            SelectedScope::Machine,
            state::resolve_state_root(state_root.map(Path::to_path_buf), SelectedScope::Machine)?,
        ));
    }
    let named = match requested {
        ScopeArg::User => Some(SelectedScope::User),
        ScopeArg::Machine => Some(SelectedScope::Machine),
        ScopeArg::Either => None,
    };
    if let Some(scope) = named {
        if !allowed.contains(&scope) {
            return Err(miette::miette!(
                "this application does not install into the {scope} scope"
            ));
        }
        return Ok((
            scope,
            state::resolve_state_root(state_root.map(Path::to_path_buf), scope)?,
        ));
    }
    discover_scope(app_id, allowed, state_root)
}

fn discover_scope(
    app_id: &AppId,
    allowed: &[SelectedScope],
    state_root: Option<&Path>,
) -> miette::Result<(SelectedScope, PathBuf)> {
    let mut found = Vec::new();
    for scope in allowed.iter().copied() {
        let root = state::resolve_state_root(state_root.map(Path::to_path_buf), scope)?;
        let installed = zup_windows::InstallLedgerStore::new(&root)
            .load(app_id, scope)
            .map_err(|error| miette::miette!("ledger: {error}"))?
            .is_some();
        if installed {
            found.push((scope, root));
        }
    }
    if found.len() > 1 {
        return Err(miette::miette!(
            "this application is installed in both scopes; choose --scope"
        ));
    }
    if let Some(result) = found.pop() {
        return Ok(result);
    }
    let scope = SelectedScope::User;
    Ok((
        scope,
        state::resolve_state_root(state_root.map(Path::to_path_buf), scope)?,
    ))
}

/// Release the lock an uninstall takes, so the next run is not told to wait for
/// one that is no longer running.
pub fn remove_lock(state_root: &Path, app_id: &AppId, scope: SelectedScope) -> miette::Result<()> {
    let scope = match scope {
        SelectedScope::User => "user",
        SelectedScope::Machine => "machine",
    };
    let key = zup_windows::InstallationLock::lock_key(app_id.as_str(), scope);
    zup_windows::InstallationLock::remove_if_unheld(state_root, &key)
        .map_err(|error| miette::miette!("remove the uninstall lock: {error}"))
}

/// Start a copy of this runtime that outlives it.
///
/// The returned handle is deliberately dropped. Whoever started the runner is
/// either the file the uninstall deletes or a process a person is watching, and
/// in both cases waiting would mean the runner could never begin.
pub fn launch_runner(
    executable: &Path,
    args: &UninstallArgs,
    ui: bool,
) -> miette::Result<std::process::Child> {
    let temporary =
        std::env::temp_dir().join(format!("zup-uninstall-{}.exe", uuid::Uuid::now_v7()));
    zup_windows::copy_new_durable(executable, &temporary)
        .map_err(|error| miette::miette!("prepare the uninstall runner: {error}"))?;
    let mut command = std::process::Command::new(&temporary);
    command
        .arg("__uninstall_runner")
        .arg("--wait-pid")
        .arg(std::process::id().to_string());
    if ui {
        command.arg("--ui");
    }
    if let Some(scope) = args.scope {
        command.arg("--scope").arg(scope.as_str());
    }
    if let Some(value) = &args.app_id {
        command.arg("--app-id").arg(value);
    }
    if let Some(value) = &args.state_root {
        command.arg("--state-root").arg(value);
    }
    if let Some(value) = &args.work_root {
        command.arg("--work-root").arg(value);
    }
    if args.non_interactive {
        command.arg("--non-interactive");
    }
    if args.yes {
        command.arg("--yes");
    }
    if args.output != OutputArg::Human {
        command.arg("--output").arg(args.output.as_str());
    }
    // Detached on Windows so the runner does not inherit a console the parent may
    // close, and so the child is not killed by a job object when the parent exits.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        command.creation_flags(DETACHED_PROCESS);
    }
    command
        .spawn()
        .map_err(|error| miette::miette!("start the uninstall runner: {error}"))
}

/// Delete the runner once this process is gone.
///
/// A detached PowerShell window is used rather than a second process of the same
/// runtime because the runner has to survive *this* process, and a child cannot
/// wait on its own parent.
pub fn schedule_runner_cleanup(path: &Path) {
    let path = path.to_string_lossy().replace('\'', "''");
    let script = format!(
        "Wait-Process -Id {} -ErrorAction SilentlyContinue; Remove-Item -LiteralPath '{}' -Force -ErrorAction SilentlyContinue",
        std::process::id(),
        path
    );
    let _ = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-WindowStyle", "Hidden", "-Command", &script])
        .spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_machine_wide_application_is_only_ever_uninstalled_from_the_machine_scope() {
        let app = AppId::new("com.example.acme").expect("valid");
        let installer = zup_core::Installer {
            preset: None,
            app: zup_core::App {
                id: app.clone(),
                name: zup_core::NonEmptyString::new("Acme").expect("valid"),
                version: semver::Version::parse("1.0.0").expect("valid"),
                publisher: None,
                main: None,
                description: None,
            },
            target: zup_core::TargetTriple::parse("x86_64-pc-windows-msvc").expect("valid"),
            frontend: Frontend::Headless,
            updates: None,
            install: zup_core::Install {
                scope: InstallScope::Machine,
                directory: zup_core::InstallDirectory {
                    user: None,
                    machine: None,
                },
                allow_directory_override: false,
            },
            prerequisites: Vec::new(),
            components: Vec::new(),
            plugins: Vec::new(),
            files: Vec::new(),
            launchers: Vec::new(),
            path: Vec::new(),
            services: Vec::new(),
            protocols: Vec::new(),
            file_associations: Vec::new(),
        };
        let root = tempfile::TempDir::new().expect("temp dir");
        let (scope, _) = resolve_scope(&app, Some(&installer), ScopeArg::User, Some(root.path()))
            .expect("a machine-wide application resolves without asking");
        assert_eq!(scope, SelectedScope::Machine);
    }
}
