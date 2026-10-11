use std::path::PathBuf;

use crate::error::PathError;
use zup_core::{InstallLocation, SelectedScope, TargetTriple};
use zup_platform::{InstallLocationError, InstallLocationResolver, TargetPath};

const PROGRAMS_NAMESPACE: &str = ".local/lib/zup/apps";

pub fn user_data_home() -> Result<PathBuf, PathError> {
    let home = home_dir()?;
    let data_home = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from);
    Ok(user_data_home_in(data_home.as_deref(), &home))
}

pub fn user_data_home_in(data_home: Option<&std::path::Path>, home: &std::path::Path) -> PathBuf {
    match data_home.filter(|dir| !dir.as_os_str().is_empty()) {
        Some(dir) => dir.to_path_buf(),

        None => home.join(".local/share"),
    }
}

pub fn user_programs_root() -> Result<PathBuf, PathError> {
    Ok(user_programs_root_in(&home_dir()?))
}

pub fn user_programs_root_in(home: &std::path::Path) -> PathBuf {
    home.join(PROGRAMS_NAMESPACE)
}

#[derive(Debug, Clone)]
pub struct LinuxInstallLocationResolver {
    machine: crate::machine::MachineRoots,
}

impl Default for LinuxInstallLocationResolver {
    fn default() -> Self {
        Self {
            machine: crate::machine::MachineRoots::production(),
        }
    }
}

impl LinuxInstallLocationResolver {
    pub fn with_machine_roots(machine: crate::machine::MachineRoots) -> Self {
        Self { machine }
    }
}

impl InstallLocationResolver for LinuxInstallLocationResolver {
    fn resolve(
        &self,
        location: InstallLocation,
        scope: SelectedScope,
        target: &TargetTriple,
    ) -> Result<TargetPath, InstallLocationError> {
        let failed = |source: PathError| InstallLocationError::ResolutionFailed {
            location,
            scope,
            source: Box::new(source),
        };
        let path = match scope {
            SelectedScope::User => match location {
                InstallLocation::UserData => user_data_home().map_err(failed)?,
                InstallLocation::Programs => user_programs_root().map_err(failed)?,
                InstallLocation::SharedData => {
                    return Err(failed(PathError::UnsupportedLocation {
                        location,
                        scope: "user",
                        reason: "there is no machine-wide data root without a privilege mechanism",
                    }));
                }
                InstallLocation::Menu | InstallLocation::Desktop => {
                    return Err(failed(PathError::UnsupportedLocation {
                        location,
                        scope: "user",
                        reason: "desktop integration is deferred past this phase",
                    }));
                }
            },

            SelectedScope::Machine => match location {
                InstallLocation::Programs => self.machine.programs.clone(),
                InstallLocation::SharedData => self.machine.shared_data.clone(),
                InstallLocation::UserData => {
                    return Err(failed(PathError::UnsupportedLocation {
                        location,
                        scope: "machine",
                        reason: "a machine installation has no per-user data directory",
                    }));
                }
                InstallLocation::Menu | InstallLocation::Desktop => {
                    return Err(failed(PathError::UnsupportedLocation {
                        location,
                        scope: "machine",
                        reason: "machine desktop integration is deferred past this phase",
                    }));
                }
            },
        };
        TargetPath::new(target, path.to_string_lossy()).map_err(|error| {
            InstallLocationError::ResolutionFailed {
                location,
                scope,
                source: Box::new(error),
            }
        })
    }
}

fn home_dir() -> Result<PathBuf, PathError> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|home| !home.as_os_str().is_empty())
        .ok_or(PathError::NoHome)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn linux_target() -> TargetTriple {
        TargetTriple::parse("x86_64-unknown-linux-gnu").expect("a Linux target")
    }

    #[test]
    fn user_data_follows_xdg_data_home() {
        let custom = PathBuf::from("/tmp/custom-data");
        let home = PathBuf::from("/home/someone");
        assert_eq!(user_data_home_in(Some(&custom), &home), custom);
    }

    #[test]
    fn user_data_falls_back_to_the_local_share() {
        let home = PathBuf::from("/home/someone");
        assert_eq!(
            user_data_home_in(None, &home),
            PathBuf::from("/home/someone/.local/share")
        );
        assert_eq!(
            user_data_home_in(Some(std::path::Path::new("")), &home),
            PathBuf::from("/home/someone/.local/share"),
            "an empty variable is unset, not a relative directory"
        );
    }

    #[test]
    fn the_three_directory_kinds_are_distinct() {
        let home = PathBuf::from("/home/someone");
        let data = user_data_home_in(None, &home);
        let programs = user_programs_root_in(&home);
        assert_ne!(data, programs);
        assert_eq!(programs, PathBuf::from("/home/someone/.local/lib/zup/apps"));
        assert!(
            !programs.starts_with(&data),
            "payload must not live inside the data directory"
        );
    }

    #[test]
    fn desktop_and_menu_locations_are_refused() {
        let resolver = LinuxInstallLocationResolver::default();
        for location in [InstallLocation::Menu, InstallLocation::Desktop] {
            assert!(
                resolver
                    .resolve(location, SelectedScope::User, &linux_target())
                    .is_err(),
                "{location} must be refused, not answered"
            );
        }
    }

    #[test]
    fn machine_scope_follows_fhs() {
        let resolver = LinuxInstallLocationResolver::default();
        let programs = resolver
            .resolve(
                InstallLocation::Programs,
                SelectedScope::Machine,
                &linux_target(),
            )
            .expect("machine payload has a policy");
        assert_eq!(programs.to_string(), "/opt");
        let shared = resolver
            .resolve(
                InstallLocation::SharedData,
                SelectedScope::Machine,
                &linux_target(),
            )
            .expect("machine variable data has a policy");
        assert_eq!(shared.to_string(), "/var/opt");
        for location in [
            InstallLocation::UserData,
            InstallLocation::Menu,
            InstallLocation::Desktop,
        ] {
            assert!(
                resolver
                    .resolve(location, SelectedScope::Machine, &linux_target())
                    .is_err(),
                "{location} has no machine policy"
            );
        }
    }

    #[test]
    fn machine_payload_data_and_state_are_distinct() {
        let resolver = LinuxInstallLocationResolver::default();
        let programs = resolver
            .resolve(
                InstallLocation::Programs,
                SelectedScope::Machine,
                &linux_target(),
            )
            .expect("programs")
            .to_string();
        let shared = resolver
            .resolve(
                InstallLocation::SharedData,
                SelectedScope::Machine,
                &linux_target(),
            )
            .expect("shared data")
            .to_string();
        assert_ne!(programs, shared);
        for root in [programs, shared] {
            assert!(root.starts_with('/'));
            assert!(!root.starts_with("/home"));
            assert!(!root.starts_with("/root"));
        }
    }
}
