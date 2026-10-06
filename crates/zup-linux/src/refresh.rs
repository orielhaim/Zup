//! Typed derived-cache refresh operations.
//!
//! The shared freedesktop databases (`mime.cache` under `$XDG_DATA_HOME/mime`,
//! `mimeinfo.cache` under `$XDG_DATA_HOME/applications`) are derived state:
//! they aggregate every application's sources, so Zup never owns their bytes
//! and never stores them in the ledger. What Zup owns is the refresh that
//! regenerates them from its authoritative sources.
//!
//! A refresh is a typed transaction operation, not a post-install script and
//! not a generic "run any command" hook:
//!
//! - it runs after the authoritative files, ordered by the transaction graph;
//! - the tool is preflighted before anything mutates, and only when MIME or
//!   desktop sources are actually part of the transaction;
//! - it is journaled, so recovery re-runs a refresh a crash interrupted;
//! - rollback re-runs it, and the runner sweeps once more after rollback so
//!   the final on-disk state is coherent regardless of node order.
//!
//! Tools are invoked directly, never through a shell.

use std::path::PathBuf;

use zup_core::{BackendResourceId, ResourceKey};

/// Backend identity of the MIME database refresh.
pub const REFRESH_MIME_ID: &str = "linux:refresh-mime-database";
/// Backend identity of the desktop database refresh.
pub const REFRESH_DESKTOP_ID: &str = "linux:refresh-desktop-database";

/// The freedesktop tool a refresh needs.
pub const MIME_REFRESH_TOOL: &str = "update-mime-database";
/// The freedesktop tool a desktop refresh needs.
pub const DESKTOP_REFRESH_TOOL: &str = "update-desktop-database";

/// Why a refresh could not run.
#[derive(Debug, thiserror::Error)]
pub enum RefreshError {
    #[error("cannot refresh derived integration state: `{tool}` is not available: {reason}")]
    Unavailable { tool: String, reason: String },
    #[error("`{tool}` failed for `{directory}`: {reason}")]
    Failed {
        tool: String,
        directory: String,
        reason: String,
    },
    #[error("refusing to refresh: {0}")]
    Refused(String),
}

/// One refresh operation: a tool plus the database directory it regenerates.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RefreshRequest {
    pub tool: String,
    pub directory: String,
}

impl RefreshRequest {
    pub fn mime(directory: &str) -> Self {
        Self {
            tool: MIME_REFRESH_TOOL.into(),
            directory: directory.into(),
        }
    }

    pub fn desktop(directory: &str) -> Self {
        Self {
            tool: DESKTOP_REFRESH_TOOL.into(),
            directory: directory.into(),
        }
    }

    pub fn backend_id(&self) -> BackendResourceId {
        let id = if self.tool == MIME_REFRESH_TOOL {
            REFRESH_MIME_ID
        } else {
            REFRESH_DESKTOP_ID
        };
        BackendResourceId::new(id).expect("a static backend id is valid")
    }

    pub fn key(&self) -> ResourceKey {
        ResourceKey::Backend { id: self.backend_id() }
    }

    pub fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("a refresh request serializes")
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, RefreshError> {
        if bytes.len() > zup_transaction::MAX_BACKEND_PAYLOAD_BYTES {
            return Err(RefreshError::Refused("refresh payload exceeds the transaction limit".into()));
        }
        let request: RefreshRequest = serde_json::from_slice(bytes)
            .map_err(|error| RefreshError::Refused(format!("invalid refresh payload: {error}")))?;
        if request.tool != MIME_REFRESH_TOOL && request.tool != DESKTOP_REFRESH_TOOL {
            return Err(RefreshError::Refused(format!(
                "unknown refresh tool `{}`",
                request.tool
            )));
        }
        if request.directory.is_empty() || !request.directory.starts_with('/') {
            return Err(RefreshError::Refused("refresh directory is not absolute".into()));
        }
        Ok(request)
    }
}

/// Resolve a tool name without a shell: the first executable file of that
/// name on `PATH`.
///
/// Execution follows the final link: resolving `update-mime-database`
/// through a distribution-managed symlink is normal. The no-follow rules
/// apply to files Zup writes, never to tools it runs.
pub fn discover_tool(name: &str) -> Option<PathBuf> {
    if name.is_empty() || name.contains('/') || name.contains('\0') {
        return None;
    }
    let path = std::env::var_os("PATH")?;
    for directory in std::env::split_paths(&path) {
        if directory.as_os_str().is_empty() {
            continue;
        }
        let candidate = directory.join(name);
        // Execution follows the final link: resolving `update-mime-database`
        // through a distribution-managed symlink is normal. The no-follow
        // rules apply to files Zup writes, never to tools it runs.
        let Ok(metadata) = std::fs::metadata(&candidate) else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        if metadata.permissions().mode() & 0o111 != 0 {
            return Some(candidate);
        }
    }
    None
}

use std::os::unix::fs::PermissionsExt as _;

/// Preflight one refresh: the tool must resolve before anything mutates.
pub fn preflight(request: &RefreshRequest) -> Result<PathBuf, RefreshError> {
    discover_tool(&request.tool).ok_or_else(|| RefreshError::Unavailable {
        tool: request.tool.clone(),
        reason: format!(
            "installing MIME/desktop integration needs `{}` to regenerate the shared database",
            request.tool
        ),
    })
}

/// Run one refresh to a successful exit. No shell, no argument interpolation:
/// the tool path and one directory argument.
pub fn run_refresh(request: &RefreshRequest) -> Result<(), RefreshError> {
    let tool = preflight(request)?;
    let output = std::process::Command::new(&tool)
        .arg(&request.directory)
        .output()
        .map_err(|error| RefreshError::Failed {
            tool: request.tool.clone(),
            directory: request.directory.clone(),
            reason: format!("could not start: {error}"),
        })?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr: String = stderr.chars().take(512).collect();
    Err(RefreshError::Failed {
        tool: request.tool.clone(),
        directory: request.directory.clone(),
        reason: format!("exit {}: {}", output.status, stderr.trim()),
    })
}

/// Whether a refresh has sources to derive from.
///
/// A refresh regenerates from authoritative files. When rollback removed
/// those files, there is nothing to regenerate and the tool itself would
/// refuse the absent directory; skipping is the honest answer, not an error.
/// The next install or repair regenerates from its own sources.
pub fn has_sources(request: &RefreshRequest) -> bool {
    let sources = if request.tool == MIME_REFRESH_TOOL {
        std::path::Path::new(&request.directory).join("packages")
    } else {
        std::path::Path::new(&request.directory).to_path_buf()
    };
    std::fs::symlink_metadata(&sources)
        .map(|metadata| metadata.is_dir())
        .unwrap_or(false)
}

#[cfg(test)]

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identities_are_backend_keys() {
        let request = RefreshRequest::mime("/home/u/.local/share/mime");
        assert_eq!(request.key(), ResourceKey::Backend { id: request.backend_id() });
        assert_eq!(request.backend_id().as_str(), REFRESH_MIME_ID);
        let request = RefreshRequest::desktop("/home/u/.local/share/applications");
        assert_eq!(request.backend_id().as_str(), REFRESH_DESKTOP_ID);
    }

    #[test]
    fn payloads_round_trip_and_reject_strangers() {
        let request = RefreshRequest::mime("/data/mime");
        let decoded = RefreshRequest::decode(&request.encode()).expect("round trip");
        assert_eq!(decoded, request);
        assert!(RefreshRequest::decode(b"not json").is_err());
        let mut evil = request.clone();
        evil.tool = "rm".into();
        assert!(RefreshRequest::decode(&evil.encode()).is_err());
        let mut relative = request.clone();
        relative.directory = "mime".into();
        assert!(RefreshRequest::decode(&relative.encode()).is_err());
    }

    #[test]
    fn discovery_never_uses_a_shell() {
        assert!(discover_tool("").is_none());
        assert!(discover_tool("a/b").is_none());
        // `sh` resolves on any Unix test machine; the mechanism is PATH
        // search, not shell execution.
        assert!(discover_tool("sh").is_some());
        assert!(discover_tool("definitely-not-a-zup-tool").is_none());
    }

    #[test]
    fn a_missing_tool_preflights_with_its_name() {
        let request = RefreshRequest {
            tool: "definitely-not-a-zup-tool".into(),
            directory: "/data/mime".into(),
        };
        let error = preflight(&request).expect_err("the tool is absent");
        assert!(error.to_string().contains("definitely-not-a-zup-tool"), "{error}");
    }
}
