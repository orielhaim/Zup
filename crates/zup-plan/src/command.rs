//! Shared command representation for executable + arguments.

use serde::{Deserialize, Serialize};

use crate::target_path::TargetPath;

/// A concrete executable path plus argument vector.
///
/// Canonical engine representation for services, launchers, protocols, and
/// file-association open commands - never a raw command line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandSpec<P = TargetPath> {
    pub executable: P,
    pub arguments: Vec<String>,
}

impl<P> CommandSpec<P> {
    pub fn new(executable: P, arguments: Vec<String>) -> Self {
        Self {
            executable,
            arguments,
        }
    }
}
