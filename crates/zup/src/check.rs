//! `zup check` and `zup plan`: what the project says, and what it would do.
//!
//! Neither command changes anything. `check` answers "is this project coherent";
//! `plan` answers "what would installing it do on this machine". They share the
//! selection and materialization path with `build` on purpose: a check that
//! re-implements the build's own traversal is a check that agrees with the build
//! until the day it does not.

use std::path::Path;

use zup_core::SelectedScope;
use zup_exec::LifecycleAction;

use crate::cli::{CheckCommand, PlanCommand};
use crate::project::{self, LoadedProject};

/// Validate a project and the files it will ship.
pub fn run_check(args: CheckCommand) -> miette::Result<()> {
    let loaded = project::load_for_build(
        &args.project.manifest,
        &args.project.target,
        &args.project.overrides(),
    )?;
    let mut variants = Vec::with_capacity(loaded.selected_targets.len());
    for (config, plan) in loaded.selected_targets.iter().zip(&loaded.build.targets) {
        // A check that stopped at the manifest would report a project with a
        // broken plugin as valid, which is the one thing a check must never do.
        // The plugin compile is in memory and writes nothing.
        if !plan.installer.plugins.is_empty() {
            zup_plugin_build::compile_plugins(plan).map_err(|error| {
                miette::miette!("plugin check for `{}`: {error}", config.profile)
            })?;
        }
        println!(
            "✓ {} is valid ({})",
            plan.installer.app.name, config.profile
        );
        println!("  Target      {}", config.target);
        println!("  Source      {}", config.source.directory.display());
        println!(
            "  Install     {} · {}",
            config.install.scope,
            project::install_directory_text(&config.install)
        );
        println!("  Components  {}", plan.installer.components.len());
        println!("  Files       {}", plan.files.len());
        println!("  Plugins     {}", plan.installer.plugins.len());
        variants.push(
            zup_artifact::DistributionVariant::resolve(config, plan, &[], None)
                .map_err(|error| miette::miette!("variant `{}`: {error}", config.profile))?,
        );
    }
    report_composition(&loaded, &variants);
    Ok(())
}

/// Say whether the selected targets can be one artifact, and what it would save.
///
/// Composition is refused loudly rather than suggested quietly, because a project
/// that silently ships two installers where one would do has a problem nobody was
/// told about.
fn report_composition(loaded: &LoadedProject, variants: &[zup_artifact::DistributionVariant]) {
    if variants.len() < 2 {
        return;
    }
    let borrowed = variants.iter().collect::<Vec<_>>();
    let names = loaded
        .selected_targets
        .iter()
        .map(|config| config.profile.to_string())
        .collect::<Vec<_>>()
        .join("\n              ");
    match zup_artifact::check_compatibility(&borrowed) {
        Err(incompatible) => {
            println!("\n✗ {} cannot be composed", names.replace('\n', ", "));
            println!("  {}", incompatible.reason.dimension().as_str());
            println!("  {}", incompatible.reason.detail());
            println!("  Build them separately with `zup build --target <profile>`");
        }
        Ok(()) => match crate::artifacts::composition_report(&borrowed) {
            Ok(savings) => {
                println!("\n✓ Can be composed as one Windows universal installer");
                println!("\n  Estimated:");
                println!(
                    "    standalone total    {}",
                    zup_presentation::format_bytes(savings.standalone_size)
                );
                println!(
                    "    unique content      {}",
                    zup_presentation::format_bytes(savings.standalone_size - savings.shared_size)
                );
                println!(
                    "    shared content      {} ({} blobs)",
                    zup_presentation::format_bytes(savings.shared_size),
                    savings.unique_blob_count
                );
            }
            Err(refusal) => {
                println!("\n✗ cannot be composed ({})", refusal.dimension.as_str());
                println!("  {}", refusal.message);
            }
        },
    }
}

/// Show what installing this project would do, without changing anything.
pub fn run_plan(args: PlanCommand) -> miette::Result<()> {
    let loaded = project::load_single_project(
        &args.project.manifest,
        &args.project.target,
        &args.project.overrides(),
    )?;
    let config = loaded
        .selected_targets
        .first()
        .expect("a single-target selection has one target");
    let installer = &loaded.build.targets[0].installer;
    let scope = SelectedScope::from(args.scope);
    let state_root = zup_windows::resolve_state_root(args.state_root, scope)
        .map_err(|error| miette::miette!("{error}"))?;
    let prior = zup_windows::InstallLedgerStore::new(&state_root)
        .load(&installer.app.id, scope)
        .map_err(|error| miette::miette!("ledger: {error}"))?;
    let mut request = zup_plan::PlanRequest::new(config.target.clone(), scope);
    // `plan` plans one target, so the repeatable `--install-directory` has one
    // value in scope. More than one is a mistake the shared alignment check
    // already refuses.
    request.install_directory = choose_directory(
        args.project
            .install_directory
            .first()
            .map(std::path::PathBuf::as_path),
        prior.as_ref(),
        installer.install.allow_directory_override,
    )?;
    for raw in &args.enable {
        let id = zup_core::ComponentId::new(raw)
            .map_err(|error| miette::miette!("component {raw}: {error}"))?;
        request.components.enable.insert(id);
    }
    for raw in &args.disable {
        let id = zup_core::ComponentId::new(raw)
            .map_err(|error| miette::miette!("component {raw}: {error}"))?;
        request.components.disable.insert(id);
    }
    let install = zup_plan::plan(&loaded.build, &request).map_err(miette::Report::new)?;
    let target =
        zup_windows::resolve_target(&install, &zup_windows::WindowsTargetContext::new(scope))
            .map_err(|error| miette::miette!("target: {error}"))?;
    let transaction = zup_windows::plan_target_lifecycle(
        LifecycleAction::Install,
        &installer.app.id,
        scope,
        Some(&target),
        &state_root,
    )
    .map_err(|error| miette::miette!("transaction plan: {error}"))?;
    let mut preview = zup_presentation::PlanPreview::from_transaction_plan(&transaction, scope)
        .with_prerequisites(&install);
    preview.application = installer.app.name.to_string();
    preview.version = installer.app.version.to_string();
    preview.install_directory = target.install_directory.to_string();
    if args.json {
        let value = serde_json::json!({
            "preview": preview,
            "transaction": transaction,
            "target": target,
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&value)
                .map_err(|error| miette::miette!("output: {error}"))?
        );
    } else {
        println!("{}", preview.human());
    }
    Ok(())
}

/// The install directory a plan should use.
///
/// `--install-directory` is honoured only where the project permits one. A plan
/// that silently ignored the flag would describe an install the user cannot get.
fn choose_directory(
    explicit: Option<&Path>,
    prior: Option<&zup_exec::InstallLedger>,
    allowed: bool,
) -> miette::Result<Option<zup_core::Template>> {
    if let Some(path) = explicit {
        if !allowed {
            return Err(miette::miette!(
                "this application does not allow choosing an install directory"
            ));
        }
        return project::install_directory_template(path).map(Some);
    }
    if !allowed {
        return Ok(None);
    }
    Ok(prior
        .and_then(|ledger| ledger.install_directory.as_ref())
        .and_then(|path| zup_core::Template::parse(&path.to_string()).ok()))
}
