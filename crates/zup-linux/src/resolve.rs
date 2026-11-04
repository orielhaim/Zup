use std::collections::BTreeMap;

use zup_core::{ResourceKey, SelectedScope};
use zup_plan::{DesiredFile, DesiredState, TargetPath, resolve_files, resolve_template_path};

use crate::error::{PathError, PlanError};
use crate::locations::LinuxInstallLocationResolver;
use crate::lowering::to_host_path;

pub fn resolve_target(
    plan: &DesiredState,
    resolver: &LinuxInstallLocationResolver,
) -> Result<DesiredState<zup_plan::TargetPath, zup_plan::TargetPath>, PlanError> {
    if plan.target.operating_system() != zup_core::TargetOperatingSystem::Linux {
        return Err(PlanError::UnsupportedTarget {
            target: plan.target.to_string(),
        });
    }
    plan.validate()?;

    let scope = plan.scope;
    let resolve = |template: &zup_core::Template, kind: &'static str| {
        let path = resolve_template_path(template, &plan.target, resolver, scope)
            .map_err(|source| PlanError::Template { kind, source })?;
        validate_linux_path(kind, &path)?;
        Ok::<TargetPath, PlanError>(path)
    };

    let install_directory = resolve(&plan.install_directory, "install directory")?;

    let mut files = resolve_files(plan, resolve)?;

    let mut services = Vec::with_capacity(plan.services.len());
    if scope == SelectedScope::Machine {
        let mut service_ids: BTreeMap<String, String> = BTreeMap::new();
        for service in &plan.services {
            let binary = resolve(&service.command.executable, "service binary")?;
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

            let synthetic = zup_plan::DesiredService {
                key: ResourceKey::Service {
                    id: service.id.clone(),
                },
                id: service.id.clone(),
                name: service.name.clone(),
                display_name: service.display_name.clone(),
                command: zup_plan::CommandSpec::new(
                    binary.clone(),
                    service.command.arguments.clone(),
                ),
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
            services.push(zup_plan::DesiredService {
                key: ResourceKey::Service {
                    id: service.id.clone(),
                },
                id: service.id.clone(),
                name: service.name.clone(),
                display_name: service.display_name.clone(),
                command: zup_plan::CommandSpec::new(binary, service.command.arguments.clone()),
                start: service.start,
                privilege: service.privilege,
            });
        }
    }

    let data_home = resolver.data_home().map_err(|source| PlanError::Template {
        kind: "XDG data home",
        source: zup_plan::TemplateResolveError::InstallLocation(
            zup_plan::InstallLocationError::ResolutionFailed {
                location: zup_core::InstallLocation::UserData,
                scope: plan.scope,
                source: Box::new(source),
            },
        ),
    })?;
    let integration = crate::integration::lower_integration_with_resolver(
        plan, &data_home, resolver,
    )
    .map_err(|error| match error {
        PlanError::Unsupported { reasons } => PlanError::Unsupported { reasons },
        PlanError::IntegrationUnsupported(reasons) => PlanError::Unsupported { reasons },
        other => PlanError::InvalidTargetPath {
            path: String::new(),
            reason: other.to_string(),
        },
    })?;
    for generated in &integration.files {
        files.push(DesiredFile {
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
            executable: generated.executable,
            privilege: match scope {
                SelectedScope::User => zup_core::Privilege::User,
                SelectedScope::Machine => zup_core::Privilege::System,
            },
        });
    }

    let prerequisites = plan.prerequisites.clone();
    let target = DesiredState {
        app: plan.app.clone(),
        target: plan.target,
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

        preset: None,
    };
    validate_target_plan(&target)?;
    Ok(target)
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

pub fn validate_target_plan(
    plan: &DesiredState<zup_plan::TargetPath, zup_plan::TargetPath>,
) -> Result<(), PlanError> {
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

    unsupported("launcher", plan.launchers.len());
    unsupported("PATH entry", plan.path_entries.len());
    unsupported("URI protocol", plan.protocols.len());
    unsupported("file association", plan.file_associations.len());
    if refused.is_empty() {
        plan.validate_resolved()?;
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
    use zup_core::Target;

    fn resolve(
        input: &DesiredState,
    ) -> Result<DesiredState<zup_plan::TargetPath, zup_plan::TargetPath>, PlanError> {
        resolve_target(input, &LinuxInstallLocationResolver::for_build())
    }

    fn plan() -> DesiredState {
        let target = Target::parse("linux-x64").expect("a Linux target");
        DesiredState {
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

            preset: None,
        }
    }

    fn file(name: &str) -> zup_plan::DesiredFile {
        zup_plan::DesiredFile {
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
                Err(PlanError::Contract(
                    zup_plan::ContractError::Duplicate(_)
                        | zup_plan::ContractError::PathCollision { .. }
                ))
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
                Err(PlanError::Contract(
                    zup_plan::ContractError::Duplicate(_)
                        | zup_plan::ContractError::PathCollision { .. }
                ))
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
        input.services.push(zup_plan::DesiredService {
            key: ResourceKey::Service {
                id: zup_core::ServiceId::new("tool").expect("an id"),
            },
            id: zup_core::ServiceId::new("tool").expect("an id"),
            name: zup_core::NonEmptyString::new("Tool").expect("a name"),
            display_name: None,
            start: zup_core::ServiceStart::Automatic,
            privilege: zup_core::Privilege::System,
            command: zup_plan::CommandSpec::new(
                zup_core::Template::parse("${location.programs}/tool/tool").expect("a template"),
                Vec::new(),
            ),
        });

        input.launchers.push(zup_plan::DesiredLauncher {
            key: ResourceKey::Launcher {
                location: zup_core::LauncherLocation::Menu,
                name: "Tool".to_owned(),
            },
            location: zup_core::LauncherLocation::Menu,
            name: zup_core::NonEmptyString::new("Tool").expect("a name"),
            target: zup_core::Template::parse("${location.programs}/tool/tool")
                .expect("a template"),
            arguments: Vec::new(),
            working_directory: None,
            privilege: zup_core::Privilege::User,

            launcher_path: (),
        });
        let error = resolve(&input).expect_err("services refuse");
        let message = error.to_string();
        assert!(message.contains("service"), "{message}");
    }

    #[test]
    fn a_non_linux_plan_is_refused() {
        let mut input = plan();
        input.target = Target::parse("windows-x64").expect("a target");
        assert!(matches!(
            resolve(&input),
            Err(PlanError::UnsupportedTarget { .. })
        ));
    }

    #[test]
    fn machine_launchers_are_refused_not_lowered() {
        let mut input = plan();
        input.scope = SelectedScope::Machine;
        input.launchers.push(zup_plan::DesiredLauncher {
            key: ResourceKey::Launcher {
                location: zup_core::LauncherLocation::Menu,
                name: "Tool".to_owned(),
            },
            location: zup_core::LauncherLocation::Menu,
            name: zup_core::NonEmptyString::new("Tool").expect("a name"),
            target: zup_core::Template::parse("${location.programs}/tool/tool")
                .expect("a template"),
            arguments: Vec::new(),
            working_directory: None,
            privilege: zup_core::Privilege::System,

            launcher_path: (),
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

    fn machine_service(start: zup_core::ServiceStart) -> zup_plan::DesiredService {
        zup_plan::DesiredService {
            key: ResourceKey::Service {
                id: zup_core::ServiceId::new("tool").expect("an id"),
            },
            id: zup_core::ServiceId::new("tool").expect("an id"),
            name: zup_core::NonEmptyString::new("Tool").expect("a name"),
            display_name: None,
            start,
            privilege: zup_core::Privilege::System,
            command: zup_plan::CommandSpec::new(
                zup_core::Template::parse("${location.programs}/tool/tool").expect("a template"),
                vec!["--serve".to_owned()],
            ),
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
        assert_eq!(resolved.summary().unwrap().resource_count, 1);
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
                Err(PlanError::Contract(
                    zup_plan::ContractError::Duplicate(_)
                        | zup_plan::ContractError::PathCollision { .. }
                ))
            ),
            "one identity is one service"
        );
    }

    fn empty_plan() -> DesiredState<zup_plan::TargetPath, zup_plan::TargetPath> {
        let target = Target::parse("linux-x64").expect("a Linux target");
        DesiredState {
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
            install_directory: zup_plan::TargetPath::new(
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
        plan.launchers.push(zup_plan::DesiredLauncher {
            key: zup_core::ResourceKey::Launcher {
                location: zup_core::LauncherLocation::Menu,
                name: "Tool".to_owned(),
            },
            location: zup_core::LauncherLocation::Menu,
            name: zup_core::NonEmptyString::new("Tool").expect("a name"),
            launcher_path: zup_plan::TargetPath::new(
                plan.target,
                "/home/u/.local/share/applications/tool.desktop",
            )
            .expect("a path"),
            target: zup_plan::TargetPath::new(plan.target, "/home/u/.local/lib/zup/apps/tool/tool")
                .expect("a path"),
            arguments: Vec::new(),
            working_directory: None,
            privilege: zup_core::Privilege::User,
        });

        let error = validate_target_plan(&plan).expect_err("machine scope with launchers refuses");
        let reasons = error.to_string();
        assert!(
            reasons.contains("launcher"),
            "launchers are named: {reasons}"
        );

        plan.launchers.clear();
        assert!(validate_target_plan(&plan).is_ok());
        plan.scope = SelectedScope::User;
        let error = validate_target_plan(&plan).expect_err("user scope with services refuses");
        assert!(
            error.to_string().contains("service"),
            "services are named: {}",
            error
        );
    }

    fn target_service(target: &zup_core::Target) -> zup_plan::DesiredService<zup_plan::TargetPath> {
        zup_plan::DesiredService {
            key: zup_core::ResourceKey::Service {
                id: zup_core::ServiceId::new("tool").expect("an id"),
            },
            id: zup_core::ServiceId::new("tool").expect("an id"),
            name: zup_core::NonEmptyString::new("Tool").expect("a name"),
            display_name: None,
            command: zup_plan::CommandSpec::new(
                zup_plan::TargetPath::new(*target, "/opt/tool/tool").expect("a path"),
                Vec::new(),
            ),
            start: zup_core::ServiceStart::Automatic,
            privilege: zup_core::Privilege::System,
        }
    }

    #[test]
    fn a_non_linux_target_is_refused() {
        let mut plan = empty_plan();
        plan.target = zup_core::Target::parse("windows-x64").expect("a target");
        let error = validate_target_plan(&plan).expect_err("a Windows target refuses");
        assert!(
            error.to_string().contains("not a Linux target"),
            "{}",
            error.to_string()
        );
    }
}
