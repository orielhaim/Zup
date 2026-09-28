//! The maintenance content store.
//!
//! A selected variant's content does not have to live inside the executable that
//! runs it. It may live in a single-target artifact's own resources, in a
//! universal artifact, or in a store the installation persisted. This module is
//! the last of those: a versioned, per-scope, per-variant directory holding the
//! selected variant's native runtime, its content package, and the artifact
//! index that describes both.
//!
//! Two properties matter and are enforced here rather than trusted:
//!
//! - **A store belongs to one application, one scope, one target, and one
//!   artifact.** The directory name is a digest over exactly those, so a store
//!   for an ARM64 variant can never be mistaken for an x64 one, and a store from
//!   a different release is a different directory.
//! - **A privileged worker is never handed an arbitrary path.** The base root is
//!   one of two, derived from the scope and bound to the calling user's
//!   identity, and every directory on the way is checked for being a real
//!   directory rather than a reparse point.

use std::path::{Component, Path, PathBuf};

use semver::Version;
use sha2::{Digest, Sha256};
use zup_core::{AppId, SelectedScope, Sha256Digest, TargetTriple};

use crate::transport::UserSid;

/// The directory a store lives in, under its base root.
pub const CONTENT_STORE_DIRECTORY: &str = ".zup-content";
/// The machine-scope base directory prefix, outside the machine state root so
/// an unelevated dispatcher can stage into it.
const MACHINE_CONTENT_BASE_DIRECTORY: &str = "zup-content";
const IDENTITY_DOMAIN: &[u8] = b"zup/content-store/identity/v1\0";
const APP_DOMAIN: &[u8] = b"zup/content-store/app/v1\0";

/// The file name of an installed maintenance executable.
///
/// Not `Setup.exe`. The file a person downloads is an installation medium named
/// for the application; the file an installation persists beside the application
/// is the runtime that maintains it, and it is named for that role. The two are
/// the same bytes with different jobs, and Apps & Features, the restart manager,
/// and the recovery path all address the persisted one by this name.
pub const MAINTENANCE_EXECUTABLE_NAME: &str = "maintenance.exe";
/// The file name of the selected variant's content package, beside the
/// maintenance executable.
pub const MAINTENANCE_PACKAGE_NAME: &str = "variant.zup";
/// The file name of the artifact index, beside the maintenance executable.
pub const MAINTENANCE_INDEX_NAME: &str = "artifact.json";

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

/// Which application, release, and machine a store belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentStoreIdentity {
    app_id: AppId,
    version: Version,
    scope: SelectedScope,
    target: TargetTriple,
    /// The digest of the artifact the content came from, which is what makes two
    /// stores for the same version and target distinguishable.
    artifact: Sha256Digest,
}

impl ContentStoreIdentity {
    /// Create an identity, refusing an empty or unparsable one.
    pub fn new(
        app_id: AppId,
        version: Version,
        scope: SelectedScope,
        target: TargetTriple,
        artifact: Sha256Digest,
    ) -> Self {
        Self {
            app_id,
            version,
            scope,
            target,
            artifact,
        }
    }

    /// The digest this identity is stored under.
    pub fn digest(&self) -> Sha256Digest {
        let mut hasher = Sha256::new();
        field(&mut hasher, IDENTITY_DOMAIN);
        field(&mut hasher, self.app_id.as_str().as_bytes());
        field(&mut hasher, self.version.to_string().as_bytes());
        field(&mut hasher, self.scope.to_string().as_bytes());
        field(&mut hasher, self.target.as_str().as_bytes());
        field(&mut hasher, self.artifact.as_bytes());
        Sha256Digest::from_hasher(hasher)
    }

    /// The store directory for this identity under `base_root`.
    pub fn path_under(&self, base_root: &Path) -> PathBuf {
        let mut app_hasher = Sha256::new();
        field(&mut app_hasher, APP_DOMAIN);
        field(&mut app_hasher, self.app_id.as_str().as_bytes());
        let app = Sha256Digest::from_hasher(app_hasher).to_hex();
        base_root
            .join(CONTENT_STORE_DIRECTORY)
            .join(app)
            .join(self.scope.to_string())
            .join(self.digest().to_hex())
    }

    pub fn target(&self) -> &TargetTriple {
        &self.target
    }

    pub fn scope(&self) -> SelectedScope {
        self.scope
    }

    pub fn version(&self) -> &Version {
        &self.version
    }

    pub fn app_id(&self) -> &AppId {
        &self.app_id
    }
}

fn field(hasher: &mut Sha256, value: &[u8]) {
    hasher.update(u64::try_from(value.len()).unwrap_or(u64::MAX).to_le_bytes());
    hasher.update(value);
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

/// The maintenance directory an installed copy lives in, which is where a
/// store's contents end up after the transaction commits.
pub fn maintenance_directory(
    state_root: &Path,
    app_id: &AppId,
    scope: SelectedScope,
    version: &Version,
) -> PathBuf {
    let scope_name = match scope {
        SelectedScope::User => "user",
        SelectedScope::Machine => "machine",
    };
    state_root
        .join("maintenance")
        .join(app_id.as_str())
        .join(scope_name)
        .join(version.to_string())
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
pub fn remove_store(base_root: &Path, store: &Path) -> Result<(), ContentStoreError> {
    let namespace = base_root.join(CONTENT_STORE_DIRECTORY);
    let relative = store
        .strip_prefix(&namespace)
        .map_err(|_| ContentStoreError::UnsafePath {
            path: store.display().to_string(),
            reason: "a store is outside the content store namespace".into(),
        })?;
    let mut parts = relative.components();
    let (
        Some(Component::Normal(app)),
        Some(Component::Normal(scope)),
        Some(Component::Normal(identity)),
        None,
    ) = (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(ContentStoreError::UnsafePath {
            path: store.display().to_string(),
            reason: "a content store directory has an unexpected shape".into(),
        });
    };
    let (Some(app), Some(scope), Some(identity)) =
        (app.to_str(), scope.to_str(), identity.to_str())
    else {
        return Err(ContentStoreError::UnsafePath {
            path: store.display().to_string(),
            reason: "a content store directory identity is not text".into(),
        });
    };
    let hex = |value: &str| value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit());
    if !hex(app) || !hex(identity) {
        return Err(ContentStoreError::UnsafePath {
            path: store.display().to_string(),
            reason: "a content store directory identity is not a digest".into(),
        });
    }
    if !matches!(scope, "user" | "machine") {
        return Err(ContentStoreError::UnsafePath {
            path: store.display().to_string(),
            reason: "a content store scope is not one this build knows".into(),
        });
    }
    match std::fs::symlink_metadata(store) {
        Ok(metadata) if !is_reparse_point(store, &metadata) && metadata.is_dir() => {
            std::fs::remove_dir_all(store).map_err(|source| io_error(store, source))?
        }
        Ok(_) => {
            return Err(ContentStoreError::UnsafePath {
                path: store.display().to_string(),
                reason: "a content store is a reparse point or special file".into(),
            });
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

    fn identity() -> ContentStoreIdentity {
        ContentStoreIdentity::new(
            AppId::new("com.acme.desktop").unwrap(),
            Version::parse("1.4.0").unwrap(),
            SelectedScope::User,
            TargetTriple::parse("x86_64-pc-windows-msvc").unwrap(),
            Sha256Digest::from_bytes([7; 32]),
        )
    }

    #[test]
    fn an_identity_depends_on_every_domain_field() {
        let base = identity();
        let other_target = ContentStoreIdentity::new(
            base.app_id().clone(),
            base.version().clone(),
            base.scope(),
            TargetTriple::parse("aarch64-pc-windows-msvc").unwrap(),
            Sha256Digest::from_bytes([7; 32]),
        );
        let other_artifact = ContentStoreIdentity::new(
            base.app_id().clone(),
            base.version().clone(),
            base.scope(),
            base.target().clone(),
            Sha256Digest::from_bytes([8; 32]),
        );
        assert_ne!(base.digest(), other_target.digest());
        assert_ne!(base.digest(), other_artifact.digest());
    }

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
