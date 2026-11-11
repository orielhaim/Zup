use std::collections::BTreeSet;
use std::num::NonZeroU32;

use serde::{Deserialize, Serialize};

use crate::{ResourceId, ServiceId, Template};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum DataCompatibility {
    #[default]
    Stable,
    Schema {
        writes: NonZeroU32,
        #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
        reads: BTreeSet<NonZeroU32>,
    },
}

impl DataCompatibility {
    pub fn accepts(&self, previous: &Self) -> bool {
        match (self, previous) {
            (Self::Stable, Self::Stable) => true,
            (
                Self::Schema { writes, reads },
                Self::Schema {
                    writes: previous, ..
                },
            ) => writes == previous || reads.contains(previous),
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PersistentAccess {
    #[default]
    Owner,
    Services {
        services: ServiceDataWriters,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(try_from = "Vec<ServiceId>", into = "Vec<ServiceId>")]
pub struct ServiceDataWriters(BTreeSet<ServiceId>);

impl ServiceDataWriters {
    pub fn services(&self) -> impl Iterator<Item = &ServiceId> {
        self.0.iter()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("persistent service access requires 1 to 256 distinct service IDs")]
pub struct InvalidServiceDataWriters;

impl TryFrom<Vec<ServiceId>> for ServiceDataWriters {
    type Error = InvalidServiceDataWriters;
    fn try_from(services: Vec<ServiceId>) -> Result<Self, Self::Error> {
        let count = services.len();
        let services: BTreeSet<_> = services.into_iter().collect();
        if !(1..=256).contains(&count) || count != services.len() {
            return Err(InvalidServiceDataWriters);
        }
        Ok(Self(services))
    }
}

impl From<ServiceDataWriters> for Vec<ServiceId> {
    fn from(services: ServiceDataWriters) -> Self {
        services.0.into_iter().collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PersistentLocation<P> {
    Path { path: P, access: PersistentAccess },
    Service { service: ServiceId },
}

impl<P> PersistentLocation<P> {
    pub fn path(&self) -> Option<&P> {
        match self {
            Self::Path { path, .. } => Some(path),
            Self::Service { .. } => None,
        }
    }

    pub fn services(&self) -> impl Iterator<Item = &ServiceId> {
        let (single, multiple) = match self {
            Self::Path {
                access: PersistentAccess::Owner,
                ..
            } => (None, None),
            Self::Path {
                access: PersistentAccess::Services { services },
                ..
            } => (None, Some(services)),
            Self::Service { service } => (Some(service), None),
        };
        single
            .into_iter()
            .chain(multiple.into_iter().flat_map(ServiceDataWriters::services))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistentDirectory<P = Template> {
    pub id: ResourceId,
    pub location: PersistentLocation<P>,
    pub compatibility: DataCompatibility,
}

#[derive(Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(untagged, deny_unknown_fields)]
enum PersistentDirectoryWire<P> {
    Path {
        id: ResourceId,
        path: P,
        #[serde(default)]
        access: PersistentAccess,
        #[serde(default)]
        compatibility: DataCompatibility,
    },
    Service {
        id: ResourceId,
        service: ServiceId,
        #[serde(default)]
        compatibility: DataCompatibility,
    },
}

impl<'de, P: Deserialize<'de>> Deserialize<'de> for PersistentDirectory<P> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match PersistentDirectoryWire::deserialize(deserializer)? {
            PersistentDirectoryWire::Path {
                id,
                path,
                access,
                compatibility,
            } => Self {
                id,
                location: PersistentLocation::Path { path, access },
                compatibility,
            },
            PersistentDirectoryWire::Service {
                id,
                service,
                compatibility,
            } => Self {
                id,
                location: PersistentLocation::Service { service },
                compatibility,
            },
        })
    }
}

impl<P: Serialize> Serialize for PersistentDirectory<P> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut record = serializer.serialize_struct(
            "PersistentDirectory",
            if matches!(self.location, PersistentLocation::Path { .. }) {
                4
            } else {
                3
            },
        )?;
        record.serialize_field("id", &self.id)?;
        match &self.location {
            PersistentLocation::Path { path, access } => {
                record.serialize_field("path", path)?;
                record.serialize_field("access", access)?;
            }
            PersistentLocation::Service { service } => {
                record.serialize_field("service", service)?
            }
        }
        record.serialize_field("compatibility", &self.compatibility)?;
        record.end()
    }
}

#[cfg(feature = "schema")]
impl<P: schemars::JsonSchema> schemars::JsonSchema for PersistentDirectory<P> {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        format!("PersistentDirectory_{}", P::schema_name()).into()
    }
    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        PersistentDirectoryWire::<P>::json_schema(generator)
    }
}

impl<P> PersistentDirectory<P> {
    pub fn map_path<Q, E>(
        &self,
        map: impl FnOnce(&P) -> Result<Q, E>,
    ) -> Result<PersistentDirectory<Q>, E> {
        Ok(PersistentDirectory {
            id: self.id.clone(),
            location: match &self.location {
                PersistentLocation::Path { path, access } => PersistentLocation::Path {
                    path: map(path)?,
                    access: access.clone(),
                },
                PersistentLocation::Service { service } => PersistentLocation::Service {
                    service: service.clone(),
                },
            },
            compatibility: self.compatibility.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[rstest::rstest]
    #[case(r#"{"id":"state","path":"/state","service":"worker"}"#)]
    #[case(r#"{"id":"state","path":"/state","access":{"kind":"services","services":[]}}"#)]
    #[case(r#"{"id":"state","path":"/state","access":{"kind":"services","services":["worker","worker"]}}"#)]
    #[case(r#"{"id":"state","service":"worker","access":{"kind":"owner"}}"#)]
    fn contradictory_data_placement_or_access_is_rejected(#[case] declaration: &str) {
        assert!(serde_json::from_str::<PersistentDirectory>(declaration).is_err());
    }
}
