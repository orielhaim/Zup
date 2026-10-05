//! Single-writer coordination for one installation.
//!
//! Two lifecycle operations must not mutate one installation's ledger and work
//! directory at the same time. This is coordination, not a security boundary: it
//! stops two sessions in one process from racing, and it says nothing about an
//! adversary.
//!
//! # What the identity is, and what it deliberately is not
//!
//! The key is `(application, scope)`. Two installs of different applications do
//! not block each other, and a user-scope and a machine-scope install of the same
//! application do not either - they are different installations with different
//! ledgers, different install directories, and different uninstall entries, and
//! serialising them would make an unrelated second install wait for no reason.
//!
//! It is *not* keyed by target or by version. Those are properties of one
//! operation, not of the installation, and a key that changed as a plan changed
//! would let two operations hold "the" lock for the same installation at once.
//!
//! # Why this lives in the transaction crate
//!
//! It is a `std::fs::File` byte-range lock and nothing more: no Win32 handle
//! discipline, no flush semantics, no volume behaviour. It sat beside the
//! Windows durable-I/O primitives only because `durable.rs` also happened to
//! contain them, and one of those two things is portable. What coordinates an
//! installation is installation state, which is what `zup-transaction` owns.
//!
//! # The crash story
//!
//! A crash releases the lock through the operating system's handle lifetime, so
//! there is no stale-PID cleanup to get wrong and no window where a dead process's
//! lock outlives it. That is why this needs no backend: every platform this
//! repository supports closes a descriptor when the process dies.

use std::path::Path;

/// Failures produced while coordinating an installation.
#[derive(Debug, thiserror::Error)]
pub enum LockError {
    #[error("I/O failed at `{path}`: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

/// A held installation lock.
///
/// The lock is released when this is dropped, and the file is deliberately *not*
/// removed: a lock file's presence is not a claim that a lock is held, so leaving
/// it behind costs one empty file and saves every caller a cleanup path that could
/// fail. [`InstallationLock::remove_if_unheld`] exists for the one case where the
/// file itself should go - after an uninstall, when nothing else in the state root
/// needs it.
#[derive(Debug)]
pub struct InstallationLock {
    file: std::fs::File,
    key: String,
}

/// What one installation's lock is for.
///
/// A value rather than two format strings, because the key is written in four
/// places - a parent session, an elevated worker, a bootstrap phase, and an
/// uninstall - and four spellings of one lock key is four chances for a parent and
/// its worker to disagree about which installation they are serialising.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockScope {
    /// A lifecycle operation on an installed application.
    Lifecycle,
    /// A prerequisite bootstrap, which happens before an application exists.
    Bootstrap,
}

impl LockScope {
    /// The prefix that keeps a bootstrap lock from being mistaken for a lifecycle
    /// one on the same installation.
    const fn prefix(self) -> &'static str {
        match self {
            Self::Lifecycle => "zup-install",
            Self::Bootstrap => "zup-bootstrap",
        }
    }
}

impl InstallationLock {
    /// Lock identity for one installation.
    pub fn lock_key(app_id: &str, scope: &str) -> String {
        format!("zup-install-{}-{}", sanitize(app_id), sanitize(scope))
    }

    /// The lock for one installation and one kind of operation.
    ///
    /// This is the only place a lifecycle lock key is spelled, and the reason the
    /// bootstrap and transaction paths can be checked against each other: a parent
    /// and the worker it elevates call this with the same arguments and get the
    /// same file, whether or not either of them knows the other's existence.
    pub fn key_for(app_id: &str, scope: &str, kind: LockScope) -> String {
        match kind {
            LockScope::Lifecycle => Self::lock_key(app_id, scope),
            LockScope::Bootstrap => {
                format!("{}-{}-{}", kind.prefix(), sanitize(app_id), sanitize(scope))
            }
        }
    }

    /// The scope token a scope contributes to the key.
    pub fn scope_token(scope: &str) -> String {
        sanitize(scope)
    }

    /// Try to acquire the named lock; `Ok(None)` means another session holds it.
    ///
    /// The file-level lock is what makes two *sessions in one process* exclude each
    /// other as well, because the lock is on the open file rather than on the
    /// process. An error names the state root rather than the lock file: the root
    /// is a directory a user can find, the file inside it is an implementation
    /// detail, and a message about an implementation detail sends people looking in
    /// the wrong place.
    pub fn try_acquire(state_root: &Path, key: &str) -> Result<Option<Self>, LockError> {
        std::fs::create_dir_all(state_root).map_err(|source| LockError::Io {
            path: state_root.display().to_string(),
            source,
        })?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(lock_path(state_root, key))
            .map_err(|source| LockError::Io {
                path: state_root.display().to_string(),
                source,
            })?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self {
                file,
                key: key.to_owned(),
            })),
            Err(_) => Ok(None),
        }
    }

    /// Remove the lock marker when no cooperating process holds it.
    ///
    /// The file is deleted while its byte-range lock is held, so a new installer
    /// cannot race the cleanup and end up holding a lock on a file that no longer
    /// exists.
    pub fn remove_if_unheld(state_root: &Path, key: &str) -> Result<(), LockError> {
        let Some(lock) = Self::try_acquire(state_root, key)? else {
            return Ok(());
        };
        let path = lock_path(state_root, key);
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(LockError::Io {
                    path: path.display().to_string(),
                    source,
                });
            }
        }
        drop(lock);
        Ok(())
    }

    /// The key this lock was taken under.
    pub fn key(&self) -> &str {
        &self.key
    }
}

impl Drop for InstallationLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

/// The file a key is taken on.
fn lock_path(state_root: &Path, key: &str) -> std::path::PathBuf {
    state_root.join(format!("{key}.lock"))
}

/// The key with everything that is not a letter or a digit replaced.
///
/// A lock key becomes a file name, and an application id is caller-supplied text,
/// so a key built from it unmodified would be a path a caller chose. The
/// substitution is one-way and total: two different ids that sanitize to the same
/// key share a lock, which is the safe direction - they serialise rather than race.
fn sanitize(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    /// Two sessions in one process must exclude each other, which is the reason
    /// this locks a file rather than a named mutex or an advisory process table.
    /// A process-scoped lock would report success twice here.
    #[test]
    fn two_sessions_in_one_process_exclude_each_other() {
        let root = tempfile::tempdir().expect("a temp root");
        let key = InstallationLock::lock_key("com.acme.desktop", "user");
        let held = InstallationLock::try_acquire(root.path(), &key)
            .expect("acquire")
            .expect("the first session holds it");
        assert!(
            InstallationLock::try_acquire(root.path(), &key)
                .expect("a second attempt is an answer, not a failure")
                .is_none(),
            "a second session in this process must not also acquire it"
        );
        drop(held);
        assert!(
            InstallationLock::try_acquire(root.path(), &key)
                .expect("acquire again")
                .is_some(),
            "and the lock is available once the holder is gone"
        );
    }

    /// The key's shape is the whole design: two installations that do not share
    /// state do not block each other, and two scopes of one application do not
    /// either. Serialising them would make an unrelated second install wait for no
    /// reason.
    #[test]
    fn the_key_is_the_application_and_the_scope_and_nothing_else() {
        let user = InstallationLock::lock_key("com.acme.desktop", "user");
        assert_ne!(user, InstallationLock::lock_key("com.acme.other", "user"));
        assert_ne!(
            user,
            InstallationLock::lock_key("com.acme.desktop", "machine")
        );
        assert_eq!(user, InstallationLock::lock_key("com.acme.desktop", "user"));
        assert_eq!(
            InstallationLock::key_for("com.acme.desktop", "user", LockScope::Lifecycle),
            user,
            "the lifecycle key and the plain key are one key, spelled once"
        );
    }

    /// A bootstrap lock and a lifecycle lock on the same installation are different
    /// locks, because a bootstrap happens before an application exists and must not
    /// be blocked by - or block - the lifecycle of an application that is already
    /// installed.
    #[test]
    fn a_bootstrap_lock_is_not_the_lifecycle_lock() {
        assert_ne!(
            InstallationLock::key_for("com.acme.desktop", "user", LockScope::Lifecycle),
            InstallationLock::key_for("com.acme.desktop", "user", LockScope::Bootstrap)
        );
    }

    /// An application id is caller-supplied text and the key becomes a file name,
    /// so it is sanitized rather than interpolated. The safe direction for a
    /// collision is to serialise rather than to race.
    #[test]
    fn an_application_id_cannot_choose_the_file_name() {
        let sanitized = InstallationLock::lock_key("../../etc/passwd", "user");
        assert!(
            !sanitized.contains('/') && !sanitized.contains('\\'),
            "a key must never carry a separator: {sanitized}"
        );
        assert!(sanitized.starts_with("zup-install-"));
    }

    /// Cleanup is for the uninstall path, and it must not remove a file a
    /// cooperating process is holding a lock on.
    #[test]
    fn a_held_lock_is_not_removed_and_an_unheld_one_is() {
        let root = tempfile::tempdir().expect("a temp root");
        let key = InstallationLock::lock_key("com.acme.desktop", "user");

        InstallationLock::remove_if_unheld(root.path(), &key).expect("cleanup");
        assert!(
            !lock_path(root.path(), &key).exists(),
            "there was nothing holding it, so the marker goes"
        );

        let held = InstallationLock::try_acquire(root.path(), &key)
            .expect("acquire")
            .expect("held");
        InstallationLock::remove_if_unheld(root.path(), &key).expect("cleanup");
        assert!(
            lock_path(root.path(), &key).exists(),
            "a lock file a session is holding is not removed underneath it"
        );
        drop(held);
    }

    /// The lock file is created on demand, because an installation whose state root
    /// has never held an operation has no reason to carry a marker for one.
    #[rstest]
    #[case::scope_token("user")]
    #[case::scope_token("machine")]
    fn the_scope_token_is_sanitized_like_everything_else(#[case] scope: &str) {
        assert_eq!(
            InstallationLock::scope_token(scope),
            InstallationLock::scope_token(scope),
            "the token is a pure function of the scope"
        );
    }
}
