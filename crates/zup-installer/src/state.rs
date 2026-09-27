//! Where an installation keeps its state, and what "apply this package" means.
//!
//! Two decisions live here because they are asked in the same breath: which
//! state root a scope owns, and which lifecycle an application-shaped request
//! resolves to once the machine's own record has been read.

use std::path::{Path, PathBuf};

use zup_core::{InstallScope, SelectedScope, Template};
use zup_exec::{InstallLedger, LifecycleAction};

/// The state root a scope owns when the caller names none.
pub fn state_root(scope: SelectedScope) -> miette::Result<PathBuf> {
    zup_windows::default_state_root(scope).map_err(|error| miette::miette!("{error}"))
}

/// The state root an invocation should use.
pub fn resolve_state_root(
    explicit: Option<PathBuf>,
    scope: SelectedScope,
) -> miette::Result<PathBuf> {
    zup_windows::resolve_state_root(explicit, scope).map_err(|error| miette::miette!("{error}"))
}

/// The state root of the other scope.
///
/// A machine-scope install keeps its content cache in the user's profile: a
/// per-machine cache would need an authority the acquisition engine has no
/// business holding, and the content is identical either way.
pub fn peer_user_state_root() -> miette::Result<PathBuf> {
    state_root(SelectedScope::User)
}

/// A directory the caller named, as a path template.
pub fn install_directory_template(path: &Path) -> miette::Result<Template> {
    let value = path.to_string_lossy();
    if value.contains("${") {
        return Err(miette::miette!(
            "install directory must not contain template variables: {value}"
        ));
    }
    Template::parse(&value).map_err(|error| miette::miette!("install directory: {error}"))
}

/// The install directory the committed ledger records.
pub fn persisted_install_directory(ledger: Option<&InstallLedger>) -> Option<Template> {
    ledger
        .and_then(|ledger| ledger.install_directory.as_ref())
        .and_then(|path| Template::parse(&path.to_string()).ok())
}

/// The install directory this operation should use.
///
/// An explicit path is honoured only where the application allows one, because
/// silently ignoring `--install-directory` reads as a broken installer and
/// silently honouring it where it is not allowed reads as a supported one.
pub fn choose_install_directory(
    explicit: Option<&Path>,
    ledger: Option<&InstallLedger>,
    allowed: bool,
) -> miette::Result<Option<Template>> {
    if let Some(path) = explicit {
        if !allowed {
            return Err(miette::miette!(
                "this application does not allow choosing an install directory"
            ));
        }
        return install_directory_template(path).map(Some);
    }
    if !allowed {
        return Ok(None);
    }
    Ok(persisted_install_directory(ledger))
}

/// The scope an application installs into when it names only one.
pub fn default_install_scope(scope: InstallScope) -> SelectedScope {
    match scope {
        InstallScope::Machine => SelectedScope::Machine,
        InstallScope::User | InstallScope::Either => SelectedScope::User,
    }
}

/// Which lifecycle "apply this package" means on this machine.
///
/// A person handed a newer `Acme-Setup.exe` does not need to know whether that
/// is an install or an upgrade, and neither does the framework updater that runs
/// it. The machine already knows what it has, so the machine decides:
///
/// | installed | package | action |
/// | --- | --- | --- |
/// | nothing | any | install |
/// | older | newer | upgrade |
/// | same version | same version | modify |
/// | newer | older | refused, with the versions named |
///
/// Same version resolving to `modify` is deliberate. A rebuild of the same
/// version is not an upgrade — the lifecycle would refuse it as one — but the
/// user's intent was to make the machine match this package, and that is what
/// modify does.
pub fn resolve_applied_action(
    installed_version: Option<&semver::Version>,
    package_version: &str,
) -> miette::Result<LifecycleAction> {
    let package_version = semver::Version::parse(package_version).map_err(|error| {
        miette::miette!("this package's version `{package_version}` is not a version: {error}")
    })?;
    let Some(installed_version) = installed_version else {
        return Ok(LifecycleAction::Install);
    };
    match package_version.cmp(installed_version) {
        std::cmp::Ordering::Greater => Ok(LifecycleAction::Upgrade),
        std::cmp::Ordering::Equal => Ok(LifecycleAction::Modify),
        std::cmp::Ordering::Less => Err(miette::miette!(
            "downgrade from {installed_version} to {package_version} is refused"
        )),
    }
}

/// The verb a failure message names.
pub fn action_name(action: LifecycleAction) -> &'static str {
    match action {
        LifecycleAction::Install => "install",
        LifecycleAction::Upgrade => "upgrade",
        LifecycleAction::Modify => "modify",
        LifecycleAction::Repair { .. } => "repair",
        LifecycleAction::Uninstall => "uninstall",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applying_a_package_resolves_the_lifecycle_from_the_machine() {
        assert_eq!(
            resolve_applied_action(None, "2.0.0").expect("fresh install"),
            LifecycleAction::Install
        );
        let older = semver::Version::parse("1.0.0").expect("valid");
        assert_eq!(
            resolve_applied_action(Some(&older), "2.0.0").expect("newer package"),
            LifecycleAction::Upgrade
        );
        let same = semver::Version::parse("2.0.0").expect("valid");
        assert_eq!(
            resolve_applied_action(Some(&same), "2.0.0").expect("same version"),
            LifecycleAction::Modify
        );
        let newer = semver::Version::parse("3.0.0").expect("valid");
        let refusal = resolve_applied_action(Some(&newer), "2.0.0")
            .expect_err("a downgrade is refused")
            .to_string();
        assert!(refusal.contains("downgrade"), "{refusal}");
        assert!(refusal.contains("3.0.0"), "the refusal names both versions");
    }

    #[test]
    fn a_package_whose_version_is_not_a_version_is_refused_rather_than_guessed() {
        assert!(resolve_applied_action(None, "not-a-version").is_err());
    }

    #[test]
    fn an_install_directory_the_application_forbids_is_refused_not_ignored() {
        let explicit = Path::new("/tmp/elsewhere");
        assert!(choose_install_directory(Some(explicit), None, false).is_err());
        assert!(choose_install_directory(Some(explicit), None, true).is_ok());
        assert!(
            choose_install_directory(None, None, false)
                .expect("nothing to choose")
                .is_none()
        );
    }

    #[test]
    fn a_directory_with_template_variables_is_not_a_directory_a_user_named() {
        let error = install_directory_template(Path::new("/opt/${location.user_data}"))
            .expect_err("a template is not a chosen path")
            .to_string();
        assert!(error.contains("template variables"), "{error}");
    }
}
