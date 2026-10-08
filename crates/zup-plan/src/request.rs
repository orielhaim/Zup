use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use zup_core::{ComponentId, TargetTriple, Template};

use crate::error::SelectedScope;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ComponentOverrides {
    pub enable: BTreeSet<ComponentId>,
    pub disable: BTreeSet<ComponentId>,
}

impl ComponentOverrides {
    pub const fn none() -> Self {
        Self {
            enable: BTreeSet::new(),
            disable: BTreeSet::new(),
        }
    }
}

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
    pub fn new(target: TargetTriple, scope: SelectedScope) -> Self {
        Self {
            target,
            scope,
            components: ComponentOverrides::none(),
            install_directory: None,
        }
    }
}
