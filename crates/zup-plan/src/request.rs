use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use zup_core::{ComponentId, Target, Template};

use crate::error::SelectedScope;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GenerationPolicy {
    ReuseVerified,
    Reconstruct,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GenerationSelection {
    pub materialization: zup_core::MaterializationId,
    pub policy: GenerationPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeploymentSelection {
    pub installation: zup_core::InstallationId,
    pub previous: Option<crate::ActivationSelection>,
    pub generation: Option<GenerationSelection>,
    pub maintenance: Option<GenerationSelection>,
}

impl DeploymentSelection {
    pub fn from_plan(plan: &crate::DeploymentPlan) -> Self {
        Self {
            installation: plan.installation,
            previous: plan.observed,
            generation: plan.nodes.iter().find_map(|node| match &node.kind {
                crate::NodeKind::Generation {
                    operation: crate::GenerationOperation::Activate,
                    intent,
                } => Some(GenerationSelection {
                    materialization: intent.materialization.id(),
                    policy: match intent.preparation {
                        crate::GenerationPreparation::Materialize => GenerationPolicy::Reconstruct,
                        crate::GenerationPreparation::ReuseVerified => {
                            GenerationPolicy::ReuseVerified
                        }
                    },
                }),
                _ => None,
            }),
            maintenance: plan.nodes.iter().find_map(|node| match &node.kind {
                crate::NodeKind::Generation { intent, .. }
                    if intent.publication == crate::GenerationPublication::Maintenance =>
                {
                    Some(GenerationSelection {
                        materialization: intent.materialization.id(),
                        policy: match intent.preparation {
                            crate::GenerationPreparation::Materialize => {
                                GenerationPolicy::Reconstruct
                            }
                            crate::GenerationPreparation::ReuseVerified => {
                                GenerationPolicy::ReuseVerified
                            }
                        },
                    })
                }
                _ => None,
            }),
        }
    }

    pub fn validate(
        &self,
        previous: Option<&crate::InstallationRecord>,
    ) -> Result<(), crate::BindingMismatch> {
        if self.previous != previous.map(|previous| previous.current.selection())
            || previous.is_some_and(|previous| {
                previous.id != self.installation || previous.validate().is_err()
            })
        {
            return Err(crate::BindingMismatch);
        }
        Ok(())
    }

    pub fn apply_to(&self, request: &mut PlanRequest) {
        request.installation = self.installation;
        request.previous = self.previous;
        if let Some(generation) = &self.generation {
            request.materialization = generation.materialization;
            request.generation_policy = generation.policy;
        }
        request.maintenance = self.maintenance.clone();
    }
}

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
#[serde(tag = "source", content = "path", rename_all = "snake_case")]
pub enum InstallDirectory {
    Override(Template),
    Installed(Template),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanRequest {
    pub maintenance: Option<GenerationSelection>,
    pub generation_policy: GenerationPolicy,
    pub installation: zup_core::InstallationId,
    pub materialization: zup_core::MaterializationId,
    pub previous: Option<crate::ActivationSelection>,
    pub target: Target,
    pub scope: SelectedScope,
    #[serde(default)]
    pub components: ComponentOverrides,
    #[serde(default)]
    pub install_directory: Option<InstallDirectory>,
}

impl PlanRequest {
    pub fn new(target: Target, scope: SelectedScope) -> Self {
        Self {
            maintenance: None,
            generation_policy: GenerationPolicy::ReuseVerified,
            installation: zup_core::InstallationId::new_v7(),
            materialization: zup_core::MaterializationId::new_v7(),
            previous: None,
            target,
            scope,
            components: ComponentOverrides::none(),
            install_directory: None,
        }
    }
}
