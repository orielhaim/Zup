//! Fallible constructors for core domain values.

use thiserror::Error;

/// Errors produced when constructing validated domain values.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ValueError {
    /// A required identifier or name was empty after trimming.
    #[error("{kind} must not be empty")]
    Empty { kind: &'static str },

    /// A URI scheme does not match RFC 3986 scheme syntax.
    #[error("invalid URI scheme `{scheme}`")]
    InvalidScheme { scheme: String },

    /// A file extension is not a bare extension such as `.acme`.
    #[error("invalid file extension `{extension}`")]
    InvalidExtension { extension: String },

    #[error("invalid plugin id `{id}`")]
    InvalidPluginId { id: String },

    #[error("invalid prerequisite id `{id}`")]
    InvalidPrerequisiteId { id: String },
}
