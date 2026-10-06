//! What the public build path may compose for a Linux target.
//!
//! One source of truth for Linux build capability, read by `zup check`,
//! `zup build`, and `zup doctor` alike. A configuration refused here is refused
//! before artifact composition on every host, so `check` never calls a project
//! buildable that `build` then rejects for a known capability reason.
//!
//! The checks read the portable installer IR and the materialized plan, which
//! is what makes them host-independent: a Windows host cross-building a Linux
//! installer answers the same question a native Linux host answers. The
//! Linux-native lowering in `zup-linux` remains authoritative at install time;
//! this is the build-time mirror of its unsupported set, and the two must agree.
//!
//! Supported in this phase:
//!
//! ```text
//! target:    x86_64-unknown-linux-gnu
//! frontends: console, headless
//! artifact:  self-contained native installer (one per target)
//! scope:     user
//! desktop:   menu launchers, URI protocols, file associations
//! ```
//!
//! Everything else - GUI, machine scope, dispatcher/universal artifacts,
//! services, literal desktop icons, PATH entries, package-manager
//! prerequisites - is refused here with a diagnostic that names the
//! configuration, not an internal crate.

use zup_core::{Frontend, InstallScope, ResolvedTargetConfig, TargetBuildPlan, TargetTriple};

/// The one Linux target this Zup version builds installers for.
pub const SUPPORTED_LINUX_TARGET: &str = "x86_64-unknown-linux-gnu";

/// Whether `target` is a Linux target at all, supported or not.
///
/// This answers the platform question only. Whether the target is one this
/// Zup version builds is [`is_supported_linux_target`]: the two have different
/// diagnostics, and conflating them would report an macOS target as "not yet
/// supported on Linux" rather than as not a Linux target.
pub fn is_linux_target(target: &TargetTriple) -> bool {
    target.operating_system() == zup_core::TargetOperatingSystem::Linux
}

/// Every reason `target` alone rules out, before any source tree is walked.
///
/// The target triple and the frontend are known from selection alone, so a
/// build refuses them before materialization - and before preset resolution,
/// which would otherwise report a missing GUI preset where the real answer is
/// that no Linux GUI runtime exists to present one.
pub fn linux_selection_errors(target: &TargetTriple, frontend: Frontend) -> Vec<String> {
    let mut errors = linux_target_errors(target);
    // The frontend question is only asked of Linux targets: anything else was
    // already refused as not a Linux target, and a GUI diagnostic naming a
    // Windows triple would be the wrong answer about the wrong platform.
    if is_linux_target(target) {
        errors.extend(linux_frontend_errors(target, frontend));
    }
    errors
}

/// Whether `target` is the Linux target the public build path composes.
pub fn is_supported_linux_target(target: &TargetTriple) -> bool {
    target.as_str() == SUPPORTED_LINUX_TARGET
}

fn linux_target_errors(target: &TargetTriple) -> Vec<String> {
    if is_supported_linux_target(target) {
        return Vec::new();
    }
    // The backend-boundary shape: selection is refused before the source tree
    // is walked, with the same prefix a host without a backend reports, so one
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

fn linux_frontend_errors(target: &TargetTriple, frontend: Frontend) -> Vec<String> {
    match frontend {
        Frontend::Gui => vec![format!(
            "GUI installer runtime is not supported for `{target}`: the Linux backend ships \
             console and headless runtimes in this phase"
        )],
        Frontend::Console | Frontend::Headless => Vec::new(),
    }
}

/// Every reason `config` cannot become a Linux installer, in manifest terms.
///
/// Empty means the public path may compose it: the target is the supported
/// one, the frontend exists as a Linux runtime template, the scope needs no
/// privilege mechanism, and the plan carries no resource the Linux backend has
/// no mechanism for. Each entry is one sentence a project author can act on.
pub fn linux_capability_errors(
    config: &ResolvedTargetConfig,
    plan: &TargetBuildPlan,
) -> Vec<String> {
    let mut errors = Vec::new();
    let target = &config.target;
    // No early return: one diagnostic names every unsupported dimension, so
    // an author fixes the configuration against the validator rather than
    // iterating one refusal at a time. The fail-fast pre-materialization check
    // in the build path is separate and stays narrow.
    errors.extend(linux_selection_errors(target, config.frontend));
    match config.install.scope {
        InstallScope::User => {}
        InstallScope::Machine => errors.push(
            "machine-scope installation is not supported by the Linux backend yet: it needs a \
             privilege mechanism this phase does not have"
                .to_owned(),
        ),
        InstallScope::Either => errors.push(
            "install scope `either` is not supported by the Linux backend yet: machine scope \
             needs a privilege mechanism this phase does not have"
                .to_owned(),
        ),
    }
    let installer = &plan.installer;
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
        unsupported("service", installer.services.len());
        unsupported(
            "package-manager prerequisite",
            installer.prerequisites.len(),
        );
    }
    // Menu launchers, URI protocols, and file associations lower into
    // freedesktop desktop entries and Shared MIME-info packages. A literal
    // desktop icon has no desktop-neutral implementation, and a
    // directory-level PATH mutation is not command exposure without editing
    // shell configuration, so those stay refused with their reasons.
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
    errors.extend(main_executable_errors(config, plan));
    errors
}

/// Whether protocol handlers lower faithfully onto Linux.
///
/// Linux delivers the URI through one `%u`, so every protocol must carry
/// exactly one `%1` placeholder, and every protocol must name the same
/// handler command: one hidden desktop entry dispatches all schemes, and
/// distinct commands would need distinct entries this phase does not lower.
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

/// Whether file associations lower faithfully onto Linux.
///
/// One hidden desktop entry opens every associated type with `%f`, so every
/// association must name the same executable.
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

/// Whether the declared application main resolves to an executable payload file.
///
/// The executable-intent model is authoritative: nothing infers executability
/// from bytes. On Linux a declared main must name a shipped file marked
/// `executable = true`, or the installer would ship a main program the user
/// cannot run without a diagnostic ever saying so.
///
/// A project that ships nothing has nothing to be incoherent with, so the rule
/// applies only once files exist: a fresh `zup init` project checks clean on
/// every host, and the requirement bites when there is a payload to get wrong.
fn main_executable_errors(config: &ResolvedTargetConfig, plan: &TargetBuildPlan) -> Vec<String> {
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
            "the declared application main `{main}` for `{}` matches no shipped file: name a \
             `[[files]]` destination, or remove `app.main`",
            config.profile,
        )];
    }
    if matches.iter().all(|file| !file.executable) {
        return vec![format!(
            "the declared application main `{main}` for `{}` is not executable: mark the shipped \
             file `executable = true`",
            config.profile,
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
            target: TargetTriple::parse(target).unwrap(),
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
                target: TargetTriple::parse(SUPPORTED_LINUX_TARGET).unwrap(),
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
    fn menu_launchers_protocols_and_associations_are_supported() {
        let config = config(
            SUPPORTED_LINUX_TARGET,
            Frontend::Console,
            InstallScope::User,
        );
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
        plan.installer.protocols.push(zup_core::Protocol {
            scheme: zup_core::ProtocolScheme::new("acme").unwrap(),
            executable: zup_core::Template::parse("${location.programs}/tool/tool").unwrap(),
            args: vec!["%1".into()],
            when: None,
        });
        plan.installer
            .file_associations
            .push(zup_core::FileAssociation {
                extension: zup_core::FileExtension::new(".foo").unwrap(),
                id: zup_core::FileAssociationId::new("acme.foo").unwrap(),
                description: None,
                executable: zup_core::Template::parse("${location.programs}/tool/tool").unwrap(),
                when: None,
            });
        let errors = linux_capability_errors(&config, &plan);
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
        let config = config(SUPPORTED_LINUX_TARGET, Frontend::Gui, InstallScope::Machine);
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
        assert!(text.contains("machine"), "{text}");
        assert!(text.contains("service"), "{text}");
        assert!(
            !text.contains("zup-linux"),
            "diagnostics name the configuration, never an internal crate: {text}"
        );
    }

    #[test]
    fn another_linux_architecture_names_the_supported_target() {
        let errors = linux_capability_errors(
            &config(
                "aarch64-unknown-linux-gnu",
                Frontend::Console,
                InstallScope::User,
            ),
            &plan(),
        );
        let text = errors.join("\n");
        assert!(text.contains(SUPPORTED_LINUX_TARGET), "{text}");
    }

    #[test]
    fn a_non_linux_target_is_not_a_linux_target() {
        let errors = linux_capability_errors(
            &config(
                "x86_64-pc-windows-msvc",
                Frontend::Console,
                InstallScope::User,
            ),
            &plan(),
        );
        let text = errors.join("\n");
        assert!(text.contains("not a Linux target"), "{text}");
    }
}
