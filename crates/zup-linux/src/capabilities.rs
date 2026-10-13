//! Target capability rules for the Linux backend, evaluated on any host.
//!
//! Build tooling calls these before artifact composition; the installer calls
//! them again through operation preparation when it runs.

use zup_core::{
    Frontend, InstallScope, ResolvedTargetConfig, SelectedScope, Target, TargetBuildPlan,
};

pub use zup_platform::{SUPPORTED_LINUX_TARGET, is_linux_target};

pub fn is_supported_linux_target(target: &Target) -> bool {
    is_linux_target(target) && zup_platform::is_supported_target(target)
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
    // assertion covers every refusal that never touched the filesystem.
    vec![if is_linux_target(target) {
        format!(
            "unsupported backend for target `{target}`: this Zup version builds Linux \
             installers for `{SUPPORTED_LINUX_TARGET}` only"
        )
    } else {
        format!("unsupported backend for target `{target}`: not a Linux target")
    }]
}

fn linux_frontend_errors(target: &Target, frontend: Frontend) -> Vec<String> {
    if zup_platform::supports_frontend(target, frontend) {
        return Vec::new();
    }
    match frontend {
        Frontend::Gui => vec![format!(
            "GUI installer runtime is not supported for `{target}`: the Linux backend ships \
             console and headless runtimes in this phase"
        )],
        Frontend::Console | Frontend::Headless => vec![format!(
            "frontend `{frontend}` is not supported for `{target}`"
        )],
    }
}

/// when the installer runs, never at build time - and either-scope
pub fn linux_capability_errors(
    config: &ResolvedTargetConfig,
    plan: &TargetBuildPlan,
) -> Vec<String> {
    let mut errors = Vec::new();
    let target = &config.target;
    errors.extend(linux_selection_errors(target, config.frontend));
    errors.extend(resource_errors(
        &plan.installer,
        matches!(
            config.install.scope,
            InstallScope::Machine | InstallScope::Either
        ),
        !matches!(config.install.scope, InstallScope::Machine),
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
        scope == SelectedScope::Machine,
        scope == SelectedScope::User,
        installer.app.name.as_str(),
        build,
    ));
    if scope == SelectedScope::Machine && !installer.plugins.is_empty() {
        errors.push(
            "machine scope refuses projects that need plugin execution: the privileged worker \
             never executes plugins as root"
                .to_owned(),
        );
    }
    if scope == SelectedScope::Machine && !installer.prerequisites.is_empty() {
        errors.push("machine scope runs no prerequisite installers".to_owned());
    }
    errors
}

fn resource_errors(
    installer: &zup_core::Installer,
    machine_leg: bool,
    user_scope: bool,
    name: &str,
    plan: &TargetBuildPlan,
) -> Vec<String> {
    let mut errors = Vec::new();
    // privileged worker never executes plugins as root.
    if user_scope && !installer.services.is_empty() {
        errors.push(format!(
            "{} service resource{} {} not supported by the Linux backend yet: system services \
             need machine scope (systemd system units); user services are deferred past this \
             phase",
            installer.services.len(),
            if installer.services.len() == 1 {
                ""
            } else {
                "s"
            },
            if installer.services.len() == 1 {
                "is"
            } else {
                "are"
            },
        ));
    }
    if machine_leg && !installer.plugins.is_empty() && !installer.services.is_empty() {
        errors.push(
            "services combined with plugins are not supported for a machine installation: \
             static manifest services run through systemd, but plugin-generated privileged \
             services remain refused until a separately designed trust model exists"
                .to_owned(),
        );
    }
    {
        let mut unsupported = |kind: &str, count: usize| {
            if count > 0 {
                errors.push(format!(
                    "{count} {kind} resource{} {} not supported by the Linux backend yet",
                    if count == 1 { "" } else { "s" },
                    if count == 1 { "is" } else { "are" },
                ));
            }
        };
        unsupported(
            "package-manager prerequisite",
            installer.prerequisites.len(),
        );
    }
    if machine_leg {
        let mut machine_unsupported = |kind: &str, count: usize| {
            if count > 0 {
                errors.push(format!(
                    "{count} {kind} resource{} {} not supported for a machine installation: \
                     machine desktop integration is deferred past this phase",
                    if count == 1 { "" } else { "s" },
                    if count == 1 { "is" } else { "are" },
                ));
            }
        };
        machine_unsupported("menu launcher", installer.launchers.len());
        machine_unsupported("URI protocol", installer.protocols.len());
        machine_unsupported("file association", installer.file_associations.len());
    }
    let desktop_launchers = installer
        .launchers
        .iter()
        .filter(|launcher| launcher.location == zup_core::LauncherLocation::Desktop)
        .count();
    if desktop_launchers > 0 {
        errors.push(format!(
            "{desktop_launchers} `desktop` launcher resource{} {} not supported by the Linux \
             backend yet: a file in `$XDG_DATA_HOME/applications` makes an application \
             discoverable, which the `menu` location provides, but placing a trusted icon on \
             the user's desktop has no desktop-neutral implementation",
            if desktop_launchers == 1 { "" } else { "s" },
            if desktop_launchers == 1 { "is" } else { "are" },
        ));
    }
    if !installer.path.is_empty() {
        errors.push(format!(
            "{} PATH entry resource{} {} not supported by the Linux backend yet: a \
             directory-level PATH mutation is not command exposure, and Linux command exposure \
             without editing shell configuration needs a portable `command` semantic this phase \
             does not add",
            installer.path.len(),
            if installer.path.len() == 1 { "" } else { "s" },
            if installer.path.len() == 1 {
                "is"
            } else {
                "are"
            },
        ));
    }
    errors.extend(protocol_errors(&installer.protocols));
    errors.extend(association_errors(&installer.file_associations));
    if installer.preset.is_some() {
        errors.push(
            "a preset window is not supported by the Linux backend yet: console and headless \
             installers present no window"
                .to_owned(),
        );
    }
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
                target: Target::parse(SUPPORTED_LINUX_TARGET).unwrap(),
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
            &config(
                SUPPORTED_LINUX_TARGET,
                Frontend::Console,
                InstallScope::User,
            ),
            &plan(),
        );
        assert!(errors.is_empty(), "{errors:?}");
        let errors = linux_capability_errors(
            &config(
                SUPPORTED_LINUX_TARGET,
                Frontend::Headless,
                InstallScope::User,
            ),
            &plan(),
        );
        assert!(errors.is_empty(), "{errors:?}");
    }

    #[test]
    fn protocols_without_one_placeholder_are_refused() {
        let config = config(
            SUPPORTED_LINUX_TARGET,
            Frontend::Console,
            InstallScope::User,
        );
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
        let config = config(
            SUPPORTED_LINUX_TARGET,
            Frontend::Console,
            InstallScope::User,
        );
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
        assert!(text.contains("`desktop` launcher"), "{text}");
        assert!(text.contains("PATH entry"), "{text}");
        assert!(
            !text.contains("zup-linux"),
            "diagnostics name the configuration, never an internal crate: {text}"
        );
    }
    #[test]
    fn every_unsupported_dimension_is_named() {
        let config = config(SUPPORTED_LINUX_TARGET, Frontend::Gui, InstallScope::User);
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
        assert!(text.contains("GUI"), "{text}");
        assert!(text.contains("service"), "{text}");
        assert!(
            !text.contains("zup-linux"),
            "diagnostics name the configuration, never an internal crate: {text}"
        );
    }

    #[test]
    fn user_and_either_services_stay_refused() {
        for scope in [InstallScope::User, InstallScope::Either] {
            let config = config(SUPPORTED_LINUX_TARGET, Frontend::Console, scope);
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
        let config = config(
            SUPPORTED_LINUX_TARGET,
            Frontend::Console,
            InstallScope::Machine,
        );
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
            let config = config(SUPPORTED_LINUX_TARGET, Frontend::Console, scope);
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
    fn another_linux_architecture_names_the_supported_target() {
        let errors = linux_capability_errors(
            &config("linux-arm64", Frontend::Console, InstallScope::User),
            &plan(),
        );
        let text = errors.join("\n");
        assert!(text.contains(SUPPORTED_LINUX_TARGET), "{text}");
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
