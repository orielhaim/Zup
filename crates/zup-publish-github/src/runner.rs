#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Runner {
    pub label: &'static str,
    pub native: bool,
}

impl Runner {
    pub const fn label(&self) -> &'static str {
        self.label
    }
}

pub const WINDOWS_ARM: &str = "windows-11-arm";

pub const WINDOWS_ARM_VS2026: &str = "windows-11-vs2026-arm";

pub fn native_runner(triple: &str) -> Option<Runner> {
    let (arch, os) = split(triple)?;
    let runner = match (os, arch) {
        ("windows", "x86_64") => Runner {
            label: "windows-latest",
            native: true,
        },
        ("windows", "aarch64") => Runner {
            label: WINDOWS_ARM,
            native: true,
        },
        ("linux", "x86_64") => Runner {
            label: "ubuntu-latest",
            native: true,
        },
        ("linux", "aarch64") => Runner {
            label: "ubuntu-24.04-arm",
            native: true,
        },
        ("macos", "aarch64") => Runner {
            label: "macos-latest",
            native: true,
        },
        ("macos", "x86_64") => Runner {
            label: "macos-15-intel",
            native: true,
        },
        _ => return None,
    };
    Some(runner)
}

pub fn cross_runner(triple: &str) -> &'static str {
    match split(triple).map(|(_, os)| os) {
        Some("windows") => "windows-latest",
        Some("macos") => "macos-latest",
        _ => "ubuntu-latest",
    }
}

pub fn is_windows(triple: &str) -> bool {
    split(triple).is_some_and(|(_, os)| os == "windows")
}

fn split(triple: &str) -> Option<(&str, &str)> {
    let mut parts = triple.split('-');
    let arch = parts.next()?;
    let _vendor = parts.next()?;
    let os = parts.next()?;
    Some((arch, normalize_os(os)))
}

fn normalize_os(os: &str) -> &str {
    match os {
        "darwin" => "macos",
        "win32" => "windows",
        other => other,
    }
}
