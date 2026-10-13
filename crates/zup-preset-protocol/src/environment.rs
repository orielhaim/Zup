use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperatingSystem {
    Windows,
    Linux,
    #[default]
    Unknown,
}

impl OperatingSystem {
    pub fn current() -> Self {
        if cfg!(windows) {
            Self::Windows
        } else if cfg!(target_os = "linux") {
            Self::Linux
        } else {
            Self::Unknown
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GraphicalSession {
    #[default]
    Unknown,
    Wayland,
    X11,
    Native,
    None,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct DesktopEnvironment {
    pub session: GraphicalSession,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desktop_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desktop_name: Option<String>,
}

impl DesktopEnvironment {
    pub fn graphical(&self) -> bool {
        !matches!(
            self.session,
            GraphicalSession::Unknown | GraphicalSession::None
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "authorization")]
pub enum MachineAuthorization {
    Unavailable {
        #[serde(default)]
        reason: String,
    },
    Available,
    Elevated,
}

impl Default for MachineAuthorization {
    fn default() -> Self {
        Self::Unavailable {
            reason: String::new(),
        }
    }
}

impl MachineAuthorization {
    pub fn can_authorize(&self) -> bool {
        matches!(self, Self::Available | Self::Elevated)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostEnvironment {
    pub operating_system: OperatingSystem,
    pub architecture: String,
    pub target: String,
    pub desktop: DesktopEnvironment,
    pub machine_authorization: MachineAuthorization,
}

impl HostEnvironment {
    pub fn graphical(&self) -> bool {
        self.desktop.graphical()
    }
}

pub fn detect_desktop() -> DesktopEnvironment {
    if cfg!(windows) {
        return DesktopEnvironment {
            session: GraphicalSession::Native,
            desktop_id: None,
            desktop_name: None,
        };
    }
    let session_type = std::env::var("XDG_SESSION_TYPE")
        .unwrap_or_default()
        .to_ascii_lowercase();
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    let x11 = std::env::var_os("DISPLAY").is_some();
    let session = match session_type.as_str() {
        "wayland" => GraphicalSession::Wayland,
        "x11" => GraphicalSession::X11,
        "tty" | "unspecified" => GraphicalSession::None,
        _ if wayland => GraphicalSession::Wayland,
        _ if x11 => GraphicalSession::X11,
        _ => GraphicalSession::None,
    };
    DesktopEnvironment {
        session,
        desktop_id: std::env::var("XDG_CURRENT_DESKTOP")
            .ok()
            .filter(|value| !value.trim().is_empty()),
        desktop_name: None,
    }
}

pub fn detect_machine_authorization() -> MachineAuthorization {
    if cfg!(windows) {
        return MachineAuthorization::Available;
    }
    match std::env::var("PKEXEC_UID") {
        Ok(_) => MachineAuthorization::Elevated,
        Err(_) if pkexec_on_path() => MachineAuthorization::Available,
        Err(_) => MachineAuthorization::Unavailable {
            reason: "machine scope needs an authorization agent, and no pkexec is on PATH".into(),
        },
    }
}

fn pkexec_on_path() -> bool {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|directory| directory.join("pkexec"))
        .any(|candidate| is_runnable(&candidate))
}

#[cfg(unix)]
fn is_runnable(candidate: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::symlink_metadata(candidate)
        .is_ok_and(|metadata| !metadata.is_dir() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_runnable(candidate: &std::path::Path) -> bool {
    candidate.is_file()
}

pub fn current(target: &str) -> HostEnvironment {
    HostEnvironment {
        operating_system: OperatingSystem::current(),
        architecture: std::env::consts::ARCH.to_owned(),
        target: target.to_owned(),
        desktop: detect_desktop(),
        machine_authorization: detect_machine_authorization(),
    }
}
