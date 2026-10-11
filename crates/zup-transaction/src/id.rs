use std::fmt;

use serde::{Deserialize, Serialize};
use zup_core::ResourceKey;

pub use zup_core::TransactionId;

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
