use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use uuid::Uuid;
use zup_core::ResourceKey;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TransactionId(Uuid);

impl TransactionId {
    pub fn new_v7() -> Self {
        Self(Uuid::now_v7())
    }

    pub fn from_uuid(uuid: Uuid) -> Self {
        Self(uuid)
    }

    pub fn as_uuid(&self) -> Uuid {
        self.0
    }
}

impl fmt::Display for TransactionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OperationId(String);

impl OperationId {
    pub const BEGIN: &'static str = "ctrl:begin";
    pub const PREFLIGHT: &'static str = "ctrl:preflight";
    pub const COMMIT_INTENT: &'static str = "ctrl:commit-intent";
    pub const VERIFY: &'static str = "ctrl:verify";
    pub const COMMIT: &'static str = "ctrl:commit";

    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn resource(kind: &str, key: &ResourceKey) -> Self {
        Self(format!("op:{kind}:{}", format_key(key)))
    }

    pub fn stage_file(key: &ResourceKey) -> Self {
        Self::resource("stage-file", key)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for OperationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

fn format_key(key: &ResourceKey) -> String {
    serde_json::to_string(key).expect("resource keys serialize")
}

impl FromStr for TransactionId {
    type Err = uuid::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(Uuid::parse_str(s)?))
    }
}
