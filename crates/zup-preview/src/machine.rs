//! The simulated machine: a scenario, and the host state machine it drives.
//!
//! Nothing here launches a process or touches a file. The child, the transport,
//! and the state directory belong to the simulator; this is the part a test or
//! a screenshot gallery can hold on its own and still reach every state a real
//! installation could be in.

use zup_core::SelectedScope;
use zup_runtime::{InstallOutcome, RuntimeEvent};
use zup_ui_host::{HostDecision, HostState, Launchable, Selection};
use zup_ui_protocol::{
    ComponentId, ComponentOption, InstallOptions, InstallScope, InstallationHealth, LaunchTarget,
    MaintenanceState, ProductIdentity, UiAction, UiCapabilities, UiCapability, UiSnapshot,
    UpdateState,
};

/// Which surface the simulated machine is presenting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    /// A machine that has never had this application.
    Install,
    /// A machine that has, and can therefore be modified, repaired or removed.
    Maintenance,
}

impl Surface {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Install => "install",
            Self::Maintenance => "maintenance",
        }
    }
}

/// What installing the application would put on a machine.
///
/// The simulator answers plans from this, so a preview shows sizes, locations
/// and categories of change that move with the choices the way a real plan's do.
#[derive(Debug, Clone)]
pub struct Footprint {
    /// The application's own files.
    pub application_bytes: u64,
    /// What each optional component adds.
    pub component_bytes: u64,
    /// What has to be downloaded first, for requirements that are missing.
    pub download_bytes: u64,
    /// How many application files the plan lists.
    pub files: usize,
    pub shortcuts: Vec<String>,
    /// Whether the application adds itself to PATH.
    pub path: bool,
    pub services: Vec<String>,
    pub protocols: Vec<String>,
    pub file_associations: Vec<String>,
    pub requirements: Vec<(String, zup_presentation::RequirementStatus, u64)>,
    /// A plan that cannot be worked out fails with this reason.
    pub plan_failure: Option<String>,
}

impl Default for Footprint {
    fn default() -> Self {
        Self {
            application_bytes: 84 * 1024 * 1024,
            component_bytes: 12 * 1024 * 1024,
            download_bytes: 0,
            files: 6,
            shortcuts: vec!["Start menu".into()],
            path: false,
            services: Vec::new(),
            protocols: Vec::new(),
            file_associations: Vec::new(),
            requirements: Vec::new(),
            plan_failure: None,
        }
    }
}

/// The machine being simulated.
///
/// Every field here is something a real machine has an answer for. Nothing is a
/// preview-only concept, because a state a preset can only meet in a preview is
/// a state its author will believe in and its users will never see.
#[derive(Debug, Clone)]
pub struct Scenario {
    pub product: ProductIdentity,
    pub surface: Surface,
    pub scopes: Vec<InstallScope>,
    pub scope: InstallScope,
    pub components: Vec<ComponentOption>,
    /// Declared groups. Empty means one implicit group of every component.
    pub groups: Vec<zup_ui_protocol::ComponentGroupOption>,
    /// A location the person chose instead of the default.
    pub install_directory: Option<String>,
    /// Where the default location resolves for a per-user install.
    pub user_directory: String,
    /// Where the default location resolves for a per-machine install.
    pub machine_directory: String,
    pub allow_directory_override: bool,
    pub existing_version: Option<String>,
    pub installed_version: String,
    pub updates_enabled: bool,
    /// What the update channel says, when the preset asks.
    pub update: UpdateState,
    /// What a repair found, when a repair has run.
    pub health: InstallationHealth,
    pub drift: Vec<String>,
    pub footprint: Footprint,
    /// The launcher the application declares, if any.
    pub launcher: Option<String>,
}

impl Default for Scenario {
    fn default() -> Self {
        Self {
            product: ProductIdentity {
                name: "Acme".into(),
                publisher: Some("Acme Inc".into()),
                version: "1.4.0".into(),
                description: Some("The Acme application.".into()),
            },
            surface: Surface::Install,
            scopes: vec![InstallScope::User, InstallScope::Machine],
            scope: InstallScope::User,
            components: Vec::new(),
            groups: Vec::new(),
            install_directory: None,
            user_directory: r"C:\Users\you\AppData\Local\Programs\Acme".into(),
            machine_directory: r"C:\Program Files\Acme".into(),
            allow_directory_override: true,
            existing_version: None,
            installed_version: "1.3.0".into(),
            updates_enabled: true,
            update: UpdateState::Idle,
            health: InstallationHealth::Unknown,
            drift: Vec::new(),
            footprint: Footprint::default(),
            launcher: Some("Acme".into()),
        }
    }
}

impl Scenario {
    /// The machine a real application would present, from its own compiled form.
    ///
    /// Every field is derived by the functions a real host uses to open a
    /// session, so the first thing a person sees is the application they are
    /// authoring rather than a demonstration. What a real machine has that this
    /// does not - an existing installation, a health reading, an update channel -
    /// is exactly what the controls are for, and it starts as unknown rather than
    /// as an answer nobody has.
    pub fn from_installer(installer: &zup_core::Installer) -> Self {
        let defaults = Self::default();
        let user = installer.install.directory.user.as_ref().or(installer
            .install
            .directory
            .machine
            .as_ref());
        let machine = installer.install.directory.machine.as_ref().or(installer
            .install
            .directory
            .user
            .as_ref());
        let name = installer.app.name.to_string();
        Self {
            product: zup_ui_host::product(installer),
            scopes: zup_ui_host::scopes(installer),
            scope: zup_ui_host::scope(zup_ui_host::default_scope(installer)),
            components: zup_ui_host::surface_components(installer, None, None),
            user_directory: user.map_or_else(
                || format!(r"C:\Users\you\AppData\Local\Programs\{name}"),
                |template| display_location(template, &name, SelectedScope::User),
            ),
            machine_directory: machine.map_or_else(
                || format!(r"C:\Program Files\{name}"),
                |template| display_location(template, &name, SelectedScope::Machine),
            ),
            allow_directory_override: installer.install.allow_directory_override,
            updates_enabled: installer.updates.is_some(),
            footprint: Footprint {
                shortcuts: installer
                    .launchers
                    .iter()
                    .map(|launcher| launcher.name.to_string())
                    .collect(),
                path: !installer.path.is_empty(),
                services: installer
                    .services
                    .iter()
                    .map(|service| service.name.to_string())
                    .collect(),
                protocols: installer
                    .protocols
                    .iter()
                    .map(|protocol| protocol.scheme.to_string())
                    .collect(),
                file_associations: installer
                    .file_associations
                    .iter()
                    .map(|association| association.extension.to_string())
                    .collect(),
                ..defaults.footprint.clone()
            },
            launcher: zup_ui_host::launchers(installer)
                .into_iter()
                .next()
                .map(|launcher| launcher.target.name),
            ..defaults
        }
    }

    /// What this machine can offer a preset.
    ///
    /// Derived over the same capabilities an installer's answer is derived from,
    /// so a preset is refused here for the same reason it would be refused there.
    pub fn capabilities(&self) -> UiCapabilities {
        let mut capabilities = UiCapabilities::new([
            UiCapability::Diagnostics,
            UiCapability::PlanPreview,
            UiCapability::InstallDirectory,
        ]);
        if !self.components.is_empty() {
            capabilities = capabilities.with(UiCapability::Components);
        }
        if self.updates_enabled {
            capabilities = capabilities.with(UiCapability::Updates);
        }
        if self.launcher.is_some() {
            capabilities = capabilities.with(UiCapability::Launch);
        }
        capabilities.with(UiCapability::Maintenance)
    }

    /// Where the default location resolves for a scope.
    fn default_directory(&self, scope: SelectedScope) -> &str {
        match scope {
            SelectedScope::User => &self.user_directory,
            SelectedScope::Machine => &self.machine_directory,
        }
    }

    /// Groups as the window will see them. An empty declaration is one implicit group.
    fn resolved_groups(&self) -> Vec<zup_ui_protocol::ComponentGroupOption> {
        if !self.groups.is_empty() {
            return self.groups.clone();
        }
        if self.components.is_empty() {
            return Vec::new();
        }
        vec![zup_ui_protocol::ComponentGroupOption {
            id: String::new(),
            label: None,
            description: None,
            prominence: zup_ui_protocol::ComponentProminence::Auto,
            selection: zup_ui_protocol::SelectionRequirement::Defaulted,
            components: self
                .components
                .iter()
                .map(|component| component.id.clone())
                .collect(),
        }]
    }

    /// The state a session on this machine opens in.
    fn host(&self) -> HostState {
        let host = match self.surface {
            Surface::Install => HostState::install(
                self.product.clone(),
                InstallOptions {
                    existing_version: self.existing_version.clone(),
                    scopes: self.scopes.clone(),
                    scope: self.scope,
                    components: self.components.clone(),
                    groups: self.resolved_groups(),
                    install_directory: self.install_directory.clone(),
                    allow_directory_override: self.allow_directory_override,
                },
                self.capabilities(),
            ),
            Surface::Maintenance => HostState::maintenance(
                self.product.clone(),
                MaintenanceState {
                    installed_version: self.installed_version.clone(),
                    // What a maintained machine has is what it was installed with.
                    components: self
                        .components
                        .iter()
                        .map(|component| ComponentOption {
                            installed: component.selected,
                            ..component.clone()
                        })
                        .collect(),
                    groups: self.resolved_groups(),
                    updates_enabled: self.updates_enabled,
                    scope: self.scope,
                    install_directory: Some(self.install_directory.clone().unwrap_or_else(|| {
                        self.default_directory(zup_ui_host::engine_scope(self.scope))
                            .to_owned()
                    })),
                    health: self.health.clone(),
                },
                self.capabilities(),
            ),
        };
        host.with_launchers(
            self.launcher
                .iter()
                .map(|name| Launchable {
                    target: LaunchTarget { name: name.clone() },
                    component: None,
                })
                .collect(),
        )
    }

    /// What the choices in `selection` would put on this machine.
    pub fn plan(&self, selection: &Selection) -> Result<zup_presentation::PlanPreview, String> {
        use zup_presentation::{ChangeGroup, ChangeKind, PlannedChange, ResourceCategory};

        if let Some(reason) = &self.footprint.plan_failure {
            return Err(reason.clone());
        }
        let directory = selection
            .install_directory
            .clone()
            .unwrap_or_else(|| self.default_directory(selection.scope).to_owned());
        let machine = selection.scope == SelectedScope::Machine;
        let maintenance = self.surface == Surface::Maintenance;
        let footprint = &self.footprint;
        let installed = |id: &ComponentId| {
            maintenance
                && self
                    .components
                    .iter()
                    .any(|component| &component.id == id && component.selected)
        };
        let change =
            |category, kind, label: String, location: Option<String>, bytes| PlannedChange {
                category,
                kind,
                label,
                location,
                scope: Some(selection.scope),
                requires_authorization: machine,
                estimated_bytes: bytes,
                component: None,
                technical_key: None,
            };
        let kept = if maintenance {
            ChangeKind::NoOp
        } else {
            ChangeKind::Create
        };
        let mut groups = Vec::new();

        let stem = self.product.name.to_ascii_lowercase().replace(' ', "-");
        let per_file = footprint.application_bytes / footprint.files.max(1) as u64;
        let mut files: Vec<PlannedChange> = (0..footprint.files)
            .map(|index| {
                let name = match index {
                    0 => format!("{stem}.exe"),
                    1 => format!("{stem}-core.dll"),
                    2 => "resources.pak".to_owned(),
                    n => format!("lib\\module-{n}.dll"),
                };
                let location = format!("{directory}\\{name}");
                change(
                    ResourceCategory::Files,
                    kept,
                    name,
                    Some(location),
                    per_file,
                )
            })
            .collect();
        for component in self
            .components
            .iter()
            .filter(|component| !component.required)
        {
            let id = zup_core::ComponentId::new(component.id.as_str()).expect("a component id");
            let chosen = selection.components.contains(&id);
            let kind = match (chosen, installed(&component.id)) {
                (true, true) => ChangeKind::NoOp,
                (true, false) => ChangeKind::Create,
                (false, true) => ChangeKind::Remove,
                (false, false) => continue,
            };
            let mut entry = change(
                ResourceCategory::Files,
                kind,
                format!("{}\\", component.id),
                Some(format!("{directory}\\{}", component.id)),
                footprint.component_bytes,
            );
            entry.component = Some(id);
            files.push(entry);
        }
        groups.push(ChangeGroup {
            category: ResourceCategory::Files,
            title: ResourceCategory::Files.title().into(),
            changes: files,
        });

        let mut push = |category: ResourceCategory, changes: Vec<PlannedChange>| {
            if !changes.is_empty() {
                groups.push(ChangeGroup {
                    category,
                    title: category.title().into(),
                    changes,
                });
            }
        };
        push(
            ResourceCategory::Launchers,
            footprint
                .shortcuts
                .iter()
                .map(|name| {
                    change(
                        ResourceCategory::Launchers,
                        kept,
                        name.clone(),
                        Some("Start menu".into()),
                        0,
                    )
                })
                .collect(),
        );
        push(
            ResourceCategory::Path,
            footprint
                .path
                .then(|| {
                    change(
                        ResourceCategory::Path,
                        kept,
                        format!("{directory}\\bin"),
                        None,
                        0,
                    )
                })
                .into_iter()
                .collect(),
        );
        push(
            ResourceCategory::Services,
            footprint
                .services
                .iter()
                .map(|name| change(ResourceCategory::Services, kept, name.clone(), None, 0))
                .collect(),
        );
        push(
            ResourceCategory::Protocols,
            footprint
                .protocols
                .iter()
                .map(|scheme| {
                    change(
                        ResourceCategory::Protocols,
                        kept,
                        format!("{scheme}:"),
                        None,
                        0,
                    )
                })
                .collect(),
        );
        push(
            ResourceCategory::FileAssociations,
            footprint
                .file_associations
                .iter()
                .map(|extension| {
                    change(
                        ResourceCategory::FileAssociations,
                        kept,
                        extension.clone(),
                        None,
                        0,
                    )
                })
                .collect(),
        );
        push(
            ResourceCategory::AppsFeatures,
            vec![change(
                ResourceCategory::AppsFeatures,
                if maintenance {
                    ChangeKind::Update
                } else {
                    ChangeKind::Create
                },
                format!("{} {}", self.product.name, self.product.version),
                None,
                0,
            )],
        );
        push(
            ResourceCategory::Maintenance,
            vec![change(
                ResourceCategory::Maintenance,
                kept,
                "Repair and uninstall".into(),
                None,
                2 * 1024 * 1024,
            )],
        );

        let estimated_bytes = groups
            .iter()
            .flat_map(|group| &group.changes)
            .filter(|change| change.kind != ChangeKind::Remove)
            .map(|change| change.estimated_bytes)
            .sum();
        let requirements = footprint
            .requirements
            .iter()
            .map(
                |(name, status, bytes)| zup_presentation::RequirementPresentation {
                    id: name.to_ascii_lowercase().replace(' ', "-"),
                    name: name.clone(),
                    status: *status,
                    estimated_bytes: *bytes,
                    shared: true,
                },
            )
            .collect();
        Ok(zup_presentation::PlanPreview {
            application: self.product.name.clone(),
            version: self.product.version.clone(),
            scope: selection.scope,
            install_directory: directory,
            selected_components: selection.components.clone(),
            estimated_bytes,
            download_bytes: footprint.download_bytes,
            requires_authorization: machine,
            groups,
            requirements,
        })
    }
}

/// A location template as a person would read it on a typical machine.
///
/// The preview has no machine to resolve against, and showing `${location.*}`
/// to somebody judging how a window reads would be showing them a string no
/// user ever sees.
fn display_location(template: &zup_core::Template, name: &str, scope: SelectedScope) -> String {
    use zup_core::{InstallLocation, Variable, VariableValue};

    let resolved = template.substitute(|variable| {
        let text = match variable {
            Variable::AppName => name.to_owned(),
            Variable::AppId => name.to_ascii_lowercase(),
            Variable::AppVersion => "1.0.0".to_owned(),
            Variable::Install => return None,
            Variable::Location(location) => match (location, scope) {
                (InstallLocation::Programs, SelectedScope::User) => {
                    r"C:\Users\you\AppData\Local\Programs".to_owned()
                }
                (InstallLocation::Programs, SelectedScope::Machine) => {
                    r"C:\Program Files".to_owned()
                }
                (InstallLocation::UserData, _) => r"C:\Users\you\AppData\Local".to_owned(),
                (InstallLocation::SharedData, _) => r"C:\ProgramData".to_owned(),
                (InstallLocation::Menu, _) => {
                    r"C:\Users\you\AppData\Roaming\Microsoft\Windows\Start Menu\Programs".to_owned()
                }
                (InstallLocation::Desktop, _) => r"C:\Users\you\Desktop".to_owned(),
            },
        };
        Some(VariableValue::Literal(text))
    });
    resolved
        .as_literal()
        .map_or_else(|| template.to_string(), |text| text.replace('/', "\\"))
}

/// A scenario and the host state machine it drives, with no process attached.
pub struct Machine {
    scenario: Scenario,
    host: HostState,
}

impl Machine {
    /// Open a session on this machine.
    pub fn new(scenario: Scenario) -> Self {
        let host = scenario.host();
        let mut machine = Self { scenario, host };
        machine.replan();
        machine
    }

    /// Reopen the session on a different machine.
    pub fn reopen(&mut self, scenario: &Scenario) {
        *self = Self::new(scenario.clone());
    }

    pub fn snapshot(&self) -> &UiSnapshot {
        self.host.snapshot()
    }

    pub fn scenario(&self) -> &Scenario {
        &self.scenario
    }

    pub fn capabilities(&self) -> &UiCapabilities {
        self.host.capabilities()
    }

    /// Feed one action to the state machine, answering any plan it asks for.
    pub fn act(&mut self, action: UiAction) -> HostDecision {
        let decision = self.host.accept(action);
        if let HostDecision::Plan(selection) = &decision {
            self.answer(selection);
        }
        decision
    }

    /// Report an engine event to the state machine.
    pub fn observe(&mut self, event: &RuntimeEvent) {
        self.host.observe(event);
    }

    /// Report the outcome of a finished transaction.
    pub fn finish_with(&mut self, outcome: &InstallOutcome) {
        self.host.finish_with(outcome);
    }

    /// Record what the update channel says.
    pub fn set_update(&mut self, state: UpdateState) {
        self.host.set_update(Some("stable".into()), state);
    }

    /// Record the resources a repair found drifted.
    pub fn set_drift(&mut self, resources: Vec<String>) {
        self.host.set_repair_drift(resources);
    }

    /// Record a failure reported before a transaction started.
    pub fn fail(&mut self, message: &str) {
        self.host.fail(message.to_owned(), false);
    }

    fn replan(&mut self) {
        if let Some(selection) = self.host.plan_request() {
            self.answer(&selection);
        }
    }

    fn answer(&mut self, selection: &Selection) {
        match self.scenario.plan(selection) {
            Ok(plan) => self.host.set_plan(plan),
            Err(reason) => self.host.plan_failed(reason),
        }
    }
}
