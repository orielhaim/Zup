use std::path::{Component, Path, PathBuf};

use sha2::{Digest, Sha256};
use zup_core::{SelectedScope, Sha256Digest};

use crate::transport::UserSid;

const MACHINE_CONTENT_BASE_DIRECTORY: &str = "zup-content";

pub fn preset_executable_name(executable_suffix: &str) -> String {
    format!("preset{executable_suffix}")
}

#[derive(Debug, thiserror::Error)]
pub enum ContentStoreError {
    #[error("content store identity is invalid: {0}")]
    InvalidIdentity(String),
    #[error("content store base is unavailable: {0}")]
    BaseUnavailable(String),
    #[error("content store path `{path}` is unsafe: {reason}")]
    UnsafePath { path: String, reason: String },
    #[error("content store I/O at `{path}`: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

pub fn content_store_base(
    state_root: &Path,
    scope: SelectedScope,
) -> Result<PathBuf, ContentStoreError> {
    match scope {
        SelectedScope::User => Ok(state_root.to_path_buf()),
        SelectedScope::Machine => UserSid::current()
            .map(|sid| std::env::temp_dir().join(machine_base_name(sid.display())))
            .map_err(|error| ContentStoreError::BaseUnavailable(error.to_string())),
    }
}

fn machine_base_name(sid: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(sid.as_bytes());
    let key = Sha256Digest::from_hasher(hasher).to_hex();
    format!("{MACHINE_CONTENT_BASE_DIRECTORY}-{}", &key[..32])
}

pub fn validate_content_store_base(
    state_root: &Path,
    scope: SelectedScope,
    base: &Path,
    expected_parent_sid: &str,
) -> Result<(), ContentStoreError> {
    match scope {
        SelectedScope::User if base == state_root => Ok(()),
        SelectedScope::User => Err(ContentStoreError::InvalidIdentity(
            "a user content store base must equal the state root".into(),
        )),
        SelectedScope::Machine => {
            if !base.is_absolute()
                || base
                    .components()
                    .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
            {
                return Err(ContentStoreError::InvalidIdentity(
                    "a machine content store base must be an absolute normalized path".into(),
                ));
            }
            let expected = machine_base_name(expected_parent_sid);
            if base.file_name().and_then(|name| name.to_str()) != Some(expected.as_str()) {
                return Err(ContentStoreError::InvalidIdentity(
                    "a machine content store base does not match the authenticated parent identity"
                        .into(),
                ));
            }
            Ok(())
        }
    }
}

pub fn ensure_directory(path: &Path) -> Result<(), ContentStoreError> {
    crate::path_safety::ensure_ancestor_chain(path, ensure_one)
}

fn ensure_one(path: &Path) -> Result<(), ContentStoreError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if crate::path_safety::is_real_dir(path, &metadata) => Ok(()),
        Ok(_) => Err(ContentStoreError::UnsafePath {
            path: path.display().to_string(),
            reason: "a content store directory is a reparse point or special file".into(),
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match std::fs::create_dir(path) {
                Ok(()) => ensure_one(path),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => ensure_one(path),
                Err(source) => Err(io_error(path, source)),
            }
        }
        Err(source) => Err(io_error(path, source)),
    }
}

pub fn verify_directory_chain(base: &Path, path: &Path) -> Result<(), ContentStoreError> {
    crate::path_safety::verify_within_base(
        base,
        path,
        || ContentStoreError::UnsafePath {
            path: path.display().to_string(),
            reason: "a content store path escapes its base".into(),
        },
        verify_one,
    )
}

fn verify_one(path: &Path) -> Result<(), ContentStoreError> {
    let metadata = std::fs::symlink_metadata(path).map_err(|source| io_error(path, source))?;
    if !crate::path_safety::is_real_dir(path, &metadata) {
        return Err(ContentStoreError::UnsafePath {
            path: path.display().to_string(),
            reason: "a content store directory is a reparse point or special file".into(),
        });
    }
    Ok(())
}

pub fn remove_store(base_root: &Path, store: &Path) -> Result<(), ContentStoreError> {
    let namespace = base_root.join(zup_transaction::CONTENT_STORE_DIRECTORY);
    let unsafe_path = |reason: &str| ContentStoreError::UnsafePath {
        path: store.display().to_string(),
        reason: reason.to_owned(),
    };
    if store.strip_prefix(&namespace).is_err() {
        return Err(unsafe_path(
            "a store is outside the content store namespace",
        ));
    }
    if !zup_transaction::is_store_shape(store) {
        return Err(unsafe_path(
            "a content store directory has an unexpected shape",
        ));
    }
    match std::fs::symlink_metadata(store) {
        Ok(metadata) if crate::path_safety::is_real_dir(store, &metadata) => {
            std::fs::remove_dir_all(store).map_err(|source| io_error(store, source))?
        }
        Ok(_) => {
            return Err(unsafe_path(
                "a content store is a reparse point or special file",
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => return Err(io_error(store, source)),
    }
    for ancestor in [store.parent(), store.parent().and_then(Path::parent)]
        .into_iter()
        .flatten()
    {
        let _ = std::fs::remove_dir(ancestor);
    }
    let _ = std::fs::remove_dir(&namespace);
    Ok(())
}

fn io_error(path: &Path, source: std::io::Error) -> ContentStoreError {
    ContentStoreError::Io {
        path: path.display().to_string(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_base_must_be_the_one_the_scope_authorizes() {
        let state = Path::new("state");
        assert!(validate_content_store_base(state, SelectedScope::User, state, "sid").is_ok());
        assert!(
            validate_content_store_base(state, SelectedScope::User, Path::new("other"), "sid")
                .is_err()
        );

        let good = PathBuf::from("C:/Temp").join(machine_base_name("S-1-5-21-1"));
        assert!(
            validate_content_store_base(state, SelectedScope::Machine, &good, "S-1-5-21-1").is_ok()
        );
        assert!(
            validate_content_store_base(state, SelectedScope::Machine, &good, "S-1-5-21-2")
                .is_err(),
            "a base derived from another identity is refused"
        );
        assert!(
            validate_content_store_base(state, SelectedScope::Machine, Path::new("relative"), "x")
                .is_err()
        );
    }

    #[test]
    fn removal_refuses_a_path_outside_the_namespace() {
        let base = tempfile::tempdir().unwrap();
        assert!(remove_store(base.path(), &base.path().join("elsewhere")).is_err());
    }
}
