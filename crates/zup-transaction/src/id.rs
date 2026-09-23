//! Stable operation identity and runtime transaction identity.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use uuid::Uuid;
use zup_core::ResourceKey;
use zup_exec::{
    Delta, FileOperationKind, FileTypeOperationKind, PathOperationKind, ProtocolOperationKind,
    ServiceOperationKind, ShortcutOperationKind,
};

/// Runtime identity of one transaction execution attempt.
///
/// Never use this for persistent resource identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TransactionId(Uuid);

impl TransactionId {
    /// Generate a new UUIDv7 transaction id.
    pub fn new_v7() -> Self {
        Self(Uuid::now_v7())
    }

    /// Wrap an existing UUID.
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

/// Deterministic logical identity of one transaction node.
///
/// Survives serialization. Never a petgraph index and never a random UUID.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OperationId(String);

impl OperationId {
    /// Control-barrier: transaction begin.
    pub const BEGIN: &'static str = "ctrl:begin";
    /// Control-barrier: preflight checks.
    pub const PREFLIGHT: &'static str = "ctrl:preflight";
    /// Control-barrier: durable commit intent.
    pub const COMMIT_INTENT: &'static str = "ctrl:commit-intent";
    /// Control-barrier: verification.
    pub const VERIFY: &'static str = "ctrl:verify";
    /// Control-barrier: commit.
    pub const COMMIT: &'static str = "ctrl:commit";

    /// Build an id from a raw stable name.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Resource operation id: `op:<kind>:<resource>`.
    pub fn resource(kind: &str, key: &ResourceKey) -> Self {
        Self(format!("op:{kind}:{}", format_key(key)))
    }

    /// Staging node for a mutating file: `stage:file:<resource>`.
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
    match key {
        ResourceKey::File { destination } => format!("file:{destination}"),
        ResourceKey::Shortcut { location, name } => format!("shortcut:{location}:{name}"),
        ResourceKey::PathEntry { value } => format!("path:{value}"),
        ResourceKey::Service { id } => format!("service:{id}"),
        ResourceKey::Protocol { scheme } => format!("protocol:{scheme}"),
        ResourceKey::FileType { id } => format!("filetype:{id}"),
        ResourceKey::FileTypeExtension { extension } => format!("fileext:{extension}"),
        ResourceKey::ExternalAction { id } => format!("action:{id}"),
    }
}

/// Map a resource delta to a stable operation-kind token.
pub fn file_kind_token(kind: FileOperationKind) -> &'static str {
    match kind {
        FileOperationKind::Create => "create",
        FileOperationKind::Replace => "replace",
        FileOperationKind::RestoreOwned => "restore_owned",
        FileOperationKind::RepairOwned => "repair_owned",
        FileOperationKind::NoOp => "noop",
        FileOperationKind::Conflict => "conflict",
        FileOperationKind::Drift => "drift",
    }
}

pub fn shortcut_kind_token(kind: ShortcutOperationKind) -> &'static str {
    match kind {
        ShortcutOperationKind::Create => "create",
        ShortcutOperationKind::UpdateOwned => "update_owned",
        ShortcutOperationKind::RestoreOwned => "restore_owned",
        ShortcutOperationKind::NoOp => "noop",
        ShortcutOperationKind::Conflict => "conflict",
        ShortcutOperationKind::Drift => "drift",
    }
}

pub fn path_kind_token(kind: PathOperationKind) -> &'static str {
    match kind {
        PathOperationKind::Add => "add",
        PathOperationKind::Present => "present",
        PathOperationKind::UpdateOwned => "update_owned",
        PathOperationKind::RestoreOwned => "restore_owned",
        PathOperationKind::Conflict => "conflict",
        PathOperationKind::Drift => "drift",
    }
}

pub fn service_kind_token(kind: ServiceOperationKind) -> &'static str {
    match kind {
        ServiceOperationKind::Create => "create",
        ServiceOperationKind::UpdateOwned => "update_owned",
        ServiceOperationKind::RestoreOwned => "restore_owned",
        ServiceOperationKind::NoOp => "noop",
        ServiceOperationKind::Conflict => "conflict",
        ServiceOperationKind::Drift => "drift",
    }
}

pub fn protocol_kind_token(kind: ProtocolOperationKind) -> &'static str {
    match kind {
        ProtocolOperationKind::Create => "create",
        ProtocolOperationKind::UpdateOwned => "update_owned",
        ProtocolOperationKind::RestoreOwned => "restore_owned",
        ProtocolOperationKind::NoOp => "noop",
        ProtocolOperationKind::Conflict => "conflict",
        ProtocolOperationKind::Drift => "drift",
    }
}

pub fn file_type_kind_token(kind: FileTypeOperationKind) -> &'static str {
    match kind {
        FileTypeOperationKind::Create => "create",
        FileTypeOperationKind::UpdateOwned => "update_owned",
        FileTypeOperationKind::RestoreOwned => "restore_owned",
        FileTypeOperationKind::NoOp => "noop",
        FileTypeOperationKind::Conflict => "conflict",
        FileTypeOperationKind::Drift => "drift",
    }
}

/// Stable token for opaque actions.
pub fn action_kind_token(_delta: Delta) -> &'static str {
    "run-opaque"
}

impl FromStr for TransactionId {
    type Err = uuid::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(Uuid::parse_str(s)?))
    }
}
