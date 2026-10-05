//! The Windows half of the content store.
//!
//! A content store's *identity* and *layout* are portable installation semantics
//! and live in `zup-transaction`; what is left here is the two questions only
//! Windows can answer:
//!
//! - **Where a store's base root sits.** A user-scope store lives in the scope's
//!   own state root. A machine-scope store cannot: a launcher has no authority
//!   over the machine state root, so it stages into a per-user directory named for
//!   the identity that is allowed to write there. That is a statement about
//!   Windows' privilege model, not about content stores.
//! - **Whether a directory is safe to write through.** Every directory on the way
//!   is checked for being a real directory rather than a reparse point, because on
//!   Windows a reparse point redirects a write without presenting as a link.
//!
//! The split matters because those are exactly the two things a Linux backend
//! answers differently - `/tmp` versus a `0700` staging directory, and symlinks
//! rather than reparse points - and neither of them is a property of a content
//! store.

use std::path::{Component, Path, PathBuf};

use sha2::{Digest, Sha256};
use zup_core::{SelectedScope, Sha256Digest};

use crate::transport::UserSid;

/// The machine-scope base directory prefix, outside the machine state root so
/// an unelevated dispatcher can stage into it.
const MACHINE_CONTENT_BASE_DIRECTORY: &str = "zup-content";

/// The file name an installer image's own preset is written out under, before
/// anything has been committed.
///
/// An install that has committed does not use this: its preset is installed
/// content under its own maintenance directory, addressed by digest. This is for
/// the one window that exists only while the install that carries it is still
/// running, and beside the image because that image is the only thing that can
/// be certain of writing there.
///
/// The suffix is the target's rather than a constant: the name is part of what a
/// target's binaries are called, so a host that assumed one platform's suffix
/// would look for a file no other platform's composition writes.
pub fn preset_executable_name(executable_suffix: &str) -> String {
    format!("preset{executable_suffix}")
}

/// Failures produced by the content store.
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

/// The base root a store of `scope` may live under.
///
/// A user-scope store lives in the state root, which the signed-in user already
/// owns. A machine-scope store lives in a per-user directory outside the machine
/// state root, because a launcher has no authority over the machine state root
/// and must not pretend otherwise.
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

/// Refuse a base root that is not one this process may use.
///
/// `expected_parent_sid` is the identity the elevated worker's parent presented,
/// so a machine-scope base is accepted only when its name is the one derived
/// from that identity.
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

/// Create `directory` and every missing parent, refusing anything that is not a
/// real directory.
pub fn ensure_directory(path: &Path) -> Result<(), ContentStoreError> {
    for ancestor in path.ancestors().collect::<Vec<_>>().into_iter().rev() {
        ensure_one(ancestor)?;
    }
    Ok(())
}

fn ensure_one(path: &Path) -> Result<(), ContentStoreError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if !is_reparse_point(path, &metadata) && metadata.is_dir() => Ok(()),
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

/// Verify a directory chain from `base` to `path`, refusing reparse points.
pub fn verify_directory_chain(base: &Path, path: &Path) -> Result<(), ContentStoreError> {
    let relative = path
        .strip_prefix(base)
        .map_err(|_| ContentStoreError::UnsafePath {
            path: path.display().to_string(),
            reason: "a content store path escapes its base".into(),
        })?;
    let mut current = base.to_path_buf();
    verify_one(&current)?;
    for component in relative.components() {
        current.push(component);
        verify_one(&current)?;
    }
    Ok(())
}

fn verify_one(path: &Path) -> Result<(), ContentStoreError> {
    let metadata = std::fs::symlink_metadata(path).map_err(|source| io_error(path, source))?;
    if is_reparse_point(path, &metadata) || !metadata.is_dir() {
        return Err(ContentStoreError::UnsafePath {
            path: path.display().to_string(),
            reason: "a content store directory is a reparse point or special file".into(),
        });
    }
    Ok(())
}

/// Remove a store directory and the empty namespaces above it.
///
/// The shape check is `zup-transaction`'s, because the shape is the layout's; what
/// is left here is the Windows half - refusing to delete through a reparse point,
/// which on Windows is the thing that makes a recursive delete unsafe.
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
        Ok(metadata) if !is_reparse_point(store, &metadata) && metadata.is_dir() => {
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

fn is_reparse_point(path: &Path, metadata: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        metadata.file_type().is_symlink() || crate::fs_bindings::is_reparse_point(path)
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        metadata.file_type().is_symlink()
    }
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

    /// A user-scope store is the caller's state root, and a machine-scope store
    /// is a directory named for the identity that is allowed to write it. Neither
    /// may be pointed anywhere else, or a caller that passes the wrong base gets
    /// a store outside any directory it is allowed to touch.
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
