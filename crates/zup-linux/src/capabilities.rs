//! Target capability rules for the Linux backend, evaluated on any host.
//!
//! Build tooling calls these before artifact composition; the installer calls
//! them again through operation preparation when it runs.

use zup_core::{Frontend, ResolvedTargetConfig, SelectedScope, Target, TargetBuildPlan};

fn is_linux_target(target: &Target) -> bool {
    target.operating_system() == zup_core::TargetOperatingSystem::Linux
}

pub fn is_supported_linux_target(target: &Target) -> bool {
    is_linux_target(target) && zup_platform::capabilities(target).is_some()
}

pub fn linux_selection_errors(target: &Target, frontend: Frontend) -> Vec<String> {
    let mut errors = linux_target_errors(target);
    if is_linux_target(target) {
        errors.extend(linux_frontend_errors(target, frontend));
    }
    errors
}

fn linux_target_errors(target: &Target) -> Vec<String> {
    if is_supported_linux_target(target) {
        return Vec::new();
    }
    vec![if is_linux_target(target) {
        format!(
            "unsupported backend for target `{target}`: this Zup version builds Linux \
             installers for `linux-x64` and `linux-arm64`"
        )
    } else {
        format!("unsupported backend for target `{target}`: not a Linux target")
    }]
}

fn linux_frontend_errors(target: &Target, frontend: Frontend) -> Vec<String> {
    if zup_platform::capabilities(target).is_some_and(|caps| caps.frontends.contains(&frontend)) {
        return Vec::new();
    }
    vec![format!(
        "frontend `{frontend}` is not supported for `{target}`"
    )]
}

pub fn linux_capability_errors(
    config: &ResolvedTargetConfig,
    plan: &TargetBuildPlan,
) -> Vec<String> {
    let mut errors = Vec::new();
    let target = &config.target;
    errors.extend(linux_selection_errors(target, config.frontend));
    errors.extend(resource_errors(
        &plan.installer,
        config.install.scope.scopes(),
        config.profile.as_str(),
        plan,
    ));
    errors
}

pub fn operation_capability_errors(build: &TargetBuildPlan, scope: SelectedScope) -> Vec<String> {
    let installer = &build.installer;
    let mut errors = Vec::new();
    errors.extend(linux_selection_errors(
        &installer.target,
        installer.frontend,
    ));
    errors.extend(resource_errors(
        installer,
        &[scope],
        installer.app.name.as_str(),
        build,
    ));

    errors
}

fn resource_errors(
    installer: &zup_core::Installer,
    scopes: &[SelectedScope],
    name: &str,
    plan: &TargetBuildPlan,
) -> Vec<String> {
    let mut errors = Vec::new();
    if let Some(capabilities) = zup_platform::capabilities(&installer.target) {
        for &scope in scopes {
            errors.extend(capabilities.resource_errors(installer, scope));
        }
    }
    errors.extend(protocol_errors(&installer.protocols));
    errors.extend(association_errors(&installer.file_associations));
    errors.extend(main_executable_errors(name, plan));
    errors
}

fn protocol_errors(protocols: &[zup_core::Protocol]) -> Vec<String> {
    let mut errors = Vec::new();
    for protocol in protocols {
        if protocol.uri_placeholder_count() != 1 {
            errors.push(format!(
                "protocol `{}` is not supported by the Linux backend yet: handler arguments \
                 must carry exactly one `%1` URI placeholder (found {}), because Linux delivers \
                 the URI through `%u`",
                protocol.scheme,
                protocol.uri_placeholder_count(),
            ));
        }
    }
    if let Some(first) = protocols.first()
        && protocols
            .iter()
            .any(|protocol| protocol.executable != first.executable || protocol.args != first.args)
    {
        errors.push(
            "protocols with different handler commands are not supported by the Linux backend \
             yet: one hidden desktop entry dispatches every scheme"
                .to_owned(),
        );
    }
    errors
}

fn association_errors(associations: &[zup_core::FileAssociation]) -> Vec<String> {
    if let Some(first) = associations.first()
        && associations
            .iter()
            .any(|association| association.executable != first.executable)
    {
        return vec![
            "file associations with different executables are not supported by the Linux backend \
             yet: one hidden desktop entry opens every associated type"
                .to_owned(),
        ];
    }
    Vec::new()
}

fn main_executable_errors(name: &str, plan: &TargetBuildPlan) -> Vec<String> {
    if plan.files.is_empty() {
        return Vec::new();
    }
    let Some(main) = &plan.installer.app.main else {
        return Vec::new();
    };
    let main = main.to_string();
    let matches = plan
        .files
        .iter()
        .filter(|file| file.destination.to_string() == main)
        .collect::<Vec<_>>();
    if matches.is_empty() {
        return vec![format!(
            "the declared application main `{main}` for `{name}` matches no shipped file: name a \
             `[[files]]` destination, or remove `app.main`",
        )];
    }
    if matches.iter().all(|file| !file.executable) {
        return vec![format!(
            "the declared application main `{main}` for `{name}` is not executable: mark the shipped \
             file `executable = true`",
        )];
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use zup_core::InstallScope;

    fn config(target: &str, frontend: Frontend, scope: InstallScope) -> ResolvedTargetConfig {
        ResolvedTargetConfig {
            profile: zup_core::TargetProfileId::new("linux").unwrap(),
            target: Target::parse(target).unwrap(),
            source: zup_core::Source::new(std::path::PathBuf::from("dist")).unwrap(),
            frontend,
            install: zup_core::Install {
                scope,
                directory: zup_core::InstallDirectory {
                    user: None,
                    machine: None,
                },
                allow_directory_override: false,
            },
        }
    }

    fn plan() -> TargetBuildPlan {
        TargetBuildPlan {
            installer: zup_core::Installer {
                preset: None,
                app: zup_core::App {
                    id: zup_core::AppId::new("com.example.tool").unwrap(),
                    name: zup_core::NonEmptyString::new("Tool").unwrap(),
                    version: semver::Version::parse("1.0.0").unwrap(),
                    publisher: None,
                    main: None,
                    description: None,
                },
                target: Target::parse("linux-x64").unwrap(),
                frontend: Frontend::Console,
                updates: None,
                install: zup_core::Install {
                    scope: InstallScope::User,
                    directory: zup_core::InstallDirectory {
                        user: None,
                        machine: None,
                    },
                    allow_directory_override: false,
                },
                prerequisites: Vec::new(),
                components: Vec::new(),
                component_groups: Vec::new(),
                plugins: Vec::new(),
                files: Vec::new(),
                launchers: Vec::new(),
                path: Vec::new(),
                services: Vec::new(),
                protocols: Vec::new(),
                file_associations: Vec::new(),
            },
            prerequisites: Vec::new(),
            plugins: Vec::new(),
            total_size: 0,
            prerequisite_size: 0,
            icons: zup_core::TargetIcons::default(),
            files: Vec::new(),
            ui_assets: Vec::new(),
        }
    }

    #[test]
    fn a_plain_console_user_plan_is_accepted() {
        let errors = linux_capability_errors(
            &config("linux-x64", Frontend::Console, InstallScope::User),
            &plan(),
        );
        assert!(errors.is_empty(), "{errors:?}");
        let errors = linux_capability_errors(
            &config("linux-x64", Frontend::Headless, InstallScope::User),
            &plan(),
        );
        assert!(errors.is_empty(), "{errors:?}");
    }

    #[test]
    fn a_gui_plan_with_a_preset_is_accepted() {
        let config = config("linux-x64", Frontend::Gui, InstallScope::User);
        let mut plan = plan();
        plan.installer.frontend = Frontend::Gui;
        plan.installer.preset = Some(zup_core::PresetRuntime {
            name: zup_core::NonEmptyString::new("aurora").unwrap(),
            version: semver::Version::parse("1.4.2").unwrap(),
            protocol: zup_preset_protocol::PRESET_PROTOCOL_VERSION,
            required_capabilities: Vec::new(),
            settings: serde_json::json!({}),
            assets: Vec::new(),
        });
        let errors = linux_capability_errors(&config, &plan);
        assert!(errors.is_empty(), "{errors:?}");
        let errors = operation_capability_errors(&plan, SelectedScope::User);
        assert!(errors.is_empty(), "{errors:?}");
    }

    #[test]
    fn protocols_without_one_placeholder_are_refused() {
        let config = config("linux-x64", Frontend::Console, InstallScope::User);
        let mut plan = plan();
        plan.installer.protocols.push(zup_core::Protocol {
            scheme: zup_core::ProtocolScheme::new("acme").unwrap(),
            executable: zup_core::Template::parse("${location.programs}/tool/tool").unwrap(),
            args: vec!["--serve".into()],
            when: None,
        });
        let errors = linux_capability_errors(&config, &plan);
        let text = errors.join("\n");
        assert!(text.contains("%1"), "{text}");
    }

    #[test]
    fn desktop_launchers_and_path_entries_stay_refused() {
        let config = config("linux-x64", Frontend::Console, InstallScope::User);
        let mut plan = plan();
        plan.installer.launchers.push(zup_core::Launcher {
            location: zup_core::LauncherLocation::Desktop,
            name: zup_core::NonEmptyString::new("Tool").unwrap(),
            target: zup_core::Template::parse("${location.programs}/tool/tool").unwrap(),
            arguments: Vec::new(),
            working_directory: None,
            component: None,
            when: None,
        });
        plan.installer.path.push(zup_core::PathEntry {
            value: zup_core::Template::parse("${location.programs}/tool").unwrap(),
            component: None,
            when: None,
        });
        let errors = linux_capability_errors(&config, &plan);
        let text = errors.join("\n");
        assert!(text.contains("desktop launcher"), "{text}");
        assert!(text.contains("PATH entry"), "{text}");
        assert!(
            !text.contains("zup-linux"),
            "diagnostics name the configuration, never an internal crate: {text}"
        );
    }
    #[test]
    fn every_unsupported_dimension_is_named() {
        let config = config("linux-x64", Frontend::Console, InstallScope::User);
        let mut plan = plan();
        plan.installer.services.push(zup_core::Service {
            id: zup_core::ServiceId::new("tool").unwrap(),
            name: zup_core::NonEmptyString::new("Tool").unwrap(),
            display_name: None,
            binary: zup_core::Template::parse("${location.programs}/tool/tool").unwrap(),
            arguments: Vec::new(),
            start: zup_core::ServiceStart::Automatic,
            component: None,
            when: None,
        });
        let errors = linux_capability_errors(&config, &plan);
        let text = errors.join("\n");
        assert!(text.contains("service"), "{text}");
        assert!(
            !text.contains("zup-linux"),
            "diagnostics name the configuration, never an internal crate: {text}"
        );
    }

    #[test]
    fn user_and_either_services_stay_refused() {
        for scope in [InstallScope::User, InstallScope::Either] {
            let config = config("linux-x64", Frontend::Console, scope);
            let mut plan = plan();
            plan.installer.services.push(machine_service());
            let errors = linux_capability_errors(&config, &plan);
            let text = errors.join("\n");
            assert!(
                text.contains("service"),
                "{scope:?} with a service is refused: {text}"
            );
        }
    }

    #[test]
    fn machine_services_with_plugins_stay_refused() {
        let config = config("linux-x64", Frontend::Console, InstallScope::Machine);
        let mut plan = plan();
        plan.installer.services.push(machine_service());
        plan.installer.plugins.push(zup_core::PluginBinding {
            id: zup_core::PluginId::new("acme.tool").unwrap(),
            component: None,
            when: None,
        });
        let errors = linux_capability_errors(&config, &plan);
        let text = errors.join("\n");
        assert!(text.contains("plugin"), "{text}");
    }

    fn machine_service() -> zup_core::Service {
        zup_core::Service {
            id: zup_core::ServiceId::new("tool").unwrap(),
            name: zup_core::NonEmptyString::new("Tool").unwrap(),
            display_name: None,
            binary: zup_core::Template::parse("${location.programs}/tool/tool").unwrap(),
            arguments: Vec::new(),
            start: zup_core::ServiceStart::Automatic,
            component: None,
            when: None,
        }
    }

    #[test]
    fn machine_scope_refuses_desktop_integration() {
        for scope in [InstallScope::Machine, InstallScope::Either] {
            let config = config("linux-x64", Frontend::Console, scope);
            let mut plan = plan();
            plan.installer.launchers.push(zup_core::Launcher {
                location: zup_core::LauncherLocation::Menu,
                name: zup_core::NonEmptyString::new("Tool").unwrap(),
                target: zup_core::Template::parse("${location.programs}/tool/tool").unwrap(),
                arguments: Vec::new(),
                working_directory: None,
                component: None,
                when: None,
            });
            let errors = linux_capability_errors(&config, &plan);
            let text = errors.join("\n");
            assert!(
                text.contains("machine"),
                "{scope:?} with a launcher is refused: {text}"
            );
        }
    }

    #[test]
    fn an_unsupported_linux_abi_names_the_supported_targets() {
        let errors = linux_capability_errors(
            &config("linux-x64-musl", Frontend::Console, InstallScope::User),
            &plan(),
        );
        let text = errors.join("\n");
        assert!(text.contains("linux-x64"), "{text}");
    }

    #[test]
    fn a_non_linux_target_is_not_a_linux_target() {
        let errors = linux_capability_errors(
            &config("windows-x64", Frontend::Console, InstallScope::User),
            &plan(),
        );
        let text = errors.join("\n");
        assert!(text.contains("not a Linux target"), "{text}");
    }
}
