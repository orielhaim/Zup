//! Turning a portable desired state into a Linux one.
//!
//! `resolve_target` answers one question: where does everything in this
//! installation go on a Linux machine. Templates become target paths through
//! the Linux location policy, every path is validated as a Linux path, and no
//! two owned resources may claim the same name.
//!
//! What it does not do is invent host paths for concepts Linux has no
//! mechanism for. A plan with active launchers, services, PATH entries,
//! protocols, associations, or package-manager prerequisites is refused here,
//! naming everything at once - resolving them into a plan the capability gate
//! would then reject would be two diagnostics for one mistake, and inventing
//! Linux spellings for them (a `.desktop` path, a unit name) would be fake
//! implementations wearing a resolver's clothes.

use std::collections::BTreeMap;

use zup_core::{ResourceKey, SelectedScope, TargetTriple};
use zup_plan::InstallPlan;
use zup_platform::{
    TargetFile, TargetPath, TargetPlan, TargetPlanSummary, TargetPrerequisite,
    TemplateResolveError, resolve_template_path,
};

use crate::locations::LinuxInstallLocationResolver;
use crate::lowering::{LinuxPathLoweringError, to_host_path};

/// Why a portable plan cannot become a Linux target plan.
#[derive(Debug, thiserror::Error)]
pub enum LinuxResolveError {
    #[error("cannot resolve for target `{target}`: not a Linux target")]
    UnsupportedTarget { target: String },

    #[error("cannot resolve {kind} `{path}`: {reason}")]
    InvalidPath {
        kind: &'static str,
        path: String,
        reason: String,
    },

    #[error("location or template failure for {kind}: {source}")]
    Template {
        kind: &'static str,
        #[source]
        source: TemplateResolveError,
    },

    #[error("two owned resources claim `{second}`: {kind} collides with `{first}`")]
    Collision {
        kind: &'static str,
        first: String,
        second: String,
    },

    #[error("this installation cannot proceed on Linux: {reasons}")]
    Unsupported { reasons: String },
}

/// Resolve a portable desired state into a Linux target plan.
///
/// Total over well-formed plans: files, the install directory, and
/// prerequisites lower normally, and anything else active is refused in one
/// diagnostic rather than resolved into a plan that could never execute.
pub fn resolve_target(plan: &InstallPlan) -> Result<TargetPlan, LinuxResolveError> {
    if plan.target.operating_system() != zup_core::TargetOperatingSystem::Linux {
        return Err(LinuxResolveError::UnsupportedTarget {
            target: plan.target.to_string(),
        });
    }
    refuse_unsupported(plan)?;

    let scope = plan.scope;
    let resolve = |template: &zup_core::Template, kind: &'static str| {
        let path =
            resolve_template_path(template, &plan.target, &LinuxInstallLocationResolver, scope)
                .map_err(|source| LinuxResolveError::Template { kind, source })?;
        validate_linux_path(kind, &path)?;
        Ok::<TargetPath, LinuxResolveError>(path)
    };

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum OwnedKind {
        Directory,
        File,
    }

    let mut owned: BTreeMap<String, (OwnedKind, &'static str)> = BTreeMap::new();
    let mut claim = |path: &TargetPath,
                     kind: &'static str,
                     path_kind: OwnedKind|
     -> Result<(), LinuxResolveError> {
        let name = path.to_string();
        if let Some((_, first)) = owned.get(&name) {
            return Err(LinuxResolveError::Collision {
                kind,
                first: (*first).to_owned(),
                second: name,
            });
        }
        // A file inside an owned directory is the normal case; anything else
        // where one owned path contains another means one of them is not what
        // it claims to be. Identity is exact text - Linux is case-sensitive,
        // so `Tool` and `tool` are two names and folding them would refuse
        // installations the filesystem accepts.
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
                return Err(LinuxResolveError::Collision {
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

    // Portable integration intent lowers into generated native files here, so
    // the snapshot, delta, transaction, and ledger below all treat a desktop
    // entry or MIME package as what it is: a file Zup owns.
    let integration = crate::integration::lower_integration(plan).map_err(|error| match error {
        crate::integration::IntegrationError::Unsupported { .. } => {
            LinuxResolveError::Unsupported {
                reasons: error.to_string(),
            }
        }
        crate::integration::IntegrationError::DataHome(source) => LinuxResolveError::Template {
            kind: "XDG data home",
            source: zup_platform::TemplateResolveError::InstallLocation(
                zup_platform::InstallLocationError::ResolutionFailed {
                    location: zup_core::InstallLocation::UserData,
                    scope: plan.scope,
                    source: Box::new(source),
                },
            ),
        },
        other => LinuxResolveError::InvalidPath {
            kind: "integration resource",
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
                |_| LinuxResolveError::InvalidPath {
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
    Ok(TargetPlan {
        app: plan.app.clone(),
        target: plan.target.clone(),
        scope: plan.scope,
        install_directory,
        selected_components: plan.selected_components.clone(),
        prerequisites,
        files,
        launchers: Vec::new(),
        path_entries: Vec::new(),
        services: Vec::new(),
        protocols: Vec::new(),
        file_associations: Vec::new(),
        summary: TargetPlanSummary {
            file_count,
            install_bytes,
            resource_count: 0,
            requires_authorization: plan.summary.requires_authorization,
            selected_component_count: plan.selected_components.len(),
            prerequisite_count: plan.prerequisites.len(),
            download_bytes: 0,
        },
        // A build plan says which preset; it does not say which bytes. The
        // executable is content, resolved by whoever supplies the payload.
        preset: None,
    })
}

/// Whether `ancestor` is a strict path ancestor of `path, component-wise.
///
/// Compared as text on `/` separators, because both spellings are canonical
/// target paths for the same Linux target: no normalization is left to do, and
/// a prefix that stops mid-component (`/opt/ac` vs `/opt/acme`) is not an
/// ancestor.
fn is_ancestor(ancestor: &str, path: &str) -> bool {
    path.len() > ancestor.len()
        && path.starts_with(ancestor)
        && path.as_bytes()[ancestor.len()] == b'/'
}

/// Prove a resolved path is a valid Linux path, or say which rule it breaks.
fn validate_linux_path(kind: &'static str, path: &TargetPath) -> Result<(), LinuxResolveError> {
    to_host_path(path).map_err(|error| {
        let (path_text, reason) = match &error {
            LinuxPathLoweringError::UnsupportedTarget { .. } => {
                (path.to_string(), "not a Linux target path".to_owned())
            }
            LinuxPathLoweringError::InvalidComponent { component, reason } => (
                path.to_string(),
                format!("component `{component}`: {reason}"),
            ),
            LinuxPathLoweringError::InvalidPath(error) => (path.to_string(), error.to_string()),
        };
        LinuxResolveError::InvalidPath {
            kind,
            path: path_text,
            reason,
        }
    })?;
    Ok(())
}

/// Refuse every active resource this backend has no mechanism for, in one
/// diagnostic.
///
/// Launchers, protocols, and file associations are not refused here: they
/// lower into generated integration files above, and only the shapes with no
/// honest mapping (a literal desktop icon, a directory PATH mutation) are
/// refused by that lowering with their reasons.
fn refuse_unsupported(plan: &InstallPlan) -> Result<(), LinuxResolveError> {
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
    unsupported("service", plan.services.len());
    unsupported("package-manager prerequisite", plan.prerequisites.len());
    if plan.scope != SelectedScope::User {
        refused
            .push("machine scope needs a privilege mechanism this phase does not have".to_owned());
    }
    if refused.is_empty() {
        Ok(())
    } else {
        Err(LinuxResolveError::Unsupported {
            reasons: refused.join("; "),
        })
    }
}

/// Resolve with an explicit target triple for the location policy.
///
/// `resolve_target` answers from the plan's own target; this answers from a
/// caller-supplied one, for plans whose target the caller has already
/// committed to (a recovery run replays a journal, not a manifest, and the
/// journal's target is the commitment).
pub fn resolve_target_for(
    plan: &InstallPlan,
    target: &TargetTriple,
) -> Result<TargetPlan, LinuxResolveError> {
    let mut borrowed = plan.clone();
    borrowed.target = target.clone();
    resolve_target(&borrowed)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let resolved = resolve_target(&input).expect("a file plan resolves");
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
                resolve_target(&input),
                Err(LinuxResolveError::Collision { .. })
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
                resolve_target(&input),
                Err(LinuxResolveError::Collision { .. })
            ),
            "a file cannot contain another file"
        );
    }

    /// Case is significant on Linux: `Tool` and `tool` are two names, and a
    /// resolver that folded them would refuse installations the filesystem
    /// accepts.
    #[test]
    fn case_distinguishes_destinations() {
        let mut input = plan();
        input.files.push(file("Tool"));
        input.files.push(file("tool"));
        assert!(
            resolve_target(&input).is_ok(),
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
        // A menu launcher is not part of this refusal: it lowers into a
        // generated desktop entry rather than being refused.
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
        let error = resolve_target(&input).expect_err("services refuse");
        let message = error.to_string();
        assert!(message.contains("service"), "{message}");
    }

    #[test]
    fn a_non_linux_plan_is_refused() {
        let mut input = plan();
        input.target = TargetTriple::parse("x86_64-pc-windows-msvc").expect("a target");
        assert!(matches!(
            resolve_target(&input),
            Err(LinuxResolveError::UnsupportedTarget { .. })
        ));
    }
}
