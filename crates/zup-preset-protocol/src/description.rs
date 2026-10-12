use serde::{Deserialize, Serialize};

use crate::{Capabilities, PRESET_PROTOCOL_VERSION};

pub const DESCRIBE_FLAG: &str = "--zup-describe";

pub const MAX_DESCRIBE_BYTES: usize = 1024 * 1024;

pub const MAX_TARGETS: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PresetDescription {
    pub name: String,
    pub version: String,
    pub ui_protocol: u32,
    pub required_capabilities: Capabilities,
    pub settings_schema: serde_json::Value,
}

impl PresetDescription {
    pub fn new(
        name: impl Into<String>,
        version: impl Into<String>,
        settings_schema: serde_json::Value,
    ) -> Self {
        Self {
            name: name.into(),
            version: version.into(),
            ui_protocol: PRESET_PROTOCOL_VERSION,
            required_capabilities: Capabilities::default(),
            settings_schema,
        }
    }

    pub fn with_capabilities(mut self, capabilities: Capabilities) -> Self {
        self.required_capabilities = capabilities;
        self
    }

    pub fn validate(&self) -> Result<(), DescribeError> {
        if self.name.trim().is_empty() {
            return Err(DescribeError::EmptyName);
        }
        if self.version.trim().is_empty() {
            return Err(DescribeError::EmptyVersion);
        }
        if !self.settings_schema.is_object() {
            return Err(DescribeError::SettingsSchemaNotAnObject);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DescribeError {
    #[error("a preset describe document names no preset")]
    EmptyName,
    #[error("a preset describe document names no version")]
    EmptyVersion,
    #[error("a preset's settings schema is not a JSON object")]
    SettingsSchemaNotAnObject,
    #[error("the describe document is {size} bytes; the limit is {limit}")]
    TooLarge { size: usize, limit: usize },
}
