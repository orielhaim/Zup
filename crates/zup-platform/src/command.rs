//! Shared command representation for executable + arguments.

use serde::{Deserialize, Serialize};

use crate::target_path::TargetPath;

/// A concrete executable path plus argument vector.
///
/// Canonical engine representation for services, launchers, protocols, and
/// file-association open commands - never a raw command line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandSpec {
    pub executable: TargetPath,
    pub arguments: Vec<String>,
}

impl CommandSpec {
    pub fn new(executable: TargetPath, arguments: Vec<String>) -> Self {
        Self {
            executable,
            arguments,
        }
    }
}
