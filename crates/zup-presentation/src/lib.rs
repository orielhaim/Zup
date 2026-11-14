#![forbid(unsafe_code)]

mod acquisition;
mod installation;
pub use installation::{DeploymentVersion, InstallationReport};

pub use acquisition::{AcquisitionThread, acquisition_thread, automation_events, progress_line};

use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde::{Deserialize, Serialize, de::Error as _};
use zup_core::format_bytes;
use zup_core::{ComponentId, ResourceKey, SelectedScope};
use zup_plan::DesiredState;

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanPreview {
    pub basis: PlanBasis,
    pub application: String,
    pub version: String,
    pub scope: SelectedScope,
    pub install_directory: String,
    pub selected_components: Vec<ComponentId>,
    pub estimated_bytes: u64,
    #[serde(default)]
    pub download_bytes: u64,
    pub storage: Option<zup_core::DeploymentStorageEstimate>,
    /// True when any change in this preview needs host-wide authority.
    pub requires_authorization: bool,
    pub groups: Vec<ChangeGroup>,
    #[serde(default)]
    pub requirements: Vec<RequirementPresentation>,
}

impl PlanPreview {
    pub fn from_desired_state<P: std::fmt::Display, L, R>(
        plan: &DesiredState<P, L, R>,
    ) -> Result<Self, zup_plan::ContractError> {
        let summary = plan.summary()?;
        let mut groups = BTreeMap::<ResourceCategory, Vec<PlannedChange>>::new();
        for directory in &plan.data {
            groups
                .entry(ResourceCategory::Data)
                .or_default()
                .push(PlannedChange {
                    category: ResourceCategory::Data,
                    kind: ChangeKind::Preserve,
                    label: directory.id.to_string(),
                    location: directory.location.path().map(ToString::to_string),
                    scope: Some(plan.scope),
                    requires_authorization: plan.scope == SelectedScope::Machine,
                    estimated_bytes: 0,
                    component: None,
                    technical_key: Some(format!("data:{}", directory.id)),
                });
        }
        for file in &plan.files {
            let category = match (&file.key, &file.placement) {
                (
                    ResourceKey::File {
                        id: zup_core::FileResourceId::Maintenance { .. },
                    },
                    _,
                ) => ResourceCategory::Maintenance,
                (_, zup_core::PayloadPlacement::Generation { .. }) => ResourceCategory::Application,
                (_, zup_core::PayloadPlacement::Fixed { .. }) => ResourceCategory::Files,
            };
            groups.entry(category).or_default().push(PlannedChange {
                category,
                kind: ChangeKind::Create,
                label: file.source_relative.to_string(),
                location: Some(file.destination().to_string()),
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
                    location: Some(service.command.executable.to_string()),
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
                    location: Some(protocol.command.executable.to_string()),
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
                    location: Some(file_association.command.executable.to_string()),
                    scope: Some(file_association.scope),
                    requires_authorization: file_association.privilege
                        == zup_core::Privilege::System,
                    estimated_bytes: 0,
                    component: None,
                    technical_key: Some(format!("{:?}", file_association.key)),
                });
        }
        if plan.target.operating_system() == zup_core::TargetOperatingSystem::Windows {
            groups
                .entry(ResourceCategory::AppsFeatures)
                .or_default()
                .push(PlannedChange {
                    category: ResourceCategory::AppsFeatures,
                    kind: ChangeKind::Create,
                    label: "Register in Apps & Features".into(),
                    location: None,
                    scope: Some(plan.scope),
                    requires_authorization: summary.requires_authorization,
                    estimated_bytes: 0,
                    component: None,
                    technical_key: Some("uninstall_entry".into()),
                });
        }
        Ok(Self {
            basis: PlanBasis::DesiredState,
            application: plan.app.name.to_string(),
            version: plan.app.version.to_string(),
            scope: plan.scope,
            install_directory: plan.install_directory.to_string(),
            selected_components: plan.selected_components.clone(),
            estimated_bytes: summary.install_bytes,
            download_bytes: summary.download_bytes,
            storage: None,
            requires_authorization: summary.requires_authorization,
            groups: groups
                .into_iter()
                .map(|(category, changes)| ChangeGroup {
                    category,
                    title: category.title().into(),
                    changes,
                })
                .collect(),
            requirements: requirements(&plan.prerequisites),
        })
    }

    pub fn from_transaction_plan(
        plan: &zup_plan::DeploymentPlan,
        scope: SelectedScope,
        target: Option<&zup_plan::BoundDeployment>,
    ) -> Result<Self, zup_plan::ContractError> {
        let mut groups = BTreeMap::<ResourceCategory, Vec<PlannedChange>>::new();
        let mut estimated_bytes = 0u64;
        for node in &plan.nodes {
            match &node.kind {
                zup_plan::NodeKind::Cleanup { operation } => {
                    let (label, location) = match &operation.candidate {
                        zup_plan::CleanupCandidate::Generation(generation) => (
                            "Collect retired generation when unused",
                            Some(generation.materialization.directory().to_string()),
                        ),
                        zup_plan::CleanupCandidate::Fixed(file) => (
                            "Collect unreferenced recovery file",
                            Some(file.path.to_string()),
                        ),
                        zup_plan::CleanupCandidate::History(history) => (
                            "Collect unreferenced deployment history",
                            Some(history.source.to_string()),
                        ),
                    };
                    groups
                        .entry(ResourceCategory::Application)
                        .or_default()
                        .push(PlannedChange {
                            category: ResourceCategory::Application,
                            kind: ChangeKind::Remove,
                            label: label.into(),
                            location,
                            scope: Some(scope),
                            requires_authorization: scope == SelectedScope::Machine,
                            estimated_bytes: 0,
                            component: None,
                            technical_key: Some(node.id.to_string()),
                        });
                }
                zup_plan::NodeKind::Retention { selection } => {
                    for snapshot in &selection.forget {
                        groups
                            .entry(ResourceCategory::Application)
                            .or_default()
                            .push(PlannedChange {
                                category: ResourceCategory::Application,
                                kind: ChangeKind::Remove,
                                label: format!("Forget collected revision {}", snapshot.revision),
                                location: None,
                                scope: Some(scope),
                                requires_authorization: scope == SelectedScope::Machine,
                                estimated_bytes: 0,
                                component: None,
                                technical_key: Some(node.id.to_string()),
                            });
                    }
                    for snapshot in &selection.retire {
                        groups
                            .entry(ResourceCategory::Application)
                            .or_default()
                            .push(PlannedChange {
                                category: ResourceCategory::Application,
                                kind: ChangeKind::Remove,
                                label: format!("Retire rollback revision {}", snapshot.revision),
                                location: None,
                                scope: Some(scope),
                                requires_authorization: scope == SelectedScope::Machine,
                                estimated_bytes: 0,
                                component: None,
                                technical_key: Some(node.id.to_string()),
                            });
                    }
                }
                zup_plan::NodeKind::PersistentDirectory { operation } => {
                    groups
                        .entry(ResourceCategory::Data)
                        .or_default()
                        .push(PlannedChange {
                            category: ResourceCategory::Data,
                            kind: if operation.stage == zup_plan::PersistentDirectoryStage::Access {
                                ChangeKind::Verify
                            } else if operation.intent.previous.is_some() {
                                ChangeKind::Preserve
                            } else {
                                ChangeKind::Create
                            },
                            label: operation.intent.directory.id.to_string(),
                            location: operation
                                .intent
                                .binding
                                .path(plan.target)
                                .ok()
                                .map(|path| path.to_string()),
                            scope: Some(scope),
                            requires_authorization: operation.intent.privilege
                                == zup_core::Privilege::System,
                            estimated_bytes: 0,
                            component: None,
                            technical_key: Some(node.id.to_string()),
                        });
                }
                zup_plan::NodeKind::NativeService { operation } => {
                    let (kind, label, name) = match operation.as_ref() {
                        zup_plan::NativeServiceOperation::Configure { service, .. }
                            if service.kind == zup_plan::IntegrationChangeKind::NoOp =>
                        {
                            (ChangeKind::Verify, "Verify native service", &service.name)
                        }
                        zup_plan::NativeServiceOperation::Configure { service, .. } => (
                            ChangeKind::Update,
                            "Configure native service",
                            &service.name,
                        ),
                        zup_plan::NativeServiceOperation::Remove { service, .. } => {
                            (ChangeKind::Remove, "Remove native service", &service.name)
                        }
                    };
                    groups
                        .entry(ResourceCategory::Services)
                        .or_default()
                        .push(PlannedChange {
                            category: ResourceCategory::Services,
                            kind,
                            label: label.into(),
                            location: Some(name.clone()),
                            scope: Some(scope),
                            requires_authorization: operation.privilege()
                                == zup_core::Privilege::System,
                            estimated_bytes: 0,
                            component: None,
                            technical_key: Some(format!("{:?}", operation.key())),
                        });
                }
                zup_plan::NodeKind::NativeCache { operation } => {
                    let category = ResourceCategory::FileAssociations;
                    groups.entry(category).or_default().push(PlannedChange {
                        category,
                        kind: ChangeKind::Update,
                        label: format!("Regenerate {}", operation.kind.title()),
                        location: Some(operation.directory.to_string()),
                        scope: Some(scope),
                        requires_authorization: false,
                        estimated_bytes: 0,
                        component: None,
                        technical_key: Some(node.id.to_string()),
                    });
                }
                zup_plan::NodeKind::Generation { operation, intent } => {
                    let (kind, label) = match operation {
                        zup_plan::GenerationOperation::Materialize => {
                            (ChangeKind::Prepare, "Prepare application files")
                        }
                        zup_plan::GenerationOperation::Seal => {
                            (ChangeKind::Seal, "Seal application generation")
                        }
                        zup_plan::GenerationOperation::Verify => {
                            (ChangeKind::Verify, "Verify retained application")
                        }
                        zup_plan::GenerationOperation::Activate => {
                            (ChangeKind::Activate, "Activate application")
                        }
                    };
                    let bytes = if *operation == zup_plan::GenerationOperation::Materialize {
                        intent.materialization.descriptor().size()
                    } else {
                        0
                    };
                    estimated_bytes = estimated_bytes.saturating_add(bytes);
                    groups
                        .entry(ResourceCategory::Application)
                        .or_default()
                        .push(PlannedChange {
                            category: ResourceCategory::Application,
                            kind,
                            label: label.into(),
                            location: Some(intent.materialization.directory().to_string()),
                            scope: Some(scope),
                            requires_authorization: intent.privilege == zup_core::Privilege::System,
                            estimated_bytes: bytes,
                            component: None,
                            technical_key: Some(
                                intent
                                    .materialization
                                    .descriptor()
                                    .tree_id()
                                    .digest()
                                    .to_hex(),
                            ),
                        });
                }
                zup_plan::NodeKind::RetainFile { key } => {
                    let category = if matches!(
                        key,
                        ResourceKey::File {
                            id: zup_core::FileResourceId::Maintenance { .. }
                        }
                    ) {
                        ResourceCategory::Maintenance
                    } else {
                        ResourceCategory::Files
                    };
                    groups.entry(category).or_default().push(PlannedChange {
                        category,
                        kind: ChangeKind::Verify,
                        label: "Verify retained file".into(),
                        location: node.file_destination().map(ToString::to_string),
                        scope: Some(scope),
                        requires_authorization: node.meta.privilege
                            == Some(zup_core::Privilege::System),
                        estimated_bytes: 0,
                        component: None,
                        technical_key: Some(format!("{key:?}")),
                    });
                }
                zup_plan::NodeKind::FileMutation { key, delta } => {
                    let category = if matches!(
                        key,
                        ResourceKey::File {
                            id: zup_core::FileResourceId::Maintenance { .. }
                        }
                    ) {
                        ResourceCategory::Maintenance
                    } else {
                        ResourceCategory::Files
                    };
                    let kind = match delta {
                        zup_plan::FileOperationKind::Create => ChangeKind::Create,
                        zup_plan::FileOperationKind::Replace
                        | zup_plan::FileOperationKind::RestoreOwned
                        | zup_plan::FileOperationKind::RepairOwned => ChangeKind::Update,
                        zup_plan::FileOperationKind::NoOp => ChangeKind::NoOp,
                        zup_plan::FileOperationKind::Drift => ChangeKind::Drift,
                        zup_plan::FileOperationKind::Conflict => ChangeKind::Conflict,
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
                zup_plan::NodeKind::FileRemoval { key } => {
                    let category = match key {
                        ResourceKey::File {
                            id: zup_core::FileResourceId::Maintenance { .. },
                        } => ResourceCategory::Maintenance,
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
                zup_plan::NodeKind::ServiceControl { operation } => {
                    groups
                        .entry(ResourceCategory::Services)
                        .or_default()
                        .push(PlannedChange {
                            category: ResourceCategory::Services,
                            kind: ChangeKind::Update,
                            label: match operation.stage {
                                zup_plan::ServiceControlStage::Quiesce => "Stop service",
                                zup_plan::ServiceControlStage::Restore => "Start service",
                            }
                            .into(),
                            location: Some(operation.native_name.clone()),
                            scope: Some(scope),
                            requires_authorization: operation.privilege
                                == zup_core::Privilege::System,
                            estimated_bytes: 0,
                            component: None,
                            technical_key: Some(format!("{:?}", operation.key)),
                        });
                }
                zup_plan::NodeKind::BackendOperation { key, .. }
                | zup_plan::NodeKind::BackendRemoval { key } => {
                    groups
                        .entry(ResourceCategory::AppsFeatures)
                        .or_default()
                        .push(PlannedChange {
                            category: ResourceCategory::AppsFeatures,
                            kind: if matches!(node.kind, zup_plan::NodeKind::BackendRemoval { .. })
                            {
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
                zup_plan::NodeKind::ReserveInstallation
                | zup_plan::NodeKind::PublishInstallation => {
                    let reservation = matches!(node.kind, zup_plan::NodeKind::ReserveInstallation);
                    groups
                        .entry(ResourceCategory::Application)
                        .or_default()
                        .push(PlannedChange {
                            category: ResourceCategory::Application,
                            kind: if reservation {
                                ChangeKind::Verify
                            } else {
                                ChangeKind::Update
                            },
                            label: if reservation {
                                "Confirm installation ownership"
                            } else {
                                "Commit installation record"
                            }
                            .into(),
                            location: None,
                            scope: Some(scope),
                            requires_authorization: scope == zup_core::SelectedScope::Machine,
                            estimated_bytes: 0,
                            component: None,
                            technical_key: None,
                        });
                }
                zup_plan::NodeKind::StageFile { .. } | zup_plan::NodeKind::Barrier => {}
            }
        }
        Ok(Self {
            basis: PlanBasis::ObservedMachine,
            application: target.map_or_else(
                || plan.requirements.app.id.to_string(),
                |target| target.app.name.to_string(),
            ),
            version: target.map_or_else(
                || plan.requirements.app.version.to_string(),
                |target| target.app.version.to_string(),
            ),
            scope,
            install_directory: target.map_or_else(
                || {
                    plan.install_directory
                        .as_ref()
                        .map(ToString::to_string)
                        .unwrap_or_default()
                },
                |target| target.install_directory.to_string(),
            ),
            selected_components: target.map_or_else(
                || plan.selected_components.clone(),
                |target| target.selected_components.clone(),
            ),
            estimated_bytes,
            storage: Some(plan.storage_estimate()),
            download_bytes: target
                .map(|target| target.summary())
                .transpose()?
                .map_or(0, |summary| summary.download_bytes),
            requires_authorization: plan.requires_authorization(),
            groups: groups
                .into_iter()
                .map(|(category, changes)| ChangeGroup {
                    category,
                    title: category.title().into(),
                    changes,
                })
                .collect(),
            requirements: requirements(target.map_or(&[], |target| &target.prerequisites)),
        })
    }

    pub fn human(&self) -> String {
        let mut output = String::new();
        let _ = writeln!(
            output,
            "{}",
            match self.basis {
                PlanBasis::DesiredState => "Desired deployment (machine state not inspected)",
                PlanBasis::Simulation => "Simulated deployment (machine state not inspected)",
                PlanBasis::ObservedMachine => "Changes against observed machine state",
            }
        );
        let _ = writeln!(output, "{} {}", self.application, self.version);
        let _ = writeln!(output, "Scope: {}", self.scope);
        if !self.install_directory.is_empty() {
            let _ = writeln!(output, "Location: {}", self.install_directory);
        }
        let _ = writeln!(output, "Install: {}", format_bytes(self.estimated_bytes));
        if let Some(storage) = self.storage {
            let _ = writeln!(
                output,
                "Additional payload: {}",
                format_bytes(storage.additional_payload_bytes())
            );
            let _ = writeln!(
                output,
                "Verified reuse: {}",
                format_bytes(storage.verified_reuse_bytes)
            );
            let _ = writeln!(
                output,
                "Recovery files retained: {}",
                format_bytes(storage.retained_fixed_bytes)
            );
            let _ = writeln!(
                output,
                "Cleanup candidates: {} (collection may be deferred)",
                format_bytes(storage.cleanup_candidate_bytes)
            );
            let _ = writeln!(
                output,
                "Logical payload sizes exclude downloads, filesystem overhead, caches and mutable data."
            );
        }
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
                    .map(|value| format!(" - {value}"))
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

fn requirements(prerequisites: &[zup_plan::PlannedPrerequisite]) -> Vec<RequirementPresentation> {
    prerequisites
        .iter()
        .map(|prerequisite| RequirementPresentation {
            id: prerequisite.id.to_string(),
            name: prerequisite.name.to_string(),
            status: RequirementStatus::Unknown,
            estimated_bytes: prerequisite.package.size().unwrap_or(0),
            shared: true,
        })
        .collect()
}

pub use zup_preset_protocol::{
    ChangeKind, OperationPhase, PlanBasis, RequirementPresentation, RequirementStatus,
    ResourceCategory,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgressPresentation {
    pub phase: OperationPhase,
    pub completed: u64,
    pub total: u64,
    pub label: String,
}

impl ProgressPresentation {
    pub fn new(
        phase: OperationPhase,
        completed: u64,
        total: u64,
        label: impl Into<String>,
    ) -> Self {
        let label = label.into();
        Self {
            phase,
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
    pub fn for_outcome(outcome: ProcessOutcome, message: &str) -> Self {
        let (kind, title, meaning, recovery) = match outcome {
            ProcessOutcome::RecoveryRequired => (
                DiagnosticKind::Recovery,
                "The last change didn't finish",
                "Setup must reconcile transaction state before making further changes.",
                "Keep the installer and transaction state. Complete recovery before starting another operation.",
            ),
            ProcessOutcome::InstallationBusy => (
                DiagnosticKind::Blocked,
                "Another operation is running",
                "This installation is in use by another maintenance operation.",
                "Wait for that operation to finish, then retry.",
            ),
            ProcessOutcome::RebootRequired => (
                DiagnosticKind::Unknown,
                "A restart is required",
                "The operation cannot finish until the machine restarts.",
                "Restart the machine before continuing.",
            ),
            ProcessOutcome::AuthorizationRequired => (
                DiagnosticKind::Permission,
                "Native authorization is required",
                "Setup has not received the authority needed for these changes.",
                "Use the native authorization flow or ask an administrator to install it.",
            ),
            ProcessOutcome::VerificationFailure => (
                DiagnosticKind::Verification,
                "Content could not be verified",
                "Setup refused content that did not pass verification.",
                "Obtain the installer from the publisher and check the reported verification failure.",
            ),
            ProcessOutcome::OwnershipConflict => (
                DiagnosticKind::Conflict,
                "An existing resource conflicts with this plan",
                "Setup will not replace resources it does not own.",
                "Resolve the ownership conflict or choose another installation location, then retry.",
            ),
            _ => (
                DiagnosticKind::Unknown,
                "Setup couldn't finish",
                "Setup could not confirm that this operation completed.",
                "Copy the details for the publisher before retrying.",
            ),
        };
        Self {
            kind,
            title: title.into(),
            meaning: meaning.into(),
            recovery: recovery.into(),
            technical_details: Some(message.into()),
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
    /// else is installing this right now" from "this installation is broken" -
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
}

// # The installed application's machine protocol
//
// `--output json` and `--output jsonl` on a generated installer report through
// [`InstallerEvent`] and [`InstallerResult`]. This is the *runtime's* protocol - what
// happened while installing, modifying, repairing, updating or uninstalling an
// application on a user's own machine - and it is deliberately not the same protocol
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
    PlanPrepared {
        preview: Box<PlanPreview>,
    },
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery_assessment: Option<Box<zup_runtime::RecoveryAssessment>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transaction: Option<String>,
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
            recovery_assessment: None,
            transaction: None,
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

    pub fn apply_outcome(&mut self, outcome: &zup_runtime::InstallOutcome) {
        use zup_runtime::InstallOutcome;
        self.outcome = ProcessOutcome::from(outcome);
        self.code = self.outcome.code();
        let message = match outcome {
            InstallOutcome::RecoveryInspected { target, .. } => {
                target.as_ref().map(|target| target.detail.clone())
            }
            InstallOutcome::RecoveryAssessed { assessment } => {
                self.recovery_assessment = Some(assessment.clone());
                self.transaction = Some(assessment.transaction.to_string());
                Some(
                    serde_json::to_string_pretty(assessment)
                        .expect("recovery assessment serializes"),
                )
            }
            InstallOutcome::Committed => None,
            InstallOutcome::Recovered { settlement } => Some(
                match settlement {
                    zup_runtime::RecoverySettlement::Committed => {
                        "Recovery committed the deployment"
                    }
                    zup_runtime::RecoverySettlement::RolledBack => {
                        "Recovery restored the previous deployment"
                    }
                }
                .into(),
            ),
            InstallOutcome::RecoveryRequired {
                transaction,
                detail,
            } => {
                self.transaction = transaction.map(|transaction| transaction.to_string());
                Some(detail.clone())
            }
            InstallOutcome::CommittedPendingReconciliation {
                transaction,
                detail,
            } => {
                self.transaction = Some(transaction.to_string());
                Some(detail.clone())
            }
            InstallOutcome::CommittedWithWarning { detail } => Some(detail.clone()),
            InstallOutcome::Failed(message) => Some(message.clone()),
            InstallOutcome::Busy { operation } => {
                Some(format!("Another {operation} operation is running"))
            }
            InstallOutcome::RebootRequired {
                prerequisite_id,
                exit_code,
            } => Some(format!(
                "Restart required by {prerequisite_id} ({exit_code})"
            )),
            InstallOutcome::AuthorizationRequired => {
                Some("Administrator authorization is required".into())
            }
            InstallOutcome::AuthorizationCancelled => {
                Some("Administrator authorization was cancelled".into())
            }
            InstallOutcome::Cancelled => Some("The operation was cancelled".into()),
            InstallOutcome::RolledBack => self
                .message
                .is_none()
                .then(|| "The operation failed and was rolled back".into()),
        };
        self.message = match (message, self.message.take()) {
            (Some(message), Some(detail)) if !message.contains(&detail) => {
                Some(format!("{message}\n{detail}"))
            }
            (message, detail) => message.or(detail),
        };
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

impl From<zup_runtime::CompletionOutcome> for ProcessOutcome {
    fn from(outcome: zup_runtime::CompletionOutcome) -> Self {
        match outcome {
            zup_runtime::CompletionOutcome::Committed
            | zup_runtime::CompletionOutcome::Recovered { .. } => Self::Success,
            zup_runtime::CompletionOutcome::RebootRequired => Self::RebootRequired,
            zup_runtime::CompletionOutcome::Cancelled => Self::Cancelled,
            zup_runtime::CompletionOutcome::RolledBack => Self::Failure,
        }
    }
}

impl From<zup_runtime::FailureKind> for ProcessOutcome {
    fn from(kind: zup_runtime::FailureKind) -> Self {
        match kind {
            zup_runtime::FailureKind::InstallationBusy => Self::InstallationBusy,
            zup_runtime::FailureKind::RecoveryRequired
            | zup_runtime::FailureKind::CommittedPendingReconciliation => Self::RecoveryRequired,
            zup_runtime::FailureKind::AuthorizationRequired
            | zup_runtime::FailureKind::AuthorizationCancelled => Self::AuthorizationRequired,
            zup_runtime::FailureKind::Transaction | zup_runtime::FailureKind::Session => {
                Self::Failure
            }
        }
    }
}

impl From<&zup_runtime::InstallOutcome> for ProcessOutcome {
    fn from(outcome: &zup_runtime::InstallOutcome) -> Self {
        use zup_runtime::InstallOutcome;
        match outcome {
            InstallOutcome::RecoveryAssessed { .. }
            | InstallOutcome::RecoveryInspected { target: None, .. } => Self::Success,
            InstallOutcome::RecoveryInspected {
                target: Some(_), ..
            } => Self::RecoveryRequired,
            InstallOutcome::RebootRequired { .. } => Self::RebootRequired,
            InstallOutcome::Committed
            | InstallOutcome::Recovered { .. }
            | InstallOutcome::CommittedWithWarning { .. } => Self::Success,
            InstallOutcome::Cancelled => Self::Cancelled,
            InstallOutcome::RecoveryRequired { .. }
            | InstallOutcome::CommittedPendingReconciliation { .. } => Self::RecoveryRequired,
            InstallOutcome::AuthorizationRequired | InstallOutcome::AuthorizationCancelled => {
                Self::AuthorizationRequired
            }
            InstallOutcome::Busy { .. } => Self::InstallationBusy,
            InstallOutcome::RolledBack | InstallOutcome::Failed(_) => Self::Failure,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_results_preserve_transaction_and_native_diagnostic() {
        let transaction = "00000000-0000-7000-8000-000000000001".parse().unwrap();
        for outcome in [
            zup_runtime::InstallOutcome::RecoveryRequired {
                transaction: Some(transaction),
                detail: "Native startup admission is closed".into(),
            },
            zup_runtime::InstallOutcome::CommittedPendingReconciliation {
                transaction: zup_core::TransactionId::from_uuid(transaction),
                detail: "Native startup admission is closed".into(),
            },
        ] {
            let mut report =
                InstallerResult::new(ProcessOutcome::Failure, "com.example.app", "1.0.0");
            report.apply_outcome(&outcome);
            let json: serde_json::Value = serde_json::from_str(&report.to_json().unwrap()).unwrap();
            assert_eq!(json["transaction"], "00000000-0000-7000-8000-000000000001");
            assert_eq!(json["message"], "Native startup admission is closed");
            assert_eq!(report.outcome, ProcessOutcome::RecoveryRequired);
        }
    }

    #[test]
    fn transaction_preview_reports_resolved_requirements_and_downloads() {
        let target = zup_core::Target::parse("linux-x64").unwrap();
        let directory = zup_core::TargetPath::new(target, "/opt/app").unwrap();
        let resolved: zup_plan::DesiredState<zup_core::TargetPath, zup_core::TargetPath> =
            serde_json::from_value(serde_json::json!({
                "requirements": null, "maintenance": null,
                "app": {"id": "com.example.app", "name": "App", "version": "1.0.0"},
                "target": target, "scope": "user", "install_directory": directory,
                "installation": "00000000-0000-7000-8000-000000000001", "generation": null,
                "generation_readers": "untracked",
                "selected_components": ["core"],
                "prerequisites": [{
                    "id": "runtime", "name": "Runtime", "target": "x64",
                    "requirement": {"kind": "runtime", "id": "test.runtime"},
                    "package": {"type": "remote", "url": "https://example.invalid/runtime",
                        "sha256": zup_core::hash_bytes(b"runtime"), "size": 2048,
                        "filename": "runtime.pkg"},
                    "installer": {}
                }],
                "data": [], "files": [], "launchers": [], "path_entries": [], "services": [],
                "protocols": [], "file_associations": [], "preset": null
            }))
            .unwrap();
        let resolved = resolved.bind_fixed().unwrap();
        let mut effects =
            zup_plan::DeploymentEffects::new(resolved.installation, resolved.requirements.clone());
        effects.install_directory = Some(directory);
        let transaction = zup_plan::compile_deployment(&effects).unwrap();
        let preview =
            PlanPreview::from_transaction_plan(&transaction, SelectedScope::User, Some(&resolved))
                .unwrap();
        assert_eq!(preview.application, "App");
        assert_eq!(preview.install_directory, "/opt/app");
        assert_eq!(preview.selected_components, resolved.selected_components);
        assert_eq!(preview.download_bytes, 2048);
        assert_eq!(preview.requirements.len(), 1);
        assert_eq!(preview.requirements[0].id, "runtime");
        assert_eq!(preview.requirements[0].estimated_bytes, 2048);
        let without_target =
            PlanPreview::from_transaction_plan(&transaction, SelectedScope::User, None).unwrap();
        assert!(without_target.requirements.is_empty());
        assert_eq!(without_target.download_bytes, 0);
    }

    #[test]
    fn diagnostics_follow_typed_outcomes() {
        for message in ["blocked by running applications", "ownership drift"] {
            assert_eq!(
                DiagnosticPresentation::for_outcome(ProcessOutcome::RecoveryRequired, message).kind,
                DiagnosticKind::Recovery
            );
        }
        assert_eq!(
            DiagnosticPresentation::for_outcome(
                ProcessOutcome::InstallationBusy,
                "ownership drift"
            )
            .kind,
            DiagnosticKind::Blocked
        );
        assert_eq!(
            DiagnosticPresentation::for_outcome(
                ProcessOutcome::OwnershipConflict,
                "running application"
            )
            .kind,
            DiagnosticKind::Conflict
        );
        assert_eq!(
            DiagnosticPresentation::for_outcome(
                ProcessOutcome::InstallationBusy,
                "blocked by running applications"
            )
            .kind,
            DiagnosticKind::Blocked
        );
    }

    #[test]
    fn automation_contract_has_stable_outcome_codes() {
        assert_eq!(ProcessOutcome::Success.code(), 0);
        assert_eq!(ProcessOutcome::Cancelled.code(), 2);
        assert_eq!(ProcessOutcome::OwnershipConflict.code(), 4);
        assert_eq!(ProcessOutcome::AuthorizationRequired.code(), 5);
        assert_eq!(ProcessOutcome::VerificationFailure.code(), 6);
        assert_eq!(ProcessOutcome::RecoveryRequired.code(), 7);
    }

    #[test]
    fn automation_jsonl_is_explicit_and_versioned() {
        let events = [
            InstallerEvent::started("Acme", "1.0.0", "install"),
            InstallerEvent::progress(&ProgressPresentation::new(
                OperationPhase::Files,
                1,
                2,
                "Installing files",
            )),
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
