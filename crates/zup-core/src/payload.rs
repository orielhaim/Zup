use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{RelativePath, Sha256Digest, Template};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ResourceId(String);

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("resource id must contain 1 to 128 ASCII letters, digits, dots, underscores or hyphens")]
pub struct ResourceIdError;

impl ResourceId {
    pub fn new(value: impl Into<String>) -> Result<Self, ResourceIdError> {
        let value = value.into();
        if value.is_empty()
            || value.len() > 128
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
        {
            return Err(ResourceIdError);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ResourceId {
    type Error = ResourceIdError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<ResourceId> for String {
    fn from(id: ResourceId) -> Self {
        id.0
    }
}

impl std::fmt::Display for ResourceId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[cfg(feature = "schema")]
impl schemars::JsonSchema for ResourceId {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "ResourceId".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string", "minLength": 1, "maxLength": 128,
            "pattern": "^[A-Za-z0-9._-]+$"
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PayloadId {
    File {
        declaration: ResourceId,
    },
    TreeMember {
        declaration: ResourceId,
        member: RelativePath,
    },
}

impl PayloadId {
    pub fn declaration(&self) -> &ResourceId {
        match self {
            Self::File { declaration } | Self::TreeMember { declaration, .. } => declaration,
        }
    }
}

impl std::fmt::Display for PayloadId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::File { declaration } => write!(formatter, "{declaration}"),
            Self::TreeMember {
                declaration,
                member,
            } => write!(formatter, "{declaration}/{member}"),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum FixedReplacement {
    #[default]
    Atomic,
    Quiescent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case", tag = "kind", deny_unknown_fields)]
pub enum PayloadPlacement<G = RelativePath, F = Template> {
    Generation {
        path: G,
    },
    Fixed {
        path: F,
        replacement: FixedReplacement,
    },
}

impl<G, F> PayloadPlacement<G, F> {
    pub fn fixed_replacement(&self) -> Option<FixedReplacement> {
        match self {
            Self::Generation { .. } => None,
            Self::Fixed { replacement, .. } => Some(*replacement),
        }
    }
}

impl<P> PayloadPlacement<P, P> {
    pub fn path(&self) -> &P {
        match self {
            Self::Generation { path } | Self::Fixed { path, .. } => path,
        }
    }

    pub fn resolve<Q, E>(
        &self,
        mut resolve: impl FnMut(&P) -> Result<Q, E>,
    ) -> Result<PayloadPlacement<Q, Q>, E> {
        Ok(match self {
            Self::Generation { path } => PayloadPlacement::Generation {
                path: resolve(path)?,
            },
            Self::Fixed { path, replacement } => PayloadPlacement::Fixed {
                path: resolve(path)?,
                replacement: *replacement,
            },
        })
    }
}

impl<F: std::fmt::Display> std::fmt::Display for PayloadPlacement<RelativePath, F> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Generation { path } => write!(formatter, "generation:{path}"),
            Self::Fixed { path, .. } => write!(formatter, "fixed:{path}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case", tag = "kind", deny_unknown_fields)]
pub enum PayloadMapping {
    Generation {
        directory: Option<RelativePath>,
    },
    GenerationFile {
        path: RelativePath,
    },
    Fixed {
        directory: Template,
        #[serde(default)]
        replacement: FixedReplacement,
    },
    FixedFile {
        path: Template,
        #[serde(default)]
        replacement: FixedReplacement,
    },
}

impl Default for PayloadMapping {
    fn default() -> Self {
        Self::Generation { directory: None }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PayloadRequirement<P = Template> {
    pub id: PayloadId,
    pub source_relative: RelativePath,
    pub placement: PayloadPlacement<RelativePath, P>,
    pub size: u64,
    pub sha256: Sha256Digest,
    pub executable: bool,
}
