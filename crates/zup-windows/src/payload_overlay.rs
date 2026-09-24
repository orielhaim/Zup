use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};

use semver::Version;
use sha2::{Digest, Sha256};
use thiserror::Error;
use zup_core::{
    AppId, ComponentId, PLUGIN_PAYLOAD_ROOT, RelativePath, SelectedScope, Sha256Digest, hash_reader,
};
use zup_exec::ExecutionPlan;
use zup_plan::{GeneratedFile, InstallPlan};
use zup_transaction::{NodeKind, TransactionPlan};

use crate::durable::{DurableError, write_durable};
use crate::transport::UserSid;

pub const PAYLOAD_OVERLAY_DIRECTORY: &str = ".zup-payload-overlays";
const OVERLAY_IDENTITY_DOMAIN: &[u8] = b"zup/payload-overlay/identity/v1\0";
const OVERLAY_APP_DOMAIN: &[u8] = b"zup/payload-overlay/app/v1\0";
const MACHINE_OVERLAY_BASE_DIRECTORY: &str = "zup-payload-overlays";

pub fn payload_overlay_base_root(
    state_root: &Path,
    scope: SelectedScope,
) -> Result<PathBuf, PayloadOverlayError> {
    match scope {
        SelectedScope::User => Ok(state_root.to_path_buf()),
        SelectedScope::Machine => UserSid::current()
            .map(|sid| std::env::temp_dir().join(machine_overlay_base_name(sid.display())))
            .map_err(|error| PayloadOverlayError::BaseUnavailable(error.to_string())),
    }
}

fn machine_overlay_base_name(sid: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(sid.as_bytes());
    let user_key = Sha256Digest::from_hasher(hasher).to_hex();
    format!("{MACHINE_OVERLAY_BASE_DIRECTORY}-{}", &user_key[..32])
}

pub fn validate_payload_overlay_base(
    state_root: &Path,
    scope: SelectedScope,
    base: &Path,
    expected_parent_sid: &str,
) -> Result<(), PayloadOverlayError> {
    match scope {
        SelectedScope::User if base == state_root => Ok(()),
        SelectedScope::User => Err(PayloadOverlayError::InvalidIdentity(
            "user payload overlay base must equal state root".into(),
        )),
        SelectedScope::Machine => {
            if !base.is_absolute()
                || base
                    .components()
                    .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
            {
                return Err(PayloadOverlayError::InvalidIdentity(
                    "machine payload overlay base must be an absolute normalized path".into(),
                ));
            }
            let expected_name = machine_overlay_base_name(expected_parent_sid);
            if base.file_name().and_then(|name| name.to_str()) != Some(expected_name.as_str()) {
                return Err(PayloadOverlayError::InvalidIdentity(
                    "machine payload overlay base does not match the authenticated parent SID"
                        .into(),
                ));
            }
            Ok(())
        }
    }
}

#[derive(Debug, Error)]
pub enum PayloadOverlayError {
    #[error("payload overlay identity is invalid: {0}")]
    InvalidIdentity(String),

    #[error("payload overlay base root is unavailable: {0}")]
    BaseUnavailable(String),

    #[error("payload overlay contains duplicate source path `{path}`")]
    DuplicateSource { path: String },

    #[error("generated payload `{path}` is not present in the install plan")]
    UnexpectedSource { path: String },

    #[error("payload overlay path `{path}` is unsafe: {reason}")]
    UnsafePath { path: String, reason: String },

    #[error("payload overlay file `{path}` is not a regular file")]
    NotRegular { path: String },

    #[error("payload overlay file `{path}` is missing")]
    Missing { path: String },

    #[error("payload overlay file `{path}` size mismatch: expected {expected}, found {found}")]
    SizeMismatch {
        path: String,
        expected: u64,
        found: u64,
    },

    #[error("payload overlay file `{path}` digest mismatch")]
    DigestMismatch { path: String },

    #[error("payload overlay I/O at `{path}`: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error(transparent)]
    Durable(#[from] DurableError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PayloadOverlayFileIdentity {
    pub source_relative: RelativePath,
    pub size: u64,
    pub sha256: Sha256Digest,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PayloadOverlayIdentity {
    app_id: AppId,
    app_version: Version,
    scope: SelectedScope,
    selected_components: Vec<ComponentId>,
    files: BTreeMap<RelativePath, PayloadOverlayFileIdentity>,
}

impl PayloadOverlayIdentity {
    pub fn new(
        app_id: AppId,
        app_version: Version,
        scope: SelectedScope,
        selected_components: impl IntoIterator<Item = ComponentId>,
        files: impl IntoIterator<Item = PayloadOverlayFileIdentity>,
    ) -> Result<Self, PayloadOverlayError> {
        Self::from_parts(
            app_id,
            app_version,
            scope,
            selected_components.into_iter().collect(),
            files,
            true,
        )
    }

    pub fn from_install_plan(plan: &InstallPlan) -> Result<Self, PayloadOverlayError> {
        Self::from_parts(
            plan.app.id.clone(),
            plan.app.version.clone(),
            plan.scope,
            plan.selected_components.clone(),
            plan.files
                .iter()
                .filter(|file| is_plugin_payload_path(&file.source_relative))
                .map(|file| PayloadOverlayFileIdentity {
                    source_relative: file.source_relative.clone(),
                    size: file.size,
                    sha256: file.sha256,
                }),
            true,
        )
    }

    pub fn from_execution_plan(
        app_id: AppId,
        app_version: Version,
        scope: SelectedScope,
        plan: &ExecutionPlan,
    ) -> Result<Self, PayloadOverlayError> {
        Self::from_parts(
            app_id,
            app_version,
            scope,
            plan.selected_components.clone(),
            plan.files
                .iter()
                .filter(|file| is_plugin_payload_path(&file.source_relative))
                .map(|file| PayloadOverlayFileIdentity {
                    source_relative: file.source_relative.clone(),
                    size: file.expected_size,
                    sha256: file.expected_sha256,
                }),
            true,
        )
    }

    pub fn from_transaction(
        app_id: AppId,
        app_version: Version,
        scope: SelectedScope,
        plan: &TransactionPlan,
    ) -> Result<Self, PayloadOverlayError> {
        let files = plan
            .nodes
            .iter()
            .filter_map(|node| {
                if !matches!(
                    node.kind,
                    NodeKind::StageFile { .. } | NodeKind::FileMutation { .. }
                ) {
                    return None;
                }
                let source_relative = node.meta.source_relative.as_ref()?;
                if !is_plugin_payload_path(source_relative) {
                    return None;
                }
                Some((|| {
                    let size = node.meta.expected_size.ok_or_else(|| {
                        PayloadOverlayError::InvalidIdentity(format!(
                            "generated source `{source_relative}` has no expected size"
                        ))
                    })?;
                    let sha256 = node.meta.expected_sha256.ok_or_else(|| {
                        PayloadOverlayError::InvalidIdentity(format!(
                            "generated source `{source_relative}` has no expected digest"
                        ))
                    })?;
                    Ok(PayloadOverlayFileIdentity {
                        source_relative: source_relative.clone(),
                        size,
                        sha256,
                    })
                })())
            })
            .collect::<Result<Vec<_>, PayloadOverlayError>>()?;
        Self::from_parts(
            app_id,
            app_version,
            scope,
            plan.selected_components.clone(),
            files,
            false,
        )
    }

    fn from_parts(
        app_id: AppId,
        app_version: Version,
        scope: SelectedScope,
        selected_components: Vec<ComponentId>,
        files: impl IntoIterator<Item = PayloadOverlayFileIdentity>,
        reject_duplicates: bool,
    ) -> Result<Self, PayloadOverlayError> {
        let mut selected_components = selected_components;
        selected_components.sort();
        if selected_components
            .windows(2)
            .any(|pair| pair[0] == pair[1])
        {
            return Err(PayloadOverlayError::InvalidIdentity(
                "selected components are duplicated".into(),
            ));
        }
        let mut identity = BTreeMap::new();
        for file in files {
            insert_file(&mut identity, file, reject_duplicates)?;
        }
        Ok(Self {
            app_id,
            app_version,
            scope,
            selected_components,
            files: identity,
        })
    }

    pub fn files(&self) -> impl ExactSizeIterator<Item = &PayloadOverlayFileIdentity> {
        self.files.values()
    }

    pub fn has_files(&self) -> bool {
        !self.files.is_empty()
    }

    pub fn digest(&self) -> Sha256Digest {
        let mut hasher = Sha256::new();
        hash_field(&mut hasher, OVERLAY_IDENTITY_DOMAIN);
        hash_field(&mut hasher, self.app_id.as_str().as_bytes());
        hash_field(&mut hasher, self.app_version.to_string().as_bytes());
        hash_field(&mut hasher, self.scope.to_string().as_bytes());
        for component in &self.selected_components {
            hash_field(&mut hasher, component.as_str().as_bytes());
        }
        for file in self.files.values() {
            hash_field(&mut hasher, file.source_relative.as_str().as_bytes());
            hash_field(&mut hasher, &file.size.to_le_bytes());
            hash_field(&mut hasher, file.sha256.as_bytes());
        }
        Sha256Digest::from_hasher(hasher)
    }

    pub fn path_under(&self, base_root: &Path) -> Option<PathBuf> {
        if !self.has_files() {
            return None;
        }
        let mut app_hasher = Sha256::new();
        hash_field(&mut app_hasher, OVERLAY_APP_DOMAIN);
        hash_field(&mut app_hasher, self.app_id.as_str().as_bytes());
        let app = Sha256Digest::from_hasher(app_hasher).to_hex();
        Some(
            base_root
                .join(PAYLOAD_OVERLAY_DIRECTORY)
                .join(app)
                .join(self.scope.to_string())
                .join(self.digest().to_hex()),
        )
    }
}

pub fn materialize_payload_overlay(
    overlay_base_root: &Path,
    plan: &InstallPlan,
    generated_files: &[GeneratedFile],
) -> Result<Option<PathBuf>, PayloadOverlayError> {
    let identity = PayloadOverlayIdentity::from_install_plan(plan)?;
    let Some(overlay_root) = identity.path_under(overlay_base_root) else {
        if generated_files.is_empty() {
            return Ok(None);
        }
        return Err(PayloadOverlayError::InvalidIdentity(
            "generated files exist without reserved install-plan sources".into(),
        ));
    };

    let mut generated = BTreeMap::new();
    for file in generated_files {
        if !is_plugin_payload_path(&file.source_relative) {
            return Err(PayloadOverlayError::UnsafePath {
                path: file.source_relative.to_string(),
                reason: "generated source is outside the reserved payload namespace".into(),
            });
        }
        let path = file.source_relative.to_string();
        if generated.contains_key(&file.source_relative) {
            return Err(PayloadOverlayError::DuplicateSource { path });
        }
        let size =
            u64::try_from(file.bytes.len()).map_err(|_| PayloadOverlayError::SizeMismatch {
                path: path.clone(),
                expected: file.size,
                found: u64::MAX,
            })?;
        if size != file.size {
            return Err(PayloadOverlayError::SizeMismatch {
                path,
                expected: file.size,
                found: size,
            });
        }
        let sha256 = Sha256Digest::from_bytes(Sha256::digest(&file.bytes).into());
        if sha256 != file.sha256 {
            return Err(PayloadOverlayError::DigestMismatch { path });
        }
        let expected = identity.files.get(&file.source_relative).ok_or_else(|| {
            PayloadOverlayError::UnexpectedSource {
                path: file.source_relative.to_string(),
            }
        })?;
        if expected.size != file.size || expected.sha256 != file.sha256 {
            return Err(PayloadOverlayError::InvalidIdentity(format!(
                "generated payload `{}` does not match its install-plan file",
                file.source_relative
            )));
        }
        generated.insert(file.source_relative.clone(), file.bytes.as_slice());
    }
    if generated.len() != identity.files.len() {
        let missing = identity
            .files
            .keys()
            .find(|source| !generated.contains_key(*source))
            .cloned()
            .ok_or_else(|| {
                PayloadOverlayError::InvalidIdentity(
                    "generated payload count does not match the install plan".into(),
                )
            })?;
        return Err(PayloadOverlayError::Missing {
            path: missing.to_string(),
        });
    }

    ensure_overlay_directories(overlay_base_root, &identity)?;
    for (source, expected) in &identity.files {
        let target = resolve_overlay_path(&overlay_root, source)?;
        ensure_overlay_parents(&overlay_root, source)?;
        verify_target_is_safe(&overlay_root, &target, source)?;
        write_durable(&target, generated[source])?;
        verify_regular_file(&target, source, expected)?;
    }
    verify_payload_overlay(overlay_base_root, &identity, &overlay_root)?;
    Ok(Some(overlay_root))
}

pub fn verify_payload_overlay(
    overlay_base_root: &Path,
    identity: &PayloadOverlayIdentity,
    overlay_root: &Path,
) -> Result<(), PayloadOverlayError> {
    let expected = identity.path_under(overlay_base_root).ok_or_else(|| {
        PayloadOverlayError::InvalidIdentity("overlay identity has no generated files".into())
    })?;
    if overlay_root != expected {
        return Err(PayloadOverlayError::InvalidIdentity(format!(
            "expected overlay `{}`, found `{}`",
            expected.display(),
            overlay_root.display()
        )));
    }
    verify_existing_directory_chain(overlay_base_root)?;
    verify_directory_chain(overlay_base_root, overlay_root)?;
    let metadata =
        fs::symlink_metadata(overlay_root).map_err(|error| io_error(overlay_root, error))?;
    if is_reparse_point(overlay_root, &metadata) || !metadata.is_dir() {
        return Err(PayloadOverlayError::UnsafePath {
            path: overlay_root.display().to_string(),
            reason: "overlay root is not a regular directory".into(),
        });
    }
    for (source, expected) in &identity.files {
        let target = resolve_overlay_path(overlay_root, source)?;
        verify_target_is_safe(overlay_root, &target, source)?;
        verify_regular_file(&target, source, expected)?;
    }
    Ok(())
}

pub fn cleanup_payload_overlay(
    overlay_base_root: &Path,
    overlay_root: Option<&Path>,
) -> Result<(), PayloadOverlayError> {
    let Some(overlay_root) = overlay_root else {
        return Ok(());
    };
    verify_existing_directory_chain(overlay_base_root)?;
    let namespace = overlay_base_root.join(PAYLOAD_OVERLAY_DIRECTORY);
    let relative =
        overlay_root
            .strip_prefix(&namespace)
            .map_err(|_| PayloadOverlayError::UnsafePath {
                path: overlay_root.display().to_string(),
                reason: "overlay is outside the private state directory".into(),
            })?;
    let mut parts = relative.components();
    let (
        Some(Component::Normal(app)),
        Some(Component::Normal(scope)),
        Some(Component::Normal(identity)),
        None,
    ) = (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(PayloadOverlayError::UnsafePath {
            path: overlay_root.display().to_string(),
            reason: "overlay directory shape is invalid".into(),
        });
    };
    let (Some(app), Some(scope), Some(identity)) =
        (app.to_str(), scope.to_str(), identity.to_str())
    else {
        return Err(PayloadOverlayError::UnsafePath {
            path: overlay_root.display().to_string(),
            reason: "overlay directory identity is invalid UTF-8".into(),
        });
    };
    if app.len() != 64
        || scope != SelectedScope::User.to_string() && scope != SelectedScope::Machine.to_string()
        || identity.len() != 64
        || !app.bytes().all(|byte| byte.is_ascii_hexdigit())
        || !identity.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(PayloadOverlayError::UnsafePath {
            path: overlay_root.display().to_string(),
            reason: "overlay directory identity is invalid".into(),
        });
    }
    remove_directory_if_present(overlay_root)?;
    let scope_root = overlay_root
        .parent()
        .ok_or_else(|| PayloadOverlayError::UnsafePath {
            path: overlay_root.display().to_string(),
            reason: "overlay has no scope directory".into(),
        })?;
    let app_root = scope_root
        .parent()
        .ok_or_else(|| PayloadOverlayError::UnsafePath {
            path: scope_root.display().to_string(),
            reason: "overlay has no app directory".into(),
        })?;
    remove_directory_if_empty(scope_root)?;
    remove_directory_if_empty(app_root)?;
    remove_directory_if_empty(&namespace)?;
    Ok(())
}

pub fn cleanup_app_payload_overlays(
    state_root: &Path,
    app_id: &AppId,
    scope: SelectedScope,
) -> Result<(), PayloadOverlayError> {
    let overlay_base_root = payload_overlay_base_root(state_root, scope)?;
    let mut app_hasher = Sha256::new();
    hash_field(&mut app_hasher, OVERLAY_APP_DOMAIN);
    hash_field(&mut app_hasher, app_id.as_str().as_bytes());
    let app = Sha256Digest::from_hasher(app_hasher).to_hex();
    let namespace = overlay_base_root.join(PAYLOAD_OVERLAY_DIRECTORY);
    let scope_root = namespace.join(app).join(scope.to_string());
    remove_directory_if_present(&scope_root)?;
    remove_directory_if_empty(scope_root.parent().expect("scope has app parent"))?;
    remove_directory_if_empty(&namespace)?;
    Ok(())
}

pub fn is_plugin_payload_path(path: &RelativePath) -> bool {
    path.components()
        .next()
        .is_some_and(|component| component.eq_ignore_ascii_case(PLUGIN_PAYLOAD_ROOT))
}

fn insert_file(
    files: &mut BTreeMap<RelativePath, PayloadOverlayFileIdentity>,
    file: PayloadOverlayFileIdentity,
    reject_duplicates: bool,
) -> Result<(), PayloadOverlayError> {
    if !is_plugin_payload_path(&file.source_relative) {
        return Err(PayloadOverlayError::UnsafePath {
            path: file.source_relative.to_string(),
            reason: "source is outside the reserved payload namespace".into(),
        });
    }
    if files.contains_key(&file.source_relative) {
        if reject_duplicates {
            return Err(PayloadOverlayError::DuplicateSource {
                path: file.source_relative.to_string(),
            });
        }
        if files[&file.source_relative] != file {
            return Err(PayloadOverlayError::InvalidIdentity(format!(
                "source `{}` has conflicting identities",
                file.source_relative
            )));
        }
    } else {
        files.insert(file.source_relative.clone(), file);
    }
    Ok(())
}

fn hash_field(hasher: &mut Sha256, value: &[u8]) {
    hasher.update(u64::try_from(value.len()).unwrap_or(u64::MAX).to_le_bytes());
    hasher.update(value);
}

fn is_reparse_point(path: &Path, metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        metadata.file_type().is_symlink() || crate::fs_bindings::is_reparse_point(path)
    }
    #[cfg(not(windows))]
    {
        let _ = metadata;
        metadata.file_type().is_symlink()
    }
}

fn ensure_overlay_directories(
    overlay_base_root: &Path,
    identity: &PayloadOverlayIdentity,
) -> Result<(), PayloadOverlayError> {
    ensure_directory_chain(overlay_base_root)?;
    let namespace = overlay_base_root.join(PAYLOAD_OVERLAY_DIRECTORY);
    ensure_directory(&namespace)?;
    let overlay_root = identity
        .path_under(overlay_base_root)
        .expect("overlay identity has files");
    let scope_root = overlay_root
        .parent()
        .expect("overlay has scope parent")
        .to_path_buf();
    let app_root = scope_root
        .parent()
        .expect("scope has app parent")
        .to_path_buf();
    ensure_directory(&app_root)?;
    ensure_directory(&scope_root)?;
    ensure_directory(&overlay_root)
}

fn ensure_directory_chain(path: &Path) -> Result<(), PayloadOverlayError> {
    let ancestors = path.ancestors().collect::<Vec<_>>();
    for ancestor in ancestors.into_iter().rev() {
        ensure_directory(ancestor)?;
    }
    Ok(())
}

fn verify_existing_directory_chain(path: &Path) -> Result<(), PayloadOverlayError> {
    for ancestor in path.ancestors() {
        match fs::symlink_metadata(ancestor) {
            Ok(_) => verify_directory(ancestor)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => return Err(io_error(ancestor, error)),
        }
    }
    Ok(())
}

fn verify_directory_chain(base: &Path, path: &Path) -> Result<(), PayloadOverlayError> {
    let relative = path
        .strip_prefix(base)
        .map_err(|_| PayloadOverlayError::UnsafePath {
            path: path.display().to_string(),
            reason: "directory escapes the overlay base".into(),
        })?;
    let mut current = base.to_path_buf();
    verify_directory(&current)?;
    for component in relative.components() {
        current.push(component);
        verify_directory(&current)?;
    }
    Ok(())
}

fn verify_directory(path: &Path) -> Result<(), PayloadOverlayError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| io_error(path, error))?;
    if is_reparse_point(path, &metadata) || !metadata.is_dir() {
        return Err(PayloadOverlayError::UnsafePath {
            path: path.display().to_string(),
            reason: "directory is a reparse point or special file".into(),
        });
    }
    Ok(())
}

fn ensure_directory(path: &Path) -> Result<(), PayloadOverlayError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if is_reparse_point(path, &metadata) || !metadata.is_dir() => {
            Err(PayloadOverlayError::UnsafePath {
                path: path.display().to_string(),
                reason: "directory target is a reparse point or special file".into(),
            })
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => match fs::create_dir(path) {
            Ok(()) => ensure_directory(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                ensure_directory(path)
            }
            Err(error) => Err(io_error(path, error)),
        },
        Err(error) => Err(io_error(path, error)),
    }
}

fn ensure_overlay_parents(
    overlay_root: &Path,
    source: &RelativePath,
) -> Result<(), PayloadOverlayError> {
    let mut current = overlay_root.to_path_buf();
    for component in source
        .components()
        .take(source.component_count().saturating_sub(1))
    {
        current.push(component);
        ensure_directory(&current)?;
    }
    Ok(())
}

fn resolve_overlay_path(
    overlay_root: &Path,
    source: &RelativePath,
) -> Result<PathBuf, PayloadOverlayError> {
    let mut target = overlay_root.to_path_buf();
    for component in source.components() {
        let mut segments = Path::new(component).components();
        if component.is_empty()
            || component.contains('/')
            || component.contains('\\')
            || component.contains(':')
            || !matches!(segments.next(), Some(Component::Normal(_)))
            || segments.next().is_some()
        {
            return Err(PayloadOverlayError::UnsafePath {
                path: source.to_string(),
                reason: "source contains a non-portable path component".into(),
            });
        }
        target.push(component);
    }
    if !target.starts_with(overlay_root) {
        return Err(PayloadOverlayError::UnsafePath {
            path: source.to_string(),
            reason: "source escapes the overlay root".into(),
        });
    }
    Ok(target)
}

fn verify_target_is_safe(
    overlay_root: &Path,
    target: &Path,
    source: &RelativePath,
) -> Result<(), PayloadOverlayError> {
    let relative =
        target
            .strip_prefix(overlay_root)
            .map_err(|_| PayloadOverlayError::UnsafePath {
                path: source.to_string(),
                reason: "target escapes the overlay root".into(),
            })?;
    let mut current = overlay_root.to_path_buf();
    let component_count = relative.components().count();
    for component in relative
        .components()
        .take(component_count.saturating_sub(1))
    {
        current.push(component);
        let metadata = fs::symlink_metadata(&current).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                PayloadOverlayError::Missing {
                    path: source.to_string(),
                }
            } else {
                io_error(&current, error)
            }
        })?;
        if is_reparse_point(&current, &metadata) || !metadata.is_dir() {
            return Err(PayloadOverlayError::UnsafePath {
                path: current.display().to_string(),
                reason: "overlay parent is a reparse point or special file".into(),
            });
        }
    }
    match fs::symlink_metadata(target) {
        Ok(metadata) if is_reparse_point(target, &metadata) || !metadata.is_file() => {
            Err(PayloadOverlayError::NotRegular {
                path: source.to_string(),
            })
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io_error(target, error)),
    }
}

fn verify_regular_file(
    target: &Path,
    source: &RelativePath,
    expected: &PayloadOverlayFileIdentity,
) -> Result<(), PayloadOverlayError> {
    let metadata = fs::symlink_metadata(target).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            PayloadOverlayError::Missing {
                path: source.to_string(),
            }
        } else {
            io_error(target, error)
        }
    })?;
    if is_reparse_point(target, &metadata) || !metadata.is_file() {
        return Err(PayloadOverlayError::NotRegular {
            path: source.to_string(),
        });
    }
    let (size, sha256) =
        hash_reader(fs::File::open(target).map_err(|error| io_error(target, error))?)
            .map_err(|error| io_error(target, error))?;
    if size != expected.size {
        return Err(PayloadOverlayError::SizeMismatch {
            path: source.to_string(),
            expected: expected.size,
            found: size,
        });
    }
    if sha256 != expected.sha256 {
        return Err(PayloadOverlayError::DigestMismatch {
            path: source.to_string(),
        });
    }
    Ok(())
}

fn remove_directory_if_present(path: &Path) -> Result<(), PayloadOverlayError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if is_reparse_point(path, &metadata) || !metadata.is_dir() => {
            Err(PayloadOverlayError::UnsafePath {
                path: path.display().to_string(),
                reason: "cleanup target is a reparse point or special file".into(),
            })
        }
        Ok(_) => fs::remove_dir_all(path).map_err(|error| io_error(path, error)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io_error(path, error)),
    }
}

fn remove_directory_if_empty(path: &Path) -> Result<(), PayloadOverlayError> {
    match fs::remove_dir(path) {
        Ok(()) => Ok(()),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty
            ) =>
        {
            Ok(())
        }
        Err(error) => Err(io_error(path, error)),
    }
}

fn io_error(path: &Path, source: std::io::Error) -> PayloadOverlayError {
    PayloadOverlayError::Io {
        path: path.display().to_string(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zup_core::{App, NonEmptyString, ResourceKey, Template};
    use zup_exec::{ExecutionSummary, FileOperation, FileOperationKind, FilePrecondition};
    use zup_plan::{PlanSummary, PlannedFile};
    use zup_platform::TargetPath;

    fn generated_source() -> RelativePath {
        RelativePath::new("__zup_plugins__/generated.bin").unwrap()
    }

    fn generated_file() -> GeneratedFile {
        let bytes = b"generated".to_vec();
        GeneratedFile {
            source_relative: generated_source(),
            destination: Template::parse("C:/target/generated.bin").unwrap(),
            size: bytes.len() as u64,
            sha256: Sha256Digest::from_bytes(Sha256::digest(&bytes).into()),
            bytes,
        }
    }

    fn install_plan() -> InstallPlan {
        let generated = generated_file();
        InstallPlan {
            app: App {
                id: AppId::new("com.example.overlay").unwrap(),
                name: NonEmptyString::new("Overlay").unwrap(),
                version: Version::parse("1.2.3").unwrap(),
                publisher: None,
                main: None,
                description: None,
            },
            scope: SelectedScope::User,
            install_directory: Template::parse("${known.local_app_data}/Overlay").unwrap(),
            selected_components: vec![ComponentId::new("core").unwrap()],
            files: vec![PlannedFile {
                key: ResourceKey::File {
                    destination: generated.destination.to_string(),
                },
                source_relative: generated.source_relative,
                destination: generated.destination,
                size: generated.size,
                sha256: generated.sha256,
                privilege: zup_core::Privilege::User,
            }],
            shortcuts: Vec::new(),
            path_entries: Vec::new(),
            services: Vec::new(),
            protocols: Vec::new(),
            file_types: Vec::new(),
            summary: PlanSummary {
                file_count: 1,
                install_bytes: generated.size,
                selected_component_count: 1,
                resource_count: 0,
                requires_elevation: false,
            },
        }
    }

    #[test]
    fn identity_path_is_deterministic_and_covers_every_domain_field() {
        let state_root = Path::new(r"C:\state");
        let first = PayloadOverlayIdentity::from_install_plan(&install_plan()).unwrap();
        let second = PayloadOverlayIdentity::from_install_plan(&install_plan()).unwrap();
        assert_eq!(first.path_under(state_root), second.path_under(state_root));
        assert_eq!(
            first.digest().to_hex(),
            "d7ca4aad5641bd3decb9e72fb8e325aeda48b038a70ca2ff89c00d804d4ae937"
        );

        let mut changed = install_plan();
        changed.app.version = Version::parse("1.2.4").unwrap();
        let changed = PayloadOverlayIdentity::from_install_plan(&changed).unwrap();
        assert_ne!(first.path_under(state_root), changed.path_under(state_root));
    }

    #[test]
    fn install_execution_and_transaction_derive_the_same_recovery_path() {
        let install = install_plan();
        let source = install.files[0].source_relative.clone();
        let destination = install.files[0].destination.to_string();
        let execution = ExecutionPlan {
            selected_components: install.selected_components.clone(),
            files: vec![FileOperation {
                key: install.files[0].key.clone(),
                kind: FileOperationKind::Create,
                destination: TargetPath::new(destination.into()).unwrap(),
                source_relative: source,
                precondition: FilePrecondition::Absent,
                expected_sha256: install.files[0].sha256,
                expected_size: install.files[0].size,
                conflict: None,
            }],
            summary: ExecutionSummary::default(),
            ..Default::default()
        };
        let transaction = zup_transaction::compile_transaction(&execution).unwrap();
        let state_root = Path::new("state");
        let from_install = PayloadOverlayIdentity::from_install_plan(&install).unwrap();
        let from_execution = PayloadOverlayIdentity::from_execution_plan(
            install.app.id.clone(),
            install.app.version.clone(),
            install.scope,
            &execution,
        )
        .unwrap();
        let from_transaction = PayloadOverlayIdentity::from_transaction(
            install.app.id.clone(),
            install.app.version.clone(),
            install.scope,
            &transaction,
        )
        .unwrap();
        assert_eq!(
            from_install.path_under(state_root),
            from_execution.path_under(state_root)
        );
        assert_eq!(
            from_install.path_under(state_root),
            from_transaction.path_under(state_root)
        );
    }

    #[test]
    fn materialization_verifies_content_and_rejects_special_targets() {
        let root = tempfile::TempDir::new().unwrap();
        let state_root = root.path().join("state");
        let plan = install_plan();
        let generated = generated_file();
        let identity = PayloadOverlayIdentity::from_install_plan(&plan).unwrap();
        let overlay = identity.path_under(&state_root).unwrap();
        let target = resolve_overlay_path(&overlay, &generated.source_relative).unwrap();
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::create_dir(&target).unwrap();

        let error = materialize_payload_overlay(&state_root, &plan, &[generated]).unwrap_err();
        assert!(matches!(error, PayloadOverlayError::NotRegular { .. }));
    }

    #[test]
    fn machine_overlay_materialization_does_not_touch_unwritable_state_root() {
        let root = tempfile::TempDir::new().unwrap();
        let state_root = root.path().join("machine-state");
        fs::write(&state_root, b"not a directory").unwrap();
        let mut plan = install_plan();
        plan.scope = SelectedScope::Machine;
        let base = payload_overlay_base_root(&state_root, SelectedScope::Machine).unwrap();
        assert_eq!(
            base,
            payload_overlay_base_root(&state_root, SelectedScope::Machine).unwrap()
        );
        assert!(!base.starts_with(&state_root));

        let generated = generated_file();
        let overlay = materialize_payload_overlay(&base, &plan, &[generated]).unwrap();
        assert!(overlay.as_deref().unwrap().starts_with(&base));
        assert!(state_root.is_file());
        cleanup_payload_overlay(&base, overlay.as_deref()).unwrap();
    }

    #[test]
    fn app_overlay_cleanup_is_scoped_to_one_installation() {
        let root = tempfile::TempDir::new().unwrap();
        let state_root = root.path().join("state");
        let first_plan = install_plan();
        let first =
            materialize_payload_overlay(&state_root, &first_plan, &[generated_file()]).unwrap();
        let mut second_plan = install_plan();
        second_plan.app.id = AppId::new("com.example.other-overlay").unwrap();
        let second =
            materialize_payload_overlay(&state_root, &second_plan, &[generated_file()]).unwrap();
        assert!(first.is_some());
        assert!(second.is_some());

        cleanup_app_payload_overlays(&state_root, &first_plan.app.id, first_plan.scope).unwrap();
        assert!(!first.unwrap().exists());
        assert!(second.unwrap().exists());
    }

    #[test]
    fn materialization_rejects_digest_mismatch_and_duplicate_sources() {
        let root = tempfile::TempDir::new().unwrap();
        let plan = install_plan();
        let mut changed = generated_file();
        changed.bytes = b"different".to_vec();
        let error =
            materialize_payload_overlay(&root.path().join("state"), &plan, &[changed]).unwrap_err();
        assert!(matches!(error, PayloadOverlayError::DigestMismatch { .. }));

        let duplicate = generated_file();
        let error = materialize_payload_overlay(
            &root.path().join("state-duplicate"),
            &plan,
            &[duplicate.clone(), duplicate],
        )
        .unwrap_err();
        assert!(matches!(error, PayloadOverlayError::DuplicateSource { .. }));
    }
}
