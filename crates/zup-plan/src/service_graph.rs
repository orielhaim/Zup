use std::collections::BTreeSet;
use zup_core::ResourceKey;

impl crate::ExecutionPlan {
    /// Includes dependents whose payload is unchanged, using both authorized generations.
    pub fn affected_services(
        &self,
        ledger: Option<&crate::InstallationRecord>,
    ) -> BTreeSet<ResourceKey> {
        let mut affected: BTreeSet<_> = self
            .services
            .iter()
            .filter(|service| {
                let data_for = |requirements: &crate::DeploymentRequirements| {
                    requirements
                        .data
                        .iter()
                        .filter(|data| data.location.services().any(|id| id.as_str() == service.id))
                        .cloned()
                        .collect::<Vec<_>>()
                };
                let data_changed = self.binding.as_ref().is_some_and(|binding| {
                    data_for(&binding.requirements)
                        != ledger
                            .map(|ledger| data_for(&ledger.current.requirements))
                            .unwrap_or_default()
                });
                data_changed
                    || (service.previous.configuration().as_ref()
                        != Some(&service.desired_configuration())
                        && (!service.preserves_runtime()
                            || service.lifecycle.change == zup_core::ChangedServiceState::Stopped))
                    || service.affected_by_files(
                        &self.files,
                        &self.removals,
                        ledger.and_then(|ledger| ledger.current.resources.get(&service.key)),
                    )
            })
            .map(|service| service.key.clone())
            .collect();
        affected.extend(self.removals.iter().filter_map(|removal| {
            matches!(removal.owned, crate::OwnedResource::Service(_)).then_some(removal.key.clone())
        }));
        loop {
            let before = affected.len();
            for service in &self.services {
                let previous = ledger.and_then(|ledger| ledger.current.resources.get(&service.key));
                let old = match previous {
                    Some(crate::OwnedResource::Service(crate::OwnedService {
                        requires_services,
                        ..
                    })) => requires_services.as_slice(),
                    _ => &[],
                };
                if service.requires_services.iter().chain(old).any(|id| {
                    affected.contains(
                        &ResourceKey::Service { id: id.clone() }
                            .canonical_for(service.command.executable.target()),
                    )
                }) {
                    affected.insert(service.key.clone());
                }
            }
            if affected.len() == before {
                return affected;
            }
        }
    }

    pub fn service_graph(
        &self,
        ledger: Option<&crate::InstallationRecord>,
        previous: bool,
    ) -> zup_core::ServiceGraph {
        if previous {
            let Some(ledger) = ledger else {
                return zup_core::ServiceGraph::new();
            };
            ledger
                .current
                .resources
                .iter()
                .filter_map(|(key, owned)| match owned {
                    crate::OwnedResource::Service(crate::OwnedService {
                        requires_services,
                        ..
                    }) => Some((
                        key.clone(),
                        zup_core::service_dependency_keys(requires_services, ledger.target),
                    )),
                    _ => None,
                })
                .collect()
        } else {
            self.services
                .iter()
                .map(|service| {
                    (
                        service.key.clone(),
                        zup_core::service_dependency_keys(
                            &service.requires_services,
                            *service.command.executable.target(),
                        ),
                    )
                })
                .collect()
        }
    }
}
