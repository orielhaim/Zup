use std::num::NonZeroU64;

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum DeploymentIdentityError {
    #[error("deployment identity must not be the nil UUID")]
    Nil,
    #[error(transparent)]
    Uuid(#[from] uuid::Error),
    #[error("activation revision must be a positive 64-bit integer: {0}")]
    Revision(#[from] std::num::ParseIntError),
    #[error("activation revision must be greater than zero")]
    ZeroRevision,
    #[error("activation revision is exhausted")]
    RevisionExhausted,
}

macro_rules! deployment_id {
    ($name:ident) => {
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
        )]
        #[serde(try_from = "uuid::Uuid", into = "uuid::Uuid")]
        pub struct $name(uuid::Uuid);

        impl $name {
            pub fn new_v7() -> Self {
                Self(uuid::Uuid::now_v7())
            }

            pub const fn as_uuid(self) -> uuid::Uuid {
                self.0
            }
        }

        impl TryFrom<uuid::Uuid> for $name {
            type Error = DeploymentIdentityError;

            fn try_from(value: uuid::Uuid) -> Result<Self, Self::Error> {
                if value.is_nil() {
                    Err(DeploymentIdentityError::Nil)
                } else {
                    Ok(Self(value))
                }
            }
        }

        impl From<$name> for uuid::Uuid {
            fn from(value: $name) -> Self {
                value.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                self.0.fmt(formatter)
            }
        }

        impl std::str::FromStr for $name {
            type Err = DeploymentIdentityError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::try_from(uuid::Uuid::parse_str(value)?)
            }
        }
    };
}

deployment_id!(InstallationId);
deployment_id!(MaterializationId);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ActivationRevision(NonZeroU64);

impl ActivationRevision {
    pub const FIRST: Self = Self(NonZeroU64::MIN);

    pub fn new(value: u64) -> Result<Self, DeploymentIdentityError> {
        NonZeroU64::new(value)
            .map(Self)
            .ok_or(DeploymentIdentityError::ZeroRevision)
    }

    pub const fn value(self) -> u64 {
        self.0.get()
    }

    pub fn next(self) -> Result<Self, DeploymentIdentityError> {
        self.0
            .checked_add(1)
            .map(Self)
            .ok_or(DeploymentIdentityError::RevisionExhausted)
    }
}

impl std::fmt::Display for ActivationRevision {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.value().fmt(formatter)
    }
}
impl std::str::FromStr for ActivationRevision {
    type Err = DeploymentIdentityError;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value.parse::<u64>()?)
    }
}
