use std::path::PathBuf;

use zup_core::{BackendResourceId, ResourceKey};

pub const REFRESH_MIME_ID: &str = "linux:refresh-mime-database";

pub const REFRESH_DESKTOP_ID: &str = "linux:refresh-desktop-database";

pub const MIME_REFRESH_TOOL: &str = "update-mime-database";

pub const DESKTOP_REFRESH_TOOL: &str = "update-desktop-database";

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
        ResourceKey::Backend {
            id: self.backend_id(),
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("a refresh request serializes")
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ExecError> {
        if bytes.len() > zup_transaction::MAX_BACKEND_PAYLOAD_BYTES {
            return Err(ExecError::RefreshRefused(
                "refresh payload exceeds the transaction limit".into(),
            ));
        }
        let request: RefreshRequest = serde_json::from_slice(bytes).map_err(|error| {
            ExecError::RefreshRefused(format!("invalid refresh payload: {error}"))
        })?;
        if request.tool != MIME_REFRESH_TOOL && request.tool != DESKTOP_REFRESH_TOOL {
            return Err(ExecError::RefreshRefused(format!(
                "unknown refresh tool `{}`",
                request.tool
            )));
        }
        if request.directory.is_empty() || !request.directory.starts_with('/') {
            return Err(ExecError::RefreshRefused(
                "refresh directory is not absolute".into(),
            ));
        }
        Ok(request)
    }
}

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

use crate::error::ExecError;
use std::os::unix::fs::PermissionsExt as _;

pub fn preflight(request: &RefreshRequest) -> Result<PathBuf, ExecError> {
    discover_tool(&request.tool).ok_or_else(|| ExecError::RefreshUnavailable {
        tool: request.tool.clone(),
        reason: format!(
            "installing MIME/desktop integration needs `{}` to regenerate the shared database",
            request.tool
        ),
    })
}

pub fn run_refresh(request: &RefreshRequest) -> Result<(), ExecError> {
    let tool = preflight(request)?;
    let output = std::process::Command::new(&tool)
        .arg(&request.directory)
        .output()
        .map_err(|error| ExecError::RefreshFailed {
            tool: request.tool.clone(),
            directory: request.directory.clone(),
            reason: format!("could not start: {error}"),
        })?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr: String = stderr.chars().take(512).collect();
    Err(ExecError::RefreshFailed {
        tool: request.tool.clone(),
        directory: request.directory.clone(),
        reason: format!("exit {}: {}", output.status, stderr.trim()),
    })
}

pub fn database_present(request: &RefreshRequest) -> bool {
    std::fs::symlink_metadata(&request.directory)
        .map(|metadata| metadata.is_dir())
        .unwrap_or(false)
}

pub fn ensure_refreshable(request: &RefreshRequest) -> Result<(), ExecError> {
    if request.tool != MIME_REFRESH_TOOL {
        return Ok(());
    }
    let packages = std::path::Path::new(&request.directory).join("packages");
    if std::fs::symlink_metadata(&packages)
        .map(|metadata| metadata.is_dir())
        .unwrap_or(false)
    {
        return Ok(());
    }
    std::fs::create_dir_all(&packages).map_err(|error| ExecError::RefreshFailed {
        tool: request.tool.clone(),
        directory: request.directory.clone(),
        reason: format!(
            "could not recreate `{}` for the post-rollback sweep: {error}",
            packages.display()
        ),
    })
}

#[cfg(test)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identities_are_backend_keys() {
        let request = RefreshRequest::mime("/home/u/.local/share/mime");
        assert_eq!(
            request.key(),
            ResourceKey::Backend {
                id: request.backend_id()
            }
        );
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
        assert!(
            error.to_string().contains("definitely-not-a-zup-tool"),
            "{error}"
        );
    }
}
