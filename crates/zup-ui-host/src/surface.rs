//! Which surface a session opens, and what it says before anything happens.

use zup_core::{InstallScope, Installer, SelectedScope};
use zup_exec::InstallLedger;
use zup_ui_protocol::{
    ComponentOption, InstallOptions, InstallationHealth, MaintenanceState, ProductIdentity,
    UiCapabilities,
};

use crate::convert;

/// The product as a person sees it named.
pub fn product(installer: &Installer) -> ProductIdentity {
    ProductIdentity {
        name: installer.app.name.to_string(),
        publisher: installer.app.publisher.as_ref().map(ToString::to_string),
        version: installer.app.version.to_string(),
        description: installer.app.description.clone(),
    }
}

/// Every scope this package installs into.
pub fn scopes(installer: &Installer) -> Vec<zup_ui_protocol::InstallScope> {
    match installer.install.scope {
        InstallScope::User => vec![zup_ui_protocol::InstallScope::User],
        InstallScope::Machine => vec![zup_ui_protocol::InstallScope::Machine],
        InstallScope::Either => vec![
            zup_ui_protocol::InstallScope::User,
            zup_ui_protocol::InstallScope::Machine,
        ],
    }
}

/// The scope a fresh session starts in when the caller named none.
pub fn default_scope(installer: &Installer) -> SelectedScope {
    match installer.install.scope {
        InstallScope::Machine => SelectedScope::Machine,
        InstallScope::User | InstallScope::Either => SelectedScope::User,
    }
}

/// The components a person may choose, with the selection this machine implies.
///
/// An existing installation's ledger is the authority on what it has; a fresh
/// session falls back to what the application declares.
pub fn components(
    installer: &Installer,
    preselected: Option<&[zup_core::ComponentId]>,
    installed: Option<&InstallLedger>,
) -> Vec<ComponentOption> {
    installer
        .components
        .iter()
        .map(|component| {
            let selected = match preselected {
                Some(preselected) => preselected.contains(&component.id),
                None => {
                    installed
                        .is_some_and(|ledger| ledger.selected_components.contains(&component.id))
                        || component.default
                        || component.required
                }
            };
            ComponentOption {
                id: convert::component(&component.id),
                name: component.name.to_string(),
                description: component.description.clone(),
                required: component.required,
                selected,
            }
        })
        .collect()
}

/// The choices a fresh installation offers.
pub fn install_options(
    installer: &Installer,
    scope: SelectedScope,
    existing_version: Option<String>,
    preselected: Option<&[zup_core::ComponentId]>,
    installed: Option<&InstallLedger>,
) -> InstallOptions {
    InstallOptions {
        existing_version,
        scopes: scopes(installer),
        scope: convert::scope(scope),
        components: components(installer, preselected, installed),
        install_directory: installed.and_then(|ledger| persisted_location(Some(ledger))),
        allow_directory_override: installer.install.allow_directory_override,
    }
}

/// What a person can do to an installation that already exists.
///
/// A maintenance session applies to the installation that exists: it does not
/// offer a choice of scope, because moving an installation between scopes is a
/// different decision. The health is `Unknown` because nothing has inspected
/// this machine yet - a repair is what turns it into an answer.
pub fn maintenance_state(
    installer: &Installer,
    ledger: &InstallLedger,
    scope: SelectedScope,
) -> MaintenanceState {
    MaintenanceState {
        installed_version: ledger.version.to_string(),
        components: components(installer, None, Some(ledger)),
        updates_enabled: installer.updates.is_some(),
        scope: convert::scope(scope),
        install_directory: persisted_location(Some(ledger)),
        health: InstallationHealth::Unknown,
    }
}

/// What this installer can offer a preset.
pub fn capabilities(installer: &Installer, maintenance: bool) -> UiCapabilities {
    zup_artifact::ui::offers_for(installer, maintenance)
}

/// The location an installation committed, as text.
///
/// Not on the snapshot: a path to a local directory is a fact about this machine
/// and not something a published preset can be shown.
pub fn persisted_location(ledger: Option<&InstallLedger>) -> Option<String> {
    ledger
        .and_then(|ledger| ledger.install_directory.as_ref())
        .and_then(|path| zup_core::Template::parse(&path.to_string()).ok())
        .map(|template| template.to_string())
}
