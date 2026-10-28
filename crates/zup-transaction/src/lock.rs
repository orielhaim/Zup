use std::path::{Path, PathBuf};

use crate::private;

#[derive(Debug, thiserror::Error)]
pub enum LockError {
    #[error("cannot acquire installation lock `{key}`: {source}")]
    Acquire {
        key: String,
        #[source]
        source: std::io::Error,
    },

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

    pub fn try_lock(file: std::fs::File, key: String) -> Result<Option<Self>, LockError> {
        match file.try_lock() {
            Ok(()) => Ok(Some(Self { file, key })),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(source)) => Err(LockError::Acquire { key, source }),
        }
    }

    pub fn try_acquire(state_root: &Path, key: &str) -> Result<Option<Self>, LockError> {
        let path = lock_path(state_root, key);
        let directory = lock_dir(state_root)?;
        let file = private::open_lock(&directory, &format!("{key}.lock")).map_err(|source| {
            LockError::Io {
                path: path.display().to_string(),
                source: match source {
                    private_fs::Error::Io(source) => source,
                    other => std::io::Error::other(other),
                },
            }
        })?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self {
                file,
                key: key.to_owned(),
            })),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(source)) => Err(LockError::Acquire {
                key: key.to_owned(),
                source,
            }),
        }
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

fn lock_dir(state_root: &Path) -> Result<private_fs::PrivateDir, LockError> {
    let path = state_root.join("locks");
    private::ensure_dir(&path).map_err(|source| LockError::Io {
        path: path.display().to_string(),
        source: match source {
            private_fs::Error::Io(source) => source,
            other => std::io::Error::other(other),
        },
    })
}

fn lock_path(state_root: &Path, key: &str) -> PathBuf {
    state_root.join("locks").join(format!("{key}.lock"))
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

    #[test]
    fn releasing_a_lock_keeps_the_same_file() {
        let root = tempfile::tempdir().expect("a temp root");
        let key = InstallationLock::lock_key("com.acme.desktop", "user");
        let path = lock_path(root.path(), &key);
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        let held = InstallationLock::try_acquire(root.path(), &key)
            .expect("acquire")
            .expect("held");
        #[cfg(unix)]
        let identity = std::fs::metadata(&path).expect("stat").ino();
        drop(held);
        assert!(path.exists(), "the lock file stays after release");
        let again = InstallationLock::try_acquire(root.path(), &key)
            .expect("acquire")
            .expect("held again");
        #[cfg(unix)]
        assert_eq!(
            std::fs::metadata(&path).expect("stat").ino(),
            identity,
            "a new acquire must lock the same file, not a replacement"
        );
        drop(again);
    }
}
