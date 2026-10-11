use zup_core::{Target, TargetArchitecture};

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

pub fn native_runner(target: Target) -> Option<Runner> {
    let windows = target.is_windows();
    let runner = match target.architecture() {
        TargetArchitecture::X86_64 if windows => Runner {
            label: "windows-latest",
            native: true,
        },
        TargetArchitecture::Aarch64(_) if windows => Runner {
            label: WINDOWS_ARM,
            native: true,
        },
        TargetArchitecture::X86_64 => Runner {
            label: "ubuntu-latest",
            native: true,
        },
        TargetArchitecture::Aarch64(_) => Runner {
            label: "ubuntu-24.04-arm",
            native: true,
        },
        _ => return None,
    };
    Some(runner)
}

pub fn cross_runner(target: Target) -> &'static str {
    if target.is_windows() {
        "windows-latest"
    } else {
        "ubuntu-latest"
    }
}

pub fn is_windows(target: Target) -> bool {
    target.is_windows()
}
