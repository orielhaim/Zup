use crate::{ResourceKey, ServiceId};
use std::collections::{BTreeMap, BTreeSet};

/// Edges point from a managed service to the managed services it requires.
pub type ServiceGraph = BTreeMap<ResourceKey, BTreeSet<ResourceKey>>;

pub fn validate_service_graph(graph: &ServiceGraph) -> Result<(), String> {
    let mut remaining = graph.clone();
    for (service, dependencies) in graph {
        if dependencies.contains(service) || dependencies.iter().any(|key| !graph.contains_key(key))
        {
            return Err(format!(
                "service {service:?} has a missing or self dependency"
            ));
        }
    }
    while !remaining.is_empty() {
        let ready: BTreeSet<_> = remaining
            .iter()
            .filter(|(_, dependencies)| dependencies.is_empty())
            .map(|(key, _)| key.clone())
            .collect();
        if ready.is_empty() {
            return Err("managed service dependency cycle".into());
        }
        remaining.retain(|key, _| !ready.contains(key));
        for dependencies in remaining.values_mut() {
            dependencies.retain(|key| !ready.contains(key));
        }
    }
    Ok(())
}

pub fn service_dependency_keys(ids: &[ServiceId], target: crate::Target) -> BTreeSet<ResourceKey> {
    ids.iter()
        .map(|id| ResourceKey::Service { id: id.clone() }.canonical_for(&target))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn key(id: &str) -> ResourceKey {
        ResourceKey::Service {
            id: ServiceId::new(id).unwrap(),
        }
    }
    #[rstest::rstest]
    #[case::chain(vec![("api", vec!["db"]), ("db", vec![])], true)]
    #[case::diamond(vec![("api", vec!["db", "queue"]), ("queue", vec!["db"]), ("db", vec![])], true)]
    #[case::missing(vec![("api", vec!["db"])], false)]
    #[case::self_reference(vec![("api", vec!["api"])], false)]
    #[case::cycle(vec![("api", vec!["db"]), ("db", vec!["api"])], false)]
    fn graph_requires_managed_acyclic_services(
        #[case] edges: Vec<(&str, Vec<&str>)>,
        #[case] valid: bool,
    ) {
        let graph = edges
            .into_iter()
            .map(|(id, dependencies)| (key(id), dependencies.into_iter().map(key).collect()))
            .collect();
        assert_eq!(validate_service_graph(&graph).is_ok(), valid);
    }
}
