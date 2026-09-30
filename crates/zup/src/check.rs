//! `zup check` and `zup plan`: what the project says, and what it would do.
//!
//! Neither command changes anything. `check` answers "is this project coherent";
//! `plan` answers "what would installing it do on this machine". They share the
//! selection and materialization path with `build` on purpose: a check that
//! re-implements the build's own traversal is a check that agrees with the build
//! until the day it does not.

use std::path::Path;

use zup_automation::{
    AutomationResult, ByteCount, CheckDetails, Composition, Details, Diagnostic, Identifier,
    LogLevel, PlanDetails,
};
use zup_core::SelectedScope;
use zup_exec::LifecycleAction;

use crate::cli::{CheckCommand, PlanCommand};
use crate::project::{self, LoadedProject};
use crate::report::Reporter;

/// Validate a project and the files it will ship.
pub fn run_check(
    args: CheckCommand,
    toolchain_root: Option<std::path::PathBuf>,
) -> miette::Result<AutomationResult> {
    let reporter = Reporter::new(args.format);
    let loaded = project::load_for_build(
        &args.project.manifest,
        &args.project.target,
        &args.project.overrides(),
        &crate::resolver(toolchain_root)?,
    )?;
    let mut variants = Vec::with_capacity(loaded.selected_targets.len());
    let mut files = 0usize;
    for (config, plan) in loaded.selected_targets.iter().zip(&loaded.build.targets) {
        // A check that stopped at the manifest would report a project with a
        // broken plugin as valid, which is the one thing a check must never do.
        // The plugin compile is in memory and writes nothing.
        if !plan.installer.plugins.is_empty() {
            zup_plugin_build::compile_plugins(plan).map_err(|error| {
                crate::failure::error_with_help(
                    "zup.check.plugin_compile_failed",
                    format!("plugin check for `{}`: {error}", config.profile),
                    "Fix the plugin source, or remove it from the profile.",
                )
            })?;
        }
        reporter.log(
            LogLevel::Info,
            format!(
                "✓ {} is valid ({})\n  Target      {}\n  Source      {}\n  Install     {} · {}\n  \
                 Components  {}\n  Files       {}\n  Plugins     {}",
                plan.installer.app.name,
                config.profile,
                config.target,
                config.source.directory.display(),
                config.install.scope,
                project::install_directory_text(&config.install),
                plan.installer.components.len(),
                plan.files.len(),
                plan.installer.plugins.len(),
            ),
        );
        files += plan.files.len();
        variants.push(
            zup_artifact::DistributionVariant::resolve(config, plan, &[], &[]).map_err(
                |error| {
                    crate::failure::error(
                        "zup.check.variant_invalid",
                        format!("variant `{}`: {error}", config.profile),
                    )
                },
            )?,
        );
    }
    let composition = composition(&loaded, &variants);
    if let Some(composition) = &composition {
        reporter.log(LogLevel::Info, composition_text(composition));
    }
    // A warning, not an error: the project is valid and `zup check` succeeds. What
    // cannot be composed is a fact about how the release has to be built, and turning
    // it into an error would make a green project read as a broken one.
    let diagnostics = match &composition {
        Some(composition) if !composition.composable => vec![
            Diagnostic::warning("zup.check.not_composable", composition.detail.clone())
                .with_help("Build them separately with `zup build --target <profile>`."),
        ],
        _ => Vec::new(),
    };
    let result = AutomationResult::new(zup_automation::OPERATION_CHECK)
        .with_application(crate::automation::application(&loaded.manifest.app))
        .with_targets(crate::automation::targets(&loaded.selected_targets))
        .with_diagnostics(diagnostics)
        .with_details(Details::Check(CheckDetails {
            variants: variants.len(),
            composition,
        }))
        .with_summary(format!(
            "{} is valid · {} file(s) across {} target(s)",
            loaded.manifest.app.name,
            files,
            variants.len()
        ));
    Ok(result)
}

/// Whether the selected targets can become one artifact.
///
/// `None` when fewer than two were selected: that is not a finding, it is the ordinary
/// case, and a report that said "cannot be composed" about one target would be
/// reporting a problem nobody has.
fn composition(
    loaded: &LoadedProject,
    variants: &[zup_artifact::DistributionVariant],
) -> Option<Composition> {
    if variants.len() < 2 {
        return None;
    }
    let borrowed = variants.iter().collect::<Vec<_>>();
    let names = loaded
        .selected_targets
        .iter()
        .map(|config| config.profile.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    // Composition is refused loudly rather than suggested quietly, because a project
    // that silently ships two installers where one would do has a problem nobody was
    // told about.
    Some(match zup_artifact::check_compatibility(&borrowed) {
        Err(incompatible) => Composition {
            composable: false,
            dimension: Some(Identifier::fixed(incompatible.reason.dimension().as_str())),
            detail: format!(
                "{names} cannot be composed: {}",
                incompatible.reason.detail()
            ),
        },
        Ok(()) => match crate::artifacts::composition_report(&borrowed) {
            Ok(savings) => Composition {
                composable: true,
                dimension: None,
                detail: format!(
                    "one Windows universal installer · {} shared of {} standalone across {} \
                     blobs",
                    zup_presentation::format_bytes(savings.shared_size),
                    zup_presentation::format_bytes(savings.standalone_size),
                    savings.unique_blob_count
                ),
            },
            Err(refusal) => Composition {
                composable: false,
                dimension: Some(Identifier::fixed(refusal.dimension.as_str())),
                detail: refusal.message.clone(),
            },
        },
    })
}

/// What a person reads for a composition finding.
fn composition_text(composition: &Composition) -> String {
    if composition.composable {
        return format!(
            "\n✓ Can be composed as one artifact\n  {}",
            composition.detail
        );
    }
    let dimension = composition
        .dimension
        .as_ref()
        .map(ToString::to_string)
        .unwrap_or_default();
    format!("\n✗ {}\n  {}", composition.detail, dimension)
}

/// Show what installing this project would do, without changing anything.
pub fn run_plan(
    args: PlanCommand,
    toolchain_root: Option<std::path::PathBuf>,
) -> miette::Result<AutomationResult> {
    let reporter = Reporter::new(args.format);
    let loaded = project::load_single_project(
        &args.project.manifest,
        &args.project.target,
        &args.project.overrides(),
        &crate::resolver(toolchain_root)?,
    )?;
    let config = loaded
        .selected_targets
        .first()
        .expect("a single-target selection has one target");
    let installer = &loaded.build.targets[0].installer;
    let scope = SelectedScope::from(args.scope);
    let state_root = zup_windows::resolve_state_root(args.state_root, scope)
        .map_err(|error| crate::failure::error("zup.plan.state_root", format!("{error}")))?;
    let prior = zup_windows::InstallLedgerStore::new(&state_root)
        .load(&installer.app.id, scope)
        .map_err(|error| crate::failure::error("zup.plan.ledger", format!("ledger: {error}")))?;
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
        let id = zup_core::ComponentId::new(raw).map_err(|error| {
            crate::failure::error("zup.plan.component", format!("component {raw}: {error}"))
        })?;
        request.components.enable.insert(id);
    }
    for raw in &args.disable {
        let id = zup_core::ComponentId::new(raw).map_err(|error| {
            crate::failure::error("zup.plan.component", format!("component {raw}: {error}"))
        })?;
        request.components.disable.insert(id);
    }
    let install = zup_plan::plan(&loaded.build, &request)
        .map_err(|error| crate::failure::error("zup.plan.refused", error.to_string()))?;
    let target =
        zup_windows::resolve_target(&install, &zup_windows::WindowsTargetContext::new(scope))
            .map_err(|error| {
                crate::failure::error("zup.plan.target", format!("target: {error}"))
            })?;
    let transaction = zup_windows::plan_target_lifecycle(
        LifecycleAction::Install,
        &installer.app.id,
        scope,
        Some(&target),
        &state_root,
    )
    .map_err(|error| {
        crate::failure::error("zup.plan.transaction", format!("transaction plan: {error}"))
    })?;
    let mut preview = zup_presentation::PlanPreview::from_transaction_plan(&transaction, scope)
        .with_prerequisites(&install);
    preview.application = installer.app.name.to_string();
    preview.version = installer.app.version.to_string();
    preview.install_directory = target.install_directory.to_string();
    reporter.log(LogLevel::Info, preview.human());
    let changes = preview
        .groups
        .iter()
        .flat_map(|group| group.changes.iter())
        .count();
    Ok(AutomationResult::new(zup_automation::OPERATION_PLAN)
        .with_application(crate::automation::application(&installer.app))
        .with_targets(crate::automation::targets(&loaded.selected_targets))
        .with_details(Details::Plan(PlanDetails {
            scope: scope.to_string(),
            install_directory: Some(target.install_directory.to_string()),
            estimated_bytes: ByteCount::new(preview.estimated_bytes),
            change_count: changes,
        }))
        .with_summary(format!(
            "Install {} {} into {}",
            installer.app.name, installer.app.version, target.install_directory
        )))
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
