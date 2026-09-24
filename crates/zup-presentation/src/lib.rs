#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};
use zup_core::{ComponentId, ResourceKey, SelectedScope};
use zup_exec::{
    ExecutionPlan, FileOperationKind, FileTypeOperationKind, PathOperationKind,
    ProtocolOperationKind, ServiceOperationKind, ShortcutOperationKind,
};
use zup_plan::InstallPlan;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceCategory {
    Files,
    Shortcuts,
    Path,
    Services,
    Protocols,
    FileAssociations,
    AppsFeatures,
    Maintenance,
    Other,
}

impl ResourceCategory {
    pub const fn title(self) -> &'static str {
        match self {
            Self::Files => "Files",
            Self::Shortcuts => "Shortcuts",
            Self::Path => "PATH",
            Self::Services => "Services",
            Self::Protocols => "Protocols",
            Self::FileAssociations => "File associations",
            Self::AppsFeatures => "Apps & Features",
            Self::Maintenance => "Maintenance",
            Self::Other => "Other",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Create,
    Update,
    Remove,
    NoOp,
    Drift,
    Conflict,
}

impl ChangeKind {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Create => "Create",
            Self::Update => "Update",
            Self::Remove => "Remove",
            Self::NoOp => "No change",
            Self::Drift => "Changed outside zup",
            Self::Conflict => "Needs attention",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedChange {
    pub category: ResourceCategory,
    pub kind: ChangeKind,
    pub label: String,
    pub location: Option<String>,
    pub scope: Option<SelectedScope>,
    pub requires_elevation: bool,
    pub estimated_bytes: u64,
    pub component: Option<ComponentId>,
    pub technical_key: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeGroup {
    pub category: ResourceCategory,
    pub title: String,
    pub changes: Vec<PlannedChange>,
}

impl ChangeGroup {
    pub fn total_bytes(&self) -> u64 {
        self.changes.iter().fold(0u64, |total, change| {
            total.saturating_add(change.estimated_bytes)
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanPreview {
    pub application: String,
    pub version: String,
    pub scope: SelectedScope,
    pub install_directory: String,
    pub selected_components: Vec<ComponentId>,
    pub estimated_bytes: u64,
    pub requires_elevation: bool,
    pub groups: Vec<ChangeGroup>,
}

impl PlanPreview {
    pub fn from_install_plan(plan: &InstallPlan) -> Self {
        let mut groups = BTreeMap::<ResourceCategory, Vec<PlannedChange>>::new();
        for file in &plan.files {
            let category = if matches!(file.key, ResourceKey::Maintenance { .. }) {
                ResourceCategory::Maintenance
            } else {
                ResourceCategory::Files
            };
            groups.entry(category).or_default().push(PlannedChange {
                category,
                kind: ChangeKind::Create,
                label: file.source_relative.to_string(),
                location: Some(file.destination.to_string()),
                scope: Some(plan.scope),
                requires_elevation: plan.summary.requires_elevation,
                estimated_bytes: file.size,
                component: None,
                technical_key: Some(format!("{:?}", file.key)),
            });
        }
        for shortcut in &plan.shortcuts {
            groups
                .entry(ResourceCategory::Shortcuts)
                .or_default()
                .push(PlannedChange {
                    category: ResourceCategory::Shortcuts,
                    kind: ChangeKind::Create,
                    label: shortcut.name.to_string(),
                    location: Some(shortcut.target.to_string()),
                    scope: Some(plan.scope),
                    requires_elevation: false,
                    estimated_bytes: 0,
                    component: None,
                    technical_key: Some(format!("{:?}", shortcut.key)),
                });
        }
        for entry in &plan.path_entries {
            groups
                .entry(ResourceCategory::Path)
                .or_default()
                .push(PlannedChange {
                    category: ResourceCategory::Path,
                    kind: ChangeKind::Create,
                    label: "Add PATH entry".into(),
                    location: Some(entry.value.to_string()),
                    scope: Some(plan.scope),
                    requires_elevation: entry.privilege == zup_core::Privilege::Machine,
                    estimated_bytes: 0,
                    component: None,
                    technical_key: Some(format!("{:?}", entry.key)),
                });
        }
        for service in &plan.services {
            groups
                .entry(ResourceCategory::Services)
                .or_default()
                .push(PlannedChange {
                    category: ResourceCategory::Services,
                    kind: ChangeKind::Create,
                    label: service.name.to_string(),
                    location: Some(service.binary.to_string()),
                    scope: Some(SelectedScope::Machine),
                    requires_elevation: true,
                    estimated_bytes: 0,
                    component: None,
                    technical_key: Some(format!("{:?}", service.key)),
                });
        }
        for protocol in &plan.protocols {
            groups
                .entry(ResourceCategory::Protocols)
                .or_default()
                .push(PlannedChange {
                    category: ResourceCategory::Protocols,
                    kind: ChangeKind::Create,
                    label: format!("{}://", protocol.scheme),
                    location: Some(protocol.executable.to_string()),
                    scope: Some(plan.scope),
                    requires_elevation: protocol.privilege == zup_core::Privilege::Machine,
                    estimated_bytes: 0,
                    component: None,
                    technical_key: Some(format!("{:?}", protocol.key)),
                });
        }
        for file_type in &plan.file_types {
            groups
                .entry(ResourceCategory::FileAssociations)
                .or_default()
                .push(PlannedChange {
                    category: ResourceCategory::FileAssociations,
                    kind: ChangeKind::Create,
                    label: format!("{} files", file_type.extension),
                    location: Some(file_type.executable.to_string()),
                    scope: Some(plan.scope),
                    requires_elevation: file_type.privilege == zup_core::Privilege::Machine,
                    estimated_bytes: 0,
                    component: None,
                    technical_key: Some(format!("{:?}", file_type.key)),
                });
        }
        groups
            .entry(ResourceCategory::AppsFeatures)
            .or_default()
            .push(PlannedChange {
                category: ResourceCategory::AppsFeatures,
                kind: ChangeKind::Create,
                label: "Register in Apps & Features".into(),
                location: None,
                scope: Some(plan.scope),
                requires_elevation: plan.scope == SelectedScope::Machine,
                estimated_bytes: 0,
                component: None,
                technical_key: Some("uninstall_entry".into()),
            });
        Self {
            application: plan.app.name.to_string(),
            version: plan.app.version.to_string(),
            scope: plan.scope,
            install_directory: plan.install_directory.to_string(),
            selected_components: plan.selected_components.clone(),
            estimated_bytes: plan.summary.install_bytes,
            requires_elevation: plan.summary.requires_elevation,
            groups: groups
                .into_iter()
                .map(|(category, changes)| ChangeGroup {
                    category,
                    title: category.title().into(),
                    changes,
                })
                .collect(),
        }
    }

    pub fn from_execution_plan(plan: &ExecutionPlan, scope: SelectedScope) -> Self {
        let mut groups = BTreeMap::<ResourceCategory, Vec<PlannedChange>>::new();
        for file in &plan.files {
            let kind = match file.kind {
                FileOperationKind::Create => ChangeKind::Create,
                FileOperationKind::Replace
                | FileOperationKind::RestoreOwned
                | FileOperationKind::RepairOwned => ChangeKind::Update,
                FileOperationKind::NoOp => ChangeKind::NoOp,
                FileOperationKind::Drift => ChangeKind::Drift,
                FileOperationKind::Conflict => ChangeKind::Conflict,
            };
            let category = if matches!(file.key, ResourceKey::Maintenance { .. }) {
                ResourceCategory::Maintenance
            } else {
                ResourceCategory::Files
            };
            groups.entry(category).or_default().push(PlannedChange {
                category,
                kind,
                label: file.source_relative.to_string(),
                location: Some(file.destination.to_string()),
                scope: Some(scope),
                requires_elevation: false,
                estimated_bytes: file.expected_size,
                component: None,
                technical_key: Some(format!("{:?}", file.key)),
            });
        }
        for shortcut in &plan.shortcuts {
            let kind = match shortcut.kind {
                ShortcutOperationKind::Create => ChangeKind::Create,
                ShortcutOperationKind::UpdateOwned | ShortcutOperationKind::RestoreOwned => {
                    ChangeKind::Update
                }
                ShortcutOperationKind::NoOp => ChangeKind::NoOp,
                ShortcutOperationKind::Drift => ChangeKind::Drift,
                ShortcutOperationKind::Conflict => ChangeKind::Conflict,
            };
            groups
                .entry(ResourceCategory::Shortcuts)
                .or_default()
                .push(PlannedChange {
                    category: ResourceCategory::Shortcuts,
                    kind,
                    label: shortcut.target.to_string(),
                    location: Some(shortcut.link_path.to_string()),
                    scope: Some(scope),
                    requires_elevation: false,
                    estimated_bytes: 0,
                    component: None,
                    technical_key: Some(format!("{:?}", shortcut.key)),
                });
        }
        for entry in &plan.path_entries {
            let kind = match entry.kind {
                PathOperationKind::Add => ChangeKind::Create,
                PathOperationKind::UpdateOwned | PathOperationKind::RestoreOwned => {
                    ChangeKind::Update
                }
                PathOperationKind::Present => ChangeKind::NoOp,
                PathOperationKind::Drift => ChangeKind::Drift,
                PathOperationKind::Conflict => ChangeKind::Conflict,
            };
            groups
                .entry(ResourceCategory::Path)
                .or_default()
                .push(PlannedChange {
                    category: ResourceCategory::Path,
                    kind,
                    label: "PATH entry".into(),
                    location: Some(entry.value.to_string()),
                    scope: Some(entry.scope),
                    requires_elevation: entry.scope == SelectedScope::Machine,
                    estimated_bytes: 0,
                    component: None,
                    technical_key: Some(format!("{:?}", entry.key)),
                });
        }
        for service in &plan.services {
            let kind = match service.kind {
                ServiceOperationKind::Create => ChangeKind::Create,
                ServiceOperationKind::UpdateOwned | ServiceOperationKind::RestoreOwned => {
                    ChangeKind::Update
                }
                ServiceOperationKind::NoOp => ChangeKind::NoOp,
                ServiceOperationKind::Drift => ChangeKind::Drift,
                ServiceOperationKind::Conflict => ChangeKind::Conflict,
            };
            groups
                .entry(ResourceCategory::Services)
                .or_default()
                .push(PlannedChange {
                    category: ResourceCategory::Services,
                    kind,
                    label: if service.display_name.is_empty() {
                        service.name.clone()
                    } else {
                        service.display_name.clone()
                    },
                    location: None,
                    scope: Some(SelectedScope::Machine),
                    requires_elevation: true,
                    estimated_bytes: 0,
                    component: None,
                    technical_key: Some(format!("{:?}", service.key)),
                });
        }
        for protocol in &plan.protocols {
            let kind = match protocol.kind {
                ProtocolOperationKind::Create => ChangeKind::Create,
                ProtocolOperationKind::UpdateOwned | ProtocolOperationKind::RestoreOwned => {
                    ChangeKind::Update
                }
                ProtocolOperationKind::NoOp => ChangeKind::NoOp,
                ProtocolOperationKind::Drift => ChangeKind::Drift,
                ProtocolOperationKind::Conflict => ChangeKind::Conflict,
            };
            groups
                .entry(ResourceCategory::Protocols)
                .or_default()
                .push(PlannedChange {
                    category: ResourceCategory::Protocols,
                    kind,
                    label: format!("{}://", protocol.scheme),
                    location: None,
                    scope: Some(protocol.scope),
                    requires_elevation: protocol.scope == SelectedScope::Machine,
                    estimated_bytes: 0,
                    component: None,
                    technical_key: Some(format!("{:?}", protocol.key)),
                });
        }
        for file_type in &plan.file_types {
            let kind = match file_type.kind {
                FileTypeOperationKind::Create => ChangeKind::Create,
                FileTypeOperationKind::UpdateOwned | FileTypeOperationKind::RestoreOwned => {
                    ChangeKind::Update
                }
                FileTypeOperationKind::NoOp => ChangeKind::NoOp,
                FileTypeOperationKind::Drift => ChangeKind::Drift,
                FileTypeOperationKind::Conflict => ChangeKind::Conflict,
            };
            groups
                .entry(ResourceCategory::FileAssociations)
                .or_default()
                .push(PlannedChange {
                    category: ResourceCategory::FileAssociations,
                    kind,
                    label: format!("{} files", file_type.extension),
                    location: None,
                    scope: Some(file_type.scope),
                    requires_elevation: file_type.scope == SelectedScope::Machine,
                    estimated_bytes: 0,
                    component: None,
                    technical_key: Some(format!("{:?}", file_type.key)),
                });
        }
        for removal in &plan.removals {
            let category = match &removal.key {
                ResourceKey::File { .. } => ResourceCategory::Files,
                ResourceKey::Maintenance { .. } => ResourceCategory::Maintenance,
                ResourceKey::Shortcut { .. } => ResourceCategory::Shortcuts,
                ResourceKey::PathEntry { .. } => ResourceCategory::Path,
                ResourceKey::Service { .. } => ResourceCategory::Services,
                ResourceKey::Protocol { .. } => ResourceCategory::Protocols,
                ResourceKey::FileType { .. } | ResourceKey::FileTypeExtension { .. } => {
                    ResourceCategory::FileAssociations
                }
                ResourceKey::UninstallEntry { .. } => ResourceCategory::AppsFeatures,
            };
            groups.entry(category).or_default().push(PlannedChange {
                category,
                kind: if removal.kind == zup_exec::RemovalKind::Drift {
                    ChangeKind::Drift
                } else {
                    ChangeKind::Remove
                },
                label: "Remove managed resource".into(),
                location: None,
                scope: Some(removal.scope),
                requires_elevation: removal.scope == SelectedScope::Machine,
                estimated_bytes: 0,
                component: None,
                technical_key: Some(format!("{:?}", removal.key)),
            });
        }
        if !plan.uninstall_entries.is_empty() {
            groups
                .entry(ResourceCategory::AppsFeatures)
                .or_default()
                .push(PlannedChange {
                    category: ResourceCategory::AppsFeatures,
                    kind: if plan.uninstall {
                        ChangeKind::Remove
                    } else {
                        ChangeKind::Update
                    },
                    label: "Apps & Features registration".into(),
                    location: None,
                    scope: Some(scope),
                    requires_elevation: scope == SelectedScope::Machine,
                    estimated_bytes: 0,
                    component: None,
                    technical_key: Some("uninstall_entry".into()),
                });
        }
        Self {
            application: String::new(),
            version: String::new(),
            scope,
            install_directory: String::new(),
            selected_components: plan.selected_components.clone(),
            estimated_bytes: plan.summary.write_bytes,
            requires_elevation: plan.summary.requires_elevation,
            groups: groups
                .into_iter()
                .map(|(category, changes)| ChangeGroup {
                    category,
                    title: category.title().into(),
                    changes,
                })
                .collect(),
        }
    }

    pub fn human(&self) -> String {
        let mut output = String::new();
        let _ = writeln!(output, "{} {}", self.application, self.version);
        let _ = writeln!(output, "Scope: {}", self.scope);
        if !self.install_directory.is_empty() {
            let _ = writeln!(output, "Location: {}", self.install_directory);
        }
        let _ = writeln!(output, "Disk: {}", format_bytes(self.estimated_bytes));
        if self.requires_elevation {
            let _ = writeln!(output, "Elevation: requested");
        }
        for group in &self.groups {
            let _ = writeln!(output, "\n{}", group.title);
            for change in &group.changes {
                let location = change
                    .location
                    .as_deref()
                    .map(|value| format!(" — {value}"))
                    .unwrap_or_default();
                let _ = writeln!(
                    output,
                    "  {} {}{location}",
                    change.kind.label(),
                    change.label
                );
            }
        }
        output
    }

    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }
}

pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationPhase {
    Prepare,
    Download,
    Verify,
    Files,
    System,
    Finish,
}

impl OperationPhase {
    pub const fn title(self) -> &'static str {
        match self {
            Self::Prepare => "Prepare",
            Self::Download => "Download",
            Self::Verify => "Verify",
            Self::Files => "Files",
            Self::System => "System",
            Self::Finish => "Finish",
        }
    }

    pub fn from_action(action: &str) -> Self {
        let action = action.to_ascii_lowercase();
        if action.contains("download") {
            Self::Download
        } else if action.contains("verif") || action.contains("signature") {
            Self::Verify
        } else if action.contains("service")
            || action.contains("shortcut")
            || action.contains("path")
            || action.contains("protocol")
            || action.contains("registry")
        {
            Self::System
        } else if action.contains("finish") || action.contains("commit") {
            Self::Finish
        } else if action.contains("file") || action.contains("install") {
            Self::Files
        } else {
            Self::Prepare
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgressPresentation {
    pub phase: OperationPhase,
    pub completed: u64,
    pub total: u64,
    pub label: String,
}

impl ProgressPresentation {
    pub fn new(completed: u64, total: u64, label: impl Into<String>) -> Self {
        let label = label.into();
        Self {
            phase: OperationPhase::from_action(&label),
            completed,
            total,
            label,
        }
    }

    pub fn percent(&self) -> Option<u32> {
        (self.total > 0)
            .then(|| ((self.completed.min(self.total) as f64 / self.total as f64) * 100.0) as u32)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticKind {
    Blocked,
    Conflict,
    Drift,
    Permission,
    Recovery,
    Verification,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiagnosticPresentation {
    pub kind: DiagnosticKind,
    pub title: String,
    pub meaning: String,
    pub recovery: String,
    pub technical_details: Option<String>,
}

impl DiagnosticPresentation {
    pub fn from_message(message: &str, recovery_required: bool) -> Self {
        let normalized = message.to_ascii_lowercase();
        if normalized.contains("running") || normalized.contains("blocked by") {
            Self {
                kind: DiagnosticKind::Blocked,
                title: "An application is still running".into(),
                meaning: "zup needs the application to close before it can continue safely.".into(),
                recovery: "Close the listed applications, then choose Retry.".into(),
                technical_details: Some(message.into()),
            }
        } else if normalized.contains("drift") || normalized.contains("modified outside") {
            Self {
                kind: DiagnosticKind::Drift,
                title: "Some installed files were changed".into(),
                meaning: "The files no longer match the copy zup installed.".into(),
                recovery: "Repair can restore owned files, or leave the modified files untouched."
                    .into(),
                technical_details: Some(message.into()),
            }
        } else if normalized.contains("recovery") || recovery_required {
            Self {
                kind: DiagnosticKind::Recovery,
                title: "Recovery is required".into(),
                meaning: "The last transaction did not finish safely.".into(),
                recovery: "Run recovery before starting another operation.".into(),
                technical_details: Some(message.into()),
            }
        } else if normalized.contains("permission")
            || normalized.contains("access")
            || normalized.contains("elevation")
        {
            Self {
                kind: DiagnosticKind::Permission,
                title: "Windows needs permission".into(),
                meaning: "This operation includes a protected system change.".into(),
                recovery: "Approve the elevation prompt, then choose Retry.".into(),
                technical_details: Some(message.into()),
            }
        } else {
            Self {
                kind: DiagnosticKind::Unknown,
                title: "The operation could not finish".into(),
                meaning: "zup stopped before the transaction was committed.".into(),
                recovery: "Choose Retry, or open diagnostics for support.".into(),
                technical_details: Some(message.into()),
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallationHealth {
    pub state: String,
    pub summary: String,
    pub drift_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdatePresentation {
    pub channel: Option<String>,
    pub state: String,
    pub current: Option<String>,
    pub available: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_are_human_readable() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1024), "1.0 KiB");
    }

    #[test]
    fn phase_classification_keeps_one_progress_model() {
        assert_eq!(
            OperationPhase::from_action("Downloading update"),
            OperationPhase::Download
        );
        assert_eq!(
            OperationPhase::from_action("Verifying signature"),
            OperationPhase::Verify
        );
        assert_eq!(
            OperationPhase::from_action("Registering services"),
            OperationPhase::System
        );
        let progress = ProgressPresentation::new(3, 4, "Installing files");
        assert_eq!(progress.phase, OperationPhase::Files);
        assert_eq!(progress.percent(), Some(75));
    }

    #[test]
    fn diagnostic_copy_is_actionable() {
        let diagnostic =
            DiagnosticPresentation::from_message("blocked by running applications", false);
        assert_eq!(diagnostic.kind, DiagnosticKind::Blocked);
        assert!(diagnostic.recovery.contains("Retry"));
    }
}
