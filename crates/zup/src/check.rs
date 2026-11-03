use std::path::Path;

use zup_automation::{
    AutomationResult, CheckDetails, Composition, Details, Diagnostic, Identifier, LogLevel,
};
use zup_automation::{ByteCount, PlanDetails};
use zup_core::SelectedScope;

use crate::cli::{CheckCommand, PlanCommand};
use crate::failure::Reporter;
use crate::project::{self, LoadedProject};

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
        zup_build::Writes::None,
    )?;
    let mut variants = Vec::with_capacity(loaded.selected_targets.len());
    let mut files = 0usize;
    for (config, plan) in loaded.selected_targets.iter().zip(&loaded.build.targets) {
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
    if borrowed.iter().any(|variant| {
        !zup_plan::capabilities(variant.target())
            .is_some_and(|caps| caps.artifacts == zup_plan::ArtifactSupport::Universal)
    }) {
        let linux = loaded
            .selected_targets
            .iter()
            .filter(|config| {
                config.target.operating_system() == zup_core::TargetOperatingSystem::Linux
            })
            .map(|config| config.profile.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        return Some(Composition {
            composable: false,
            dimension: Some(Identifier::fixed("platform")),
            detail: format!(
                "{linux} ship one self-contained Linux installer each and cannot be composed \
                 into a dispatcher artifact: build them separately with `zup build --target \
                 <profile>`"
            ),
        });
    }
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

pub fn run_plan(
    args: PlanCommand,
    toolchain_root: Option<std::path::PathBuf>,
) -> miette::Result<AutomationResult> {
    let reporter = Reporter::new(args.format);
    let selected = project::select_project(
        &args.project.manifest,
        &args.project.target,
        &args.project.overrides(),
        true,
    )?;
    let loaded = project::materialize_project(
        selected,
        &crate::resolver(toolchain_root)?,
        zup_build::Writes::None,
    )?;
    plan_installation(args, &reporter, &loaded)
}

fn plan_installation(
    args: PlanCommand,
    reporter: &Reporter,
    loaded: &LoadedProject,
) -> miette::Result<AutomationResult> {
    let config = &loaded.selected_targets[0];
    let installer = &loaded.build.targets[0].installer;
    let scope = SelectedScope::from(args.scope);
    let mut request = zup_plan::PlanRequest::new(config.target, scope);
    request.install_directory = choose_directory(
        args.project
            .install_directory
            .first()
            .map(std::path::PathBuf::as_path),
        installer.install.allow_directory_override,
    )?
    .map(zup_plan::InstallDirectory::Override);
    for (raw_ids, selection) in [
        (&args.enable, &mut request.components.enable),
        (&args.disable, &mut request.components.disable),
    ] {
        for raw in raw_ids {
            selection.insert(zup_core::ComponentId::new(raw).map_err(|error| {
                crate::failure::error("zup.plan.component", format!("component {raw}: {error}"))
            })?);
        }
    }
    let planned =
        zup_plugin_build::plan_installation(&loaded.build, &request, &zup_plan::NeverCancelled)
            .map_err(|error| crate::failure::error("zup.plan.refused", error.to_string()))?;
    let target = match config.target.operating_system() {
        zup_core::TargetOperatingSystem::Windows => zup_windows::resolve_target(
            &planned.plan,
            &zup_windows::WindowsTargetContext::for_build(scope),
        )
        .map_err(|error| crate::failure::error("zup.plan.target", error.to_string()))?,
        zup_core::TargetOperatingSystem::Linux => zup_linux::resolve_target(
            &planned.plan,
            &zup_linux::LinuxInstallLocationResolver::for_build(),
        )
        .map_err(|error| crate::failure::error("zup.plan.target", error.to_string()))?,
        _ => {
            return Err(crate::failure::error(
                "zup.plan.target",
                "unsupported target",
            ));
        }
    };
    let summary = format!(
        "Simulated deployment of {} {} into {} (machine state not inspected)",
        installer.app.name, installer.app.version, target.install_directory
    );
    reporter.log(LogLevel::Info, &summary);
    use zup_automation::PlanResource;
    let mut resources = Vec::new();
    for file in &target.files {
        reporter.log(
            LogLevel::Info,
            format!("  File {} ({} bytes)", file.destination, file.size),
        );
        resources.push(PlanResource::File {
            destination: file.destination.to_string(),
            size: ByteCount::new(file.size),
            digest: zup_automation::Digest::sha256(file.sha256.to_hex()),
            generated: !loaded.build.targets[0]
                .files
                .iter()
                .any(|input| input.source_relative == file.source_relative),
        });
    }
    for launcher in &target.launchers {
        resources.push(PlanResource::Launcher {
            name: launcher.name.to_string(),
            location: format!("{:?}", launcher.location),
            executable: launcher.target.to_string(),
            arguments: launcher.arguments.clone(),
            working_directory: launcher.working_directory.as_ref().map(ToString::to_string),
        });
    }
    for entry in &target.path_entries {
        resources.push(PlanResource::PathEntry {
            value: entry.value.to_string(),
        });
    }
    for service in &target.services {
        resources.push(PlanResource::Service {
            id: service.id.to_string(),
            executable: service.command.executable.to_string(),
            arguments: service.command.arguments.clone(),
            start: format!("{:?}", service.start),
        });
    }
    for protocol in &target.protocols {
        resources.push(PlanResource::Protocol {
            scheme: protocol.scheme.to_string(),
            executable: protocol.command.executable.to_string(),
            arguments: protocol.command.arguments.clone(),
        });
    }
    for association in &target.file_associations {
        resources.push(PlanResource::FileAssociation {
            id: association.id.to_string(),
            extension: association.extension.to_string(),
            executable: association.command.executable.to_string(),
            arguments: association.command.arguments.clone(),
        });
    }
    for prerequisite in &target.prerequisites {
        resources.push(PlanResource::Prerequisite {
            id: prerequisite.id.to_string(),
            name: prerequisite.name.to_string(),
        });
    }
    reporter.log(
        LogLevel::Info,
        zup_presentation::PlanPreview::from_desired_state(&target)
            .map_err(|error| crate::failure::error("zup.plan.contract", error.to_string()))?
            .human(),
    );
    Ok(AutomationResult::new(zup_automation::OPERATION_PLAN)
        .with_application(crate::automation::application(&installer.app))
        .with_targets(crate::automation::targets(&loaded.selected_targets))
        .with_details(Details::Plan(PlanDetails {
            machine_state_observed: false,
            scope: scope.to_string(),
            install_directory: Some(target.install_directory.to_string()),
            estimated_bytes: ByteCount::new(
                target
                    .summary()
                    .map_err(|error| crate::failure::error("zup.plan.contract", error.to_string()))?
                    .install_bytes,
            ),
            change_count: resources.len(),
            resources,
        }))
        .with_summary(summary))
}

fn choose_directory(
    explicit: Option<&Path>,
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
    Ok(None)
}
