//! What a compiled preset executable says about itself when it is packaged.
//!
//! `zup preset pack` runs the executable with `--zup-describe` and reads one JSON
//! document. That is the only moment a preset binary is executed, and it
//! happens in the preset author's build, never in an application's build.
//!
//! The describe document is what only the preset knows: the UI protocol it
//! speaks, the capabilities it cannot work without, and the JSON Schema of its
//! settings, generated from the same type the preset will deserialize at
//! runtime. The `.zupui` that carries it is a build product and belongs to
//! `zup-artifact`; this is the public contract a preset author writes against.

use serde::{Deserialize, Serialize};

use crate::{PRESET_PROTOCOL_VERSION, Capabilities};

/// The flag a preset executable recognizes to print its description.
pub const DESCRIBE_FLAG: &str = "--zup-describe";

/// The largest a describe document may be.
///
/// A preset's schema is generated from its own types, so this is a statement
/// about what a generated schema can be rather than a limit that could be hit
/// by an application.
pub const MAX_DESCRIBE_BYTES: usize = 1024 * 1024;

/// The most binaries one preset may carry, one per supported target.
pub const MAX_TARGETS: usize = 64;

/// What `preset.exe --zup-describe` prints.
///
/// Only what the preset itself knows. Its name and version are here so a
/// publisher can check them against the Cargo manifest rather than trust them,
/// and a human description is absent on purpose: that belongs to the package
/// listing, not to a compiled artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PresetDescription {
    /// The preset's name, from its Cargo package.
    pub name: String,
    /// The preset's version, from its Cargo package.
    pub version: String,
    /// The UI wire protocol this build speaks.
    pub ui_protocol: u32,
    /// What this preset cannot present without.
    pub required_capabilities: Capabilities,
    /// The JSON Schema of this preset's settings.
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

/// A describe document that cannot be used.
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
