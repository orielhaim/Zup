//! Where an installed application's own state lives.
//!
//! An installation keeps a handful of directories that every backend agrees on,
//! because they are *installation state* rather than platform policy: the ledger
//! that records what this machine owns, the work directory a transaction builds
//! in, and the generation directories that hold one version's maintenance runtime
//! and its content.
//!
//! Two things are deliberately kept apart, and conflating them is how a portable
//! plan ends up naming a Windows directory:
//!
//! - **The layout** - what the directories are called and how they nest - is
//!   portable, and lives here.
//! - **Where a scope's state root sits** - `%LOCALAPPDATA%\zup` versus
//!   `$XDG_STATE_HOME/zup` versus `/var/lib/zup` - is a backend's answer, and each
//!   backend supplies its own.
//!
//! The layout is stated over a [`TargetTriple`] rather than over a host path so
//! that a plan can describe an installation for another machine: a scope's state
//! is a directory *name*, and the base it is joined onto is the only part that
//! needs a filesystem.
//!
//! # Why generations nest under one version
//!
//! The maintenance runtime is versioned so an upgrade stages the new copy beside
//! the old one and a transaction can roll back to bytes that are still on disk. A
//! question asked of "what this installation's window content is" therefore has to
//! span every generation it has owned, which is why
//! [`maintenance_root`] and [`maintenance_directory`] are both here: one answers
//! for all of them, one for a single version.
//!
//! [`TargetTriple`]: zup_core::TargetTriple

use std::path::{Path, PathBuf};

use semver::Version;
use zup_core::{AppId, SelectedScope};

/// The directory every zup state root's installed content hangs under.
const MAINTENANCE_DIRECTORY: &str = "maintenance";

/// The file name of the selected variant's content package, beside the
/// maintenance runtime.
///
/// Not an executable name. The package is a portable document - a manifest and
/// exactly the blobs its variant needs - and the runtime beside it carries
/// whatever suffix its *target* names it with. Keeping the two apart is what lets
/// one maintenance directory describe a Linux installation and a Windows one
/// without either inventing the other's naming.
pub const MAINTENANCE_PACKAGE_NAME: &str = "variant.zup";

/// The file name of the artifact index, beside the maintenance runtime.
pub const MAINTENANCE_INDEX_NAME: &str = "artifact.json";

/// The directory name every maintenance runtime is persisted under.
pub const MAINTENANCE_RUNTIME_DIRECTORY: &str = "maintenance";

/// The directory name every scope's state root ends in.
///
/// A scope's root is the backend's to place, but the *name* is one fact about
/// zup's own layout rather than about a platform, and two backends disagreeing
/// about it would mean one of them could not read the other's installations.
pub const STATE_FOLDER: &str = "zup";

/// The name a scope contributes to a state directory.
///
/// A value rather than two format strings because the scope name appears in three
/// places - the maintenance root, the content-store root, and a diagnostic - and
/// three spellings of one segment is three chances for two of them to disagree
/// about which installation is meant.
pub const fn scope_name(scope: SelectedScope) -> &'static str {
    match scope {
        SelectedScope::User => "user",
        SelectedScope::Machine => "machine",
    }
}

/// The directory every version of one application's maintenance runtime lives in.
///
/// The version is one level below this, so a question about an installation's
/// window content has to span all of them: an update owns two generations until
/// the old one is retired.
pub fn maintenance_root(state_root: &Path, app_id: &AppId, scope: SelectedScope) -> PathBuf {
    state_root
        .join(MAINTENANCE_DIRECTORY)
        .join(app_id.as_str())
        .join(scope_name(scope))
}

/// The directory an installed copy's content lives in, which is where a staged
/// variant's contents end up after the transaction commits.
///
/// The version is a [`Version`] rather than a string because a caller holding one
/// has already parsed it, and a `&str` here would invite a caller to pass a
/// version that was never checked - and the directory name is what tells one
/// generation of an installation from the next.
pub fn maintenance_directory(
    state_root: &Path,
    app_id: &AppId,
    scope: SelectedScope,
    version: &Version,
) -> PathBuf {
    maintenance_root(state_root, app_id, scope).join(version.to_string())
}

/// Where an installation persists the maintenance runtime for one scope and
/// version.
///
/// `executable_suffix` is the target's own, so the file is named the way that
/// target's binaries are named. A caller that passed one platform's suffix for
/// another target's installation would persist a file nothing launches.
pub fn maintenance_runtime_path(
    state_root: &Path,
    app_id: &AppId,
    scope: SelectedScope,
    version: &Version,
    executable_suffix: &str,
) -> PathBuf {
    maintenance_directory(state_root, app_id, scope, version).join(format!(
        "{MAINTENANCE_RUNTIME_DIRECTORY}{executable_suffix}"
    ))
}

/// Whether this process is the maintenance copy an installation persisted.
///
/// Recognized by where it lives rather than by what it is called, so the name
/// stays free to change and an installer a user renamed is still recognized for
/// what it is.
///
/// Stated over the layout's own shape - a `maintenance` directory with a version
/// level below it - rather than over one installation's root, because the caller
/// that asks is deciding what *role* a process was started in from its path alone,
/// before it knows which application or scope the path belongs to. Anchoring on a
/// root would make that question unanswerable at the only moment it is asked.
pub fn is_maintenance_path(path: &Path) -> bool {
    let components = path
        .components()
        .map(|component| component.as_os_str())
        .collect::<Vec<_>>();
    components
        .windows(2)
        .any(|pair| pair[0] == MAINTENANCE_DIRECTORY && pair[1] != MAINTENANCE_DIRECTORY)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zup_core::TargetTriple;

    fn app_id() -> AppId {
        AppId::new("com.acme.desktop").expect("a valid id")
    }

    fn target() -> TargetTriple {
        TargetTriple::parse("x86_64-pc-windows-msvc").expect("a target")
    }

    fn version(text: &str) -> Version {
        Version::parse(text).expect("a version")
    }

    /// The two scopes differ in a directory *name* as well as a base, and the two
    /// installations they describe have different ledgers and different install
    /// directories - which is why they must not share a directory.
    #[test]
    fn the_scopes_never_share_a_maintenance_root() {
        let state = Path::new("/var/lib/zup");
        let user = maintenance_root(state, &app_id(), SelectedScope::User);
        let machine = maintenance_root(state, &app_id(), SelectedScope::Machine);
        assert_ne!(user, machine);
        assert!(user.ends_with("user"));
        assert!(machine.ends_with("machine"));
        assert_eq!(
            user.parent().and_then(Path::file_name),
            Some(app_id().as_str().as_ref()),
            "both nest under the application, so a reader can ask about one app across scopes"
        );
    }

    /// A generation is one level below the root, and the root is what answers a
    /// question about an installation rather than about one release of it.
    #[test]
    fn a_generation_is_one_version_below_the_root() {
        let state = Path::new("/var/lib/zup");
        let root = maintenance_root(state, &app_id(), SelectedScope::User);
        let one = maintenance_directory(state, &app_id(), SelectedScope::User, &version("1.4.0"));
        let two = maintenance_directory(state, &app_id(), SelectedScope::User, &version("1.5.0"));
        assert!(one.starts_with(&root));
        assert!(two.starts_with(&root));
        assert_ne!(
            one, two,
            "two generations are two directories while both are owned"
        );
    }

    /// The persisted runtime is named by the *target's* convention. A Linux target
    /// produces a file with no suffix, and a Windows target one with `.exe` - from
    /// the same call, differing only in the argument that says which target it is.
    #[test]
    fn the_persisted_runtime_is_named_by_the_targets_convention() {
        let state = Path::new("/var/lib/zup");
        let windows = maintenance_runtime_path(
            state,
            &app_id(),
            SelectedScope::User,
            &version("1.4.0"),
            target().executable_suffix(),
        );
        let linux = maintenance_runtime_path(
            state,
            &app_id(),
            SelectedScope::User,
            &version("1.4.0"),
            TargetTriple::parse("x86_64-unknown-linux-gnu")
                .expect("a target")
                .executable_suffix(),
        );
        assert_eq!(
            windows.file_name().and_then(|n| n.to_str()),
            Some("maintenance.exe")
        );
        assert_eq!(
            linux.file_name().and_then(|n| n.to_str()),
            Some("maintenance")
        );
    }

    /// The maintenance copy is recognized by where it lives, not by its name, so
    /// an installer a user renamed is still recognized. A path with no version
    /// level below the maintenance directory is not a persisted runtime, and a
    /// file outside one is not this installation's at all.
    ///
    /// The paths are composed rather than written as text, because a path spelled
    /// with one host's separator is a single filename on the other - which is the
    /// same reason the function is stated over components and the same reason a
    /// literal here would test the wrong host.
    #[test]
    fn a_persisted_runtime_is_recognized_by_where_it_lives() {
        let persisted = Path::new("zup")
            .join(MAINTENANCE_DIRECTORY)
            .join(app_id().as_str())
            .join("user")
            .join("1.4.0")
            .join(format!("{MAINTENANCE_RUNTIME_DIRECTORY}.exe"));
        assert!(is_maintenance_path(&persisted));

        assert!(
            !is_maintenance_path(
                &Path::new("Downloads").join(format!("{MAINTENANCE_RUNTIME_DIRECTORY}-Setup.exe"))
            ),
            "a file outside any maintenance directory is an installation medium, not a runtime"
        );
        assert!(
            !is_maintenance_path(&Path::new("zup").join(MAINTENANCE_DIRECTORY)),
            "the maintenance directory itself is not a generation's runtime"
        );
    }

    /// The package and the index beside a runtime are portable documents, so their
    /// names carry no platform suffix. A `.exe` here would be a portable concept
    /// naming one platform's convention.
    #[test]
    fn the_sidecar_documents_carry_no_executable_suffix() {
        for name in [MAINTENANCE_PACKAGE_NAME, MAINTENANCE_INDEX_NAME] {
            assert!(
                !name.ends_with(".exe"),
                "{name} is a document, not a program"
            );
        }
    }
}
