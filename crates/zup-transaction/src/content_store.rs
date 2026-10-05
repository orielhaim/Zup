//! Which application, release and machine a content store belongs to.
//!
//! A selected variant's content does not have to live inside the executable that
//! runs it. It may live in a single-target artifact's own resources, in a
//! universal artifact, or in a store the installation persisted. This module owns
//! the third case's *identity and layout*: a versioned, per-scope, per-variant
//! directory holding the selected variant's native runtime, its content package,
//! and the artifact index that describes both.
//!
//! One property is enforced here rather than trusted: **a store belongs to one
//! application, one scope, one target, and one artifact.** The directory name is a
//! digest over exactly those, so a store for an ARM64 variant can never be
//! mistaken for an x64 one, and a store from a different release is a different
//! directory.
//!
//! What this deliberately does *not* own is where a store's base root sits, and
//! whether a directory on the way is safe to write through. Those are the
//! backend's: the base depends on what authority the writing process has, and the
//! path validation depends on what the host filesystem considers an indirection. A
//! store that answered both would be a store that could only be correct on one
//! platform.

use std::path::{Path, PathBuf};

use semver::Version;
use sha2::{Digest, Sha256};
use zup_core::{AppId, SelectedScope, Sha256Digest, TargetTriple};

/// The directory a store lives in, under its base root.
pub const CONTENT_STORE_DIRECTORY: &str = ".zup-content";

const IDENTITY_DOMAIN: &[u8] = b"zup/content-store/identity/v1\0";
const APP_DOMAIN: &[u8] = b"zup/content-store/app/v1\0";

/// Which application, release, and machine a store belongs to.
///
/// A value rather than four arguments threaded through a path builder, because the
/// four are one identity: a digest over three of them is a store for the wrong
/// machine wearing the right application's name, which is exactly the confusion
/// this type makes unrepresentable.
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
    /// Create an identity.
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
    ///
    /// `<base>/.zup-content/<app>/<scope>/<identity>` - app below scope so that a
    /// question about one application spans its scopes, and identity last because
    /// it is the part that makes the directory unique.
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

/// Length-prefix a value into the identity digest.
///
/// Without it, moving a byte from one field to the next would produce the same
/// digest - `("ab", "c")` and `("a", "bc")` would be indistinguishable, and two
/// different installations would share a store.
fn field(hasher: &mut Sha256, value: &[u8]) {
    hasher.update(u64::try_from(value.len()).unwrap_or(u64::MAX).to_le_bytes());
    hasher.update(value);
}

/// Whether `store` has the shape a store directory is written with.
///
/// Split out of the backend's removal path so the *shape* is one fact about the
/// layout rather than something each platform re-derives. It checks spelling only:
/// whether a directory is safe to delete is the backend's question, because
/// "safe" means "not an indirection", and only the host can answer that.
pub fn is_store_shape(store: &Path) -> bool {
    let Ok(relative) = store.strip_prefix(store.parent().unwrap_or(Path::new("."))) else {
        return false;
    };
    let mut components = relative.components();
    let digest = |value: Option<&std::ffi::OsStr>| {
        value.is_some_and(|value| {
            value.to_str().is_some_and(|value| {
                value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
        })
    };
    let scope = components
        .next()
        .and_then(|component| component.as_os_str().to_str());
    matches!(
        (
            digest(components.next().map(|c| c.as_os_str())),
            scope,
            components.next()
        ),
        (true, Some("user" | "machine"), None)
    )
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

    /// The whole point of the identity: a store for one machine is not the store
    /// for another, and a store from a different artifact is not the same content.
    /// The four fields are exactly the four that differ between two installations
    /// that must not be confused.
    #[test]
    fn an_identity_depends_on_every_field() {
        let base = identity();
        for other in [
            ContentStoreIdentity::new(
                base.app_id().clone(),
                base.version().clone(),
                base.scope(),
                TargetTriple::parse("aarch64-pc-windows-msvc").unwrap(),
                Sha256Digest::from_bytes([7; 32]),
            ),
            ContentStoreIdentity::new(
                base.app_id().clone(),
                base.version().clone(),
                base.scope(),
                base.target().clone(),
                Sha256Digest::from_bytes([8; 32]),
            ),
            ContentStoreIdentity::new(
                base.app_id().clone(),
                base.version().clone(),
                SelectedScope::Machine,
                base.target().clone(),
                Sha256Digest::from_bytes([7; 32]),
            ),
            ContentStoreIdentity::new(
                base.app_id().clone(),
                Version::parse("1.5.0").unwrap(),
                base.scope(),
                base.target().clone(),
                Sha256Digest::from_bytes([7; 32]),
            ),
            ContentStoreIdentity::new(
                AppId::new("com.acme.other").unwrap(),
                base.version().clone(),
                base.scope(),
                base.target().clone(),
                Sha256Digest::from_bytes([7; 32]),
            ),
        ] {
            assert_ne!(base.digest(), other.digest());
        }
    }

    /// Fields are length-prefixed, so two identities whose fields concatenate to
    /// the same bytes are still different identities. Without the prefix a byte
    /// moving from the end of one field to the front of the next would leave the
    /// digest unchanged, and two different applications would share one store.
    #[test]
    fn fields_are_length_prefixed_so_a_shift_cannot_collide() {
        // The same 27 bytes, split one segment earlier: `com.acme.desktop` split
        // as `com.acme.des` + `ktop` is not expressible through `AppId`, so the
        // shift is made on the boundary `AppId` does allow - an id whose *text*
        // contains what the other one's ends with.
        let base = identity();
        let absorbed = ContentStoreIdentity::new(
            AppId::new("com.acme.desktopx").unwrap(),
            base.version().clone(),
            base.scope(),
            base.target().clone(),
            Sha256Digest::from_bytes([7; 32]),
        );
        assert_ne!(base.digest(), absorbed.digest());

        // And the property itself, on the digest function, where the shift is
        // exact: the same bytes in two fields hash differently from the same bytes
        // in one.
        let mut joined = Sha256::new();
        field(&mut joined, b"abcd");
        let mut split = Sha256::new();
        field(&mut split, b"ab");
        field(&mut split, b"cd");
        assert_ne!(
            Sha256Digest::from_hasher(joined),
            Sha256Digest::from_hasher(split),
            "an unprefixed field encoding would make these two equal"
        );
    }

    /// The directory spells the scope, and the scope is the one name a reader
    /// needs in order to know *whose* store this is.
    #[test]
    fn the_store_path_nests_app_then_scope_then_identity() {
        let path = identity().path_under(Path::new("/base"));
        let relative = path.strip_prefix("/base").expect("under the base");
        let segments = relative
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(segments.len(), 4, "{segments:?}");
        assert_eq!(segments[0], CONTENT_STORE_DIRECTORY);
        assert_eq!(segments[1].len(), 64, "the application is named by digest");
        assert_eq!(segments[2], "user");
        assert_eq!(segments[3], identity().digest().to_hex());
    }
}
