use std::fs::{self, File};
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use zup_core::{Installer, MAX_PLUGIN_ARTIFACTS, RelativePath, hash_reader};
use zup_manifest::Plugin;

use crate::error::BuildError;
use zup_core::ResolvedPlugin;

pub const MAX_PLUGIN_SOURCE_BYTES: u64 = 16 * 1024 * 1024;

pub(crate) fn validate_plugin_declaration_count(
    manifest_count: usize,
    installer_count: usize,
) -> Result<(), BuildError> {
    let count = manifest_count.max(installer_count);
    if count > MAX_PLUGIN_ARTIFACTS {
        return Err(BuildError::TooManyPluginDeclarations {
            count,
            limit: MAX_PLUGIN_ARTIFACTS,
        });
    }
    Ok(())
}

pub(crate) fn resolve_plugins(
    project_root: &Path,
    plugins: &[Plugin],
    installer: &Installer,
) -> Result<Vec<ResolvedPlugin>, BuildError> {
    validate_plugin_declaration_count(plugins.len(), installer.plugins.len())?;
    if plugins.len() != installer.plugins.len() {
        return Err(BuildError::PluginSourceMismatch {
            id: installer
                .plugins
                .first()
                .map_or_else(|| "<none>".to_owned(), |plugin| plugin.id.to_string()),
        });
    }

    let mut resolved = Vec::with_capacity(plugins.len());
    for (plugin, binding) in plugins.iter().zip(&installer.plugins) {
        if plugin.id != binding.id {
            return Err(BuildError::PluginSourceMismatch {
                id: plugin.id.to_string(),
            });
        }
        let (source, source_relative) = resolve_source(project_root, &plugin.source)?;
        let (size, sha256) = hash_source(&source)?;
        resolved.push(ResolvedPlugin {
            id: plugin.id.clone(),
            source,
            source_relative,
            size,
            sha256,
        });
    }
    Ok(resolved)
}

fn resolve_source(
    project_root: &Path,
    source: &str,
) -> Result<(PathBuf, RelativePath), BuildError> {
    if source.is_empty()
        || source.contains('\0')
        || source.contains('\\')
        || source.starts_with('/')
        || has_drive_prefix(source)
    {
        return Err(BuildError::PluginSourceEscapesProject {
            path: PathBuf::from(source),
        });
    }

    let relative = RelativePath::new(source).map_err(|error| BuildError::UnsafeRelativePath {
        path: source.to_owned(),
        reason: error.to_string(),
    })?;
    let path = Path::new(source);
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::RootDir
                    | Component::Prefix(_)
                    | Component::ParentDir
                    | Component::CurDir
            )
        })
    {
        return Err(BuildError::PluginSourceEscapesProject {
            path: path.to_path_buf(),
        });
    }

    let project_root = if project_root.is_absolute() {
        lexical_normalize(project_root)
    } else {
        let current = std::env::current_dir().map_err(|source| BuildError::Io {
            path: project_root.to_path_buf(),
            source,
        })?;
        lexical_normalize(&current.join(project_root))
    };
    let joined = lexical_normalize(&project_root.join(path));
    if !is_within(&project_root, &joined) {
        return Err(BuildError::PluginSourceEscapesProject {
            path: path.to_path_buf(),
        });
    }

    let mut current = project_root;
    for component in path.components() {
        let Component::Normal(component) = component else {
            return Err(BuildError::PluginSourceEscapesProject {
                path: path.to_path_buf(),
            });
        };
        current.push(component);
        let metadata = fs::symlink_metadata(&current).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                BuildError::PluginSourceMissing {
                    path: current.clone(),
                }
            } else {
                BuildError::SourceReadFailure {
                    path: current.clone(),
                    source: error,
                }
            }
        })?;
        let file_type = metadata.file_type();
        if file_type.is_symlink() {
            return Err(BuildError::PluginSourceSymlink { path: current });
        }
        if current != joined {
            if !file_type.is_dir() {
                return Err(BuildError::PluginSourceNotRegular { path: current });
            }
        } else if !file_type.is_file() {
            return Err(BuildError::PluginSourceNotRegular { path: current });
        }
    }

    Ok((joined, relative))
}

fn hash_source(path: &Path) -> Result<(u64, zup_core::Sha256Digest), BuildError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| BuildError::SourceReadFailure {
        path: path.to_path_buf(),
        source: error,
    })?;
    if metadata.file_type().is_symlink() {
        return Err(BuildError::PluginSourceSymlink {
            path: path.to_path_buf(),
        });
    }
    if !metadata.file_type().is_file() {
        return Err(BuildError::PluginSourceNotRegular {
            path: path.to_path_buf(),
        });
    }
    if metadata.len() > MAX_PLUGIN_SOURCE_BYTES {
        return Err(BuildError::PluginSourceTooLarge {
            path: path.to_path_buf(),
            size: metadata.len(),
            limit: MAX_PLUGIN_SOURCE_BYTES,
        });
    }

    let file = File::open(path).map_err(|source| BuildError::SourceReadFailure {
        path: path.to_path_buf(),
        source,
    })?;
    let (size, digest) = hash_reader(file.take(MAX_PLUGIN_SOURCE_BYTES + 1)).map_err(|source| {
        BuildError::SourceReadFailure {
            path: path.to_path_buf(),
            source,
        }
    })?;
    if size > MAX_PLUGIN_SOURCE_BYTES {
        return Err(BuildError::PluginSourceTooLarge {
            path: path.to_path_buf(),
            size,
            limit: MAX_PLUGIN_SOURCE_BYTES,
        });
    }

    let after = fs::symlink_metadata(path).map_err(|source| BuildError::SourceReadFailure {
        path: path.to_path_buf(),
        source,
    })?;
    if after.file_type().is_symlink() {
        return Err(BuildError::PluginSourceSymlink {
            path: path.to_path_buf(),
        });
    }
    if after.len() != size
        || after.len() != metadata.len()
        || (after.modified().ok() != metadata.modified().ok())
    {
        return Err(BuildError::PluginSourceChanged {
            path: path.to_path_buf(),
        });
    }
    Ok((size, digest))
}

fn has_drive_prefix(source: &str) -> bool {
    let bytes = source.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut output = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                output.pop();
            }
            other => output.push(other.as_os_str()),
        }
    }
    output
}

fn is_within(base: &Path, candidate: &Path) -> bool {
    let mut base_components = base.components();
    let mut candidate_components = candidate.components();
    loop {
        match (base_components.next(), candidate_components.next()) {
            (None, Some(_)) => return true,
            (None, None) => return true,
            (Some(_), None) => return false,
            (Some(base_component), Some(candidate_component)) => {
                if base_component != candidate_component {
                    return false;
                }
            }
        }
    }
}
