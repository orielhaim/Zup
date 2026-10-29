use zup_core::{ComponentId as EngineComponentId, SelectedScope};
use zup_preset_protocol::{
    ChangeGroup, ComponentId, ComponentProminence, DiagnosticKind, DiagnosticPresentation,
    InstallScope, PlanPreview, PlannedChange, SelectionRequirement,
};

pub fn scope(value: SelectedScope) -> InstallScope {
    match value {
        SelectedScope::User => InstallScope::User,
        SelectedScope::Machine => InstallScope::Machine,
    }
}

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

fn change(value: &zup_presentation::PlannedChange) -> PlannedChange {
    PlannedChange {
        category: value.category,
        kind: value.kind,
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
        category: value.category,
        changes: value.changes.iter().map(change).collect(),
    }
}

pub fn plan(value: &zup_presentation::PlanPreview) -> PlanPreview {
    PlanPreview {
        scope: scope(value.scope),
        install_directory: value.install_directory.clone(),
        selected_components: components(&value.selected_components),
        estimated_bytes: value.estimated_bytes,
        download_bytes: value.download_bytes,
        requires_authorization: value.requires_authorization,
        groups: value.groups.iter().map(group).collect(),
        requirements: value.requirements.clone(),
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
