#![forbid(unsafe_code)]

mod acquisition;

pub use acquisition::{AcquisitionThread, acquisition_thread, automation_events, progress_line};

use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde::{Deserialize, Serialize, de::Error as _};
use zup_core::{ComponentId, ResourceKey, SelectedScope};
use zup_exec::{
    ExecutionPlan, FileAssociationOperationKind, FileOperationKind, LauncherOperationKind,
    PathOperationKind, ProtocolOperationKind, ServiceOperationKind,
};
use zup_plan::InstallPlan;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceCategory {
    Files,
    Launchers,
    Path,
    Services,
    Protocols,
    FileAssociations,
    AppsFeatures,
    Maintenance,
    Prerequisites,
    Other,
}

impl ResourceCategory {
    pub const fn title(self) -> &'static str {
        match self {
            Self::Files => "Files",
            Self::Launchers => "Launchers",
            Self::Path => "Search path",
            Self::Services => "Services",
            Self::Protocols => "Protocols",
            Self::FileAssociations => "File associations",
            Self::AppsFeatures => "Apps & Features",
            Self::Maintenance => "Maintenance",
            Self::Prerequisites => "Requirements",
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
    /// True when this single change needs host-wide authority.
    pub requires_authorization: bool,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequirementStatus {
    Satisfied,
    Missing,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequirementPresentation {
    pub id: String,
    pub name: String,
    pub status: RequirementStatus,
    pub estimated_bytes: u64,
    pub shared: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanPreview {
    pub application: String,
    pub version: String,
    pub scope: SelectedScope,
    pub install_directory: String,
    pub selected_components: Vec<ComponentId>,
    pub estimated_bytes: u64,
    #[serde(default)]
    pub download_bytes: u64,
    /// True when any change in this preview needs host-wide authority.
    pub requires_authorization: bool,
    pub groups: Vec<ChangeGroup>,
    #[serde(default)]
    pub requirements: Vec<RequirementPresentation>,
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
                requires_authorization: file.privilege == zup_core::Privilege::System,
                estimated_bytes: file.size,
                component: None,
                technical_key: Some(format!("{:?}", file.key)),
            });
        }
        for launcher in &plan.launchers {
            groups
                .entry(ResourceCategory::Launchers)
                .or_default()
                .push(PlannedChange {
                    category: ResourceCategory::Launchers,
                    kind: ChangeKind::Create,
                    label: launcher.name.to_string(),
                    location: Some(launcher.target.to_string()),
                    scope: Some(plan.scope),
                    requires_authorization: launcher.privilege == zup_core::Privilege::System,
                    estimated_bytes: 0,
                    component: None,
                    technical_key: Some(format!("{:?}", launcher.key)),
                });
        }
        for entry in &plan.path_entries {
            groups
                .entry(ResourceCategory::Path)
                .or_default()
                .push(PlannedChange {
                    category: ResourceCategory::Path,
                    kind: ChangeKind::Create,
                    label: "Add search-path entry".into(),
                    location: Some(entry.value.to_string()),
                    scope: Some(entry.scope),
                    requires_authorization: entry.privilege == zup_core::Privilege::System,
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
                    requires_authorization: service.privilege == zup_core::Privilege::System,
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
                    scope: Some(protocol.scope),
                    requires_authorization: protocol.privilege == zup_core::Privilege::System,
                    estimated_bytes: 0,
                    component: None,
                    technical_key: Some(format!("{:?}", protocol.key)),
                });
        }
        for file_association in &plan.file_associations {
            groups
                .entry(ResourceCategory::FileAssociations)
                .or_default()
                .push(PlannedChange {
                    category: ResourceCategory::FileAssociations,
                    kind: ChangeKind::Create,
                    label: format!("{} files", file_association.extension),
                    location: Some(file_association.executable.to_string()),
                    scope: Some(file_association.scope),
                    requires_authorization: file_association.privilege
                        == zup_core::Privilege::System,
                    estimated_bytes: 0,
                    component: None,
                    technical_key: Some(format!("{:?}", file_association.key)),
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
                requires_authorization: plan.summary.requires_authorization,
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
            download_bytes: plan.summary.download_bytes,
            requires_authorization: plan.summary.requires_authorization,
            groups: groups
                .into_iter()
                .map(|(category, changes)| ChangeGroup {
                    category,
                    title: category.title().into(),
                    changes,
                })
                .collect(),
            requirements: plan
                .prerequisites
                .iter()
                .map(|prerequisite| RequirementPresentation {
                    id: prerequisite.id.to_string(),
                    name: prerequisite.name.to_string(),
                    status: RequirementStatus::Unknown,
                    estimated_bytes: prerequisite.package.size().unwrap_or(0),
                    shared: true,
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
                requires_authorization: file.privilege == zup_core::Privilege::System,
                estimated_bytes: file.expected_size,
                component: None,
                technical_key: Some(format!("{:?}", file.key)),
            });
        }
        for launcher in &plan.launchers {
            let kind = match launcher.kind {
                LauncherOperationKind::Create => ChangeKind::Create,
                LauncherOperationKind::UpdateOwned | LauncherOperationKind::RestoreOwned => {
                    ChangeKind::Update
                }
                LauncherOperationKind::NoOp => ChangeKind::NoOp,
                LauncherOperationKind::Drift => ChangeKind::Drift,
                LauncherOperationKind::Conflict => ChangeKind::Conflict,
            };
            groups
                .entry(ResourceCategory::Launchers)
                .or_default()
                .push(PlannedChange {
                    category: ResourceCategory::Launchers,
                    kind,
                    label: launcher.target.to_string(),
                    location: Some(launcher.launcher_path.to_string()),
                    scope: Some(scope),
                    requires_authorization: launcher.privilege == zup_core::Privilege::System,
                    estimated_bytes: 0,
                    component: None,
                    technical_key: Some(format!("{:?}", launcher.key)),
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
                    label: "Search-path entry".into(),
                    location: Some(entry.value.to_string()),
                    scope: Some(entry.scope),
                    requires_authorization: entry.privilege == zup_core::Privilege::System,
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
                    requires_authorization: service.privilege == zup_core::Privilege::System,
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
                    requires_authorization: protocol.privilege == zup_core::Privilege::System,
                    estimated_bytes: 0,
                    component: None,
                    technical_key: Some(format!("{:?}", protocol.key)),
                });
        }
        for file_association in &plan.file_associations {
            let kind = match file_association.kind {
                FileAssociationOperationKind::Create => ChangeKind::Create,
                FileAssociationOperationKind::UpdateOwned
                | FileAssociationOperationKind::RestoreOwned => ChangeKind::Update,
                FileAssociationOperationKind::NoOp => ChangeKind::NoOp,
                FileAssociationOperationKind::Drift => ChangeKind::Drift,
                FileAssociationOperationKind::Conflict => ChangeKind::Conflict,
            };
            groups
                .entry(ResourceCategory::FileAssociations)
                .or_default()
                .push(PlannedChange {
                    category: ResourceCategory::FileAssociations,
                    kind,
                    label: format!("{} files", file_association.extension),
                    location: None,
                    scope: Some(file_association.scope),
                    requires_authorization: file_association.privilege
                        == zup_core::Privilege::System,
                    estimated_bytes: 0,
                    component: None,
                    technical_key: Some(format!("{:?}", file_association.key)),
                });
        }
        for removal in &plan.removals {
            let category = match &removal.key {
                ResourceKey::File { .. } => ResourceCategory::Files,
                ResourceKey::Maintenance { .. } => ResourceCategory::Maintenance,
                ResourceKey::Launcher { .. } => ResourceCategory::Launchers,
                ResourceKey::PathEntry { .. } => ResourceCategory::Path,
                ResourceKey::Service { .. } => ResourceCategory::Services,
                ResourceKey::Protocol { .. } => ResourceCategory::Protocols,
                ResourceKey::FileAssociation { .. }
                | ResourceKey::FileAssociationExtension { .. } => {
                    ResourceCategory::FileAssociations
                }
                ResourceKey::Backend { .. } => ResourceCategory::AppsFeatures,
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
                requires_authorization: removal.privilege == zup_core::Privilege::System,
                estimated_bytes: 0,
                component: None,
                technical_key: Some(format!("{:?}", removal.key)),
            });
        }
        Self {
            application: String::new(),
            version: String::new(),
            scope,
            install_directory: String::new(),
            selected_components: plan.selected_components.clone(),
            estimated_bytes: plan.summary.write_bytes,
            download_bytes: 0,
            requires_authorization: plan.summary.requires_authorization,
            groups: groups
                .into_iter()
                .map(|(category, changes)| ChangeGroup {
                    category,
                    title: category.title().into(),
                    changes,
                })
                .collect(),
            requirements: Vec::new(),
        }
    }

    pub fn from_transaction_plan(
        plan: &zup_transaction::TransactionPlan,
        scope: SelectedScope,
    ) -> Self {
        let mut groups = BTreeMap::<ResourceCategory, Vec<PlannedChange>>::new();
        let mut estimated_bytes = 0u64;
        for node in &plan.nodes {
            match &node.kind {
                zup_transaction::NodeKind::FileMutation { key, delta } => {
                    let category = if matches!(key, ResourceKey::Maintenance { .. }) {
                        ResourceCategory::Maintenance
                    } else {
                        ResourceCategory::Files
                    };
                    let kind = match delta {
                        zup_transaction::FileDelta::Create => ChangeKind::Create,
                        zup_transaction::FileDelta::Replace
                        | zup_transaction::FileDelta::RestoreOwned
                        | zup_transaction::FileDelta::RepairOwned => ChangeKind::Update,
                        zup_transaction::FileDelta::NoOp => ChangeKind::NoOp,
                        zup_transaction::FileDelta::Drift => ChangeKind::Drift,
                        zup_transaction::FileDelta::Conflict => ChangeKind::Conflict,
                    };
                    let size = node.meta.expected_size.unwrap_or(0);
                    estimated_bytes = estimated_bytes.saturating_add(size);
                    groups.entry(category).or_default().push(PlannedChange {
                        category,
                        kind,
                        label: node
                            .meta
                            .source_relative
                            .as_ref()
                            .map(ToString::to_string)
                            .unwrap_or_else(|| format!("{key:?}")),
                        location: node
                            .meta
                            .source_relative
                            .as_ref()
                            .map(|_| format!("{key:?}")),
                        scope: Some(scope),
                        requires_authorization: node.meta.privilege
                            == Some(zup_core::Privilege::System),
                        estimated_bytes: size,
                        component: None,
                        technical_key: Some(format!("{key:?}")),
                    });
                }
                zup_transaction::NodeKind::FileRemoval { key } => {
                    let category = match key {
                        ResourceKey::Maintenance { .. } => ResourceCategory::Maintenance,
                        _ => ResourceCategory::Files,
                    };
                    groups.entry(category).or_default().push(PlannedChange {
                        category,
                        kind: ChangeKind::Remove,
                        label: "Remove file".into(),
                        location: Some(format!("{key:?}")),
                        scope: Some(scope),
                        requires_authorization: node.meta.privilege
                            == Some(zup_core::Privilege::System),
                        estimated_bytes: 0,
                        component: None,
                        technical_key: Some(format!("{key:?}")),
                    });
                }
                zup_transaction::NodeKind::BackendOperation { key, .. }
                | zup_transaction::NodeKind::BackendRemoval { key } => {
                    groups
                        .entry(ResourceCategory::AppsFeatures)
                        .or_default()
                        .push(PlannedChange {
                            category: ResourceCategory::AppsFeatures,
                            kind: if matches!(
                                node.kind,
                                zup_transaction::NodeKind::BackendRemoval { .. }
                            ) {
                                ChangeKind::Remove
                            } else {
                                ChangeKind::Update
                            },
                            label: "Backend operation".into(),
                            location: None,
                            scope: Some(scope),
                            requires_authorization: node.meta.privilege
                                == Some(zup_core::Privilege::System),
                            estimated_bytes: 0,
                            component: None,
                            technical_key: Some(format!("{key:?}")),
                        });
                }
                zup_transaction::NodeKind::StageFile { .. }
                | zup_transaction::NodeKind::Barrier => {}
            }
        }
        Self {
            application: String::new(),
            version: String::new(),
            scope,
            install_directory: plan
                .install_directory
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_default(),
            selected_components: plan.selected_components.clone(),
            estimated_bytes,
            download_bytes: 0,
            requires_authorization: plan.requires_authorization(),
            groups: groups
                .into_iter()
                .map(|(category, changes)| ChangeGroup {
                    category,
                    title: category.title().into(),
                    changes,
                })
                .collect(),
            requirements: Vec::new(),
        }
    }

    pub fn with_declared_prerequisites(mut self, prerequisites: &[zup_core::Prerequisite]) -> Self {
        self.requirements = prerequisites
            .iter()
            .map(|prerequisite| RequirementPresentation {
                id: prerequisite.id.to_string(),
                name: prerequisite.name.to_string(),
                status: RequirementStatus::Unknown,
                estimated_bytes: prerequisite.package.size().unwrap_or(0),
                shared: true,
            })
            .collect();
        self
    }

    pub fn with_prerequisites(mut self, plan: &InstallPlan) -> Self {
        self.requirements = plan
            .prerequisites
            .iter()
            .map(|prerequisite| RequirementPresentation {
                id: prerequisite.id.to_string(),
                name: prerequisite.name.to_string(),
                status: RequirementStatus::Unknown,
                estimated_bytes: prerequisite.package.size().unwrap_or(0),
                shared: true,
            })
            .collect();
        self
    }

    pub fn human(&self) -> String {
        let mut output = String::new();
        let _ = writeln!(output, "{} {}", self.application, self.version);
        let _ = writeln!(output, "Scope: {}", self.scope);
        if !self.install_directory.is_empty() {
            let _ = writeln!(output, "Location: {}", self.install_directory);
        }
        let _ = writeln!(output, "Install: {}", format_bytes(self.estimated_bytes));
        if self.download_bytes > 0 {
            let _ = writeln!(output, "Download: {}", format_bytes(self.download_bytes));
        }
        if self.requires_authorization {
            let _ = writeln!(output, "Authorization: system access required");
        }
        if !self.requirements.is_empty() {
            let _ = writeln!(output, "\nRequirements");
            for requirement in &self.requirements {
                let status = match requirement.status {
                    RequirementStatus::Satisfied => "✓",
                    RequirementStatus::Missing => "+",
                    RequirementStatus::Unknown => "?",
                };
                let _ = writeln!(
                    output,
                    "  {status} {} ({})",
                    requirement.name,
                    format_bytes(requirement.estimated_bytes)
                );
            }
            let _ = writeln!(
                output,
                "  Shared system dependencies are not removed when this application is uninstalled."
            );
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
            || action.contains("launcher")
            || action.contains("path")
            || action.contains("protocol")
            || action.contains("system")
            || action.contains("settings")
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
            || normalized.contains("authorization")
            || normalized.contains("privilege")
            || normalized.contains("elevation")
        {
            Self {
                kind: DiagnosticKind::Permission,
                title: "Windows needs permission".into(),
                meaning: "This operation includes a protected system change.".into(),
                recovery: "Approve the Windows administrator prompt, then choose Retry.".into(),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputFormat {
    Human,
    Json,
    Jsonl,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessOutcome {
    Success,
    Cancelled,
    InvalidInvocation,
    Configuration,
    OwnershipConflict,
    AuthorizationRequired,
    VerificationFailure,
    RecoveryRequired,
    RebootRequired,
    /// Another operation holds this installation's lock.
    ///
    /// Its own code, because a scheduled retry has to be able to tell "somebody
    /// else is installing this right now" from "this installation is broken" —
    /// the first is not a failure of anything and the second is. Collapsing them
    /// into `1` is how a second unattended installer turns a five-second wait
    /// into an alert.
    InstallationBusy,
    Failure,
}

impl ProcessOutcome {
    pub const fn code(self) -> i32 {
        match self {
            Self::Success => 0,
            Self::Cancelled => 2,
            Self::InvalidInvocation | Self::Configuration => 3,
            Self::OwnershipConflict => 4,
            Self::AuthorizationRequired => 5,
            Self::VerificationFailure => 6,
            Self::RecoveryRequired => 7,
            Self::InstallationBusy => 8,
            Self::RebootRequired => 3010,
            Self::Failure => 1,
        }
    }

    /// Classify an outcome from its message.
    ///
    /// A last resort, used where a typed value has already been flattened into a
    /// `miette` report. Everywhere a typed outcome or a typed event kind is
    /// available the caller uses that instead — this function reads English, and
    /// reading English is how "another operation is running" becomes a failure
    /// with code 1 and an alert at three in the morning.
    pub fn from_message(message: &str) -> Self {
        let normalized = message.to_ascii_lowercase();
        if normalized.contains("cancelled")
            || normalized.contains("canceled")
            || normalized.contains("cancel was requested")
        {
            Self::Cancelled
        } else if normalized.contains("authorization")
            || normalized.contains("elevation")
            || normalized.contains("privilege")
            || normalized.contains("administrator")
        {
            Self::AuthorizationRequired
        } else if normalized.contains("reboot")
            || normalized.contains("restart required")
            || normalized.contains("3010")
        {
            Self::RebootRequired
        } else if normalized.contains("recovery") || normalized.contains("journal") {
            Self::RecoveryRequired
        } else if normalized.contains("busy")
            || normalized.contains("already running")
            || normalized.contains("another operation")
        {
            // Before the ownership and verification branches: a message about
            // another operation in progress frequently mentions the very files
            // that are locked, and classifying it as an ownership conflict would
            // tell a user to repair an installation that is perfectly healthy.
            Self::InstallationBusy
        } else if normalized.contains("drift")
            || normalized.contains("ownership")
            || normalized.contains("occupied")
            || normalized.contains("conflict")
        {
            Self::OwnershipConflict
        } else if normalized.contains("verification")
            || normalized.contains("signature")
            || normalized.contains("trust")
            || normalized.contains("tuf")
            || normalized.contains("digest")
            || normalized.contains("hash")
            || normalized.contains("integrity")
            || normalized.contains("quarantine")
        {
            Self::VerificationFailure
        } else if normalized.contains("configuration")
            || normalized.contains("invalid")
            || normalized.contains("required option")
        {
            Self::Configuration
        } else {
            Self::Failure
        }
    }
}

// # The installed application's machine protocol
//
// `--output json` and `--output jsonl` on a generated installer report through
// [`InstallerEvent`] and [`InstallerResult`]. This is the *runtime's* protocol — what
// happened while installing, modifying, repairing, updating or uninstalling an
// application on a user's own machine — and it is deliberately not the same protocol
// as `zup build --format json`.
//
// Two protocols, two products, two consumers. The developer CLI's contract lives in
// `zup-automation` and speaks about operations, artifacts, targets and release
// results; this one speaks about an outcome, a process exit code and a plan, because
// that is what a runtime has. Merging them would give both a compromise, and would
// make a change to an installer's progress reporting a change to a CI contract.
//
// The names say which is which. `InstallerEvent` and `InstallerResult` are the installed
// application's; `zup_automation::StreamEvent` and `zup_automation::AutomationResult`
// are the developer CLI's. Neither crate depends on the other.

/// The version of the installed application's protocol.
///
/// Its own counter, unrelated to `zup_automation::PROTOCOL`. They are two protocols
/// and a shared number would be a shared compatibility question, which they do not
/// have.
pub const INSTALLER_PROTOCOL_VERSION: u32 = 1;

/// One event in the installed application's `--output jsonl` stream.
///
/// `started`, then whatever the operation reported, then `completed` or `failed`. The
/// consumer is a wrapper script or an enterprise deployment tool, and the vocabulary
/// is deliberately about *progress* rather than about artifacts: a runtime has no
/// build to describe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum InstallerEvent {
    Started {
        protocol_version: u32,
        application: String,
        version: String,
        action: String,
    },
    Phase {
        state: String,
    },
    Progress {
        phase: OperationPhase,
        completed: u64,
        total: u64,
        label: String,
    },
    PrerequisiteCheck {
        id: String,
        name: String,
        satisfied: bool,
        version: Option<String>,
    },
    PrerequisiteDownload {
        id: String,
        completed: u64,
        total: Option<u64>,
    },
    PrerequisiteInstall {
        id: String,
        name: String,
    },
    RebootRequired {
        id: String,
        exit_code: i32,
    },
    Blocked {
        message: String,
        processes: Vec<u32>,
    },
    Cancelling {
        state: String,
    },
    Completed {
        outcome: ProcessOutcome,
    },
    Failed {
        outcome: ProcessOutcome,
        code: i32,
        message: String,
        diagnostic: Option<DiagnosticPresentation>,
    },
}

impl InstallerEvent {
    pub fn started(
        application: impl Into<String>,
        version: impl Into<String>,
        action: impl Into<String>,
    ) -> Self {
        Self::Started {
            protocol_version: INSTALLER_PROTOCOL_VERSION,
            application: application.into(),
            version: version.into(),
            action: action.into(),
        }
    }

    pub fn progress(progress: &ProgressPresentation) -> Self {
        Self::Progress {
            phase: progress.phase,
            completed: progress.completed,
            total: progress.total,
            label: progress.label.clone(),
        }
    }

    pub fn blocked(message: impl Into<String>) -> Self {
        let message = message.into();
        let processes = message
            .split(|character: char| !character.is_ascii_digit())
            .filter_map(|part| part.parse::<u32>().ok())
            .filter(|pid| *pid > 0)
            .collect();
        Self::Blocked { message, processes }
    }

    pub fn blocked_with_processes(
        message: impl Into<String>,
        processes: impl IntoIterator<Item = u32>,
    ) -> Self {
        Self::Blocked {
            message: message.into(),
            processes: processes.into_iter().filter(|pid| *pid > 0).collect(),
        }
    }
}

/// The final result of one installed-application operation.
///
/// `outcome` and `code` are the two things a deployment tool branches on, and both
/// are in here rather than left to the process's exit status, because a wrapper that
/// has to read a message to learn why an install failed is a wrapper that will read
/// the wrong message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallerResult {
    pub protocol_version: u32,
    pub outcome: ProcessOutcome,
    pub code: i32,
    pub application: String,
    pub version: String,
    pub scope: Option<SelectedScope>,
    pub install_directory: Option<String>,
    pub log_path: Option<String>,
    pub message: Option<String>,
    pub drift: Vec<String>,
}

impl InstallerResult {
    pub fn new(
        outcome: ProcessOutcome,
        application: impl Into<String>,
        version: impl Into<String>,
    ) -> Self {
        Self {
            protocol_version: INSTALLER_PROTOCOL_VERSION,
            outcome,
            code: outcome.code(),
            application: application.into(),
            version: version.into(),
            scope: None,
            install_directory: None,
            log_path: None,
            message: None,
            drift: Vec::new(),
        }
    }

    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    pub fn to_jsonl<I>(events: I) -> Result<String, serde_json::Error>
    where
        I: IntoIterator<Item = InstallerEvent>,
    {
        let events = events.into_iter().collect::<Vec<_>>();
        let valid_start = matches!(
            events.first(),
            Some(InstallerEvent::Started {
                protocol_version,
                ..
            }) if *protocol_version == INSTALLER_PROTOCOL_VERSION
        );
        if !valid_start {
            return Err(serde_json::Error::custom(
                "automation JSONL must begin with a versioned started event",
            ));
        }
        let mut output = String::new();
        for event in events {
            output.push_str(&serde_json::to_string(&event)?);
            output.push('\n');
        }
        Ok(output)
    }
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
        assert_eq!(
            OperationPhase::from_action("Updating application settings"),
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

    #[test]
    fn automation_contract_has_stable_outcome_codes() {
        assert_eq!(ProcessOutcome::Success.code(), 0);
        assert_eq!(ProcessOutcome::Cancelled.code(), 2);
        assert_eq!(ProcessOutcome::OwnershipConflict.code(), 4);
        assert_eq!(ProcessOutcome::AuthorizationRequired.code(), 5);
        assert_eq!(ProcessOutcome::VerificationFailure.code(), 6);
        assert_eq!(ProcessOutcome::RecoveryRequired.code(), 7);
        assert_eq!(
            ProcessOutcome::from_message("verification failed"),
            ProcessOutcome::VerificationFailure
        );
        assert_eq!(
            ProcessOutcome::from_message("pinned artifact digest mismatch"),
            ProcessOutcome::VerificationFailure
        );
    }

    #[test]
    fn automation_jsonl_is_explicit_and_versioned() {
        let events = [
            InstallerEvent::started("Acme", "1.0.0", "install"),
            InstallerEvent::progress(&ProgressPresentation::new(1, 2, "Installing files")),
            InstallerEvent::Completed {
                outcome: ProcessOutcome::Success,
            },
        ];
        let output = InstallerResult::to_jsonl(events).unwrap();
        let lines = output.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 3);
        let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first["type"], "started");
        assert_eq!(first["protocol_version"], INSTALLER_PROTOCOL_VERSION);
        let last: serde_json::Value = serde_json::from_str(lines[2]).unwrap();
        assert_eq!(last["type"], "completed");
    }

    #[test]
    fn typed_blocker_ids_do_not_parse_process_names() {
        let event = InstallerEvent::blocked_with_processes(
            "7-Zip.exe (PID 4820), Helper (PID 7312)",
            [4820, 7312],
        );
        let value = serde_json::to_value(event).unwrap();
        assert_eq!(value["processes"], serde_json::json!([4820, 7312]));
    }

    #[test]
    fn automation_jsonl_rejects_unversioned_starts() {
        let error = InstallerResult::to_jsonl([InstallerEvent::Completed {
            outcome: ProcessOutcome::Success,
        }])
        .unwrap_err();
        assert!(error.to_string().contains("versioned started"));
    }

    #[test]
    fn blocked_output_exposes_process_ids() {
        let event = InstallerEvent::blocked(
            "blocked by running applications: Acme.exe (PID 4820), Helper (PID 7312)",
        );
        let value = serde_json::to_value(event).unwrap();
        assert_eq!(value["processes"], serde_json::json!([4820, 7312]));
    }
}
