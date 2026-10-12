use std::collections::{BTreeMap, BTreeSet};

use zup_core::{Component, ComponentId};

use crate::error::PlanError;
use crate::request::ComponentOverrides;

pub fn select_components(
    components: &[Component],
    overrides: &ComponentOverrides,
) -> Result<Vec<ComponentId>, PlanError> {
    let index: BTreeMap<ComponentId, &Component> = components
        .iter()
        .map(|component| (component.id.clone(), component))
        .collect();

    for id in overrides.enable.iter().chain(overrides.disable.iter()) {
        if !index.contains_key(id) {
            return Err(PlanError::UnknownComponentOverride { id: id.clone() });
        }
    }
    for id in &overrides.enable {
        if overrides.disable.contains(id) {
            return Err(PlanError::ComponentBothEnabledAndDisabled { id: id.clone() });
        }
    }
    for id in &overrides.disable {
        if index.get(id).is_some_and(|component| component.required) {
            return Err(PlanError::RequiredComponentDisabled { id: id.clone() });
        }
    }

    let mut wanted: BTreeSet<ComponentId> = BTreeSet::new();
    for component in components {
        let explicitly_disabled = overrides.disable.contains(&component.id);
        if component.required || (component.default && !explicitly_disabled) {
            wanted.insert(component.id.clone());
        }
    }
    for id in &overrides.enable {
        wanted.insert(id.clone());
    }

    let mut selected = BTreeSet::new();
    let mut stack: Vec<ComponentId> = components
        .iter()
        .filter(|component| wanted.contains(&component.id))
        .map(|component| component.id.clone())
        .collect();

    while let Some(id) = stack.pop() {
        if !selected.insert(id.clone()) {
            continue;
        }
        if let Some(component) = index.get(&id) {
            for required in &component.requires {
                if !selected.contains(required) {
                    stack.push(required.clone());
                }
            }
        }
    }

    for id in &overrides.disable {
        if selected.contains(id) {
            return Err(PlanError::DependencyExplicitlyDisabled { id: id.clone() });
        }
    }

    Ok(components
        .iter()
        .filter(|component| selected.contains(&component.id))
        .map(|component| component.id.clone())
        .collect())
}
