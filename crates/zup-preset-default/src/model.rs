//! What the window says, as a pure function of one snapshot.
//!
//! Every word a person reads and every action a control would ask for is
//! decided here, without GPUI, so the rules can be tested directly. A host that
//! publishes the wrong state is a bug in the host; a window that offers the
//! wrong thing for a state is a bug here, and only this half is visible without
//! a display.

use zup_ui_sdk::prelude::*;

/// The one screen a snapshot belongs to.
///
/// These follow the lifecycle, not a sequence: there is no step a person
/// navigates to, only the state the installation is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    /// Deciding what to install, and where.
    Install,
    /// Looking after an installation that exists.
    Maintenance,
    /// An operation is running.
    Operation,
    /// The last operation committed.
    Outcome,
    /// Applications hold files the operation needs.
    Blocked,
    /// The last operation failed, or left work to reconcile.
    Problem,
}

impl Screen {
    pub fn of(snapshot: &UiSnapshot) -> Self {
        match &snapshot.state {
            UiState::Options => Self::Install,
            UiState::Maintenance | UiState::ConfirmUninstall => Self::Maintenance,
            UiState::Running | UiState::WaitingForSafeCancellation => Self::Operation,
            UiState::Succeeded => Self::Outcome,
            UiState::Blocked { .. } => Self::Blocked,
            UiState::Failed | UiState::RecoveryRequired => Self::Problem,
        }
    }
}

/// The operation a snapshot is about.
pub fn operation(snapshot: &UiSnapshot) -> OperationKind {
    snapshot
        .operation
        .unwrap_or_else(|| match &snapshot.surface {
            UiSurface::Install(options) if options.existing_version.is_some() => {
                OperationKind::Upgrade
            }
            UiSurface::Install(_) => OperationKind::Install,
            UiSurface::Maintenance(_) => OperationKind::Modify,
        })
}

/// A byte count as a person usually reads a file size.
pub fn size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} bytes");
    }
    let mut value = bytes as f64 / 1024.;
    let mut unit = 0;
    while value >= 1024. && unit < UNITS.len() - 1 {
        value /= 1024.;
        unit += 1;
    }
    if value < 10. {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.0} {}", UNITS[unit])
    }
}

// -- Install ----------------------------------------------------------------

/// What one summary fact is about, which decides its icon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FactKind {
    Size,
    Download,
    Scope(InstallScope),
    Approval { required: bool },
}

/// One thing a person should know before pressing the button.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fact {
    pub kind: FactKind,
    pub text: String,
}

/// Whether the current choices need an administrator's approval.
///
/// The plan is the authority once there is one. Before it arrives, installing
/// for everyone is the choice that needs approval on this platform, and saying
/// so early is better than surprising somebody at the prompt.
pub fn needs_approval(snapshot: &UiSnapshot) -> bool {
    match snapshot.plan.latest() {
        Some(plan) if plan.scope == snapshot.surface.scope() => plan.requires_authorization,
        _ => snapshot.surface.scope() == InstallScope::Machine,
    }
}

/// The facts the install summary shows, in reading order.
pub fn summary(snapshot: &UiSnapshot) -> Vec<Fact> {
    let mut facts = Vec::new();
    match snapshot.plan.latest() {
        Some(plan) if plan.estimated_bytes > 0 => facts.push(Fact {
            kind: FactKind::Size,
            text: size(plan.estimated_bytes),
        }),
        None if snapshot.plan.is_computing() => facts.push(Fact {
            kind: FactKind::Size,
            text: "Working out the size…".into(),
        }),
        _ => {}
    }
    if let Some(plan) = snapshot.plan.latest()
        && plan.download_bytes > 0
    {
        facts.push(Fact {
            kind: FactKind::Download,
            text: format!("{} to download", size(plan.download_bytes)),
        });
    }
    let scope = snapshot.surface.scope();
    facts.push(Fact {
        kind: FactKind::Scope(scope),
        text: match scope {
            InstallScope::User => "For your account".into(),
            InstallScope::Machine => "For everyone on this computer".into(),
        },
    });
    let required = needs_approval(snapshot);
    facts.push(Fact {
        kind: FactKind::Approval { required },
        text: if required {
            "Admin approval required".into()
        } else {
            "No admin approval".into()
        },
    });
    facts
}

/// The commit bar's facts. Every fact stays visible when the window is resized;
/// the bar wraps instead of dropping one.
pub fn commit_facts(snapshot: &UiSnapshot) -> Vec<Fact> {
    summary(snapshot)
}

/// Where the installation goes, as the window shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    /// The concrete path, when one is known.
    pub path: Option<String>,
    /// Whether the person chose it, rather than the application.
    pub custom: bool,
    /// Whether a person may choose another.
    pub changeable: bool,
}

pub fn location(snapshot: &UiSnapshot) -> Location {
    let custom = snapshot.surface.install_directory().map(str::to_owned);
    let resolved = snapshot
        .plan
        .latest()
        .map(|plan| plan.install_directory.clone())
        .filter(|path| !path.is_empty());
    Location {
        custom: custom.is_some() && matches!(snapshot.surface, UiSurface::Install(_)),
        path: custom.or(resolved),
        changeable: snapshot.surface.allows_directory_override(),
    }
}

/// The folder an installation goes into when a person picks `chosen`.
///
/// A person who picks `D:\Apps` means "under D:\Apps", not "spread my files
/// across D:\Apps", so the application's own folder is added unless they
/// already picked one by that name.
pub fn folder_for(chosen: &std::path::Path, product: &str) -> String {
    let named = chosen
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case(product));
    let folder = if named || product.is_empty() {
        chosen.to_path_buf()
    } else {
        chosen.join(product)
    };
    folder.to_string_lossy().into_owned()
}

/// One scope a person can choose between.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeChoice {
    pub scope: InstallScope,
    pub title: &'static str,
    pub detail: &'static str,
    pub needs_approval: bool,
}

/// The scopes on offer, or nothing when there is no choice to make.
pub fn scope_choices(snapshot: &UiSnapshot) -> Vec<ScopeChoice> {
    let UiSurface::Install(options) = &snapshot.surface else {
        return Vec::new();
    };
    if options.scopes.len() < 2 {
        return Vec::new();
    }
    options
        .scopes
        .iter()
        .map(|scope| match scope {
            InstallScope::User => ScopeChoice {
                scope: *scope,
                title: "Just me",
                detail: "Only your account",
                needs_approval: false,
            },
            InstallScope::Machine => ScopeChoice {
                scope: *scope,
                title: "Everyone on this computer",
                detail: "Every account on this computer · Admin approval required",
                needs_approval: true,
            },
        })
        .collect()
}

/// What a component row shows and does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComponentKind {
    /// Always included; drawn as included, not as a disabled control.
    Required,
    /// A person's choice.
    Optional { selected: bool },
}

/// What applying the current choices would do to one installed component.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pending {
    Add,
    Remove,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentRow {
    pub id: ComponentId,
    pub name: String,
    pub description: Option<String>,
    pub kind: ComponentKind,
    /// On an existing installation, what the choice would change.
    pub pending: Option<Pending>,
}

impl ComponentRow {
    /// The action a press asks for: the opposite of what the row shows.
    pub fn toggle(&self) -> Option<UiAction> {
        match self.kind {
            ComponentKind::Required => None,
            ComponentKind::Optional { selected } => Some(UiAction::SetComponent {
                component: self.id.clone(),
                selected: !selected,
            }),
        }
    }
}

pub fn component_rows(surface: &UiSurface) -> Vec<ComponentRow> {
    let maintenance = matches!(surface, UiSurface::Maintenance(_));
    surface
        .components()
        .iter()
        .map(|component| ComponentRow {
            id: component.id.clone(),
            name: component.name.clone(),
            description: component.description.clone(),
            kind: if component.required {
                ComponentKind::Required
            } else {
                ComponentKind::Optional {
                    selected: component.selected,
                }
            },
            pending: match (maintenance, component.installed, component.selected) {
                (true, false, true) => Some(Pending::Add),
                (true, true, false) => Some(Pending::Remove),
                _ => None,
            },
        })
        .collect()
}

/// Whether any component is a person's choice.
pub fn has_optional_components(surface: &UiSurface) -> bool {
    surface
        .components()
        .iter()
        .any(|component| !component.required)
}

/// What the main button on the install screen says.
pub fn install_label(snapshot: &UiSnapshot) -> &'static str {
    match &snapshot.surface {
        UiSurface::Install(options) if options.existing_version.is_some() => "Upgrade",
        _ => "Install",
    }
}

/// The line under the product's name on the install screen.
pub fn install_subtitle(snapshot: &UiSnapshot) -> Option<String> {
    match &snapshot.surface {
        UiSurface::Install(options) => options
            .existing_version
            .as_ref()
            .map(|installed| format!("Replaces version {installed}, which is installed now")),
        UiSurface::Maintenance(_) => None,
    }
}

/// Who published the product and which version it is, in one line.
pub fn byline(product: &ProductIdentity) -> String {
    match &product.publisher {
        Some(publisher) => format!("{publisher} · Version {}", product.version),
        None => format!("Version {}", product.version),
    }
}

// -- Operation --------------------------------------------------------------

/// The operation's title, as the progress screen says it.
pub fn operation_title(kind: OperationKind, product: &str) -> String {
    match kind {
        OperationKind::Install => format!("Installing {product}"),
        OperationKind::Upgrade => format!("Upgrading {product}"),
        OperationKind::Modify => format!("Updating {product}"),
        OperationKind::Repair => format!("Repairing {product}"),
        OperationKind::Uninstall => format!("Uninstalling {product}"),
    }
}

/// What the current phase is doing, in words.
pub fn phase_label(phase: OperationPhase, kind: OperationKind) -> &'static str {
    match (phase, kind) {
        (OperationPhase::Prepare, OperationKind::Uninstall) => "Preparing to uninstall",
        (OperationPhase::Prepare, _) => "Preparing",
        (OperationPhase::Download, _) => "Downloading required files",
        (OperationPhase::Verify, _) => "Verifying the download",
        (OperationPhase::Files, OperationKind::Uninstall) => "Removing files",
        (OperationPhase::Files, OperationKind::Repair) => "Restoring files",
        (OperationPhase::Files, _) => "Copying files",
        (OperationPhase::System, OperationKind::Uninstall) => "Removing system integration",
        (OperationPhase::System, _) => "Configuring the system",
        (OperationPhase::Finish, _) => "Finishing up",
    }
}

/// How the progress screen reads right now.
#[derive(Debug, Clone, PartialEq)]
pub struct Progress {
    pub title: String,
    pub phase: String,
    /// What the engine says it is doing, when it adds something to the phase.
    pub activity: Option<String>,
    /// 0..=1, or `None` while the work is not countable.
    pub fraction: Option<f32>,
    /// How much of a download is done, when the phase is one.
    pub amount: Option<String>,
    /// Whether a stop was asked for and the engine is on its way to a safe point.
    pub stopping: bool,
}

pub fn progress(snapshot: &UiSnapshot) -> Progress {
    let kind = operation(snapshot);
    let stopping = snapshot.state == UiState::WaitingForSafeCancellation;
    let report = snapshot.progress.as_ref();
    let phase = report.map_or(OperationPhase::Prepare, |progress| progress.phase);
    let phase_text = phase_label(phase, kind);
    let activity = report
        .map(|progress| {
            progress
                .label
                .trim()
                .trim_end_matches('…')
                .trim()
                .to_owned()
        })
        .filter(|label| !label.is_empty() && !label.eq_ignore_ascii_case(phase_text));
    let fraction = report
        .filter(|progress| progress.total > 0)
        .map(|progress| progress.completed.min(progress.total) as f32 / progress.total as f32);
    let amount = report
        .filter(|progress| progress.phase == OperationPhase::Download && progress.total > 0)
        .map(|progress| format!("{} of {}", size(progress.completed), size(progress.total)));
    Progress {
        title: if stopping {
            "Stopping…".into()
        } else {
            operation_title(kind, &snapshot.product.name)
        },
        phase: if stopping {
            "Waiting for a safe point to stop".into()
        } else {
            phase_text.into()
        },
        activity: if stopping {
            Some("Nothing will be left half-finished.".into())
        } else {
            activity
        },
        fraction,
        amount,
        stopping,
    }
}

// -- Outcome ----------------------------------------------------------------

/// How a committed operation reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub title: String,
    pub detail: String,
    /// What the person should know that the title does not say.
    pub note: Option<String>,
    /// Resources a repair left as they were, because they had been changed.
    pub left_alone: Vec<String>,
    /// The launcher the host can start, when it can.
    pub launch: Option<String>,
    pub removed: bool,
}

pub fn outcome(snapshot: &UiSnapshot) -> Outcome {
    let name = &snapshot.product.name;
    let version = &snapshot.product.version;
    let audience = match snapshot.surface.scope() {
        InstallScope::User => "for your account",
        InstallScope::Machine => "for everyone on this computer",
    };
    let launch = snapshot.launch.as_ref().map(|target| target.name.clone());
    let base = Outcome {
        title: String::new(),
        detail: String::new(),
        note: None,
        left_alone: Vec::new(),
        launch,
        removed: false,
    };
    match operation(snapshot) {
        OperationKind::Install => Outcome {
            title: format!("{name} is ready"),
            detail: format!("Version {version} was installed {audience}."),
            ..base
        },
        OperationKind::Upgrade => Outcome {
            title: format!("{name} is up to date"),
            detail: match &snapshot.surface {
                UiSurface::Install(InstallOptions {
                    existing_version: Some(previous),
                    ..
                }) => format!("Upgraded from version {previous} to {version}."),
                _ => format!("Upgraded to version {version}."),
            },
            ..base
        },
        OperationKind::Modify => Outcome {
            title: "Your changes were applied".into(),
            detail: format!("{name} now has the components you chose."),
            ..base
        },
        OperationKind::Repair => Outcome {
            title: format!("{name} was repaired"),
            detail: "The files and settings setup manages were restored.".into(),
            note: (!snapshot.repair_drift.is_empty())
                .then(|| "These were changed outside setup, so they were left as they are:".into()),
            left_alone: snapshot.repair_drift.clone(),
            ..base
        },
        OperationKind::Uninstall => Outcome {
            title: format!("{name} was removed"),
            detail: "The app and everything setup added to this computer are gone.".into(),
            note: Some(format!(
                "Files {name} created outside its install folder, such as your documents and \
                 settings, were left in place."
            )),
            launch: None,
            removed: true,
            ..base
        },
    }
}

// -- Maintenance ------------------------------------------------------------

/// What the installation's health says to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Health {
    /// Nothing has inspected it in this session.
    Unknown,
    Healthy,
    /// These managed resources no longer match what was installed.
    Drifted(Vec<String>),
}

pub fn health(state: &MaintenanceState) -> Health {
    match &state.health {
        InstallationHealth::Unknown => Health::Unknown,
        InstallationHealth::UpToDate => Health::Healthy,
        InstallationHealth::Drifted { resources } if resources.is_empty() => Health::Healthy,
        InstallationHealth::Drifted { resources } => Health::Drifted(resources.clone()),
    }
}

/// How serious an update row is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Neutral,
    Positive,
    Attention,
    Negative,
}

/// The update situation, as one row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateRow {
    pub title: String,
    pub detail: String,
    pub tone: Tone,
    /// The button, and whether it is the one this row is about.
    pub action: Option<(&'static str, bool)>,
    pub busy: bool,
}

pub fn update_row(snapshot: &UiSnapshot) -> Option<UpdateRow> {
    if !snapshot.surface.updates_enabled() {
        return None;
    }
    let state = snapshot
        .update
        .as_ref()
        .map_or(&UpdateState::Idle, |update| &update.state);
    let channel = snapshot
        .update
        .as_ref()
        .and_then(|update| update.channel.as_deref());
    Some(match state {
        UpdateState::Idle => UpdateRow {
            title: "Check for updates".into(),
            detail: match channel {
                Some(channel) => {
                    format!("See whether a newer version is on the {channel} channel.")
                }
                None => "See whether a newer version is available.".into(),
            },
            tone: Tone::Neutral,
            action: Some(("Check", false)),
            busy: false,
        },
        UpdateState::Checking { detail } => UpdateRow {
            title: "Checking for updates…".into(),
            detail: detail.clone(),
            tone: Tone::Neutral,
            action: None,
            busy: true,
        },
        UpdateState::Installing { detail } => UpdateRow {
            title: "Installing the update…".into(),
            detail: detail.clone(),
            tone: Tone::Neutral,
            action: None,
            busy: true,
        },
        UpdateState::UpToDate { current } => UpdateRow {
            title: "You're up to date".into(),
            detail: format!("Version {current} is the newest."),
            tone: Tone::Positive,
            action: Some(("Check again", false)),
            busy: false,
        },
        UpdateState::Available { current, available } => UpdateRow {
            title: format!("Version {available} is available"),
            detail: format!("You have version {current}."),
            tone: Tone::Attention,
            action: Some(("Update", true)),
            busy: false,
        },
        UpdateState::Failed { message } => UpdateRow {
            title: "Couldn't check for updates".into(),
            detail: message.clone(),
            tone: Tone::Negative,
            action: Some(("Try again", false)),
            busy: false,
        },
    })
}

/// Which components applying the current choices would add and remove.
pub fn pending_changes(surface: &UiSurface) -> (Vec<String>, Vec<String>) {
    let rows = component_rows(surface);
    let named = |wanted: Pending| {
        rows.iter()
            .filter(|row| row.pending == Some(wanted))
            .map(|row| row.name.clone())
            .collect::<Vec<_>>()
    };
    (named(Pending::Add), named(Pending::Remove))
}

/// The sentence that says what applying the changes would do.
pub fn pending_sentence(surface: &UiSurface) -> Option<String> {
    let (added, removed) = pending_changes(surface);
    let list = |names: &[String]| match names {
        [] => String::new(),
        [one] => one.clone(),
        [first @ .., last] => format!("{} and {last}", first.join(", ")),
    };
    match (added.is_empty(), removed.is_empty()) {
        (true, true) => None,
        (false, true) => Some(format!("Adds {}.", list(&added))),
        (true, false) => Some(format!("Removes {}.", list(&removed))),
        (false, false) => Some(format!(
            "Adds {} and removes {}.",
            list(&added),
            list(&removed)
        )),
    }
}

/// Who an installation is for, as a phrase.
pub fn audience(scope: InstallScope) -> &'static str {
    match scope {
        InstallScope::User => "Installed for your account",
        InstallScope::Machine => "Installed for everyone on this computer",
    }
}

// -- Problems ---------------------------------------------------------------

/// How serious a problem is, which decides how loudly it is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// Nothing was changed; trying again is safe.
    Failure,
    /// The installation is between two states until it is reconciled.
    Recovery,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub severity: Severity,
    pub kind: DiagnosticKind,
    pub title: String,
    pub meaning: String,
    pub recovery: String,
    pub technical: Option<String>,
}

pub fn problem(snapshot: &UiSnapshot) -> Problem {
    let severity = if snapshot.state == UiState::RecoveryRequired {
        Severity::Recovery
    } else {
        Severity::Failure
    };
    match &snapshot.diagnostic {
        Some(diagnostic) => Problem {
            severity,
            kind: diagnostic.kind,
            title: diagnostic.title.clone(),
            meaning: diagnostic.meaning.clone(),
            recovery: diagnostic.recovery.clone(),
            technical: diagnostic
                .technical_details
                .clone()
                .filter(|details| !details.trim().is_empty()),
        },
        None => match severity {
            Severity::Recovery => Problem {
                severity,
                kind: DiagnosticKind::Recovery,
                title: "The last change didn't finish".into(),
                meaning: "Setup was interrupted while it was changing this computer.".into(),
                recovery: "Let setup finish restoring it before you make other changes.".into(),
                technical: None,
            },
            Severity::Failure => Problem {
                severity,
                kind: DiagnosticKind::Unknown,
                title: "Setup couldn't finish".into(),
                meaning: "Nothing was changed on this computer.".into(),
                recovery: "Try again.".into(),
                technical: None,
            },
        },
    }
}

/// A blocked operation, as the person can act on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blocked {
    pub title: String,
    pub detail: String,
    pub apps: Vec<String>,
}

pub fn blocked(snapshot: &UiSnapshot) -> Blocked {
    let UiState::Blocked { blockers } = &snapshot.state else {
        return Blocked {
            title: String::new(),
            detail: String::new(),
            apps: Vec::new(),
        };
    };
    let apps: Vec<String> = blockers
        .iter()
        .map(|blocker| blocker.trim().to_owned())
        .filter(|blocker| !blocker.is_empty())
        .collect();
    let verb = match operation(snapshot) {
        OperationKind::Uninstall => "remove",
        _ => "update",
    };
    Blocked {
        title: match apps.as_slice() {
            [] => "Close open apps to continue".into(),
            [one] => format!("Close {} to continue", app_name(one)),
            _ => "Close these apps to continue".into(),
        },
        detail: match apps.len() {
            0 => format!("An app is using files setup needs to {verb}."),
            1 => format!(
                "It's using files setup needs to {verb}. Save your work and close it, then try again."
            ),
            _ => format!(
                "They're using files setup needs to {verb}. Save your work and close them, then try again."
            ),
        },
        apps,
    }
}

/// The readable part of a blocker, without the process detail after it.
pub fn app_name(blocker: &str) -> &str {
    blocker
        .split_once(" (")
        .map_or(blocker, |(name, _)| name)
        .trim()
}

/// The process detail of a blocker, when it has one.
pub fn app_detail(blocker: &str) -> Option<&str> {
    blocker
        .split_once(" (")
        .map(|(_, rest)| rest.trim_end_matches(')').trim())
        .filter(|detail| !detail.is_empty())
}

// -- Plan -------------------------------------------------------------------

/// One resource category, as a person reads it.
pub fn category_title(category: ResourceCategory) -> &'static str {
    match category {
        ResourceCategory::Files => "Files",
        ResourceCategory::Launchers => "Shortcuts",
        ResourceCategory::Path => "Command-line PATH",
        ResourceCategory::Services => "Services",
        ResourceCategory::Protocols => "Web links",
        ResourceCategory::FileAssociations => "File types",
        ResourceCategory::AppsFeatures => "Apps & features",
        ResourceCategory::Maintenance => "Uninstall support",
        ResourceCategory::Prerequisites => "Required software",
        ResourceCategory::Other => "Other changes",
    }
}

/// Why a category matters, for somebody who has not met the term.
pub fn category_about(category: ResourceCategory) -> &'static str {
    match category {
        ResourceCategory::Files => "The application's own files.",
        ResourceCategory::Launchers => "Ways to open it from the menu or the desktop.",
        ResourceCategory::Path => "Lets a terminal find its commands.",
        ResourceCategory::Services => "Programs that keep running in the background.",
        ResourceCategory::Protocols => "Links this application is registered to open.",
        ResourceCategory::FileAssociations => "Files that open with this application.",
        ResourceCategory::AppsFeatures => "Where it appears in the list of installed applications.",
        ResourceCategory::Maintenance => {
            "What setup keeps so the application can be repaired or removed."
        }
        ResourceCategory::Prerequisites => "Other software this application needs.",
        ResourceCategory::Other => "Other changes to this computer.",
    }
}

/// What a change does, as a verb a person reads.
pub fn change_label(kind: ChangeKind) -> &'static str {
    match kind {
        ChangeKind::Create => "Added",
        ChangeKind::Update => "Updated",
        ChangeKind::Remove => "Removed",
        ChangeKind::NoChange => "Unchanged",
        ChangeKind::Drifted => "Changed outside setup",
        ChangeKind::Conflict => "Conflict",
    }
}

/// How many changes of each kind a group holds, in a stable order.
pub fn change_counts(group: &ChangeGroup) -> Vec<(ChangeKind, usize)> {
    [
        ChangeKind::Create,
        ChangeKind::Update,
        ChangeKind::Remove,
        ChangeKind::Conflict,
        ChangeKind::Drifted,
        ChangeKind::NoChange,
    ]
    .into_iter()
    .map(|kind| {
        (
            kind,
            group
                .changes
                .iter()
                .filter(|change| change.kind == kind)
                .count(),
        )
    })
    .filter(|(_, count)| *count > 0)
    .collect()
}

/// A group's counts as one line: "12 added · 1 removed".
pub fn counts_line(group: &ChangeGroup) -> String {
    change_counts(group)
        .into_iter()
        .map(|(kind, count)| match kind {
            ChangeKind::Create => format!("{count} added"),
            ChangeKind::Update => format!("{count} updated"),
            ChangeKind::Remove => format!("{count} removed"),
            ChangeKind::Conflict => format!("{count} in conflict"),
            ChangeKind::Drifted => format!("{count} changed outside setup"),
            ChangeKind::NoChange => format!("{count} unchanged"),
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

/// The plan's context, compressed so the sheet can start on the changes.
///
/// The same facts sit on the main screen as a decision summary. Here they are
/// one line of context, because the sheet has to make sense on its own without
/// repeating that summary as a form.
pub fn review_context(snapshot: &UiSnapshot) -> (String, Option<String>) {
    let line = summary(snapshot)
        .into_iter()
        .map(|fact| fact.text)
        .collect::<Vec<_>>()
        .join(" · ");
    let location = snapshot
        .plan
        .latest()
        .map(|plan| plan.install_directory.clone())
        .filter(|path| !path.is_empty())
        .or_else(|| location(snapshot).path);
    (line, location)
}

/// Whether a group changes anything, rather than only confirming what is there.
pub fn group_changes_anything(group: &ChangeGroup) -> bool {
    group
        .changes
        .iter()
        .any(|change| change.kind != ChangeKind::NoChange)
}

/// How a requirement stands, in words.
pub fn requirement_status(status: RequirementStatus) -> &'static str {
    match status {
        RequirementStatus::Satisfied => "Already installed",
        RequirementStatus::Missing => "Will be installed",
        RequirementStatus::Unknown => "Checked during install",
    }
}
