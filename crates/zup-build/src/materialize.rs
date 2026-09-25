//! Project source-root resolution and file materialization.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use tracing::{debug, info, info_span};
use walkdir::WalkDir;
use zup_core::{FileMapping, Installer, PrerequisitePackage, RelativePath, Template, hash_reader};
use zup_manifest::Manifest;

use crate::digest::Sha256Digest;
use crate::error::BuildError;
use crate::pattern::FilePattern;
use crate::plan::{BuildPlan, ResolvedFile, ResolvedPrerequisite};
use crate::plugins::{resolve_plugins, validate_plugin_declaration_count};
use crate::windows::validate_windows_destination;

/// Materialize installer sources into a deterministic build plan.
///
/// `manifest_path` must point at the project's `zup.toml`. Source discovery is
/// rooted at that file's directory and never depends on the process CWD.
pub fn materialize(
    manifest_path: &Path,
    manifest: &Manifest,
    installer: Installer,
) -> Result<BuildPlan, BuildError> {
    let project_root = project_root(manifest_path);
    validate_plugin_declaration_count(manifest.plugins.len(), installer.plugins.len())?;
    let mut installer = installer;
    if let Some(updates) = &manifest.updates {
        let root_path = project_root.join(&updates.root);
        let root_file = File::open(&root_path).map_err(|source| BuildError::Io {
            path: root_path.clone(),
            source,
        })?;
        let mut trusted_root = Vec::new();
        root_file
            .take(1024 * 1024 + 1)
            .read_to_end(&mut trusted_root)
            .map_err(|source| BuildError::Io {
                path: root_path.clone(),
                source,
            })?;
        if trusted_root.len() > 1024 * 1024 {
            return Err(BuildError::UpdateRootTooLarge);
        }
        installer.updates = Some(zup_core::UpdateConfig {
            repository: updates.repository.clone(),
            channel: updates.channel.clone(),
            trusted_root,
        });
    }
    let source_root = resolve_source_root(&project_root, &manifest.source.directory)?;
    let plugins = resolve_plugins(&project_root, &manifest.plugins, &installer)?;
    let (prerequisites, prerequisite_size) = resolve_prerequisites(&project_root, &installer)?;

    let span = info_span!(
        "materialize",
        project_root = %project_root.display(),
        source_root = %source_root.display()
    );
    let _guard = span.enter();

    let file_rules = installer.files.len();
    info!(file_rules, "materializing file mappings");

    let mut resolved = Vec::new();
    for (index, mapping) in installer.files.iter().enumerate() {
        let rule_span = info_span!(
            "file_rule",
            index,
            pattern = mapping.source.as_str(),
            destination = %mapping.destination
        );
        let _rule_guard = rule_span.enter();
        expand_mapping(&source_root, mapping, &mut resolved)?;
    }

    detect_collisions(&resolved)?;

    let mut total_size = 0u64;
    for file in &resolved {
        total_size = total_size
            .checked_add(file.size)
            .ok_or(BuildError::SizeOverflow)?;
    }

    resolved.sort_by(|a, b| {
        BuildPlan::sort_key(a)
            .cmp(&BuildPlan::sort_key(b))
            .then_with(|| a.source.cmp(&b.source))
    });

    let file_count = resolved.len();
    info!(file_count, total_size, "materialization complete");

    Ok(BuildPlan {
        installer,
        prerequisites,
        plugins,
        files: resolved,
        total_size,
        prerequisite_size,
    })
}

fn resolve_prerequisites(
    project_root: &Path,
    installer: &Installer,
) -> Result<(Vec<ResolvedPrerequisite>, u64), BuildError> {
    let mut resolved = Vec::new();
    let mut total = 0u64;
    for prerequisite in &installer.prerequisites {
        match &prerequisite.package {
            PrerequisitePackage::Remote { size, .. } => {
                if let Some(size) = size {
                    total = total.checked_add(*size).ok_or(BuildError::SizeOverflow)?;
                }
            }
            PrerequisitePackage::Embedded { path, sha256, size } => {
                let source = project_root.join(path.as_str());
                let normalized = lexical_normalize(&source);
                if !is_within(&lexical_normalize(project_root), &normalized) {
                    return Err(BuildError::PrerequisiteSource {
                        id: prerequisite.id.to_string(),
                        path: path.as_str().into(),
                    });
                }
                reject_prerequisite_reparse(&normalized, &prerequisite.id)?;
                let metadata = fs::symlink_metadata(&normalized).map_err(|source| {
                    if source.kind() == std::io::ErrorKind::NotFound {
                        BuildError::PrerequisiteSource {
                            id: prerequisite.id.to_string(),
                            path: normalized.clone(),
                        }
                    } else {
                        BuildError::Io {
                            path: normalized.clone(),
                            source,
                        }
                    }
                })?;
                if metadata.file_type().is_symlink() || !metadata.is_file() {
                    return Err(BuildError::PrerequisiteSource {
                        id: prerequisite.id.to_string(),
                        path: normalized,
                    });
                }
                let (actual_size, actual_digest) = hash_file(&normalized)?;
                if actual_size != *size || actual_digest != *sha256 {
                    return Err(BuildError::PrerequisiteIdentity {
                        id: prerequisite.id.to_string(),
                        path: normalized,
                    });
                }
                total = total
                    .checked_add(actual_size)
                    .ok_or(BuildError::SizeOverflow)?;
                resolved.push(ResolvedPrerequisite {
                    id: prerequisite.id.clone(),
                    source: normalized,
                    source_relative: path.clone(),
                    size: actual_size,
                    sha256: actual_digest,
                });
            }
        }
    }
    resolved.sort_by(|left, right| left.id.cmp(&right.id));
    Ok((resolved, total))
}

fn project_root(manifest_path: &Path) -> PathBuf {
    manifest_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn resolve_source_root(project_root: &Path, directory: &Path) -> Result<PathBuf, BuildError> {
    if directory.is_absolute() {
        return Err(BuildError::SourceEscapesProject {
            path: directory.to_path_buf(),
        });
    }

    let joined = project_root.join(directory);
    let normalized = lexical_normalize(&joined);
    let project_norm = lexical_normalize(project_root);

    if !is_within(&project_norm, &normalized) {
        return Err(BuildError::SourceEscapesProject {
            path: directory.to_path_buf(),
        });
    }

    let meta = fs::metadata(&normalized).map_err(|source| {
        if source.kind() == std::io::ErrorKind::NotFound {
            BuildError::SourceMissing {
                path: normalized.clone(),
                src: None,
            }
        } else {
            BuildError::Io {
                path: normalized.clone(),
                source,
            }
        }
    })?;

    if !meta.is_dir() {
        return Err(BuildError::SourceNotDirectory { path: normalized });
    }

    debug!(path = %normalized.display(), "resolved source root");
    Ok(normalized)
}

fn reject_prerequisite_reparse(
    path: &Path,
    id: &zup_core::PrerequisiteId,
) -> Result<(), BuildError> {
    let mut current = path.to_path_buf();
    loop {
        if let Ok(metadata) = fs::symlink_metadata(&current) {
            let reparse = if metadata.file_type().is_symlink() {
                true
            } else {
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt;
                    metadata.file_attributes() & 0x400 != 0
                }
                #[cfg(not(windows))]
                {
                    false
                }
            };
            if reparse {
                return Err(BuildError::PrerequisiteSource {
                    id: id.to_string(),
                    path: path.to_path_buf(),
                });
            }
        }
        let Some(parent) = current.parent() else {
            break;
        };
        if parent == current {
            break;
        }
        current = parent.to_path_buf();
    }
    Ok(())
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn is_within(base: &Path, candidate: &Path) -> bool {
    if candidate == base {
        return true;
    }
    let mut base_components = base.components();
    let mut candidate_components = candidate.components();
    loop {
        match (base_components.next(), candidate_components.next()) {
            (None, Some(_)) => return true,
            (None, None) => return true,
            (Some(_), None) => return false,
            (Some(a), Some(b)) => {
                if a != b {
                    return false;
                }
            }
        }
    }
}

fn expand_mapping(
    source_root: &Path,
    mapping: &FileMapping,
    out: &mut Vec<ResolvedFile>,
) -> Result<(), BuildError> {
    let pattern = FilePattern::compile(&mapping.source)?;
    let discovered = discover(source_root, &pattern)?;
    let matched = discovered.len();
    debug!(matched, "files discovered for pattern");

    if matched == 0 && !mapping.allow_empty {
        return Err(BuildError::PatternMatchedNothing {
            pattern: mapping.source.clone(),
        });
    }

    for (absolute, source_relative) in discovered {
        let suffix = pattern.destination_suffix(source_relative.as_str())?;
        let suffix_path =
            RelativePath::new(&suffix).map_err(|err| BuildError::PathNotRepresentable {
                path: suffix.clone(),
                reason: err.to_string(),
            })?;
        let destination = mapping.destination.join_relative(&suffix_path);
        validate_windows_destination(&destination)?;

        let (size, sha256) = hash_file(&absolute)?;
        out.push(ResolvedFile {
            source: absolute,
            source_relative,
            destination,
            size,
            sha256,
            component: mapping.component.clone(),
            condition: mapping.when.clone(),
        });
    }

    Ok(())
}

fn discover(
    source_root: &Path,
    pattern: &FilePattern,
) -> Result<Vec<(PathBuf, RelativePath)>, BuildError> {
    let mut matches = Vec::new();

    for entry in WalkDir::new(source_root).follow_links(false).into_iter() {
        let entry = entry.map_err(|err| {
            let path = err
                .path()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| source_root.to_path_buf());
            let source = err
                .into_io_error()
                .unwrap_or_else(|| std::io::Error::other("walkdir entry error"));
            BuildError::Io { path, source }
        })?;

        let file_type = entry.file_type();
        let absolute = entry.path().to_path_buf();

        let relative_os = absolute.strip_prefix(source_root).unwrap_or(entry.path());
        let source_relative = match RelativePath::from_path(relative_os) {
            Ok(path) => path,
            Err(err) => {
                // Root entry itself has empty relative path — skip.
                if relative_os.as_os_str().is_empty() {
                    continue;
                }
                return Err(path_error(relative_os, err));
            }
        };

        let match_key = source_relative.as_str();
        if !pattern.is_match(match_key) {
            continue;
        }

        if file_type.is_symlink() {
            return Err(BuildError::MatchedSymlink {
                path: absolute,
                pattern: pattern.pattern.clone(),
            });
        }

        if file_type.is_dir() {
            // Directories are not payload entries; their children are walked.
            continue;
        }

        if !file_type.is_file() {
            return Err(BuildError::MatchedSpecialFile {
                path: absolute,
                pattern: pattern.pattern.clone(),
            });
        }

        matches.push((absolute, source_relative));
    }

    Ok(matches)
}

fn path_error(path: &Path, err: zup_core::RelativePathError) -> BuildError {
    match err {
        zup_core::RelativePathError::NotUtf8 { path } => BuildError::PathNotRepresentable {
            path,
            reason: "path is not valid UTF-8".to_owned(),
        },
        other => BuildError::UnsafeRelativePath {
            path: path.display().to_string(),
            reason: other.to_string(),
        },
    }
}

fn hash_file(path: &Path) -> Result<(u64, Sha256Digest), BuildError> {
    let before = fs::metadata(path).map_err(|source| BuildError::SourceReadFailure {
        path: path.to_path_buf(),
        source,
    })?;

    let file = File::open(path).map_err(|source| BuildError::SourceReadFailure {
        path: path.to_path_buf(),
        source,
    })?;

    let (size, digest) = hash_reader(file).map_err(|source| BuildError::SourceReadFailure {
        path: path.to_path_buf(),
        source,
    })?;

    let after = fs::metadata(path).map_err(|source| BuildError::SourceReadFailure {
        path: path.to_path_buf(),
        source,
    })?;

    if before.len() != after.len() || before.len() != size {
        return Err(BuildError::SourceChangedDuringBuild {
            path: path.to_path_buf(),
        });
    }

    match (before.modified(), after.modified()) {
        (Ok(a), Ok(b)) if a != b => {
            return Err(BuildError::SourceChangedDuringBuild {
                path: path.to_path_buf(),
            });
        }
        _ => {}
    }

    Ok((size, digest))
}

fn detect_collisions(files: &[ResolvedFile]) -> Result<(), BuildError> {
    let mut exact: BTreeMap<String, usize> = BTreeMap::new();
    let mut folded: BTreeMap<String, usize> = BTreeMap::new();

    for (index, file) in files.iter().enumerate() {
        let destination = file.destination.to_string();
        let source = file.source_relative.as_str().to_owned();

        if let Some(&prev) = exact.get(&destination) {
            return Err(BuildError::DestinationCollision {
                destination,
                first: files[prev].source_relative.as_str().to_owned(),
                second: source,
            });
        }

        let canonical = destination.to_lowercase();
        if let Some(&prev) = folded.get(&canonical) {
            return Err(BuildError::WindowsDestinationCollision {
                first: files[prev].destination.to_string(),
                second: destination,
                canonical,
            });
        }

        exact.insert(destination, index);
        folded.insert(canonical, index);
    }

    Ok(())
}

/// Build a destination template from a base template and a suffix path.
/// Exposed for tests and future planners.
pub fn materialize_destination(base: &Template, suffix: &RelativePath) -> Template {
    base.join_relative(suffix)
}
