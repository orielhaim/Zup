//! Shared command representation for executable + arguments.

use serde::{Deserialize, Serialize};

use crate::TargetPath;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
