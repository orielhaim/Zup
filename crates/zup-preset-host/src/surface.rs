use zup_core::{InstallScope, Installer, SelectedScope};
use zup_exec::InstallLedger;
use zup_preset_protocol::{
    Capabilities, ComponentGroupOption, ComponentOption, ComponentProminence, InstallOptions,
    InstallationHealth, MaintenanceState, ProductIdentity, SelectionRequirement,
};

use crate::convert;

pub fn product(installer: &Installer) -> ProductIdentity {
    ProductIdentity {
        name: installer.app.name.to_string(),
        publisher: installer.app.publisher.as_ref().map(ToString::to_string),
        version: installer.app.version.to_string(),
        description: installer.app.description.clone(),
    }
}

pub fn scopes(installer: &Installer) -> Vec<zup_preset_protocol::InstallScope> {
    match installer.install.scope {
        InstallScope::User => vec![zup_preset_protocol::InstallScope::User],
        InstallScope::Machine => vec![zup_preset_protocol::InstallScope::Machine],
        InstallScope::Either => vec![
            zup_preset_protocol::InstallScope::User,
            zup_preset_protocol::InstallScope::Machine,
        ],
    }
}

pub fn default_scope(installer: &Installer) -> SelectedScope {
    match installer.install.scope {
        InstallScope::Machine => SelectedScope::Machine,
        InstallScope::User | InstallScope::Either => SelectedScope::User,
    }
}

pub fn components(
    installer: &Installer,
    preselected: Option<&[zup_core::ComponentId]>,
    installed: Option<&InstallLedger>,
) -> Vec<ComponentOption> {
    installer
        .components
        .iter()
        .map(|component| {
            let has = installed.map(|ledger| ledger.selected_components.contains(&component.id));
            let selected = match (preselected, has) {
                (Some(preselected), _) => preselected.contains(&component.id),
                (None, Some(has)) => has || component.required,
                (None, None) => component.default || component.required,
            };
            ComponentOption {
                id: convert::component(&component.id),
                name: component.name.to_string(),
                description: component.description.clone(),
                required: component.required,
                selected,
                installed: has.unwrap_or(false),
            }
        })
        .collect()
}

pub fn component_groups(
    installer: &Installer,
    listed: &[ComponentOption],
) -> Vec<ComponentGroupOption> {
    let mut groups: Vec<ComponentGroupOption> = installer
        .component_groups
        .iter()
        .map(|group| ComponentGroupOption {
            id: group.id.to_string(),
            label: group.label.as_ref().map(ToString::to_string),
            description: group.description.clone(),
            prominence: convert::prominence(group.prominence),
            selection: convert::selection(group.selection),
            components: installer
                .components
                .iter()
                .filter(|component| component.group.as_ref().is_some_and(|id| id == &group.id))
                .map(|component| convert::component(&component.id))
                .collect(),
        })
        .filter(|group| !group.components.is_empty())
        .collect();
    let claimed: std::collections::BTreeSet<&zup_preset_protocol::ComponentId> =
        groups.iter().flat_map(|group| &group.components).collect();
    let rest: Vec<_> = listed
        .iter()
        .filter(|component| !claimed.contains(&component.id))
        .map(|component| component.id.clone())
        .collect();
    if !rest.is_empty() {
        groups.insert(
            0,
            ComponentGroupOption {
                id: String::new(),
                label: None,
                description: None,
                prominence: ComponentProminence::Auto,
                selection: SelectionRequirement::Defaulted,
                components: rest,
            },
        );
    }
    groups
}

pub fn install_options(
    installer: &Installer,
    scope: SelectedScope,
    existing_version: Option<String>,
    preselected: Option<&[zup_core::ComponentId]>,
    installed: Option<&InstallLedger>,
) -> InstallOptions {
    let components = components(installer, preselected, installed);
    let groups = component_groups(installer, &components);
    InstallOptions {
        existing_version,
        scopes: scopes(installer),
        scope: convert::scope(scope),
        components,
        groups,
        install_directory: installed.and_then(|ledger| persisted_location(Some(ledger))),
        allow_directory_override: installer.install.allow_directory_override,
    }
}

pub fn maintenance_state(
    installer: &Installer,
    ledger: &InstallLedger,
    scope: SelectedScope,
) -> MaintenanceState {
    let components = components(installer, None, Some(ledger));
    let groups = component_groups(installer, &components);
    MaintenanceState {
        installed_version: ledger.version.to_string(),
        components,
        groups,
        updates_enabled: installer.updates.is_some(),
        scope: convert::scope(scope),
        install_directory: persisted_location(Some(ledger)),
        health: InstallationHealth::Unknown,
    }
}

pub fn launchers(installer: &Installer) -> Vec<crate::Launchable> {
    let mut launchers: Vec<&zup_core::Launcher> = installer.launchers.iter().collect();
    launchers.sort_by_key(|launcher| launcher.location != zup_core::LauncherLocation::Menu);
    launchers
        .into_iter()
        .map(|launcher| crate::Launchable {
            target: zup_preset_protocol::LaunchTarget {
                name: launcher.name.to_string(),
            },
            component: launcher.component.as_ref().map(convert::component),
        })
        .collect()
}

pub fn capabilities(installer: &Installer, maintenance: bool) -> Capabilities {
    zup_artifact::preset::offers_for(installer, maintenance)
}

pub fn persisted_location(ledger: Option<&InstallLedger>) -> Option<String> {
    ledger
        .and_then(|ledger| ledger.install_directory.as_ref())
        .and_then(|path| zup_core::Template::parse(&path.to_string()).ok())
        .map(|template| template.to_string())
}
