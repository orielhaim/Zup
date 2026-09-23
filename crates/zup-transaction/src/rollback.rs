//! Rollback capability and plan-level rollback guarantee.

use serde::{Deserialize, Serialize};

/// How a single operation can be undone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RollbackCapability {
    /// `zup` owns a precise receipt and can undo exactly.
    Automatic,
    /// An explicit compensating command exists.
    Compensating,
    /// Cannot be undone.
    None,
}

/// Plan-level rollback guarantee, visible before execution begins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RollbackGuarantee {
    /// All operations are `Automatic`.
    Full,
    /// No `None` capabilities; some operations use compensation.
    Compensating,
    /// At least one irreversible operation.
    Incomplete,
}

impl RollbackGuarantee {
    /// Combine node capabilities into a plan guarantee.
    pub fn from_capabilities<'a>(caps: impl IntoIterator<Item = &'a RollbackCapability>) -> Self {
        let mut has_compensating = false;
        let mut has_none = false;
        for cap in caps {
            match cap {
                RollbackCapability::Automatic => {}
                RollbackCapability::Compensating => has_compensating = true,
                RollbackCapability::None => has_none = true,
            }
        }
        if has_none {
            Self::Incomplete
        } else if has_compensating {
            Self::Compensating
        } else {
            Self::Full
        }
    }
}
