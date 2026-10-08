use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum LockError {
    #[error("I/O failed at `{path}`: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

#[derive(Debug)]
pub struct InstallationLock {
    file: std::fs::File,
    key: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockScope {
    Lifecycle,
    Bootstrap,
}

impl LockScope {
    const fn prefix(self) -> &'static str {
        match self {
            Self::Lifecycle => "zup-install",
            Self::Bootstrap => "zup-bootstrap",
        }
    }
}

impl InstallationLock {
    pub fn lock_key(app_id: &str, scope: &str) -> String {
        format!("zup-install-{}-{}", sanitize(app_id), sanitize(scope))
    }

    pub fn key_for(app_id: &str, scope: &str, kind: LockScope) -> String {
        match kind {
            LockScope::Lifecycle => Self::lock_key(app_id, scope),
            LockScope::Bootstrap => {
                format!("{}-{}-{}", kind.prefix(), sanitize(app_id), sanitize(scope))
            }
        }
    }

    pub fn scope_token(scope: &str) -> String {
        sanitize(scope)
    }

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

    pub fn key(&self) -> &str {
        &self.key
    }
}

impl Drop for InstallationLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

fn lock_path(state_root: &Path, key: &str) -> std::path::PathBuf {
    state_root.join(format!("{key}.lock"))
}

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

    /// locks, because a bootstrap happens before an application exists and must not
    #[test]
    fn a_bootstrap_lock_is_not_the_lifecycle_lock() {
        assert_ne!(
            InstallationLock::key_for("com.acme.desktop", "user", LockScope::Lifecycle),
            InstallationLock::key_for("com.acme.desktop", "user", LockScope::Bootstrap)
        );
    }

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
