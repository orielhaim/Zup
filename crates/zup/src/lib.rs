//! Developer-facing CLI for zup.

use std::path::{Path, PathBuf};

use clap::{Args, Parser, Subcommand, ValueEnum};
use zup_core::{AppId, ComponentId, RelativePath, ResourceKey, SelectedScope, hash_reader};
use zup_exec::{LifecycleAction, RemovalKind};
use zup_plan::{PluginArchitecture, PluginHostFacts, PluginOperatingSystem};
use zup_runtime::{InstallOutcome, RuntimeRequest};

/// Process entry point for the internal worker mode.
#[derive(Debug, Parser)]
#[command(
    name = "zup",
    version,
    about = "A programmable application installer for the modern desktop",
    disable_help_subcommand = true
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    #[cfg(feature = "build")]
    Build(BuildCommand),
    Install(ManifestCommand),
    Upgrade(ManifestCommand),
    Update(UpdateCommand),
    Modify(ManifestCommand),
    Repair(RepairCommand),
    Uninstall(UninstallCommand),
    #[command(name = "__uninstall_runner", hide = true)]
    UninstallRunner(UninstallRunnerCommand),
    Recover(RecoverCommand),
    #[command(name = "__worker", hide = true)]
    Worker {
        bootstrap: String,
    },
    /// Print the protocol/worker bootstrap format for tests.
    #[command(hide = true)]
    WorkerHelp,
}

#[derive(Debug, Args)]
struct UpdateCommand {
    #[command(subcommand)]
    command: Option<UpdateCommands>,
    #[arg(long, value_enum)]
    scope: Option<ScopeArg>,
    #[arg(long)]
    state_root: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
enum UpdateCommands {
    Check,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ScopeArg {
    User,
    Machine,
}

impl From<ScopeArg> for SelectedScope {
    fn from(value: ScopeArg) -> Self {
        match value {
            ScopeArg::User => Self::User,
            ScopeArg::Machine => Self::Machine,
        }
    }
}

fn default_install_scope(scope: zup_core::InstallScope) -> SelectedScope {
    match scope {
        zup_core::InstallScope::Machine => SelectedScope::Machine,
        zup_core::InstallScope::User | zup_core::InstallScope::Either => SelectedScope::User,
    }
}

#[derive(Debug, Args)]
struct ManifestCommand {
    #[cfg(feature = "build")]
    #[arg(long, default_value = "zup.toml")]
    manifest: PathBuf,
    #[arg(long, value_enum, default_value = "user")]
    scope: ScopeArg,
    #[arg(long)]
    state_root: Option<PathBuf>,
    #[arg(long)]
    work_root: Option<PathBuf>,
    #[arg(long = "enable")]
    enable: Vec<String>,
    #[arg(long = "disable")]
    disable: Vec<String>,
}

#[derive(Debug, Args)]
struct RepairCommand {
    #[command(flatten)]
    install: ManifestCommand,
    #[arg(long)]
    force_files: bool,
}

#[derive(Debug, Args)]
struct UninstallCommand {
    #[arg(long)]
    app_id: Option<String>,
    #[arg(long, value_enum, default_value = "user")]
    scope: ScopeArg,
    #[arg(long)]
    state_root: Option<PathBuf>,
    #[arg(long)]
    work_root: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct UninstallRunnerCommand {
    #[arg(long)]
    wait_pid: u32,
    #[arg(long, default_value_t = false)]
    ui: bool,
    #[command(flatten)]
    uninstall: UninstallCommand,
}

#[derive(Debug, Args)]
struct RecoverCommand {
    #[arg(long)]
    transaction_id: uuid::Uuid,
    #[arg(long, value_enum, default_value = "user")]
    scope: ScopeArg,
    #[arg(long)]
    state_root: Option<PathBuf>,
    #[arg(long)]
    payload_root: Option<PathBuf>,
    #[arg(long)]
    work_root: Option<PathBuf>,
}

#[cfg(feature = "build")]
#[derive(Debug, Args)]
struct BuildCommand {
    #[arg(long, default_value = "zup.toml")]
    manifest: PathBuf,
    #[arg(long)]
    output: Option<PathBuf>,
    #[arg(long)]
    runtime: Option<PathBuf>,
    #[arg(long, default_value_t = default_build_target())]
    target: String,
}

pub fn run() -> miette::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        #[cfg(feature = "build")]
        Some(Commands::Build(args)) => run_build(args)?,
        Some(Commands::Install(args)) => run_manifest_transition(LifecycleAction::Install, args)?,
        Some(Commands::Upgrade(args)) => run_manifest_transition(LifecycleAction::Upgrade, args)?,
        Some(Commands::Update(args)) => run_update(args)?,
        Some(Commands::Modify(args)) => run_manifest_transition(LifecycleAction::Modify, args)?,
        Some(Commands::Repair(args)) => run_manifest_transition(
            LifecycleAction::Repair {
                force_files: args.force_files,
            },
            args.install,
        )?,
        Some(Commands::Uninstall(args)) => run_uninstall(args)?,
        Some(Commands::UninstallRunner(args)) => {
            zup_windows::wait_for_process_exit(args.wait_pid)
                .map_err(|error| miette::miette!("wait for maintenance process: {error}"))?;
            let cleanup_path =
                zup_windows::current_exe().map_err(|e| miette::miette!("executable: {e}"))?;
            let result = if args.ui {
                let executable = zup_windows::current_exe()
                    .map_err(|error| miette::miette!("executable: {error}"))?;
                let bundle = zup_bundle::EmbeddedBundle::open(&executable)
                    .map_err(|error| miette::miette!("installer package: {error}"))?;
                run_graphical_frontend(executable, &bundle, true, true)
            } else {
                run_uninstall(args.uninstall)
            };
            schedule_runner_cleanup(&cleanup_path);
            result?;
        }
        Some(Commands::Recover(args)) => run_recover(args)?,
        Some(Commands::Worker { bootstrap }) => run_worker_mode(&bootstrap)?,
        Some(Commands::WorkerHelp) => {
            println!(
                "zup __worker <protocol>|<session>|<pipe>|<parent_pid>|<parent_sid>|<plan_hash>"
            );
        }
        None => {
            let executable =
                zup_windows::current_exe().map_err(|e| miette::miette!("executable: {e}"))?;
            match zup_bundle::EmbeddedBundle::open(&executable) {
                Ok(bundle) => run_graphical_frontend(executable, &bundle, false, false)?,
                Err(error) if error.is_missing_resource() => {}
                Err(error) => return Err(miette::miette!("installer package: {error}")),
            }
        }
    }
    Ok(())
}

fn state_root(path: &Path) -> miette::Result<PathBuf> {
    std::fs::create_dir_all(path).map_err(|error| miette::miette!("state root: {error}"))?;
    path.canonicalize()
        .map_err(|error| miette::miette!("state root: {error}"))
}

fn default_state_root(scope: SelectedScope) -> miette::Result<PathBuf> {
    let (variable, folder) = match scope {
        SelectedScope::User => ("LOCALAPPDATA", "zup"),
        SelectedScope::Machine => ("PROGRAMDATA", "zup"),
    };
    let base = std::env::var_os(variable)
        .map(PathBuf::from)
        .ok_or_else(|| miette::miette!("{variable} is not available"))?;
    if scope == SelectedScope::Machine {
        let base = base
            .canonicalize()
            .map_err(|error| miette::miette!("{variable}: {error}"))?;
        Ok(base.join(folder))
    } else {
        state_root(&base.join(folder))
    }
}

fn choose_state_root(path: Option<PathBuf>, scope: SelectedScope) -> miette::Result<PathBuf> {
    match path {
        Some(path) if scope == SelectedScope::Machine => {
            if path.exists() {
                state_root(&path)
            } else if path.is_absolute() {
                Ok(path)
            } else {
                Ok(std::env::current_dir()
                    .map_err(|e| miette::miette!("working directory: {e}"))?
                    .join(path))
            }
        }
        Some(path) => state_root(&path),
        None => default_state_root(scope),
    }
}

#[cfg(feature = "build")]
fn default_build_target() -> String {
    #[cfg(all(windows, target_arch = "aarch64"))]
    {
        "aarch64-pc-windows-msvc".to_owned()
    }
    #[cfg(all(windows, not(target_arch = "aarch64")))]
    {
        "x86_64-pc-windows-msvc".to_owned()
    }
    #[cfg(not(windows))]
    {
        zup_plugin_contract::HOST_TARGET.to_owned()
    }
}

#[cfg(feature = "build")]
fn run_build(args: BuildCommand) -> miette::Result<()> {
    let manifest_path = args
        .manifest
        .canonicalize()
        .map_err(|e| miette::miette!("manifest: {e}"))?;
    let source =
        std::fs::read_to_string(&manifest_path).map_err(|e| miette::miette!("manifest: {e}"))?;
    let manifest = zup_manifest::parse(&source).map_err(|e| miette::miette!("manifest: {e}"))?;
    let installer =
        zup_manifest::parse_and_compile(&source).map_err(|e| miette::miette!("installer: {e}"))?;
    let build = zup_build::materialize(&manifest_path, &manifest, installer)
        .map_err(|e| miette::miette!("materialize: {e}"))?;
    let target = args.target;
    let runtime = match args.runtime {
        Some(path) => path
            .canonicalize()
            .map_err(|e| miette::miette!("runtime: {e}"))?,
        None => {
            let current =
                zup_windows::current_exe().map_err(|e| miette::miette!("runtime: {e}"))?;
            #[cfg(windows)]
            let setup = current.with_file_name("zup-setup.exe");
            #[cfg(not(windows))]
            let setup = current.with_file_name("zup-setup");
            setup.canonicalize().map_err(|error| {
                miette::miette!(
                    "windowed runtime `{}` is unavailable next to `{}` ({error}); build `zup-setup` or pass --runtime",
                    setup.display(),
                    current.display()
                )
            })?
        }
    };
    let runtime_target = zup_bundle::read_pe_target(&runtime)
        .map_err(|error| miette::miette!("runtime target: {error}"))?;
    if runtime_target != target {
        return Err(miette::miette!(
            "runtime target `{runtime_target}` does not match requested target `{target}`"
        ));
    }
    let plugin_artifacts = zup_plugin_build::compile_plugins(&build, &target)
        .map_err(|error| miette::miette!("plugin compilation: {error}"))?;
    let output = args.output.unwrap_or_else(|| {
        let name: String = build
            .installer
            .app
            .name
            .as_str()
            .chars()
            .map(|character| {
                if character.is_control() || "<>:\"/\\|?*".contains(character) {
                    '-'
                } else {
                    character
                }
            })
            .collect();
        let name = name.trim_matches([' ', '.']);
        let name = if name.is_empty() {
            build.installer.app.id.as_str()
        } else {
            name
        };
        manifest_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(format!("{name}-Setup.exe"))
    });
    let runtime_size = std::fs::metadata(&runtime)
        .map_err(|e| miette::miette!("runtime: {e}"))?
        .len();
    let (size, bundle_size) =
        zup_bundle::build_self_contained_executable(&runtime, &output, &build, &plugin_artifacts)
            .map_err(|e| miette::miette!("installer output: {e}"))?;
    println!(
        "{} ({} bytes total; {} bytes RCDATA package; {} bytes over runtime)",
        output.display(),
        size,
        bundle_size,
        size - runtime_size,
    );
    Ok(())
}

fn run_embedded_transition(
    action: LifecycleAction,
    scope: SelectedScope,
    state: Option<PathBuf>,
    enable: Vec<String>,
    disable: Vec<String>,
) -> miette::Result<()> {
    execute(prepare_embedded_transition(
        action, scope, state, enable, disable,
    )?)
}

fn prepare_embedded_transition(
    action: LifecycleAction,
    scope: SelectedScope,
    state: Option<PathBuf>,
    enable: Vec<String>,
    disable: Vec<String>,
) -> miette::Result<RuntimeRequest> {
    prepare_embedded_transition_with_cancellation(
        action,
        scope,
        state,
        enable,
        disable,
        &zup_plan::NeverCancelled,
    )
}

fn prepare_embedded_transition_with_cancellation(
    action: LifecycleAction,
    scope: SelectedScope,
    state: Option<PathBuf>,
    enable: Vec<String>,
    disable: Vec<String>,
    cancellation: &dyn zup_plan::CancellationQuery,
) -> miette::Result<RuntimeRequest> {
    let executable = zup_windows::current_exe().map_err(|e| miette::miette!("executable: {e}"))?;
    let bundle = zup_bundle::EmbeddedBundle::open(&executable)
        .map_err(|e| miette::miette!("installer package: {e}"))?;
    let build = bundle
        .build_plan()
        .map_err(|e| miette::miette!("installer plan: {e}"))?;
    let app_id = build.installer.app.id.clone();
    let state_root = choose_state_root(state, scope)?;
    let prior = zup_windows::InstallLedgerStore::new(&state_root)
        .load(&app_id, scope)
        .map_err(|e| miette::miette!("ledger: {e}"))?;
    prepare_embedded_request(
        EmbeddedPreparation {
            action,
            scope,
            build: &build,
            prior,
            state_root,
            payload_root: executable,
            enable,
            disable,
        },
        cancellation,
        || {
            zup_plugin_runtime::WasmtimePluginExecutor::load(
                bundle,
                zup_plugin_contract::HOST_TARGET,
            )
            .map_err(|error| miette::miette!("plugin runtime: {error}"))
        },
    )
}

struct EmbeddedPreparation<'a> {
    action: LifecycleAction,
    scope: SelectedScope,
    build: &'a zup_plan::BuildPlan,
    prior: Option<zup_exec::InstallLedger>,
    state_root: PathBuf,
    payload_root: PathBuf,
    enable: Vec<String>,
    disable: Vec<String>,
}

fn prepare_embedded_request<E>(
    preparation: EmbeddedPreparation<'_>,
    cancellation: &dyn zup_plan::CancellationQuery,
    load_executor: impl FnOnce() -> Result<E, miette::Report>,
) -> miette::Result<RuntimeRequest>
where
    E: zup_plan::PluginExecutor,
{
    let EmbeddedPreparation {
        action,
        scope,
        build,
        prior,
        state_root,
        payload_root,
        enable,
        disable,
    } = preparation;
    let app_id = build.installer.app.id.clone();
    if action == LifecycleAction::Uninstall {
        let ledger = prior.ok_or_else(|| miette::miette!("installation not found"))?;
        let execution =
            zup_windows::plan_target_lifecycle(action, &app_id, scope, None, &state_root)
                .map_err(|e| miette::miette!("lifecycle plan: {e}"))?;
        return Ok(RuntimeRequest {
            app_id,
            app_version: ledger.version,
            scope,
            execution_plan: execution,
            work_root: state_root.join("work"),
            state_root,
            payload_root,
            payload_overlay_root: None,
            payload_overlay_base_root: None,
            recovery_id: None,
        });
    }

    let mut request = zup_plan::PlanRequest::new(scope);
    if matches!(
        action,
        LifecycleAction::Upgrade | LifecycleAction::Modify | LifecycleAction::Repair { .. }
    ) {
        let previous = prior
            .as_ref()
            .ok_or_else(|| miette::miette!("installation not found"))?;
        for component in &build.installer.components {
            if previous.selected_components.contains(&component.id) {
                request.components.enable.insert(component.id.clone());
            } else if !component.required {
                request.components.disable.insert(component.id.clone());
            }
        }
    }
    for raw in enable {
        let id = ComponentId::new(&raw).map_err(|e| miette::miette!("component {raw}: {e}"))?;
        request.components.disable.remove(&id);
        request.components.enable.insert(id);
    }
    for raw in disable {
        let id = ComponentId::new(&raw).map_err(|e| miette::miette!("component {raw}: {e}"))?;
        request.components.enable.remove(&id);
        request.components.disable.insert(id);
    }

    let mut executor = load_executor()?;
    let planned = zup_plan::plan_with_plugins(
        build,
        &request,
        plugin_host_facts()?,
        &mut executor,
        cancellation,
    )
    .map_err(|error| miette::miette!("plan: {error}"))?;
    let install = &planned.plan;
    let mut target =
        zup_windows::resolve_target(install, &zup_windows::WindowsTargetContext::new(scope))
            .map_err(|e| miette::miette!("target: {e}"))?;
    let (size, sha256) = hash_reader(
        std::fs::File::open(&payload_root)
            .map_err(|error| miette::miette!("installer executable: {error}"))?,
    )
    .map_err(|error| miette::miette!("installer executable: {error}"))?;
    let scope_name = match scope {
        SelectedScope::User => "user",
        SelectedScope::Machine => "machine",
    };
    let destination = state_root
        .join("maintenance")
        .join(app_id.as_str())
        .join(scope_name)
        .join(target.app.version.to_string())
        .join("Setup.exe");
    let destination = zup_platform::TargetPath::new(destination)
        .map_err(|error| miette::miette!("maintenance destination: {error}"))?;
    target.files.push(zup_platform::TargetFile {
        key: ResourceKey::Maintenance {
            app_id: app_id.to_string(),
            version: target.app.version.to_string(),
            destination: destination.to_string(),
        },
        source_relative: RelativePath::new("__zup_maintenance__.exe").unwrap(),
        destination,
        size,
        sha256,
        privilege: scope.privilege(),
    });
    target.summary.file_count += 1;
    target.summary.install_bytes = target.summary.install_bytes.saturating_add(size);
    target.summary.resource_count += 1;
    let execution =
        zup_windows::plan_target_lifecycle(action, &app_id, scope, Some(&target), &state_root)
            .map_err(|e| miette::miette!("lifecycle plan: {e}"))?;
    let (payload_overlay_base_root, payload_overlay_root) =
        materialize_plugin_overlay(&state_root, scope, install, &planned.generated_files)?;
    let work_root = state_root.join("work");
    Ok(RuntimeRequest {
        app_id,
        app_version: target.app.version.clone(),
        scope,
        execution_plan: execution,
        state_root,
        work_root,
        payload_root,
        payload_overlay_root,
        payload_overlay_base_root,
        recovery_id: None,
    })
}

fn materialize_plugin_overlay(
    state_root: &Path,
    scope: SelectedScope,
    install: &zup_plan::InstallPlan,
    generated_files: &[zup_plan::GeneratedFile],
) -> miette::Result<(Option<PathBuf>, Option<PathBuf>)> {
    if generated_files.is_empty() {
        return Ok((None, None));
    }
    let base = zup_windows::payload_overlay_base_root(state_root, scope)
        .map_err(|error| miette::miette!("plugin payload overlay base: {error}"))?;
    match zup_windows::materialize_payload_overlay(&base, install, generated_files) {
        Ok(overlay) => Ok((Some(base), overlay)),
        Err(error) => {
            if let Ok(identity) = zup_windows::PayloadOverlayIdentity::from_install_plan(install)
                && let Some(overlay) = identity.path_under(&base)
            {
                let _ = zup_windows::cleanup_payload_overlay(&base, Some(&overlay));
            }
            Err(miette::miette!("plugin payload overlay: {error}"))
        }
    }
}

fn plugin_host_facts() -> miette::Result<PluginHostFacts> {
    let architecture = match std::env::consts::ARCH {
        "x86_64" => PluginArchitecture::X86_64,
        "aarch64" => PluginArchitecture::Aarch64,
        architecture => {
            return Err(miette::miette!(
                "unsupported plugin host architecture `{architecture}`"
            ));
        }
    };
    Ok(PluginHostFacts::new(
        PluginOperatingSystem::Windows,
        architecture,
    ))
}

fn run_manifest_transition(action: LifecycleAction, args: ManifestCommand) -> miette::Result<()> {
    let executable = zup_windows::current_exe().map_err(|e| miette::miette!("executable: {e}"))?;
    match zup_bundle::EmbeddedBundle::open(&executable) {
        Ok(bundle) => {
            let scope = if bundle.plan().installer.install.scope == zup_core::InstallScope::Machine
            {
                SelectedScope::Machine
            } else {
                SelectedScope::from(args.scope)
            };
            run_embedded_transition(action, scope, args.state_root, args.enable, args.disable)
        }
        Err(error) if error.is_missing_resource() => run_manifest_source_transition(action, args),
        Err(error) => Err(miette::miette!("installer package: {error}")),
    }
}

#[cfg(feature = "build")]
fn run_manifest_source_transition(
    action: LifecycleAction,
    args: ManifestCommand,
) -> miette::Result<()> {
    let scope = SelectedScope::from(args.scope);
    let state_root = choose_state_root(args.state_root.clone(), scope)?;
    let manifest_path = args
        .manifest
        .canonicalize()
        .map_err(|error| miette::miette!("manifest: {error}"))?;
    let source = std::fs::read_to_string(&manifest_path)
        .map_err(|error| miette::miette!("manifest: {error}"))?;
    let manifest =
        zup_manifest::parse(&source).map_err(|error| miette::miette!("manifest: {error}"))?;
    let installer = zup_manifest::parse_and_compile(&source)
        .map_err(|error| miette::miette!("installer: {error}"))?;
    let app_id = installer.app.id.clone();
    let prior = zup_windows::InstallLedgerStore::new(&state_root)
        .load(&app_id, scope)
        .map_err(|error| miette::miette!("ledger: {error}"))?;
    let build = zup_build::materialize(&manifest_path, &manifest, installer)
        .map_err(|error| miette::miette!("materialize: {error}"))?;
    let mut request = zup_plan::PlanRequest::new(scope);
    if matches!(action, LifecycleAction::Repair { .. })
        && (!args.enable.is_empty() || !args.disable.is_empty())
    {
        return Err(miette::miette!(
            "repair uses the committed component selection"
        ));
    }
    if matches!(
        action,
        LifecycleAction::Upgrade | LifecycleAction::Modify | LifecycleAction::Repair { .. }
    ) {
        let previous = prior
            .as_ref()
            .ok_or_else(|| miette::miette!("installation not found"))?;
        for component in &build.installer.components {
            if previous.selected_components.contains(&component.id) {
                request.components.enable.insert(component.id.clone());
            } else if !component.required {
                request.components.disable.insert(component.id.clone());
            }
        }
    }
    for raw in args.enable {
        let id =
            ComponentId::new(&raw).map_err(|error| miette::miette!("component {raw}: {error}"))?;
        request.components.disable.remove(&id);
        request.components.enable.insert(id);
    }
    for raw in args.disable {
        let id =
            ComponentId::new(&raw).map_err(|error| miette::miette!("component {raw}: {error}"))?;
        request.components.enable.remove(&id);
        request.components.disable.insert(id);
    }
    let install = zup_plan::plan(&build, &request).map_err(|error| match error {
        zup_plan::PlanError::PluginPlanningRequired { plugin_id } => miette::miette!(
            "active plugin `{plugin_id}` requires an embedded AOT package; source plugin JIT is disabled"
        ),
        error => miette::miette!("plan: {error}"),
    })?;
    let target =
        zup_windows::resolve_target(&install, &zup_windows::WindowsTargetContext::new(scope))
            .map_err(|error| miette::miette!("target: {error}"))?;
    let execution =
        zup_windows::plan_target_lifecycle(action, &app_id, scope, Some(&target), &state_root)
            .map_err(|error| miette::miette!("lifecycle plan: {error}"))?;
    let payload_root = manifest_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(&manifest.source.directory);
    let work_root = args.work_root.unwrap_or_else(|| state_root.join("work"));
    execute(RuntimeRequest {
        app_id,
        app_version: target.app.version.clone(),
        scope,
        execution_plan: execution,
        state_root,
        work_root,
        payload_root,
        payload_overlay_root: None,
        payload_overlay_base_root: None,
        recovery_id: None,
    })
}

#[cfg(not(feature = "build"))]
fn run_manifest_source_transition(
    _action: LifecycleAction,
    _args: ManifestCommand,
) -> miette::Result<()> {
    Err(miette::miette!(
        "source-manifest lifecycle mode is unavailable in a runtime-only zup build"
    ))
}

fn run_update(args: UpdateCommand) -> miette::Result<()> {
    let executable = zup_windows::current_exe().map_err(|e| miette::miette!("executable: {e}"))?;
    let bundle = zup_bundle::EmbeddedBundle::open(&executable).map_err(|e| {
        miette::miette!("update configuration requires an installed zup package: {e}")
    })?;
    let installer = &bundle.plan().installer;
    let config = installer
        .updates
        .as_ref()
        .ok_or_else(|| miette::miette!("updates are not configured in this package"))?;
    let mut scope = if installer.install.scope == zup_core::InstallScope::Machine {
        SelectedScope::Machine
    } else {
        args.scope
            .map(SelectedScope::from)
            .unwrap_or(SelectedScope::User)
    };
    let mut state_root = choose_state_root(args.state_root.clone(), scope)?;
    let mut ledger = zup_windows::InstallLedgerStore::new(&state_root)
        .load(&installer.app.id, scope)
        .map_err(|e| miette::miette!("installation ledger: {e}"))?;
    if ledger.is_none()
        && args.scope.is_none()
        && args.state_root.is_none()
        && scope == SelectedScope::User
    {
        scope = SelectedScope::Machine;
        state_root = choose_state_root(None, scope)?;
        ledger = zup_windows::InstallLedgerStore::new(&state_root)
            .load(&installer.app.id, scope)
            .map_err(|e| miette::miette!("installation ledger: {e}"))?;
    }
    let ledger =
        ledger.ok_or_else(|| miette::miette!("installation not found in selected scope"))?;
    let update_root = if scope == SelectedScope::Machine && args.state_root.is_none() {
        default_state_root(SelectedScope::User)?
    } else {
        state_root.clone()
    };
    let client = zup_update::Client::new(config, installer.app.id.as_str(), &update_root);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| miette::miette!("update runtime: {e}"))?;
    let result = runtime
        .block_on(client.check(&ledger.version))
        .map_err(|e| miette::miette!("update check: {e}"))?;
    match result {
        zup_update::CheckResult::UpToDate { current } => {
            println!("up to date ({current})");
        }
        zup_update::CheckResult::UpdateAvailable {
            current,
            available,
            target,
        } => {
            println!("update available: {current} → {available}");
            if args.command.is_none() {
                let downloaded = update_root
                    .join("updates")
                    .join("downloads")
                    .join(format!("Setup-{}.exe", uuid::Uuid::now_v7()));
                runtime
                    .block_on(client.download(&target, &downloaded))
                    .map_err(|e| miette::miette!("verified update download: {e}"))?;
                let status = std::process::Command::new(&downloaded)
                    .arg("upgrade")
                    .arg("--scope")
                    .arg(scope.to_string())
                    .arg("--state-root")
                    .arg(&state_root)
                    .spawn()
                    .map_err(|e| miette::miette!("start verified update: {e}"))?;
                println!("started verified Setup.exe (pid {})", status.id());
            }
        }
    }
    Ok(())
}

fn run_uninstall(args: UninstallCommand) -> miette::Result<()> {
    let executable = zup_windows::current_exe().map_err(|e| miette::miette!("executable: {e}"))?;
    if executable
        .to_string_lossy()
        .to_ascii_lowercase()
        .contains("\\maintenance\\")
    {
        launch_uninstall_runner(&executable, &args, false)?;
        return Ok(());
    }
    let embedded = match zup_bundle::EmbeddedBundle::open(&executable) {
        Ok(bundle) => Some(bundle),
        Err(error) if error.is_missing_resource() => None,
        Err(error) => return Err(miette::miette!("installer package: {error}")),
    };
    let scope = embedded.as_ref().map_or_else(
        || SelectedScope::from(args.scope),
        |bundle| {
            if bundle.plan().installer.install.scope == zup_core::InstallScope::Machine {
                SelectedScope::Machine
            } else {
                SelectedScope::from(args.scope)
            }
        },
    );
    let app_id = match args.app_id {
        Some(id) => AppId::new(&id).map_err(|error| miette::miette!("app ID: {error}"))?,
        None => embedded
            .as_ref()
            .ok_or_else(|| miette::miette!("installer package unavailable"))?
            .plan()
            .installer
            .app
            .id
            .clone(),
    };
    let state_root = choose_state_root(args.state_root, scope)?;
    if embedded.is_some() {
        let result = run_embedded_transition(
            LifecycleAction::Uninstall,
            scope,
            Some(state_root.clone()),
            vec![],
            vec![],
        );
        if result.is_ok() {
            remove_uninstall_lock(&state_root, &app_id, scope)?;
            zup_windows::cleanup_app_payload_overlays(&state_root, &app_id, scope)
                .map_err(|error| miette::miette!("cleanup payload overlays: {error}"))?;
        }
        return result;
    }
    let ledger = zup_windows::InstallLedgerStore::new(&state_root)
        .load(&app_id, scope)
        .map_err(|error| miette::miette!("ledger: {error}"))?
        .ok_or_else(|| miette::miette!("installation not found"))?;
    let execution = zup_windows::plan_target_lifecycle(
        LifecycleAction::Uninstall,
        &app_id,
        scope,
        None,
        &state_root,
    )
    .map_err(|error| miette::miette!("uninstall plan: {error}"))?;
    let work_root = args.work_root.unwrap_or_else(|| state_root.join("work"));
    let result = execute(RuntimeRequest {
        app_id: app_id.clone(),
        app_version: ledger.version,
        scope,
        execution_plan: execution,
        payload_root: std::env::current_dir()
            .map_err(|error| miette::miette!("working directory: {error}"))?,
        payload_overlay_root: None,
        payload_overlay_base_root: None,
        state_root: state_root.clone(),
        work_root,
        recovery_id: None,
    });
    if result.is_ok() {
        remove_uninstall_lock(&state_root, &app_id, scope)?;
        zup_windows::cleanup_app_payload_overlays(&state_root, &app_id, scope)
            .map_err(|error| miette::miette!("cleanup payload overlays: {error}"))?;
    }
    result
}

fn remove_uninstall_lock(
    state_root: &Path,
    app_id: &AppId,
    scope: SelectedScope,
) -> miette::Result<()> {
    let scope = match scope {
        SelectedScope::User => "user",
        SelectedScope::Machine => "machine",
    };
    let key = zup_windows::InstallationLock::lock_key(app_id.as_str(), scope);
    zup_windows::InstallationLock::remove_if_unheld(state_root, &key)
        .map_err(|error| miette::miette!("remove uninstall lock: {error}"))
}

fn launch_uninstall_runner(
    executable: &Path,
    args: &UninstallCommand,
    ui: bool,
) -> miette::Result<()> {
    let temporary =
        std::env::temp_dir().join(format!("zup-uninstall-{}.exe", uuid::Uuid::now_v7()));
    zup_windows::copy_new_durable(executable, &temporary)
        .map_err(|error| miette::miette!("prepare uninstall runner: {error}"))?;
    let mut command = std::process::Command::new(&temporary);
    command
        .arg("__uninstall_runner")
        .arg("--wait-pid")
        .arg(std::process::id().to_string());
    if ui {
        command.arg("--ui");
    }
    command.arg("--scope").arg(match args.scope {
        ScopeArg::User => "user",
        ScopeArg::Machine => "machine",
    });
    if let Some(value) = &args.app_id {
        command.arg("--app-id").arg(value);
    }
    if let Some(value) = &args.state_root {
        command.arg("--state-root").arg(value);
    }
    if let Some(value) = &args.work_root {
        command.arg("--work-root").arg(value);
    }
    command
        .spawn()
        .map_err(|error| miette::miette!("start uninstall runner: {error}"))?;
    Ok(())
}

fn schedule_runner_cleanup(path: &Path) {
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

fn run_recover(args: RecoverCommand) -> miette::Result<()> {
    use zup_transaction::TransactionStore;
    let executable = zup_windows::current_exe().map_err(|e| miette::miette!("executable: {e}"))?;
    let bundle = match zup_bundle::EmbeddedBundle::open(&executable) {
        Ok(bundle) => Some(bundle),
        Err(error) if error.is_missing_resource() => None,
        Err(error) => return Err(miette::miette!("installer package: {error}")),
    };
    let scope = bundle.as_ref().map_or_else(
        || SelectedScope::from(args.scope),
        |bundle| default_install_scope(bundle.plan().installer.install.scope),
    );
    let state_root = choose_state_root(args.state_root, scope)?;
    let id = zup_transaction::TransactionId::from_uuid(args.transaction_id);
    let record = zup_transaction::FilesystemTransactionStore::new(&state_root)
        .load(&id)
        .map_err(|error| miette::miette!("transaction: {error}"))?;
    if record
        .plan
        .nodes
        .iter()
        .any(|node| matches!(node.kind, zup_transaction::NodeKind::StageFile { .. }))
        && args.payload_root.is_none()
        && bundle.is_none()
    {
        return Err(miette::miette!(
            "--payload-root is required to recover a file installation"
        ));
    }
    let overlay_identity = zup_windows::PayloadOverlayIdentity::from_transaction(
        record.app_id.clone(),
        record.app_version.clone(),
        record.scope,
        &record.plan,
    )
    .map_err(|error| miette::miette!("payload overlay identity: {error}"))?;
    let payload_overlay_base_root = if overlay_identity.has_files() {
        Some(
            zup_windows::payload_overlay_base_root(&state_root, record.scope)
                .map_err(|error| miette::miette!("payload overlay base: {error}"))?,
        )
    } else {
        None
    };
    let payload_overlay_root = payload_overlay_base_root
        .as_deref()
        .and_then(|base| overlay_identity.path_under(base));
    if let (Some(base), Some(overlay)) = (
        payload_overlay_base_root.as_deref(),
        payload_overlay_root.as_deref(),
    ) {
        zup_windows::verify_payload_overlay(base, &overlay_identity, overlay)
            .map_err(|error| miette::miette!("payload overlay recovery: {error}"))?;
    }
    let work_root = args.work_root.unwrap_or_else(|| state_root.join("work"));
    let payload_root = args.payload_root.unwrap_or_else(|| {
        if bundle.is_some() {
            executable
        } else {
            std::env::current_dir().unwrap_or_else(|_| state_root.clone())
        }
    });
    execute(RuntimeRequest {
        app_id: record.app_id,
        app_version: record.app_version,
        scope: record.scope,
        execution_plan: zup_exec::ExecutionPlan::default(),
        state_root,
        work_root,
        payload_root,
        payload_overlay_root,
        payload_overlay_base_root,
        recovery_id: Some(id),
    })
}

fn execute(request: RuntimeRequest) -> miette::Result<()> {
    let drifted: Vec<String> = request
        .execution_plan
        .removals
        .iter()
        .filter(|op| op.kind == RemovalKind::Drift)
        .map(|op| format!("{:?}", op.key))
        .collect();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| {
            if request.recovery_id.is_none() {
                cleanup_overlay(
                    &request.state_root,
                    request.scope,
                    request.payload_overlay_base_root.as_deref(),
                    request.payload_overlay_root.as_deref(),
                );
            }
            miette::miette!("runtime: {error}")
        })?;
    let (outcome, _) = runtime
        .block_on(zup_runtime::run_install(request))
        .map_err(|error| miette::miette!("install session: {error}"))?;
    match outcome {
        InstallOutcome::Committed => {
            println!("committed");
            for key in drifted {
                eprintln!("left drifted resource untouched: {key}");
            }
            Ok(())
        }
        other => Err(miette::miette!("transaction: {other:?}")),
    }
}

fn execute_with_control(
    request: RuntimeRequest,
    cancel: zup_runtime::CancellationHandle,
    events: tokio::sync::broadcast::Sender<zup_runtime::RuntimeEvent>,
) -> miette::Result<InstallOutcome> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| {
            if request.recovery_id.is_none() {
                cleanup_overlay(
                    &request.state_root,
                    request.scope,
                    request.payload_overlay_base_root.as_deref(),
                    request.payload_overlay_root.as_deref(),
                );
            }
            miette::miette!("runtime: {error}")
        })?;
    runtime
        .block_on(zup_runtime::run_install_control(request, cancel, events))
        .map_err(|error| miette::miette!("install session: {error}"))
}

fn cleanup_overlay(
    state_root: &Path,
    scope: SelectedScope,
    base: Option<&Path>,
    overlay: Option<&Path>,
) {
    let Some(overlay) = overlay else {
        return;
    };
    let base = base
        .map(Path::to_path_buf)
        .or_else(|| zup_windows::payload_overlay_base_root(state_root, scope).ok());
    if let Some(base) = base {
        let _ = zup_windows::cleanup_payload_overlay(&base, Some(overlay));
    }
}

fn run_graphical_frontend(
    executable: PathBuf,
    bundle: &zup_bundle::EmbeddedBundle,
    force_maintenance: bool,
    auto_uninstall: bool,
) -> miette::Result<()> {
    let installer = &bundle.plan().installer;
    let app_id = &installer.app.id;
    let scopes = match installer.install.scope {
        zup_core::InstallScope::User => vec![SelectedScope::User],
        zup_core::InstallScope::Machine => vec![SelectedScope::Machine],
        zup_core::InstallScope::Either => vec![SelectedScope::User, SelectedScope::Machine],
    };
    let maintenance_launch = force_maintenance || is_maintenance_executable(&executable);
    let installed = scopes.iter().find_map(|scope| {
        let state = choose_state_root(None, *scope).ok()?;
        let ledger = zup_windows::InstallLedgerStore::new(state)
            .load(app_id, *scope)
            .ok()??;
        Some((*scope, ledger))
    });
    let selected_scope = installed.as_ref().map_or(scopes[0], |(scope, _)| *scope);
    let components = installer
        .components
        .iter()
        .map(|component| zup_ui::ComponentOption {
            id: component.id.clone(),
            name: component.name.to_string(),
            description: component.description.clone(),
            required: component.required,
            selected: installed
                .as_ref()
                .map_or(component.default || component.required, |(_, ledger)| {
                    ledger.selected_components.contains(&component.id)
                }),
        })
        .collect::<Vec<_>>();
    let identity = zup_ui::ProductIdentity {
        name: installer.app.name.to_string(),
        publisher: installer.app.publisher.as_ref().map(ToString::to_string),
        version: installer.app.version.to_string(),
        description: installer.app.description.clone(),
    };
    let surface = if maintenance_launch {
        let (_scope, ledger) = installed
            .as_ref()
            .ok_or_else(|| miette::miette!("installed application was not found"))?;
        zup_ui::Surface::Maintenance {
            identity,
            installed_version: ledger.version.to_string(),
            components: components
                .iter()
                .cloned()
                .map(|mut component| {
                    component.selected = ledger.selected_components.contains(&component.id);
                    component
                })
                .collect(),
            updates_enabled: installer.updates.is_some(),
        }
    } else {
        zup_ui::Surface::Installer {
            identity,
            install: zup_ui::InstallModel {
                existing_version: installed
                    .as_ref()
                    .map(|(_, ledger)| ledger.version.to_string()),
                scopes: if installed.is_some() {
                    vec![selected_scope]
                } else {
                    scopes
                },
                selected_scope,
                components,
            },
        }
    };
    let (commands_tx, commands_rx) = std::sync::mpsc::channel();
    let (events_tx, events_rx) = std::sync::mpsc::channel();
    let state = installed
        .map(|(scope, ledger)| {
            choose_state_root(None, scope).map(|state| (scope, state, ledger.version))
        })
        .transpose()?;
    let backend_exe = executable.clone();
    std::thread::Builder::new()
        .name("zup-ui-runtime".into())
        .spawn(move || ui_backend(backend_exe, selected_scope, state, commands_rx, events_tx))
        .map_err(|error| miette::miette!("start UI runtime bridge: {error}"))?;
    if auto_uninstall {
        commands_tx
            .send(zup_ui::UiCommand::ConfirmUninstall)
            .map_err(|error| miette::miette!("start uninstall session: {error}"))?;
    }
    zup_ui::run(surface, commands_tx, events_rx);
    Ok(())
}

fn ui_backend(
    executable: PathBuf,
    default_scope: SelectedScope,
    installed: Option<(SelectedScope, PathBuf, semver::Version)>,
    commands: std::sync::mpsc::Receiver<zup_ui::UiCommand>,
    events: std::sync::mpsc::Sender<zup_ui::UiEvent>,
) {
    use std::sync::{Arc, Mutex};
    use zup_ui::{UiCommand as C, UiEvent as E};

    let cancel_slot = Arc::new(Mutex::new(None::<zup_runtime::CancellationHandle>));
    let retry_slot = Arc::new(Mutex::new(None::<RetryIntent>));
    let bridge = UiOperationBridge {
        cancel_slot: cancel_slot.clone(),
        retry_slot: retry_slot.clone(),
        events: events.clone(),
    };
    for command in commands {
        match command {
            C::Cancel => {
                if let Some(cancel) = cancel_slot.lock().expect("cancel state").as_ref() {
                    cancel.cancel();
                    let _ = events.send(E::CancellationWaiting);
                } else {
                    let _ = events.send(E::OperationFinished(InstallOutcome::Cancelled));
                }
            }
            C::Uninstall => {
                let _ = events.send(E::ConfirmUninstall);
            }
            C::DismissUninstall => {
                let _ = events.send(E::DismissUninstall);
            }
            C::Update => {
                let exe = executable.clone();
                let events = events.clone();
                let scope = installed
                    .as_ref()
                    .map_or(default_scope, |(scope, _, _)| *scope);
                let current = installed
                    .as_ref()
                    .map_or_else(|| "unknown".into(), |(_, _, version)| version.to_string());
                std::thread::spawn(move || {
                    let result = update_from_ui(&exe, scope);
                    match result {
                        Ok(Some((current, available))) => {
                            let _ = events.send(E::UpdateAvailable { current, available });
                        }
                        Ok(None) => {
                            let _ = events.send(E::UpToDate { current });
                        }
                        Err(message) => {
                            let _ = events.send(E::Error {
                                message,
                                recovery_required: false,
                            });
                        }
                    }
                });
            }
            C::Install { scope, components } => {
                start_ui_transition(
                    executable.clone(),
                    scope,
                    if installed.is_some() {
                        LifecycleAction::Upgrade
                    } else {
                        LifecycleAction::Install
                    },
                    components,
                    false,
                    bridge.clone(),
                );
            }
            C::Modify { components } => {
                let scope = installed
                    .as_ref()
                    .map_or(default_scope, |(scope, _, _)| *scope);
                start_ui_transition(
                    executable.clone(),
                    scope,
                    LifecycleAction::Modify,
                    components,
                    false,
                    bridge.clone(),
                );
            }
            C::Repair => {
                let scope = installed
                    .as_ref()
                    .map_or(default_scope, |(scope, _, _)| *scope);
                let components = installed_components(&executable, scope).unwrap_or_default();
                start_ui_transition(
                    executable.clone(),
                    scope,
                    LifecycleAction::Repair { force_files: false },
                    components,
                    false,
                    bridge.clone(),
                );
            }
            C::ConfirmUninstall => {
                let scope = installed
                    .as_ref()
                    .map_or(default_scope, |(scope, _, _)| *scope);
                if is_maintenance_executable(&executable) {
                    let Some((scope, state_root, _)) = installed.as_ref() else {
                        let _ = events.send(E::Error {
                            message: "Installed application was not found".into(),
                            recovery_required: false,
                        });
                        continue;
                    };
                    let app_id = match zup_bundle::EmbeddedBundle::open(&executable) {
                        Ok(bundle) => bundle.plan().installer.app.id.to_string(),
                        Err(error) => {
                            let _ = events.send(E::Error {
                                message: format!(
                                    "The maintenance package could not be read: {error}"
                                ),
                                recovery_required: false,
                            });
                            continue;
                        }
                    };
                    let args = UninstallCommand {
                        app_id: Some(app_id),
                        scope: match scope {
                            SelectedScope::User => ScopeArg::User,
                            SelectedScope::Machine => ScopeArg::Machine,
                        },
                        state_root: Some(state_root.clone()),
                        work_root: None,
                    };
                    match launch_uninstall_runner(&executable, &args, true) {
                        Ok(()) => {
                            let _ = events.send(E::Quit);
                        }
                        Err(error) => {
                            let _ = events.send(E::Error {
                                message: error.to_string(),
                                recovery_required: false,
                            });
                        }
                    }
                } else {
                    start_ui_transition(
                        executable.clone(),
                        scope,
                        LifecycleAction::Uninstall,
                        vec![],
                        true,
                        bridge.clone(),
                    );
                }
            }
            C::Retry => {
                if let Some(intent) = retry_slot.lock().expect("retry state").clone() {
                    start_ui_transition(
                        executable.clone(),
                        intent.scope,
                        intent.action,
                        intent.components,
                        intent.cleanup_lock,
                        bridge.clone(),
                    );
                }
            }
        }
    }
}

fn is_maintenance_executable(executable: &Path) -> bool {
    executable
        .to_string_lossy()
        .to_ascii_lowercase()
        .contains("\\maintenance\\")
}

#[derive(Clone)]
struct RetryIntent {
    scope: SelectedScope,
    action: LifecycleAction,
    components: Vec<ComponentId>,
    cleanup_lock: bool,
}

#[derive(Clone)]
struct UiOperationBridge {
    cancel_slot: std::sync::Arc<std::sync::Mutex<Option<zup_runtime::CancellationHandle>>>,
    retry_slot: std::sync::Arc<std::sync::Mutex<Option<RetryIntent>>>,
    events: std::sync::mpsc::Sender<zup_ui::UiEvent>,
}

fn start_ui_transition(
    executable: PathBuf,
    scope: SelectedScope,
    action: LifecycleAction,
    selected: Vec<ComponentId>,
    cleanup_lock: bool,
    bridge: UiOperationBridge,
) {
    let cancel = zup_runtime::CancellationHandle::new();
    *bridge.cancel_slot.lock().expect("cancel state") = Some(cancel.clone());
    *bridge.retry_slot.lock().expect("retry state") = Some(RetryIntent {
        scope,
        action,
        components: selected.clone(),
        cleanup_lock,
    });
    let events = bridge.events.clone();
    std::thread::spawn(move || {
        let _ = events.send(zup_ui::UiEvent::Progress {
            completed: 0,
            total: 0,
            action: "Preparing…".into(),
        });
        let enable = selected.iter().map(ToString::to_string).collect::<Vec<_>>();
        let disabled = zup_bundle::EmbeddedBundle::open(&executable)
            .ok()
            .map(|bundle| {
                bundle
                    .plan()
                    .installer
                    .components
                    .iter()
                    .filter(|item| !item.required && !selected.contains(&item.id))
                    .map(|item| item.id.to_string())
                    .collect()
            })
            .unwrap_or_default();
        let request = prepare_embedded_transition_with_cancellation(
            action, scope, None, enable, disabled, &cancel,
        );
        let request = match request {
            Ok(request) => request,
            Err(error) => {
                let _ = events.send(zup_ui::UiEvent::Error {
                    message: error.to_string(),
                    recovery_required: false,
                });
                *bridge.cancel_slot.lock().expect("cancel state") = None;
                return;
            }
        };
        let drifted = request
            .execution_plan
            .removals
            .iter()
            .filter(|op| op.kind == RemovalKind::Drift)
            .map(|op| format!("{:?}", op.key))
            .collect::<Vec<_>>();
        let app_id = request.app_id.clone();
        let state_root = request.state_root.clone();
        let (runtime_events, _) = tokio::sync::broadcast::channel(256);
        let mut rx = runtime_events.subscribe();
        let event_tx = events.clone();
        let pump = std::thread::spawn(move || {
            while let Ok(event) = rx.blocking_recv() {
                let terminal = matches!(
                    event,
                    zup_runtime::RuntimeEvent::Completed { .. }
                        | zup_runtime::RuntimeEvent::Failed { .. }
                );
                let _ = event_tx.send(zup_ui::UiEvent::Runtime(event));
                if terminal {
                    break;
                }
            }
        });
        let overlay_state_root = request.state_root.clone();
        let overlay_base_root = request.payload_overlay_base_root.clone();
        let overlay_root = request.payload_overlay_root.clone();
        let recovery = request.recovery_id.is_some();
        match execute_with_control(request, cancel, runtime_events) {
            Ok(outcome) => {
                if !matches!(outcome, InstallOutcome::RecoveryRequired) {
                    cleanup_overlay(
                        &overlay_state_root,
                        scope,
                        overlay_base_root.as_deref(),
                        overlay_root.as_deref(),
                    );
                }
                if matches!(&outcome, InstallOutcome::Failed(message) if message == "blocked by running applications")
                {
                    *bridge.cancel_slot.lock().expect("cancel state") = None;
                    let _ = pump.join();
                    return;
                }
                if matches!(action, LifecycleAction::Repair { .. })
                    && outcome == InstallOutcome::Committed
                {
                    let _ = events.send(zup_ui::UiEvent::RepairFinished {
                        drifted_resources: drifted,
                    });
                }
                if cleanup_lock && outcome == InstallOutcome::Committed {
                    let _ = remove_uninstall_lock(&state_root, &app_id, scope);
                    let _ = zup_windows::cleanup_app_payload_overlays(&state_root, &app_id, scope);
                }
                let _ = events.send(zup_ui::UiEvent::OperationFinished(outcome));
            }
            Err(error) => {
                if !recovery {
                    cleanup_overlay(
                        &overlay_state_root,
                        scope,
                        overlay_base_root.as_deref(),
                        overlay_root.as_deref(),
                    );
                }
                let _ = events.send(zup_ui::UiEvent::Error {
                    message: error.to_string(),
                    recovery_required: false,
                });
            }
        }
        let _ = pump.join();
        *bridge.cancel_slot.lock().expect("cancel state") = None;
    });
}

fn installed_components(
    executable: &Path,
    scope: SelectedScope,
) -> miette::Result<Vec<ComponentId>> {
    let bundle = zup_bundle::EmbeddedBundle::open(executable)
        .map_err(|e| miette::miette!("package: {e}"))?;
    let state = choose_state_root(None, scope)?;
    let ledger = zup_windows::InstallLedgerStore::new(&state)
        .load(&bundle.plan().installer.app.id, scope)
        .map_err(|e| miette::miette!("ledger: {e}"))?
        .ok_or_else(|| miette::miette!("installation not found"))?;
    Ok(ledger.selected_components)
}

fn update_from_ui(
    executable: &Path,
    scope: SelectedScope,
) -> Result<Option<(String, String)>, String> {
    let bundle = zup_bundle::EmbeddedBundle::open(executable).map_err(|e| e.to_string())?;
    let installer = &bundle.plan().installer;
    let config = installer
        .updates
        .as_ref()
        .ok_or_else(|| "Updates are not configured for this application".to_owned())?;
    let state = choose_state_root(None, scope).map_err(|e| e.to_string())?;
    let ledger = zup_windows::InstallLedgerStore::new(&state)
        .load(&installer.app.id, scope)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "Installation not found".to_owned())?;
    let update_root = if scope == SelectedScope::Machine {
        default_state_root(SelectedScope::User).map_err(|e| e.to_string())?
    } else {
        state.clone()
    };
    let client = zup_update::Client::new(config, installer.app.id.as_str(), &update_root);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    match runtime
        .block_on(client.check(&ledger.version))
        .map_err(|e| e.to_string())?
    {
        zup_update::CheckResult::UpToDate { current: _ } => Ok(None),
        zup_update::CheckResult::UpdateAvailable {
            current,
            available,
            target,
        } => {
            let destination = update_root
                .join("updates")
                .join("downloads")
                .join(format!("Setup-{}.exe", uuid::Uuid::now_v7()));
            runtime
                .block_on(client.download(&target, &destination))
                .map_err(|e| e.to_string())?;
            std::process::Command::new(destination)
                .spawn()
                .map_err(|e| e.to_string())?;
            Ok(Some((current.to_string(), available.to_string())))
        }
    }
}

/// Hidden elevated/unelevated worker entry.
fn run_worker_mode(bootstrap_arg: &str) -> miette::Result<()> {
    let bootstrap = zup_windows::parse_bootstrap(bootstrap_arg)
        .map_err(|e| miette::miette!("worker bootstrap rejected: {e}"))?;

    if bootstrap.expected_parent_pid == 0 {
        return Err(miette::miette!(
            "worker bootstrap rejected: zero parent pid"
        ));
    }

    // Run the real worker runtime (connect → authenticate → execute → exit).
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| miette::miette!("tokio runtime: {e}"))?;

    let cancel = tokio_util::sync::CancellationToken::new();
    rt.block_on(zup_windows::run_worker(bootstrap, cancel))
        .map_err(|e| miette::miette!("worker failed: {e}"))
        .map(|_outcome| ())
}

#[cfg(all(test, feature = "build"))]
mod tests {
    use std::cell::Cell;

    use tempfile::TempDir;
    use zup_plan::{
        CancellationQuery, PluginExecutor, PluginFailure, PluginPlanningContext,
        PluginResourceProposal,
    };

    use super::*;

    struct FakeExecutor;

    impl PluginExecutor for FakeExecutor {
        fn plan(
            &mut self,
            _binding: &zup_core::PluginBinding,
            _context: &PluginPlanningContext,
            _cancellation: &dyn CancellationQuery,
        ) -> Result<PluginResourceProposal, PluginFailure> {
            unreachable!()
        }
    }

    fn build_with_plugin(root: &TempDir) -> zup_build::BuildPlan {
        std::fs::create_dir_all(root.path().join("dist")).unwrap();
        std::fs::create_dir_all(root.path().join("plugins")).unwrap();
        std::fs::write(root.path().join("plugins/helper.wasm"), b"plugin").unwrap();
        let source = r#"
schema = 1
[app]
id = "com.example.embedded-plugin"
name = "Embedded Plugin"
version = "1.0.0"
[source]
directory = "dist"
[install]
scope = "user"
[install.directory]
user = "${known.local_app_data}/EmbeddedPlugin"
[[plugins]]
id = "helper"
source = "plugins/helper.wasm"
"#;
        let manifest = zup_manifest::parse(source).unwrap();
        let installer = zup_manifest::parse_and_compile(source).unwrap();
        zup_build::materialize(&root.path().join("zup.toml"), &manifest, installer).unwrap()
    }

    #[test]
    fn uninstall_does_not_load_the_plugin_executor() {
        let root = TempDir::new().unwrap();
        let build = build_with_plugin(&root);
        let mut ledger =
            zup_exec::InstallLedger::new(build.installer.app.id.clone(), SelectedScope::User);
        ledger.version = build.installer.app.version.clone();
        let loads = Cell::new(0);
        let result = prepare_embedded_request(
            EmbeddedPreparation {
                action: LifecycleAction::Uninstall,
                scope: SelectedScope::User,
                build: &build,
                prior: Some(ledger),
                state_root: root.path().join("state"),
                payload_root: root.path().to_path_buf(),
                enable: Vec::new(),
                disable: Vec::new(),
            },
            &zup_plan::NeverCancelled,
            || {
                loads.set(loads.get() + 1);
                Ok(FakeExecutor)
            },
        );
        assert!(result.is_err());
        assert_eq!(loads.get(), 0);
    }

    #[test]
    fn ui_cancellation_is_forwarded_before_lifecycle_planning() {
        let root = TempDir::new().unwrap();
        let build = build_with_plugin(&root);
        let cancel = zup_runtime::CancellationHandle::new();
        cancel.cancel();
        let result = prepare_embedded_request(
            EmbeddedPreparation {
                action: LifecycleAction::Install,
                scope: SelectedScope::User,
                build: &build,
                prior: None,
                state_root: root.path().join("state"),
                payload_root: root.path().to_path_buf(),
                enable: Vec::new(),
                disable: Vec::new(),
            },
            &cancel,
            || Ok(FakeExecutor),
        );
        let error = match result {
            Ok(_) => panic!("cancelled planning unexpectedly succeeded"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("planning was cancelled"));
    }
}
