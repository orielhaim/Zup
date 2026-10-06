//! Where a Linux installation's three directory kinds live.
//!
//! Three questions live here and they are not the same question, so they do not
//! share an answer:
//!
//! - **Zup state** - the ledger, the transaction journal, the lock - belongs to
//!   zup itself and lives under the XDG state home. It is owned by
//!   [`crate::state`], not by this module, and it is never the application's
//!   data directory.
//! - **Application user data** - what `${location.user_data}` names - follows
//!   XDG data semantics: `$XDG_DATA_HOME`, falling back to `~/.local/share`.
//!   Resolution belongs here rather than scattered across call sites, because
//!   every caller must agree about where "this user's data" is.
//! - **Installed application payload** - what `${location.programs}` names - has
//!   no freedesktop equivalent of Program Files, so zup defines its own policy:
//!   a Zup-owned programs namespace under the user's local hierarchy,
//!   `~/.local/lib/zup/apps`. It is deliberately not `~/.local/share`, which is
//!   data, and not `~/.local/bin`, which is on the user's `PATH` and would make
//!   every installed payload implicitly executable-by-name.
//!
//! The invariant is: payload, data, and state are three different directories,
//! and a caller that confuses any two of them has a bug this module exists to
//! prevent.

use std::path::PathBuf;

use zup_core::{InstallLocation, SelectedScope, TargetTriple};
use zup_platform::{InstallLocationError, InstallLocationResolver, TargetPath};

/// The programs namespace zup owns inside the user's local hierarchy.
const PROGRAMS_NAMESPACE: &str = ".local/lib/zup/apps";

/// Why a Linux install location could not be named.
#[derive(Debug, thiserror::Error)]
pub enum LinuxLocationError {
    #[error("no home directory: `$HOME` is unset, so no user location can be resolved")]
    NoHome,

    #[error("no {0} location on Linux in user scope: {1}")]
    Unsupported(InstallLocation, &'static str),

    #[error("machine scope is not supported on Linux in this phase")]
    MachineScope,
}

/// The XDG data home: `$XDG_DATA_HOME`, else `~/.local/share`.
///
/// Read through the `xdg` crate rather than re-deriving the fallback, because
/// the fallback is part of the specification and hand-rolled fallbacks drift.
pub fn user_data_home() -> Result<PathBuf, LinuxLocationError> {
    let home = home_dir()?;
    let data_home = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from);
    Ok(user_data_home_in(data_home.as_deref(), &home))
}

/// The pure half of [`user_data_home`]: the policy with the environment already
/// read.
///
/// Split out because tests must not mutate the process environment to prove a
/// fallback: env mutation is process-global, so parallel tests would race, and
/// this crate forbids `unsafe_code` while edition-2024 `set_var` is unsafe.
/// A pure function is testable without either problem.
pub fn user_data_home_in(data_home: Option<&std::path::Path>, home: &std::path::Path) -> PathBuf {
    match data_home.filter(|dir| !dir.as_os_str().is_empty()) {
        Some(dir) => dir.to_path_buf(),
        // `~/.local/share` is the XDG fallback, spelled here rather than
        // re-derived, so a test can pin it without depending on the `xdg`
        // crate's internals agreeing with this comment.
        None => home.join(".local/share"),
    }
}

/// The Zup-owned programs namespace: `~/.local/lib/zup/apps`.
///
/// Home-relative rather than XDG-relative on purpose. There is no XDG variable
/// for "where installed programs live", and deriving one from `$XDG_DATA_HOME`
/// would put payload in the data directory whenever the variable is set and
/// somewhere else whenever it is not - one policy for one question, stable
/// across environments.
pub fn user_programs_root() -> Result<PathBuf, LinuxLocationError> {
    Ok(user_programs_root_in(&home_dir()?))
}

/// The pure half of [`user_programs_root`]. See [`user_data_home_in`].
pub fn user_programs_root_in(home: &std::path::Path) -> PathBuf {
    home.join(PROGRAMS_NAMESPACE)
}

/// Maps a semantic install location onto a concrete Linux path.
#[derive(Debug, Default, Clone, Copy)]
pub struct LinuxInstallLocationResolver;

impl InstallLocationResolver for LinuxInstallLocationResolver {
    fn resolve(
        &self,
        location: InstallLocation,
        scope: SelectedScope,
        target: &TargetTriple,
    ) -> Result<TargetPath, InstallLocationError> {
        let failed = |source: LinuxLocationError| InstallLocationError::ResolutionFailed {
            location,
            scope,
            source: Box::new(source),
        };
        // Machine scope has no Linux policy in this phase: there is no
        // privilege mechanism to create machine-owned directories, so resolving
        // one would name a directory the installer could never honestly own.
        if scope == SelectedScope::Machine {
            return Err(failed(LinuxLocationError::MachineScope));
        }
        let path = match location {
            InstallLocation::UserData => user_data_home().map_err(failed)?,
            InstallLocation::Programs => user_programs_root().map_err(failed)?,
            InstallLocation::SharedData => {
                return Err(failed(LinuxLocationError::Unsupported(
                    location,
                    "there is no machine-wide data root without a privilege mechanism",
                )));
            }
            InstallLocation::Menu | InstallLocation::Desktop => {
                return Err(failed(LinuxLocationError::Unsupported(
                    location,
                    "desktop integration is deferred past this phase",
                )));
            }
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

/// The user's home directory, or why there is none.
fn home_dir() -> Result<PathBuf, LinuxLocationError> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|home| !home.as_os_str().is_empty())
        .ok_or(LinuxLocationError::NoHome)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn linux_target() -> TargetTriple {
        TargetTriple::parse("x86_64-unknown-linux-gnu").expect("a Linux target")
    }

    /// The data home follows `$XDG_DATA_HOME` when it is set, and the standard
    /// fallback when it is not. Both halves matter: a test that only covers the
    /// fallback cannot tell a resolver that ignores the variable from one that
    /// honours it.
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

    /// Payload, data, and state are three different directories. A policy that
    /// let any two coincide would let an uninstall of one take the other with it.
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

    /// Deferred locations are refused at the boundary, not answered with a
    /// directory the installer cannot honour.
    #[test]
    fn desktop_and_menu_locations_are_refused() {
        let resolver = LinuxInstallLocationResolver;
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
    fn machine_scope_has_no_linux_policy() {
        let resolver = LinuxInstallLocationResolver;
        assert!(
            resolver
                .resolve(
                    InstallLocation::Programs,
                    SelectedScope::Machine,
                    &linux_target()
                )
                .is_err()
        );
    }
}
