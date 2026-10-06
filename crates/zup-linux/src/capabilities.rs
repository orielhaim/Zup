//! What a Linux installer in this phase may touch, decided before it touches anything.
//!
//! The rule is: the complete target is validated *before* the transaction
//! mutates anything. A transaction that installs files successfully and then
//! fails on a service leaves a machine with half an installation and a journal
//! that says the plan was coherent. It was not.
//!
//! Supported now: user scope, files, maintenance content, transaction state, and
//! the console and headless frontends (the frontend is a property of the
//! installer binary, not of the plan, so it is selected by which binary runs).
//!
//! Portable launchers, protocols, and file associations never reach this gate
//! as target resources: resolution lowers them into generated integration
//! files (desktop entries, MIME packages) that travel the file path. A target
//! plan that still names launchers, PATH entries, services, protocols, or
//! associations directly was not produced by this backend's resolution and is
//! refused here, in one diagnostic that names every unsupported active
//! resource. One coherent step, not a scatter of `Unsupported` returns through
//! individual executor methods.

use zup_core::SelectedScope;
use zup_platform::TargetPlan;

/// A target plan that cannot be installed on Linux in this phase, naming everything
/// that makes it so.
#[derive(Debug, thiserror::Error)]
#[error("this installation cannot proceed on Linux: {reasons}")]
pub struct LinuxCapabilityError {
    reasons: String,
}

impl LinuxCapabilityError {
    /// Every reason the plan was refused, as one diagnostic.
    pub fn reasons(&self) -> &str {
        &self.reasons
    }
}

/// Prove a target plan is installable on Linux before the transaction runs.
///
/// Pure observation: nothing is created, resolved, or mutated. A plan that
/// passes may still fail at apply time - a disk can fill - but it will not fail
/// because the plan asked for something this backend has no mechanism for.
pub fn validate_target_plan(plan: &TargetPlan) -> Result<(), LinuxCapabilityError> {
    let mut refused: Vec<String> = Vec::new();

    if plan.target.operating_system() != zup_core::TargetOperatingSystem::Linux {
        refused.push(format!(
            "target `{}` is not a Linux target",
            plan.target.as_str()
        ));
    }
    if plan.scope != SelectedScope::User {
        let scope = plan.scope;
        refused.push(format!(
            "scope `{scope}` is not supported: machine scope needs a privilege mechanism this phase does not have",
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
    unsupported("service", plan.services.len());
    unsupported("launcher", plan.launchers.len());
    unsupported("PATH entry", plan.path_entries.len());
    unsupported("URI protocol", plan.protocols.len());
    unsupported("file association", plan.file_associations.len());
    unsupported("package-manager prerequisite", plan.prerequisites.len());

    if refused.is_empty() {
        Ok(())
    } else {
        Err(LinuxCapabilityError {
            reasons: refused.join("; "),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_plan() -> TargetPlan {
        let target = zup_core::TargetTriple::parse("x86_64-unknown-linux-gnu").expect("a target");
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

    #[test]
    fn a_plain_user_file_plan_is_accepted() {
        assert!(validate_target_plan(&empty_plan()).is_ok());
    }

    /// The diagnostic names *every* unsupported active resource, not just the
    /// first one found. A caller that fixed one refusal only to discover the
    /// next would be iterating against the validator instead of the manifest.
    #[test]
    fn one_diagnostic_names_every_unsupported_resource() {
        let mut plan = empty_plan();
        plan.scope = SelectedScope::Machine;
        plan.services.push(zup_platform::TargetService {
            key: zup_core::ResourceKey::Service {
                id: zup_core::ServiceId::new("tool").expect("an id"),
            },
            id: zup_core::ServiceId::new("tool").expect("an id"),
            name: zup_core::NonEmptyString::new("Tool").expect("a name"),
            display_name: None,
            command: zup_platform::CommandSpec::new(
                zup_platform::TargetPath::new(
                    plan.target.clone(),
                    "/home/u/.local/lib/zup/apps/tool/tool",
                )
                .expect("a path"),
                Vec::new(),
            ),
            start: zup_core::ServiceStart::Automatic,
            privilege: zup_core::Privilege::System,
        });
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

        let error = validate_target_plan(&plan).expect_err("machine scope with services refuses");
        let reasons = error.reasons();
        assert!(reasons.contains("machine"), "scope is named: {reasons}");
        assert!(reasons.contains("service"), "services are named: {reasons}");
        assert!(
            reasons.contains("launcher"),
            "launchers are named: {reasons}"
        );
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
