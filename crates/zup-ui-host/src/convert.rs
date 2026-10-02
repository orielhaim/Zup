//! Internal engine model to public protocol model.
//!
//! Every crossing between the two is here, in one place, as an explicit
//! conversion. The engine's types are not the protocol's types, deliberately:
//! `zup_core::ComponentId` and the protocol's `ComponentId` will drift apart
//! the moment the engine renames something, and that drift must cost a change
//! here rather than every published preset at once.

use zup_core::{ComponentId as EngineComponentId, SelectedScope};
use zup_presentation::OperationPhase as EnginePhase;
use zup_ui_protocol::{
    ChangeGroup, ChangeKind, ComponentId, ComponentProminence, DiagnosticKind,
    DiagnosticPresentation, InstallScope, OperationPhase, PlanPreview, PlannedChange,
    RequirementPresentation, RequirementStatus, ResourceCategory, SelectionRequirement,
};

/// The protocol's scope for an engine scope.
pub fn scope(value: SelectedScope) -> InstallScope {
    match value {
        SelectedScope::User => InstallScope::User,
        SelectedScope::Machine => InstallScope::Machine,
    }
}

/// The engine's scope for a protocol scope.
pub fn engine_scope(value: InstallScope) -> SelectedScope {
    match value {
        InstallScope::User => SelectedScope::User,
        InstallScope::Machine => SelectedScope::Machine,
    }
}

pub fn component(value: &EngineComponentId) -> ComponentId {
    ComponentId::new(value.as_str()).expect("an engine component id is never empty")
}

pub fn prominence(value: zup_core::ComponentProminence) -> ComponentProminence {
    match value {
        zup_core::ComponentProminence::Auto => ComponentProminence::Auto,
        zup_core::ComponentProminence::Primary => ComponentProminence::Primary,
        zup_core::ComponentProminence::Secondary => ComponentProminence::Secondary,
    }
}

pub fn selection(value: zup_core::SelectionRequirement) -> SelectionRequirement {
    match value {
        zup_core::SelectionRequirement::Defaulted => SelectionRequirement::Defaulted,
        zup_core::SelectionRequirement::Explicit => SelectionRequirement::Explicit,
    }
}

pub fn components<'a>(values: impl IntoIterator<Item = &'a EngineComponentId>) -> Vec<ComponentId> {
    values.into_iter().map(component).collect()
}

pub fn phase(value: EnginePhase) -> OperationPhase {
    match value {
        EnginePhase::Prepare => OperationPhase::Prepare,
        EnginePhase::Download => OperationPhase::Download,
        EnginePhase::Verify => OperationPhase::Verify,
        EnginePhase::Files => OperationPhase::Files,
        EnginePhase::System => OperationPhase::System,
        EnginePhase::Finish => OperationPhase::Finish,
    }
}

pub fn requirement(value: &zup_presentation::RequirementPresentation) -> RequirementPresentation {
    RequirementPresentation {
        id: value.id.clone(),
        name: value.name.clone(),
        status: match value.status {
            zup_presentation::RequirementStatus::Satisfied => RequirementStatus::Satisfied,
            zup_presentation::RequirementStatus::Missing => RequirementStatus::Missing,
            zup_presentation::RequirementStatus::Unknown => RequirementStatus::Unknown,
        },
        estimated_bytes: value.estimated_bytes,
        shared: value.shared,
    }
}

fn category(value: zup_presentation::ResourceCategory) -> ResourceCategory {
    use zup_presentation::ResourceCategory as Engine;
    match value {
        Engine::Files => ResourceCategory::Files,
        Engine::Launchers => ResourceCategory::Launchers,
        Engine::Path => ResourceCategory::Path,
        Engine::Services => ResourceCategory::Services,
        Engine::Protocols => ResourceCategory::Protocols,
        Engine::FileAssociations => ResourceCategory::FileAssociations,
        Engine::AppsFeatures => ResourceCategory::AppsFeatures,
        Engine::Maintenance => ResourceCategory::Maintenance,
        Engine::Prerequisites => ResourceCategory::Prerequisites,
        Engine::Other => ResourceCategory::Other,
    }
}

fn change_kind(value: zup_presentation::ChangeKind) -> ChangeKind {
    use zup_presentation::ChangeKind as Engine;
    match value {
        Engine::Create => ChangeKind::Create,
        Engine::Update => ChangeKind::Update,
        Engine::Remove => ChangeKind::Remove,
        Engine::NoOp => ChangeKind::NoChange,
        Engine::Drift => ChangeKind::Drifted,
        Engine::Conflict => ChangeKind::Conflict,
    }
}

fn change(value: &zup_presentation::PlannedChange) -> PlannedChange {
    PlannedChange {
        category: category(value.category),
        kind: change_kind(value.kind),
        label: value.label.clone(),
        location: value.location.clone(),
        scope: value.scope.map(scope),
        requires_authorization: value.requires_authorization,
        estimated_bytes: value.estimated_bytes,
        component: value.component.as_ref().map(component),
    }
}

fn group(value: &zup_presentation::ChangeGroup) -> ChangeGroup {
    ChangeGroup {
        category: category(value.category),
        changes: value.changes.iter().map(change).collect(),
    }
}

/// What this machine would do, as a preset reads it.
///
/// The application's name and version are not repeated here: they are in the
/// snapshot's product identity, and a preview that carried its own copy would
/// let a preset render two names for one application.
pub fn plan(value: &zup_presentation::PlanPreview) -> PlanPreview {
    PlanPreview {
        scope: scope(value.scope),
        install_directory: value.install_directory.clone(),
        selected_components: components(&value.selected_components),
        estimated_bytes: value.estimated_bytes,
        download_bytes: value.download_bytes,
        requires_authorization: value.requires_authorization,
        groups: value.groups.iter().map(group).collect(),
        requirements: value.requirements.iter().map(requirement).collect(),
    }
}

pub fn diagnostic(value: &zup_presentation::DiagnosticPresentation) -> DiagnosticPresentation {
    use zup_presentation::DiagnosticKind as Engine;
    DiagnosticPresentation {
        kind: match value.kind {
            Engine::Blocked => DiagnosticKind::Blocked,
            Engine::Conflict => DiagnosticKind::Conflict,
            Engine::Drift => DiagnosticKind::Drift,
            Engine::Permission => DiagnosticKind::Permission,
            Engine::Recovery => DiagnosticKind::Recovery,
            Engine::Verification => DiagnosticKind::Verification,
            Engine::Unknown => DiagnosticKind::Unknown,
        },
        title: value.title.clone(),
        meaning: value.meaning.clone(),
        recovery: value.recovery.clone(),
        technical_details: value.technical_details.clone(),
    }
}
