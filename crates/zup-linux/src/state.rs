//! Where a Linux scope's persistent state lives.
//!
//! Linux has a published answer to this and it is not a Windows directory with a
//! different name. There is no `%ProgramData%`, and pretending otherwise would
//! mean inventing a machine-wide data root whose ownership rules nothing on Linux
//! describes. The two answers that do exist come from the XDG Base Directory
//! specification:
//!
//! - **A user scope** owns `<state home>/zup`, which is
//!   `$XDG_STATE_HOME/zup` or `~/.local/state/zup` when that variable is unset.
//!   The specification places state here rather than under `data_home` precisely
//!   because this content is not portable to another machine: it is a record of
//!   what happened *on this one*.
//! - **A machine scope** owns `/var/lib/zup`. `/var/lib` is the FHS location for
//!   state that outlives any one user's session and is not part of the software
//!   itself, which is exactly what an installation's ledger, work directory and
//!   content store are.
//!
//! The invariant that matters is preserved across the pair, and it is the reason
//! these are two functions rather than one with a flag:
//!
//! - **User state may be created by the normal process.** It is the caller's own
//!   directory, and an installer that cannot record what it did because a
//!   directory was missing would be a worse installer.
//! - **Machine state must never be silently created by an unelevated process.**
//!   Creating `/var/lib/zup` requires privilege, and a process that could do it
//!   without asking is a process with more authority than the user intended to
//!   give it. So [`machine_state_root`] returns the location and never creates it,
//!   and a caller that needs the directory has to obtain authority first.
//!
//! Nothing in this module hard-codes `$HOME`. The XDG crate resolves the
//! environment, including its own refusal to accept a relative `XDG_STATE_HOME`,
//! so there is one place where the spelling of "this user's state" is decided
//! rather than one per call site.

use std::path::{Path, PathBuf};

use zup_core::SelectedScope;

/// The directory name zup's state roots end in, under whatever base a platform
/// decides the scope's state belongs in.
const STATE_FOLDER: &str = "zup";

/// The base machine state root on Linux.
///
/// A constant rather than a lookup because it is the FHS's own answer and nothing
/// on a Linux host overrides it: `/var/lib` is where state that is not software
/// lives, and there is no per-user override of it. Naming it in one place is what
/// keeps the privilege rule below checkable - a caller can see that this path is
/// outside anything the user owns.
const MACHINE_STATE_BASE: &str = "/var/lib";

/// Failures produced while resolving a state root.
#[derive(Debug, thiserror::Error)]
pub enum LinuxStateError {
    #[error("this host reports no XDG state directory")]
    NoStateHome,
    #[error("state root `{path}`: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// The state root a scope owns when the caller names none.
///
/// The user root is created, because the user owns it. The machine root is
/// returned and never created, because creating `/var/lib/zup` is a privileged
/// act and this function is callable by an unelevated process.
pub fn state_root(scope: SelectedScope) -> Result<PathBuf, LinuxStateError> {
    match scope {
        SelectedScope::User => user_state_root(),
        SelectedScope::Machine => machine_state_root(),
    }
}

/// The user scope's persistent state root, created if it is missing.
///
/// Canonicalized rather than merely absolute because everything downstream - the
/// ledger, the transaction store, the installation lock - treats a state root as
/// an identity, and two spellings of one directory are two identities to them.
/// On Linux that is cheap: `canonicalize` resolves the symlinks a user is
/// perfectly entitled to have in `$HOME`.
pub fn user_state_root() -> Result<PathBuf, LinuxStateError> {
    create_user_root(xdg::BaseDirectories::new().get_state_home())
}

/// The user root under an XDG state home, created if missing.
///
/// Split from [`user_state_root`] so the two decisions - which base directory the
/// specification resolves to, and what zup does inside it - are separately
/// testable. The second one is ours; the first is the specification's.
fn create_user_root(base: Option<PathBuf>) -> Result<PathBuf, LinuxStateError> {
    let root = base.ok_or(LinuxStateError::NoStateHome)?.join(STATE_FOLDER);
    std::fs::create_dir_all(&root).map_err(|source| io_error(&root, source))?;
    canonical(&root)
}

/// The machine scope's persistent state root, **without creating it**.
///
/// The absence of `create_dir_all` here is the whole reason this function is
/// separate from [`user_state_root`]: on Linux the two scopes differ not in which
/// base directory they sit in but in whether an ordinary process is allowed to
/// make them. A caller that reaches this and needs the directory has to obtain
/// authority first, and the absence of the directory afterwards is a diagnostic
/// about that rather than about the path.
pub fn machine_state_root() -> Result<PathBuf, LinuxStateError> {
    Ok(Path::new(MACHINE_STATE_BASE).join(STATE_FOLDER))
}

/// Canonicalize a path that exists, so a state root is one identity rather than
/// several spellings of one.
fn canonical(path: &Path) -> Result<PathBuf, LinuxStateError> {
    path.canonicalize().map_err(|source| io_error(path, source))
}

fn io_error(path: &Path, source: std::io::Error) -> LinuxStateError {
    LinuxStateError::Io {
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two scopes differ in more than which base they sit in, and the
    /// difference that matters is about authority rather than about a path: the
    /// user root is created, and the machine root is not, because making
    /// `/var/lib/zup` is a privileged act.
    #[test]
    fn the_user_root_is_created_and_the_machine_root_is_not() {
        let user = user_state_root().expect("a user state root");
        assert!(user.is_dir(), "{} should have been created", user.display());
        assert!(
            user.ends_with(STATE_FOLDER),
            "{} should end in the state folder",
            user.display()
        );

        let machine = machine_state_root().expect("a machine state root");
        assert_eq!(machine, Path::new("/var/lib/zup"));
        assert!(
            !machine.exists(),
            "creating the machine root is a privileged act, and this process has not been \
             given one"
        );
    }

    /// The scope selects which of those two answers applies, so a caller never has
    /// to spell the choice itself. A test cannot assert the *absence* of a machine
    /// directory on a machine that happens to have one, so what is pinned here is
    /// that the selection is the selection and not a flag the caller might invert.
    #[test]
    fn a_scope_selects_between_the_two_answers() {
        assert_eq!(
            state_root(SelectedScope::User).expect("user state"),
            user_state_root().expect("user state")
        );
        assert_eq!(
            state_root(SelectedScope::Machine).expect("machine state"),
            machine_state_root().expect("machine state")
        );
    }

    /// A host that resolves no XDG state home produces no state root at all, rather
    /// than a fallback somewhere zup chose. The specification's own refusal of a
    /// relative `XDG_STATE_HOME` arrives here as `None`, and inventing a
    /// directory for it would put an installation's ledger somewhere the user did
    /// not choose.
    #[test]
    fn a_host_with_no_state_home_has_no_state_root() {
        assert!(matches!(
            create_user_root(None),
            Err(LinuxStateError::NoStateHome)
        ));
    }

    /// And a state home that does resolve produces a real, created, canonical
    /// directory under it. Canonical matters: everything downstream treats a state
    /// root as an identity, and two spellings of one directory are two identities.
    #[test]
    fn a_resolved_state_home_produces_a_created_canonical_root() {
        let base = tempfile::tempdir().expect("a temp base");
        let root = create_user_root(Some(base.path().to_path_buf())).expect("a user root");
        assert!(root.is_dir(), "{} should have been created", root.display());
        assert_eq!(
            root.file_name().and_then(|name| name.to_str()),
            Some(STATE_FOLDER)
        );
        assert_eq!(
            root,
            root.canonicalize().expect("canonical"),
            "and a state root is canonical, so one directory has one identity"
        );
    }
}
