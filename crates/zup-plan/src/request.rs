//! Plan request and component overrides.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use zup_core::{ComponentId, TargetTriple, Template};

use crate::error::SelectedScope;

/// Explicit component enable/disable overrides.
///
/// Empty sets mean “use manifest defaults”.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ComponentOverrides {
    /// Force-select these components (and their dependency closure).
    pub enable: BTreeSet<ComponentId>,
    /// Force-deselect these components (unless required or needed).
    pub disable: BTreeSet<ComponentId>,
}

impl ComponentOverrides {
    /// No overrides: use manifest defaults only.
    pub const fn none() -> Self {
        Self {
            enable: BTreeSet::new(),
            disable: BTreeSet::new(),
        }
    }
}

/// Everything needed to compute one desired installation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanRequest {
    pub target: TargetTriple,
    pub scope: SelectedScope,
    #[serde(default)]
    pub components: ComponentOverrides,
    #[serde(default)]
    pub install_directory: Option<Template>,
}

impl PlanRequest {
    /// Request a target with manifest-default component selection.
    pub fn new(target: TargetTriple, scope: SelectedScope) -> Self {
        Self {
            target,
            scope,
            components: ComponentOverrides::none(),
            install_directory: None,
        }
    }
}
