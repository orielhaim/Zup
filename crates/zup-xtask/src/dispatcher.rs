use std::path::{Path, PathBuf};

use zup_toolchain::{Subsystem, ToolchainComponent};

pub const PACKAGE: &str = "zup-dispatch";

pub const TARGET: &str = zup_toolchain::DISPATCHER_TARGET;

pub const GUI: &str = "zup-dispatch";
pub const CONSOLE: &str = "zup-dispatch-console";

pub const ONLINE: &str = "zup-dispatch-online";

pub const ONLINE_CONSOLE: &str = "zup-dispatch-console-online";

#[derive(Debug, thiserror::Error)]
#[error(
    "the {name} dispatcher is not in {directory}; run `cargo xtask toolchain build` and try again"
)]
pub struct Missing {
    pub name: String,
    pub directory: String,
}

pub fn installed_name(subsystem: Subsystem, online: bool) -> String {
    zup_toolchain::file_name(
        &ToolchainComponent::Dispatcher { subsystem, online },
        std::env::consts::EXE_SUFFIX,
    )
}

fn candidates(name: &str) -> Vec<(String, &'static str)> {
    let both = |subsystem: Subsystem| {
        [
            (installed_name(subsystem, false), "the offline launcher"),
            (installed_name(subsystem, true), "the online launcher"),
        ]
    };
    match name {
        GUI => both(Subsystem::Gui).to_vec(),
        CONSOLE => both(Subsystem::Console).to_vec(),
        ONLINE => vec![(installed_name(Subsystem::Gui, true), "the online launcher")],
        ONLINE_CONSOLE => vec![(
            installed_name(Subsystem::Console, true),
            "the online console launcher",
        )],
        other => vec![(
            format!("{other}{}", std::env::consts::EXE_SUFFIX),
            "the launcher",
        )],
    }
}

pub fn beside(directory: &Path, name: &str) -> Result<PathBuf, Missing> {
    for (file, _) in candidates(name) {
        let candidate = directory.join(file);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(Missing {
        name: name.to_owned(),
        directory: directory.display().to_string(),
    })
}

fn profile_directory(executable: &Path) -> PathBuf {
    let mut directory = executable.parent().unwrap_or(executable).to_path_buf();
    if directory.ends_with("deps") {
        directory.pop();
    }
    directory
}

fn staged_candidates(executable: &Path) -> Vec<PathBuf> {
    let profile = profile_directory(executable);
    let mut out = Vec::new();
    if let Ok(version) = crate::toolchain::version() {
        out.push(
            profile
                .join(crate::toolchain::STAGED_DIRECTORY)
                .join(version),
        );
    }
    out.push(profile);
    out
}

pub fn beside_test_executable(executable: &Path, name: &str) -> Result<PathBuf, Missing> {
    let mut last = Missing {
        name: name.to_owned(),
        directory: String::new(),
    };
    for directory in staged_candidates(executable) {
        match beside(&directory, name) {
            Ok(found) => return Ok(found),
            Err(missing) => last = missing,
        }
    }
    Err(last)
}

pub fn released(executable: &Path, name: &str) -> Result<PathBuf, Missing> {
    let profile = profile_directory(executable);
    let Some(target) = profile.parent().map(Path::to_path_buf) else {
        return beside(&profile, name);
    };
    let release = target.join("release");
    let mut candidates = vec![release.clone()];
    if let Ok(version) = crate::toolchain::version() {
        candidates.insert(
            0,
            release
                .join(crate::toolchain::STAGED_DIRECTORY)
                .join(version),
        );
    }
    let mut last = Missing {
        name: name.to_owned(),
        directory: release.display().to_string(),
    };
    for directory in candidates {
        match beside(&directory, name) {
            Ok(found) => return Ok(found),
            Err(missing) => last = missing,
        }
    }
    Err(last)
}
