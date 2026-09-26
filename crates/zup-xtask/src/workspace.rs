//! Workspace layout discovery.
//!
//! The root manifest is the only source of truth for which directories are
//! workspace members. Members are resolved from disk rather than from
//! `cargo metadata` so the checks run before anything is built, on any host,
//! and produce the same answer everywhere.

use std::fs;
use std::path::Path;

use toml::Value;

/// One workspace member.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    pub name: String,
    /// Member directory relative to the workspace root, `/`-separated.
    pub directory: String,
}

/// Read the root manifest and resolve every member, sorted by directory.
///
/// `members` and `exclude` entries are either exact directories or a single
/// trailing `*` segment, which expands to the sorted immediate subdirectories
/// that hold a `Cargo.toml`.
pub fn members(root: &Path) -> Result<Vec<Member>, String> {
    let manifest = read_manifest(&root.join("Cargo.toml"))?;
    let workspace = manifest.get("workspace").ok_or_else(|| {
        format!(
            "{}: no [workspace] table",
            root.join("Cargo.toml").display()
        )
    })?;

    let mut directories = expand(root, string_list(workspace.get("members")))?;
    for excluded in expand(root, string_list(workspace.get("exclude")))? {
        directories.retain(|directory| *directory != excluded);
    }
    directories.sort();
    directories.dedup();

    directories
        .into_iter()
        .map(|directory| {
            let manifest = read_manifest(&root.join(&directory).join("Cargo.toml"))?;
            let name = manifest
                .get("package")
                .and_then(|package| package.get("name"))
                .and_then(Value::as_str)
                .ok_or_else(|| format!("{directory}: no package.name"))?
                .to_owned();
            Ok(Member { name, directory })
        })
        .collect()
}

/// Read one manifest as untyped TOML.
pub fn read_manifest(path: &Path) -> Result<Value, String> {
    let text = fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()))?;
    toml::from_str(&text).map_err(|error| format!("{}: {error}", path.display()))
}

fn string_list(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| entry.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// Resolve member entries to `/`-separated directories relative to `root`.
fn expand(root: &Path, entries: Vec<String>) -> Result<Vec<String>, String> {
    let mut directories = Vec::new();
    for entry in entries {
        let entry = entry.replace('\\', "/");
        let Some(prefix) = entry.strip_suffix("/*") else {
            directories.push(entry);
            continue;
        };
        let parent = root.join(prefix);
        let entries = fs::read_dir(&parent)
            .map_err(|error| format!("{}: {error}", parent.display()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("{}: {error}", parent.display()))?;
        let mut found = Vec::new();
        for entry in entries {
            if !entry.path().join("Cargo.toml").is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            found.push(format!("{prefix}/{name}"));
        }
        found.sort();
        directories.extend(found);
    }
    Ok(directories)
}

/// A `/`-separated relative path, for reports that must read the same on every
/// host.
pub fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}
