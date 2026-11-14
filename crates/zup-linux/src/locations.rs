use std::path::PathBuf;
use zup_core::TargetPath;

use crate::error::PathError;
use zup_core::{InstallLocation, SelectedScope, Target};
use zup_plan::{InstallLocationError, InstallLocationResolver};

pub const MACHINE_PROGRAMS_ROOT: &str = "/opt";

pub const MACHINE_STATE_ROOT: &str = "/var/lib/zup";

pub const MACHINE_SHARED_DATA_ROOT: &str = "/var/opt";

const PROGRAMS_NAMESPACE: &str = ".local/lib/zup/apps";

const CANONICAL_BUILD_HOME: &str = "/home/zup-build";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineRoots {
    pub programs: PathBuf,

    pub state: PathBuf,

    pub shared_data: PathBuf,
    pub environment: PathBuf,
}

impl MachineRoots {
    pub fn application_state(&self, app: &zup_core::AppId) -> PathBuf {
        self.state.join("apps").join(app.state_key().to_hex())
    }

    pub fn production() -> Self {
        Self {
            programs: PathBuf::from(MACHINE_PROGRAMS_ROOT),
            state: PathBuf::from(MACHINE_STATE_ROOT),
            shared_data: PathBuf::from(MACHINE_SHARED_DATA_ROOT),
            environment: PathBuf::from("/usr/local/lib/environment.d"),
        }
    }

    pub fn new(
        programs: PathBuf,
        state: PathBuf,
        shared_data: PathBuf,
        environment: PathBuf,
    ) -> Self {
        Self {
            programs,
            state,
            shared_data,
            environment,
        }
    }
}

pub fn user_data_home() -> Result<PathBuf, PathError> {
    let home = home_dir()?;
    let data_home = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from);
    Ok(user_data_home_in(data_home.as_deref(), &home))
}

pub fn user_data_home_in(data_home: Option<&std::path::Path>, home: &std::path::Path) -> PathBuf {
    match data_home.filter(|dir| dir.as_os_str().as_encoded_bytes().starts_with(b"/")) {
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
    machine: MachineRoots,
    user_home: Option<PathBuf>,
}

impl Default for LinuxInstallLocationResolver {
    fn default() -> Self {
        Self {
            machine: MachineRoots::production(),
            user_home: None,
        }
    }
}

impl LinuxInstallLocationResolver {
    pub(crate) fn environment_directory(&self) -> &std::path::Path {
        &self.machine.environment
    }
    pub fn with_machine_roots(machine: MachineRoots) -> Self {
        Self {
            machine,
            user_home: None,
        }
    }

    /// Fixed roots for build-time validation on any host.
    ///
    /// The runtime resolves user roots live from the installing user's
    /// environment; a build host validating a target plan needs the same
    /// *relations* without touching its own environment. The resolved values
    /// never reach an installer: the bundle carries templates, and the runtime
    /// re-resolves them live.
    pub fn for_build() -> Self {
        Self {
            machine: MachineRoots::production(),
            user_home: Some(PathBuf::from(CANONICAL_BUILD_HOME)),
        }
    }

    fn user_roots(&self) -> Result<(PathBuf, PathBuf), PathError> {
        match &self.user_home {
            Some(home) => Ok((home.clone(), user_data_home_in(None, home))),
            None => Ok((home_dir()?, user_data_home()?)),
        }
    }

    pub(crate) fn data_home(&self) -> Result<PathBuf, PathError> {
        match &self.user_home {
            Some(home) => Ok(user_data_home_in(None, home)),
            None => user_data_home(),
        }
    }

    pub(crate) fn desktop(&self) -> Result<PathBuf, PathError> {
        let path = match &self.user_home {
            Some(home) => Some(home.join("Desktop")),
            None => directories::UserDirs::new().and_then(|dirs| {
                dirs.desktop_dir()
                    .filter(|path| *path != dirs.home_dir())
                    .map(PathBuf::from)
            }),
        };
        path.ok_or(PathError::UnsupportedLocation {
            location: InstallLocation::Desktop,
            scope: "user",
            reason: "the XDG desktop directory is disabled or not configured",
        })
    }

    pub(crate) fn config_home(&self) -> Result<PathBuf, PathError> {
        match &self.user_home {
            Some(home) => Ok(home.join(".config")),
            None => Ok(std::env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .unwrap_or(home_dir()?.join(".config"))),
        }
    }

    pub(crate) fn require_environment_generator(&self) -> Result<(), PathError> {
        #[cfg(target_os = "linux")]
        if self.user_home.is_none()
            && ![
                "/usr/lib/systemd/user-environment-generators/30-systemd-environment-d-generator",
                "/lib/systemd/user-environment-generators/30-systemd-environment-d-generator",
            ]
            .iter()
            .any(|path| std::path::Path::new(path).is_file())
        {
            return Err(PathError::Refused { path: "environment.d".into(), reason: "directory PATH integration requires the systemd environment.d generator; this system does not provide it".into() });
        }
        Ok(())
    }

    pub(crate) fn validate_machine_path(&self, path: &TargetPath) -> Result<(), PathError> {
        #[cfg(target_os = "linux")]
        if self.user_home.is_none() && self.machine == MachineRoots::production() {
            crate::fs::verify_ancestor_trust(&crate::lowering::to_host_path(path)?, 0)?;
        }
        #[cfg(not(target_os = "linux"))]
        let _ = path;
        Ok(())
    }
}

impl InstallLocationResolver for LinuxInstallLocationResolver {
    fn resolve(
        &self,
        location: InstallLocation,
        scope: SelectedScope,
        target: &Target,
    ) -> Result<TargetPath, InstallLocationError> {
        let failed = |source: PathError| InstallLocationError::ResolutionFailed {
            location,
            scope,
            source: Box::new(source),
        };
        let path = match scope {
            SelectedScope::User => {
                let (home, data_home) = self.user_roots().map_err(failed)?;
                match location {
                    InstallLocation::UserData => data_home,
                    InstallLocation::Programs => user_programs_root_in(&home),
                    InstallLocation::SharedData => {
                        return Err(failed(PathError::UnsupportedLocation {
                            location,
                            scope: "user",
                            reason: "there is no machine-wide data root without a privilege mechanism",
                        }));
                    }
                    InstallLocation::Menu => data_home.join("applications"),
                    InstallLocation::Desktop => self.desktop().map_err(failed)?,
                }
            }

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
        .filter(|home| home.as_os_str().as_encoded_bytes().starts_with(b"/"))
        .ok_or(PathError::NoHome)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn linux_target() -> Target {
        Target::parse("linux-x64").expect("a Linux target")
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
    fn desktop_and_menu_locations_resolve_at_build_time() {
        let resolver = LinuxInstallLocationResolver::for_build();
        for location in [InstallLocation::Menu, InstallLocation::Desktop] {
            assert!(
                resolver
                    .resolve(location, SelectedScope::User, &linux_target())
                    .is_ok(),
                "{location} has a user location"
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

    #[test]
    fn a_build_resolver_answers_without_an_environment() {
        let resolver = LinuxInstallLocationResolver::for_build();
        let programs = resolver
            .resolve(
                InstallLocation::Programs,
                SelectedScope::User,
                &linux_target(),
            )
            .expect("a build resolver answers user scope")
            .to_string();
        assert!(programs.starts_with(CANONICAL_BUILD_HOME), "{programs}");
        let data = resolver
            .resolve(
                InstallLocation::UserData,
                SelectedScope::User,
                &linux_target(),
            )
            .expect("a build resolver answers user data")
            .to_string();
        assert!(data.starts_with(CANONICAL_BUILD_HOME), "{data}");
        assert_ne!(programs, data);
    }
}
