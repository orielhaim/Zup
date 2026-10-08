use std::collections::BTreeMap;

use zup_core::{ResourceKey, SelectedScope};
use zup_plan::InstallPlan;
use zup_platform::{
    TargetFile, TargetPath, TargetPlan, TargetPlanSummary, TargetPrerequisite,
    resolve_template_path,
};

use crate::error::{PathError, PlanError};
use crate::locations::LinuxInstallLocationResolver;
use crate::lowering::to_host_path;

pub fn resolve_target(
    plan: &InstallPlan,
    resolver: &LinuxInstallLocationResolver,
) -> Result<TargetPlan, PlanError> {
    if plan.target.operating_system() != zup_core::TargetOperatingSystem::Linux {
        return Err(PlanError::UnsupportedTarget {
            target: plan.target.to_string(),
        });
    }
    refuse_unsupported(plan)?;

    let scope = plan.scope;
    let resolve = |template: &zup_core::Template, kind: &'static str| {
        let path = resolve_template_path(template, &plan.target, resolver, scope)
            .map_err(|source| PlanError::Template { kind, source })?;
        validate_linux_path(kind, &path)?;
        Ok::<TargetPath, PlanError>(path)
    };

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum OwnedKind {
        Directory,
        File,
    }

    let mut owned: BTreeMap<String, (OwnedKind, &'static str)> = BTreeMap::new();
    let mut claim =
        |path: &TargetPath, kind: &'static str, path_kind: OwnedKind| -> Result<(), PathError> {
            let name = path.to_string();
            if let Some((_, first)) = owned.get(&name) {
                return Err(PathError::Collision {
                    kind,
                    first: (*first).to_owned(),
                    second: name,
                });
            }

            for (existing, (existing_kind, first)) in &owned {
                let conflict = match (path_kind, *existing_kind) {
                    (OwnedKind::Directory, OwnedKind::Directory) => false,
                    (OwnedKind::File, OwnedKind::Directory) => is_ancestor(&name, existing),
                    (OwnedKind::Directory, OwnedKind::File) => is_ancestor(existing, &name),
                    (OwnedKind::File, OwnedKind::File) => {
                        is_ancestor(existing, &name) || is_ancestor(&name, existing)
                    }
                };
                if conflict {
                    return Err(PathError::Collision {
                        kind,
                        first: (*first).to_owned(),
                        second: name,
                    });
                }
            }
            owned.insert(name, (path_kind, kind));
            Ok(())
        };

    let install_directory = resolve(&plan.install_directory, "install directory")?;
    claim(
        &install_directory,
        "install directory",
        OwnedKind::Directory,
    )?;

    let prerequisites = plan
        .prerequisites
        .iter()
        .map(|prerequisite| TargetPrerequisite {
            id: prerequisite.id.clone(),
            name: prerequisite.name.clone(),
            target: prerequisite.target,
            requirement: prerequisite.requirement.clone(),
            package: prerequisite.package.clone(),
            installer: prerequisite.installer.clone(),
        })
        .collect();

    let mut files = Vec::with_capacity(plan.files.len());
    let mut install_bytes = 0u64;
    for file in &plan.files {
        install_bytes = install_bytes.saturating_add(file.size);
        let destination = resolve(&file.destination, "file destination")?;
        claim(&destination, "file destination", OwnedKind::File)?;
        files.push(TargetFile {
            key: ResourceKey::File {
                destination: destination.to_string(),
            },
            source_relative: file.source_relative.clone(),
            destination,
            size: file.size,
            sha256: file.sha256,
            executable: file.executable,
            privilege: file.privilege,
        });
    }

    let mut services = Vec::with_capacity(plan.services.len());
    if scope == SelectedScope::Machine {
        let mut service_ids: BTreeMap<String, String> = BTreeMap::new();
        for service in &plan.services {
            let binary = resolve(&service.binary, "service binary")?;
            let identity = service.id.as_str().to_owned();
            if let Some(first) = service_ids.get(&identity) {
                return Err(PathError::Collision {
                    kind: "service id",
                    first: first.clone(),
                    second: identity,
                }
                .into());
            }
            service_ids.insert(identity, service.name.to_string());

            let unit =
                crate::services::unit_name(&service.id).map_err(|error| PathError::Invalid {
                    kind: "service id",
                    path: service.id.to_string(),
                    reason: error.to_string(),
                })?;

            let synthetic = zup_platform::TargetService {
                key: ResourceKey::Service {
                    id: service.id.clone(),
                },
                id: service.id.clone(),
                name: service.name.clone(),
                display_name: service.display_name.clone(),
                command: zup_platform::CommandSpec::new(binary.clone(), service.arguments.clone()),
                start: service.start,
                privilege: service.privilege,
            };
            let display = service
                .display_name
                .as_ref()
                .map(|name| name.to_string())
                .unwrap_or_else(|| service.name.to_string());
            crate::services::render_unit(&synthetic, &display).map_err(|error| {
                PathError::Invalid {
                    kind: "service command",
                    path: unit.clone(),
                    reason: error.to_string(),
                }
            })?;
            services.push(zup_platform::TargetService {
                key: ResourceKey::Service {
                    id: service.id.clone(),
                },
                id: service.id.clone(),
                name: service.name.clone(),
                display_name: service.display_name.clone(),
                command: zup_platform::CommandSpec::new(binary, service.arguments.clone()),
                start: service.start,
                privilege: service.privilege,
            });
        }
    }

    let integration = crate::integration::lower_integration(plan).map_err(|error| match error {
        PlanError::Unsupported { reasons } => PlanError::Unsupported { reasons },
        PlanError::IntegrationUnsupported(reasons) => PlanError::Unsupported { reasons },
        PlanError::Path(PathError::NoHome) => PlanError::Template {
            kind: "XDG data home",
            source: zup_platform::TemplateResolveError::InstallLocation(
                zup_platform::InstallLocationError::ResolutionFailed {
                    location: zup_core::InstallLocation::UserData,
                    scope: plan.scope,
                    source: Box::new(PathError::NoHome),
                },
            ),
        },
        other => PlanError::InvalidTargetPath {
            path: String::new(),
            reason: other.to_string(),
        },
    })?;
    for generated in &integration.files {
        install_bytes = install_bytes.saturating_add(generated.size);
        claim(
            &generated.destination,
            "integration destination",
            OwnedKind::File,
        )?;
        files.push(TargetFile {
            key: generated.key.clone(),
            source_relative: zup_core::RelativePath::new(&generated.source_relative).map_err(
                |_| PathError::Invalid {
                    kind: "integration source",
                    path: generated.source_relative.clone(),
                    reason: "generated source name is not a relative path".into(),
                },
            )?,
            destination: generated.destination.clone(),
            size: generated.size,
            sha256: generated.sha256,
            executable: false,
            privilege: zup_core::Privilege::User,
        });
    }

    let file_count = files.len();
    let resource_count = services.len();
    let target = TargetPlan {
        app: plan.app.clone(),
        target: plan.target.clone(),
        scope: plan.scope,
        install_directory,
        selected_components: plan.selected_components.clone(),
        prerequisites,
        files,
        launchers: Vec::new(),
        path_entries: Vec::new(),
        services,
        protocols: Vec::new(),
        file_associations: Vec::new(),
        summary: TargetPlanSummary {
            file_count,
            install_bytes,
            resource_count,
            requires_authorization: plan.summary.requires_authorization,
            selected_component_count: plan.selected_components.len(),
            prerequisite_count: plan.prerequisites.len(),
            download_bytes: 0,
        },

        preset: None,
    };
    validate_target_plan(&target)?;
    Ok(target)
}

fn is_ancestor(ancestor: &str, path: &str) -> bool {
    path.len() > ancestor.len()
        && path.starts_with(ancestor)
        && path.as_bytes()[ancestor.len()] == b'/'
}

fn validate_linux_path(kind: &'static str, path: &TargetPath) -> Result<(), PathError> {
    to_host_path(path).map_err(|error| {
        let (path_text, reason) = match &error {
            PathError::UnsupportedTarget { .. } => {
                (path.to_string(), "not a Linux target path".to_owned())
            }
            PathError::InvalidComponent { component, reason } => (
                path.to_string(),
                format!("component `{component}`: {reason}"),
            ),
            PathError::Path(error) => (path.to_string(), error.to_string()),
            other => (path.to_string(), other.to_string()),
        };
        PathError::Invalid {
            kind,
            path: path_text,
            reason,
        }
    })?;
    Ok(())
}

fn refuse_unsupported(plan: &InstallPlan) -> Result<(), PlanError> {
    let mut refused: Vec<String> = Vec::new();
    let mut unsupported = |kind: &str, count: usize| {
        if count > 0 {
            refused.push(format!(
                "{count} {kind} resource{} {} not supported on Linux in this phase",
                if count == 1 { "" } else { "s" },
                if count == 1 { "is" } else { "are" },
            ));
        }
    };
    if plan.scope != SelectedScope::Machine {
        unsupported("service", plan.services.len());
    }
    unsupported("package-manager prerequisite", plan.prerequisites.len());
    if plan.scope == SelectedScope::Machine {
        unsupported("launcher", plan.launchers.len());
        unsupported("PATH entry", plan.path_entries.len());
        unsupported("URI protocol", plan.protocols.len());
        unsupported("file association", plan.file_associations.len());
    }
    if refused.is_empty() {
        Ok(())
    } else {
        Err(PlanError::Unsupported {
            reasons: refused.join("; "),
        })
    }
}

pub fn validate_target_plan(plan: &TargetPlan) -> Result<(), PlanError> {
    let mut refused: Vec<String> = Vec::new();
    if plan.target.operating_system() != zup_core::TargetOperatingSystem::Linux {
        refused.push(format!(
            "target `{}` is not a Linux target",
            plan.target.as_str()
        ));
    }
    let mut unsupported = |kind: &str, count: usize| {
        if count > 0 {
            refused.push(format!(
                "{count} {kind} resource{} {} not supported on Linux in this phase",
                if count == 1 { "" } else { "s" },
                if count == 1 { "is" } else { "are" },
            ));
        }
    };
    if plan.scope != zup_core::SelectedScope::Machine {
        unsupported("service", plan.services.len());
    }
    unsupported("package-manager prerequisite", plan.prerequisites.len());
    unsupported("launcher", plan.launchers.len());
    unsupported("PATH entry", plan.path_entries.len());
    unsupported("URI protocol", plan.protocols.len());
    unsupported("file association", plan.file_associations.len());
    if refused.is_empty() {
        Ok(())
    } else {
        Err(PlanError::Unsupported {
            reasons: refused.join("; "),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zup_core::TargetTriple;

    fn resolve(input: &InstallPlan) -> Result<TargetPlan, PlanError> {
        resolve_target(input, &LinuxInstallLocationResolver::default())
    }

    fn plan() -> InstallPlan {
        let target = TargetTriple::parse("x86_64-unknown-linux-gnu").expect("a Linux target");
        InstallPlan {
            app: zup_core::App {
                id: zup_core::AppId::new("com.example.tool").expect("an id"),
                name: zup_core::NonEmptyString::new("Tool").expect("a name"),
                version: semver::Version::parse("1.0.0").expect("a version"),
                publisher: None,
                main: None,
                description: None,
            },
            target,
            scope: SelectedScope::User,
            install_directory: zup_core::Template::parse("${location.programs}/tool")
                .expect("a template"),
            selected_components: Vec::new(),
            prerequisites: Vec::new(),
            files: Vec::new(),
            launchers: Vec::new(),
            path_entries: Vec::new(),
            services: Vec::new(),
            protocols: Vec::new(),
            file_associations: Vec::new(),
            summary: zup_plan::PlanSummary {
                file_count: 0,
                install_bytes: 0,
                selected_component_count: 0,
                resource_count: 0,
                requires_authorization: false,
                prerequisite_count: 0,
                download_bytes: 0,
            },
        }
    }

    fn file(name: &str) -> zup_plan::PlannedFile {
        zup_plan::PlannedFile {
            key: ResourceKey::File {
                destination: format!("${{location.programs}}/tool/{name}"),
            },
            source_relative: zup_core::RelativePath::new(name).expect("a path"),
            destination: zup_core::Template::parse(&format!("${{location.programs}}/tool/{name}"))
                .expect("a template"),
            size: 4,
            sha256: zup_core::hash_bytes(b"data"),
            privilege: zup_core::Privilege::User,
            executable: false,
        }
    }

    #[test]
    fn files_resolve_under_the_programs_namespace() {
        let mut input = plan();
        input.files.push(file("tool"));
        let resolved = resolve(&input).expect("a file plan resolves");
        assert_eq!(resolved.files.len(), 1);
        assert!(
            resolved.files[0]
                .destination
                .to_string()
                .ends_with(".local/lib/zup/apps/tool/tool"),
            "payload lands in the programs namespace: {}",
            resolved.files[0].destination
        );
    }

    #[test]
    fn duplicate_destinations_are_refused() {
        let mut input = plan();
        input.files.push(file("tool"));
        input.files.push(file("tool"));
        assert!(
            matches!(
                resolve(&input),
                Err(PlanError::Path(PathError::Collision { .. }))
            ),
            "two files may not claim one destination"
        );
    }

    #[test]
    fn a_file_inside_another_file_is_refused() {
        let mut input = plan();
        input.files.push(file("tool"));
        let mut nested = file("tool/data");
        nested.source_relative = zup_core::RelativePath::new("data").expect("a path");
        input.files.push(nested);
        assert!(
            matches!(
                resolve(&input),
                Err(PlanError::Path(PathError::Collision { .. }))
            ),
            "a file cannot contain another file"
        );
    }

    #[test]
    fn case_distinguishes_destinations() {
        let mut input = plan();
        input.files.push(file("Tool"));
        input.files.push(file("tool"));
        assert!(
            resolve(&input).is_ok(),
            "case-sensitive filesystems hold both names"
        );
    }

    #[test]
    fn services_are_refused_with_everything_named() {
        let mut input = plan();
        input.services.push(zup_plan::PlannedService {
            key: ResourceKey::Service {
                id: zup_core::ServiceId::new("tool").expect("an id"),
            },
            id: zup_core::ServiceId::new("tool").expect("an id"),
            name: zup_core::NonEmptyString::new("Tool").expect("a name"),
            display_name: None,
            binary: zup_core::Template::parse("${location.programs}/tool/tool")
                .expect("a template"),
            arguments: Vec::new(),
            start: zup_core::ServiceStart::Automatic,
            privilege: zup_core::Privilege::System,
        });

        input.launchers.push(zup_plan::PlannedLauncher {
            key: ResourceKey::Launcher {
                location: zup_core::LauncherLocation::Menu,
                name: "tool".to_owned(),
            },
            location: zup_core::LauncherLocation::Menu,
            name: zup_core::NonEmptyString::new("Tool").expect("a name"),
            target: zup_core::Template::parse("${location.programs}/tool/tool")
                .expect("a template"),
            arguments: Vec::new(),
            working_directory: None,
            privilege: zup_core::Privilege::User,
        });
        let error = resolve(&input).expect_err("services refuse");
        let message = error.to_string();
        assert!(message.contains("service"), "{message}");
    }

    #[test]
    fn a_non_linux_plan_is_refused() {
        let mut input = plan();
        input.target = TargetTriple::parse("x86_64-pc-windows-msvc").expect("a target");
        assert!(matches!(
            resolve(&input),
            Err(PlanError::UnsupportedTarget { .. })
        ));
    }

    #[test]
    fn machine_launchers_are_refused_not_lowered() {
        let mut input = plan();
        input.scope = SelectedScope::Machine;
        input.launchers.push(zup_plan::PlannedLauncher {
            key: ResourceKey::Launcher {
                location: zup_core::LauncherLocation::Menu,
                name: "tool".to_owned(),
            },
            location: zup_core::LauncherLocation::Menu,
            name: zup_core::NonEmptyString::new("Tool").expect("a name"),
            target: zup_core::Template::parse("${location.programs}/tool/tool")
                .expect("a template"),
            arguments: Vec::new(),
            working_directory: None,
            privilege: zup_core::Privilege::System,
        });
        let error = resolve(&input).expect_err("machine launchers refuse");
        assert!(error.to_string().contains("launcher"), "{error}");
    }

    #[test]
    fn machine_files_resolve_under_opt() {
        let mut input = plan();
        input.scope = SelectedScope::Machine;
        input.files.push(file("tool"));
        let resolved = resolve(&input).expect("a machine file plan resolves");
        assert!(
            resolved.files[0]
                .destination
                .to_string()
                .starts_with("/opt/"),
            "payload lands under the program tree: {}",
            resolved.files[0].destination
        );
    }

    fn machine_service(start: zup_core::ServiceStart) -> zup_plan::PlannedService {
        zup_plan::PlannedService {
            key: ResourceKey::Service {
                id: zup_core::ServiceId::new("tool").expect("an id"),
            },
            id: zup_core::ServiceId::new("tool").expect("an id"),
            name: zup_core::NonEmptyString::new("Tool").expect("a name"),
            display_name: None,
            binary: zup_core::Template::parse("${location.programs}/tool/tool")
                .expect("a template"),
            arguments: vec!["--serve".to_owned()],
            start,
            privilege: zup_core::Privilege::System,
        }
    }

    #[test]
    fn machine_services_resolve_into_target_services() {
        let mut input = plan();
        input.scope = SelectedScope::Machine;
        input
            .services
            .push(machine_service(zup_core::ServiceStart::Automatic));
        let resolved = resolve(&input).expect("a machine service plan resolves");
        assert_eq!(resolved.services.len(), 1);
        assert_eq!(
            resolved.services[0].start,
            zup_core::ServiceStart::Automatic
        );
        assert_eq!(
            resolved.services[0].command.arguments,
            vec!["--serve".to_owned()]
        );
        assert_eq!(resolved.summary.resource_count, 1);
        let unit = crate::services::unit_name(&resolved.services[0].id).expect("a unit name");
        let again = crate::services::unit_name(&zup_core::ServiceId::new("tool").expect("an id"))
            .expect("a unit name");
        assert_eq!(unit, again, "identity is stable across resolutions");
    }

    #[test]
    fn duplicate_service_identities_collide() {
        let mut input = plan();
        input.scope = SelectedScope::Machine;
        input
            .services
            .push(machine_service(zup_core::ServiceStart::Automatic));
        input
            .services
            .push(machine_service(zup_core::ServiceStart::Manual));
        assert!(
            matches!(
                resolve(&input),
                Err(PlanError::Path(PathError::Collision { .. }))
            ),
            "one identity is one service"
        );
    }

    fn empty_plan() -> TargetPlan {
        let target = TargetTriple::parse("x86_64-unknown-linux-gnu").expect("a Linux target");
        TargetPlan {
            app: zup_core::App {
                id: zup_core::AppId::new("com.example.tool").expect("an id"),
                name: zup_core::NonEmptyString::new("Tool").expect("a name"),
                version: semver::Version::parse("1.0.0").expect("a version"),
                publisher: None,
                main: None,
                description: None,
            },
            target: target.clone(),
            scope: SelectedScope::User,
            install_directory: zup_platform::TargetPath::new(
                target,
                "/home/u/.local/lib/zup/apps/tool",
            )
            .expect("a path"),
            selected_components: Vec::new(),
            prerequisites: Vec::new(),
            files: Vec::new(),
            launchers: Vec::new(),
            path_entries: Vec::new(),
            services: Vec::new(),
            protocols: Vec::new(),
            file_associations: Vec::new(),
            summary: zup_platform::TargetPlanSummary {
                file_count: 0,
                install_bytes: 0,
                resource_count: 0,
                requires_authorization: false,
                selected_component_count: 0,
                prerequisite_count: 0,
                download_bytes: 0,
            },
            preset: None,
        }
    }

    #[rstest::rstest]
    #[case::user(SelectedScope::User)]
    #[case::machine(SelectedScope::Machine)]
    fn plain_file_plans_are_accepted(#[case] scope: SelectedScope) {
        let mut plan = empty_plan();
        plan.scope = scope;
        assert!(validate_target_plan(&plan).is_ok());
    }

    #[test]
    fn one_diagnostic_names_every_unsupported_resource() {
        let mut plan = empty_plan();
        plan.scope = SelectedScope::Machine;
        plan.services.push(target_service(&plan.target.clone()));
        plan.launchers.push(zup_platform::TargetLauncher {
            key: zup_core::ResourceKey::Launcher {
                location: zup_core::LauncherLocation::Menu,
                name: "tool".to_owned(),
            },
            location: zup_core::LauncherLocation::Menu,
            name: zup_core::NonEmptyString::new("Tool").expect("a name"),
            launcher_path: zup_platform::TargetPath::new(
                plan.target.clone(),
                "/home/u/.local/share/applications/tool.desktop",
            )
            .expect("a path"),
            target: zup_platform::TargetPath::new(
                plan.target.clone(),
                "/home/u/.local/lib/zup/apps/tool/tool",
            )
            .expect("a path"),
            arguments: Vec::new(),
            working_directory: None,
            privilege: zup_core::Privilege::User,
        });

        let error = validate_target_plan(&plan).expect_err("machine scope with launchers refuses");
        let reasons = error.reasons();
        assert!(
            reasons.contains("launcher"),
            "launchers are named: {reasons}"
        );

        plan.launchers.clear();
        assert!(validate_target_plan(&plan).is_ok());
        plan.scope = SelectedScope::User;
        let error = validate_target_plan(&plan).expect_err("user scope with services refuses");
        assert!(
            error.reasons().contains("service"),
            "services are named: {}",
            error.reasons()
        );
    }

    fn target_service(target: &zup_core::TargetTriple) -> zup_platform::TargetService {
        zup_platform::TargetService {
            key: zup_core::ResourceKey::Service {
                id: zup_core::ServiceId::new("tool").expect("an id"),
            },
            id: zup_core::ServiceId::new("tool").expect("an id"),
            name: zup_core::NonEmptyString::new("Tool").expect("a name"),
            display_name: None,
            command: zup_platform::CommandSpec::new(
                zup_platform::TargetPath::new(target.clone(), "/opt/tool/tool").expect("a path"),
                Vec::new(),
            ),
            start: zup_core::ServiceStart::Automatic,
            privilege: zup_core::Privilege::System,
        }
    }

    #[test]
    fn a_non_linux_target_is_refused() {
        let mut plan = empty_plan();
        plan.target = zup_core::TargetTriple::parse("x86_64-pc-windows-msvc").expect("a target");
        let error = validate_target_plan(&plan).expect_err("a Windows target refuses");
        assert!(
            error.reasons().contains("not a Linux target"),
            "{}",
            error.reasons()
        );
    }
}
